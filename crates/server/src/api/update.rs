//! 更新端点。执行安装要求真实本机连接与浏览器同源请求，不能借远程会话提权。

use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Query};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::Deserialize;

use crate::error::ApiError;
use crate::remote::{Via, guard::RemotePeer};
use crate::state::AppState;
use crate::update::{CheckResponse, InstallStatus, manager};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/update/check", get(check))
        .route("/api/update/status", get(status))
        .route("/api/update/install", post(install))
        .layer(DefaultBodyLimit::max(1024))
        .layer(axum::middleware::map_response(no_store))
}

async fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

#[derive(Default, Deserialize)]
struct CheckQuery {
    #[serde(default)]
    force: bool,
}

async fn check(
    Query(query): Query<CheckQuery>,
    peer: Option<Extension<RemotePeer>>,
    headers: HeaderMap,
) -> Result<Json<CheckResponse>, ApiError> {
    let local = local_request(peer.as_ref().map(|peer| &peer.0), &headers, false);
    let result = manager()
        .map_err(unavailable)?
        .check(query.force, local)
        .await
        .map_err(|message| {
            ApiError::new(StatusCode::BAD_GATEWAY, "update/check-failed", message)
        })?;
    Ok(Json(result))
}

async fn status() -> Result<Json<InstallStatus>, ApiError> {
    Ok(Json(manager().map_err(unavailable)?.status()))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallBody {
    version: String,
}

async fn install(
    peer: Option<Extension<RemotePeer>>,
    headers: HeaderMap,
    Json(body): Json<InstallBody>,
) -> Result<Json<InstallStatus>, ApiError> {
    if !local_request(peer.as_ref().map(|peer| &peer.0), &headers, true) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "update/local-only",
            "安装更新仅允许本机桌面应用的同源请求",
        ));
    }
    let updater = manager().map_err(unavailable)?;
    if !updater.can_install() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "update/desktop-required",
            "此实例不支持自动安装，请在 Windows 桌面应用中操作",
        ));
    }
    let result = updater.install(&body.version).await.map_err(|message| {
        ApiError::new(StatusCode::CONFLICT, "update/install-rejected", message)
    })?;
    Ok(Json(result))
}

fn unavailable(message: String) -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "update/unavailable",
        message,
    )
}

fn local_request(peer: Option<&RemotePeer>, headers: &HeaderMap, require_origin: bool) -> bool {
    if !peer.is_some_and(|peer| peer.via == Via::Local) {
        return false;
    }
    // 未识别的本机反向代理也不能代替桌面发起安装。
    if [
        "cf-ray",
        "cf-connecting-ip",
        "forwarded",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
    ]
    .iter()
    .any(|name| headers.contains_key(*name))
    {
        return false;
    }
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|host| host.to_str().ok())
    else {
        return false;
    };
    let Ok(target) = reqwest::Url::parse(&format!("http://{host}")) else {
        return false;
    };
    let loopback = target.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if !loopback
        || !target.username().is_empty()
        || target.password().is_some()
        || target.path() != "/"
        || target.query().is_some()
        || target.fragment().is_some()
    {
        return false;
    }
    if let Some(site) = headers.get("sec-fetch-site")
        && site != "same-origin"
    {
        return false;
    }
    let Some(origin) = headers.get(header::ORIGIN) else {
        return !require_origin;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Ok(parsed) = reqwest::Url::parse(origin) else {
        return false;
    };
    parsed.origin().ascii_serialization() == origin && parsed.origin() == target.origin()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn local() -> RemotePeer {
        RemotePeer {
            via: Via::Local,
            peer: "127.0.0.1".to_string(),
        }
    }

    fn headers(host: &str, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, host.parse().unwrap());
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
        }
        headers
    }

    #[test]
    fn installation_requires_annotated_local_peer_loopback_host_and_exact_origin() {
        let valid = headers("127.0.0.1:43654", Some("http://127.0.0.1:43654"));
        assert!(local_request(Some(&local()), &valid, true));
        assert!(!local_request(None, &valid, true));
        for via in [Via::Lan, Via::Tunnel] {
            let remote = RemotePeer {
                via,
                peer: "127.0.0.1".to_string(),
            };
            assert!(!local_request(Some(&remote), &valid, true));
        }
        for (host, origin) in [
            ("evil.example:43654", Some("http://evil.example:43654")),
            ("127.0.0.1:43654", Some("http://evil.example")),
            ("127.0.0.1:43654", Some("http://127.0.0.1:43655")),
            ("127.0.0.1:43654", Some("https://127.0.0.1:43654")),
            ("127.0.0.1:43654", Some("null")),
            ("127.0.0.1:43654", Some("http://127.0.0.1:43654/path")),
            ("user@127.0.0.1:43654", Some("http://127.0.0.1:43654")),
            ("127.0.0.1:43654", None),
        ] {
            assert!(
                !local_request(Some(&local()), &headers(host, origin), true),
                "{host}: {origin:?}"
            );
        }
        for host in ["localhost:43654", "[::1]:43654"] {
            assert!(local_request(
                Some(&local()),
                &headers(host, Some(&format!("http://{host}"))),
                true
            ));
        }
        assert!(local_request(
            Some(&local()),
            &headers("127.0.0.1:43654", None),
            false
        ));
    }

    #[test]
    fn proxy_headers_and_cross_site_metadata_cannot_claim_local_installation() {
        for name in [
            "cf-ray",
            "cf-connecting-ip",
            "forwarded",
            "x-forwarded-for",
            "x-forwarded-host",
            "x-forwarded-proto",
        ] {
            let mut request = headers("localhost:43654", Some("http://localhost:43654"));
            request.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                "anything".parse().unwrap(),
            );
            assert!(!local_request(Some(&local()), &request, true));
        }
        let mut request = headers("localhost:43654", Some("http://localhost:43654"));
        request.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert!(!local_request(Some(&local()), &request, true));
        request.insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert!(local_request(Some(&local()), &request, true));
    }

    #[tokio::test]
    async fn http_install_rejects_remote_and_csrf_before_starting_any_update_task() {
        for (via, origin) in [
            (Via::Lan, "http://127.0.0.1:43654"),
            (Via::Tunnel, "http://127.0.0.1:43654"),
            (Via::Local, "http://evil.example"),
        ] {
            let app = Router::new()
                .route("/api/update/install", post(install))
                .layer(Extension(RemotePeer {
                    via,
                    peer: "127.0.0.1".to_string(),
                }))
                .layer(axum::middleware::map_response(no_store));
            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/update/install")
                        .header(header::HOST, "127.0.0.1:43654")
                        .header(header::ORIGIN, origin)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(r#"{"version":"99.0.0"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], "update/local-only");
        }
    }

    #[test]
    fn install_body_does_not_accept_caller_supplied_download_targets() {
        assert!(
            serde_json::from_str::<InstallBody>(
                r#"{"version":"1.0.0","url":"https://evil.example"}"#
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<InstallBody>(r#"{"version":"1.0.0","path":"C:/setup.exe"}"#)
                .is_err()
        );
    }
}
