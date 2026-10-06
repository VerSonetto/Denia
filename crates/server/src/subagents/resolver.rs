//! 派遣解析：定义/临时规格 + 父会话有效能力 → 不可变派遣快照。
//!
//! 本模块是**纯函数**：不读文件、不创建进程、不写会话。所有输入（已解析的
//! 定义、父会话的可授予工具、部署注册表）由调用方提供，因此授权规则可以
//! 脱离运行环境被完整测试。
//!
//! 授权公式（执行计划 6.1）：
//!
//! ```text
//! parentGrant  = 父会话有效 preset/features 与权限档限制后的可授予工具
//! requested    = inherit → parentGrant；allowlist → 指定集合
//! effective    = parentGrant ∩ requested − childHardDenied
//! runtimeAllowed = effective ∩ 当前部署仍可用工具（执行时按注册表判定）
//! ```

use std::collections::BTreeSet;

use denia_core::config::ModelSelection;
use denia_core::preset::PresetFeatures;
use denia_core::session::PermissionMode;
use denia_core::subagent::{
    ModelChoice, PermissionCeiling, ProfileSource, SubagentInlineSpec, ToolSelection,
    child_hard_denied, codes,
};
use denia_tools::runtime_command::DelegateArgs;

use super::profiles::{ProfileError, ResolvedProfile};

/// 只读权限档下从父可授予工具里摘掉的工具名。
///
/// 与 `agent-loop` 的只读收窄同源（bash/写文件），并额外摘掉 `job_start`：
/// `decide_for` 在只读档拒绝一切命令，把它留在 schema 里只会让子代理拿到
/// 一个"看得见、用不了"的工具。
const READ_ONLY_EXCLUDED: &[&str] = &["bash", "write_file", "edit", "job_start"];

/// 有效工具授权的来源（诊断与 UI 展示用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelOrigin {
    CallOverride,
    ProfileExplicit,
    Parent,
}

/// 派遣解析结果：冻结进子会话快照的不可变事实。
#[derive(Debug, Clone)]
pub struct ResolvedDispatch {
    /// 限定 id；临时定义为 `inline:<name>`。
    pub qualified_id: String,
    pub source: Option<ProfileSource>,
    pub revision: u64,
    pub inline: bool,
    pub fork: bool,
    pub name: String,
    pub description: String,
    pub instructions: String,
    /// 有效工具（排序去重后的明确列表）。
    pub tools: Vec<String>,
    pub selection: ModelSelection,
    pub model_origin: ModelOrigin,
    pub permission_ceiling: PermissionCeiling,
}

impl ResolvedDispatch {
    /// 派遣快照的稳定 hash：供 UI/审计比对"实际生效的规格"。
    pub fn fingerprint(&self) -> String {
        let material = format!(
            "{}\u{1}{}\u{1}{}",
            self.qualified_id,
            self.tools.join(","),
            self.instructions
        );
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in material.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{hash:016x}")
    }
}

/// 父会话可授予的工具集合。
///
/// 三处收窄，任一命中即摘除，绝不放大：
/// 1. 部署注册表（只有真实注册的工具才可能被授予）；
/// 2. 父 preset / features（`features` 关闭的工具族与 preset 白名单）；
/// 3. 父当前权限档（只读档不给命令与写工具——schema 与执行口径一致）。
pub fn parent_grant(
    registered: &[String],
    features: &PresetFeatures,
    preset_tools: Option<&[String]>,
    mode: PermissionMode,
) -> Vec<String> {
    let excluded: Vec<&str> = features.excluded_tools();
    let mut grant: Vec<String> = registered
        .iter()
        .filter(|name| !excluded.contains(&name.as_str()))
        .filter(|name| preset_tools.is_none_or(|list| list.iter().any(|item| item == *name)))
        .filter(|name| !(mode.is_read_only() && READ_ONLY_EXCLUDED.contains(&name.as_str())))
        .cloned()
        .collect();
    grant.sort();
    grant.dedup();
    grant
}

/// 计算有效工具授权。
///
/// - `inherit`：继承父可授予工具，自动排除硬禁项；
/// - `allowlist`：显式集合；未知工具、硬禁项、父未授予的工具分别报错
///   （**不**静默少给），空数组合法（零业务工具）。
pub fn compute_tool_grant(
    parent_grant: &[String],
    selection: &ToolSelection,
    registered: &[String],
) -> Result<Vec<String>, ProfileError> {
    let requested: Vec<String> = match selection {
        ToolSelection::Inherit => parent_grant.to_vec(),
        ToolSelection::Allowlist { names } => denia_core::subagent::dedup_tool_names(names.clone()),
    };
    let registered_set: BTreeSet<&str> = registered.iter().map(String::as_str).collect();
    let unknown: Vec<String> = requested
        .iter()
        .filter(|name| !registered_set.contains(name.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        let mut error = ProfileError::new(
            codes::TOOL_UNKNOWN,
            format!("工具选择里包含部署未注册的工具：{}", unknown.join("、")),
        )
        .field("tools");
        error.candidates = registered.to_vec();
        return Err(error);
    }
    if matches!(selection, ToolSelection::Allowlist { .. }) {
        let hard: Vec<String> = requested
            .iter()
            .filter(|name| child_hard_denied(name))
            .cloned()
            .collect();
        if !hard.is_empty() {
            return Err(ProfileError::new(
                codes::TOOL_HARD_DENIED,
                format!(
                    "子代理不能使用这些工具（禁止派遣子代理/宿主配置/会话主控）：{}",
                    hard.join("、")
                ),
            )
            .field("tools"));
        }
        let missing: Vec<String> = requested
            .iter()
            .filter(|name| !parent_grant.contains(name))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(ProfileError::new(
                codes::TOOL_NOT_GRANTED,
                format!(
                    "父会话当前未授予这些工具，子代理不能把它重新授予：{}",
                    missing.join("、")
                ),
            )
            .field("tools"));
        }
    }
    let mut effective: Vec<String> = requested
        .into_iter()
        .filter(|name| parent_grant.contains(name))
        .filter(|name| !child_hard_denied(name))
        .collect();
    effective.sort();
    effective.dedup();
    Ok(effective)
}

/// 校验调用参数（`DelegateArgs` 的严格契约）。
///
/// 覆盖：空 prompt、被移除的 `max_depth`、profile/inline 互斥、旧字段与新
/// 规格同时出现的歧义、模型覆盖必须成套。
pub fn validate_delegate_args(args: &DelegateArgs) -> Result<(), ProfileError> {
    if args.prompt.trim().is_empty() {
        return Err(ProfileError::new(codes::PROFILE_INVALID, "prompt 不能为空").field("prompt"));
    }
    if args.max_depth.is_some() {
        return Err(ProfileError::new(
            codes::DEPTH_CONFIG_REMOVED,
            "max_depth 已移除：子代理禁止派遣子代理，不存在可配置的委派深度；请删除该参数",
        )
        .field("max_depth"));
    }
    if args.profile_id.is_some() && args.inline.is_some() {
        return Err(ProfileError::new(
            codes::AMBIGUOUS_SPEC,
            "profile_id 与 inline 互斥：要么使用已保存的定义，要么在调用时给出临时定义",
        )
        .field("inline"));
    }
    let uses_profile = args.profile_id.is_some() || args.inline.is_some();
    let uses_legacy = args.persona.is_some() || args.allowed_tools.is_some();
    if uses_profile && uses_legacy {
        return Err(ProfileError::new(
            codes::AMBIGUOUS_SPEC,
            "persona/allowed_tools 是旧参数，不能与 profile_id/inline 同时出现；\
             请把角色与工具写进定义或 inline 规格",
        )
        .field("persona"));
    }
    if args.provider.is_some() && args.model.is_none() {
        return Err(ProfileError::new(
            codes::PROFILE_INVALID,
            "换 provider 时必须同时给出 model：否则会误继承另一个 provider 的 model/effort",
        )
        .field("provider"));
    }
    Ok(())
}

/// 解析最终模型选择。
///
/// 优先级：合法调用覆盖 > 定义显式选择 > 父模型。
/// - 换 provider 必须成套给出 provider+model（否则拒绝，见
///   [`validate_delegate_args`]），此时不继承父的 effort。
/// - 只给 model（沿用父 provider）同样不继承父的 effort：effort 是模型级参数，
///   换模型后沿用可能不合法。
pub fn resolve_selection(
    parent: &ModelSelection,
    choice: &ModelChoice,
    args: &DelegateArgs,
) -> (ModelSelection, ModelOrigin) {
    match (args.provider.as_deref(), args.model.as_deref()) {
        (Some(provider), Some(model)) => (
            ModelSelection {
                provider: provider.to_string(),
                model: model.to_string(),
                reasoning_effort: args.reasoning_effort.clone(),
            },
            ModelOrigin::CallOverride,
        ),
        (None, Some(model)) => (
            ModelSelection {
                provider: parent.provider.clone(),
                model: model.to_string(),
                reasoning_effort: args.reasoning_effort.clone(),
            },
            ModelOrigin::CallOverride,
        ),
        _ => {
            if let Some(selection) = choice.selection() {
                let mut selection = selection.clone();
                if let Some(effort) = &args.reasoning_effort {
                    selection.reasoning_effort = Some(effort.clone());
                }
                return (selection, ModelOrigin::ProfileExplicit);
            }
            let mut selection = parent.clone();
            if let Some(effort) = &args.reasoning_effort {
                selection.reasoning_effort = Some(effort.clone());
            }
            (selection, ModelOrigin::Parent)
        }
    }
}

/// fork 的模型必须与父一致：跨模型 fork 会把不兼容的思考载荷复制过去。
pub fn enforce_fork_model(
    fork: bool,
    selection: &ModelSelection,
    parent: &ModelSelection,
) -> Result<(), ProfileError> {
    if fork && selection != parent {
        return Err(ProfileError::new(
            codes::PROFILE_INVALID,
            "fork_agent 必须沿用父模型（跨模型 fork 会复制不兼容的思考载荷）；\
             需要另一个模型请改用 spawn_agent",
        )
        .field("model"));
    }
    Ok(())
}

/// 权限上限 → 实际权限档：只允许收窄，绝不放大。
pub fn effective_permission_mode(
    ceiling: PermissionCeiling,
    parent_mode: PermissionMode,
) -> PermissionMode {
    match ceiling {
        PermissionCeiling::Inherit => parent_mode,
        PermissionCeiling::ReadOnly => PermissionMode::ReadOnly,
    }
}

/// 只读上限下被硬拒的工具（执行层兜底；schema 层已摘除）。
pub fn ceiling_denied_tool(ceiling: PermissionCeiling, tool: &str) -> bool {
    if ceiling != PermissionCeiling::ReadOnly {
        return false;
    }
    matches!(
        tool,
        "write_file" | "edit" | "bash" | "job_start" | "todo_write"
    )
}

/// 把调用参数解析成派遣规格。
///
/// `profile` 为已解析的定义（`profile_id` 或默认 develop 时给出）；`inline`
/// 时由调用方给出临时规格。两者由 [`validate_delegate_args`] 保证互斥。
pub fn resolve_dispatch(
    args: &DelegateArgs,
    fork: bool,
    parent_selection: &ModelSelection,
    parent_grant: &[String],
    registered: &[String],
    profile: Option<&ResolvedProfile>,
    default_route: bool,
) -> Result<ResolvedDispatch, ProfileError> {
    validate_delegate_args(args)?;
    let (qualified_id, source, revision, inline, name, description, instructions, choice, ceiling) =
        if let Some(spec) = &args.inline {
            if let Err(issues) = spec.validate() {
                let message = issues
                    .iter()
                    .map(|issue| issue.message.as_str())
                    .collect::<Vec<_>>()
                    .join(";");
                return Err(ProfileError::new(codes::PROFILE_INVALID, message).field("inline"));
            }
            inline_parts(spec)
        } else if let Some(profile) = profile {
            (
                profile.qualified_id.clone(),
                Some(profile.source),
                profile.revision,
                false,
                profile.profile.name.clone(),
                profile.profile.description.clone(),
                profile.profile.instructions.clone(),
                profile.profile.model.clone(),
                profile.profile.permission_ceiling,
            )
        } else {
            // 没有 profile 也没有 inline：只能是"省略即用 develop"的默认路由，
            // 由调用方先解析好并传进来；走到这里说明调用方漏了解析。
            return Err(ProfileError::new(
                codes::PROFILE_NOT_FOUND,
                if default_route {
                    "默认的 develop 定义当前不可用；请在调用里显式指定 profile_id 或 inline 规格"
                } else {
                    "派遣缺少定义：请给出 profile_id 或 inline 规格"
                },
            )
            .field("profile_id"));
        };

    let selection_input = choice.clone();
    let tools = if let Some(spec) = &args.inline {
        compute_tool_grant(parent_grant, &spec.tools, registered)?
    } else if let Some(profile) = profile {
        compute_tool_grant(parent_grant, &profile.profile.tools, registered)?
    } else {
        Vec::new()
    };
    let (selection, model_origin) = resolve_selection(parent_selection, &selection_input, args);
    enforce_fork_model(fork, &selection, parent_selection)?;
    let ok = (
        qualified_id,
        source,
        revision,
        inline,
        name,
        description,
        instructions,
    );
    Ok(ResolvedDispatch {
        qualified_id: ok.0,
        source: ok.1,
        revision: ok.2,
        inline: ok.3,
        fork,
        name: ok.4,
        description: args
            .description
            .clone()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or(ok.5),
        instructions: ok.6,
        tools,
        selection,
        model_origin,
        permission_ceiling: ceiling,
    })
}

#[allow(clippy::type_complexity)]
fn inline_parts(
    spec: &SubagentInlineSpec,
) -> (
    String,
    Option<ProfileSource>,
    u64,
    bool,
    String,
    String,
    String,
    ModelChoice,
    PermissionCeiling,
) {
    (
        // 临时定义不进管理目录，但快照要能显示它从哪来。
        format!("inline:{}", spec.name.trim()),
        None,
        0,
        true,
        spec.name.clone(),
        spec.description.clone(),
        spec.instructions.clone(),
        spec.model.clone(),
        spec.permission_ceiling,
    )
}

/// 旧描述符的保守授权：按历史只读集合构造，绝不等于 inherit。
///
/// 计划 12.3：旧 descriptor 缺授权列表不能等价 inherit；显式旧列表也最多与
/// 历史上限相交，再扣硬禁项。权威实现在 core，避免 agent-loop 与 server
/// 各写一份解释。
pub fn legacy_grant(explicit: Option<&[String]>) -> Vec<String> {
    denia_core::subagent::legacy_child_tools(explicit)
}

/// 旧参数路径（`persona` / `allowed_tools`）：映射为角色补充 + 显式列表。
///
/// 绝不因此拿到全工具：显式列表最多与历史只读上限相交，再扣硬禁项；缺列表
/// 时按历史只读集合保守构造。与新规格同时出现由
/// [`validate_delegate_args`] 判为歧义并拒绝。
pub fn legacy_dispatch(
    args: &DelegateArgs,
    fork: bool,
    parent_selection: &ModelSelection,
    registered: &[String],
) -> Result<ResolvedDispatch, ProfileError> {
    // 未知工具名先报错（不静默丢弃）；硬禁项与超出历史上限的项按交集静默扣除。
    let unknown: Vec<String> = args
        .allowed_tools
        .iter()
        .flatten()
        .filter(|name| !registered.contains(name))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        let mut error = ProfileError::new(
            codes::TOOL_UNKNOWN,
            format!(
                "旧 allowed_tools 里包含部署未注册的工具：{}",
                unknown.join("、")
            ),
        )
        .field("allowed_tools");
        error.candidates = registered.to_vec();
        return Err(error);
    }
    let tools = legacy_grant(args.allowed_tools.as_deref());
    let (selection, _) = resolve_selection(parent_selection, &ModelChoice::Inherit, args);
    enforce_fork_model(fork, &selection, parent_selection)?;
    let name = args
        .description
        .clone()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| "子代理".to_string());
    Ok(ResolvedDispatch {
        qualified_id: format!("legacy:{}", name.trim()),
        source: None,
        revision: 0,
        inline: false,
        fork,
        name: name.clone(),
        description: name,
        instructions: args.persona.clone().unwrap_or_default(),
        tools,
        selection,
        model_origin: ModelOrigin::Parent,
        permission_ceiling: PermissionCeiling::Inherit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::subagent::SubagentInlineSpec;

    fn registered() -> Vec<String> {
        [
            "read_file",
            "read_tool_output",
            "ls",
            "glob",
            "grep",
            "skill",
            "web_fetch",
            "write_file",
            "edit",
            "bash",
            "todo_write",
            "send_message",
            "spawn_agent",
            "fork_agent",
            "list_agents",
            "wait_agent",
            "interrupt_agent",
            "exit_plan",
            "get_goal",
            "update_goal",
            "create_preset",
            "ask",
            "browser",
            "job_start",
            "job_list",
            "job_output",
            "job_kill",
            "mcp__srv__tool",
        ]
        .iter()
        .map(|name| (*name).to_string())
        .collect()
    }

    fn args(prompt: &str) -> DelegateArgs {
        DelegateArgs {
            prompt: prompt.to_string(),
            description: None,
            profile_id: None,
            inline: None,
            provider: None,
            model: None,
            reasoning_effort: None,
            allowed_tools: None,
            run_in_background: None,
            persona: None,
            max_depth: None,
        }
    }

    fn parent() -> ModelSelection {
        ModelSelection {
            provider: "p1".into(),
            model: "m1".into(),
            reasoning_effort: Some("high".into()),
        }
    }

    #[test]
    fn parent_grant_narrows_by_features_preset_and_read_only() {
        let all = registered();
        let features = PresetFeatures::default();
        let with_all = parent_grant(&all, &features, None, PermissionMode::AutoEdit);
        assert!(with_all.contains(&"bash".to_string()));
        assert!(with_all.contains(&"spawn_agent".to_string()));
        // 只读档摘掉命令与写工具（schema 与执行一致）。
        let read_only = parent_grant(&all, &features, None, PermissionMode::ReadOnly);
        for name in ["bash", "write_file", "edit", "job_start"] {
            assert!(
                !read_only.contains(&name.to_string()),
                "{name} 应在只读档被摘除"
            );
        }
        assert!(read_only.contains(&"read_file".to_string()));
        // features 关闭 subagents：派遣工具不再可授予。
        let mut no_subagents = features;
        no_subagents.subagents = false;
        let narrowed = parent_grant(&all, &no_subagents, None, PermissionMode::AutoEdit);
        assert!(!narrowed.contains(&"spawn_agent".to_string()));
        // preset 白名单继续收窄。
        let preset_tools = vec!["read_file".to_string(), "bash".to_string()];
        let listed = parent_grant(
            &all,
            &features,
            Some(&preset_tools),
            PermissionMode::AutoEdit,
        );
        assert_eq!(listed, vec!["bash".to_string(), "read_file".to_string()]);
    }

    #[test]
    fn inherit_excludes_hard_denied_and_allowlist_reports_each_reason() {
        let all = registered();
        let grant = parent_grant(
            &all,
            &PresetFeatures::default(),
            None,
            PermissionMode::AutoEdit,
        );
        let effective = compute_tool_grant(&grant, &ToolSelection::Inherit, &all).unwrap();
        for name in [
            "spawn_agent",
            "fork_agent",
            "exit_plan",
            "get_goal",
            "create_preset",
        ] {
            assert!(!effective.contains(&name.to_string()), "{name} 必须被硬禁");
        }
        // send_message 是汇报通道，inherit 下保留。
        assert!(effective.contains(&"send_message".to_string()));

        // 显式 allowlist 含硬禁项 → 校验失败。
        let error = compute_tool_grant(
            &grant,
            &ToolSelection::Allowlist {
                names: vec!["read_file".into(), "spawn_agent".into()],
            },
            &all,
        )
        .unwrap_err();
        assert_eq!(error.code, codes::TOOL_HARD_DENIED);

        // 未知工具 → 报错并给出候选。
        let error = compute_tool_grant(
            &grant,
            &ToolSelection::Allowlist {
                names: vec!["nope".into()],
            },
            &all,
        )
        .unwrap_err();
        assert_eq!(error.code, codes::TOOL_UNKNOWN);
        assert!(!error.candidates.is_empty());

        // 父未授予 → 报错（不静默少给）。
        let restricted = vec!["read_file".to_string()];
        let error = compute_tool_grant(
            &restricted,
            &ToolSelection::Allowlist {
                names: vec!["read_file".into(), "bash".into()],
            },
            &all,
        )
        .unwrap_err();
        assert_eq!(error.code, codes::TOOL_NOT_GRANTED);
        assert!(error.message.contains("bash"));

        // 空 allowlist = 零业务工具，合法。
        let empty = compute_tool_grant(
            &grant,
            &ToolSelection::Allowlist { names: Vec::new() },
            &all,
        )
        .unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn delegate_args_reject_removed_depth_and_ambiguity() {
        let mut depth = args("do");
        depth.max_depth = Some(serde_json::json!(3));
        assert_eq!(
            validate_delegate_args(&depth).unwrap_err().code,
            codes::DEPTH_CONFIG_REMOVED
        );

        let mut both = args("do");
        both.profile_id = Some("builtin:develop".into());
        both.inline = Some(SubagentInlineSpec {
            name: "x".into(),
            description: "y".into(),
            instructions: String::new(),
            tools: ToolSelection::Inherit,
            model: ModelChoice::Inherit,
            permission_ceiling: PermissionCeiling::Inherit,
        });
        assert_eq!(
            validate_delegate_args(&both).unwrap_err().code,
            codes::AMBIGUOUS_SPEC
        );

        let mut legacy_mix = args("do");
        legacy_mix.profile_id = Some("builtin:develop".into());
        legacy_mix.allowed_tools = Some(vec!["read_file".into()]);
        assert_eq!(
            validate_delegate_args(&legacy_mix).unwrap_err().code,
            codes::AMBIGUOUS_SPEC
        );

        // 换 provider 必须成套：只给 provider 会被拒绝（会误继承别家的 model）。
        let mut half_model = args("do");
        half_model.provider = Some("p".into());
        assert!(validate_delegate_args(&half_model).is_err());
        // 只给 model = 沿用父 provider 换模型，合法。
        let mut model_only = args("do");
        model_only.model = Some("m".into());
        assert!(validate_delegate_args(&model_only).is_ok());

        let mut empty = args("   ");
        empty.profile_id = Some("builtin:develop".into());
        assert!(validate_delegate_args(&empty).is_err());
        assert!(validate_delegate_args(&args("ok")).is_ok());
    }

    #[test]
    fn model_priority_is_call_then_profile_then_parent() {
        let parent = parent();
        let (selection, origin) = resolve_selection(&parent, &ModelChoice::Inherit, &args("x"));
        assert_eq!(selection, parent);
        assert_eq!(origin, ModelOrigin::Parent);

        let explicit = ModelChoice::Explicit {
            selection: ModelSelection {
                provider: "p2".into(),
                model: "m2".into(),
                reasoning_effort: Some("low".into()),
            },
        };
        let (selection, origin) = resolve_selection(&parent, &explicit, &args("x"));
        assert_eq!(selection.model, "m2");
        assert_eq!(origin, ModelOrigin::ProfileExplicit);

        let mut override_args = args("x");
        override_args.provider = Some("p3".into());
        override_args.model = Some("m3".into());
        let (selection, origin) = resolve_selection(&parent, &explicit, &override_args);
        assert_eq!(selection.provider, "p3");
        assert_eq!(selection.model, "m3");
        // 换 provider 不继承父的 effort。
        assert_eq!(selection.reasoning_effort, None);
        assert_eq!(origin, ModelOrigin::CallOverride);

        // 只给 model（沿用父 provider）同样不继承父 effort，但显式 effort 生效。
        let mut same = args("x");
        same.model = Some("m9".into());
        let (selection, _) = resolve_selection(&parent, &explicit, &same);
        assert_eq!(selection.provider, "p1");
        assert_eq!(selection.model, "m9");
        assert_eq!(selection.reasoning_effort, None);
        same.reasoning_effort = Some("low".into());
        let (selection, _) = resolve_selection(&parent, &explicit, &same);
        assert_eq!(selection.reasoning_effort, Some("low".into()));
    }

    #[test]
    fn fork_refuses_a_different_model() {
        let parent = parent();
        let other = ModelSelection {
            provider: "p2".into(),
            model: "m2".into(),
            reasoning_effort: None,
        };
        assert!(enforce_fork_model(true, &other, &parent).is_err());
        assert!(enforce_fork_model(true, &parent, &parent).is_ok());
        assert!(enforce_fork_model(false, &other, &parent).is_ok());
    }

    #[test]
    fn permission_ceiling_only_narrows() {
        assert_eq!(
            effective_permission_mode(PermissionCeiling::Inherit, PermissionMode::Full),
            PermissionMode::Full
        );
        assert_eq!(
            effective_permission_mode(PermissionCeiling::ReadOnly, PermissionMode::Full),
            PermissionMode::ReadOnly
        );
        assert_eq!(
            effective_permission_mode(PermissionCeiling::ReadOnly, PermissionMode::AutoEdit),
            PermissionMode::ReadOnly
        );
        assert!(ceiling_denied_tool(
            PermissionCeiling::ReadOnly,
            "write_file"
        ));
        assert!(ceiling_denied_tool(PermissionCeiling::ReadOnly, "bash"));
        assert!(!ceiling_denied_tool(
            PermissionCeiling::Inherit,
            "write_file"
        ));
        assert!(!ceiling_denied_tool(
            PermissionCeiling::ReadOnly,
            "read_file"
        ));
    }

    #[test]
    fn legacy_grant_is_conservative_and_never_inherit() {
        let empty = legacy_grant(None);
        assert!(empty.contains(&"read_file".to_string()));
        assert!(!empty.contains(&"write_file".to_string()));
        assert!(!empty.contains(&"bash".to_string()));
        // 旧日志里写了写工具，也最多与历史只读上限相交。
        let explicit = legacy_grant(Some(&[
            "read_file".to_string(),
            "write_file".to_string(),
            "browser".to_string(),
        ]));
        assert_eq!(
            explicit,
            vec!["browser".to_string(), "read_file".to_string()]
        );
        // 硬禁项即使写在旧日志里也被扣掉。
        let with_denied = legacy_grant(Some(&["spawn_agent".to_string(), "ls".to_string()]));
        assert_eq!(with_denied, vec!["ls".to_string()]);
    }

    #[test]
    fn inline_dispatch_freezes_tools_and_rejects_ungranted() {
        let all = registered();
        let grant = parent_grant(
            &all,
            &PresetFeatures::default(),
            None,
            PermissionMode::AutoEdit,
        );
        let mut delegate = args("调查缓存");
        delegate.inline = Some(SubagentInlineSpec {
            name: "缓存调查员".into(),
            description: "专注缓存读写与失效逻辑调查".into(),
            instructions: "不修改文件".into(),
            tools: ToolSelection::Allowlist {
                names: vec!["read_file".into(), "grep".into()],
            },
            model: ModelChoice::Inherit,
            permission_ceiling: PermissionCeiling::ReadOnly,
        });
        let resolved =
            resolve_dispatch(&delegate, false, &parent(), &grant, &all, None, false).unwrap();
        assert!(resolved.inline);
        assert_eq!(
            resolved.tools,
            vec!["grep".to_string(), "read_file".to_string()]
        );
        assert_eq!(resolved.permission_ceiling, PermissionCeiling::ReadOnly);
        assert!(resolved.qualified_id.starts_with("inline:"));

        // inline 选择父未授予的工具 → 失败，不静默少给。
        let restricted = vec!["read_file".to_string()];
        let error = resolve_dispatch(&delegate, false, &parent(), &restricted, &all, None, false)
            .unwrap_err();
        assert_eq!(error.code, codes::TOOL_NOT_GRANTED);
    }
}
