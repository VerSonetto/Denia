//! 服务宿主:参数解析、绑端口、建 state、装路由、优雅停机。
//!
//! 两个宿主共用它:`denia`(纯服务,浏览器访问控制台)与 `denia-desktop`
//! (Tauri 壳,进程内起同一份服务,窗口加载本机地址)。
//!
//! ## 为什么先绑端口再建 state
//!
//! 端口被占是配置问题,应当在动数据目录之前就报出来。反过来做的话,一次
//! 「3600 已被占用」会先跑完一遍初始化 —— 清扫空壳会话、给工作区账本去引用、
//! 建 MCP 运行时 —— 才失败,而这些副作用都已经落盘了。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::serve::{ListenerExt, TapIo};

use crate::state::{AppState, build_state};

/// 命令行参数。两个宿主同名同义:`--home/--host/--port/--web`。
#[derive(Debug, Clone)]
pub struct CliArgs {
    pub home: Option<PathBuf>,
    pub host: String,
    pub port: u16,
    /// 显式的前端产物目录;`None` = 用内嵌控制台。
    pub web: Option<PathBuf>,
}

impl Default for CliArgs {
    fn default() -> Self {
        Self {
            home: None,
            host: "127.0.0.1".to_string(),
            port: 3600,
            web: None,
        }
    }
}

impl CliArgs {
    pub fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self::default();
        let mut args = args.peekable();
        while let Some(arg) = args.next() {
            let value = |args: &mut std::iter::Peekable<_>, flag: &str| {
                args.next()
                    .ok_or_else(|| format!("missing value for {flag}"))
            };
            match arg.as_str() {
                "--home" => parsed.home = Some(PathBuf::from(value(&mut args, "--home")?)),
                "--host" => parsed.host = value(&mut args, "--host")?,
                "--port" => {
                    parsed.port = value(&mut args, "--port")?
                        .parse()
                        .map_err(|_| "--port must be a number".to_string())?;
                }
                "--web" => parsed.web = Some(PathBuf::from(value(&mut args, "--web")?)),
                "--help" | "-h" => return Err("help requested".to_string()),
                other => return Err(format!("unknown argument: {other}")),
            }
        }
        Ok(parsed)
    }
}

/// 一次启动的输入。
pub struct HostOptions {
    pub home: PathBuf,
    pub host: String,
    pub port: u16,
    pub web_dir: Option<PathBuf>,
}

/// 已就绪但尚未开始 accept 的服务。
///
/// `addr` 是**真实绑定地址**:`--port 0` 时由内核分配,桌面壳据此建窗口,
/// 因此不依赖「默认端口没被占」这个假设。
pub struct Host {
    pub addr: SocketAddr,
    pub state: Arc<AppState>,
    listener: tokio::net::TcpListener,
    router: Router,
}

/// 绑端口、建 state、装路由。失败时进程里没有半启动的服务。
pub async fn prepare(
    options: HostOptions,
) -> Result<Host, Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = format!("{}:{}", options.host, options.port)
        .parse()
        .map_err(|_| format!("invalid bind address {}:{}", options.host, options.port))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|error| format!("bind {addr}: {error}"))?;
    let addr = listener.local_addr()?;

    // 绑定地址非回环 ⇒ 远程浏览器 ⇒ 目录选择器走 browse。
    let bound_remote = !matches!(options.host.as_str(), "127.0.0.1" | "localhost" | "::1");
    let state = Arc::new(build_state(&options.home, bound_remote, addr.port()).await?);

    let web_dir = options
        .web_dir
        .filter(|dir| dir.join("index.html").is_file());
    match &web_dir {
        Some(dist) => tracing::info!(dist = %dist.display(), "serving console from directory"),
        None => tracing::info!("serving embedded console"),
    }

    // 业务 Router:两条 listener 共用同一份路由与 state。
    let business = axum::Router::new()
        .merge(crate::api::router())
        .with_state(state.clone())
        .fallback({
            let dir = web_dir.clone();
            move |uri: axum::http::Uri, headers: axum::http::HeaderMap| {
                let dir = dir.clone();
                async move {
                    // 静态资源走"构建期预压缩 + 按 Accept-Encoding 选实体":
                    // 经公网隧道发到手机时,字节数就是延迟。
                    let accept_encoding = headers
                        .get(axum::http::header::ACCEPT_ENCODING)
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or("");
                    crate::web_assets::response_for(&uri, dir.as_deref(), accept_encoding)
                }
            }
        });
    // API 响应压缩在 api::router() 内部挂载:它只包裹注册时已有的路由,
    // 上面这个 fallback 因此不被压缩层碰 —— 静态资源自己按
    // Accept-Encoding 挑构建期预压缩好的实体,运行时零 CPU。
    // 主 listener:只做来源标注(本机 UI 要看得到 PIN 与票据链接,而
    // 以 `--host 0.0.0.0` 启动时局域网来客必须被标成 Lan 而不是本机)。
    let router = business.clone().layer(axum::middleware::from_fn_with_state(
        state.remote.clone(),
        crate::remote::guard::annotate,
    ));
    // 远程 listener:同一份业务路由,外面再套一层「远程门」。
    state
        .remote
        .attach_router(business.layer(axum::middleware::from_fn_with_state(
            state.remote.clone(),
            crate::remote::guard::gate,
        )));

    Ok(Host {
        addr,
        state,
        listener,
        router,
    })
}

impl Host {
    /// 开始 accept,直到 `shutdown` 完成。
    ///
    /// 收尾必须走 [`AppState::remote`] 的 `shutdown`:隧道子进程与远程
    /// listener 都在它手里,不显式收干净就会留下孤儿 cloudflared。
    pub async fn serve(
        self,
        shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> std::io::Result<()> {
        let remote = self.state.remote.clone();
        axum::serve(
            nodelay(self.listener),
            self.router
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            shutdown.await;
            tracing::info!("shutdown signal received");
            remote.shutdown().await;
        })
        .await
    }
}

/// 给每个 accept 出来的连接关掉 Nagle。
///
/// 控制台的交互形态决定了这很值钱:打字机式的 SSE 是一连串几十到几百字节的
/// 小帧,而手机走蜂窝网络时,对端往往不会立刻回 ACK —— Nagle 会把小帧攒住
/// 等前一帧确认,每一跳白付几十毫秒。keepalive 长连接越多,这个开关的收益越大。
///
/// 返回具体的 `TapIo<…>` 而不是 `impl Listener`:axum 的 `Connected` 实现是
/// 按 `IncomingStream<'_, TapIo<L, F>>` 这个形状写的,类型一旦被抹掉,
/// `into_make_service_with_connect_info::<SocketAddr>()` 就找不到自己的实现。
fn nodelay(
    listener: tokio::net::TcpListener,
) -> TapIo<tokio::net::TcpListener, fn(&mut tokio::net::TcpStream)> {
    listener.tap_io(set_nodelay as fn(&mut tokio::net::TcpStream))
}

fn set_nodelay(stream: &mut tokio::net::TcpStream) {
    if let Err(error) = stream.set_nodelay(true) {
        tracing::debug!(%error, "could not set TCP_NODELAY");
    }
}

pub fn resolve_home(explicit: Option<&Path>) -> PathBuf {
    if let Some(home) = explicit {
        return home.to_path_buf();
    }
    if let Some(home) = std::env::var_os("DENIA_HOME").map(PathBuf::from) {
        return home;
    }
    // 品牌改名前的旧环境变量:仍认,但提示迁移。
    if let Some(home) = std::env::var_os("DSH_RS_HOME").map(PathBuf::from) {
        eprintln!("note: DSH_RS_HOME is deprecated, rename it to DENIA_HOME");
        return home;
    }
    let mut home = std::env::temp_dir();
    if let Some(dir) = dirs_home() {
        home = dir;
    }
    let next = home.join(".denia");
    // 老用户的 ~/.dsh-rs:首次落到 ~/.denia 时整体迁移(一次性目录搬移,
    // 会话/配置/凭据/工作区全保留);之后 ~/.dsh-rs 不再被读取。
    let legacy = home.join(".dsh-rs");
    if legacy.is_dir() {
        migrate_legacy_home(&legacy, &next);
    }
    next
}

/// 把旧品牌目录 `~/.dsh-rs` 整体搬到 `~/.denia`。只在 `~/.denia` 不存在时执行
/// (绝不覆盖新目录里的数据);搬移成功后旧目录留作备份,不再参与解析。
/// 任一步失败仅告警,下次启动重试——绝不让一次搬移失败挡住启动。
fn migrate_legacy_home(legacy: &Path, next: &Path) {
    if next.exists() {
        return;
    }
    // 正式实例可能还开着旧目录里的文件:Windows 上被占用的目录 rename 会
    // 失败,此时留给下次启动重试。
    match std::fs::rename(legacy, next) {
        Ok(()) => {
            tracing::info!(
                from = %legacy.display(),
                to = %next.display(),
                "migrated legacy data directory to ~/.denia"
            );
        }
        Err(error) => {
            tracing::warn!(
                from = %legacy.display(),
                to = %next.display(),
                %error,
                "could not migrate legacy data directory now; will retry on next start"
            );
        }
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}
