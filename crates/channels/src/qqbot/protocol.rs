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
}
