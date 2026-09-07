//! `browser` 工具:给模型的内嵌浏览器控制面(与 ZCode Browser Use 命令面一致)。
//!
//! 工具本体只做参数解析 + 转发到 [`BrowserHub`];截图以视觉输入回注会话
//! (与 `read_file` 图片注入同一条路)。

use std::sync::Arc;

use async_trait::async_trait;
use denia_browser::{BrowserCommand, CommandOutcome};
use denia_core::message::ImageData;
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::{Tool, ToolContext, ToolOutput, parse_args_lenient};

const TOOL_DESCRIPTION: &str = r#"控制内嵌浏览器(headless Chrome),可导航、读取页面、交互操作。ZCode 同款命令面:
- 导航:navigate{url}(仅 http/https)、back{}、forward{}、reload{}
- 读取:getState{}、snapshot{maxElements?, includeHidden?}(返回可交互元素列表,每个带 ref 编号与稳定定位器 locator/selector;后续交互用 ref 或 locator)、screenshot{fullPage?, clip?}(返回截图,图片自动注入会话)、elementInfo{x,y}
- 交互:click{ref?|locator?|x,y, button?, doubleClick?}、fill{ref|locator, value}(整体替换输入框内容)、type{ref?|locator?, text}(追加粘贴)、press{key, ref?}、scroll{ref?|x,y, deltaX?, deltaY?}(滚动,缺省向下 360px) 、hover{ref?|x,y}、select{ref, values[]}、check{ref, checked?}、drag{fromRef,toRef}。ref 和 locator 也可直接放 ref 字段:CSS 选择器、#id、[aria-label=...]、text:文本
- 等待:waitFor{selector?|text?|textGone?, timeoutMs?}
- 弹窗:getDialog{}、handleDialog{accept, promptText?}(页面 alert/confirm/prompt 会挂起等待处理)
- 执行:evaluate{expression}(页面 JS,返回 JSON)
- tab:newTab{url?}、list{}、activate{tabId}、close{tabId?}(缺省 tabId = 当前 tab)
- 视口:viewportSet{width,height}、viewportReset{}
- 网络抓包:networkList{urlFilter?, max?}(该 tab 捕获的请求:URL/方法/状态码/请求响应头/postData;环形缓冲约 200 条)、networkGetBody{requestId, bodyKind?}(bodyKind 缺省 "response" 取响应体;传 "request" 取 POST 请求体;文本直出,二进制返回 base64 标记)
稳定性保证:
- 元素失效自动重定位:ref 过期会按 locator/selector/xpath/文本 回退链自动重绑,并在 1.2s 窗口内自动刷新快照重试,无需重复 snapshot
- 统一超时:命令总耗时 30s(waitFor 可显式更长,上限 60s),CDP 单请求 30s
- 断线自动重连:浏览器掉线自动重启实例并恢复活跃 tab(最多重试 3 次);关闭最后一个 tab 会彻底回收 Chrome 进程与资源
典型流程:snapshot 拿 ref → 用 ref/locator 交互 → 需要视觉时 screenshot。要分析页面背后的 API 调用时先 networkList。"#;

#[derive(Deserialize)]
#[allow(dead_code)]
struct BrowserArgs {
    #[serde(flatten)]
    command: BrowserCommand,
}

/// 浏览器中枢句柄(server 注入;REST API 与工具共用同一实例)。
pub type BrowserHub = Arc<dyn BrowserExecute + Send + Sync>;

#[async_trait]
pub trait BrowserExecute {
    async fn execute(&self, command: BrowserCommand) -> CommandOutcome;
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
                        "bodyKind": { "type": "string", "enum": ["response", "request"], "description": "networkGetBody:缺省取响应体;request 取 POST 请求体" }
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
}
