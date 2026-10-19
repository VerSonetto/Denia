//! QQ Bot API v2 协议:常量、事件解析与请求体构造。
//!
//! 与 hermes 的 qqbot 适配器对齐:
//! - 网关事件走 `C2C_MESSAGE_CREATE`(私聊)与 `GROUP_AT_MESSAGE_CREATE`(群 @);
//! - 被动回复必须带用户消息的 `msg_id`,否则会触发主动消息配额;
//! - 群消息正文开头带 `@机器人`,入站时剥掉。

use serde::Deserialize;
use serde_json::{Value, json};

pub const API_BASE: &str = "https://api.sgroup.qq.com";
pub const TOKEN_URL: &str = "https://bots.qq.com/app/getAppAccessToken";
pub const GATEWAY_PATH: &str = "/gateway";
/// 与 hermes 一致:群 @、私聊、频道私信与互动的 intent 组合。
pub const INTENTS: u64 = (1 << 25) | (1 << 30) | (1 << 12) | (1 << 26);
pub const MAX_MESSAGE_LENGTH: usize = 4000;
pub const MSG_TYPE_TEXT: i64 = 0;

/// 会话类型:私聊按用户 openid,群聊按 group_openid。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatType {
    C2c,
    Group,
}

impl ChatType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::C2c => "c2c",
            Self::Group => "group",
        }
    }
}

/// 一条入站消息(已从网关事件里提取)。
#[derive(Debug, Clone)]
pub struct InboundMessage {
    /// 平台消息 id:被动回复要带它。
    pub id: String,
    /// 回复目标:私聊是用户 openid,群聊是 group_openid。
    pub chat_id: String,
    pub chat_type: ChatType,
    /// 发送者 openid(群聊是 member_openid),用作绑定 key。
    pub sender: String,
    pub content: String,
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attachment {
    #[serde(default)]
    pub content_type: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub filename: String,
}

impl Attachment {
    pub fn is_image(&self) -> bool {
        self.content_type
            .to_ascii_lowercase()
            .starts_with("image/")
    }
}

/// 从网关 dispatch 事件提取入站消息;非消息事件返回 `None`。
pub fn parse_inbound(event_type: &str, d: &Value) -> Option<InboundMessage> {
    match event_type {
        "C2C_MESSAGE_CREATE" => {
            let id = d.get("id")?.as_str()?.to_string();
            let sender = d.pointer("/author/user_openid")?.as_str()?.to_string();
            if sender.is_empty() {
                return None;
            }
            Some(InboundMessage {
                id,
                chat_id: sender.clone(),
                chat_type: ChatType::C2c,
                sender,
                content: d
                    .get("content")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                attachments: attachments(d),
            })
        }
        "GROUP_AT_MESSAGE_CREATE" => {
            let id = d.get("id")?.as_str()?.to_string();
            let group = d.get("group_openid")?.as_str()?.to_string();
            if group.is_empty() {
                return None;
            }
            let sender = d
                .pointer("/author/member_openid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            Some(InboundMessage {
                id,
                chat_id: group,
                chat_type: ChatType::Group,
                sender,
                content: strip_at_mention(
                    d.get("content").and_then(Value::as_str).unwrap_or_default(),
                ),
                attachments: attachments(d),
            })
        }
        _ => None,
    }
}

fn attachments(d: &Value) -> Vec<Attachment> {
    d.get("attachments")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// 群消息正文以 `@机器人 ` 开头;去掉第一个空白前的 @ 段。
pub fn strip_at_mention(content: &str) -> String {
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix('@') {
        return rest
            .split_once(char::is_whitespace)
            .map(|(_, body)| body.trim_start().to_string())
            .unwrap_or_default();
    }
    trimmed.to_string()
}

/// 网关载荷(op/t/s/d)。
#[derive(Debug, Clone, Deserialize)]
pub struct GatewayPayload {
    #[serde(default)]
    pub op: i64,
    #[serde(default, rename = "t")]
    pub event_type: Option<String>,
    #[serde(default)]
    pub s: Option<i64>,
    #[serde(default)]
    pub d: Value,
}

pub fn identify_body(token: &str) -> Value {
    json!({
        "op": 2,
        "d": {
            "token": format!("QQBot {token}"),
            "intents": INTENTS,
            "shard": [0, 1]
        }
    })
}

pub fn resume_body(token: &str, session_id: &str, seq: i64) -> Value {
    json!({
        "op": 6,
        "d": {
            "token": format!("QQBot {token}"),
            "session_id": session_id,
            "seq": seq
        }
    })
}

pub fn heartbeat_body(seq: Option<i64>) -> Value {
    json!({ "op": 1, "d": seq })
}

/// 文本消息体。第一片带 `msg_id` 作为被动回复,后续片不带。
pub fn text_body(content: &str, msg_id: Option<&str>, msg_seq: u32) -> Value {
    let mut body = json!({
        "content": content,
        "msg_type": MSG_TYPE_TEXT,
        "msg_seq": msg_seq,
    });
    if let Some(id) = msg_id {
        body["msg_id"] = json!(id);
    }
    body
}

/// 审批按钮键盘:允许一次 / 拒绝(同一 group,点击后其余置灰)。
pub fn approval_keyboard(session_id: &str, request_id: &str) -> Value {
    json!({
        "content": {
            "rows": [{
                "buttons": [
                    callback_button(
                        "allow",
                        "✅ 允许一次",
                        "已允许",
                        &format!("approve:{session_id}:{request_id}:allow-once"),
                        1,
                    ),
                    callback_button(
                        "deny",
                        "❌ 拒绝",
                        "已拒绝",
                        &format!("approve:{session_id}:{request_id}:deny"),
                        0,
                    ),
                ]
            }]
        }
    })
}

/// ask 单选题的选项键盘;多选、无选项或超过 5 项时返回 `None`(改发文本)。
pub fn ask_keyboard(
    session_id: &str,
    request_id: &str,
    question: &denia_core::session::AskQuestion,
) -> Option<Value> {
    if question.multi_select || question.options.is_empty() || question.options.len() > 5 {
        return None;
    }
    let buttons: Vec<Value> = question
        .options
        .iter()
        .enumerate()
        .map(|(index, option)| {
            callback_button(
                &format!("opt{index}"),
                &option.label,
                &option.label,
                &format!(
                    "ask:{session_id}:{request_id}:{}:{index}",
                    question.id
                ),
                if option.recommended { 1 } else { 0 },
            )
        })
        .collect();
    Some(json!({ "content": { "rows": [{ "buttons": buttons }] } }))
}

fn callback_button(id: &str, label: &str, visited: &str, data: &str, style: i64) -> Value {
    json!({
        "id": id,
        "render_data": { "label": label, "visited_label": visited, "style": style },
        "action": {
            "type": 1,
            "permission": { "type": 2 },
            "data": data,
            "click_limit": 1,
        },
        "group_id": "action",
    })
}

/// 按钮点击携带的决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ButtonAction {
    Approve {
        session_id: String,
        request_id: String,
        allow: bool,
    },
    Ask {
        session_id: String,
        request_id: String,
        question_id: String,
        option_index: usize,
    },
}

pub fn parse_button_data(data: &str) -> Option<ButtonAction> {
    if let Some(rest) = data.strip_prefix("approve:") {
        let mut parts = rest.rsplit(':');
        let decision = parts.next()?;
        let request_id = parts.next()?;
        let session_id = parts.next()?;
        if session_id.is_empty() || request_id.is_empty() {
            return None;
        }
        let allow = match decision {
            "allow-once" => true,
            "deny" => false,
            _ => return None,
        };
        return Some(ButtonAction::Approve {
            session_id: session_id.to_string(),
            request_id: request_id.to_string(),
            allow,
        });
    }
    let rest = data.strip_prefix("ask:")?;
    let mut tail = rest.rsplitn(3, ':');
    let index = tail.next()?.parse::<usize>().ok()?;
    let question_id = tail.next()?.to_string();
    let head = tail.next()?;
    let (session_id, request_id) = head.split_once(':')?;
    if session_id.is_empty() || request_id.is_empty() || question_id.is_empty() {
        return None;
    }
    Some(ButtonAction::Ask {
        session_id: session_id.to_string(),
        request_id: request_id.to_string(),
        question_id,
        option_index: index,
    })
}

/// `INTERACTION_CREATE` 事件里与按钮决策相关的字段。
#[derive(Debug, Clone)]
pub struct Interaction {
    pub id: String,
    pub chat_type: ChatType,
    pub chat_id: String,
    pub sender: String,
    pub button_data: String,
}

pub fn parse_interaction(d: &Value) -> Option<Interaction> {
    let id = d.get("id")?.as_str()?.to_string();
    let chat_type = match d.get("chat_type").and_then(Value::as_i64) {
        Some(1) => ChatType::Group,
        Some(2) => ChatType::C2c,
        _ => return None,
    };
    let (chat_id, sender) = match chat_type {
        ChatType::Group => (
            d.get("group_openid")?.as_str()?.to_string(),
            d.get("group_member_openid")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        ChatType::C2c => {
            let user = d.get("user_openid")?.as_str()?.to_string();
            (user.clone(), user)
        }
    };
    let button_data = d
        .pointer("/data/resolved/button_data")
        .and_then(Value::as_str)?
        .to_string();
    if button_data.is_empty() {
        return None;
    }
    Some(Interaction {
        id,
        chat_type,
        chat_id,
        sender,
        button_data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_c2c_and_group_messages() {
        let c2c: Value = serde_json::json!({
            "id": "m1",
            "content": "/workspace",
            "author": { "user_openid": "u1" }
        });
        let message = parse_inbound("C2C_MESSAGE_CREATE", &c2c).unwrap();
        assert_eq!(message.chat_type, ChatType::C2c);
        assert_eq!(message.sender, "u1");
        assert_eq!(message.content, "/workspace");

        let group: Value = serde_json::json!({
            "id": "m2",
            "content": "@机器人 你好",
            "group_openid": "g1",
            "author": { "member_openid": "member1" }
        });
        let message = parse_inbound("GROUP_AT_MESSAGE_CREATE", &group).unwrap();
        assert_eq!(message.chat_type, ChatType::Group);
        assert_eq!(message.chat_id, "g1");
        assert_eq!(message.sender, "member1");
        assert_eq!(message.content, "你好");
    }

    #[test]
    fn strips_at_mention_variants() {
        assert_eq!(strip_at_mention("@bot  hello"), "hello");
        assert_eq!(strip_at_mention("  @bot   hello world"), "hello world");
        assert_eq!(strip_at_mention("plain"), "plain");
        assert_eq!(strip_at_mention("@bot"), "");
    }

    #[test]
    fn builds_text_body_with_reply_id_only_when_given() {
        let reply = text_body("hi", Some("m1"), 7);
        assert_eq!(reply["msg_id"], "m1");
        assert_eq!(reply["msg_type"], 0);
        assert_eq!(reply["msg_seq"], 7);
        let active = text_body("hi", None, 1);
        assert!(active.get("msg_id").is_none());
    }

    #[test]
    fn builds_approval_keyboard_and_parses_decision() {
        let keyboard = approval_keyboard("s1", "r1");
        let buttons = &keyboard["content"]["rows"][0]["buttons"];
        assert_eq!(buttons[0]["action"]["data"], "approve:s1:r1:allow-once");
        assert_eq!(buttons[1]["action"]["data"], "approve:s1:r1:deny");
        assert_eq!(
            parse_button_data("approve:s1:r1:allow-once"),
            Some(ButtonAction::Approve {
                session_id: "s1".into(),
                request_id: "r1".into(),
                allow: true,
            })
        );
        assert_eq!(
            parse_button_data("approve:s1:r1:deny"),
            Some(ButtonAction::Approve {
                session_id: "s1".into(),
                request_id: "r1".into(),
                allow: false,
            })
        );
        assert_eq!(parse_button_data("approve:s1:r1:bogus"), None);
        assert_eq!(parse_button_data("approve:s1:r1"), None);
    }

    #[test]
    fn builds_ask_keyboard_and_parses_option() {
        use denia_core::session::{AskOption, AskQuestion};
        let question = AskQuestion {
            id: "q1".into(),
            question: "选哪个".into(),
            header: None,
            detail: None,
            options: vec![
                AskOption {
                    label: "A".into(),
                    description: None,
                    recommended: true,
                },
                AskOption {
                    label: "B".into(),
                    description: None,
                    recommended: false,
                },
            ],
            multi_select: false,
            allow_custom: true,
        };
        let keyboard = ask_keyboard("s1", "r1", &question).unwrap();
        assert_eq!(
            keyboard["content"]["rows"][0]["buttons"][0]["action"]["data"],
            "ask:s1:r1:q1:0"
        );
        assert_eq!(
            parse_button_data("ask:s1:r1:q1:1"),
            Some(ButtonAction::Ask {
                session_id: "s1".into(),
                request_id: "r1".into(),
                question_id: "q1".into(),
                option_index: 1,
            })
        );
        let multi = AskQuestion {
            multi_select: true,
            ..question
        };
        assert!(ask_keyboard("s1", "r1", &multi).is_none());
    }

    #[test]
    fn parses_interaction_payloads() {
        let c2c = serde_json::json!({
            "id": "i1",
            "chat_type": 2,
            "user_openid": "u1",
            "data": { "resolved": { "button_data": "approve:s1:r1:allow-once", "button_id": "allow" } }
        });
        let interaction = parse_interaction(&c2c).unwrap();
        assert_eq!(interaction.id, "i1");
        assert_eq!(interaction.chat_type, ChatType::C2c);
        assert_eq!(interaction.chat_id, "u1");
        assert_eq!(interaction.sender, "u1");

        let group = serde_json::json!({
            "id": "i2",
            "chat_type": 1,
            "group_openid": "g1",
            "group_member_openid": "m1",
            "data": { "resolved": { "button_data": "ask:s1:r1:q1:0" } }
        });
        let interaction = parse_interaction(&group).unwrap();
        assert_eq!(interaction.chat_type, ChatType::Group);
        assert_eq!(interaction.chat_id, "g1");
        assert_eq!(interaction.sender, "m1");
    }
}
