//! 注入通道的驱动器侧辅助：通道识别（restore 反向扫描）、技能目录渲染、
//! 用户 `/技能名` 手势解析、工具触碰路径提取。对齐 dsh agent-instructions /
//! tool-skill 的交互语义，文案中文化。
use denia_core::session::{SessionEnvelope, SessionEvent};
use std::path::{Path, PathBuf};

/// 工作区指令注入文本的完整前缀（第一行标签 + 第二行通道名）。
pub const WORKSPACE_PREFIX: &str = "<system-reminder>\n工作区指令:";
/// 技能目录注入文本的完整前缀。
pub const SKILL_CATALOG_PREFIX: &str = "<system-reminder>\n技能目录:";

const REMINDER_CLOSE: &str = "</system-reminder>";

/// 从日志反向扫描最后一条匹配通道的注入消息全文；返回值就是幂等比较基准。
pub fn restore_injected_text(events: &[SessionEnvelope], prefix: &str) -> Option<String> {
    events.iter().rev().find_map(|envelope| match &envelope.event {
        SessionEvent::UserMessage {
            text,
            injected: true,
            ..
        } if text.starts_with(prefix) => Some(text.clone()),
        _ => None,
    })
}

/// 提取一条本通道注入文本的正文（去掉框架与引导语行）；供新旧内容比较。
fn extract_body(message: &str) -> &str {
    let inner = message
        .strip_prefix("<system-reminder>")
        .and_then(|s| s.strip_prefix('\n'))
        .and_then(|s| s.strip_suffix(REMINDER_CLOSE))
        .and_then(|s| s.strip_suffix('\n'))
        .unwrap_or("");
    match inner.split_once("\n\n") {
        Some((_, body)) => body,
        None => "",
    }
}

/// 计算本步应注入的技能目录文本；返回 None 表示无需注入。
///
/// 引导语由**目录内容是否变化**决定：首次用"本会话可用的技能如下"，
/// 旧目录存在但内容相同 → None（幂等），内容变化才切换"取代"文案。
/// 否则首次发布后 `has_visible` 翻转会同一段目录以"取代"版重发一次。
pub fn render_skill_catalog(entries: &[(String, String)], previous: Option<&str>) -> Option<String> {
    // 空目录正文为空串；非空目录正文 = <available_skills> 行列表 + 固定尾注。
    // 尾注计入正文:幂等比较的就是这段完整内容。
    let guidance = "技能是可复用的任务指令集。若用户点名某技能，或任务与某技能的描述明确匹配，先调用 skill 工具（action=load）加载其完整指令，再执行任务；可同时加载多个适用技能。目录只含摘要：未加载前不得凭摘要推断或执行技能内容。用户也可能以 /技能名 直接调用技能，其正文会以注入消息出现，此时无需再调 skill 工具。";
    let body = if entries.is_empty() {
        String::new()
    } else {
        let lines = entries
            .iter()
            .map(|(name, description)| format!("- `{name}`: {description}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("<available_skills>\n{lines}\n</available_skills>\n\n{guidance}")
    };
    let frame = |head: &str, body: &str| {
        if body.is_empty() {
            format!("<system-reminder>\n{head}\n{REMINDER_CLOSE}")
        } else {
            format!("<system-reminder>\n{head}\n\n{body}\n{REMINDER_CLOSE}")
        }
    };
    match previous {
        None => {
            if entries.is_empty() {
                None
            } else {
                Some(frame("技能目录:本会话可用的技能如下:", &body))
            }
        }
        Some(prev) => {
            if extract_body(prev) == body {
                return None;
            }
            let head = if entries.is_empty() {
                "技能目录:此技能目录取代之前发布的技能目录。当前没有可用的技能。"
            } else {
                "技能目录:此技能目录取代之前发布的技能目录。本会话可用的技能如下:"
            };
            Some(frame(head, &body))
        }
    }
}

/// 解析用户 `/技能名` 手势：只认真实用户消息（injected=false）的首行，
/// 外部/注入文本无法伪造这个手势；名字必须是合法 kebab-case。
pub fn skill_gesture(prompt: &str) -> Option<String> {
    let first = prompt.lines().next()?.trim();
    let name = first.strip_prefix('/')?;
    is_skill_name(name).then(|| name.to_string())
}

/// 与 skills.rs 一致的 kebab-case 技能名词法。
fn is_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .split('-')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()))
}

/// 从工具原始参数里提取触碰路径（read_file/write_file/edit 共用 `path` 字段）；
/// 相对路径按会话 cwd 落位，供嵌套目录发现用。
pub fn touched_path(cwd: &Path, raw_arguments: &str) -> Option<PathBuf> {
    let value: serde_json::Value = serde_json::from_str(raw_arguments.trim()).ok()?;
    let path = value.get("path")?.as_str()?.trim();
    if path.is_empty() {
        return None;
    }
    Some(if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        cwd.join(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(text: &str, injected: bool) -> SessionEnvelope {
        SessionEnvelope {
            seq: 1,
            time: 1,
            event: SessionEvent::UserMessage {
                text: text.into(),
                injected,
                images: Vec::new(),
            },
        }
    }

    #[test]
    fn restore_finds_last_channel_message_only() {
        let events = vec![
            envelope("<system-reminder>\n工作区指令:旧\nx", true),
            envelope("普通用户消息", false),
            envelope("<system-reminder>\n工作区指令:新\nx", true),
            envelope("<system-reminder>\n技能目录:目录\nx", true),
        ];
        let restored = restore_injected_text(&events, WORKSPACE_PREFIX).unwrap();
        assert!(restored.contains("新"));
        assert!(restore_injected_text(&events, SKILL_CATALOG_PREFIX).unwrap().contains("目录"));
        assert_eq!(restore_injected_text(&events, "<system-reminder>\n无此通道:"), None);
    }

    #[test]
    fn catalog_rendering_switches_on_content_change_not_visibility() {
        let entries = vec![("review".to_string(), "审查代码".to_string())];
        // 从未发布且目录为空 → 不注入。
        assert_eq!(render_skill_catalog(&[], None), None);
        let first = render_skill_catalog(&entries, None).unwrap();
        assert!(first.contains("本会话可用的技能如下"));
        assert!(first.contains("- `review`: 审查代码"));
        assert!(!first.contains("取代"));
        // 回归:发布后内容未变 → 不得再注入(旧实现会以"取代"文案重发一次)。
        assert_eq!(render_skill_catalog(&entries, Some(&first)), None);
        // 内容变化 → "取代"文案。
        let changed = vec![("review".to_string(), "新描述".to_string())];
        let replaced = render_skill_catalog(&changed, Some(&first)).unwrap();
        assert!(replaced.contains("取代之前发布的技能目录"));
        // 目录清空 → 空替换声明,且再算一步幂等。
        let emptied = render_skill_catalog(&[], Some(&first)).unwrap();
        assert!(emptied.contains("当前没有可用的技能"));
        assert_eq!(render_skill_catalog(&[], Some(&emptied)), None);
    }

    #[test]
    fn gesture_only_accepts_kebab_case_first_line() {
        assert_eq!(skill_gesture("/review\n帮我审查"), Some("review".into()));
        assert_eq!(skill_gesture("/review"), Some("review".into()));
        assert_eq!(skill_gesture(" /review "), Some("review".into()));
        assert_eq!(skill_gesture("/Review"), None);
        assert_eq!(skill_gesture("/a b"), None);
        assert_eq!(skill_gesture("/"), None);
        assert_eq!(skill_gesture("普通消息"), None);
        assert_eq!(skill_gesture(""), None);
    }

    #[test]
    fn touched_path_resolves_relative_and_rejects_empty() {
        let cwd = Path::new("/work");
        assert_eq!(
            touched_path(cwd, r#"{"path":"src/main.rs"}"#),
            Some(PathBuf::from("/work/src/main.rs"))
        );
        assert_eq!(
            touched_path(cwd, r#"{"path":"/abs/x.txt"}"#),
            Some(PathBuf::from("/abs/x.txt"))
        );
        assert_eq!(touched_path(cwd, r#"{"path":"  "}"#), None);
        assert_eq!(touched_path(cwd, "不是 JSON"), None);
    }
}
