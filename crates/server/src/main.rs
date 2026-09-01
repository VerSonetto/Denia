//! axum HTTP API + SSE push + console hosting. Binary: `denia`.

mod api;
mod error;
mod state;
mod web_assets;
mod workspace;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use state::build_state;
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = match CliArgs::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            eprintln!("usage: denia [--home <dir>] [--host <addr>] [--port <port>] [--web <dist>]");
            std::process::exit(2);
        }
    };

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime starts");
    runtime.block_on(async move {
        let home = resolve_home(args.home.as_deref());
        let bound_remote = !matches!(args.host.as_str(), "127.0.0.1" | "localhost" | "::1");
        let state = match build_state(&home, bound_remote) {
            Ok(state) => Arc::new(state),
            Err(error) => {
                tracing::error!(%error, "failed to initialize");
                std::process::exit(1);
            }
        };

        let web_dir = args
            .web
            .clone()
            .filter(|dir| dir.join("index.html").is_file());
        let router = axum::Router::new()
            .merge(api::router())
            .with_state(state.clone())
            .fallback({
                let dir = web_dir.clone();
                move |uri: axum::http::Uri| {
                    let dir = dir.clone();
                    async move { web_assets::response_for(&uri, dir.as_deref()) }
                }
            });
        match &web_dir {
            Some(dist) => tracing::info!(dist = %dist.display(), "serving console from directory"),
            None => tracing::info!("serving embedded console"),
        }

        let addr: SocketAddr = format!("{}:{}", args.host, args.port)
            .parse()
            .unwrap_or_else(|_| panic!("valid bind address {}:{}", args.host, args.port));
        tracing::info!(home = %home.display(), %addr, "denia starting");
        println!();
        println!("  denia console:  http://{addr}");
        println!("  home:            {}", home.display());
        println!();

        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .unwrap_or_else(|error| panic!("bind {addr}: {error}"));
        axum::serve(listener, router)
            .await
            .expect("server runs");
    });
}

struct CliArgs {
    home: Option<PathBuf>,
    host: String,
    port: u16,
    web: Option<PathBuf>,
}

impl CliArgs {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut parsed = Self {
            home: None,
            host: "127.0.0.1".to_string(),
            port: 3600,
            web: None,
        };
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

fn resolve_home(explicit: Option<&std::path::Path>) -> PathBuf {
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
    // 老用户的 ~/.dsh-rs:新目录还不存在时沿用,不丢已有会话与配置。
    let legacy = home.join(".dsh-rs");
    if !next.exists() && legacy.is_dir() {
        return legacy;
    }
    next
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}
