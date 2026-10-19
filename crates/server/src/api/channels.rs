//! 消息通道端点:微信连接状态与扫码登录。
//!
//! 登录是异步的:POST 只启动状态机,前端轮询 `GET /api/channels/weixin`
//! 直到 `login.status` 变成 `confirmed` 或 `failed`。

use std::sync::Arc;

use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/channels/weixin", get(weixin_status))
        .route("/api/channels/weixin/login", post(weixin_login))
        .route("/api/channels/weixin/logout", post(weixin_logout))
}

fn weixin_json(state: &AppState) -> serde_json::Value {
    let account = state.im.weixin_account();
    let (connected, last_error) = state.im.weixin_status();
    let (last_inbound_at, last_inbound_chars, last_action) = state.im.weixin_activity();
    let login = state.im.weixin_login();
    json!({
        "connected": connected,
        // 账号 token 永不回传:前端只需要展示"连的是哪个号"。
        "account": account.map(|account| json!({
            "accountId": account.account_id,
            "userId": account.user_id,
        })),
        "lastError": last_error,
        "lastInboundAt": last_inbound_at,
        "lastInboundChars": last_inbound_chars,
        "lastAction": last_action,
        "login": login.map(|login| json!({
            "status": login.status,
            "message": login.message,
            "qrSvg": login.qr_svg,
            "qrContent": login.qr_content,
        })),
    })
}

async fn weixin_status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(weixin_json(&state))
}

async fn weixin_login(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let host: Arc<dyn denia_channels::bridge::ChannelHost> =
        Arc::new(crate::im::ServerChannelHost(state.clone()));
    state.im.start_login(host);
    Json(weixin_json(&state))
}

async fn weixin_logout(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    state.im.logout().await;
    Ok(Json(weixin_json(&state)))
}
