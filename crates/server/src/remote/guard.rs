//! 远程门(guard):挂在远程 listener 上的鉴权与来源校验中间件。
//!
//! ## 判定顺序(每一步都不能提前放行)
//!
//! 1. **来源分类**:回环 + 带 CF 头 = 隧道流量;回环无 CF 头 = 本机直通;
//!    其余 = 局域网。拿不到 peer 信息按局域网处理(最保守)。
//! 2. **本机直通**:`Via::Local` 且本机是请求目标时直接放行 —— 桌面 UI 自己
//!    不需要票据。判定依据是 Host 是本机地址(不是"peer 是回环",否则
//!    cloudflared 回源也会被误判)。
//! 3. **HTTPS 强制**(仅隧道):`X-Forwarded-Proto` 必须是 https。
//! 4. **Host 白名单**:防 DNS rebinding —— 攻击者把 `evil.com` 解析到
//!    127.0.0.1,浏览器会带上我们的 cookie,但 Host 头是 `evil.com`。
//! 5. **静态资源白名单**:控制台外壳(html/js/css)必须能在无凭据时加载,
//!    否则扫码页根本渲染不出来。只放行"扩展名属于前端资源"的 GET,
//!    且路径不在 `/api/` 下。
//! 6. **兑换端点**:`/api/remote/*` 自带票据/限流逻辑,不走会话鉴权。
//! 7. **会话鉴权**:其余 `/api/*` 要求有效会话 cookie。

use std::sync::Arc;

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::{RemoteManager, SESSION_COOKIE, Via};

/// 免鉴权的兑换/状态端点前缀。
const REMOTE_API_PREFIX: &str = "/api/remote/";

/// 远程 listener 上的请求扩展:交给下游 handler 的来源信息。
#[derive(Debug, Clone)]
pub struct RemotePeer {
    pub via: Via,
    /// 有效来源 IP(隧道流量取 `cf-connecting-ip`)。
    pub peer: String,
}

/// 只标注来源、不做任何拦截的中间件。
///
/// 主 listener 用它:本机 UI 的请求需要被正确标成 `Local`(而不是"没有
/// 标注就默认本机"),否则用户以 `--host 0.0.0.0` 启动时,局域网里的任何
/// 人都能被当成"本机",看到 PIN 与带票据链接。
pub async fn annotate(
    State(manager): State<Arc<RemoteManager>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let _ = manager;
    insert_peer(&mut request);
    next.run(request).await
}

/// 取连接的对端地址。
///
/// 直接读请求扩展而不是用 `ConnectInfo` 提取器:提取器是 `FromRequestParts`
/// 且没有 `Optional` 实现,写进中间件签名就会要求**所有**挂载点都必须
/// 装配 connect info;而 `MockConnectInfo` 走的是另一个扩展键,测试也得跟着
/// 分叉。这里两种键都认,中间件因此对装配方式不敏感。
fn connect_addr(request: &Request<Body>) -> Option<std::net::SocketAddr> {
    request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr)
        .or_else(|| {
            request
                .extensions()
                .get::<MockConnectInfo<std::net::SocketAddr>>()
                .map(|MockConnectInfo(addr)| *addr)
        })
}

/// 从请求里判定来源并写入扩展。
fn insert_peer(request: &mut Request<Body>) {
    let peer = connect_addr(request);
    let headers = request.headers();
    let has_cf = headers.contains_key("cf-ray") || headers.contains_key("cf-connecting-ip");
    let via = RemoteManager::classify(peer, has_cf);
    let effective = RemoteManager::effective_peer(
        peer,
        via,
        headers
            .get("cf-connecting-ip")
            .and_then(|value| value.to_str().ok()),
    );
    request.extensions_mut().insert(RemotePeer {
        via,
        peer: effective,
    });
}

/// 中间件主体。
pub async fn gate(
    State(manager): State<Arc<RemoteManager>>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    insert_peer(&mut request);
    let (via, effective) = {
        let peer = request.extensions().get::<RemotePeer>().unwrap();
        (peer.via, peer.peer.clone())
    };
    let headers = request.headers().clone();

    // 1. 本机直通:桌面 UI 走主 listener 本来就不经过这道门;这里再兜一次
    //    "远程 listener 上的本机请求",让开发时直连远程端口也能用。
    if via == Via::Local {
        return next.run(request).await;
    }

    // 2. 功能总开关。
    if !manager.config().enabled {
        return deny(
            StatusCode::FORBIDDEN,
            "remote/disabled",
            "远程连接已在设置中关闭",
        );
    }

    // 3. 隧道流量必须经 HTTPS 到达。
    if via == Via::Tunnel && !forwarded_https(&headers) {
        return deny(
            StatusCode::BAD_REQUEST,
            "remote/https-required",
            "公网隧道连接必须使用 HTTPS",
        );
    }

    // 4. Host 白名单(防 DNS rebinding)。
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let tunnel_host = manager.tunnel_host().await;
    if !manager.host_allowed(&host, tunnel_host.as_deref()) {
        tracing::warn!(%host, %effective, "rejected request with unlisted Host header");
        return deny(
            StatusCode::FORBIDDEN,
            "remote/host-rejected",
            "请求的 Host 不在允许列表内",
        );
    }

    let path = request.uri().path().to_string();
    let method = request.method().clone();

    // 5. 静态资源与兑换端点无需会话。
    if is_console_asset(&path, &method) || path.starts_with(REMOTE_API_PREFIX) {
        return next.run(request).await;
    }

    // 6. 会话鉴权。
    let token = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| cookie_value(cookies, SESSION_COOKIE));
    let Some(token) = token else {
        return deny(
            StatusCode::UNAUTHORIZED,
            "remote/session-required",
            "需要远程访问会话,请重新扫码连接",
        );
    };
    match manager.authenticate(&token, &effective) {
        Some(_) => next.run(request).await,
        None => deny(
            StatusCode::UNAUTHORIZED,
            "remote/session-invalid",
            "远程会话已失效,请重新扫码连接",
        ),
    }
}

/// `X-Forwarded-Proto` 是否为 https(cloudflared 会设置)。
fn forwarded_https(headers: &HeaderMap) -> bool {
    headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(',')
                .next()
                .map(str::trim)
                .is_some_and(|proto| proto.eq_ignore_ascii_case("https"))
        })
        .unwrap_or(false)
}

/// 控制台外壳资源:扫码后浏览器要能先渲染出页面,才能提交票据。
///
/// 只放行 GET/HEAD 且扩展名属于前端产物(`.html`/`.js`/`.css`/图标/字体)。
/// 其余路径一律要求会话 —— 包括 `/api/*`,所以即便有人猜到资源路径,
/// 也拿不到任何数据接口。
fn is_console_asset(path: &str, method: &Method) -> bool {
    if !matches!(*method, Method::GET | Method::HEAD) {
        return false;
    }
    if path.starts_with("/api/") {
        return false;
    }
    // 根路径与 SPA 深链交给 index.html。
    if path == "/" || !path.rsplit('/').next().unwrap_or_default().contains('.') {
        return true;
    }
    let extension = path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    matches!(
        extension.as_str(),
        "html"
            | "js"
            | "mjs"
            | "css"
            | "json"
            | "map"
            | "svg"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "ico"
            | "woff"
            | "woff2"
            | "ttf"
            | "otf"
            | "txt"
            | "webmanifest"
    )
}

/// 从 Cookie 头里取一个值。
fn cookie_value(cookies: &str, name: &str) -> Option<String> {
    cookies.split(';').find_map(|part| {
        let (key, value) = part.split_once('=')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

/// 统一的拒绝响应:与项目其余 API 一致的 `{error:{code,message}}` 形状,
/// 并带 `Cache-Control: no-store`,避免中间层缓存住 401。
fn deny(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({ "error": { "code": code, "message": message } });
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(body),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_assets_are_public_but_apis_are_not() {
        assert!(is_console_asset("/", &Method::GET));
        assert!(is_console_asset("/index.html", &Method::GET));
        assert!(is_console_asset("/assets/index-abc123.js", &Method::GET));
        assert!(is_console_asset("/assets/index-abc123.css", &Method::GET));
        assert!(is_console_asset("/favicon.ico", &Method::GET));
        // SPA 深链(无扩展名)交 index.html。
        assert!(is_console_asset("/sessions/abc", &Method::GET));
        // 数据接口一律要会话。
        assert!(!is_console_asset("/api/sessions", &Method::GET));
        assert!(!is_console_asset("/api/remote/status", &Method::GET));
        // 写方法不放行,即便路径像资源。
        assert!(!is_console_asset("/index.html", &Method::POST));
        assert!(!is_console_asset("/assets/app.js", &Method::DELETE));
    }

    #[test]
    fn forwarded_proto_must_be_https() {
        let mut headers = HeaderMap::new();
        assert!(!forwarded_https(&headers), "没有该头时不能当作 https");
        headers.insert("x-forwarded-proto", "http".parse().unwrap());
        assert!(!forwarded_https(&headers));
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(forwarded_https(&headers));
        // 多级代理链取第一段。
        headers.insert("x-forwarded-proto", "https, http".parse().unwrap());
        assert!(forwarded_https(&headers));
        headers.insert("x-forwarded-proto", "HTTPS".parse().unwrap());
        assert!(forwarded_https(&headers), "大小写不敏感");
    }

    #[test]
    fn cookie_lookup_is_exact_and_tolerant_of_spacing() {
        let cookies = "a=1; denia_remote_session=tok123 ; b=2";
        assert_eq!(
            cookie_value(cookies, SESSION_COOKIE).as_deref(),
            Some("tok123")
        );
        assert_eq!(cookie_value(cookies, "a").as_deref(), Some("1"));
        assert_eq!(cookie_value(cookies, "missing"), None);
        assert_eq!(cookie_value("", SESSION_COOKIE), None);
        // 名字是前缀关系时不能误配。
        assert_eq!(cookie_value("denia_remote_session_x=1", SESSION_COOKIE), None);
    }
}
