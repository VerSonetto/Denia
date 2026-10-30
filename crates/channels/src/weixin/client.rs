//! iLink HTTP 客户端:拉取、发送、typing 与扫码登录接口。
//!
//! - 长轮询超时按"空更新"处理(服务端挂起是正常状态);
//! - 每个 POST 都带 `base_info.channel_version`;
//! - 请求头里的 `X-WECHAT-UIN` 每次随机生成。

use std::time::Duration;

use serde_json::{Value, json};

use super::protocol::{
    API_TIMEOUT, APP_CLIENT_VERSION, APP_ID, CHANNEL_VERSION, EP_GET_BOT_QR, EP_GET_CONFIG,
    EP_GET_QR_STATUS, EP_GET_UPDATES, EP_SEND_MESSAGE, EP_SEND_TYPING, LONG_POLL_TIMEOUT, QrTicket,
    UpdatesResponse, headers, text_message,
};

/// 单次配置查询/typing 的超时。
const CONFIG_TIMEOUT: Duration = Duration::from_secs(10);
/// 扫码接口的超时。
const QR_TIMEOUT: Duration = Duration::from_secs(35);

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl Client {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|error| format!("初始化微信 HTTP 客户端失败:{error}"))?;
        Ok(Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        })
    }

    fn encoded(&self, mut payload: Value) -> Result<String, String> {
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "base_info".to_string(),
                json!({ "channel_version": CHANNEL_VERSION }),
            );
        }
        serde_json::to_string(&payload).map_err(|error| format!("微信请求序列化失败:{error}"))
    }

    fn post_request(
        &self,
        endpoint: &str,
        body: &str,
        timeout: Duration,
    ) -> reqwest::RequestBuilder {
        let mut request = self
            .http
            .post(format!("{}/{}", self.base_url, endpoint))
            .timeout(timeout)
            .body(body.to_string());
        for (name, value) in headers(Some(&self.token), Some(body)) {
            request = request.header(name, value);
        }
        request
    }

    async fn post(
        &self,
        endpoint: &str,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let body = self.encoded(payload)?;
        let response = self
            .post_request(endpoint, &body, timeout)
            .send()
            .await
            .map_err(|error| format!("微信请求失败({endpoint}):{error}"))?;
        read_json(response, endpoint).await
    }

    /// 长轮询:超时按空更新返回,让循环继续。
    pub async fn get_updates(&self, sync_buf: &str) -> Result<UpdatesResponse, String> {
        let body = self.encoded(json!({ "get_updates_buf": sync_buf }))?;
        let response = match self
            .post_request(
                EP_GET_UPDATES,
                &body,
                LONG_POLL_TIMEOUT + Duration::from_secs(5),
            )
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) if error.is_timeout() => return Ok(UpdatesResponse::default()),
            Err(error) => return Err(format!("拉取微信消息失败:{error}")),
        };
        let value = read_json(response, EP_GET_UPDATES).await?;
        serde_json::from_value(value)
            .map_err(|error| format!("微信消息响应结构不符({EP_GET_UPDATES}):{error}"))
    }

    pub async fn send_text(
        &self,
        to: &str,
        text: &str,
        context_token: Option<&str>,
        client_id: &str,
    ) -> Result<Value, String> {
        let payload = text_message(to, text, context_token, client_id);
        self.post(EP_SEND_MESSAGE, payload, API_TIMEOUT).await
    }

    pub async fn get_config(
        &self,
        user_id: &str,
        context_token: Option<&str>,
    ) -> Result<Value, String> {
        let mut payload = json!({ "ilink_user_id": user_id });
        if let Some(token) = context_token {
            payload["context_token"] = Value::String(token.to_string());
        }
        self.post(EP_GET_CONFIG, payload, CONFIG_TIMEOUT).await
    }

    pub async fn send_typing(
        &self,
        to: &str,
        typing_ticket: &str,
        status: i64,
    ) -> Result<(), String> {
        let payload = json!({
            "ilink_user_id": to,
            "typing_ticket": typing_ticket,
            "status": status,
        });
        self.post(EP_SEND_TYPING, payload, API_TIMEOUT)
            .await
            .map(|_| ())
    }

    /// 下载媒体(图片/文件)密文;大小上限由调用方检查。
    pub async fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        let response = self
            .http
            .get(url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|error| format!("下载微信媒体失败:{error}"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("下载微信媒体 HTTP {status}"));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|error| format!("读取微信媒体失败:{error}"))?;
        if bytes.len() > super::media::MAX_IMAGE_BYTES {
            return Err(format!(
                "微信媒体超过大小上限({} MB)",
                super::media::MAX_IMAGE_BYTES / 1024 / 1024
            ));
        }
        Ok(bytes.to_vec())
    }
}

async fn read_json(response: reqwest::Response, endpoint: &str) -> Result<Value, String> {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    if !status.is_success() {
        let preview: String = text.chars().take(200).collect();
        return Err(format!("微信接口 HTTP {status} ({endpoint}):{preview}"));
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("微信响应不是合法 JSON({endpoint}):{error}"))
}

fn qr_http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|error| format!("初始化微信扫码客户端失败:{error}"))
}

fn qr_request(url: String) -> Result<reqwest::RequestBuilder, String> {
    Ok(qr_http()?
        .get(url)
        .header("iLink-App-Id", APP_ID)
        .header("iLink-App-ClientVersion", APP_CLIENT_VERSION.to_string())
        .timeout(QR_TIMEOUT))
}

/// 取登录二维码。`bot_type=3` 是个人微信 bot 类型。
pub async fn fetch_qr(base_url: &str) -> Result<QrTicket, String> {
    let url = format!(
        "{}/{}?bot_type=3",
        base_url.trim_end_matches('/'),
        EP_GET_BOT_QR
    );
    let response = qr_request(url)?
        .send()
        .await
        .map_err(|error| format!("获取微信登录二维码失败:{error}"))?;
    let value = read_json(response, EP_GET_BOT_QR).await?;
    let qrcode = value
        .get("qrcode")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if qrcode.is_empty() {
        return Err("微信登录二维码响应缺少 qrcode".to_string());
    }
    let content = value
        .get("qrcode_img_content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok(QrTicket { qrcode, content })
}

/// 查询扫码状态。返回原始 JSON,由登录状态机解释。
pub async fn poll_qr(base_url: &str, qrcode: &str) -> Result<Value, String> {
    let url = format!(
        "{}/{}?qrcode={}",
        base_url.trim_end_matches('/'),
        EP_GET_QR_STATUS,
        qrcode
    );
    let response = qr_request(url)?
        .send()
        .await
        .map_err(|error| format!("查询微信扫码状态失败:{error}"))?;
    read_json(response, EP_GET_QR_STATUS).await
}
