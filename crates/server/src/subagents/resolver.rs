//! 派遣解析：把"选哪个定义"变成可执行的派遣规格。
//!
//! 纯解析与校验，不创建进程、不写会话——那些是 `Runtime::delegate` 的事。
//! 三种来源在这里合流：
//!
//! - `profile_id`：已保存定义（qualifiedId 或逻辑 id），走名册；
//! - `inline`：临时定义，不落盘、不进名册；
//! - 都没给：有效 `develop`；但**带旧字段**时走保守的兼容映射，
//!   绝不把旧调用悄悄升级成写能力。

use std::collections::BTreeSet;
use std::path::Path;

use denia_core::subagent::{
    DelegateArgs, DelegateTarget, INLINE_SUBAGENT_ID, InlineSubagentSpec, LEGACY_SUBAGENT_READ_ONLY_TOOLS,
    ModelChoice, PermissionCeiling, ProfileRef, SubagentProfile, ToolChoice, is_child_hard_denied,
};
use serde_json::{Value, json};

use super::policy::{EffectiveToolGrant, GrantRequest, grant_for};
use super::profiles::{ResolvedProfile, SubagentError, SubagentProfileStore};

/// 解析结果：一份可执行的定义 + 它的来源身份与解析诊断。
#[derive(Debug, Clone)]
pub struct DispatchDefinition {
    pub profile: SubagentProfile,
    /// 已保存定义才有引用；inline 与旧式兼容映射都没有。
    pub reference: Option<ProfileRef>,
    /// 面向错误的显示名（`builtin:develop` / `inline(临时定义)`）。
    pub display_id: String,
    /// 旧式兼容映射：`persona` 已经并进 instructions。
    pub legacy: bool,
    pub note: Option<String>,
    /// 调用级对工具集合的进一步收窄（只能更小）。
    pub narrowing: Option<Vec<String>>,
}

impl DispatchDefinition {
    /// 工具结果里的定义摘要：只报身份，不把 instructions 全文回给模型。
    pub fn summary(&self) -> Value {
        json!({
            "id": self.profile.id,
            "qualifiedId": self.reference.as_ref().map(|r| r.qualified_id.clone()),
            "name": self.profile.name,
            "source": self.reference.as_ref().map(|r| r.source.as_str()).unwrap_or("inline"),
            "revision": self.reference.as_ref().map(|r| r.revision.clone()),
        })
    }
}

fn from_resolved(resolved: ResolvedProfile, narrowing: Option<Vec<String>>) -> DispatchDefinition {
    DispatchDefinition {
        reference: Some(ProfileRef {
            qualified_id: resolved.qualified_id.clone(),
            id: resolved.profile.id.clone(),
            source: resolved.source,
            revision: resolved.revision,
        }),
        display_id: resolved.qualified_id,
        profile: resolved.profile,
        legacy: false,
        note: None,
        narrowing,
    }
}

/// 解析一次派遣请求指向的定义。
pub fn resolve_definition(
    store: &SubagentProfileStore,
    target: &DelegateTarget,
    project_root: Option<&Path>,
    args: &DelegateArgs,
) -> Result<DispatchDefinition, String> {
    if args.persona.is_some() && matches!(target, DelegateTarget::Profile(_)) {
        return Err(
            "subagent/ambiguous-target: persona（旧字段）不能与 profile_id 同时出现，\
             角色提示请写进定义的 instructions"
                .into(),
        );
    }
    match target {
        DelegateTarget::Profile(id) => {
            let resolved = store.resolve(id, project_root).map_err(|e| e.to_string())?;
            Ok(from_resolved(resolved, args.allowed_tools.clone()))
        }
        DelegateTarget::Inline(spec) => {
            let profile = spec.clone().into_profile();
            let diagnostics = profile.validate();
            if !diagnostics.is_empty() {
                let reasons: Vec<String> = diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.reason.clone())
                    .collect();
                return Err(format!(
                    "subagent/invalid-profile: 临时定义无法执行：{}",
                    reasons.join("；")
                ));
            }
            Ok(DispatchDefinition {
                reference: None,
                display_id: "inline(临时定义)".to_string(),
                profile,
                legacy: false,
                note: None,
                narrowing: args.allowed_tools.clone(),
            })
        }
        DelegateTarget::Default => {
            if args.persona.is_some() || args.allowed_tools.is_some() {
                return legacy_definition(args);
            }
            let resolved = store
                .resolve_default(project_root)
                .map_err(|e| e.to_string())?;
            Ok(from_resolved(resolved, None))
        }
    }
}

/// 旧式调用（只有 `persona` / `allowed_tools`）的兼容映射。
///
/// 旧工具集合上限就是当年的只读集合，显式列表也只能与它相交，再扣硬禁项；
/// 权限上限按只读构造——旧调用**不会**因为默认定义变成 develop 而长出新
/// 的写能力。被裁掉的工具名会出现在诊断里，不静默丢掉。
fn legacy_definition(args: &DelegateArgs) -> Result<DispatchDefinition, String> {
    let mut dropped: Vec<String> = Vec::new();
    let names: Vec<String> = match &args.allowed_tools {
        Some(requested) => requested
            .iter()
            .filter(|name| {
                let keep = LEGACY_SUBAGENT_READ_ONLY_TOOLS.contains(&name.as_str())
                    && !is_child_hard_denied(name);
                if !keep {
                    dropped.push((*name).clone());
                }
                keep
            })
            .cloned()
            .collect(),
        None => LEGACY_SUBAGENT_READ_ONLY_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
    };
    let description = args
        .description
        .clone()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "旧式派遣（兼容）".to_string());
    let profile = SubagentProfile {
        schema_version: denia_core::subagent::SUBAGENT_SCHEMA_VERSION,
        id: INLINE_SUBAGENT_ID.to_string(),
        name: description.chars().take(80).collect(),
        description,
        instructions: args.persona.clone().unwrap_or_default(),
        enabled: true,
        tools: ToolChoice::Allowlist { names },
        model: ModelChoice::Inherit,
        permission_ceiling: PermissionCeiling::ReadOnly,
        color: None,
    };
    let mut note = "旧式派遣参数已按兼容规则映射：只读工具集合、只读权限上限。要开放写能力请显式给 profile_id 或 inline".to_string();
    if !dropped.is_empty() {
        note.push_str(&format!(
            "；以下工具不在旧只读上限内，已丢弃：{}",
            dropped.join("、")
        ));
    }
    Ok(DispatchDefinition {
        reference: None,
        display_id: "legacy(兼容映射)".to_string(),
        profile,
        legacy: true,
        note: Some(note),
        narrowing: None,
    })
}

/// 计算最终生效的工具授权。
///
/// 调用级 `allowed_tools` 只能对所选定义进一步收窄；试图扩大（或塞进硬禁
/// 项）在这里失败，不会静默少给工具。
pub fn policy_grant(
    profile: &SubagentProfile,
    narrowing: Option<&[String]>,
    parent_grant: &[String],
) -> Result<EffectiveToolGrant, String> {
    let base = grant_for(&GrantRequest {
        parent_grant,
        choice: &profile.tools,
        permission_ceiling: profile.permission_ceiling,
    })
    .map_err(|error| error.to_string())?;
    let Some(narrow) = narrowing else {
        return Ok(base);
    };
    let denied: Vec<&str> = narrow
        .iter()
        .filter(|name| is_child_hard_denied(name))
        .map(String::as_str)
        .collect();
    if !denied.is_empty() {
        return Err(SubagentError::new(
            "subagent/hard-denied-tool",
            format!("子代理不能持有这些工具：{}", denied.join("、")),
        )
        .field("allowed_tools")
        .to_string());
    }
    let outside: Vec<&str> = narrow
        .iter()
        .filter(|name| !base.allows(name))
        .map(String::as_str)
        .collect();
    if !outside.is_empty() {
        return Err(SubagentError::new(
            "subagent/tool-not-granted",
            format!(
                "allowed_tools 不能超出所选定义请求的工具集合：{}",
                outside.join("、")
            ),
        )
        .field("allowed_tools")
        .to_string());
    }
    let unique: BTreeSet<String> = narrow.iter().cloned().collect();
    Ok(EffectiveToolGrant {
        tools: unique.into_iter().collect(),
        permission_ceiling: base.permission_ceiling,
    })
}

/// inline 规格的构造辅助（测试与 API 预览共用）。
pub fn inline_spec_from(profile: &SubagentProfile) -> InlineSubagentSpec {
    InlineSubagentSpec {
        name: profile.name.clone(),
        description: profile.description.clone(),
        instructions: profile.instructions.clone(),
        tools: profile.tools.clone(),
        model: profile.model.clone(),
        permission_ceiling: profile.permission_ceiling,
        color: profile.color.clone(),
    }
}
