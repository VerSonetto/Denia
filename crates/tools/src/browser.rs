//! `browser` 工具:给模型的浏览器控制面(基于 **Playwright**)。
//!
//! 工具本体只做参数解析 + 转发到 [`BrowserHub`];截图以视觉输入回注会话
//! (与 `read_file` 图片注入同一条路)。
//!
//! 底层是微软 Playwright 驱动的**真实浏览器**(headless 无窗口后台运行,
//! 画面走控制台侧边栏),元素引用走官方 aria snapshot 的 `[ref=eN]` + `aria-ref` 引擎。

use std::sync::Arc;

use async_trait::async_trait;
use denia_browser::{BrowserCommand, CommandOutcome};
use denia_core::message::ImageData;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolOutput, parse_args_lenient};

const TOOL_DESCRIPTION: &str = r#"控制内嵌浏览器(Playwright 驱动 Chromium;无窗口后台运行,用户可在控制台浏览器侧栏实时看到画面)。命令面:
- 导航:navigate{url}(仅 http/https)、back{}、forward{}、reload{}
- 读取:getState{}(浏览器没跑时会按需启动)、snapshot{maxElements?, includeHidden?}(返回可访问性树,每个可交互元素带 `[ref=eN]` 引用;后续交互用该引用)、screenshot{fullPage?, clip?}(返回截图,图片自动注入会话)、elementInfo{x,y}
- 交互:click{ref?|locator?|x,y, button?, doubleClick?}、fill{ref, value}(整体替换输入框内容)、type{ref?, text}(追加输入)、press{key, ref?}、scroll{ref?|x,y, deltaX?, deltaY?}(滚动,缺省向下 360px)、hover{ref?|x,y}、select{ref, values[]}、check{ref, checked?}、drag{fromRef,toRef}
- 等待:waitFor{selector?|text?|textGone?, timeoutMs?}
- 弹窗:getDialog{}、handleDialog{accept, promptText?}(页面 alert/confirm/prompt 会挂起等待处理)
- 执行:evaluate{expression}(页面 JS,返回 JSON)
- tab:newTab{url?}、list{}(未运行时返回空 tabs,不拉起实例)、activate{tabId}、close{tabId?}(缺省 tabId = 当前 tab)
- 视口:viewportSet{width,height}、viewportReset{}
- 网络抓包:networkList{urlFilter?, max?}(该 tab 捕获的请求:URL/方法/资源类型/请求体,环形缓冲约 200 条)、networkGetBody{requestId, bodyKind?}(bodyKind 缺省 "response" 取响应体;传 "request" 取 POST 请求体)
- 可视化模式:任意命令可带 visualMode: true,展开控制台浏览器侧栏让用户实时看到画面;默认不开

元素引用约定:snapshot 产出的 `[ref=eN]` 直接传 `e6`(也接受 `@e6`);ref 只在最近一次 snapshot 之后有效,页面导航/重渲染后需重新 snapshot。也可以直接传 CSS 选择器或 Playwright 定位器(`#id`、`[aria-label=...]`、`text=登录`、`getByRole('button')`)。
启动语义:交互命令会按需拉起浏览器;list/close 等查询与善后命令在浏览器没跑时原地返回。
稳定性:元素可操作性等待(可见/可命中/稳定)由 Playwright 负责,过期引用会明确报错而不是静默点错。
典型流程:snapshot 拿 ref → 用 ref 交互 → 需要视觉时 screenshot。分析页面接口先 networkList。"#;

#[derive(Deserialize)]
#[allow(dead_code)]
struct BrowserArgs {
    /// 可视化模式:让用户在浏览器侧栏实时看到浏览器操作(控制台展开判定用,
    /// 不参与命令本身;由工具层单独取出)。
    #[serde(default)]
    visual_mode: bool,
    #[serde(flatten)]
    command: BrowserCommand,
}

/// 浏览器中枢句柄(server 注入;REST API 与工具共用同一实例)。
pub type BrowserHub = Arc<dyn BrowserExecute + Send + Sync>;

#[async_trait]
pub trait BrowserExecute {
    async fn execute(&self, command: BrowserCommand) -> CommandOutcome;

    /// 请求控制台进入可视化模式(展开浏览器侧栏)。默认空实现:
    /// 不支持侧栏的部署静默忽略,不阻断命令。
    async fn request_visual_mode(&self) {}
}

pub struct BrowserTool {
    schema: ToolSchema,
    hub: BrowserHub,
}

impl BrowserTool {
    pub fn new(hub: BrowserHub) -> Self {
        Self {
            schema: ToolSchema {
                name: "browser".to_string(),
                description: TOOL_DESCRIPTION.to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "description": "浏览器命令;method 决定其余字段",
                    "properties": {
                        "method": {
                            "type": "string",
                            "enum": [
                                "navigate", "back", "forward", "reload", "getState",
                                "snapshot", "screenshot", "click", "fill", "type", "press",
                                "scroll", "hover", "select", "check", "drag",
                                "elementInfo", "evaluate", "waitFor",
                                "getDialog", "handleDialog",
                                "newTab", "list", "activate", "close",
                                "viewportSet", "viewportReset",
                                "networkList", "networkGetBody"
                            ],
                            "description": "命令名"
                        },
                        "url": { "type": "string", "description": "navigate/newTab:目标地址(仅 http/https)" },
                        "tabId": { "type": "string", "description": "目标 tab;缺省 = 当前 tab" },
                        "ref": { "type": "string", "description": "snapshot 给出的元素编号(如 e12);也接受 CSS 选择器/locator/text:文本" },
                        "x": { "type": "number" }, "y": { "type": "number" },
                        "deltaX": { "type": "number", "description": "scroll:横向滚动增量(像素;缺省 0)" },
                        "deltaY": { "type": "number", "description": "scroll:纵向滚动增量(像素;缺省 360)" },
                        "width": { "type": "integer" }, "height": { "type": "integer" },
                        "value": { "type": "string", "description": "fill:输入内容" },
                        "text": { "type": "string", "description": "type:文本;handleDialog:prompt 回答" },
                        "key": { "type": "string", "description": "press:键名(Enter/Tab/ArrowDown/a…)" },
                        "button": { "type": "string", "enum": ["left", "right", "middle"] },
                        "doubleClick": { "type": "boolean" },
                        "values": { "type": "array", "items": { "type": "string" }, "description": "select:选中值" },
                        "checked": { "type": "boolean", "description": "check:缺省取反" },
                        "fullPage": { "type": "boolean" },
                        "clip": {
                            "type": "object",
                            "properties": {
                                "x": { "type": "number" }, "y": { "type": "number" },
                                "width": { "type": "number" }, "height": { "type": "number" }
                            }
                        },
                        "maxElements": { "type": "integer" },
                        "includeHidden": { "type": "boolean" },
                        "expression": { "type": "string", "description": "evaluate:页面 JS 表达式" },
                        "selector": { "type": "string" },
                        "textGone": { "type": "string", "description": "waitFor:等到该文本消失" },
                        "timeoutMs": { "type": "integer" },
                        "accept": { "type": "boolean", "description": "handleDialog:接受还是取消" },
                        "fromRef": { "type": "string" }, "toRef": { "type": "string" },
                        "from": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } } },
                        "to": { "type": "object", "properties": { "x": { "type": "number" }, "y": { "type": "number" } } },
                        "urlFilter": { "type": "string", "description": "networkList:按 URL 子串过滤" },
                        "max": { "type": "integer", "description": "networkList:最多返回条数(默认 50,上限 200)" },
                        "requestId": { "type": "string", "description": "networkGetBody:networkList 给出的请求 ID" },
                        "bodyKind": { "type": "string", "enum": ["response", "request"], "description": "networkGetBody:缺省取响应体;request 取 POST 请求体" },
                        "visualMode": { "type": "boolean", "description": "可视化模式:展开浏览器侧栏,让用户实时看到浏览器画面与操作;缺省 false" }
                    },
                    "required": ["method"]
                }),
            },
            hub,
        }
    }
}

/// 把 CommandOutcome 渲染成模型可见文本;截图同时注入视觉输入。
fn render_outcome(outcome: CommandOutcome, ctx: &ToolContext, screenshot: bool) -> ToolOutput {
    let mut text = if outcome.ok {
        "ok".to_string()
    } else {
        let error = outcome.error.as_ref();
        format!(
            "error[{}]: {}",
            error.map(|e| e.code.as_str()).unwrap_or("unknown"),
            error.map(|e| e.message.as_str()).unwrap_or("")
        )
    };
    if let Some(state) = &outcome.state {
        text.push_str(&format!("\nstate: {state}"));
    }
    if let Some(value) = &outcome.value {
        if !value.is_null() {
            text.push_str(&format!("\nvalue: {value}"));
        }
    }
    if let Some(snapshot) = &outcome.snapshot {
        text.push_str(&format!("\nsnapshot: {snapshot}"));
    }
    if let Some(tabs) = &outcome.tabs {
        text.push_str(&format!("\ntabs: {tabs}"));
    }
    if let Some(dialog) = &outcome.dialog {
        let rendered = if dialog.is_null() {
            "\ndialog: none".to_string()
        } else {
            format!("\ndialog: {dialog}")
        };
        text.push_str(&rendered);
    }
    // 截图 → 视觉注入(read_file 同款约定)。
    if screenshot && outcome.ok {
        if let Some(image) = &outcome.image {
            if let Ok(data) =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &image.base64)
            {
                if ctx.vision_supported {
                    if let Some(sink) = &ctx.emit_event {
                        sink(denia_core::session::SessionEvent::UserMessage {
                            text: "[harness] 浏览器截图已注入为视觉输入".to_string(),
                            injected: true,
                            images: vec![ImageData {
                                mime: image.mime_type.clone(),
                                data: image.base64.clone(),
                            }],
                            channel: None,
                        });
                    }
                    text.push_str("\n(截图已注入会话,模型可直接看到)");
                } else {
                    text.push_str("\n(当前模型不支持视觉,截图未注入)");
                }
                let _ = data;
            }
        }
    }
    ToolOutput {
        content: text,
        is_error: !outcome.ok,
    }
}

#[async_trait]
impl Tool for BrowserTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        // 宽容解析:method 必须有;其余字段按命令收集(先拿原始 JSON 再喂 serde)。
        let raw: serde_json::Value = match parse_args_lenient(arguments) {
            Ok(value) => value,
            Err(error) => {
                return ToolOutput {
                    content: format!("invalid arguments: {error}"),
                    is_error: true,
                };
            }
        };
        let command: BrowserCommand = match serde_json::from_value(raw.clone()) {
            Ok(command) => command,
            Err(error) => {
                return ToolOutput {
                    content: format!(
                        "invalid browser command: {error};命令面:navigate/back/forward/reload/getState/snapshot/screenshot/click/fill/type/press/scroll/hover/select/check/drag/elementInfo/evaluate/waitFor/getDialog/handleDialog/newTab/list/activate/close/viewportSet/viewportReset"
                    ),
                    is_error: true,
                };
            }
        };
        let _ = BrowserArgs::deserialize(&raw); // flatten 解析仅用于校验,失败不阻断
        // 可视化模式是顶层标志:在命令执行前请求控制台展开侧栏,
        // 用户看到的首个画面就是本次命令的结果(而非命令结束后才出现)。
        let visual_mode = raw
            .get("visualMode")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if visual_mode {
            self.hub.request_visual_mode().await;
        }
        let wants_screenshot = command_is_screenshot(&command);
        let outcome = self.hub.execute(command).await;
        render_outcome(outcome, ctx, wants_screenshot)
    }
}

fn command_is_screenshot(command: &BrowserCommand) -> bool {
    matches!(command, BrowserCommand::Screenshot { .. })
}

/// 桥接:`denia_browser::BrowserManager` 实现 `BrowserExecute`
/// (孤儿规则:本 crate 同时见过 trait 与类型)。
#[async_trait]
impl BrowserExecute for denia_browser::BrowserManager {
    async fn execute(&self, command: BrowserCommand) -> CommandOutcome {
        denia_browser::BrowserManager::execute(self, command).await
    }

    async fn request_visual_mode(&self) {
        denia_browser::BrowserManager::request_visual_mode(self);
    }
}
