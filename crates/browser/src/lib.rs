//! 内嵌浏览器:通过 **chromiumoxide**(纯 Rust CDP 客户端)驱动本机
//! Chrome/Edge。
//!
//! # 架构
//!
//! ```text
//! denia ──▶ chromiumoxide(CDP over WebSocket)
//!              └─▶ 系统 Chrome/Edge(零下载、零打包,发布版自包含)
//! ```
//!
//! 与旧实现(playwright-rs + 官方 Node driver)的区别:
//! - **不再依赖 Node/Playwright driver**:旧实现把 driver 目录的构建机
//!   绝对路径编译期钉进 exe,CI 构建的发布版在用户机器上找不到该路径;
//!   现在直接用系统浏览器,发布包自包含;
//! - **元素引用自维护**:`snapshot` 基于无障碍树产出 `[ref=eN]`,交互命令
//!   按 ref 查 backendNodeId,再用 CDP 原语(DOM/Input)定位与操作,
//!   不往页面注入全局变量(见 elements.rs);
//! - **无窗口后台运行**:headless 启动,不弹浏览器窗口;
//!   画面通过控制台侧边栏的 screencast 实时展示;
//! - **进程必须回收**:浏览器实例占住 profile 的单例锁,残留会让下一次启动
//!   以 `ExitStatus(21)` 失败。回收有三层:最后一个 tab 关闭即自动收、
//!   闲置超时看护、模型显式 `shutdownBrowser`;另有启动前清扫孤儿进程兜底
//!   (见 [`procctl`])。

pub mod driver;
mod elements;
pub mod manager;
pub mod network;
mod procctl;

pub use manager::{BrowserEvent, BrowserManager, FrameViewport, TabInfo};

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// 模型可用的浏览器命令(与工具层 schema 一一对应)。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(
    tag = "method",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum BrowserCommand {
    Navigate {
        url: String,
        tab_id: Option<String>,
    },
    Back {
        tab_id: Option<String>,
    },
    Forward {
        tab_id: Option<String>,
    },
    Reload {
        tab_id: Option<String>,
    },
    GetState {
        tab_id: Option<String>,
    },
    Snapshot {
        tab_id: Option<String>,
        max_elements: Option<u32>,
        include_hidden: Option<bool>,
    },
    Screenshot {
        tab_id: Option<String>,
        full_page: Option<bool>,
        #[serde(rename = "clip")]
        clip: Option<ClipRect>,
    },
    Click {
        tab_id: Option<String>,
        r#ref: Option<String>,
        /// 稳定定位器:CSS 选择器、#id、[aria-label=...] 或文本匹配。
        locator: Option<String>,
        x: Option<f64>,
        y: Option<f64>,
        button: Option<String>,
        double_click: Option<bool>,
    },
    Fill {
        tab_id: Option<String>,
        r#ref: String,
        value: String,
    },
    Type {
        tab_id: Option<String>,
        r#ref: Option<String>,
        text: String,
    },
    Press {
        tab_id: Option<String>,
        key: String,
        r#ref: Option<String>,
    },
    Scroll {
        tab_id: Option<String>,
        r#ref: Option<String>,
        x: Option<f64>,
        y: Option<f64>,
        /// 面板滚轮:自定义滚动增量(缺省 360px 一格)。
        delta_x: Option<f64>,
        delta_y: Option<f64>,
    },
    Hover {
        tab_id: Option<String>,
        r#ref: Option<String>,
        x: Option<f64>,
        y: Option<f64>,
    },
    Select {
        tab_id: Option<String>,
        r#ref: String,
        values: Vec<String>,
    },
    Check {
        tab_id: Option<String>,
        r#ref: String,
        checked: Option<bool>,
    },
    Drag {
        tab_id: Option<String>,
        from_ref: Option<String>,
        to_ref: Option<String>,
        from: Option<Point>,
        to: Option<Point>,
    },
    ElementInfo {
        tab_id: Option<String>,
        x: f64,
        y: f64,
    },
    Evaluate {
        tab_id: Option<String>,
        expression: String,
    },
    WaitFor {
        tab_id: Option<String>,
        selector: Option<String>,
        text: Option<String>,
        text_gone: Option<String>,
        timeout_ms: Option<u64>,
    },
    GetDialog {
        tab_id: Option<String>,
    },
    HandleDialog {
        tab_id: Option<String>,
        accept: bool,
        prompt_text: Option<String>,
    },
    NewTab {
        url: Option<String>,
    },
    List,
    Close {
        tab_id: Option<String>,
    },
    Activate {
        tab_id: String,
    },
    ViewportSet {
        tab_id: Option<String>,
        width: u32,
        height: u32,
    },
    ViewportReset {
        tab_id: Option<String>,
    },
    /// 网络抓包:列出该 tab 捕获的请求(环形缓冲)。
    NetworkList {
        tab_id: Option<String>,
        url_filter: Option<String>,
        max: Option<u32>,
    },
    /// 网络抓包:取某请求的响应体(文本或 base64 标记)。
    NetworkGetBody {
        tab_id: Option<String>,
        #[serde(alias = "requestId")]
        request_id: String,
        /// "response"(默认)取响应体;"request" 取请求体 postData。
        #[serde(default)]
        body_kind: Option<String>,
    },
    /// 面板实时画面:开启当前 tab 的 screencast(仅控制台用)。
    StartScreencast {
        tab_id: Option<String>,
    },
    /// 停止当前 tab 的 screencast。
    StopScreencast {
        tab_id: Option<String>,
    },
    /// 回收浏览器进程:关掉所有 tab 并结束 Chrome/Edge 进程树,释放 profile 锁。
    /// 与 `close`(只关标签页)相对 —— 任务收尾必须走这一条。
    ShutdownBrowser,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct ClipRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

/// 统一命令结果。
#[derive(Debug, Clone, Serialize)]
pub struct CommandOutcome {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CommandError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<ImagePayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tabs: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dialog: Option<serde_json::Value>,
    pub elapsed_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CommandError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImagePayload {
    pub base64: String,
    #[serde(rename = "mimeType")]
    pub mime_type: String,
}

impl CommandOutcome {
    pub fn ok_value(value: serde_json::Value, elapsed_ms: u64) -> Self {
        Self {
            ok: true,
            error: None,
            value: Some(value),
            state: None,
            image: None,
            snapshot: None,
            tabs: None,
            dialog: None,
            elapsed_ms,
        }
    }

    pub fn err(code: &str, message: impl Into<String>, elapsed_ms: u64) -> Self {
        Self {
            ok: false,
            error: Some(CommandError {
                code: code.to_string(),
                message: message.into(),
            }),
            value: None,
            state: None,
            image: None,
            snapshot: None,
            tabs: None,
            dialog: None,
            elapsed_ms,
        }
    }
}

impl BrowserCommand {
    /// 返回命令的显式目标 tab(无 tab 字段或未给时返回 None)。
    pub fn tab_id_probe(&self) -> Option<&str> {
        match self {
            BrowserCommand::Navigate { tab_id, .. }
            | BrowserCommand::Back { tab_id }
            | BrowserCommand::Forward { tab_id }
            | BrowserCommand::Reload { tab_id }
            | BrowserCommand::GetState { tab_id }
            | BrowserCommand::Snapshot { tab_id, .. }
            | BrowserCommand::Screenshot { tab_id, .. }
            | BrowserCommand::Click { tab_id, .. }
            | BrowserCommand::Fill { tab_id, .. }
            | BrowserCommand::Type { tab_id, .. }
            | BrowserCommand::Press { tab_id, .. }
            | BrowserCommand::Scroll { tab_id, .. }
            | BrowserCommand::Hover { tab_id, .. }
            | BrowserCommand::Select { tab_id, .. }
            | BrowserCommand::Check { tab_id, .. }
            | BrowserCommand::Drag { tab_id, .. }
            | BrowserCommand::ElementInfo { tab_id, .. }
            | BrowserCommand::Evaluate { tab_id, .. }
            | BrowserCommand::WaitFor { tab_id, .. }
            | BrowserCommand::GetDialog { tab_id }
            | BrowserCommand::HandleDialog { tab_id, .. }
            | BrowserCommand::Close { tab_id }
            | BrowserCommand::ViewportSet { tab_id, .. }
            | BrowserCommand::ViewportReset { tab_id }
            | BrowserCommand::NetworkList { tab_id, .. }
            | BrowserCommand::NetworkGetBody { tab_id, .. }
            | BrowserCommand::StartScreencast { tab_id }
            | BrowserCommand::StopScreencast { tab_id } => tab_id.as_deref(),

            BrowserCommand::NewTab { .. }
            | BrowserCommand::List
            | BrowserCommand::ShutdownBrowser
            | BrowserCommand::Activate { .. } => None,
        }
    }
}

/// 浏览器中枢:server 持有,工具与 REST API 共用。
pub type SharedBrowser = Arc<manager::BrowserManager>;

/// 默认 profile 目录:$DENIA_HOME/browser/profile(持久,登录态跨重启存活)。
pub fn default_profile_dir(home: &std::path::Path) -> PathBuf {
    home.join("browser").join("profile")
}

/// 该 profile 是否仍被一个活着的浏览器进程占用(只读:不回收、不抹记录)。
///
/// 「收尾收干净没有」只能这么问:控制面状态不算(进程可以活在控制面之外),
/// 光翻进程列表又不区分是不是本实例拉起的。诊断接口与测试共用这一口径。
pub fn browser_profile_held(profile_dir: &std::path::Path) -> bool {
    procctl::held(profile_dir)
}
