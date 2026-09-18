//! Playwright 驱动管理:拉起 driver、装浏览器、建持久化上下文。
//!
//! denia 通过 `playwright-rs`(Apache-2.0)使用微软 Playwright:它是
//! **JSON-RPC over stdio 到 Playwright 官方 Node driver** 的绑定,与
//! playwright-python / java / dotnet 同架构。driver 自带捆绑 Node 二进制
//! (见 crate 的 ADR 0006),**不依赖用户环境里的 Node**。
//!
//! 浏览器二进制需与 driver 版本严格匹配:首次启动若缺失,这里按需下载
//! (`install_browsers`),之后由 `ms-playwright` 缓存复用。

use std::path::{Path, PathBuf};
use std::time::Duration;

use playwright_rs::api::LaunchOptions;
use playwright_rs::protocol::{BrowserContext, BrowserContextOptions, Playwright};

/// 首次下载浏览器的日志前缀(前端/日志里便于识别长耗时来源)。
pub const BROWSER_DOWNLOAD_HINT: &str = "首次使用需下载 Playwright 浏览器(约 200MB),请耐心等待";

/// 一个已就绪的浏览器会话。
pub struct Session {
    /// Playwright 句柄(持有 driver 进程生命周期)。
    pub playwright: Playwright,
    /// 持久化上下文(profile 落盘,登录态跨重启存活)。
    pub context: BrowserContext,
    /// 本实例代次:看护任务只收这一代的尸。
    pub generation: u64,
}

/// 启动参数。
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// profile 目录(持久化,存 cookie/localStorage)。
    pub profile_dir: PathBuf,
    /// 产物目录(下载、trace 等)。
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

/// 拉起 Playwright 与浏览器;浏览器缺失时按需下载后重试一次。
pub async fn launch(spec: &LaunchSpec, generation: u64) -> Result<Session, String> {
    let playwright = Playwright::launch()
        .await
        .map_err(|error| format!("启动 Playwright driver 失败: {error}"))?;

    let context = match launch_context(&playwright, spec).await {
        Ok(context) => context,
        Err(first_error) => {
            // 浏览器未安装是首次运行的常态:下载后重试一次。
            if !looks_like_missing_browser(&first_error) {
                return Err(first_error);
            }
            tracing::info!("{BROWSER_DOWNLOAD_HINT}");
            playwright_rs::install_browsers(Some(&["chromium"]))
                .await
                .map_err(|error| format!("下载 Playwright 浏览器失败: {error}"))?;
            launch_context(&playwright, spec)
                .await
                .map_err(|error| format!("浏览器安装后仍无法启动: {error}"))?
        }
    };

    Ok(Session {
        playwright,
        context,
        generation,
    })
}

/// 建持久化上下文(profile 落盘)。
async fn launch_context(
    playwright: &Playwright,
    spec: &LaunchSpec,
) -> Result<BrowserContext, String> {
    std::fs::create_dir_all(&spec.profile_dir)
        .map_err(|error| format!("创建浏览器 profile 目录失败: {error}"))?;
    std::fs::create_dir_all(&spec.artifacts_dir)
        .map_err(|error| format!("创建浏览器产物目录失败: {error}"))?;

    let options = BrowserContextOptions::builder()
        .headless(spec.headless)
        .viewport(playwright_rs::protocol::Viewport {
            width: spec.viewport.0,
            height: spec.viewport.1,
        })
        .accept_downloads(true)
        .build();
    let mut options = options;
    if !spec.args.is_empty() {
        options.args = Some(spec.args.clone());
    }

    playwright
        .chromium()
        .launch_persistent_context_with_options(
            spec.profile_dir.to_string_lossy().to_string(),
            options,
        )
        .await
        .map_err(|error| error.to_string())
}

/// 判断错误是否属于"浏览器二进制未安装"。
fn looks_like_missing_browser(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("executable doesn't exist")
        || lower.contains("browser not installed")
        || lower.contains("browsernotinstalled")
        || lower.contains("please run the following command to download")
}

/// 默认命令超时(秒)。
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// 把 `LaunchOptions` 的常用项暴露给上层(测试/自定义可执行文件)。
pub fn base_launch_options(headless: bool) -> LaunchOptions {
    LaunchOptions::new().headless(headless)
}
