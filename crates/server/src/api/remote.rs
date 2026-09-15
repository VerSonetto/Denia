//! 远程连接的 REST 入口:统一连接管理(选择方式 → 生成链接与二维码 →
//! 展示状态 → 断开)。
//!
//! 这组端点挂在**两处**:主 listener(本机 UI 调用,可看到 PIN 与票据链接)
//! 与远程 listener(扫码后前端在这里兑换票据)。差别由「远程门」的
//! `RemotePeer` 扩展区分:只有本机来源才拿得到 PIN 与带票据的链接。

use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::remote::{ExchangeOutcome, RemoteManager, SESSION_COOKIE, Via, guard::RemotePeer};
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/remote/status", get(status))
        .route("/api/remote/lan/start", post(start_lan))
        .route("/api/remote/lan/stop", post(stop_lan))
        .route("/api/remote/lan/address", post(select_lan_address))
        .route("/api/remote/tunnel/start", post(start_tunnel))
        .route("/api/remote/tunnel/stop", post(stop_tunnel))
        .route("/api/remote/ticket/refresh", post(refresh_ticket))
        .route("/api/remote/sessions/revoke", post(revoke_session))
        .route("/api/remote/disconnect-all", post(disconnect_all))
        .route("/api/remote/exchange", post(exchange))
        .route("/api/remote/pin", post(verify_pin))
        .route("/api/remote/logout", post(logout))
}

/* ---- 来源判定 ---- */

/// 请求来自本机(桌面 UI):看得到 PIN 与带票据链接。
///
/// 两条 listener 上都装了标注中间件(`annotate` / `gate`),所以
/// `RemotePeer` 扩展一定存在;缺失只可能是测试直连 Router,按本机处理。
fn is_local(peer: Option<&RemotePeer>) -> bool {
    peer.is_none_or(|peer| peer.via == Via::Local)
}

fn peer_of(peer: Option<&RemotePeer>) -> String {
    peer.map(|peer| peer.peer.clone())
        .unwrap_or_else(|| "local".to_string())
}

fn extension_peer(peer: Option<Extension<RemotePeer>>) -> Option<RemotePeer> {
    peer.map(|Extension(peer)| peer)
}

/* ---- 状态 ---- */

async fn status(
    State(state): State<Arc<AppState>>,
    peer: Option<Extension<RemotePeer>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let peer = extension_peer(peer);
    let local = is_local(peer.as_ref());
    let status = state.remote.status(local).await;
    let mut value = serde_json::to_value(status).map_err(internal)?;
    // 前端要靠这个布尔决定「关闭隧道」按钮是否出现:PIN 与带票据链接的
    // 有无已经隐含了它,但显式给一个字段比让 UI 去反推更不容易出错。
    if let Some(object) = value.as_object_mut() {
        object.insert("local".to_string(), serde_json::Value::Bool(local));
    }
    Ok(Json(value))
}

/* ---- 开启 / 关闭 ---- */

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LanStart {
    /// 选定的网卡地址;留空取推荐项。
    #[serde(default)]
    address: Option<String>,
}

async fn start_lan(
    State(state): State<Arc<AppState>>,
    Json(body): Json<LanStart>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let status = state
        .remote
        .start_lan(body.address)
        .await
        .map_err(bad_request)?;
    Ok(Json(serde_json::to_value(status).map_err(internal)?))
}

async fn stop_lan(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let stopped = state.remote.stop_lan().await;
    Json(json!({ "ok": true, "stopped": stopped }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddressBody {
    address: String,
}

async fn select_lan_address(
    State(state): State<Arc<AppState>>,
    Json(body): Json<AddressBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state
        .remote
        .select_lan_address(&body.address)
        .map_err(bad_request)?;
    let status = state.remote.status(true).await;
    Ok(Json(serde_json::to_value(status).map_err(internal)?))
}

async fn start_tunnel(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let status = state.remote.start_tunnel().await.map_err(bad_request)?;
    Ok(Json(serde_json::to_value(status).map_err(internal)?))
}

async fn stop_tunnel(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let stopped = state.remote.stop_tunnel("user-request").await;
    Json(json!({ "ok": true, "stopped": stopped }))
}

async fn refresh_ticket(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    state.remote.refresh_ticket().await.map_err(bad_request)?;
    let status = state.remote.status(true).await;
    Ok(Json(serde_json::to_value(status).map_err(internal)?))
}

#[derive(Debug, Deserialize)]
struct SessionBody {
    id: String,
}

async fn revoke_session(
    State(state): State<Arc<AppState>>,
    Json(body): Json<SessionBody>,
) -> Json<serde_json::Value> {
    let revoked = state.remote.revoke_session(&body.id);
    Json(json!({ "ok": true, "revoked": revoked }))
}

/// 「立即关闭隧道并吊销全部会话」。
async fn disconnect_all(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let revoked = state.remote.disconnect_all("user-request").await;
    Json(json!({ "ok": true, "revoked": revoked }))
}

/* ---- 兑换 ---- */

#[derive(Debug, Deserialize)]
struct ExchangeBody {
    ticket: String,
}

/// 票据兑换。返回会话令牌(写进 HttpOnly Cookie)或 PIN 挑战。
async fn exchange(
    State(state): State<Arc<AppState>>,
    peer: Option<Extension<RemotePeer>>,
    Json(body): Json<ExchangeBody>,
) -> Result<Response, ApiError> {
    let peer = peer_of(extension_peer(peer).as_ref());
    match state.remote.exchange(&body.ticket, &peer) {
        Ok(ExchangeOutcome::PinRequired { challenge, attempts }) => Ok(Json(json!({
            "status": "pin-required",
            "challenge": challenge,
            "attempts": attempts,
        }))
        .into_response()),
        Ok(ExchangeOutcome::Session { token, via }) => {
            Ok(session_response(&state.remote, via_of(via), token))
        }
        Err(failure) => Err(failure.into()),
    }
}

#[derive(Debug, Deserialize)]
struct PinBody {
    challenge: String,
    pin: String,
}

async fn verify_pin(
    State(state): State<Arc<AppState>>,
    peer: Option<Extension<RemotePeer>>,
    Json(body): Json<PinBody>,
) -> Result<Response, ApiError> {
    let peer = peer_of(extension_peer(peer).as_ref());
    // 会话 cookie 的属性(尤其 `Secure`)按**票据所属通道**决定,不看这次
    // 请求从哪来:手机在隧道上提交 PIN,请求经 cloudflared 回源,来源是
    // 隧道;但若有人把票据拿回本机兑换,也必须拿到隧道口径的 cookie。
    let via = state
        .remote
        .challenge_channel(&body.challenge)
        .map(via_of)
        .unwrap_or(Via::Tunnel);
    match state.remote.verify_pin(&body.challenge, &body.pin, &peer) {
        Ok(issued) => Ok(session_response(&state.remote, via, issued.token)),
        Err(failure) => Err(failure.into()),
    }
}

/// 退出当前远程会话:吊销会话并清 cookie。
async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(token) = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| cookie_value(cookies, SESSION_COOKIE))
    {
        state.remote.revoke_token(&token);
    }
    (
        StatusCode::OK,
        [(header::SET_COOKIE, clear_cookie(&state.remote).await)],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

/* ---- 会话 Cookie ---- */

/// 会话响应:令牌只走 `Set-Cookie`,不出现在 JSON body 里
/// (body 会被前端日志/错误上报之类的地方顺手记下来)。
fn session_response(manager: &RemoteManager, via: Via, token: String) -> Response {
    let cookie = cookie_for(manager, via, &token);
    let max_age = manager.session_absolute_seconds(via);
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({ "status": "ok", "via": via.as_str(), "maxAge": max_age })),
    )
        .into_response()
}

/// 组会话 Cookie。隧道通道强制 `Secure`;局域网是 http,带 `Secure` 会被
/// 浏览器直接丢弃,所以只在隧道通道加 —— 这是"公网不削弱、局域网从简"
/// 的落点,局域网侧的残余风险见交付说明。
fn cookie_for(manager: &RemoteManager, via: Via, token: &str) -> String {
    let secure = manager.cookie_secure(via);
    let max_age = manager.session_absolute_seconds(via);
    let mut cookie = format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age}"
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

async fn clear_cookie(manager: &RemoteManager) -> String {
    let _ = manager;
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

/// 通道 → 请求来源。cookie 属性与超时参数按通道决定。
fn via_of(channel: crate::remote::Channel) -> Via {
    match channel {
        crate::remote::Channel::Lan => Via::Lan,
        crate::remote::Channel::Tunnel => Via::Tunnel,
    }
}

fn cookie_value(cookies: &str, name: &str) -> Option<String> {
    cookies.split(';').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

fn bad_request(message: String) -> ApiError {
    ApiError::bad_request("remote/operation-failed", message)
}

/// 把领域失败翻译成带 `Retry-After` 的 HTTP 响应。
impl From<crate::remote::RemoteFailure> for ApiError {
    fn from(failure: crate::remote::RemoteFailure) -> Self {
        ApiError::new(failure.status, failure.code, failure.message)
            .with_retry_after(failure.retry_after_seconds)
    }
}

fn internal(error: serde_json::Error) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "remote/serialize",
        error.to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::guard::{RemotePeer, annotate, gate};
    use crate::remote::{Channel, RemoteManager, Via};
    use axum::body::Body;
    use axum::extract::connect_info::MockConnectInfo;
    use axum::http::{Request, StatusCode, header};
    use tower::ServiceExt;

    /// 造一个临时 home 上的 AppState + 业务 Router(与 main.rs 同构)。
    async fn harness() -> (Arc<AppState>, axum::Router) {
        let home = std::env::temp_dir().join(format!("denia-remote-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let state = Arc::new(crate::state::build_state(&home, false).await.unwrap());
        // 测试里的 Host 用一个固定的"局域网地址";真实环境靠网卡探测进白名单,
        // 测试机不一定有对应网段,所以显式列进 allowedHosts。
        state
            .settings
            .update(
                crate::remote::config::REMOTE_NS,
                json!({"allowedHosts": [DEFAULT_HOST]}),
                None,
            )
            .unwrap();
        // 与 main.rs 同构:API 路由 + 静态资源 fallback(远程连接要能把
        // 控制台 HTML 发给扫码的浏览器,少了这层测不出真实形态)。
        let business = crate::api::router()
            .with_state(state.clone())
            .fallback(|uri: axum::http::Uri| async move {
                crate::web_assets::response_for(&uri, None)
            });
        // start_lan 会起真实 listener,需要先装配 Router(与 main.rs 同序)。
        state.remote.attach_router(business.clone());
        (state, business)
    }

    /// 主 listener 形态:只有来源标注。
    fn main_app(state: &Arc<AppState>, business: &axum::Router) -> axum::Router {
        business
            .clone()
            .layer(axum::middleware::from_fn_with_state(
                state.remote.clone(),
                annotate,
            ))
    }

    /// 远程 listener 形态:来源标注 + 远程门。
    fn remote_app(state: &Arc<AppState>, business: &axum::Router) -> axum::Router {
        business
            .clone()
            .layer(axum::middleware::from_fn_with_state(
                state.remote.clone(),
                gate,
            ))
    }

    /// 用给定的 peer 地址发一个请求。
    async fn send(
        app: axum::Router,
        peer: &str,
        request: Request<Body>,
    ) -> (StatusCode, HeaderMap, serde_json::Value) {
        let app = app.layer(MockConnectInfo(peer.parse::<std::net::SocketAddr>().unwrap()));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
        (status, headers, value)
    }

    /// 默认 Host:局域网模式下的合法访问地址(真实 HTTP/1.1 请求必带 Host)。
    const DEFAULT_HOST: &str = "192.168.1.10:3602";

    fn post(path: &str, body: serde_json::Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::HOST, DEFAULT_HOST)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn get(path: &str) -> Request<Body> {
        Request::builder()
            .method("GET")
            .uri(path)
            .header(header::HOST, DEFAULT_HOST)
            .body(Body::empty())
            .unwrap()
    }

    /// 直接问 manager 要一张局域网票据(避免依赖真实的网卡绑定)。
    fn lan_ticket(state: &Arc<AppState>, single_use: bool) -> String {
        state
            .remote
            .issue_ticket_for_test(Channel::Lan, "http://192.168.1.10:3602", single_use)
    }

    #[tokio::test]
    async fn exchange_requires_a_valid_ticket() {
        let (state, business) = harness().await;
        let app = remote_app(&state, &business);

        // 伪造的票据被拒,并且不带 cookie。
        let (status, headers, body) = send(
            app.clone(),
            "192.168.1.20:5000",
            post("/api/remote/exchange", json!({"ticket": "not-a-ticket"})),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "remote/ticket-invalid");
        assert!(!headers.contains_key(header::SET_COOKIE), "失败时不得发 cookie");

        // 正确票据换到会话 cookie。
        let ticket = lan_ticket(&state, false);
        let (status, headers, body) = send(
            app,
            "192.168.1.20:5000",
            post("/api/remote/exchange", json!({"ticket": ticket})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "ok");
        let cookie = headers
            .get(header::SET_COOKIE)
            .expect("必须下发会话 cookie")
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("SameSite=Strict"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
        // 局域网是 http:`Secure` 会被浏览器丢弃,所以这里不能带。
        assert!(!cookie.contains("Secure"), "局域网 cookie 不该带 Secure:{cookie}");
    }

    #[tokio::test]
    async fn api_requires_session_and_rejects_host_mismatch() {
        let (state, business) = harness().await;
        let app = remote_app(&state, &business);

        // 无 cookie 访问数据接口 → 401。
        let (status, _, body) = send(app.clone(), "192.168.1.20:5000", get("/api/sessions")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "remote/session-required");

        // 兑换拿到 cookie。
        let ticket = lan_ticket(&state, false);
        let (_, headers, _) = send(
            app.clone(),
            "192.168.1.20:5000",
            post("/api/remote/exchange", json!({"ticket": ticket})),
        )
        .await;
        let cookie = headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        // 带 cookie + 合法 Host → 放行。
        let mut request = get("/api/sessions");
        request
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        request
            .headers_mut()
            .insert(header::HOST, "192.168.1.10:3602".parse().unwrap());
        let (status, _, _) = send(app.clone(), "192.168.1.20:5000", request).await;
        assert_eq!(status, StatusCode::OK, "同网段的合法 Host 应当放行");

        // 带 cookie 但 Host 是外部域名(DNS rebinding)→ 403。
        let mut request = get("/api/sessions");
        request
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        request
            .headers_mut()
            .insert(header::HOST, "evil.example.com".parse().unwrap());
        let (status, _, body) = send(app, "192.168.1.20:5000", request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"]["code"], "remote/host-rejected");
    }

    #[tokio::test]
    async fn tunnel_traffic_must_be_https_and_gets_secure_cookie() {
        let (state, business) = harness().await;
        let app = remote_app(&state, &business);
        // 回环 peer + CF 头 = 隧道流量。
        let tunnel_peer = "127.0.0.1:5000";

        // 没有 X-Forwarded-Proto: https → 拒绝。
        let ticket = lan_ticket(&state, false);
        let mut request = post("/api/remote/exchange", json!({"ticket": ticket}));
        request
            .headers_mut()
            .insert("cf-ray", "abc123".parse().unwrap());
        request
            .headers_mut()
            .insert(header::HOST, "127.0.0.1:3602".parse().unwrap());
        let (status, _, body) = send(app.clone(), tunnel_peer, request).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"]["code"], "remote/https-required");

        // 隧道票据(强制一次性)兑换出来的 cookie 必须带 Secure。
        // 关掉 PIN 要求,这条用例只关心 HTTPS 与 cookie 属性。
        state
            .settings
            .update(
                crate::remote::config::REMOTE_NS,
                json!({"tunnel": {"requirePin": false}}),
                None,
            )
            .unwrap();
        let token = state
            .remote
            .issue_ticket_for_test(Channel::Tunnel, "https://x.trycloudflare.com", true);
        let mut request = post("/api/remote/exchange", json!({"ticket": token}));
        request
            .headers_mut()
            .insert("cf-ray", "abc123".parse().unwrap());
        request
            .headers_mut()
            .insert("x-forwarded-proto", "https".parse().unwrap());
        request
            .headers_mut()
            .insert(header::HOST, "127.0.0.1:3602".parse().unwrap());
        let (status, headers, body) = send(app, tunnel_peer, request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let cookie = headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.contains("Secure"), "公网会话必须带 Secure:{cookie}");
    }

    #[tokio::test]
    async fn pin_flow_issues_session_only_after_correct_pin() {
        let (state, business) = harness().await;
        let app = remote_app(&state, &business);
        // 打开隧道的 PIN 二次验证:直接构造一张要求 PIN 的票据。
        let pin = state.remote.set_pin_for_test(Channel::Tunnel, "246810");
        let token = state
            .remote
            .issue_ticket_for_test(Channel::Tunnel, "https://x.trycloudflare.com", true);

        let mut request = post("/api/remote/exchange", json!({"ticket": token}));
        request
            .headers_mut()
            .insert("cf-ray", "abc".parse().unwrap());
        request
            .headers_mut()
            .insert("x-forwarded-proto", "https".parse().unwrap());
        request
            .headers_mut()
            .insert(header::HOST, "127.0.0.1:3602".parse().unwrap());
        let (status, headers, body) = send(app.clone(), "127.0.0.1:5000", request).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "pin-required");
        assert!(!headers.contains_key(header::SET_COOKIE), "PIN 未过不得发会话");
        let challenge = body["challenge"].as_str().unwrap().to_string();

        // 错误 PIN → 401,且给出剩余次数。
        let (status, _, body) = send(
            app.clone(),
            "127.0.0.1:5000",
            post(
                "/api/remote/pin",
                json!({"challenge": challenge, "pin": "000000"}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"]["code"], "remote/pin-invalid");

        // 正确 PIN → 会话 cookie。
        let (status, headers, body) = send(
            app,
            "127.0.0.1:5000",
            post(
                "/api/remote/pin",
                json!({"challenge": challenge, "pin": pin}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let cookie = headers
            .get(header::SET_COOKIE)
            .expect("PIN 正确后必须下发会话")
            .to_str()
            .unwrap()
            .to_string();
        assert!(cookie.contains("Secure"), "隧道会话必须带 Secure:{cookie}");
    }

    #[tokio::test]
    async fn pin_failures_are_rate_limited() {
        let (state, business) = harness().await;
        let app = remote_app(&state, &business);
        state.remote.set_pin_for_test(Channel::Tunnel, "111111");
        // 把退避阈值调小,测试不必等真实延迟。
        state
            .settings
            .update(
                crate::remote::config::REMOTE_NS,
                json!({"rateLimit": {"maxFailures": 2, "baseDelayMs": 60000}}),
                None,
            )
            .unwrap();
        state.remote.apply_settings();

        let token = state
            .remote
            .issue_ticket_for_test(Channel::Tunnel, "https://x.trycloudflare.com", true);
        let mut request = post("/api/remote/exchange", json!({"ticket": token}));
        request
            .headers_mut()
            .insert("cf-ray", "abc".parse().unwrap());
        request
            .headers_mut()
            .insert("x-forwarded-proto", "https".parse().unwrap());
        request
            .headers_mut()
            .insert(header::HOST, "127.0.0.1:3602".parse().unwrap());
        let (_, _, body) = send(app.clone(), "127.0.0.1:5000", request).await;
        let challenge = body["challenge"].as_str().unwrap().to_string();

        // 连续两次错 PIN 即触发退避。
        for _ in 0..2 {
            let (status, _, _) = send(
                app.clone(),
                "127.0.0.1:5000",
                post(
                    "/api/remote/pin",
                    json!({"challenge": challenge, "pin": "000000"}),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }

        let (status, headers, body) = send(
            app,
            "127.0.0.1:5000",
            post(
                "/api/remote/pin",
                json!({"challenge": challenge, "pin": "111111"}),
            ),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "限流必须是 429 而不是 401,否则客户端分不清'密码错'与'被限流':{body}"
        );
        assert!(
            body["error"]["code"] == "remote/backoff"
                || body["error"]["code"] == "remote/rate-limited",
            "退避后必须被限流,得到 {body}"
        );
        let retry_after = headers
            .get(header::RETRY_AFTER)
            .expect("限流响应必须带 Retry-After")
            .to_str()
            .unwrap()
            .parse::<u64>()
            .expect("Retry-After 是秒数");
        assert!(retry_after >= 1, "必须告诉用户等多久,得到 {retry_after}");
    }

    #[tokio::test]
    async fn disconnect_revokes_every_session_and_ticket() {
        let (state, business) = harness().await;
        let app = remote_app(&state, &business);

        let ticket = lan_ticket(&state, false);
        let (_, headers, _) = send(
            app.clone(),
            "192.168.1.20:5000",
            post("/api/remote/exchange", json!({"ticket": ticket})),
        )
        .await;
        let cookie = headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();

        let mut request = get("/api/sessions");
        request
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        request
            .headers_mut()
            .insert(header::HOST, "192.168.1.10:3602".parse().unwrap());
        let (status, _, _) = send(app.clone(), "192.168.1.20:5000", request).await;
        assert_eq!(status, StatusCode::OK);

        // 「立即关闭并吊销全部会话」。
        state.remote.disconnect_all("test").await;

        let mut request = get("/api/sessions");
        request
            .headers_mut()
            .insert(header::COOKIE, cookie.parse().unwrap());
        request
            .headers_mut()
            .insert(header::HOST, "192.168.1.10:3602".parse().unwrap());
        let (status, _, body) = send(app.clone(), "192.168.1.20:5000", request).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "断开后旧 cookie 必须立即失效");
        assert_eq!(body["error"]["code"], "remote/session-invalid");

        // 旧票据同样失效。
        let (status, _, _) = send(
            app,
            "192.168.1.20:5000",
            post("/api/remote/exchange", json!({"ticket": ticket})),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "断开后票据必须失效");
    }

    #[tokio::test]
    async fn local_requests_bypass_the_gate() {
        let (state, business) = harness().await;
        let app = remote_app(&state, &business);
        // 回环 peer、无 CF 头 = 本机直通。
        let (status, _, _) = send(app, "127.0.0.1:5000", get("/api/sessions")).await;
        assert_eq!(status, StatusCode::OK, "本机请求不该被远程门拦下");
    }

    /// 真实 listener 端到端:起局域网连接 → 用真 HTTP 客户端从"外部"访问
    /// → 关闭后端口确实释放、旧链接确实失效。
    ///
    /// 这条用例是唯一覆盖"远程 listener 真的能收发 HTTP"的测试 —— 上面的
    /// `oneshot` 用例只走内存路由,碰不到 bind/serve/释放这条链路。
    #[tokio::test]
    async fn lan_listener_serves_traffic_and_releases_port_on_stop() {
        let (state, _business) = harness().await;
        let status = state.remote.start_lan(None).await.expect("局域网连接应当能开启");
        let port = status.port;
        assert!(port > 0, "必须拿到真实端口");
        let ticket_url = status.link.ticket_url.clone();
        let ticket = ticket_url
            .split("ticket=")
            .nth(1)
            .expect("链接必须带票据")
            .to_string();

        // 客户端按"局域网来客"访问。连接走回环(测试机上局域网地址可能被
        // 防火墙静默丢包,直连那个地址会把用例挂死),但 Host 头按局域网
        // 地址给 —— Host 白名单校验因此真的被执行到。
        let base = format!("http://127.0.0.1:{port}");
        let lan_host = format!("{}:{}", status.address, port);
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();

        let response = client
            .get(format!("{base}/api/remote/status"))
            .header("host", lan_host.clone())
            .send()
            .await
            .expect("远程端口必须能连上");
        assert_eq!(response.status(), 200);
        let body: serde_json::Value = response.json().await.unwrap();
        assert_eq!(body["lan"]["port"], port);

        // 带票据链接对应的路径 + query:能拿到控制台 HTML。
        let ticket_path = ticket_url
            .split_once(&format!(":{port}"))
            .map(|(_, rest)| rest.to_string())
            .expect("票据链接必须指向本次绑定的端口");
        let response = client
            .get(format!("{base}{ticket_path}"))
            .header("host", lan_host.clone())
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success(), "带票据链接应当能打开控制台");

        // 关闭:端口必须真的释放(重新 bind 同一端口成功才算释放)。
        assert!(state.remote.stop_lan().await);
        let rebind = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
        assert!(rebind.is_ok(), "关闭局域网连接后端口必须释放");

        // 旧票据同样作废。
        let exchange = client
            .post(format!("{base}/api/remote/exchange"))
            .header("host", lan_host)
            .json(&serde_json::json!({"ticket": ticket}))
            .send()
            .await;
        assert!(
            exchange.is_err() || !exchange.unwrap().status().is_success(),
            "关闭后旧链接不得再能兑换"
        );
    }

    #[tokio::test]
    async fn status_hides_secrets_from_remote_clients() {
        let (state, business) = harness().await;
        let app = remote_app(&state, &business);
        // 局域网开起来(绑 0.0.0.0:0),拿到票据与 PIN 视图。
        state.remote.start_lan(None).await.unwrap();
        state.remote.set_pin_for_test(Channel::Lan, "135790");

        // 本机:看得到带票据链接与 PIN。
        let (status, _, body) = send(app.clone(), "127.0.0.1:5000", get("/api/remote/status")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["lan"]["link"]["ticketUrl"].as_str().unwrap().contains("ticket="));
        assert_eq!(body["lan"]["link"]["pin"], "135790");
        assert!(!body["lan"]["link"]["qrSvg"].as_str().unwrap().is_empty());

        // 局域网来客:即便带了合法 Host,也拿不到票据链接与 PIN。
        let mut request = get("/api/remote/status");
        request
            .headers_mut()
            .insert(header::HOST, "192.168.1.10:3602".parse().unwrap());
        let (status, _, body) = send(app, "192.168.1.20:5000", request).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body["lan"]["link"]["ticketUrl"].as_str().unwrap() == body["lan"]["link"]["url"],
            "远程客户端不该拿到带票据的链接:{body}"
        );
        assert!(body["lan"]["link"]["pin"].is_null(), "PIN 不得回给远程客户端");
    }

    /// 隧道已开(回源 listener 绑回环)时再开局域网:必须在**同一端口**上
    /// 换成局域网绑定,否则 cloudflared 的回源目标就没人监听了。
    #[tokio::test]
    async fn lan_can_start_while_tunnel_is_running() {
        let (state, _business) = harness().await;
        // 先造出"隧道专用回环 listener"的等价状态(不起真实 cloudflared)。
        let tunnel_port = state
            .remote
            .spawn_listener_for_test(
                std::net::SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0),
                false,
            )
            .await
            .expect("回环 listener 应当能起");

        let lan = state
            .remote
            .start_lan(None)
            .await
            .expect("隧道运行时也必须能开局域网");
        assert_eq!(
            lan.port, tunnel_port,
            "局域网必须复用隧道回源端口,否则隧道指向没人监听的地址"
        );

        // 局域网关掉后,隧道回源 listener 应当被重新放回去(而不是留下一个
        // 开着局域网绑定、但用户以为已经关掉的 listener)。
        assert!(state.remote.stop_lan().await);
        assert!(
            state.remote.listener_active(),
            "关局域网不该顺手把隧道回源监听也拆掉"
        );
    }

    /// 两条通道同时开着时,各自的票据链接必须互不串台。
    ///
    /// 曾经共用一个"最近签发的票据"槽:开隧道后局域网卡片会显示隧道的
    /// 链接与二维码 —— 用户扫了会连到公网入口,与卡片描述完全不符。
    #[tokio::test]
    async fn tickets_are_tracked_per_channel() {
        let (state, business) = harness().await;
        let app = remote_app(&state, &business);
        state.remote.start_lan(None).await.unwrap();

        // 隧道票据槽单独写入(不起真实隧道:只验证两条槽互不覆盖)。
        state
            .remote
            .set_ticket_for_test(Channel::Tunnel, "https://x.trycloudflare.com/?ticket=t");

        let (status, _, body) = send(app, "127.0.0.1:5000", get("/api/remote/status")).await;
        assert_eq!(status, StatusCode::OK);
        let lan_ticket = body["lan"]["link"]["ticketUrl"].as_str().unwrap();
        assert!(
            lan_ticket.contains("192.168."),
            "局域网卡片必须显示局域网链接,而不是隧道的:{lan_ticket}"
        );
        assert!(
            state.remote.ticket_for_test(Channel::Tunnel).is_some(),
            "隧道票据槽应当还在"
        );
        assert_ne!(
            state.remote.ticket_for_test(Channel::Lan).unwrap(),
            state.remote.ticket_for_test(Channel::Tunnel).unwrap(),
            "两条通道不得共用同一个票据"
        );
    }
}
