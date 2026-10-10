//! 任务账本端点:会话当前任务状态的**只读**投影视图。
//!
//! 账本的真值只有一处 —— `denia-session` 的折叠。这个端点不另折一份副本,
//! 只把会话已经折好的状态整理成摘要形状发给前端(状态、结局、未决问题、
//! 待完成、证据与验证的摘要):
//!
//! - 折叠必须带会话作用域(本会话身份 + rewind 报废的 revision),在外面
//!   自己折一遍就会与真值漂移;
//! - 正文级内容(事实/假设/失败尝试的全文、指纹清单)留在日志里。面板要回答
//!   的是"能不能收口、凭什么",不是把日志再发一遍 —— 所以这里只给条数与摘要。
//!
//! 只读:模型侧的写入口是 `update_task` / `run_checks` 两个工具,执行证据与
//! 验证结论由宿主执行链产生。端点没有任何写路径,也不提供"补一条证据"的口子。

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use denia_core::task::{
    EvidenceSource, LedgerOverflow, Requirement, RequirementKind, RevisionRecord, TaskOutcome,
    TaskState, TaskStatus, ValidationResult, ValidationVerdict, VerificationState,
};
use serde::Serialize;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    // 与 goal 的只读视图同形:GET 取投影,没有写路径。
    Router::new().route("/api/sessions/{id}/task", get(get_task))
}

async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let task = load_task(&state, &id).await?;
    Ok(Json(TaskView {
        task: task.as_ref().map(TaskSnapshot::of),
    }))
}

/// 打开(或复用)会话并读它的任务投影。
///
/// 走 `spawn_blocking`:会话默认冷打开,首次读要扫一遍日志,不能让异步
/// 执行器陪着等 IO。读完**不需要** `ensure_hot` —— 冷加载在解析时就把聚合
/// 折好了(`denia_session::Session::task`),这也是这里不自己折的原因。
async fn load_task(state: &AppState, id: &str) -> Result<Option<TaskState>, ApiError> {
    let live = state.live.clone();
    let sessions = state.sessions.clone();
    let id = id.to_string();
    let live = tokio::task::spawn_blocking(move || live.get_or_load(&sessions, &id))
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "task/load-failed",
                format!("读取会话失败:{error}"),
            )
        })?
        .map_err(ApiError::from_session)?;
    Ok(live.session.task())
}

/// 顶层信封:`task` 为 `null` = 该会话没有任务事件(旧会话、普通问答)。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskView {
    task: Option<TaskSnapshot>,
}

/// 账本的展示形状:状态 + 裁决 + 摘要级明细。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TaskSnapshot {
    id: String,
    /// 当前 revision(当前要求集的身份)。
    revision: String,
    status: TaskStatus,
    /// 结局,由折叠裁决;`None` = 仍在进行(尚未收口)。
    outcome: Option<TaskOutcome>,
    /// 当前 revision 的验证状态,含"为什么不是通过"的原因。
    verification: VerificationState,
    goal: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocked_reason: Option<String>,
    /// 拒绝过复用旧 revision id 的换版:换版前旧 id 仍在报废集合里。
    revision_conflict: bool,
    revision_rejected: u32,
    revisions: Vec<RevisionSnapshot>,
    requirements: Vec<RequirementSnapshot>,
    open_questions: Vec<ClaimSnapshot>,
    remaining: Vec<String>,
    evidence: Vec<EvidenceSnapshot>,
    validations: Vec<ValidationSnapshot>,
    /// 本视图没有展开的集合条数:正文留在日志里,但"有多少"不能藏。
    counts: LedgerCounts,
    overflow: LedgerOverflow,
    created_at: u64,
    updated_at: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RevisionSnapshot {
    id: String,
    ordinal: u32,
    reason: String,
    at: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RequirementSnapshot {
    id: String,
    revision: String,
    kind: RequirementKind,
    text: String,
    declared_at: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaimSnapshot {
    id: String,
    text: String,
    at: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceSnapshot {
    id: String,
    revision: String,
    /// 来路(工具调用 / 检查运行 / 文件读取);命令全文与输出在日志里。
    source: EvidenceSource,
    summary: String,
    fingerprint_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin_session: Option<String>,
    at: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ValidationSnapshot {
    revision: String,
    /// 结论由退出码算出;`None` = 回放出来的空报告,不构成结论。
    verdict: Option<ValidationVerdict>,
    checks: Vec<CheckSnapshot>,
    covered: Vec<String>,
    fingerprint_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    origin_session: Option<String>,
    at: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckSnapshot {
    command: String,
    exit_code: Option<i32>,
    expect_exit_code: i32,
    passed: bool,
    workdir: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LedgerCounts {
    facts: usize,
    assumptions: usize,
    failed_attempts: usize,
    changes: usize,
}

impl TaskSnapshot {
    fn of(state: &TaskState) -> Self {
        Self {
            id: state.id.as_str().to_string(),
            revision: state.revision.as_str().to_string(),
            status: state.status,
            outcome: state.outcome(),
            verification: state.verification(),
            goal: state.goal.clone(),
            blocked_reason: state.blocked_reason.clone(),
            revision_conflict: state.revision_conflict,
            revision_rejected: state.revision_rejected,
            revisions: state.revisions.iter().map(revision).collect(),
            requirements: state.requirements.iter().map(requirement).collect(),
            open_questions: state
                .open_questions
                .iter()
                .map(|item| ClaimSnapshot {
                    id: item.id.as_str().to_string(),
                    text: item.text.clone(),
                    at: item.at,
                })
                .collect(),
            remaining: state.remaining.clone(),
            evidence: state.evidence.iter().map(evidence).collect(),
            validations: state.validations.iter().map(validation).collect(),
            counts: LedgerCounts {
                facts: state.facts.len(),
                assumptions: state.assumptions.len(),
                failed_attempts: state.failed_attempts.len(),
                changes: state.changes.len(),
            },
            overflow: state.overflow,
            created_at: state.created_at,
            updated_at: state.updated_at,
        }
    }
}

fn revision(record: &RevisionRecord) -> RevisionSnapshot {
    RevisionSnapshot {
        id: record.id.as_str().to_string(),
        ordinal: record.ordinal,
        reason: record.reason.clone(),
        at: record.at,
    }
}

fn requirement(item: &Requirement) -> RequirementSnapshot {
    RequirementSnapshot {
        id: item.id.as_str().to_string(),
        revision: item.revision.as_str().to_string(),
        kind: item.kind,
        text: item.text.clone(),
        declared_at: item.declared_at,
    }
}

fn evidence(item: &denia_core::task::Evidence) -> EvidenceSnapshot {
    EvidenceSnapshot {
        id: item.id.as_str().to_string(),
        revision: item.revision.as_str().to_string(),
        source: item.source.clone(),
        summary: item.summary.clone(),
        fingerprint_count: item.fingerprints.len(),
        origin_session: item.origin_session.clone(),
        at: item.at,
    }
}

fn validation(item: &ValidationResult) -> ValidationSnapshot {
    ValidationSnapshot {
        revision: item.revision().as_str().to_string(),
        verdict: item.verdict(),
        checks: item
            .runs()
            .iter()
            .map(|run| CheckSnapshot {
                command: run.command.clone(),
                exit_code: run.exit_code,
                expect_exit_code: run.expect_exit_code,
                passed: run.passed(),
                workdir: run.workdir.clone(),
            })
            .collect(),
        covered: item
            .covered()
            .iter()
            .map(|id| id.as_str().to_string())
            .collect(),
        fingerprint_count: item.fingerprints().len(),
        origin_session: item.origin_session().map(str::to_string),
        at: item.at(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::{SessionEnvelope, SessionEvent};
    use denia_core::task::{
        CheckRun, Evidence, EvidenceId, FileFingerprint, RequirementClaim, RequirementId,
        RevisionId, SourceRef, TaskId, TaskOp,
    };

    fn user_message(text: &str) -> SessionEvent {
        SessionEvent::UserMessage {
            text: text.to_string(),
            injected: false,
            channel: None,
            images: Vec::new(),
        }
    }

    /// 一次开账:验收项引用日志里那条用户消息(seq 与时间戳都要对得上,否则
    /// 折叠层按"核不到出处"处理)。
    fn open_op(user: &SessionEnvelope) -> TaskOp {
        TaskOp::Open {
            task_id: TaskId::new("task-1"),
            revision: RevisionId::new("rev-a"),
            goal: "修好解析器".into(),
            requirements: vec![RequirementClaim {
                id: RequirementId::new("req-1"),
                kind: RequirementKind::Acceptance,
                text: "cargo test 全绿".into(),
                source: SourceRef {
                    session: None,
                    seq: user.seq,
                    time_ms: user.time,
                    quote: Some("跑通测试".into()),
                },
            }],
        }
    }

    #[tokio::test]
    async fn task_view_projects_status_outcome_and_requirement_summary() {
        let home = std::env::temp_dir().join(format!("denia-task-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let state = Arc::new(crate::state::build_state(&home, false, 0).await.unwrap());
        let session = state.sessions.create(&home, true).unwrap();
        let id = session.id().to_string();
        let user = session
            .append(user_message("修好解析器,跑通测试再说完事"))
            .unwrap();
        session
            .append(SessionEvent::Task { op: open_op(&user) })
            .unwrap();
        session
            .append(SessionEvent::Task { op: TaskOp::Close })
            .unwrap();
        session.flush().unwrap();
        drop(session);

        let response = get_task(State(state.clone()), Path(id.clone()))
            .await
            .unwrap()
            .into_response();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let task = &body["task"];
        assert_eq!(task["id"], "task-1");
        assert_eq!(task["revision"], "rev-a");
        assert_eq!(task["status"], "closed");
        assert_eq!(
            task["outcome"], "unverified",
            "没有验证结论:收口只能判未验证完成,不能凭空是通过"
        );
        assert_eq!(task["verification"]["kind"], "unverified");
        assert_eq!(task["verification"]["reason"]["kind"], "never-run");
        assert_eq!(task["requirements"][0]["id"], "req-1");
        assert_eq!(task["requirements"][0]["kind"], "acceptance");
        assert!(
            task["counts"]["facts"].is_u64(),
            "摘要视图要给出未展开的条数"
        );
        assert!(task["evidence"].is_array());
        drop(state);
        std::fs::remove_dir_all(home).unwrap();
    }

    // 未开账的旧会话:账本为 null,而不是一个空对象(前端据此决定不显示面板)。
    #[tokio::test]
    async fn task_view_is_null_without_task_events() {
        let home = std::env::temp_dir().join(format!("denia-task-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let state = Arc::new(crate::state::build_state(&home, false, 0).await.unwrap());
        let session = state.sessions.create(&home, true).unwrap();
        let id = session.id().to_string();
        session.append(user_message("只是问个问题")).unwrap();
        session.flush().unwrap();
        drop(session);

        let response = get_task(State(state.clone()), Path(id))
            .await
            .unwrap()
            .into_response();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(body["task"].is_null());
        drop(state);
        std::fs::remove_dir_all(home).unwrap();
    }

    // 冻结的输入指纹与检查运行的摘要:命令与退出码进视图,输出正文不进。
    #[test]
    fn validation_summary_keeps_commands_but_not_output() {
        let result = ValidationResult::from_check_runs(
            RevisionId::new("rev-a"),
            vec![CheckRun {
                command: "cargo test".into(),
                exit_code: Some(0),
                expect_exit_code: 0,
                workdir: ".".into(),
                fingerprints: vec![FileFingerprint {
                    path: "src/lib.rs".into(),
                    digest: "d1".into(),
                }],
            }],
            vec![RequirementId::new("req-1")],
            Some("session-1".into()),
            7,
        )
        .expect("有检查运行才构成结论");
        let view = validation(&result);
        assert_eq!(view.verdict, Some(ValidationVerdict::Passed));
        assert_eq!(view.checks[0].command, "cargo test");
        assert!(view.checks[0].passed);
        assert_eq!(view.covered, vec!["req-1".to_string()]);
        assert_eq!(view.fingerprint_count, 1);
        let json = serde_json::to_value(&view).unwrap();
        assert!(json.get("checks").is_some());
        assert!(json.get("runs").is_none(), "检查运行的原始结构不进视图");
        assert!(json.get("stdout").is_none(), "检查输出正文不进视图");

        let record = Evidence {
            id: EvidenceId::new("ev-1"),
            revision: RevisionId::new("rev-a"),
            source: EvidenceSource::ToolCall {
                call_id: "call-1".into(),
                tool: "read_file".into(),
            },
            fingerprints: vec![FileFingerprint {
                path: "src/lib.rs".into(),
                digest: "d1".into(),
            }],
            summary: "读到解析分支".into(),
            origin_session: None,
            at: 8,
        };
        let view = evidence(&record);
        assert_eq!(view.summary, "读到解析分支");
        assert_eq!(view.fingerprint_count, 1, "指纹只报条数,清单留在日志里");
        assert!(view.origin_session.is_none());
    }
}
