//! Binary `denia`:纯服务宿主 —— 起 HTTP 服务,控制台由浏览器访问。
//!
//! 桌面端(`denia-desktop`)进程内起的是同一份服务(见 [`denia_server::host`]),
//! 这里只是它的一个薄壳:解析参数、装日志、把 ctrl_c 接到优雅停机。

use denia_server::host::{CliArgs, HostOptions, prepare, resolve_home};
use denia_server::native_folder_picker;
use tracing_subscriber::EnvFilter;

fn main() {
    // Exclusive CLI mode: native folder picker as this process's first window.
    // Must run before tracing so stdout stays a single JSON line.
    if native_folder_picker::is_pick_folder_mode(std::env::args().skip(1)) {
        native_folder_picker::run_as_cli();
    }

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
        let host = match prepare(HostOptions {
            home: home.clone(),
            host: args.host.clone(),
            port: args.port,
            web_dir: args.web.clone(),
        })
        .await
        {
            Ok(host) => host,
            Err(error) => {
                tracing::error!(%error, "failed to initialize");
                std::process::exit(1);
            }
        };
        let addr = host.addr;

        tracing::info!(home = %home.display(), %addr, "denia starting");
        println!();
        println!("  denia console:  http://{addr}");
        println!("  home:            {}", home.display());
        println!();

        host.serve(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .expect("server runs");
    });
}
