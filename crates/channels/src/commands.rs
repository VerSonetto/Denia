//! `/workspace`、`/switch`、`/permission` 等指令的解析与回复文本。
//!
//! 纯函数:不碰网络、不读宿主,便于单测覆盖文案与边界。

use denia_core::config::ModelSelection;
use denia_core::session::PermissionMode;

use crate::bridge::{ModelOption, SessionInfo, WorkspaceInfo};

/// 一条已解析的指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 裸输入列出工作区;带参选择。
    Workspace(Option<String>),
    /// 裸输入列出会话;带参切换(`new` = 新会话)。
    Switch(Option<String>),
    /// 裸输入列出当前模型;带参切换(`供应商id 模型名称`)。
    Model(Option<String>),
    /// 裸输入列出档位;带参切换。
    Permission(Option<String>),
    Cancel,
    Approve,
    Reject,
    Help,
    Unknown(String),
}

impl Command {
    /// 命令名(诊断与日志用)。
    pub fn name(&self) -> &'static str {
        match self {
            Self::Workspace(_) => "/workspace",
            Self::Switch(_) => "/switch",
            Self::Model(_) => "/model",
            Self::Permission(_) => "/permission",
            Self::Cancel => "/cancel",
            Self::Approve => "/approve",
            Self::Reject => "/reject",
            Self::Help => "/help",
            Self::Unknown(_) => "/unknown",
        }
    }
}

/// 解析文本指令;普通消息返回 `None`。
pub fn parse(text: &str) -> Option<Command> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut pieces = rest.split_whitespace();
    let name = pieces.next().unwrap_or_default().to_ascii_lowercase();
    let argument = pieces.collect::<Vec<_>>().join(" ");
    let argument = if argument.is_empty() {
        None
    } else {
        Some(argument)
    };
    Some(match name.as_str() {
        "workspace" | "ws" => Command::Workspace(argument),
        "switch" | "session" => Command::Switch(argument),
        "model" => Command::Model(argument),
        "permission" | "perm" => Command::Permission(argument),
        "cancel" | "stop" => Command::Cancel,
        "approve" | "allow" => Command::Approve,
        "reject" | "deny" => Command::Reject,
        "help" | "start" => Command::Help,
        other => Command::Unknown(other.to_string()),
    })
}

/// 权限档位的中文名。
pub fn permission_label(mode: PermissionMode) -> &'static str {
    match mode {
        PermissionMode::ReadOnly => "只读",
        PermissionMode::AutoEdit => "自动编辑",
        PermissionMode::Plan => "计划",
        PermissionMode::Full => "完全访问",
    }
}

fn permission_line(mode: PermissionMode) -> String {
    let note = match mode {
        PermissionMode::ReadOnly => "工具面只留只读类，写操作被拒绝",
        PermissionMode::AutoEdit => "工作区内编辑自动放行，区外写/删除/bash 改动走审批",
        PermissionMode::Plan => "只读执行面，用 exit_plan 提交计划待审批",
        PermissionMode::Full => "全部自动放行，路径可出工作区，不弹审批",
    };
    format!(
        "{}（{}）— {}",
        permission_label(mode),
        mode.as_str(),
        note
    )
}

/// `/workspace` 裸输入:列出全部工作区。
pub fn workspace_list(workspaces: &[WorkspaceInfo], current: Option<&str>) -> String {
    if workspaces.is_empty() {
        return "还没有工作区。请先在 denia 控制台里添加一个工作区，然后再回来选择。".to_string();
    }
    let mut out = String::from("可用工作区：\n");
    for (index, workspace) in workspaces.iter().enumerate() {
        let marker = if Some(workspace.id.as_str()) == current {
            " [当前]"
        } else {
            ""
        };
        out.push_str(&format!(
            "{}. {}{} — {}\n",
            index + 1,
            workspace.title,
            marker,
            workspace.path
        ));
    }
    out.push_str("\n发送 /workspace <编号> 选择工作区；选中后未切换会话时默认使用新会话。");
    out
}

/// `/switch` 裸输入:列出当前工作区的会话。
pub fn session_list(
    workspace: &WorkspaceInfo,
    sessions: &[SessionInfo],
    current: Option<&str>,
) -> String {
    if sessions.is_empty() {
        return format!(
            "工作区「{}」还没有会话。直接发送消息会新建一个会话。",
            workspace.title
        );
    }
    let mut out = format!("工作区「{}」的会话：\n", workspace.title);
    for (index, session) in sessions.iter().enumerate() {
        let marker = if Some(session.id.as_str()) == current {
            " [当前]"
        } else if session.running {
            " [运行中]"
        } else {
            ""
        };
        out.push_str(&format!(
            "{}. {}{} — {}\n",
            index + 1,
            session.display_name(),
            marker,
            format_age(session.created_at)
        ));
    }
    out.push_str("\n发送 /switch <编号> 切换会话；/switch new 新建会话。");
    out
}

/// 模型查找结果。
pub enum ModelLookup<'a> {
    Found(&'a ModelOption),
    NotFound,
    /// 宽容匹配命中了多个(例如同名模型有多种前缀),让用户用完整 id。
    Ambiguous(Vec<&'a ModelOption>),
}

/// 查找模型:先精确匹配 id,再容错匹配"去掉 `cn:` 这类前缀的 id"与显示名。
///
/// 目录里的 id 常带路由前缀(如 `cn:deepseek-v4.1-flash`),要求用户记住前缀
/// 不现实;只要同一提供商下唯一命中,就用目录里的完整 id。
pub fn find_model<'a>(
    models: &'a [ModelOption],
    provider: &str,
    query: &str,
) -> ModelLookup<'a> {
    let same_provider = |option: &&ModelOption| option.provider.eq_ignore_ascii_case(provider);
    if let Some(option) = models
        .iter()
        .filter(same_provider)
        .find(|option| option.model == query)
    {
        return ModelLookup::Found(option);
    }
    let candidates: Vec<&ModelOption> = models
        .iter()
        .filter(same_provider)
        .filter(|option| {
            option.model.rsplit(':').next() == Some(query)
                || option.model_name.split(" (").next() == Some(query)
        })
        .collect();
    match candidates.len() {
        1 => ModelLookup::Found(candidates[0]),
        0 => ModelLookup::NotFound,
        _ => ModelLookup::Ambiguous(candidates),
    }
}

/// `/model` 裸输入:当前模型 + 可用目录(只给 id,方便直接复制命令)。
pub fn model_list(current: Option<&ModelSelection>, models: &[ModelOption]) -> String {
    let mut out = String::from("当前模型：");
    match current {
        Some(selection) => out.push_str(&format!("{}/{}", selection.provider, selection.model)),
        None => out.push_str("未设置（首次发消息时自动选择目录里的第一个可用模型）"),
    }
    out.push_str("\n\n可用模型：\n");
    if models.is_empty() {
        out.push_str("（模型目录为空，请先在 denia 控制台配置模型提供商）\n");
    }
    const MAX_SHOWN: usize = 60;
    for option in models.iter().take(MAX_SHOWN) {
        out.push_str(&format!("- {} {}\n", option.provider, option.model));
    }
    if models.len() > MAX_SHOWN {
        out.push_str(&format!(
            "…还有 {} 个未列出，可直接用完整 id 设置。\n",
            models.len() - MAX_SHOWN
        ));
    }
    out.push_str("\n发送 /model <供应商id> <模型id> 切换，例如 /model Astra deepseek_v4_flash。");
    out
}

/// `/permission` 裸输入:列出四档与当前值。
pub fn permission_list(current: PermissionMode) -> String {
    let mut out = String::from("权限模式（当前会话）：\n");
    for mode in [
        PermissionMode::ReadOnly,
        PermissionMode::AutoEdit,
        PermissionMode::Plan,
        PermissionMode::Full,
    ] {
        let marker = if mode == current { " [当前]" } else { "" };
        out.push_str(&format!("- {}{}\n", permission_line(mode), marker));
    }
    out.push_str("\n发送 /permission <档位> 切换，例如 /permission auto-edit。");
    out
}

pub fn help_text() -> String {
    "denia 微信助手\n\n\
     /workspace — 列出工作区；/workspace <编号> 选择工作区\n\
     /switch — 列出当前工作区的会话；/switch <编号> 切换；/switch new 新建会话\n\
     /model — 查看当前模型；/model <供应商id> <模型id> 切换\n\
     /permission — 列出权限档位；/permission <档位> 切换\n\
     /cancel — 中断当前正在执行的轮次\n\
     /approve、/reject — 结算挂起的审批请求\n\n\
     选中工作区后直接发送消息即可开始（未切换会话时默认新建会话）。"
        .to_string()
}

/// 毫秒时间戳 -> 相对时间。
pub fn format_age(created_at_ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let age = now.saturating_sub(created_at_ms);
    let minutes = age / 60_000;
    if minutes < 1 {
        "刚刚".to_string()
    } else if minutes < 60 {
        format!("{minutes} 分钟前")
    } else if minutes < 60 * 24 {
        format!("{} 小时前", minutes / 60)
    } else {
        format!("{} 天前", minutes / (60 * 24))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_and_arguments() {
        assert_eq!(parse("/workspace"), Some(Command::Workspace(None)));
        assert_eq!(
            parse("/workspace 2"),
            Some(Command::Workspace(Some("2".into())))
        );
        assert_eq!(
            parse("/switch My Project"),
            Some(Command::Switch(Some("My Project".into())))
        );
        assert_eq!(parse("/permission"), Some(Command::Permission(None)));
        assert_eq!(
            parse("/permission full"),
            Some(Command::Permission(Some("full".into())))
        );
        assert_eq!(parse("/model"), Some(Command::Model(None)));
        assert_eq!(
            parse("/model openai gpt-4o"),
            Some(Command::Model(Some("openai gpt-4o".into())))
        );
        assert_eq!(parse("/cancel"), Some(Command::Cancel));
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("/nope"), Some(Command::Unknown("nope".into())));
    }

    #[test]
    fn workspace_list_marks_current() {
        let spaces = vec![WorkspaceInfo {
            id: "a".into(),
            title: "项目".into(),
            path: "D:/p".into(),
            session_count: 0,
        }];
        let text = workspace_list(&spaces, Some("a"));
        assert!(text.contains("1. 项目 [当前] — D:/p"));
    }

    #[test]
    fn permission_list_marks_every_mode() {
        let text = permission_list(PermissionMode::AutoEdit);
        assert!(text.contains("自动编辑（auto-edit）"));
        assert!(text.contains("[当前]"));
        assert!(text.contains("完全访问（full）"));
    }

    fn model_option(provider: &str, model: &str, name: &str) -> ModelOption {
        ModelOption {
            provider: provider.into(),
            provider_name: provider.into(),
            model: model.into(),
            model_name: name.into(),
        }
    }

    #[test]
    fn model_lookup_accepts_full_id_prefixless_id_and_display_name() {
        let models = vec![
            model_option("workbuddy", "cn:deepseek-v4.1-flash", "deepseek-v4.1-flash (workbuddy)"),
            model_option("workbuddy", "cn:hy4-preview-f", "hy4-preview-free (workbuddy)"),
            model_option("Astra", "bedrock_gpt_6_astra", "gpt_6_astra (bedrock_openai)"),
        ];
        let found = |query: &str| match find_model(&models, "workbuddy", query) {
            ModelLookup::Found(option) => option.model.clone(),
            ModelLookup::NotFound => "missing".to_string(),
            ModelLookup::Ambiguous(_) => "ambiguous".to_string(),
        };
        assert_eq!(found("cn:deepseek-v4.1-flash"), "cn:deepseek-v4.1-flash");
        assert_eq!(found("deepseek-v4.1-flash"), "cn:deepseek-v4.1-flash");
        assert_eq!(found("hy4-preview-free"), "cn:hy4-preview-f");
        assert_eq!(found("nope"), "missing");
        // 提供商大小写不敏感。
        assert!(matches!(
            find_model(&models, "astra", "bedrock_gpt_6_astra"),
            ModelLookup::Found(_)
        ));
    }
}
