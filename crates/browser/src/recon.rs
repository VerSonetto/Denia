//! JS 逆向侦察能力(与 DevTools 对照的工作台):
//!
//! - 脚本表:`Debugger.scriptParsed` 累积每个 tab 的 {scriptId, url, 长度};
//! - 源码检索:`Debugger.searchInContent` 按文本/正则跨脚本搜索(定位加密函数/端点);
//! - 文本断点:检索命中 → `Debugger.setBreakpointByUrl`(URL 级,刷新自动恢复);
//! - 暂停调试:`Debugger.paused` 存帧栈/作用域名,`evaluateOnCallFrame` 求值、单步、恢复;
//! - 控制台消息:`Runtime.consoleAPICalled` / `exceptionThrown` 环形缓冲;
//! - 网络增强:`requestWillBeSentExtraInfo` / `responseReceivedExtraInfo` 合并真实请求头
//!   (含 Cookie)与响应头(含 Set-Cookie),`requestWillBeSent` 补 initiator 调用栈。
//!
//! 状态全部存放在 [`ReconStore`](管理器共享),事件泵写入,命令读取;断点暂停期间
//! 页面冻结,主动命令执行前会自动恢复所在 tab(见 `commands::dispatch`)。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::{json, Value};

use crate::manager::BrowserManager;
use crate::BrowserEvent;

/// 单 tab 脚本表上限(超出丢最旧,防长期运行膨胀)。
const SCRIPTS_CAP: usize = 300;
/// 控制台消息环形上限。
const CONSOLE_CAP: usize = 300;
/// 检索跳过的大脚本阈值(bundle 动辄几 MB,全量拉取太伤)。字节数。
const SEARCH_SKIP_LEN: usize = 1_500_000;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptInfo {
    pub script_id: String,
    pub url: String,
    pub length: u64,
    pub start_line: i64,
    pub is_module: bool,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchMatch {
    pub script_id: String,
    pub url: String,
    /// 0-based 行号(CDP 约定;前端展示 +1)。
    pub line_number: i64,
    pub line_content: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BreakpointInfo {
    pub breakpoint_id: String,
    pub url: String,
    /// 0-based 行号。
    pub line_number: i64,
    pub condition: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PausedFrame {
    pub call_frame_id: String,
    pub function_name: String,
    pub url: String,
    /// 0-based 行号。
    pub line_number: i64,
    pub column_number: i64,
    pub scope_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PausedInfo {
    pub tab_id: String,
    pub reason: String,
    pub frames: Vec<PausedFrame>,
    /// 命中的断点 CDP id(可能为空:手动暂停/异常断点)。
    pub hit_breakpoints: Vec<String>,
    /// 暂停时刻(epoch ms)。
    pub at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleEntry {
    pub seq: u64,
    pub level: String,
    pub text: String,
    pub url: String,
    pub line: i64,
    pub at_ms: u64,
}

#[derive(Default)]
struct ConsoleLog {
    entries: HashMap<String, Vec<ConsoleEntry>>,
    counter: u64,
}

impl ConsoleLog {
    fn push(&mut self, tab_id: &str, mut entry: ConsoleEntry) {
        self.counter += 1;
        entry.seq = self.counter;
        let list = self.entries.entry(tab_id.to_string()).or_default();
        list.push(entry);
        if list.len() > CONSOLE_CAP {
            let excess = list.len() - CONSOLE_CAP;
            list.drain(..excess);
        }
    }

    fn list(&self, tab_id: &str, max: usize) -> Vec<ConsoleEntry> {
        self.entries
            .get(tab_id)
            .map(|list| {
                let start = list.len().saturating_sub(max);
                list[start..].to_vec()
            })
            .unwrap_or_default()
    }

    fn clear(&mut self, tab_id: &str) {
        self.entries.remove(tab_id);
    }
}

pub struct ReconStore {
    /// tabId -> 脚本表(插入序)。
    scripts: Mutex<HashMap<String, Vec<ScriptInfo>>>,
    /// scriptId -> tabId(事件反查)。
    by_script: Mutex<HashMap<String, String>>,
    /// 断点:URL 级,跨刷新存活;tab 关闭保留。
    breakpoints: Mutex<Vec<BreakpointInfo>>,
    /// 当前暂停态(None = 未暂停);resume/step 后清。
    paused: Mutex<Option<PausedInfo>>,
    paused_tab: Mutex<Option<String>>,
    console: Mutex<ConsoleLog>,
}

impl ReconStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(HashMap::new()),
            by_script: Mutex::new(HashMap::new()),
            breakpoints: Mutex::new(Vec::new()),
            paused: Mutex::new(None),
            paused_tab: Mutex::new(None),
            console: Mutex::new(ConsoleLog::default()),
        })
    }

    // ---------- 事件入口(事件泵调用) ----------

    /// 处理侦察相关 CDP 事件;返回需要广播的面板事件。
    pub fn on_event(&self, tab_id: &str, method: &str, params: &Value) -> Vec<BrowserEvent> {
        match method {
            "Debugger.scriptParsed" => {
                self.record_script(tab_id, params);
                Vec::new()
            }
            "Debugger.paused" => {
                let paused = self.record_paused(tab_id, params);
                match paused {
                    Some(info) => {
                        let frames: Vec<Value> = info
                            .frames
                            .iter()
                            .map(|frame| {
                                json!({
                                    "callFrameId": frame.call_frame_id,
                                    "functionName": frame.function_name,
                                    "url": frame.url,
                                    "lineNumber": frame.line_number,
                                    "columnNumber": frame.column_number,
                                })
                            })
                            .collect();
                        vec![BrowserEvent::DebuggerPaused {
                            tab_id: tab_id.to_string(),
                            reason: info.reason,
                            frames,
                        }]
                    }
                    None => Vec::new(),
                }
            }
            "Debugger.resumed" => {
                let tab = self.paused_tab.lock().unwrap().take();
                self.paused.lock().unwrap().take();
                tab.map(|tab_id| BrowserEvent::DebuggerResumed { tab_id })
                    .into_iter()
                    .collect()
            }
            "Runtime.consoleAPICalled" => {
                let entry = self.console_entry(tab_id, params);
                if let Some(entry) = entry {
                    self.console.lock().unwrap().push(tab_id, entry);
                }
                Vec::new()
            }
            "Runtime.exceptionThrown" => {
                let entry = self.exception_entry(tab_id, params);
                if let Some(entry) = entry {
                    self.console.lock().unwrap().push(tab_id, entry);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn record_script(&self, tab_id: &str, params: &Value) {
        let script_id = params.get("scriptId").and_then(Value::as_str).unwrap_or("");
        if script_id.is_empty() {
            return;
        }
        let url = params.get("url").and_then(Value::as_str).unwrap_or("").to_string();
        let info = ScriptInfo {
            script_id: script_id.to_string(),
            url,
            length: params.get("length").and_then(Value::as_u64).unwrap_or(0),
            start_line: params.get("startLine").and_then(Value::as_i64).unwrap_or(0),
            is_module: params.get("isModule").and_then(Value::as_bool).unwrap_or(false),
            hash: params.get("hash").and_then(Value::as_str).unwrap_or("").to_string(),
        };
        self.by_script
            .lock()
            .unwrap()
            .insert(script_id.to_string(), tab_id.to_string());
        let mut map = self.scripts.lock().unwrap();
        let list = map.entry(tab_id.to_string()).or_default();
        if list.iter().any(|existing| existing.script_id == info.script_id) {
            // 同 scriptId 重复解析(导航重载后同 URL 新 scriptId,旧条目保留无害)。
            return;
        }
        list.push(info);
        if list.len() > SCRIPTS_CAP {
            let excess = list.len() - SCRIPTS_CAP;
            list.drain(..excess);
        }
    }

    fn record_paused(&self, tab_id: &str, params: &Value) -> Option<PausedInfo> {
        let reason = params
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("other")
            .to_string();
        let hit: Vec<String> = params
            .get("hitBreakpoints")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let scripts = self.scripts.lock().unwrap();
        let mut frames = Vec::new();
        if let Some(call_frames) = params.get("callFrames").and_then(Value::as_array) {
            for frame in call_frames.iter().take(24) {
                let call_frame_id = frame
                    .get("callFrameId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let function_name = frame
                    .get("functionName")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let location = frame.get("location").unwrap_or(&Value::Null);
                let script_id = location
                    .get("scriptId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let url = scripts
                    .get(tab_id)
                    .and_then(|list| list.iter().find(|s| s.script_id == script_id))
                    .map(|s| s.url.clone())
                    .unwrap_or_else(|| "(未知脚本)".to_string());
                let scope_names: Vec<String> = frame
                    .get("scopeChain")
                    .and_then(Value::as_array)
                    .map(|scopes| {
                        scopes
                            .iter()
                            .filter_map(|scope| scope.get("type").and_then(Value::as_str))
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                frames.push(PausedFrame {
                    call_frame_id,
                    function_name: if function_name.is_empty() {
                        "(匿名)".to_string()
                    } else {
                        function_name
                    },
                    url,
                    line_number: location.get("lineNumber").and_then(Value::as_i64).unwrap_or(0),
                    column_number: location.get("columnNumber").and_then(Value::as_i64).unwrap_or(0),
                    scope_names,
                });
            }
        }
        let info = PausedInfo {
            tab_id: tab_id.to_string(),
            reason,
            frames,
            hit_breakpoints: hit,
            at_ms: crate::manager::now_ms(),
        };
        *self.paused.lock().unwrap() = Some(info.clone());
        *self.paused_tab.lock().unwrap() = Some(tab_id.to_string());
        Some(info)
    }

    fn console_entry(&self, _tab_id: &str, params: &Value) -> Option<ConsoleEntry> {
        let level = params
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("log")
            .to_string();
        let args: Vec<String> = params
            .get("args")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .take(8)
                    .map(remote_object_text)
                    .collect()
            })
            .unwrap_or_default();
        let text = if args.is_empty() {
            "(无参数)".to_string()
        } else {
            args.join(" ")
        };
        let (url, line) = stack_location(params.get("stackTrace"));
        Some(ConsoleEntry {
            seq: 0, // push 时覆写
            level,
            text,
            url,
            line,
            at_ms: params
                .get("timestamp")
                .and_then(Value::as_f64)
                .map(|millis| millis as u64)
                .unwrap_or_else(crate::manager::now_ms),
        })
    }

    fn exception_entry(&self, _tab_id: &str, params: &Value) -> Option<ConsoleEntry> {
        let details = params.get("exceptionDetails")?;
        let text = details
            .get("exception")
            .and_then(|e| e.get("description"))
            .or_else(|| details.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("页面异常")
            .to_string();
        let url = details
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let line = details.get("lineNumber").and_then(Value::as_i64).unwrap_or(0);
        Some(ConsoleEntry {
            seq: 0,
            level: "error".to_string(),
            text,
            url,
            line,
            at_ms: params
                .get("timestamp")
                .and_then(Value::as_f64)
                .map(|millis| millis as u64)
                .unwrap_or_else(crate::manager::now_ms),
        })
    }

    // ---------- 查询/操作(命令入口调用,浏览器须在跑) ----------

    pub fn paused_info(&self) -> Option<PausedInfo> {
        self.paused.lock().unwrap().clone()
    }

    /// 当前暂停所在的 tab(未暂停 = None)。
    pub fn paused_tab(&self) -> Option<String> {
        self.paused_tab.lock().unwrap().clone()
    }

    pub fn is_paused(&self, tab_id: Option<&str>) -> bool {
        let guard = self.paused_tab.lock().unwrap();
        match (guard.as_ref(), tab_id) {
            (Some(paused), Some(tab)) => paused == tab,
            (Some(_), None) => true,
            _ => false,
        }
    }

    pub fn list_breakpoints(&self, _tab_id: &str) -> Vec<BreakpointInfo> {
        self.breakpoints.lock().unwrap().clone()
    }

    pub fn console_list(&self, tab_id: &str, max: usize) -> Vec<ConsoleEntry> {
        self.console.lock().unwrap().list(tab_id, max.clamp(1, CONSOLE_CAP))
    }

    pub fn clear(&self, tab_id: &str) {
        self.scripts.lock().unwrap().remove(tab_id);
        self.console.lock().unwrap().clear(tab_id);
        // 断点是 URL 级,跨 tab/刷新存活,不随 clear 删除。
    }

    /// 浏览器退出:全部状态失效(新实例 scriptId/断点位置全部重来)。
    pub fn clear_all(&self) {
        self.scripts.lock().unwrap().clear();
        self.by_script.lock().unwrap().clear();
        self.breakpoints.lock().unwrap().clear();
        *self.paused.lock().unwrap() = None;
        *self.paused_tab.lock().unwrap() = None;
        self.console.lock().unwrap().entries.clear();
    }

    // ---------- 需要 CDP 的异步操作(manager 转发) ----------

    /// 列出该 tab 的脚本表。
    pub fn scripts(&self, tab_id: &str, url_filter: Option<&str>) -> Vec<ScriptInfo> {
        let map = self.scripts.lock().unwrap();
        map.get(tab_id)
            .map(|list| {
                list.iter()
                    .filter(|script| {
                        url_filter
                            .map(|needle| script.url.contains(needle))
                            .unwrap_or(true)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 跨脚本文本/正则检索(js-reverse `search_in_sources` 语义)。
    pub async fn search(
        &self,
        manager: &BrowserManager,
        tab_id: &str,
        query: &str,
        is_regex: bool,
        case_sensitive: bool,
        url_filter: Option<&str>,
        max_results: usize,
    ) -> Result<(Vec<SearchMatch>, usize), String> {
        if query.is_empty() {
            return Err("query 不能为空".to_string());
        }
        let (handle, session) = manager.recon_cdp_pair(tab_id).await?;
        let scripts = self.scripts(tab_id, url_filter);
        let mut matches = Vec::new();
        let mut skipped_large = 0usize;
        for script in scripts {
            if matches.len() >= max_results {
                break;
            }
            if script.length > SEARCH_SKIP_LEN as u64 {
                skipped_large += 1;
                continue;
            }
            let result = handle
                .send_with_session(
                    "Debugger.searchInContent",
                    json!({
                        "scriptId": script.script_id,
                        "query": query,
                        "caseSensitive": case_sensitive,
                        "isRegex": is_regex,
                    }),
                    Some(&session),
                )
                .await
                .map_err(|error| format!("检索 {}({}) 失败: {error}", script.url, script.script_id))?;
            if let Some(found) = result.get("result").and_then(Value::as_array) {
                for hit in found {
                    if matches.len() >= max_results {
                        break;
                    }
                    matches.push(SearchMatch {
                        script_id: script.script_id.clone(),
                        url: script.url.clone(),
                        line_number: hit.get("lineNumber").and_then(Value::as_i64).unwrap_or(0),
                        line_content: hit
                            .get("lineContent")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    });
                }
            }
        }
        Ok((matches, skipped_large))
    }

    /// 取源码片段(js-reverse `get_script_source` 语义;超大文件截断)。
    pub async fn source(
        &self,
        manager: &BrowserManager,
        tab_id: &str,
        script_id: Option<&str>,
        url: Option<&str>,
        start_line: Option<i64>,
        end_line: Option<i64>,
    ) -> Result<Value, String> {
        let (handle, session) = manager.recon_cdp_pair(tab_id).await?;
        let (target_id, target_url) = if let Some(id) = script_id {
            (id.to_string(), None)
        } else {
            let url = url.ok_or("需要 scriptId 或 url")?;
            let found = self
                .scripts(tab_id, Some(url))
                .into_iter()
                .find(|script| script.url == url || script.url.ends_with(url));
            let id = found
                .map(|script| script.script_id)
                .ok_or_else(|| format!("脚本未找到: {url}(先 reconScripts 拿 scriptId)"))?;
            (id, Some(url.to_string()))
        };
        let result = handle
            .send_with_session("Debugger.getScriptSource", json!({ "scriptId": target_id }), Some(&session))
            .await
            .map_err(|error| format!("取源码失败: {error}"))?;
        let mut source = result
            .get("scriptSource")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let truncated = source.len() > 100_000;
        if truncated {
            source.truncate(100_000);
        }
        // 按行截取(startLine/endLine 为 0-based;缺省全量)。
        let lines: Vec<&str> = source.lines().collect();
        let start = start_line.unwrap_or(0).max(0) as usize;
        let end = end_line
            .map(|line| (line + 1).max(start as i64) as usize)
            .unwrap_or(lines.len())
            .min(lines.len());
        let slice = lines[start..end].join("\n");
        Ok(json!({
            "scriptId": target_id,
            "url": target_url.unwrap_or_else(|| self.script_url(tab_id, &target_id)),
            "startLine": start,
            "endLine": end,
            "totalLines": lines.len(),
            "truncated": truncated,
            "source": slice,
        }))
    }

    /// 文本断点:检索定位 → setBreakpointByUrl(js-reverse `set_breakpoint_on_text` 语义)。
    /// URL 级断点,刷新/重载后自动恢复;同 (url,line) 幂等。
    pub async fn set_breakpoint(
        &self,
        manager: &BrowserManager,
        tab_id: &str,
        query: &str,
        url_filter: Option<&str>,
        occurrence: usize,
        condition: Option<&str>,
    ) -> Result<BreakpointInfo, String> {
        let (handle, session) = manager.recon_cdp_pair(tab_id).await?;
        let (matches, _) = self
            .search(manager, tab_id, query, false, true, url_filter, 40)
            .await?;
        let hit = matches
            .get(occurrence.saturating_sub(1))
            .ok_or_else(|| format!("检索「{query}」无第 {occurrence} 个命中"))?;
        let url = if hit.url.is_empty() {
            return Err(format!(
                "命中脚本无 URL(内联/动态脚本,scriptId {}),无法设 URL 级断点;换更独特的文本",
                hit.script_id
            ));
        } else {
            hit.url.clone()
        };
        // 幂等:同 tab 同 (url,line) 已有断点直接返回。
        {
            let guard = self.breakpoints.lock().unwrap();
            if let Some(existing) = guard
                .iter()
                .find(|bp| bp.url == url && bp.line_number == hit.line_number)
            {
                return Ok(existing.clone());
            }
        }
        let mut params = json!({
            "lineNumber": hit.line_number,
            "url": url,
            "columnNumber": 0,
        });
        if let Some(condition_text) = condition.filter(|text| !text.trim().is_empty()) {
            params["condition"] = Value::String(condition_text.to_string());
        }
        let result = handle
            .send_with_session(
                "Debugger.setBreakpointByUrl",
                params,
                Some(&session),
            )
            .await
            .map_err(|error| format!("设断点失败({url}:{}): {error}", hit.line_number))?;
        let breakpoint_id = result
            .get("breakpointId")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let info = BreakpointInfo {
            breakpoint_id,
            url,
            line_number: hit.line_number,
            condition: condition.map(str::to_string),
        };
        self.breakpoints.lock().unwrap().push(info.clone());
        Ok(info)
    }

    /// 删断点:本地表 + CDP(忽略远端已失效)。
    pub async fn remove_breakpoint(
        &self,
        manager: &BrowserManager,
        _tab_id: &str,
        breakpoint_id: &str,
    ) -> Result<bool, String> {
        let removed = {
            let mut guard = self.breakpoints.lock().unwrap();
            let Some(index) = guard.iter().position(|bp| bp.breakpoint_id == breakpoint_id) else {
                return Ok(false);
            };
            guard.remove(index)
        };
        if let Ok((handle, session)) = manager.recon_cdp_pair(_tab_id).await {
            let _ = handle
                .send_with_session(
                    "Debugger.removeBreakpoint",
                    json!({ "breakpointId": removed.breakpoint_id }),
                    Some(&session),
                )
                .await;
        }
        Ok(true)
    }

    /// 暂停帧上求值(js-reverse `evaluate_script` paused 语义)。
    pub async fn debug_eval(
        &self,
        manager: &BrowserManager,
        tab_id: &str,
        frame_index: usize,
        expression: &str,
    ) -> Result<Value, String> {
        let paused = self
            .paused
            .lock()
            .unwrap()
            .clone()
            .ok_or("页面未暂停;先下断点并触发")?;
        if paused.tab_id != tab_id {
            return Err(format!("暂停发生在 tab {},不是当前 tab", paused.tab_id));
        }
        let frame = paused
            .frames
            .get(frame_index)
            .ok_or_else(|| format!("帧 {frame_index} 不存在(共 {} 帧)", paused.frames.len()))?;
        let (handle, session) = manager.recon_cdp_pair(tab_id).await?;
        let result = handle
            .send_with_session(
                "Debugger.evaluateOnCallFrame",
                json!({
                    "callFrameId": frame.call_frame_id,
                    "expression": expression,
                    "returnByValue": true,
                    "generatePreview": true,
                }),
                Some(&session),
            )
            .await
            .map_err(|error| format!("帧内求值失败: {error}"))?;
        if let Some(details) = result.get("exceptionDetails") {
            let message = details
                .pointer("/exception/description")
                .or_else(|| details.get("text"))
                .and_then(Value::as_str)
                .unwrap_or("表达式抛异常");
            return Err(message.to_string());
        }
        Ok(remote_object_value(result.get("result").unwrap_or(&Value::Null)))
    }

    /// 单步:over/into/out。
    pub async fn step(
        &self,
        manager: &BrowserManager,
        tab_id: &str,
        direction: &str,
    ) -> Result<(), String> {
        let method = match direction {
            "over" => "Debugger.stepOver",
            "into" => "Debugger.stepInto",
            "out" => "Debugger.stepOut",
            _ => return Err(format!("direction 只能是 over/into/out,收到 {direction}")),
        };
        self.assert_paused(tab_id)?;
        let (handle, session) = manager.recon_cdp_pair(tab_id).await?;
        handle
            .send_with_session(method, json!({}), Some(&session))
            .await
            .map_err(|error| error)?;
        Ok(())
    }

    /// 恢复执行(断点保留)。
    pub async fn resume(&self, manager: &BrowserManager, tab_id: &str) -> Result<(), String> {
        self.assert_paused(tab_id)?;
        let (handle, session) = manager.recon_cdp_pair(tab_id).await?;
        handle
            .send_with_session("Debugger.resume", json!({}), Some(&session))
            .await
            .map_err(|error| error)?;
        // resumed 事件会清状态;此处兜底(事件可能先到)。
        *self.paused.lock().unwrap() = None;
        *self.paused_tab.lock().unwrap() = None;
        Ok(())
    }

    fn assert_paused(&self, tab_id: &str) -> Result<(), String> {
        if !self.is_paused(Some(tab_id)) {
            return Err("页面未暂停".to_string());
        }
        Ok(())
    }

    fn script_url(&self, tab_id: &str, script_id: &str) -> String {
        self.scripts
            .lock()
            .unwrap()
            .get(tab_id)
            .and_then(|list| list.iter().find(|s| s.script_id == script_id))
            .map(|s| s.url.clone())
            .unwrap_or_default()
    }
}

/// RemoteObject → 展示文本(object 取 description/value 的 JSON 化)。
fn remote_object_text(object: &Value) -> String {
    if let Some(value) = object.get("value") {
        if value.is_string() {
            return value.as_str().unwrap_or("").to_string();
        }
        if value.is_null() {
            return "null".to_string();
        }
    }
    match object.get("subtype").and_then(Value::as_str) {
        Some("error") => {
            object.get("description").and_then(Value::as_str).unwrap_or("[Error]").to_string()
        }
        Some("array" | "object" | "map" | "set" | "regexp" | "date" | "node") => {
            let description = object.get("description").and_then(Value::as_str).unwrap_or("");
            if description.is_empty() {
                let preview: Vec<String> = object
                    .pointer("/preview/properties")
                    .and_then(Value::as_array)
                    .map(|props| {
                        props
                            .iter()
                            .filter_map(|prop| {
                                let name = prop.get("name").and_then(Value::as_str)?;
                                let value = prop.get("value").and_then(Value::as_str).unwrap_or("?");
                                Some(format!("{name}: {value}"))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                format!("{{{}}}", preview.join(", "))
            } else {
                description.to_string()
            }
        }
        _ => {
            let description = object.get("description").and_then(Value::as_str).unwrap_or("");
            if description.is_empty() {
                "[值]".to_string()
            } else {
                description.to_string()
            }
        }
    }
}

/// RemoteObject → 可回传值(returnByValue 时 value 即为 JSON;兜底 description)。
fn remote_object_value(object: &Value) -> Value {
    if let Some(value) = object.get("value") {
        if !value.is_null() {
            return value.clone();
        }
    }
    let (r#type, subtype) = (
        object.get("type").and_then(Value::as_str).unwrap_or(""),
        object.get("subtype").and_then(Value::as_str).unwrap_or(""),
    );
    if r#type == "undefined" {
        return json!({ "kind": "undefined" });
    }
    if r#type == "function" {
        return json!({ "kind": "function", "description": object.get("description").and_then(Value::as_str).unwrap_or("") });
    }
    if subtype == "error" {
        return json!({ "kind": "error", "description": object.get("description").and_then(Value::as_str).unwrap_or("") });
    }
    json!({
        "kind": subtype,
        "description": object.get("description").and_then(Value::as_str).unwrap_or(""),
        "preview": object.get("preview"),
    })
}

/// 从 stackTrace 里取 (url, lineNumber)(0-based)。
fn stack_location(stack: Option<&Value>) -> (String, i64) {
    let Some(stack) = stack else {
        return (String::new(), 0);
    };
    let frames = stack.get("callFrames").and_then(Value::as_array);
    let Some(first) = frames.and_then(|list| list.first()) else {
        return (String::new(), 0);
    };
    (
        first.get("url").and_then(Value::as_str).unwrap_or("").to_string(),
        first.get("lineNumber").and_then(Value::as_i64).unwrap_or(0),
    )
}

/// manager 里 browser 未跑/无 handle 时的错误。
pub fn not_running_error(action: &str) -> String {
    format!("浏览器未运行,无法{action};先启动浏览器")
}