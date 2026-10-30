//! 微信 iLink Bot API 的线上协议(常量、请求头、消息结构)。
//!
//! - 长轮询 `ilink/bot/getupdates` 驱动入站;
//! - 发送 `ilink/bot/sendmessage`，每条回复必须回显该用户最新的 `context_token`;
//! - 扫码登录 `ilink/bot/get_bot_qrcode` + `ilink/bot/get_qrcode_status`。

use base64::Engine;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// 微信下发的 id 字段可能是数字(如雪花 message_id)也可能是字符串。
/// 整批消息因为一个类型不符被丢弃的代价太大,这里统一宽容处理。
fn flexible_id<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Option::<Value>::deserialize(deserializer)?;
    Ok(raw.map(|value| match value {
        Value::String(text) => text,
        Value::Number(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        _ => String::new(),
    }))
}

/// iLink API 默认入口(登录返回 `baseurl` 时以返回值为准)。
pub const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
/// 渠道版本,随每个 POST 的 `base_info` 上报。
pub const CHANNEL_VERSION: &str = "2.2.0";
/// 客户端版本:高 16 位主版本、中 8 位次版本(2.2.0)。
pub const APP_CLIENT_VERSION: u64 = (2 << 16) | (2 << 8);

pub const APP_ID: &str = "bot";

pub const EP_GET_UPDATES: &str = "ilink/bot/getupdates";
pub const EP_SEND_MESSAGE: &str = "ilink/bot/sendmessage";
pub const EP_SEND_TYPING: &str = "ilink/bot/sendtyping";
pub const EP_GET_CONFIG: &str = "ilink/bot/getconfig";
pub const EP_GET_BOT_QR: &str = "ilink/bot/get_bot_qrcode";
pub const EP_GET_QR_STATUS: &str = "ilink/bot/get_qrcode_status";

/// 长轮询在服务端挂起的时长;实际值可由响应里的 `longpolling_timeout_ms` 调整。
pub const LONG_POLL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);
pub const API_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 会话过期错误码:需要停止拉取并等待重新配对。
pub const SESSION_EXPIRED_ERRCODE: i64 = -14;
/// 频率限制 / 会话未就绪错误码。
pub const RATE_LIMIT_ERRCODE: i64 = -2;

pub const ITEM_TEXT: i64 = 1;
pub const ITEM_IMAGE: i64 = 2;
pub const ITEM_VOICE: i64 = 3;
pub const ITEM_FILE: i64 = 4;
pub const ITEM_VIDEO: i64 = 5;

pub const MSG_TYPE_BOT: i64 = 2;
pub const MSG_STATE_FINISH: i64 = 2;

/// 单条文本的分片上限(iLink 在约 2048 字符处截断,留余量给 Markdown)。
pub const MAX_TEXT_CHARS: usize = 1800;

/// 构造 iLink 请求头。
///
/// `X-WECHAT-UIN` 是每次请求随机生成的 base64(内容是十进制随机数),不是身份,
/// 只用于让服务端区分客户端实例。
pub fn headers(token: Option<&str>, body: Option<&str>) -> Vec<(String, String)> {
    let uin = base64::engine::general_purpose::STANDARD
        .encode(format!("{}", rand::random::<u32>()).as_bytes());
    let mut headers = vec![
        ("iLink-App-Id".to_string(), APP_ID.to_string()),
        (
            "iLink-App-ClientVersion".to_string(),
            APP_CLIENT_VERSION.to_string(),
        ),
        ("X-WECHAT-UIN".to_string(), uin),
    ];
    if let Some(body) = body {
        headers.push(("Content-Type".to_string(), "application/json".to_string()));
        headers.push(("Content-Length".to_string(), body.len().to_string()));
        headers.push((
            "AuthorizationType".to_string(),
            "ilink_bot_token".to_string(),
        ));
    }
    if let Some(token) = token {
        headers.push(("Authorization".to_string(), format!("Bearer {token}")));
    }
    headers
}

/// `getupdates` 的响应。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdatesResponse {
    #[serde(default)]
    pub ret: Option<i64>,
    #[serde(default)]
    pub errcode: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
    #[serde(default)]
    pub msgs: Vec<InboundMessage>,
    #[serde(default, deserialize_with = "flexible_id")]
    pub get_updates_buf: Option<String>,
    #[serde(default)]
    pub longpolling_timeout_ms: Option<u64>,
}

impl UpdatesResponse {
    pub fn is_error(&self) -> bool {
        matches!(self.ret, Some(ret) if ret != 0) || matches!(self.errcode, Some(code) if code != 0)
    }

    pub fn is_session_expired(&self) -> bool {
        self.ret == Some(SESSION_EXPIRED_ERRCODE) || self.errcode == Some(SESSION_EXPIRED_ERRCODE)
    }

    pub fn error_text(&self) -> String {
        format!(
            "ret={} errcode={} errmsg={}",
            self.ret.unwrap_or(0),
            self.errcode.unwrap_or(0),
            self.errmsg.as_deref().unwrap_or("")
        )
    }
}

/// 入站消息。
#[derive(Debug, Clone, Deserialize)]
pub struct InboundMessage {
    #[serde(default, deserialize_with = "flexible_id")]
    pub from_user_id: Option<String>,
    #[serde(default, deserialize_with = "flexible_id")]
    pub to_user_id: Option<String>,
    #[serde(default, deserialize_with = "flexible_id")]
    pub message_id: Option<String>,
    #[serde(default, deserialize_with = "flexible_id")]
    pub context_token: Option<String>,
    #[serde(default, deserialize_with = "flexible_id")]
    pub room_id: Option<String>,
    #[serde(default)]
    pub item_list: Vec<Item>,
}

impl InboundMessage {
    /// 提取文本:优先纯文本项;引用消息把引用标题拼在正文前。
    pub fn text(&self) -> String {
        for item in &self.item_list {
            if item.kind != Some(ITEM_TEXT) {
                continue;
            }
            let text = item
                .text_item
                .as_ref()
                .map(|entry| entry.text.clone())
                .unwrap_or_default();
            let Some(reference) = &item.ref_msg else {
                return text;
            };
            let title = reference.title.clone().unwrap_or_default();
            let quoted = reference
                .message_item
                .as_ref()
                .map(|nested| Item::plain_text(nested))
                .unwrap_or_default();
            let prefix = [title, quoted]
                .into_iter()
                .filter(|part| !part.trim().is_empty())
                .collect::<Vec<_>>()
                .join(" | ");
            return if prefix.is_empty() {
                text
            } else {
                format!("[引用: {prefix}]\n{text}")
            };
        }
        // 没有文本项:语音里可能有腾讯的转写文本(中文可用)。
        for item in &self.item_list {
            if item.kind == Some(ITEM_VOICE)
                && let Some(voice) = &item.voice_item
                && !voice.text.trim().is_empty()
            {
                return format!("[语音转写]\n{}", voice.text);
            }
        }
        String::new()
    }

    /// 是否只包含非文本消息(图片/文件等)。
    pub fn has_media(&self) -> bool {
        self.item_list.iter().any(|item| {
            matches!(
                item.kind,
                Some(ITEM_IMAGE | ITEM_VOICE | ITEM_FILE | ITEM_VIDEO)
            )
        })
    }

    /// 收集图片项(含引用消息里的图片);仅返回 image_item 完整的项。
    pub fn images(&self) -> Vec<&ImageItem> {
        let mut images = Vec::new();
        for item in &self.item_list {
            if item.kind == Some(ITEM_IMAGE)
                && let Some(image) = &item.image_item
            {
                images.push(image);
            }
            if let Some(reference) = &item.ref_msg
                && let Some(nested) = reference.message_item.as_deref()
                && nested.kind == Some(ITEM_IMAGE)
                && let Some(image) = &nested.image_item
            {
                images.push(image);
            }
        }
        images
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Item {
    #[serde(rename = "type", default)]
    pub kind: Option<i64>,
    #[serde(default)]
    pub text_item: Option<TextItem>,
    #[serde(default)]
    pub voice_item: Option<VoiceItem>,
    #[serde(default)]
    pub image_item: Option<ImageItem>,
    #[serde(default)]
    pub ref_msg: Option<RefMessage>,
}

impl Item {
    fn plain_text(item: &Item) -> String {
        item.text_item
            .as_ref()
            .map(|entry| entry.text.clone())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextItem {
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct VoiceItem {
    #[serde(default)]
    pub text: String,
}

/// 图片项。媒体本体在 `media` 块里;部分消息直接在 `image_item` 上带
/// 十六进制 `aeskey` 与直链。
#[derive(Debug, Clone, Deserialize)]
pub struct ImageItem {
    #[serde(default)]
    pub media: Option<MediaRef>,
    #[serde(default)]
    pub mid_size: Option<u64>,
    /// 部分消息在这里直接给 hex 形式的 AES key。
    #[serde(default)]
    pub aeskey: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub thumb_url: Option<String>,
}

/// 媒体引用:加密 query 参数 / base64 AES key / 完整直链。
#[derive(Debug, Clone, Deserialize)]
pub struct MediaRef {
    #[serde(default)]
    pub encrypt_query_param: Option<String>,
    #[serde(default)]
    pub aes_key: Option<String>,
    #[serde(default)]
    pub full_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RefMessage {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub message_item: Option<Box<Item>>,
}

/// 发送请求的 `msg` 体。
pub fn text_message(
    to: &str,
    text: &str,
    context_token: Option<&str>,
    client_id: &str,
) -> serde_json::Value {
    let mut message = serde_json::json!({
        "from_user_id": "",
        "to_user_id": to,
        "client_id": client_id,
        "message_type": MSG_TYPE_BOT,
        "message_state": MSG_STATE_FINISH,
        "item_list": [{ "type": ITEM_TEXT, "text_item": { "text": text } }],
    });
    if let Some(token) = context_token {
        message["context_token"] = serde_json::Value::String(token.to_string());
    }
    serde_json::json!({ "msg": message })
}

/// 一次取码得到的二维码。
#[derive(Debug, Clone, Default)]
pub struct QrTicket {
    /// 轮询状态用的 token。
    pub qrcode: String,
    /// 微信客户端实际扫描的 liteapp URL(可能为空,回退到 token)。
    pub content: String,
}

/// 二维码登录状态。
#[derive(Debug, Clone, Default)]
pub struct QrState {
    pub qrcode: String,
    pub qr_content: String,
    /// 供前端 `<img src>` 直接展示的 SVG;渲染失败时为空。
    pub qr_svg: String,
    pub status: String,
    pub message: Option<String>,
    pub account: Option<super::store::Account>,
}

impl QrState {
    pub fn waiting() -> Self {
        Self {
            status: "wait".to_string(),
            ..Self::default()
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "confirmed" | "failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实的 getupdates 会把 message_id 与游标下发为数字;以前整批解析失败,
    /// 表现为“发了命令没任何回复”。
    #[test]
    fn tolerates_numeric_ids_and_cursor() {
        let response: UpdatesResponse = serde_json::from_value(serde_json::json!({
            "ret": 0,
            "get_updates_buf": 7517587740095998216i64,
            "msgs": [{
                "from_user_id": "u1",
                "message_id": 7517587740095998216i64,
                "context_token": "ctx",
                "item_list": [{ "type": 1, "text_item": { "text": "/workspace" } }]
            }]
        }))
        .unwrap();
        assert_eq!(
            response.get_updates_buf.as_deref(),
            Some("7517587740095998216")
        );
        assert_eq!(
            response.msgs[0].message_id.as_deref(),
            Some("7517587740095998216")
        );
        assert_eq!(response.msgs[0].text(), "/workspace");
    }

    #[test]
    fn extracts_image_items_including_quotes() {
        let response: UpdatesResponse = serde_json::from_value(serde_json::json!({
            "ret": 0,
            "msgs": [{
                "from_user_id": "u1",
                "message_id": 42,
                "item_list": [{
                    "type": 2,
                    "image_item": {
                        "media": {
                            "encrypt_query_param": "abc%2B=",
                            "aes_key": "AAAAAAAAAAAAAAAAAAAAAA=="
                        },
                        "mid_size": 160,
                        "aeskey": "00112233445566778899aabbccddeeff"
                    }
                }]
            }]
        }))
        .unwrap();
        let images = response.msgs[0].images();
        assert_eq!(images.len(), 1);
        let image = images[0];
        assert_eq!(image.mid_size, Some(160));
        assert_eq!(
            image.aeskey.as_deref(),
            Some("00112233445566778899aabbccddeeff")
        );
        assert_eq!(
            image.media.as_ref().unwrap().encrypt_query_param.as_deref(),
            Some("abc%2B=")
        );
        assert!(response.msgs[0].has_media());
    }

    #[test]
    fn extracts_text_and_quote_prefix() {
        let message: InboundMessage = serde_json::from_value(serde_json::json!({
            "from_user_id": "u1",
            "item_list": [{
                "type": 1,
                "text_item": { "text": "正文" },
                "ref_msg": { "title": "标题", "message_item": { "type": 1, "text_item": { "text": "被引用的内容" } } }
            }]
        }))
        .unwrap();
        assert_eq!(message.text(), "[引用: 标题 | 被引用的内容]\n正文");
        assert!(!message.has_media());
    }

    #[test]
    fn voice_transcript_is_marked() {
        let message: InboundMessage = serde_json::from_value(serde_json::json!({
            "from_user_id": "u1",
            "item_list": [{ "type": 3, "voice_item": { "text": "你好" } }]
        }))
        .unwrap();
        assert_eq!(message.text(), "[语音转写]\n你好");
        assert!(message.has_media());
    }

    #[test]
    fn text_message_echoes_context_token() {
        let value = text_message("u1", "hi", Some("ctx"), "cid");
        assert_eq!(value["msg"]["context_token"], "ctx");
        assert_eq!(value["msg"]["item_list"][0]["text_item"]["text"], "hi");
    }
}
