//! 统一工具授权计算：schema、目录与执行器消费同一份结果。
//!
//! 计算公式（执行计划 §6.1）：
//!
//! ```text
//! parentGrant = 父会话有效 preset/features 限制后的可授予工具（注册能力）
//! requested   = inherit → parentGrant；allowlist → 指定集合
//! effective   = parentGrant ∩ requested − childHardDenied
//! ```
//!
//! 显式选择父未授予的工具**失败**，不静默少给：静默少给会让模型以为拿到了
//! 工具，实际每次调用都被拒，排查起来是"工具莫名其妙不工作"。
//! `inherit` 自动扣掉硬禁项（用户没点名要它，只是继承的自然结果）。

use std::collections::BTreeSet;

use denia_core::subagent::{
    PermissionCeiling, ToolChoice, is_child_hard_denied, HOST_ADMINISTRATION_TOOLS,
};

use super::profiles::SubagentError;

/// 一次派遣最终生效的工具授权。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveToolGrant {
    /// 已排序去重的工具名；空列表 = 零业务工具（纯推理任务，合法）。
    pub tools: Vec<String>,
    pub permission_ceiling: PermissionCeiling,
}

impl EffectiveToolGrant {
    pub fn allows(&self, name: &str) -> bool {
        self.tools.iter().any(|tool| tool == name)
    }
}

/// 派遣时的请求来源：定义或临时 inline 规格共用。
pub struct GrantRequest<'a> {
    /// 父会话当前可授予的工具（注册能力 ∩ preset features/tools）。
    pub parent_grant: &'a [String],
    pub choice: &'a ToolChoice,
    pub permission_ceiling: PermissionCeiling,
}

/// 计算一次派遣的有效工具授权。
pub fn grant_for(request: &GrantRequest<'_>) -> Result<EffectiveToolGrant, SubagentError> {
    let parent: BTreeSet<&str> = request.parent_grant.iter().map(String::as_str).collect();
    let tools = match request.choice {
        ToolChoice::Inherit => parent
            .iter()
            .filter(|name| !is_child_hard_denied(name))
            .map(|name| (*name).to_string())
            .collect(),
        ToolChoice::Allowlist { names } => {
            // 硬禁项：显式点名也不给，且大声失败（静默剔除会让"我明明勾了"变成谜案）。
            let denied: Vec<&String> = names
                .iter()
                .filter(|name| is_child_hard_denied(name))
                .collect();
            if !denied.is_empty() {
                let administration: Vec<&String> = denied
                    .iter()
                    .copied()
                    .filter(|name| HOST_ADMINISTRATION_TOOLS.contains(&name.as_str()))
                    .collect();
                let reason = if administration.is_empty() {
                    "子代理不能派遣子代理，也不能改变会话主控模式或操作会话目标"
                } else {
                    "宿主管理类工具只属于人类配置面，不下发给子代理"
                };
                return Err(SubagentError::new(
                    "subagent/hard-denied-tool",
                    format!(
                        "{reason}：{}",
                        denied
                            .iter()
                            .map(|name| name.as_str())
                            .collect::<Vec<_>>()
                            .join("、")
                    ),
                )
                .field("tools"));
            }
            // 父未授予的工具：逐项报原因。
            let ungarnted: Vec<&str> = names
                .iter()
                .filter(|name| !parent.contains(name.as_str()))
                .map(String::as_str)
                .collect();
            if !ungarnted.is_empty() {
                return Err(SubagentError::new(
                    "subagent/tool-not-granted",
                    format!(
                        "以下工具不在父代理当前可授予的集合里，子代理无法获得：{}",
                        ungarnted.join("、")
                    ),
                )
                .field("tools")
                .candidates(request.parent_grant.iter().cloned().collect()));
            }
            let unique: BTreeSet<String> = names.iter().cloned().collect();
            unique.into_iter().collect()
        }
    };
    Ok(EffectiveToolGrant {
        tools,
        permission_ceiling: request.permission_ceiling,
    })
}

/// 从注册表工具名与 preset 收窄结果算出父会话可授予的工具集合。
///
/// `registered` 是部署当前注册的**全部**能力（含已授权但尚未加载的 MCP 工具）；
/// `excluded` 是 preset features 关掉的工具；`whitelist` 是 preset 的工具白名单。
pub fn parent_grant(
    registered: &[String],
    excluded: &[&str],
    whitelist: Option<&[String]>,
) -> Vec<String> {
    let mut out: Vec<String> = registered
        .iter()
        .filter(|name| !excluded.contains(&name.as_str()))
        .filter(|name| whitelist.is_none_or(|list| list.iter().any(|item| item == *name)))
        .cloned()
        .collect();
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent() -> Vec<String> {
        ["bash", "read_file", "write_file", "spawn_agent", "create_preset", "edit"]
            .iter()
            .map(|name| (*name).to_string())
            .collect()
    }

    fn names(values: &[&str]) -> ToolChoice {
        ToolChoice::Allowlist {
            names: values.iter().map(|name| (*name).to_string()).collect(),
        }
    }

    fn grant(choice: &ToolChoice) -> Result<EffectiveToolGrant, SubagentError> {
        grant_for(&GrantRequest {
            parent_grant: &parent(),
            choice,
            permission_ceiling: PermissionCeiling::Inherit,
        })
    }

    #[test]
    fn inherit_excludes_hard_denied_only() {
        let result = grant(&ToolChoice::Inherit).unwrap();
        assert_eq!(result.tools, vec!["bash", "edit", "read_file", "write_file"]);
        assert!(!result.allows("spawn_agent"));
        assert!(!result.allows("create_preset"));
    }

    #[test]
    fn empty_allowlist_is_zero_tools_not_inherit() {
        let result = grant(&names(&[])).unwrap();
        assert!(result.tools.is_empty());
    }

    #[test]
    fn explicit_choice_cannot_reach_beyond_the_parent_grant() {
        let error = grant(&names(&["bash", "browser"])).unwrap_err();
        assert_eq!(error.code, "subagent/tool-not-granted");
        assert!(error.reason.contains("browser"), "{}", error.reason);
        assert!(!error.reason.contains("bash，"), "bash 是父已授予项，不该出现在拒绝列表");
    }

    #[test]
    fn explicit_choice_of_hard_denied_tool_fails_loudly() {
        let error = grant(&names(&["read_file", "fork_agent"])).unwrap_err();
        assert_eq!(error.code, "subagent/hard-denied-tool");
        assert!(error.reason.contains("fork_agent"));
    }

    #[test]
    fn allowlist_is_sorted_and_deduped() {
        let result = grant(&names(&["write_file", "bash", "bash"])).unwrap();
        assert_eq!(result.tools, vec!["bash", "write_file"]);
    }

    #[test]
    fn parent_grant_follows_preset_features_and_whitelist() {
        let registered: Vec<String> = ["bash", "browser", "read_file", "spawn_agent"]
            .iter()
            .map(|name| (*name).to_string())
            .collect();
        let grant = parent_grant(&registered, &["browser"], None);
        assert_eq!(grant, vec!["bash", "read_file", "spawn_agent"]);
        let narrowed = parent_grant(&registered, &[], Some(&["read_file".to_string()]));
        assert_eq!(narrowed, vec!["read_file"]);
    }
}
