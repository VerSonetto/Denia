//! Goal 模式端点:会话目标的读取与操作(set/edit/pause/resume/clear)。
//!
//! 目标状态以 `goal` 操作事件落会话日志(事件源折叠);本模块只做:
//! 参数与状态转换校验(fail loud)、写事件并广播给 follow 订阅者、
//! 触发续跑检查。运行中的轮次照常执行,不因目标操作中断——唯一例外
//! 是 pause(dsh 语义:暂停中止运行中的轮次)。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use denia_core::session::{GoalOp, GoalState, GoalStatus, SessionEvent};
use serde::{Deserialize, Serialize};

use crate::error::ApiError;
use crate::state::{AppState, ServerEvent};

pub fn router() -> Router<Arc<AppState>> {
    Router::new().route(
        "/api/sessions/{id}/goal",
        get(get_goal).post(goal_action),
    )
}

/// 目标视图:折叠后的目标状态 + 服务端权威的减法记账用量 + 配置上限
/// (前端 GoalBar 与 /goal 命令结果的展示口径)。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GoalView {
    goal: Option<GoalState>,
    tokens_used: u64,
    max_rounds: u32,
    max_goal_token_budget: u64,
}

async fn goal_view(state: &AppState, id: &str) -> Result<GoalView, ApiError> {
    let live = state
        .live
        .get_or_load(&state.sessions, id)
        .map_err(ApiError::from_session)?;
    let config = state.runtime.goals_config();
    Ok(GoalView {
        tokens_used: live.session.goal_tokens_used().unwrap_or(0),
        goal: live.session.goal(),
        max_rounds: config.max_rounds,
        max_goal_token_budget: config.max_goal_token_budget,
    })
}

async fn get_goal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let view = goal_view(&state, &id).await?;
    let body = serde_json::to_value(&view).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "goal/serialize",
            e.to_string(),
        )
    })?;
    Ok(Json(body))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoalActionBody {
    /// set | edit | pause | resume | clear
    action: String,
    #[serde(default)]
    objective: Option<String>,
    #[serde(default)]
    token_budget: Option<u64>,
    /// 用户操作时的模型选择:set/edit/resume 会把它记录为该会话最近的
    /// 人为选择——goal 自动续跑据此回推模型(新会话日志里还没有
    /// request-header,没有这条记录第一轮就无从发起)。
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// 用户输入的命令原文(如 "/goal xxx"):落 command-run 事件供前端
    /// 按真实 seq 回显;斜杠命令路径携带,GoalBar 按钮路径不携带。
    #[serde(default)]
    echo_text: Option<String>,
}

async fn goal_action(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<GoalActionBody>,
) -> Result<impl IntoResponse, ApiError> {
    let live = state
        .live
        .get_or_load(&state.sessions, &id)
        .map_err(ApiError::from_session)?;
    if live.session.header().subagent.is_some() {
        return Err(ApiError::bad_request(
            "subagent/read-only",
            "子代理会话不支持目标模式",
        ));
    }
    let config = state.runtime.goals_config();
    let current = live.session.goal();
    let action = body.action.as_str();

    // 参数与转换校验(折叠层对非法转换另有忽略兜底,这里 fail loud)。
    let op = match action {
        "set" => {
            // complete 是终态且 GoalBar 已不显示:直接设置新目标(覆盖);
            // 其余已存在状态仍拒绝(先清除或编辑)。
            if current.is_some_and(|goal| goal.status != GoalStatus::Complete) {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "goal/exists",
                    "该会话已有目标;请先清除或编辑现有目标",
                ));
            }
            let objective = non_empty(&body.objective, "目标内容不能为空")?;
            GoalOp::Set {
                objective,
                token_budget: checked_budget(&body.token_budget, &config)?,
            }
        }
        "edit" => {
            let goal = require_goal(&current)?;
            if goal.status == GoalStatus::Complete {
                return Err(ApiError::bad_request(
                    "goal/complete",
                    "目标已完成(终态);请清除后重新设置",
                ));
            }
            if body.objective.is_none() && body.token_budget.is_none() {
                return Err(ApiError::bad_request("goal/empty-edit", "没有可编辑的字段"));
            }
            if body.objective.is_some() {
                non_empty(&body.objective, "目标内容不能为空")?;
            }
            GoalOp::Edit {
                objective: body.objective.as_ref().map(|t| t.trim().to_string()),
                token_budget: checked_budget(&body.token_budget, &config)?,
            }
        }
        "pause" => {
            let goal = require_goal(&current)?;
            if goal.status != GoalStatus::Active {
                return Err(ApiError::bad_request(
                    "goal/invalid-transition",
                    format!("当前状态({})不允许暂停", goal.status.label()),
                ));
            }
            GoalOp::Pause
        }
        "resume" => {
            let goal = require_goal(&current)?;
            if !matches!(goal.status, GoalStatus::Paused | GoalStatus::Blocked) {
                return Err(ApiError::bad_request(
                    "goal/invalid-transition",
                    format!("当前状态({})不允许恢复", goal.status.label()),
                ));
            }
            GoalOp::Resume
        }
        "clear" => {
            require_goal(&current)?;
            GoalOp::Clear
        }
        other => {
            return Err(ApiError::bad_request(
                "goal/unknown-action",
                format!("未知操作:{other:?}(可用:set/edit/pause/resume/clear)"),
            ));
        }
    };

    // 用户操作 = 记录模型选择(供续跑回推)+ 清失败计数;与发送消息同语义。
    if matches!(action, "set" | "edit" | "resume") {
        let provider = body.provider.as_deref().unwrap_or_default().trim().to_string();
        let model = body.model.as_deref().unwrap_or_default().trim().to_string();
        if !provider.is_empty() && !model.is_empty() {
            state.runtime.human_turn(
                &id,
                &denia_core::config::ModelSelection {
                    provider,
                    model,
                    reasoning_effort: body.reasoning_effort.clone(),
                },
            );
        }
    }

    // 写事件(spawn_blocking:append 是阻塞 fs)+ 刷盘 + 推订阅者。
    // 命令回显先于 goal 操作落盘:前端按 seq 渲染,回显行永远排在
    // 该命令触发的状态变化之前。
    let action_live = live.clone();
    let action_op = op.clone();
    let echo_text = body.echo_text.clone();
    let envelope = tokio::task::spawn_blocking(move || -> Result<_, String> {
        if let Some(text) = echo_text {
            action_live
                .session
                .append(SessionEvent::CommandRun {
                    name: "goal".into(),
                    text,
                })
                .map_err(|e| e.to_string())?;
        }
        let envelope = action_live
            .session
            .apply_goal(action_op)
            .map_err(|e| e.to_string())?;
        action_live.session.flush().map_err(|e| e.to_string())?;
        Ok(envelope)
    })
    .await
    .map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "goal/action-failed",
            format!("goal 操作执行失败:{e}"),
        )
    })?
    .map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "goal/write-failed",
            format!("goal 事件写入失败:{e}"),
        )
    })?;
    let _ = live.followers.send(envelope);
    let _ = state.events.send(ServerEvent::SessionsUpdated);

    // pause 的 dsh 语义:运行中暂停会中止当前轮次(用户显式停,不是悄悄删队列)。
    if action == "pause"
        && live.running.load(Ordering::SeqCst)
        && let Some(token) = live.cancel.lock().unwrap().as_ref()
    {
        token.cancel();
    }

    // 状态回到 active 的操作(set/edit/resume)在空闲会话上立即触发续跑。
    let resumed = live.session.goal().is_some_and(|goal| {
        goal.status == GoalStatus::Active && action != "pause"
    });
    if resumed && !live.running.load(Ordering::SeqCst) {
        state.runtime.continue_goal(&id);
    }

    let view = goal_view(&state, &id).await?;
    let body = serde_json::to_value(&view).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "goal/serialize",
            e.to_string(),
        )
    })?;
    Ok((StatusCode::OK, Json(body)))
}

fn require_goal(goal: &Option<GoalState>) -> Result<GoalState, ApiError> {
    goal.clone().ok_or_else(|| {
        ApiError::bad_request("goal/none", "该会话没有设置目标")
    })
}

fn non_empty(value: &Option<String>, message: &str) -> Result<String, ApiError> {
    let text = value
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| ApiError::bad_request("goal/empty-objective", message))?;
    Ok(text.to_string())
}

fn checked_budget(
    budget: &Option<u64>,
    config: &crate::agent_runtime::GoalsConfig,
) -> Result<Option<u64>, ApiError> {
    match budget {
        None => Ok(None),
        // 0 原样透传:op 层语义为 Set 不设预算 / Edit 清除预算。
        Some(0) => Ok(Some(0)),
        Some(budget) if *budget > config.max_goal_token_budget => Err(ApiError::bad_request(
            "goal/budget-too-large",
            format!(
                "token 预算超出全局上限 {}(goals.maxGoalTokenBudget)",
                config.max_goal_token_budget
            ),
        )),
        Some(budget) => Ok(Some(*budget)),
    }
}
