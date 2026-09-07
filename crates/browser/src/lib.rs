//! 内嵌浏览器:拉起 Chrome/Edge + CDP 驱动,命令面与 ZCode "Browser Use" 对齐。
//!
//! 架构对照(见 D:\ZCode\analysis\browser-use\REPORT.md):
//! - ZCode 用 Electron `<webview>` + `webContents.debugger`(CDP 1.3);
//!   这里用真实 Chrome/Edge(headless=new,持久 profile)+ 自写 CDP 客户端。
//! - 命令面、快照脚本、ref 机制、虚拟剪贴板、截图 CSS 像素校正全部移植。

pub mod cdp;
pub mod commands;
pub mod launch;
pub mod manager;
pub mod recon;
pub mod scripts;

pub use manager::{BrowserEvent, BrowserManager};

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// 一条模型/控制台发来的浏览器命令(与 ZCode `Wo` discriminated union 同形)。
///
/// `rename_all_fields` 把变体字段统一转 camelCase,与前端的 tabId/fromRef/
/// toRef/doubleClick 等命名对齐(否则 activate 的 tab_id 字段无法反序列化,
/// 面板切 tab 会 400)。
#[derive(Debug, Clone, Deserialize)]
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
        /// 稳定定位器：CSS 选择器、#id、[aria-label=...] 或文本匹配。
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
    /// 网络抓包:列出该 tab 捕获的请求(DevTools Network 等价;环形缓冲)。
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
    /// 面板实时画面:开启当前 tab 的 screencast(仅控制台用,模型命令面不含)。
    StartScreencast {
        tab_id: Option<String>,
    },
    /// 停止当前 tab 的 screencast。
    StopScreencast {
        tab_id: Option<String>,
    },
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

/// 统一命令结果(ZCode 结果形状:ok/error{code,message}/value)。
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
    /// 返回命令的显式目标 tab(无 tab 字段或未给时返回 None;execute 层用于
    /// 断点暂停自动恢复的判定)。
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
            | BrowserCommand::Activate { .. } => None,
        }
    }
}

/// 浏览器中枢:server 持有,工具与 REST API 共用。
pub type SharedBrowser = Arc<manager::BrowserManager>;

/// 默认 profile 目录:$DENIA_HOME/browser/profile(持久,等价 persist 分区)。
pub fn default_profile_dir(home: &std::path::Path) -> PathBuf {
    home.join("browser").join("profile")
}
