//! 浏览器驱动:探测本机 Chromium 系浏览器(Chrome/Edge)、启动实例、
//! 建立 CDP 连接并看护生命周期。
//!
//! # 架构
//!
//! ```text
//! denia ──▶ chromiumoxide(CDP over WebSocket,纯 Rust)
//!              └─▶ 系统 Chrome/Edge(零下载、零打包,发布版自包含)
//! ```
//!
//! 与旧实现(playwright-rs + 官方 Node driver)的区别:
//! - 不再依赖 Node/Playwright driver:旧实现把 driver 目录的**构建机绝对
//!   路径**编译期钉进 exe,CI 构建出的发布版在用户机器上找不到该路径,
//!   浏览器工具直接不可用;现在 exe 只依赖系统浏览器;
//! - 不再需要首次下载数百 MB 的 Playwright 浏览器;
//! - 进程与 CDP 连接由本模块照看:连接断开(浏览器退出/被杀)即触发退出
//!   信号,上层清理会话并按需重新拉起;
//! - [`shutdown`] 提供主动回收:优雅退出 → 超时强杀进程树,与「只关标签页」
//!   的 `Page::close` 区分开。

use std::path::{Path, PathBuf};
use std::time::Duration;

use chromiumoxide::Handler;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::handler::viewport::Viewport;
use futures::StreamExt;

use crate::procctl;

/// 一个已就绪的浏览器会话。
pub struct Session {
    /// chromiumoxide 浏览器句柄(命令入口;进程由其托管,drop 时兜底 kill)。
    pub browser: Browser,
    /// 本实例代次:看护与清理只认这一代。
    pub generation: u64,
    /// 本进程拉起的浏览器主进程 pid(用于优雅回收时按进程树收干净)。
    pub pid: Option<u32>,
    /// 本实例用的浏览器可执行文件路径:回收子孙进程时按镜像名做护栏。
    pub exe: PathBuf,
    /// profile 归属令牌持有者:只要它在,本实例就被记为「正在用这个 profile」。
    /// 会话 drop 时随之释放记录。
    _token: procctl::ProfileToken,
}

impl Session {
    /// 保留 profile 归属记录并把令牌从结构体里摘掉(不走 Drop 的删除分支)。
    ///
    /// 只用于「强杀也没能把进程收干净」的异常路径:记录留着,下次冷启动的
    /// [`procctl::reap_stale`] 才能按 PID + 身份再试一次。
    pub fn keep_profile_record(&mut self) {
        let mut detached = procctl::ProfileToken::detached();
        std::mem::swap(&mut self._token, &mut detached);
        detached.leak();
    }

    /// 主动释放 profile 归属令牌(进程已收干净时用)。
    pub fn release_profile_token(&mut self) {
        self._token = procctl::ProfileToken::detached();
    }
}

/// 会话退出信号:浏览器进程退出/连接断开时触发。
pub type SessionExit = tokio::sync::oneshot::Receiver<()>;

/// 启动参数。
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// profile 目录(持久化,存 cookie/localStorage)。
    pub profile_dir: PathBuf,
    /// 产物目录(下载等,预留)。
    pub artifacts_dir: PathBuf,
    /// 是否无头。denia 固定无窗口(headless)后台运行,
    /// 用户画面走控制台面板的 screencast,不弹浏览器窗口。
    pub headless: bool,
    /// 视口尺寸。
    pub viewport: (u32, u32),
    /// 附加 Chromium 启动参数。
    pub args: Vec<String>,
}

impl LaunchSpec {
    pub fn new(home: &Path) -> Self {
        let browser_home = home.join("browser");
        Self {
            profile_dir: browser_home.join("profile"),
            artifacts_dir: browser_home.join("artifacts"),
            headless: true,
            viewport: (1440, 900),
            args: Vec::new(),
        }
    }
}

/// 拉起浏览器并建立 CDP 连接,返回会话与退出信号。
pub async fn launch(spec: &LaunchSpec, generation: u64) -> Result<(Session, SessionExit), String> {
    std::fs::create_dir_all(&spec.profile_dir)
        .map_err(|error| format!("创建浏览器 profile 目录失败: {error}"))?;
    std::fs::create_dir_all(&spec.artifacts_dir)
        .map_err(|error| format!("创建浏览器产物目录失败: {error}"))?;

    let executable = detect_browser_executable()?;
    tracing::info!("使用本机浏览器: {}", executable.display());

    let viewport = Viewport {
        width: spec.viewport.0,
        height: spec.viewport.1,
        device_scale_factor: None,
        emulating_mobile: false,
        is_landscape: false,
        has_touch: false,
    };

    // 冷启动前先回收上一代没退干净的残留:Chrome 按 user-data-dir 上单例锁,
    // 残留进程会让本次 launch 以 `ExitStatus(21)` 这种看不出所以然的形态失败。
    // 只有「真的被别的实例占着」才中止 —— 否则宁可扫干净再启。
    match procctl::reap_stale(&spec.profile_dir).await {
        Ok(procctl::Reaped::ProcessTree) => {
            tracing::info!("已回收上次残留的浏览器进程,本次为干净冷启动")
        }
        Ok(_) => {}
        Err(conflict) => return Err(conflict),
    }

    let mut builder = BrowserConfig::builder()
        .chrome_executable(&executable)
        .user_data_dir(&spec.profile_dir)
        .window_size(spec.viewport.0, spec.viewport.1)
        .viewport(Some(viewport))
        .arg("--no-first-run")
        .arg("--no-default-browser-check");
    builder = if spec.headless {
        builder.new_headless_mode()
    } else {
        builder.with_head()
    };
    for arg in &spec.args {
        builder = builder.arg(arg.clone());
    }
    let config = builder.build()?;

    let (mut browser, handler) = Browser::launch(config)
        .await
        .map_err(|error| format!("启动浏览器失败({}): {error}", executable.display()))?;

    // 先取 pid 再登记归属:令牌内容就是这份 pid 清单,它同时是「本实例占用
    // profile」的凭据,也是回收时判断哪些进程算我们的依据。
    let pid = browser
        .get_mut_child()
        .and_then(|child| child.as_mut_inner().id());
    let token = procctl::claim(&spec.profile_dir, pid, &executable);

    let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(drive_handler(handler, exit_tx));

    Ok((
        Session {
            browser,
            generation,
            pid,
            exe: executable,
            _token: token,
        },
        exit_rx,
    ))
}

/// 回收结果:`graceful` = 浏览器自己退的(未动刀);`gone` = 进程确定消失。
#[derive(Debug, Clone, Copy)]
pub struct ShutdownOutcome {
    pub graceful: bool,
    pub gone: bool,
}

/// 优雅回收一个会话:关掉所有页面 → 请求浏览器退出 → 等 → 超时才强杀。
///
/// 顺序按「先礼后兵」:CDP `Browser.close` 让 Chrome 自己走完清理(落盘
/// preferences、释放 profile 单例锁),避免反复强杀把 profile 写坏;强杀走
/// 进程树是因为 Chrome 会 fork 出 GPU/network/utility 等子进程,主进程死了
/// 它们未必跟着退。
pub async fn shutdown(session: &mut Session) -> ShutdownOutcome {
    // 页面先收:让 targetDestroyed 之类的清理链路正常走完。
    let pages: Vec<_> = session
        .browser
        .pages()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|page| async move {
            let _ = page.close().await;
        })
        .collect();
    futures::future::join_all(pages).await;

    let _ = session.browser.close().await;

    // 等浏览器自己退:给足 3s(落盘 preferences 需要一点时间)。
    let grace = Duration::from_secs(3);
    if tokio::time::timeout(grace, wait_exit(&mut session.browser))
        .await
        .unwrap_or(false)
    {
        tracing::info!("浏览器已优雅退出");
        return ShutdownOutcome {
            graceful: true,
            gone: true,
        };
    }

    tracing::info!(
        "浏览器未在 {}s 内退出,按进程树强制回收(pid={:?})",
        grace.as_secs(),
        session.pid
    );
    // 先 kill 直接子进程(tokio 持有句柄,能拿到退出状态),再按进程树扫子孙
    // 兜底:主进程已死时 taskkill /T 找不到树,靠创建者 PID 清单补收。
    if let Some(child) = session.browser.get_mut_child() {
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
    let gone = procctl::reap_own(session.pid, Some(&session.exe)).await;
    ShutdownOutcome {
        graceful: false,
        gone,
    }
}

/// 轮询等浏览器进程退出(chromiumoxide 的 `try_wait` 会把退出码收走,不留僵尸)。
async fn wait_exit(browser: &mut Browser) -> bool {
    loop {
        match browser.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) => tokio::time::sleep(Duration::from_millis(100)).await,
            // try_wait 报错按「已不可用」处理,交给上层强杀兜底。
            Err(_) => return false,
        }
    }
}

/// 持续驱动 CDP 事件循环。
///
/// chromiumoxide 的命令/事件都依赖 handler 被持续 poll。连接级错误
/// (socket 断开等)会以 `Err` 冒出,此时提前触发退出信号;浏览器进程
/// 被杀时事件流可能只是静默停摆(不结束),那一路径由上层对
/// `Browser::try_wait` 的轮询兜底(见 manager.rs 的看护任务)。
async fn drive_handler(mut handler: Handler, exit: tokio::sync::oneshot::Sender<()>) {
    loop {
        match handler.next().await {
            Some(Ok(())) => continue,
            Some(Err(error)) => {
                tracing::info!("浏览器连接断开: {error}");
                break;
            }
            None => {
                tracing::info!("浏览器事件循环结束");
                break;
            }
        }
    }
    let _ = exit.send(());
}

/// 探测本机可用的 Chromium 系浏览器可执行文件。
///
/// 顺序:
/// 1. `DENIA_BROWSER_EXECUTABLE`(显式覆盖,测试/特殊环境用);
/// 2. `CHROME`(与 chromiumoxide 探测约定一致);
/// 3. 常见安装路径:Chrome(系统级/用户级)→ Edge(系统级/用户级);
/// 4. chromiumoxide 自带探测(含注册表/PATH)。
pub fn detect_browser_executable() -> Result<PathBuf, String> {
    if let Some(path) = env_executable("DENIA_BROWSER_EXECUTABLE") {
        return Ok(path);
    }
    if let Some(path) = env_executable("CHROME") {
        return Ok(path);
    }
    if let Some(path) = common_install_paths()
        .into_iter()
        .find(|path| path.is_file())
    {
        return Ok(path);
    }
    chromiumoxide::detection::default_executable(chromiumoxide::detection::DetectionOptions {
        msedge: true,
        unstable: false,
    })
    .map_err(|error| {
        format!(
            "未找到可用的 Chrome/Edge 浏览器: {error}。\
             请安装 Chrome 或 Edge,或设置 DENIA_BROWSER_EXECUTABLE 指向浏览器可执行文件"
        )
    })
}

fn env_executable(name: &str) -> Option<PathBuf> {
    let value = std::env::var(name).ok()?;
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    path.is_file().then_some(path)
}

/// 常见安装路径(Windows 优先 Chrome,再 Edge;其它平台交给 chromiumoxide 探测)。
fn common_install_paths() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut paths = Vec::new();
        let program_files =
            std::env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_string());
        let program_files_x86 = std::env::var("ProgramFiles(x86)")
            .unwrap_or_else(|_| r"C:\Program Files (x86)".to_string());
        let local_app_data = std::env::var("LOCALAPPDATA").unwrap_or_default();
        for path in [
            format!(r"{program_files}\Google\Chrome\Application\chrome.exe"),
            format!(r"{program_files_x86}\Google\Chrome\Application\chrome.exe"),
            format!(r"{local_app_data}\Google\Chrome\Application\chrome.exe"),
            format!(r"{program_files_x86}\Microsoft\Edge\Application\msedge.exe"),
            format!(r"{program_files}\Microsoft\Edge\Application\msedge.exe"),
            format!(r"{local_app_data}\Microsoft\Edge\Application\msedge.exe"),
        ] {
            paths.push(PathBuf::from(path));
        }
        paths
    }
    #[cfg(not(windows))]
    {
        Vec::new()
    }
}
