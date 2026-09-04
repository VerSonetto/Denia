//! JS 逆向侦察 API:脚本表/源码检索/断点/暂停调试/控制台(逆向工作台)。
//!
//! 全部走 `BrowserManager.recon`;操作要求浏览器已在运行(侦察是交互态,
//! 不因面板点击自发拉起 Chrome)。断点暂停会冻结页面:主动 browser 命令
//! 执行前会自动恢复(见 `manager::execute`),面板用户从这里接管调试节奏。

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/browser/recon/scripts", post(recon_scripts))
        .route("/api/browser/recon/search", post(recon_search))
        .route("/api/browser/recon/source", post(recon_source))
        .route("/api/browser/recon/breakpoint", post(recon_breakpoint))
        .route("/api/browser/recon/breakpointRemove", post(recon_breakpoint_remove))
        .route("/api/browser/recon/breakpoints", post(recon_breakpoints))
        .route("/api/browser/recon/paused", post(recon_paused))
        .route("/api/browser/recon/eval", post(recon_eval))
        .route("/api/browser/recon/step", post(recon_step))
        .route("/api/browser/recon/resume", post(recon_resume))
        .route("/api/browser/recon/console", post(recon_console))
        .route("/api/browser/recon/clear", post(recon_clear))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Req {
    #[serde(default)]
    tab_id: Option<String>,
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    is_regex: Option<bool>,
    #[serde(default)]
    case_sensitive: Option<bool>,
    #[serde(default)]
    url_filter: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
    #[serde(default)]
    script_id: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    start_line: Option<i64>,
    #[serde(default)]
    end_line: Option<i64>,
    #[serde(default)]
    occurrence: Option<usize>,
    #[serde(default)]
    condition: Option<String>,
    #[serde(default)]
    breakpoint_id: Option<String>,
    #[serde(default)]
    frame_index: Option<usize>,
    #[serde(default)]
    expression: Option<String>,
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    max: Option<usize>,
}

fn bad(message: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, message.into())
}

/// POST /api/browser/recon/scripts {tabId?, urlFilter?} — 已解析脚本表。
async fn recon_scripts(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    let scripts = state.browser.recon().scripts(&tab_id, req.url_filter.as_deref());
    Ok(Json(json!({ "tabId": tab_id, "scripts": scripts, "count": scripts.len() })))
}

/// POST /api/browser/recon/search {tabId?, query, isRegex?, caseSensitive?, urlFilter?, maxResults?}
/// — 跨脚本文本/正则检索(js-reverse `search_in_sources` 语义)。
async fn recon_search(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let query = req.query.clone().ok_or_else(|| bad("query 必填"))?;
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    let max = req.max_results.unwrap_or(30).clamp(1, 100);
    let (matches, skipped_large) = state
        .browser
        .recon()
        .search(
            &state.browser,
            &tab_id,
            &query,
            req.is_regex.unwrap_or(false),
            req.case_sensitive.unwrap_or(false),
            req.url_filter.as_deref(),
            max,
        )
        .await
        .map_err(|e| bad(e))?;
    Ok(Json(json!({
        "tabId": tab_id,
        "matches": matches,
        "count": matches.len(),
        "skippedLarge": skipped_large,
    })))
}

/// POST /api/browser/recon/source {tabId?, scriptId?|url?, startLine?, endLine?}
/// — 取脚本源码(片段)。
async fn recon_source(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    let source = state
        .browser
        .recon()
        .source(
            &state.browser,
            &tab_id,
            req.script_id.as_deref(),
            req.url.as_deref(),
            req.start_line,
            req.end_line,
        )
        .await
        .map_err(|e| bad(e))?;
    Ok(Json(source))
}

/// POST /api/browser/recon/breakpoint {tabId?, query, urlFilter?, occurrence?, condition?}
/// — 文本断点:检索定位 → setBreakpointByUrl(js-reverse `set_breakpoint_on_text` 语义)。
async fn recon_breakpoint(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let query = req.query.clone().ok_or_else(|| bad("query 必填"))?;
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    let info = state
        .browser
        .recon()
        .set_breakpoint(
            &state.browser,
            &tab_id,
            &query,
            req.url_filter.as_deref(),
            req.occurrence.unwrap_or(1),
            req.condition.as_deref(),
        )
        .await
        .map_err(|e| bad(e))?;
    Ok(Json(json!({ "tabId": tab_id, "breakpoint": info })))
}

/// POST /api/browser/recon/breakpointRemove {tabId?, breakpointId} — 删断点。
async fn recon_breakpoint_remove(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let breakpoint_id = req.breakpoint_id.clone().ok_or_else(|| bad("breakpointId 必填"))?;
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    let removed = state
        .browser
        .recon()
        .remove_breakpoint(&state.browser, &tab_id, &breakpoint_id)
        .await
        .map_err(|e| bad(e))?;
    Ok(Json(json!({ "removed": removed })))
}

/// POST /api/browser/recon/breakpoints {tabId?} — 断点列表。
async fn recon_breakpoints(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    let breakpoints = state.browser.recon().list_breakpoints(&tab_id);
    Ok(Json(json!({ "tabId": tab_id, "breakpoints": breakpoints })))
}

/// POST /api/browser/recon/paused {tabId?} — 当前暂停详情(未暂停 = null)。
async fn recon_paused(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    let paused = state.browser.recon().paused_info();
    let mine = paused.filter(|info| info.tab_id == tab_id);
    Ok(Json(json!({ "tabId": tab_id, "paused": mine })))
}

/// POST /api/browser/recon/eval {tabId?, frameIndex?, expression}
/// — 暂停帧内求值(js-reverse paused `evaluate_script` 语义)。
async fn recon_eval(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let expression = req.expression.clone().ok_or_else(|| bad("expression 必填"))?;
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    let value = state
        .browser
        .recon()
        .debug_eval(&state.browser, &tab_id, req.frame_index.unwrap_or(0), &expression)
        .await
        .map_err(|e| bad(e))?;
    Ok(Json(json!({ "tabId": tab_id, "value": value })))
}

/// POST /api/browser/recon/step {tabId?, direction: over|into|out} — 单步。
async fn recon_step(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let direction = req.direction.clone().unwrap_or_else(|| "over".to_string());
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    state
        .browser
        .recon()
        .step(&state.browser, &tab_id, &direction)
        .await
        .map_err(|e| bad(e))?;
    Ok(Json(json!({ "stepped": direction })))
}

/// POST /api/browser/recon/resume {tabId?} — 恢复执行(断点保留)。
async fn recon_resume(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    state
        .browser
        .recon()
        .resume(&state.browser, &tab_id)
        .await
        .map_err(|e| bad(e))?;
    Ok(Json(json!({ "resumed": true })))
}

/// POST /api/browser/recon/console {tabId?, max?} — 控制台消息(环形,最新在前?按时间序)。
async fn recon_console(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    let messages = state.browser.recon().console_list(&tab_id, req.max.unwrap_or(100));
    Ok(Json(json!({ "tabId": tab_id, "messages": messages, "count": messages.len() })))
}

/// POST /api/browser/recon/clear {tabId?} — 清该 tab 的脚本表/控制台(断点保留,URL 级)。
async fn recon_clear(
    State(state): State<Arc<AppState>>,
    Json(req): Json<Req>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let (tab_id, _) = state.browser.recon_resolve_tab(req.tab_id.as_deref()).await.map_err(|e| bad(e))?;
    state.browser.recon().clear(&tab_id);
    state.browser.network_clear(&tab_id);
    Ok(Json(json!({ "cleared": true })))
}