//! QQ Bot REST 客户端:access_token、gateway 地址、发消息与附件下载。

use std::time::{Duration, Instant};

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::protocol::{
    API_BASE, ChatType, GATEWAY_PATH, MAX_MESSAGE_LENGTH, TOKEN_URL, text_body,
};

const API_TIMEOUT: Duration = Duration::from_secs(30);
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);
/// 附件下载上限:QQ 图片一般几 MB,限制避免内存被单条消息吃满。
pub const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

pub struct Client {
    http: reqwest::Client,
    app_id: String,
    client_secret: String,
    token: Mutex<Option<(String, Instant)>>,
}

impl Client {
    pub fn new(app_id: impl Into<String>, client_secret: impl Into<String>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| format!("初始化 QQ HTTP 客户端失败:{error}"))?;
        Ok(Self {
            http,
            app_id: app_id.into(),
            client_secret: client_secret.into(),
            token: Mutex::new(None),
        })
    }

    /// 有效 access_token(锁内单飞刷新)。
    pub async fn token(&self) -> Result<String, String> {
        let mut guard = self.token.lock().await;
        if let Some((token, expires_at)) = guard.as_ref()
            && Instant::now() + TOKEN_REFRESH_MARGIN < *expires_at
        {
            return Ok(token.clone());
        }
        let response = self
            .http
            .post(TOKEN_URL)
            .json(&json!({
                "appId": self.app_id,
                "clientSecret": self.client_secret,
            }))
            .timeout(API_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("QQ 获取 access_token 失败:{error}"))?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let preview: String = text.chars().take(200).collect();
            return Err(format!("QQ 获取 access_token HTTP {status}:{preview}"));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|error| format!("QQ token 响应不是合法 JSON:{error}"))?;
        let access = value
            .get("access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| "QQ token 响应缺少 access_token".to_string())?
            .to_string();
        let expires = value
            .get("expires_in")
            .and_then(|raw| {
                raw.as_u64()
                    .or_else(|| raw.as_str().and_then(|text| text.parse().ok()))
            })
            .unwrap_or(7200);
        *guard = Some((access.clone(), Instant::now() + Duration::from_secs(expires)));
        Ok(access)
    }

    async fn headers(&self) -> Result<Vec<(String, String)>, String> {
        let token = self.token().await?;
        Ok(vec![
            (AUTHORIZATION.as_str().to_string(), format!("QQBot {token}")),
            (CONTENT_TYPE.as_str().to_string(), "application/json".to_string()),
        ])
    }

    async fn get_json(&self, url: &str) -> Result<Value, String> {
        let mut request = self.http.get(url).timeout(API_TIMEOUT);
        for (name, value) in self.headers().await? {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("QQ 请求失败:{error}"))?;
        read_json(response).await
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value, String> {
        let mut request = self
            .http
            .post(format!("{API_BASE}{path}"))
            .timeout(API_TIMEOUT)
            .json(&body);
        for (name, value) in self.headers().await? {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("QQ 请求失败({path}):{error}"))?;
        read_json(response).await
    }

    pub async fn gateway_url(&self) -> Result<String, String> {
        let value = self
            .get_json(&format!("{API_BASE}{GATEWAY_PATH}"))
            .await?;
        value
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| "QQ gateway 响应缺少 url".to_string())
    }

    /// 发送文本。`msg_id` 只给第一片:QQ 被动回复窗口用它绑定用户消息。
    pub async fn send_text(
        &self,
        chat_type: ChatType,
        chat_id: &str,
        content: &str,
        msg_id: Option<&str>,
        msg_seq: u32,
    ) -> Result<(), String> {
        let path = match chat_type {
            ChatType::Group => format!("/v2/groups/{chat_id}/messages"),
            ChatType::C2c => format!("/v2/users/{chat_id}/messages"),
        };
        let text: String = content.chars().take(MAX_MESSAGE_LENGTH).collect();
        self.post_json(&path, text_body(&text, msg_id, msg_seq))
            .await
            .map(|_| ())
    }

    /// 下载附件(QQ 多媒体 CDN 需要 Authorization 头)。
    pub async fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut request = self.http.get(url).timeout(Duration::from_secs(30));
        for (name, value) in self.headers().await? {
            request = request.header(name, value);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("下载 QQ 附件失败:{error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("下载 QQ 附件 HTTP {status}"));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("读取 QQ 附件失败:{error}"))?;
        if bytes.len() > MAX_ATTACHMENT_BYTES {
            return Err(format!(
                "QQ 附件超过大小上限({} MB)",
                MAX_ATTACHMENT_BYTES / 1024 / 1024
            ));
        }
        Ok(bytes.to_vec())
    }
}

async fn read_json(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let preview: String = text.chars().take(300).collect();
        return Err(format!("QQ 接口 HTTP {status}:{preview}"));
    }
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&text).map_err(|error| format!("QQ 响应不是合法 JSON:{error}"))
}
