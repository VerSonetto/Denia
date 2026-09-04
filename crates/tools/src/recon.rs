//! `recon` 工具:给模型的 JS 逆向工作台(与面板共享同一 ReconStore)。
//!
//! 模型执行逆向工作流:networkList 找接口 → initiator 定位置 → search 定位源码 →
//! breakpoint 设断点(URL 级,刷新自动恢复)→ 触发页面操作 → paused 查帧栈 →
//! eval 在暂停帧内求值(看入参/局部变量)→ step 单步 → resume。
//!
//! 断点语义:命中后页面冻结。暂停期间**不要**调用 browser 交互命令(会自动恢复
//! 执行、打断调试);检查状态一律用本工具的 paused/eval。调试完毕必须 resume。

use std::sync::Arc;

use async_trait::async_trait;
use denia_browser::BrowserManager;
use denia_core::tool::ToolSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Tool, ToolContext, ToolOutput, parse_args_lenient};

const TOOL_DESCRIPTION: &str = r#"JS 逆向工作台:定位加密/签名函数、断点调试。与浏览器面板的「逆向」区同一数据源,操作互通:
- scripts{urlFilter?}: 列出当前 tab 已解析的脚本(scriptId 做 source 定位;urlFilter 按 URL 子串过滤)
- search{query, isRegex?, caseSensitive?, urlFilter?, maxResults?}: 跨脚本检索源码文本/正则(命中行号 0-based;超 1.5MB 的大脚本自动跳过)
- source{scriptId?|url?, startLine?, endLine?}: 取脚本源码片段(0-based 行;totalLines 给出总行数,大文件按行分片拉)
- breakpoint{query, urlFilter?, occurrence?, condition?}: 文本断点——在检索命中的那一行设断(URL 级,刷新/重载后自动恢复;同位置幂等)。条件是可选 JS 表达式
- breakpoints{}: 列出已设断点;breakpointRemove{breakpointId}: 删除
- paused{}: 当前暂停详情(帧栈 callFrameId/函数名/行号/作用域名);未暂停返回 null
- eval{expression, frameIndex?}: 在指定暂停帧(缺省 0=最顶层)内求值,可读入参、局部变量、this
- step{direction: over|into|out}: 单步前进;resume{}: 恢复执行(断点保留)
- console{max?}: 该 tab 最近的浏览器控制台消息(error/warn/log)
典型逆向流程:
1. networkList 找目标接口 → 看 initiator 调用栈定位发起函数
2. search 在源码里找到该函数/可疑关键字(如 encrypt、sign、aes)
3. breakpoint 在命中行设断点,然后 reload 或页面操作触发
4. paused 查栈 → eval 读该帧的入参/返回值/arguments → step 单步追加密过程
5. 结束后 resume 恢复页面
断点暂停会冻结页面:暂停期间调用其他 browser 交互命令会自动恢复执行(打断调试),请用 eval/step/resume 完成调试;调试完必须 resume。"#;

#[derive(Deserialize)]
#[allow(dead_code)]
struct ReconArgs {
    method: String,
    #[serde(default)]
    tab_id: Option<String>,
}

/// 逆向中枢句柄(server 注入;与 BrowserHub 同源同一 BrowserManager)。
pub type ReconHub = Arc<dyn ReconExecute + Send + Sync>;

#[async_trait]
pub trait ReconExecute {
    /// 执行一条逆向操作;返回 JSON 结果,错误为 `Err(message)`。
    async fn execute_recon(&self, method: &str, args: Value) -> Result<Value, String>;
}

pub struct ReconTool {
    schema: ToolSchema,
    hub: ReconHub,
}

impl ReconTool {
    pub fn new(hub: ReconHub) -> Self {
        Self {
            schema: ToolSchema {
                name: "recon".to_string(),
                description: TOOL_DESCRIPTION.to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "description": "JS 逆向操作;method 决定其余字段",
                    "properties": {
                        "method": {
                            "type": "string",
                            "enum": [
                                "scripts", "search", "source",
                                "breakpoint", "breakpoints", "breakpointRemove",
                                "paused", "eval", "step", "resume", "console"
                            ],
                            "description": "操作名"
                        },
                        "tabId": { "type": "string", "description": "目标 tab;缺省 = 当前 tab" },
                        "query": { "type": "string", "description": "search/breakpoint:要检索的脚本文本(区分大小写;如函数名、关键字)" },
                        "isRegex": { "type": "boolean", "description": "search:query 按正则解释" },
                        "caseSensitive": { "type": "boolean", "description": "search:区分大小写(默认 false)" },
                        "urlFilter": { "type": "string", "description": "search/scripts/breakpoint:只搜 URL 含此子串的脚本" },
                        "maxResults": { "type": "integer", "description": "search:最多返回命中数(默认 30,上限 100)" },
                        "scriptId": { "type": "string", "description": "source:脚本 ID(scripts 给出)" },
                        "url": { "type": "string", "description": "source:脚本 URL(与 scriptId 二选一)" },
                        "startLine": { "type": "integer", "description": "source:起始行(0-based,缺省 0)" },
                        "endLine": { "type": "integer", "description": "source:结束行(0-based,含;缺省到文件尾)" },
                        "occurrence": { "type": "integer", "description": "breakpoint:第几个命中行(默认 1)" },
                        "condition": { "type": "string", "description": "breakpoint:断点条件 JS 表达式(真值才停)" },
                        "breakpointId": { "type": "string", "description": "breakpointRemove:断点 ID(breakpoints 给出)" },
                        "expression": { "type": "string", "description": "eval:在暂停帧内求值的 JS 表达式(如 arguments、this、局部变量名)" },
                        "frameIndex": { "type": "integer", "description": "eval:目标帧(暂停栈自上而下,缺省 0 最顶层)" },
                        "direction": { "type": "string", "enum": ["over", "into", "out"], "description": "step:单步方向" },
                        "max": { "type": "integer", "description": "console:最多返回条数(默认 100,上限 300)" }
                    },
                    "required": ["method"]
                }),
            },
            hub,
        }
    }
}

/// 把逆向结果渲染成模型可见文本(紧凑 JSON)。
fn render(value: Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        _ => serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string()),
    }
}

#[async_trait]
impl Tool for ReconTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, _ctx: &ToolContext) -> ToolOutput {
        let raw: Value = match parse_args_lenient(arguments) {
            Ok(value) => value,
            Err(error) => {
                return ToolOutput {
                    content: format!("invalid arguments: {error}"),
                    is_error: true,
                };
            }
        };
        let method = match raw.get("method").and_then(Value::as_str) {
            Some(method) => method.to_string(),
            None => {
                return ToolOutput {
                    content: "recon 工具需要 method(scripts/search/source/breakpoint/breakpoints/breakpointRemove/paused/eval/step/resume/console)".to_string(),
                    is_error: true,
                };
            }
        };
        match self.hub.execute_recon(&method, raw).await {
            Ok(value) => ToolOutput {
                content: render(value),
                is_error: false,
            },
            Err(error) => ToolOutput {
                content: format!("recon[{method}] 失败: {error}"),
                is_error: true,
            },
        }
    }
}

/// 桥接:`denia_browser::BrowserManager` 实现 `ReconExecute`
/// (孤儿规则:本 crate 同时见过 trait 与类型)。
#[async_trait]
impl ReconExecute for BrowserManager {
    async fn execute_recon(&self, method: &str, args: Value) -> Result<Value, String> {
        recon_dispatch(self, method, &args).await
    }
}

/// 分发到 BrowserManager.recon() 的对应操作。
async fn recon_dispatch(
    manager: &BrowserManager,
    method: &str,
    args: &Value,
) -> Result<Value, String> {
    let get = |key: &str| args.get(key).cloned().unwrap_or(Value::Null);
    let get_str = |key: &str| get(key).as_str().map(str::to_string);
    let tab_id = get_str("tabId");
    let recon = manager.recon();
    // 需要浏览器运行的入口统一先解析 tab(不拉起浏览器)。
    let (tab, _) = match method {
        "scripts" | "search" | "source" | "breakpoint" | "breakpoints" | "breakpointRemove"
        | "paused" | "eval" | "step" | "resume" | "console" => {
            manager.recon_resolve_tab(tab_id.as_deref()).await?
        }
        _ => return Err(format!("未知 method: {method}")),
    };
    match method {
        "scripts" => {
            let scripts = recon.scripts(&tab, get_str("urlFilter").as_deref());
            Ok(json!({ "tabId": tab, "scripts": scripts, "count": scripts.len() }))
        }
        "search" => {
            let query = get_str("query").ok_or("search 需要 query")?;
            let max = get("maxResults").as_u64().unwrap_or(30).clamp(1, 100) as usize;
            let (matches, skipped_large) = recon
                .search(
                    manager,
                    &tab,
                    &query,
                    get("isRegex").as_bool().unwrap_or(false),
                    get("caseSensitive").as_bool().unwrap_or(false),
                    get_str("urlFilter").as_deref(),
                    max,
                )
                .await?;
            Ok(
                json!({ "tabId": tab, "matches": matches, "count": matches.len(), "skippedLarge": skipped_large }),
            )
        }
        "source" => {
            recon
                .source(
                    manager,
                    &tab,
                    get_str("scriptId").as_deref(),
                    get_str("url").as_deref(),
                    get("startLine").as_i64(),
                    get("endLine").as_i64(),
                )
                .await
        }
        "breakpoint" => {
            let query = get_str("query").ok_or("breakpoint 需要 query")?;
            let info = recon
                .set_breakpoint(
                    manager,
                    &tab,
                    &query,
                    get_str("urlFilter").as_deref(),
                    get("occurrence").as_u64().unwrap_or(1) as usize,
                    get_str("condition").as_deref(),
                )
                .await?;
            Ok(json!({ "tabId": tab, "breakpoint": info }))
        }
        "breakpoints" => {
            let breakpoints = recon.list_breakpoints(&tab);
            Ok(json!({ "tabId": tab, "breakpoints": breakpoints }))
        }
        "breakpointRemove" => {
            let id = get_str("breakpointId").ok_or("breakpointRemove 需要 breakpointId")?;
            let removed = recon.remove_breakpoint(manager, &tab, &id).await?;
            Ok(json!({ "removed": removed }))
        }
        "paused" => {
            // 返回当前 tab 的暂停详情(未暂停返回 null)。
            let paused = recon.paused_info();
            let mine = paused.filter(|info| info.tab_id == tab);
            Ok(json!({ "tabId": tab, "paused": mine }))
        }
        "eval" => {
            let expression = get_str("expression").ok_or("eval 需要 expression")?;
            let frame_index = get("frameIndex").as_u64().unwrap_or(0) as usize;
            let value = recon
                .debug_eval(manager, &tab, frame_index, &expression)
                .await?;
            Ok(json!({ "tabId": tab, "value": value }))
        }
        "step" => {
            let direction = get_str("direction").unwrap_or_else(|| "over".to_string());
            recon.step(manager, &tab, &direction).await?;
            Ok(json!({ "stepped": direction }))
        }
        "resume" => {
            recon.resume(manager, &tab).await?;
            Ok(json!({ "resumed": true }))
        }
        "console" => {
            let max = get("max").as_u64().unwrap_or(100).clamp(1, 300) as usize;
            let messages = recon.console_list(&tab, max);
            Ok(json!({ "tabId": tab, "messages": messages, "count": messages.len() }))
        }
        _ => Err(format!("未知 method: {method}")),
    }
}
