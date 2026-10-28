//! denia 桌面端:Tauri 壳 + 进程内 axum 服务。
//!
//! ## 形态
//!
//! 壳在**进程内**起一份与 `denia`(纯服务宿主)完全相同的服务 —— 同一个
//! [`denia_server::host::prepare`],同一份路由装配、来源标注与优雅停机 ——
//! 然后把窗口指向它真实绑定的地址(`http://127.0.0.1:<port>`)。
//!
//! 这样做的直接结果是**前端一行都不用改**:控制台走的还是它熟悉的那条
//! HTTP/SSE/WebSocket 路径,打字机增量、终端 PTY、构建期预压缩实体、
//! 远程连接全都原样生效。桌面端与 Web 端不是两套实现,只是同一份服务的
//! 两个入口。
//!
//! ## 为什么不用自定义协议直传(不监听端口)
//!
//! dsh 的 Electron 壳走的是 `dsh-app://` + 分帧字节管道,那是因为它的宿主
//! 是 Node 子进程,需要一套跨进程协议。denia 是 Rust 单体:壳与服务同进程,
//! 再套一层 IPC 只会把已经跑通的 SSE/WebSocket 重新实现一遍,还多出两套
//! 协议各自维护性能的问题。
//!
//! ## 端口
//!
//! 默认与 `denia` 共用同一套参数,绑 `127.0.0.1:43654`,然后把窗口指向
//! 真实绑定地址。若要同时跑 `denia` 实例与桌面端,或换一个端口,用 `--port`
//! (传 `0` 让内核分配空闲端口)。
//!
//! ## 退出顺序
//!
//! 窗口全关时 Tauri 默认直接结束进程,但服务侧的收尾(杀 cloudflared 隧道、
//! 关远程 listener、清票据与会话)还没跑完。所以退出被拦一道:先发停机信号,
//! 等 `serve` 返回(收尾完成)才真正落闸,见 [`ExitGate`]。
//!
//! ## 与 `denia` 二进制的关系
//!
//! 两者共用 [`denia_server::host`]。桌面端额外负责三件事:开窗口、支持
//! `--pick-folder` 子进程模式(目录选择器在 Windows 上必须由"以该对话框为
//! 第一个窗口"的进程来弹,见 `denia_server::native_folder_picker`)、以及
//! 上面那道退出闸门。

// 双击启动时不要多出一个黑色控制台窗口。仅 release 开:debug 下保留控制台,
// 日志与 panic 信息看得见。
//
// 这不影响 `--pick-folder` 子进程:它由服务端以管道承接 stdout
// (`Stdio::piped()`),GUI 子系统进程同样继承父进程给的管道句柄,
// `println!` 照常写得进去。
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod console;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use denia_server::host::{CliArgs, HostOptions, prepare, resolve_home};
use denia_server::native_folder_picker;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::sync::oneshot;

/// 主窗口标题。
const WINDOW_TITLE: &str = "Denia";

/// 主窗口 label。Tauri 用 label 定位窗口,固定值便于后续查找(导航、报错)。
const MAIN_WINDOW: &str = "main";

/// 默认窗口尺寸。
const WINDOW_SIZE: (f64, f64) = (1280.0, 860.0);

/// 占位页入场动画的自然时长:约 1.1s 写完整字标,随后墨点落款约 0.7s。
/// 服务就绪后要等它播完再导航,否则动画不是被截断就是整段跳过。
const PLACEHOLDER_ANIMATION: Duration = Duration::from_millis(1800);

/// 等占位页加载的上限。冷启动时 WebView2 初始化常要一两秒,超过就不再等。
const PLACEHOLDER_LOAD_LIMIT: Duration = Duration::from_millis(4000);

fn main() {
    // 独占模式:目录选择器子进程。**必须排在 `console::adopt()` 之前** ——
    // 它的 stdout 是父进程给的管道,JSON 结果靠它回传;adopt 会把空的标准
    // 句柄接到控制台上,顺序反了就白拿不到结果。
    if native_folder_picker::is_pick_folder_mode(std::env::args().skip(1)) {
        native_folder_picker::run_as_cli();
    }
    if native_folder_picker::is_pick_files_mode(std::env::args().skip(1)) {
        native_folder_picker::run_files_as_cli();
    }

    // 必须在任何子进程之前:给壳持有一个(隐藏的)控制台,否则每一次
    // git/bash/MCP/cloudflared 都会在用户眼前闪一个黑框。见 `console`。
    console::adopt();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = match parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            eprintln!(
                "usage: denia-desktop [--home <dir>] [--host <addr>] [--port <port>] \
                 [--web <dist>] [--width <px>] [--height <px>]"
            );
            std::process::exit(2);
        }
    };

    let home = resolve_home(args.host.home.as_deref());
    let (gate, shutdown_rx) = ExitGate::new();

    tauri::Builder::default()
        .manage(gate.clone())
        .setup(move |app| {
            #[cfg(windows)]
            denia_server::update::register_installer_launcher(
                denia_server::update::launch_windows_installer,
            )
            .map_err(std::io::Error::other)?;
            let handle = app.handle().clone();
            let options = HostOptions {
                home: home.clone(),
                host: args.host.host.clone(),
                port: args.host.port,
                web_dir: args.host.web.clone(),
            };
            let size = match (args.width, args.height) {
                (None, None) => WINDOW_SIZE,
                (width, height) => (
                    width.unwrap_or(WINDOW_SIZE.0),
                    height.unwrap_or(WINDOW_SIZE.1),
                ),
            };
            let service_gate = gate.clone();
            // 占位页最近一次加载完成的时刻。服务通常比 WebView2 初始化更快,
            // 不等这个信号就会在页面还没画出来时导航,入场动画被整段跳过。
            let page_loaded_at = Arc::new(Mutex::new(None::<Instant>));

            // 服务跑在 Tauri 的异步运行时上:与壳同进程同 runtime,服务一挂
            // 窗口就没有意义了,不需要独立 runtime 的生命周期管理。
            tauri::async_runtime::spawn(async move {
                // 窗口先建在**内嵌占位页**上,服务就绪后再导航过去。
                // 反过来做(先等服务再建窗口)会让启动的几百毫秒里什么都不
                // 显示,用户看到的是"双击了没反应";而失败时也没有窗口可以
                // 承载错误信息 —— 那正是最需要看见原因的时候。
                if let Err(error) = open_window(&handle, size, page_loaded_at.clone()) {
                    tracing::error!(%error, "failed to create window");
                    service_gate.mark_cleaned();
                    handle.exit(1);
                    return;
                }

                match prepare(options).await {
                    Ok(host) => {
                        let addr = host.addr;
                        tracing::info!(%addr, home = %home.display(), "denia desktop serving");
                        // 服务就绪 ≠ 启动页已画出来。先等占位页加载完成(冷启动
                        // WebView2 常晚于服务就绪),再按动画的自然时长放完,最后
                        // 才导航 —— 否则动画要么被截断,要么整段看不到。
                        let load_deadline = Instant::now() + PLACEHOLDER_LOAD_LIMIT;
                        let mut loaded_at = placeholder_loaded_at(&page_loaded_at);
                        while loaded_at.is_none() && Instant::now() < load_deadline {
                            tokio::time::sleep(Duration::from_millis(16)).await;
                            loaded_at = placeholder_loaded_at(&page_loaded_at);
                        }
                        if let Some(loaded_at) = loaded_at {
                            let remaining = (loaded_at + PLACEHOLDER_ANIMATION)
                                .saturating_duration_since(Instant::now());
                            if !remaining.is_zero() {
                                tokio::time::sleep(remaining).await;
                            }
                        }
                        if let Err(error) = navigate(&handle, addr.port()) {
                            tracing::error!(%error, "failed to load console");
                            fail(&handle, &format!("无法加载控制台:{error}"));
                        }
                        // 收尾完成后才允许真正退出 —— `serve` 内部会杀隧道、
                        // 关远程 listener、清票据,那些都得在进程存活时跑。
                        if let Err(error) = host
                            .serve(async move {
                                let _ = shutdown_rx.await;
                            })
                            .await
                        {
                            tracing::error!(%error, "server stopped");
                        }
                        service_gate.mark_cleaned();
                        handle.exit(0);
                    }
                    Err(error) => {
                        // 起不来必须让用户看见:静默退出会留下一个"点了没反应"
                        // 的图标,那比报错难查得多。
                        tracing::error!(%error, "failed to initialize denia service");
                        fail(&handle, &error.to_string());
                    }
                }
            });

            // 从终端里跑时 ctrl_c 也要能正常退出(带收尾)。
            let signal = gate.clone();
            tauri::async_runtime::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    signal.signal();
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("tauri application builds")
        .run(|app, event| {
            // 窗口全关时 Tauri 默认直接结束进程。这里先拦住,把停机信号递出去,
            // 等 `serve` 收尾完成后再由上面那个任务调 `exit` 真正退出。
            if let tauri::RunEvent::ExitRequested { api, .. } = event {
                let gate = app.state::<ExitGate>();
                if gate.is_cleaned() {
                    // 收尾已经跑完(或从未起过服务),这次退出就是最终落闸。
                    return;
                }
                api.prevent_exit();
                gate.signal();
            }
        });
}

/// 占位页最近一次加载完成的时刻。
fn placeholder_loaded_at(state: &Mutex<Option<Instant>>) -> Option<Instant> {
    *state.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// 建主窗口,停在**内嵌占位页**上;服务就绪后由 [`navigate`] 送到真实地址。
///
/// 先建窗口再等服务的顺序很重要:启动的几百毫秒里用户应当看到东西,
/// 而不是"双击了没反应";失败时也才有窗口可以承载错误信息。
fn open_window(
    handle: &tauri::AppHandle,
    size: (f64, f64),
    page_loaded_at: Arc<Mutex<Option<Instant>>>,
) -> tauri::Result<()> {
    let builder =
        WebviewWindowBuilder::new(handle, MAIN_WINDOW, WebviewUrl::App("index.html".into()));
    // Windows 默认只解码 ICO 首帧(16px)，任务栏缩放需使用高分辨率源图。
    #[cfg(windows)]
    let builder = builder.icon(tauri::include_image!("icons/128x128@2x.png"))?;
    // Windows 的原生拖放处理会拦截 WebView 内的 HTML5 拖放（会话分屏、文件引用等）。
    #[cfg(windows)]
    let builder = builder.disable_drag_drop_handler();
    builder
        .title(WINDOW_TITLE)
        // 使用前端自绘标题栏，避免原生装饰与应用视觉体系割裂。
        .decorations(false)
        .inner_size(size.0, size.1)
        .min_inner_size(720.0, 480.0)
        .center()
        // 窗口最终停在哪一页是排查"白屏"时唯一有用的线索:占位页与真实控制台
        // 都叫 Denia,不看 URL 分不出是导航没发生还是控制台自己挂了。
        .on_page_load(move |_webview, payload| {
            let url = payload.url();
            tracing::info!(url = %url, "desktop window loaded page");
            // 只认内嵌占位页:about:blank 是 WebView2 的初始页,导航后的
            // 控制台地址不该再被当成"启动页已就绪"。Windows 下 Tauri 的
            // App URL 是 http://tauri.localhost/,不是 tauri:// scheme。
            if url.scheme() == "tauri"
                || url.host_str() == Some("tauri.localhost")
                || url.as_str().contains("index.html")
            {
                // 每次加载都覆盖:占位页若被重新加载,动画从头开始,
                // 等待锚点也应跟着后移。
                *page_loaded_at
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = Some(Instant::now());
            }
        })
        .on_new_window(|url, _features| {
            if is_external_web_url(&url) {
                tauri::async_runtime::spawn_blocking(move || {
                    if let Err(error) = open_external_web_url(&url) {
                        tracing::warn!(%error, "could not open external link");
                    }
                });
            }
            tauri::webview::NewWindowResponse::Deny
        })
        .build()?;
    Ok(())
}

fn is_external_web_url(url: &tauri::Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
}

#[cfg(windows)]
fn open_external_web_url(url: &tauri::Url) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    if !is_external_web_url(url) {
        return Err("仅支持打开 HTTP/HTTPS 链接".to_string());
    }
    let file: Vec<u16> = url.as_str().encode_utf16().chain(Some(0)).collect();
    let verb: Vec<u16> = "open\0".encode_utf16().collect();
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    } as isize;
    if result > 32 {
        Ok(())
    } else {
        Err(format!("系统浏览器启动失败（错误 {result}）"))
    }
}

#[cfg(not(windows))]
fn open_external_web_url(url: &tauri::Url) -> Result<(), String> {
    if !is_external_web_url(url) {
        return Err("仅支持打开 HTTP/HTTPS 链接".to_string());
    }
    let command = if cfg!(target_os = "macos") {
        "/usr/bin/open"
    } else {
        "xdg-open"
    };
    std::process::Command::new(command)
        .arg(url.as_str())
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("系统浏览器启动失败: {error}"))
}

#[cfg(test)]
mod external_link_tests {
    use super::is_external_web_url;

    #[test]
    fn external_links_only_allow_web_protocols_without_credentials() {
        for url in [
            "https://github.com/VerSonetto/Denia",
            "http://example.com/path?q=1",
        ] {
            assert!(is_external_web_url(&url.parse().unwrap()));
        }
        for url in [
            "file:///C:/Windows/notepad.exe",
            "javascript:alert(1)",
            "data:text/plain,hi",
            "mailto:test@example.com",
            "https://user:password@example.com",
        ] {
            assert!(!is_external_web_url(&url.parse().unwrap()));
        }
    }
}

/// 服务就绪:把主窗口导航到真实控制台地址。
fn navigate(handle: &tauri::AppHandle, port: u16) -> tauri::Result<()> {
    let window = handle
        .get_webview_window(MAIN_WINDOW)
        .ok_or_else(|| tauri::Error::WindowNotFound)?;
    window.navigate(loopback(port))?;
    Ok(())
}

fn loopback(port: u16) -> tauri::Url {
    format!("http://127.0.0.1:{port}")
        .parse()
        .expect("loopback url parses")
}

/// 启动失败:把原因写进已经开着的那个窗口。
///
/// 用初始化脚本注入而不是 data: URL —— 实测 WebView2 不渲染 data: 顶层导航,
/// 那会让用户对着一片空白窗口猜发生了什么。窗口本身也留着:用户看得见原因,
/// 而不是一个静默消失的进程。
fn fail(handle: &tauri::AppHandle, message: &str) {
    let Some(window) = handle.get_webview_window(MAIN_WINDOW) else {
        // 窗口都没建起来(极早期失败):至少让日志留下痕迹。
        tracing::error!(%message, "denia desktop failed before a window existed");
        return;
    };
    let script = format!(
        "document.body.dataset.fatal = '1';\
         document.getElementById('fatal-message').textContent = {};",
        serde_json::to_string(message).unwrap_or_else(|_| "\"启动失败\"".to_string())
    );
    if let Err(error) = window.eval(&script) {
        tracing::error!(%error, %message, "could not display startup failure in the window");
    }
}

/// 退出闸门:把"请求退出"和"允许退出"分成两件事。
///
/// Tauri 的 `ExitRequested` 在窗口全关时会立刻触发,而服务侧收尾是异步的。
/// 没有这道闸门,隧道进程会被留在系统里(cloudflared 是子进程,父进程一没
/// 就成孤儿),远程 listener 也不会释放。信号用 oneshot:只发一次,
/// `take` 天然幂等。
#[derive(Clone)]
struct ExitGate {
    signal: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    /// 服务侧收尾已完成(或从未起过服务)。
    cleaned: Arc<AtomicBool>,
}

impl ExitGate {
    /// 返回闸门与它对应的停机信号接收端。接收端是 owned 的,可以安全地
    /// 移动进 `serve` 的停机 future。
    fn new() -> (Self, oneshot::Receiver<()>) {
        let (tx, rx) = oneshot::channel();
        let gate = Self {
            signal: Arc::new(Mutex::new(Some(tx))),
            cleaned: Arc::new(AtomicBool::new(false)),
        };
        (gate, rx)
    }

    /// 请求停机。重复调用无副作用。
    fn signal(&self) {
        let taken = self
            .signal
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        if let Some(tx) = taken {
            let _ = tx.send(());
        }
    }

    fn is_cleaned(&self) -> bool {
        self.cleaned.load(Ordering::SeqCst)
    }

    fn mark_cleaned(&self) {
        self.cleaned.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug, Default)]
struct DesktopArgs {
    host: CliArgs,
    width: Option<f64>,
    height: Option<f64>,
}

fn parse_args(args: impl Iterator<Item = String>) -> Result<DesktopArgs, String> {
    let mut parsed = DesktopArgs::default();
    let mut service: Vec<String> = Vec::new();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--width" => {
                let value = args.next().ok_or("missing value for --width")?;
                parsed.width = Some(value.parse().map_err(|_| "--width must be a number")?);
            }
            "--height" => {
                let value = args.next().ok_or("missing value for --height")?;
                parsed.height = Some(value.parse().map_err(|_| "--height must be a number")?);
            }
            // 服务参数原样透传,由 CliArgs 统一解析(错误信息也只有一份)。
            other => service.push(other.to_string()),
        }
    }
    parsed.host = CliArgs::parse(service.into_iter())?;
    Ok(parsed)
}
