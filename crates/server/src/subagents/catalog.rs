//! 工具目录：给管理页与派遣预览一份"部署里真实存在什么、能不能授予"的清单。
//!
//! 名字与描述来自**部署已注册的工具表**，不硬编码外部参考实现的命名；分类与
//! 硬禁原因在这里补上。目录只描述可能性，真正的收窄由
//! [`super::policy::grant_for`] 按父会话的有效授权算。

use denia_core::subagent::{HOST_ADMINISTRATION_TOOLS, is_child_hard_denied};
use serde::Serialize;

/// 目录里的一行。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCatalogEntry {
    pub name: String,
    pub description: String,
    /// read | write | shell | orchestration | interaction | skill | browser | mcp | host
    pub category: &'static str,
    /// 子代理能不能拿到它。
    pub grantable: bool,
    /// 不可授予时的原因（展示给配置的人看）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn category_of(name: &str) -> &'static str {
    match name {
        "read_file" | "read_tool_output" | "ls" | "glob" | "grep" | "web_fetch" => "read",
        "write_file" | "edit" => "write",
        "bash" | "job_start" | "job_list" | "job_output" | "job_kill" => "shell",
        "spawn_agent" | "fork_agent" | "interrupt_agent" | "list_agents" | "wait_agent"
        | "send_message" => "orchestration",
        "ask" | "exit_plan" | "get_goal" | "update_goal" => "interaction",
        "skill" => "skill",
        "browser" => "browser",
        "create_preset" => "host",
        _ if name.starts_with("mcp__") => "mcp",
        "mcp_list" => "mcp",
        _ => "other",
    }
}

/// 子代理为什么拿不到这个工具。
fn denial_reason(name: &str) -> Option<String> {
    if HOST_ADMINISTRATION_TOOLS.contains(&name) {
        return Some("宿主管理面：定义与 preset 的写入只属于人类配置面".to_string());
    }
    if is_child_hard_denied(name) {
        return Some(match name {
            "spawn_agent" | "fork_agent" | "interrupt_agent" | "list_agents" | "wait_agent" => {
                "子代理不能派遣子代理".to_string()
            }
            "exit_plan" => "计划审批是会话主控模式的一部分，不下发给子代理".to_string(),
            _ => "会话目标由主代理管理".to_string(),
        });
    }
    None
}

/// 按注册表工具名（与描述）生成目录。
pub fn catalog(registered: &[(String, String)]) -> Vec<ToolCatalogEntry> {
    let mut entries: Vec<ToolCatalogEntry> = registered
        .iter()
        .map(|(name, description)| {
            let reason = denial_reason(name);
            ToolCatalogEntry {
                name: name.clone(),
                description: description.clone(),
                category: category_of(name),
                grantable: reason.is_none(),
                reason,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}
