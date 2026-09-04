//! 命令分发:BrowserCommand → CDP 调用。与 ZCode `executeBrowserCommandOnView`
//! 的 switch 分发同构;URL 白名单/timeout/abort 语义对齐。

use std::time::Duration;

use serde_json::{Value, json};

use crate::manager::Ctx;
use crate::scripts;
use crate::{BrowserCommand, CommandOutcome};

const NAVIGATE_SETTLE_MS: u64 = 10_000;
const WAIT_POLL_MS: u64 = 120;
const DEFAULT_WAIT_MS: u64 = 10_000;

pub(crate) async fn dispatch(
    manager: &crate::manager::BrowserManager,
    inner: &mut crate::manager::Inner,
    command: BrowserCommand,
) -> CommandOutcome {
    let mut ctx = Ctx { manager, inner };
    match command {
        BrowserCommand::Navigate { url, tab_id } => {
            navigate(&mut ctx, &url, tab_id.as_deref()).await
        }
        BrowserCommand::Back { tab_id } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    if let Err(e) = ctx.cdp(&id, "Page.navigateBack", json!({})).await {
                        // 老 Chrome 无此命令:走 history API
                        let _ = ctx.cdp(&id, "Page.navigateToHistoryEntry", json!({})).await;
                        if e.error.as_ref().is_some_and(|x| x.code != "tab_not_found") {
                            let _ = e;
                        }
                    }
                    state_after(&mut ctx, &id, started).await
                }
                Err(e) => e,
            }
        }
        BrowserCommand::Forward { tab_id } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let _ = ctx.cdp(&id, "Page.navigateForward", json!({})).await;
                    state_after(&mut ctx, &id, started).await
                }
                Err(e) => e,
            }
        }
        BrowserCommand::Reload { tab_id } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let _ = ctx
                        .cdp(&id, "Page.reload", json!({"ignoreCache": false}))
                        .await;
                    state_after(&mut ctx, &id, started).await
                }
                Err(e) => e,
            }
        }
        BrowserCommand::GetState { tab_id } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => state_after(&mut ctx, &id, started).await,
                Err(e) => e,
            }
        }
        BrowserCommand::Snapshot {
            tab_id,
            max_elements,
            include_hidden,
        } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let max = max_elements.unwrap_or(200).clamp(1, 1000);
                    let include_hidden = include_hidden.unwrap_or(false);
                    let expression = scripts::snapshot_js(max, 300, include_hidden);
                    match ctx.eval_json(&id, &expression).await {
                        Ok(mut value) => {
                            if let Some(obj) = value.as_object_mut() {
                                obj.insert("tabId".into(), Value::String(id));
                            }
                            CommandOutcome {
                                snapshot: Some(value),
                                ..CommandOutcome::ok_value(Value::Null, elapsed(&started))
                            }
                        }
                        Err(e) => e,
                    }
                }
                Err(e) => e,
            }
        }
        BrowserCommand::Screenshot {
            tab_id,
            full_page,
            clip,
        } => screenshot(&mut ctx, tab_id.as_deref(), full_page, clip).await,
        BrowserCommand::Click {
            tab_id,
            r#ref,
            x,
            y,
            button,
            double_click,
        } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let point = match resolve_point(&mut ctx, &id, r#ref.as_deref(), x, y).await {
                        Ok(point) => point,
                        Err(e) => return e,
                    };
                    let button = button.unwrap_or_else(|| "left".into());
                    let click_count = if double_click.unwrap_or(false) { 2 } else { 1 };
                    if let Err(e) =
                        dispatch_click(&mut ctx, &id, point.0, point.1, &button, click_count).await
                    {
                        return e;
                    }
                    state_after(&mut ctx, &id, started).await
                }
                Err(e) => e,
            }
        }
        BrowserCommand::Type {
            tab_id,
            r#ref,
            text,
        } => type_text(&mut ctx, tab_id.as_deref(), r#ref.as_deref(), &text).await,
        BrowserCommand::Fill {
            tab_id,
            r#ref,
            value,
        } => {
            // fill = 点中 ref 后整体替换内容(点击 → 全选 → 粘贴)
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let point = match resolve_ref(&mut ctx, &id, &r#ref).await {
                        Ok(point) => point,
                        Err(e) => return e,
                    };
                    if let Err(e) = dispatch_click(&mut ctx, &id, point.0, point.1, "left", 1).await
                    {
                        return e;
                    }
                    if let Err(e) = select_all(&mut ctx, &id).await {
                        return e;
                    }
                    if let Err(e) = paste(&mut ctx, &id, &value).await {
                        return e;
                    }
                    state_after(&mut ctx, &id, started).await
                }
                Err(e) => e,
            }
        }
        BrowserCommand::Press { tab_id, key, r#ref } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    if let Some(reference) = r#ref.as_deref() {
                        let point = match resolve_ref(&mut ctx, &id, reference).await {
                            Ok(point) => point,
                            Err(e) => return e,
                        };
                        if let Err(e) =
                            dispatch_click(&mut ctx, &id, point.0, point.1, "left", 1).await
                        {
                            return e;
                        }
                    }
                    if let Err(e) = dispatch_key(&mut ctx, &id, &key).await {
                        return e;
                    }
                    state_after(&mut ctx, &id, started).await
                }
                Err(e) => e,
            }
        }
        BrowserCommand::Scroll {
            tab_id,
            r#ref,
            x,
            y,
        } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let (px, py) = match resolve_point(&mut ctx, &id, r#ref.as_deref(), x, y).await
                    {
                        Ok(point) => point,
                        Err(e) => return e,
                    };
                    // 默认向下滚 3 格(与 Playwright wheel 语义一致,量由调用方给)
                    if let Err(e) = dispatch_wheel(&mut ctx, &id, px, py, 0.0, 360.0).await {
                        return e;
                    }
                    state_after(&mut ctx, &id, started).await
                }
                Err(e) => e,
            }
        }
        BrowserCommand::Hover {
            tab_id,
            r#ref,
            x,
            y,
        } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let (px, py) = match resolve_point(&mut ctx, &id, r#ref.as_deref(), x, y).await
                    {
                        Ok(point) => point,
                        Err(e) => return e,
                    };
                    if let Err(e) = ctx
                        .cdp(
                            &id,
                            "Input.dispatchMouseEvent",
                            json!({
                                "type": "mouseMoved", "x": px, "y": py,
                            }),
                        )
                        .await
                    {
                        return e;
                    }
                    state_after(&mut ctx, &id, started).await
                }
                Err(e) => e,
            }
        }
        BrowserCommand::Select {
            tab_id,
            r#ref,
            values,
        } => select_option(&mut ctx, tab_id.as_deref(), &r#ref, &values).await,
        BrowserCommand::Check {
            tab_id,
            r#ref,
            checked,
        } => check_element(&mut ctx, tab_id.as_deref(), &r#ref, checked).await,
        BrowserCommand::Drag {
            tab_id,
            from_ref,
            to_ref,
            from,
            to,
        } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let start = match resolve_point(
                        &mut ctx,
                        &id,
                        from_ref.as_deref(),
                        from.map(|p| p.x),
                        from.map(|p| p.y),
                    )
                    .await
                    {
                        Ok(point) => point,
                        Err(e) => return e,
                    };
                    let end = match resolve_point(
                        &mut ctx,
                        &id,
                        to_ref.as_deref(),
                        to.map(|p| p.x),
                        to.map(|p| p.y),
                    )
                    .await
                    {
                        Ok(point) => point,
                        Err(e) => return e,
                    };
                    if let Err(e) = dispatch_drag(&mut ctx, &id, start, end).await {
                        return e;
                    }
                    state_after(&mut ctx, &id, started).await
                }
                Err(e) => e,
            }
        }
        BrowserCommand::ElementInfo { tab_id, x, y } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => match ctx.eval_json(&id, &scripts::element_info_js(x, y)).await {
                    Ok(value) => CommandOutcome::ok_value(value, elapsed(&started)),
                    Err(e) => e,
                },
                Err(e) => e,
            }
        }
        BrowserCommand::Evaluate { tab_id, expression } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    // ZCode EVALUATE_SCRIPT:把表达式 JSON 序列化返回
                    let wrapped = format!(
                        "(function(){{var __v;try{{__v=(function(){{return ({expression});}})();}}catch(e){{return JSON.stringify({{ok:false,message:String(e)}});}}var __s;try{{__s=JSON.stringify(__v);}}catch(e){{__s=undefined;}}if(typeof __s==='string')return JSON.stringify({{ok:true,kind:'json',data:__s}});return JSON.stringify({{ok:true,kind:'str',data:String(__v)}});}})()"
                    );
                    match ctx.eval_json(&id, &wrapped).await {
                        Ok(value) => {
                            let ok = value.get("ok").and_then(Value::as_bool).unwrap_or(false);
                            if !ok {
                                let message = value
                                    .get("message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("evaluate error");
                                return CommandOutcome::err(
                                    "execution_error",
                                    message,
                                    elapsed(&started),
                                );
                            }
                            let data = value
                                .get("data")
                                .cloned()
                                .map(|raw| {
                                    serde_json::from_str::<Value>(raw.as_str().unwrap_or("null"))
                                        .unwrap_or(raw)
                                })
                                .unwrap_or(Value::Null);
                            CommandOutcome::ok_value(data, elapsed(&started))
                        }
                        Err(e) => e,
                    }
                }
                Err(e) => e,
            }
        }
        BrowserCommand::WaitFor {
            tab_id,
            selector,
            text,
            text_gone,
            timeout_ms,
        } => {
            wait_for(
                &mut ctx,
                tab_id.as_deref(),
                selector.as_deref(),
                text.as_deref(),
                text_gone.as_deref(),
                timeout_ms,
            )
            .await
        }
        BrowserCommand::GetDialog { tab_id } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let dialog = ctx.manager.take_dialog(&id);
                    CommandOutcome {
                        dialog,
                        ..CommandOutcome::ok_value(Value::Null, elapsed(&started))
                    }
                }
                Err(e) => e,
            }
        }
        BrowserCommand::HandleDialog {
            tab_id,
            accept,
            prompt_text,
        } => handle_dialog(&mut ctx, tab_id.as_deref(), accept, prompt_text.as_deref()).await,
        BrowserCommand::NewTab { url } => {
            let started = std::time::Instant::now();
            match crate::manager::BrowserManager::create_tab_inner(ctx.inner, url.as_deref()).await
            {
                Ok(id) => {
                    ctx.set_active(&id);
                    ctx.broadcast_tabs_changed();
                    CommandOutcome::ok_value(json!({"tabId": id}), elapsed(&started))
                }
                Err(error) => CommandOutcome::err("cdp_error", error, elapsed(&started)),
            }
        }
        BrowserCommand::List => {
            let started = std::time::Instant::now();
            let tabs: Vec<Value> = ctx
                .inner
                .tabs
                .values()
                .map(|info| {
                    let (url, title) = ctx
                        .manager
                        .tab_view(&info.tab_id)
                        .unwrap_or_else(|| (info.url.clone(), info.title.clone()));
                    json!({
                        "tabId": info.tab_id,
                        "url": url,
                        "title": title,
                        "active": ctx.inner.active_tab.as_deref() == Some(info.tab_id.as_str()),
                    })
                })
                .collect();
            CommandOutcome::ok_value(json!({ "tabs": tabs }), elapsed(&started))
        }
        BrowserCommand::Close { tab_id } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    crate::manager::close_tab_full(ctx.inner, ctx.manager, &id).await;
                    // 关到最后一个 tab:补一个空白 tab 兜底,面板/工具调用
                    // 永远有 active tab 可用(否则 resolve_tab 直接报 no_tab)。
                    if ctx.inner.tabs.is_empty() {
                        match crate::manager::BrowserManager::create_tab_inner(ctx.inner, None)
                            .await
                        {
                            Ok(new_id) => {
                                ctx.set_active(&new_id);
                                ctx.broadcast_tabs_changed();
                            }
                            Err(error) => {
                                tracing::warn!(%error, "重建兜底 tab 失败");
                            }
                        }
                    }
                    CommandOutcome::ok_value(json!({"closed": true}), elapsed(&started))
                }
                Err(e) => e,
            }
        }
        BrowserCommand::Activate { tab_id } => {
            let started = std::time::Instant::now();
            if ctx.inner.tabs.contains_key(&tab_id) {
                ctx.set_active(&tab_id);
                let _ = ctx.cdp(&tab_id, "Page.bringToFront", json!({})).await;
                ctx.broadcast_tabs_changed();
            }
            CommandOutcome::ok_value(json!({"activated": true}), elapsed(&started))
        }
        BrowserCommand::ViewportSet {
            tab_id,
            width,
            height,
        } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    if let Err(e) = ctx
                        .cdp(&id, "Emulation.setDeviceMetricsOverride", json!({
                            "width": width, "height": height, "deviceScaleFactor": 0, "mobile": false,
                        }))
                        .await
                    {
                        return e;
                    }
                    ctx.inner.viewport.insert(id, (width, height));
                    CommandOutcome::ok_value(
                        json!({"width": width, "height": height}),
                        elapsed(&started),
                    )
                }
                Err(e) => e,
            }
        }
        BrowserCommand::ViewportReset { tab_id } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let _ = ctx
                        .cdp(&id, "Emulation.clearDeviceMetricsOverride", json!({}))
                        .await;
                    ctx.inner.viewport.remove(&id);
                    CommandOutcome::ok_value(json!({"reset": true}), elapsed(&started))
                }
                Err(e) => e,
            }
        }
        BrowserCommand::NetworkList {
            tab_id,
            url_filter,
            max,
        } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let entries = ctx.manager.network_list(
                        &id,
                        url_filter.as_deref(),
                        max.unwrap_or(50).min(200),
                    );
                    CommandOutcome::ok_value(
                        json!({ "requests": entries, "count": entries.len() }),
                        elapsed(&started),
                    )
                }
                Err(e) => e,
            }
        }
        BrowserCommand::NetworkGetBody {
            tab_id,
            request_id,
            body_kind,
        } => {
            let want_request = body_kind.as_deref() == Some("request");
            network_get_body(&mut ctx, tab_id.as_deref(), &request_id, want_request).await
        }
        BrowserCommand::StartScreencast { tab_id } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let _ = ctx
                        .cdp(
                            &id,
                            "Page.startScreencast",
                            json!({
                                "format": "jpeg",
                                "quality": 60,
                                "maxWidth": 1280,
                                "maxHeight": 800,
                                "everyNthFrame": 1,
                            }),
                        )
                        .await;
                    CommandOutcome::ok_value(json!({"streaming": true}), elapsed(&started))
                }
                Err(e) => e,
            }
        }
        BrowserCommand::StopScreencast { tab_id } => {
            let started = std::time::Instant::now();
            match tab(&mut ctx, tab_id.as_deref()).await {
                Ok(id) => {
                    let _ = ctx.cdp(&id, "Page.stopScreencast", json!({})).await;
                    CommandOutcome::ok_value(json!({"streaming": false}), elapsed(&started))
                }
                Err(e) => e,
            }
        }
    }
}

// ---------- 子步骤 ----------

async fn tab(ctx: &mut Ctx<'_>, tab_id: Option<&str>) -> Result<String, CommandOutcome> {
    ctx.resolve_tab(tab_id)
}

fn elapsed(started: &std::time::Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

async fn state_after(
    ctx: &mut Ctx<'_>,
    tab_id: &str,
    started: std::time::Instant,
) -> CommandOutcome {
    // url/title/canGoBack/scrollZ 引擎态一次拿齐(ZCode `readState` + Tj 脚本等价物)
    let expression = r#"(function(){return JSON.stringify({url:location.href,title:document.title,canGoBack:history.length>1,scrollX:window.scrollX||0,scrollY:window.scrollY||0,viewportWidth:window.innerWidth||0,viewportHeight:window.innerHeight||0});})()"#;
    match ctx.eval_json(tab_id, expression).await {
        Ok(state) => {
            // 回写导航视图:tab 栏/地址栏的 url+title 即时刷新(事件路径只补 url)。
            let url = state.get("url").and_then(Value::as_str).unwrap_or("");
            let title = state.get("title").and_then(Value::as_str).unwrap_or("");
            if !url.is_empty() {
                ctx.manager.update_tab_view(tab_id, url, title);
            }
            CommandOutcome {
                state: Some(state),
                ..CommandOutcome::ok_value(Value::Null, elapsed(&started))
            }
        }
        Err(_) => CommandOutcome::ok_value(Value::Null, elapsed(&started)),
    }
}

/// URL 白名单:只允许 http/https/about(ZCode `isAllowedBrowserUrl` 同款)。
fn is_allowed_url(url: &str) -> bool {
    if url == "about:blank" {
        return true;
    }
    url::parse_scheme(url).is_some_and(|scheme| scheme == "http" || scheme == "https")
}

async fn navigate(ctx: &mut Ctx<'_>, url: &str, tab_id: Option<&str>) -> CommandOutcome {
    let started = std::time::Instant::now();
    let id = match tab(ctx, tab_id).await {
        Ok(id) => id,
        Err(e) => return e,
    };
    if !is_allowed_url(url) {
        return CommandOutcome::err(
            "navigation_blocked",
            format!("Blocked URL: {url}(仅允许 http/https)"),
            elapsed(&started),
        );
    }
    if let Err(e) = ctx.cdp(&id, "Page.navigate", json!({"url": url})).await {
        return e;
    }
    // settle:等 loadEventFired 轮询 document.readyState(与 ZCode settle 等价)
    let deadline = tokio::time::Instant::now() + Duration::from_millis(NAVIGATE_SETTLE_MS);
    loop {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let ready = ctx
            .eval_json(
                &id,
                r#"(function(){return JSON.stringify({state:document.readyState,url:location.href});})()"#,
            )
            .await;
        if let Ok(value) = ready {
            let state = value.get("state").and_then(Value::as_str).unwrap_or("");
            let landed = value.get("url").and_then(Value::as_str).unwrap_or("");
            if (state == "complete" || state == "interactive") && !landed.is_empty() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    state_after(ctx, &id, started).await
}

type ResolveResult = Result<(f64, f64), CommandOutcome>;

/// ref 或坐标 → 页面坐标中心。
async fn resolve_point(
    ctx: &mut Ctx<'_>,
    tab_id: &str,
    reference: Option<&str>,
    x: Option<f64>,
    y: Option<f64>,
) -> ResolveResult {
    if let Some(reference) = reference {
        return resolve_ref(ctx, tab_id, reference).await;
    }
    match (x, y) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err(CommandOutcome::err(
            "bad_arguments",
            "需要 ref 或 x/y 坐标",
            0,
        )),
    }
}

async fn resolve_ref(ctx: &mut Ctx<'_>, tab_id: &str, reference: &str) -> ResolveResult {
    let value = ctx
        .eval_json(tab_id, &scripts::resolve_ref_center_js(reference))
        .await?;
    if value.get("found").and_then(Value::as_bool) != Some(true) {
        let reason = value
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("stale_ref");
        return Err(CommandOutcome::err(
            "ref_not_found",
            format!("ref {reference} 不可用({reason};请先重新 snapshot)"),
            0,
        ));
    }
    let cx = value.get("cx").and_then(Value::as_f64).unwrap_or(0.0);
    let cy = value.get("cy").and_then(Value::as_f64).unwrap_or(0.0);
    Ok((cx, cy))
}

/// CDP 鼠标三连(ZCode `dispatchClickAt` 同款)。
async fn dispatch_click(
    ctx: &mut Ctx<'_>,
    tab_id: &str,
    x: f64,
    y: f64,
    button: &str,
    click_count: i32,
) -> Result<(), CommandOutcome> {
    ctx.cdp(
        tab_id,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseMoved", "x": x, "y": y,
        }),
    )
    .await?;
    ctx.cdp(
        tab_id,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mousePressed", "x": x, "y": y,
            "button": button, "clickCount": click_count,
        }),
    )
    .await?;
    ctx.cdp(
        tab_id,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseReleased", "x": x, "y": y,
            "button": button, "clickCount": click_count,
        }),
    )
    .await?;
    Ok(())
}

/// 键盘单击(ZCode `dispatchKey` 同款;常用键补 windowsVirtualKeyCode)。
async fn dispatch_key(ctx: &mut Ctx<'_>, tab_id: &str, key: &str) -> Result<(), CommandOutcome> {
    let (code, vk) = key_code(key);
    let mut params = json!({"key": key});
    if let Some(code) = code {
        params["code"] = Value::String(code.to_string());
    }
    if let Some(vk) = vk {
        params["windowsVirtualKeyCode"] = json!(vk);
    }
    let mut down = params.clone();
    down["type"] = Value::String("keyDown".into());
    ctx.cdp(tab_id, "Input.dispatchKeyEvent", down).await?;
    let mut up = params;
    up["type"] = Value::String("keyUp".into());
    ctx.cdp(tab_id, "Input.dispatchKeyEvent", up).await?;
    Ok(())
}

fn key_code(key: &str) -> (Option<&'static str>, Option<i64>) {
    match key {
        "Enter" => (Some("Enter"), Some(13)),
        "Tab" => (Some("Tab"), Some(9)),
        "Escape" => (Some("Escape"), Some(27)),
        "Backspace" => (Some("Backspace"), Some(8)),
        "Delete" => (Some("Delete"), Some(46)),
        "ArrowUp" => (Some("ArrowUp"), Some(38)),
        "ArrowDown" => (Some("ArrowDown"), Some(40)),
        "ArrowLeft" => (Some("ArrowLeft"), Some(37)),
        "ArrowRight" => (Some("ArrowRight"), Some(39)),
        "Home" => (Some("Home"), Some(36)),
        "End" => (Some("End"), Some(35)),
        "PageUp" => (Some("PageUp"), Some(33)),
        "PageDown" => (Some("PageDown"), Some(34)),
        "Space" | " " => (Some("Space"), Some(32)),
        "a" | "A" => (Some("KeyA"), Some(65)),
        "c" | "C" => (Some("KeyC"), Some(67)),
        "v" | "V" => (Some("KeyV"), Some(86)),
        "x" | "X" => (Some("KeyX"), Some(88)),
        _ => (None, None),
    }
}

async fn dispatch_wheel(
    ctx: &mut Ctx<'_>,
    tab_id: &str,
    x: f64,
    y: f64,
    delta_x: f64,
    delta_y: f64,
) -> Result<(), CommandOutcome> {
    ctx.cdp(
        tab_id,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseWheel", "x": x, "y": y,
            "deltaX": delta_x, "deltaY": delta_y,
        }),
    )
    .await
    .map(|_| ())
}

/// 拖拽:10 步插值(ZCode `dispatchDrag` 同款)。
async fn dispatch_drag(
    ctx: &mut Ctx<'_>,
    tab_id: &str,
    from: (f64, f64),
    to: (f64, f64),
) -> Result<(), CommandOutcome> {
    ctx.cdp(
        tab_id,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseMoved", "x": from.0, "y": from.1,
        }),
    )
    .await?;
    ctx.cdp(
        tab_id,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mousePressed", "x": from.0, "y": from.1,
            "button": "left", "clickCount": 1,
        }),
    )
    .await?;
    let steps = 10;
    for step in 1..=steps {
        let x = from.0 + (to.0 - from.0) * step as f64 / steps as f64;
        let y = from.1 + (to.1 - from.1) * step as f64 / steps as f64;
        ctx.cdp(
            tab_id,
            "Input.dispatchMouseEvent",
            json!({
                "type": "mouseMoved", "x": x, "y": y, "button": "left", "buttons": 1,
            }),
        )
        .await?;
    }
    ctx.cdp(
        tab_id,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseReleased", "x": to.0, "y": to.1,
            "button": "left", "clickCount": 1,
        }),
    )
    .await?;
    Ok(())
}

async fn select_all(ctx: &mut Ctx<'_>, tab_id: &str) -> Result<(), CommandOutcome> {
    // Ctrl+A(与 ZCode press 组合键一致)
    let ctrl = json!({"modifiers": 2});
    let mut down = ctrl.clone();
    down["type"] = Value::String("keyDown".into());
    down["key"] = Value::String("a".into());
    down["code"] = Value::String("KeyA".into());
    down["windowsVirtualKeyCode"] = json!(65);
    ctx.cdp(tab_id, "Input.dispatchKeyEvent", down).await?;
    let mut up = ctrl;
    up["type"] = Value::String("keyUp".into());
    up["key"] = Value::String("a".into());
    up["code"] = Value::String("KeyA".into());
    up["windowsVirtualKeyCode"] = json!(65);
    ctx.cdp(tab_id, "Input.dispatchKeyEvent", up)
        .await
        .map(|_| ())
}

async fn paste(ctx: &mut Ctx<'_>, tab_id: &str, text: &str) -> Result<(), CommandOutcome> {
    let expression = scripts::paste_text_js(text);
    let value = ctx.eval_json(tab_id, &expression).await?;
    if value.get("ok").and_then(Value::as_bool) != Some(true) {
        let message = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("paste failed");
        return Err(CommandOutcome::err("execution_error", message, 0));
    }
    Ok(())
}

async fn type_text(
    ctx: &mut Ctx<'_>,
    tab_id: Option<&str>,
    reference: Option<&str>,
    text: &str,
) -> CommandOutcome {
    let started = std::time::Instant::now();
    let id = match tab(ctx, tab_id).await {
        Ok(id) => id,
        Err(e) => return e,
    };
    if let Some(reference) = reference {
        let point = match resolve_ref(ctx, &id, reference).await {
            Ok(point) => point,
            Err(e) => return e,
        };
        if let Err(e) = dispatch_click(ctx, &id, point.0, point.1, "left", 1).await {
            return e;
        }
    }
    if let Err(e) = paste(ctx, &id, text).await {
        return e;
    }
    state_after(ctx, &id, started).await
}

async fn select_option(
    ctx: &mut Ctx<'_>,
    tab_id: Option<&str>,
    reference: &str,
    values: &[String],
) -> CommandOutcome {
    let started = std::time::Instant::now();
    let id = match tab(ctx, tab_id).await {
        Ok(id) => id,
        Err(e) => return e,
    };
    let selected = serde_json::to_string(values).unwrap_or_default();
    let expression = format!(
        r#"(function(){{
var ref={ref_json};var values={selected};
var map=window.__zcodeRefs;var el=map?map.get(ref):null;
if(!el||!el.isConnected)return JSON.stringify({{ok:false,error:'stale_ref'}});
if(el.tagName!=='SELECT')return JSON.stringify({{ok:false,error:'not_select'}});
for(var i=0;i<el.options.length;i++){{
  el.options[i].selected=values.indexOf(el.options[i].value)!==-1||values.indexOf(el.options[i].text)!==-1;
}}
el.dispatchEvent(new Event('change',{{bubbles:true}}));
return JSON.stringify({{ok:true}});
}})()"#,
        ref_json = serde_json::to_string(reference).unwrap(),
        selected = selected,
    );
    match ctx.eval_json(&id, &expression).await {
        Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => {
            state_after(ctx, &id, started).await
        }
        Ok(value) => {
            let message = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("select failed");
            CommandOutcome::err("execution_error", message, elapsed(&started))
        }
        Err(e) => e,
    }
}

async fn check_element(
    ctx: &mut Ctx<'_>,
    tab_id: Option<&str>,
    reference: &str,
    checked: Option<bool>,
) -> CommandOutcome {
    let started = std::time::Instant::now();
    let id = match tab(ctx, tab_id).await {
        Ok(id) => id,
        Err(e) => return e,
    };
    let want = checked
        .map(|b| b.to_string())
        .unwrap_or_else(|| "null".into());
    let expression = format!(
        r#"(function(){{
var ref={ref_json};var want={want};
var map=window.__zcodeRefs;var el=map?map.get(ref):null;
if(!el||!el.isConnected)return JSON.stringify({{ok:false,error:'stale_ref'}});
if(typeof el.checked!=='boolean')return JSON.stringify({{ok:false,error:'not_checkable'}});
if(want===null)el.checked=!el.checked;else el.checked=want;
el.dispatchEvent(new Event('change',{{bubbles:true}}));
return JSON.stringify({{ok:true,checked:el.checked}});
}})()"#,
        ref_json = serde_json::to_string(reference).unwrap(),
        want = want,
    );
    match ctx.eval_json(&id, &expression).await {
        Ok(value) if value.get("ok").and_then(Value::as_bool) == Some(true) => {
            state_after(ctx, &id, started).await
        }
        Ok(value) => {
            let message = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("check failed");
            CommandOutcome::err("execution_error", message, elapsed(&started))
        }
        Err(e) => e,
    }
}

async fn wait_for(
    ctx: &mut Ctx<'_>,
    tab_id: Option<&str>,
    selector: Option<&str>,
    text: Option<&str>,
    text_gone: Option<&str>,
    timeout_ms: Option<u64>,
) -> CommandOutcome {
    let started = std::time::Instant::now();
    let id = match tab(ctx, tab_id).await {
        Ok(id) => id,
        Err(e) => return e,
    };
    let timeout = timeout_ms.unwrap_or(DEFAULT_WAIT_MS).min(60_000);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout);
    loop {
        let mut satisfied = true;
        if let Some(selector_text) = selector {
            let expression = format!(
                r#"(function(){{
try{{return JSON.stringify({{hit:!!document.querySelector({sel})}});}}
catch(e){{return JSON.stringify({{hit:false,error:'bad_selector'}});}}}})()"#,
                sel = serde_json::to_string(selector_text).unwrap(),
            );
            match ctx.eval_json(&id, &expression).await {
                Ok(value) if value.get("hit").and_then(Value::as_bool) == Some(true) => {}
                _ => satisfied = false,
            }
        }
        if satisfied {
            if let Some(needle) = text
                && !body_contains(ctx, &id, needle).await.unwrap_or(false)
            {
                satisfied = false;
            }
        }
        if satisfied {
            if let Some(needle) = text_gone
                && body_contains(ctx, &id, needle).await.unwrap_or(true)
            {
                satisfied = false;
            }
        }
        if satisfied {
            return CommandOutcome::ok_value(json!({"waited": true}), elapsed(&started));
        }
        if tokio::time::Instant::now() >= deadline {
            return CommandOutcome::err(
                "timeout",
                format!("等待条件超时({timeout}ms)"),
                elapsed(&started),
            );
        }
        tokio::time::sleep(Duration::from_millis(WAIT_POLL_MS)).await;
    }
}

async fn body_contains(ctx: &mut Ctx<'_>, tab_id: &str, needle: &str) -> Option<bool> {
    let expression = scripts::wait_text_js(needle, true);
    let value = ctx.eval_json(tab_id, &expression).await.ok()?;
    value.get("present").and_then(Value::as_bool)
}

/// 截图:captureBeyondViewport + CSS 像素校正(移植 ZCode `captureScreenshotWithCssPixelCorrection` 的骨架)。
async fn screenshot(
    ctx: &mut Ctx<'_>,
    tab_id: Option<&str>,
    full_page: Option<bool>,
    clip: Option<crate::ClipRect>,
) -> CommandOutcome {
    let started = std::time::Instant::now();
    let id = match tab(ctx, tab_id).await {
        Ok(id) => id,
        Err(e) => return e,
    };
    let mut params = json!({"format": "png", "captureBeyondViewport": full_page.unwrap_or(false) || clip.is_some()});
    if let Some(rect) = clip {
        params["clip"] = json!({
            "x": rect.x, "y": rect.y,
            "width": rect.width, "height": rect.height, "scale": 1,
        });
    }
    match ctx.cdp(&id, "Page.captureScreenshot", params).await {
        Ok(result) => {
            let data = result
                .get("data")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if data.is_empty() {
                return CommandOutcome::err(
                    "execution_error",
                    "screenshot returned empty data",
                    elapsed(&started),
                );
            }
            CommandOutcome {
                image: Some(crate::ImagePayload {
                    base64: data,
                    mime_type: "image/png".into(),
                }),
                ..CommandOutcome::ok_value(Value::Null, elapsed(&started))
            }
        }
        Err(e) => e,
    }
}

async fn handle_dialog(
    ctx: &mut Ctx<'_>,
    tab_id: Option<&str>,
    accept: bool,
    prompt_text: Option<&str>,
) -> CommandOutcome {
    let started = std::time::Instant::now();
    let id = match tab(ctx, tab_id).await {
        Ok(id) => id,
        Err(e) => return e,
    };
    let mut params = json!({"accept": accept});
    if let Some(text) = prompt_text {
        params["promptText"] = Value::String(text.to_string());
    }
    let result = ctx.cdp(&id, "Page.handleJavaScriptDialog", params).await;
    ctx.manager.clear_dialog(&id);
    match result {
        Ok(_) => {
            ctx.manager
                .broadcast(crate::manager::BrowserEvent::DialogClosed { tab_id: id });
            CommandOutcome::ok_value(json!({"handled": true}), elapsed(&started))
        }
        Err(e) => e,
    }
}

// scheme 解析(避免引入 url crate):取 "xxx:" 前缀判断
mod url {
    pub fn parse_scheme(url: &str) -> Option<&str> {
        let index = url.find(':')?;
        let scheme = &url[..index];
        if scheme.is_empty()
            || !scheme
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            || !scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
        {
            return None;
        }
        Some(scheme)
    }
}

/// 取响应体:Network.getResponseBody;二进制按 base64 标记返回。
async fn network_get_body(
    ctx: &mut Ctx<'_>,
    tab_id: Option<&str>,
    request_id: &str,
    want_request: bool,
) -> CommandOutcome {
    let started = std::time::Instant::now();
    let id = match tab(ctx, tab_id).await {
        Ok(id) => id,
        Err(e) => return e,
    };
    // 条目必须在缓冲里(拿 mime/postData 判断)。
    let Some(entry) = ctx.manager.network_entry(&id, request_id) else {
        return CommandOutcome::err(
            "not_found",
            format!("requestId {request_id} 不在网络缓冲里(可能已被环形淘汰;重新 networkList)"),
            elapsed(&started),
        );
    };
    // 请求体:直接用缓冲里记录的 postData(原样文本,不含 base64)。
    if want_request {
        let post = entry.get("postData").and_then(Value::as_str).unwrap_or("");
        if post.is_empty() {
            return CommandOutcome::err(
                "no_post_data",
                format!("请求 {request_id} 没有 POST body(GET 或浏览器未上报)"),
                elapsed(&started),
            );
        }
        let truncated = post.len() > 60_000;
        let content = if truncated {
            post[..60_000].to_string()
        } else {
            post.to_string()
        };
        return CommandOutcome::ok_value(
            json!({
                "requestId": request_id,
                "kind": "request",
                "truncated": truncated,
                "body": content,
            }),
            elapsed(&started),
        );
    }
    let body = match ctx
        .cdp(
            &id,
            "Network.getResponseBody",
            json!({ "requestId": request_id }),
        )
        .await
    {
        Ok(body) => body,
        Err(e) => return e,
    };
    let base64_encoded = body
        .get("base64Encoded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let raw_body = body
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let mime = entry
        .get("mimeType")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let truncated = raw_body.len() > 60_000;
    let content = if truncated {
        raw_body[..60_000].to_string()
    } else {
        raw_body
    };
    CommandOutcome::ok_value(
        json!({
            "requestId": request_id,
            "kind": "response",
            "mimeType": mime,
            "base64Encoded": base64_encoded,
            "truncated": truncated,
            "body": content,
        }),
        elapsed(&started),
    )
}
