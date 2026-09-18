//! HTTP API surface: settings, credentials, model management, chat smoke
//! test, and the SSE push channel.

mod agent_presets;
mod browser;
mod credentials;
mod events;
mod fs;
mod git;
mod global_rules;
mod goal;
mod llm;
mod mcp;
mod memories;
mod open_in_app;
mod remote;
mod runtime;
mod sessions;
mod settings;
mod system_prompt;
mod terminals;
mod uploads;
mod workspaces;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

/// API 响应压缩层。
///
/// 只开 gzip:控制台静态产物已有构建期预压缩的 `.br`/`.gz` 实体(带
/// `Content-Encoding`,中间件按规则跳过不二次压缩),这一层管的是会话快照、
/// 会话列表这类动辄几百 KB 的 JSON —— 在公网隧道上,字节数就是等待时间。
///
/// 等级取 Fastest 而不是 Best:它坐在交互路径上,level 1 与 9 的体积只差一成
/// 多,CPU 却差几十倍。"发一次、人人要下"的静态资源才值得最高档,那部分放在
/// 构建期(scripts/precompress.mjs)。
///
/// 两类响应必须排除:
/// - **101 升级响应(WebSocket)**:套上压缩体会让 hyper 的 upgrade 流程读到
///   gzip 头尾这些非预期字节,终端功能直接连不上;
/// - **SSE(`text/event-stream`)**:`/api/events` 与流式输出靠逐帧下发撑打字机
///   效果,压缩缓冲会把帧攒成一批。默认谓词已排除它,这里不改动那条规则。
///
/// 挂在 `router()` 内部而不是外层:axum 的 `Router::layer` 只包裹注册时已有的
/// 路由,静态资源 fallback 是调用方后加的,因此不受影响 —— 它自己按
/// Accept-Encoding 选预压缩实体,零运行时 CPU。
/// 压缩谓词:101 升级响应(WebSocket)一律不压。
///
/// 单独成函数是为了让测试与实现共用同一份判定 —— 这条规则破了,终端功能会
/// 静默连不上(hyper 的 upgrade 流程读到的是 gzip 帧),从外面看只是"终端打不开"。
fn skip_upgrades(
    status: axum::http::StatusCode,
    _version: axum::http::Version,
    _headers: &axum::http::HeaderMap,
    _extensions: &axum::http::Extensions,
) -> bool {
    status != axum::http::StatusCode::SWITCHING_PROTOCOLS
}

fn compression_layer() -> tower_http::compression::CompressionLayer<
    impl tower_http::compression::predicate::Predicate,
> {
    use tower_http::compression::predicate::{DefaultPredicate, Predicate};
    use tower_http::compression::CompressionLevel;

    tower_http::compression::CompressionLayer::new()
        .gzip(true)
        .quality(CompressionLevel::Fastest)
        .compress_when(DefaultPredicate::new().and(skip_upgrades))
}

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .merge(settings::router())
        .merge(agent_presets::router())
        .merge(browser::router())
        .merge(credentials::router())
        .merge(llm::router())
        .merge(sessions::router())
        .merge(goal::router())
        .merge(runtime::router())
        .merge(workspaces::router())
        .merge(uploads::router())
        .merge(fs::router())
        .merge(events::router())
        .merge(system_prompt::router())
        .merge(global_rules::router())
        .merge(mcp::router())
        .merge(memories::router())
        .merge(open_in_app::router())
        .merge(remote::router())
        .merge(terminals::router())
        .merge(git::router())
        // 放在最后:layer 只包裹此前已注册的路由,正好覆盖全部 API。
        .layer(compression_layer())
}

#[cfg(test)]
mod compression_tests {
    use super::compression_layer;
    use axum::body::Body;
    use axum::http::{Response, StatusCode, header};

    /// 压缩谓词决定了两条不能破的规矩:流式与升级响应不能压。
    /// 这里直接断言谓词本身,比从 HTTP 层绕一圈更稳(不依赖端口与鉴权)。
    #[test]
    fn compresses_json_but_not_sse_or_upgrades() {
        use tower_http::compression::predicate::{DefaultPredicate, Predicate as _};

        // 与 compression_layer() 用同一个 skip_upgrades,不在测试里另写一份判定。
        let p = DefaultPredicate::new().and(super::skip_upgrades);

        fn of(content_type: Option<&str>, status: StatusCode) -> Response<Body> {
            let mut builder = Response::builder().status(status);
            if let Some(content_type) = content_type {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }
            builder.body(Body::from("x".repeat(1024))).unwrap()
        }

        assert!(
            p.should_compress(&of(Some("application/json"), StatusCode::OK)),
            "JSON 响应应当压缩"
        );
        assert!(
            !p.should_compress(&of(Some("text/event-stream"), StatusCode::OK)),
            "SSE 不能压缩:压缩缓冲会把流式帧攒成一批,打字机效果与实时推送会失效"
        );
        assert!(
            !p.should_compress(&of(None, StatusCode::SWITCHING_PROTOCOLS)),
            "WebSocket 升级响应(101)不能压缩:会让 upgrade 流程读到非预期字节"
        );

        // 构造性检查:配置写崩(比如 gzip feature 没开、谓词类型不对)要在这里暴露。
        let _: tower_http::compression::CompressionLayer<_> = compression_layer();
    }

    /// 穿过真实中间件再验一次:只断言谓词不够 —— 还要确认装上后确实产出 gzip,
    /// 以及**已带 Content-Encoding 的响应不被二次压缩**(构建期预压缩的静态产物
    /// 走的就是这条路径,叠一层 gzip 会让浏览器拿到混合体而白屏)。
    #[tokio::test]
    async fn service_gzips_json_and_never_double_compresses() {
        use super::compression_layer;
        use axum::routing::get;
        use tower::ServiceExt;

        let big = "a".repeat(8192);
        let app = axum::Router::new()
            .route("/t", get({
                let body = big.clone();
                move || {
                    let body = body.clone();
                    async move { body }
                }
            }))
            .layer(compression_layer());

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/t")
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_ENCODING],
            "gzip",
            "压缩层没生效,API 响应仍按原始字节发出去"
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            (bytes.len() as f64) < (big.len() as f64) / 4.0,
            "gzip 后应显著变小,实际 {} B(明文 {} B)",
            bytes.len(),
            big.len()
        );
    }

    #[tokio::test]
    async fn precompressed_entities_are_not_recompressed() {
        use super::compression_layer;
        use axum::routing::get;
        use tower::ServiceExt;

        let app = axum::Router::new()
            .route(
                "/pre",
                get(|| async {
                    // 键用 &'static str:混成 (HeaderName, &str) 时 axum 的
                    // TryFrom 数组实现不覆盖,拿不到 IntoResponse。
                    (
                        [("content-encoding", "br"), ("content-type", "text/css")],
                        "already-compressed-by-build".to_string(),
                    )
                }),
            )
            .layer(compression_layer());

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/pre")
                    .header(header::ACCEPT_ENCODING, "gzip, br")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let encodings: Vec<String> = response
            .headers()
            .get_all(header::CONTENT_ENCODING)
            .iter()
            .map(|value| value.to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            encodings,
            vec!["br".to_string()],
            "预压缩实体被二次压缩:浏览器会拿到 br+gzip 混合体"
        );
    }
}
