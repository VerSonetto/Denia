//! 主代理创建**永久**子代理定义：写入 `$DENIA_HOME/subagents` 或项目
//! `.denia/subagents`，与设置页共用同一份 `ProfileStore`。
//!
//! 与 `spawn_agent` 的 `inline` 分工：
//! - inline 是临时定义，只冻结在本次子会话快照里，不落盘、不进管理目录；
//! - 本模块写的是用户可见、可编辑、可删除的持久配置，供以后反复派遣。
//!   系统提示要求模型默认优先临时定义，只在用户明确要求固化时使用。

use super::*;
use denia_core::subagent::{
    ModelChoice, ProfileWriteScope, SUBAGENT_SCHEMA_VERSION, SubagentProfile, ToolSelection,
    child_hard_denied,
};
use denia_tools::runtime_command::DefineSubagentArgs;

impl Runtime {
    /// 创建或覆盖一个永久子代理定义。
    pub(super) async fn define_subagent(
        &self,
        args: DefineSubagentArgs,
        ctx: &ToolContext,
    ) -> Result<Value, String> {
        let owner = ctx.session_id.as_deref().ok_or("缺少会话身份")?;
        let parent = self.live(owner).await?;
        // 硬边界：定义管理是宿主配置能力，子代理无论被授予什么都不得调用。
        if parent.session.header().subagent.is_some() {
            return Err(
                "subagent/host-administration:子代理不能创建或修改子代理定义；请把需求交回主代理"
                    .into(),
            );
        }

        let scope = match args.scope.as_deref().unwrap_or("user") {
            "user" => ProfileWriteScope::User,
            "project" => ProfileWriteScope::Project,
            other => {
                return Err(format!(
                    "subagent/scope-forbidden:scope 只能是 user 或 project，收到 {other}"
                ));
            }
        };
        if scope == ProfileWriteScope::Project && !ctx.cwd.is_dir() {
            return Err(format!(
                "subagent/scope-forbidden:项目级定义需要有效的工作目录，当前 {} 不可用",
                ctx.cwd.display()
            ));
        }
        let project_root = ctx
            .cwd
            .is_dir()
            .then(|| crate::subagents::project_root_for(&ctx.cwd));

        let tools = args.tools.unwrap_or(ToolSelection::Inherit);
        if let ToolSelection::Allowlist { names } = &tools {
            let driver = self
                .inner
                .driver
                .get()
                .and_then(Weak::upgrade)
                .ok_or("会话驱动器尚未就绪，暂时无法校验工具名")?;
            let registered: std::collections::BTreeSet<String> =
                driver.tools().names().into_iter().collect();
            let unknown: Vec<&String> = names
                .iter()
                .filter(|name| !registered.contains(name.as_str()))
                .collect();
            if !unknown.is_empty() {
                return Err(format!(
                    "subagent/tool-unknown:工具未注册：{}",
                    unknown
                        .iter()
                        .map(|name| name.as_str())
                        .collect::<Vec<_>>()
                        .join("、")
                ));
            }
            let denied: Vec<&String> = names
                .iter()
                .filter(|name| child_hard_denied(name))
                .collect();
            if !denied.is_empty() {
                return Err(format!(
                    "subagent/tool-hard-denied:这些工具子代理永远不能使用：{}",
                    denied
                        .iter()
                        .map(|name| name.as_str())
                        .collect::<Vec<_>>()
                        .join("、")
                ));
            }
        }

        let model = args.model.unwrap_or_default();
        let mut warnings: Vec<String> = Vec::new();
        if let ModelChoice::Explicit { selection } = &model
            && let Err(error) = self
                .inner
                .registry
                .resolve_call(
                    &selection.provider,
                    &selection.model,
                    selection.reasoning_effort.as_deref(),
                )
                .await
        {
            // 定义仍允许保存：派遣时会回退跟随主会话（与 delegate 同口径），
            // 用户以后也可能补上这个 provider。这里只把事实说清楚。
            warnings.push(format!(
                "模型 {}/{} 当前不可用（{}）；派遣时会回退为跟随主会话",
                selection.provider, selection.model, error
            ));
        }

        let profile = SubagentProfile {
            schema_version: SUBAGENT_SCHEMA_VERSION,
            id: args.id.trim().to_string(),
            name: args.name.trim().to_string(),
            description: args.description.trim().to_string(),
            instructions: args.instructions.clone(),
            enabled: true,
            tools,
            model,
            permission_ceiling: args.permission_ceiling.unwrap_or_default(),
            color: None,
        };
        profile.validate().map_err(|issues| {
            format!(
                "subagent/profile-invalid:{}",
                issues
                    .iter()
                    .map(|issue| issue.message.as_str())
                    .collect::<Vec<_>>()
                    .join("；")
            )
        })?;
        let body = args.instructions;
        let overwrite = args.overwrite.unwrap_or(false);
        let store = self.inner.profiles.clone();
        let existing = store
            .catalog(project_root.as_deref())
            .effective
            .into_iter()
            .find(|view| view.profile.id == profile.id);
        let (view, action) = if let Some(existing) = existing {
            if !overwrite {
                return Err(format!(
                    "subagent/profile-exists:{} 已存在（{}）；如确实要覆盖，请显式传 overwrite=true",
                    existing.qualified_id, existing.qualified_id
                ));
            }
            let raw = existing.qualified_id.clone();
            let revision = existing.revision;
            let root = project_root.clone();
            let profile = profile.clone();
            let body = body.clone();
            let store = store.clone();
            let result = tokio::task::spawn_blocking(move || {
                store.update(root.as_deref(), &raw, profile, body, revision)
            })
            .await
            .map_err(|error| format!("subagent/write:写定义任务失败：{error}"))?;
            (result.map_err(|error| error.render())?, "updated")
        } else {
            let root = project_root.clone();
            let profile = profile.clone();
            let body = body.clone();
            let store = store.clone();
            let result = tokio::task::spawn_blocking(move || {
                store.create(scope, root.as_deref(), profile, body)
            })
            .await
            .map_err(|error| format!("subagent/write:写定义任务失败：{error}"))?;
            (result.map_err(|error| error.render())?, "created")
        };

        let _ = self.inner.events.send(ServerEvent::SubagentsUpdated);
        let mut trace = crate::trajectory::TraceRecord::now(owner, "subagent");
        trace.operation = Some("defined".into());
        trace.agent_id = Some(owner.to_string());
        trace.call_id = Some(view.qualified_id.clone());
        trace.data = Some(json!({
            "action": action,
            "qualifiedId": view.qualified_id,
            "source": view.source,
            "scope": match scope {
                ProfileWriteScope::User => "user",
                ProfileWriteScope::Project => "project",
            },
            "model": view.profile.model.clone(),
            "warnings": warnings.clone(),
        }));
        self.inner.trace.record(&trace);
        Ok(json!({
            "action": action,
            "qualifiedId": view.qualified_id,
            "source": view.source,
            "revision": view.revision,
            "editable": view.editable,
            "profile": view.profile,
            "warnings": warnings,
            "note": "永久定义已写入子代理管理页；用户可查看、编辑或删除。",
        }))
    }
}
