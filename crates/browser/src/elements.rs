//! 元素引用与交互:aria 快照(`[ref=eN]`)、定位解析(CSS/文本/引用)、
//! 以及基于 CDP 原语的点击/输入/悬停/滚动/拖拽等动作。
//!
//! # ref 体系
//!
//! - `snapshot` 遍历无障碍树(Accessibility.getFullAXTree),渲染成缩进文本,
//!   并为带 DOM 节点的条目分配 `[ref=eN]`;ref → backendNodeId 的映射由
//!   `BrowserManager` 按 tab 持有。
//! - 页面导航后映射作废(backendNodeId 在新文档里会重新分配,旧 ref 可能
//!   指向别的元素),因此导航事件会清空映射,模型需重新 snapshot。
//! - 交互命令按 ref 查 backendNodeId,再用 CDP 原语(DOM/Input)定位与操作;
//!   不往页面注入全局变量。
//! - 交互前有轻量"可操作性"等待:滚动到视图内 + 取内容四边形(不可见/零尺寸
//!   会拿不到),失败在 1 秒窗口内重试。

use std::collections::HashMap;
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::accessibility::{
    AxNode, AxNodeId, AxValue, GetFullAxTreeParams,
};
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, DescribeNodeParams, DiscardSearchResultsParams, FocusParams,
    GetContentQuadsParams, GetDocumentParams, GetSearchResultsParams, NodeId, PerformSearchParams,
    QuerySelectorParams, ResolveNodeParams, ScrollIntoViewIfNeededParams,
};
use chromiumoxide::cdp::browser_protocol::input::{
    DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams, DispatchMouseEventType,
    InsertTextParams, MouseButton,
};
use chromiumoxide::cdp::js_protocol::runtime::{CallFunctionOnParams, EvaluateParams};
use chromiumoxide::keys::get_key_definition;
use chromiumoxide::page::Page;
use serde_json::Value;

/// 点击/拖拽类动作的可操作性等待窗口。
const ACTION_RETRY_TIMES: usize = 10;
const ACTION_RETRY_INTERVAL: Duration = Duration::from_millis(100);

/// 一次快照的结果:渲染文本 + ref 映射。
pub struct Snapshot {
    pub text: String,
    pub refs: HashMap<String, i64>,
}

/// 生成页面快照(AI 模式):无障碍树 → 缩进文本 + `[ref=eN]`。
pub async fn snapshot(page: &Page) -> Result<Snapshot, String> {
    let response = page
        .execute(GetFullAxTreeParams::builder().build())
        .await
        .map_err(|error| format!("生成页面快照失败: {error}"))?;
    let nodes = response.result.nodes;
    Ok(render_ax_tree(&nodes))
}

/// 把无障碍树渲染成缩进文本并分配 ref。
fn render_ax_tree(nodes: &[AxNode]) -> Snapshot {
    let index: HashMap<String, &AxNode> = nodes
        .iter()
        .map(|node| (ax_id_string(&node.node_id), node))
        .collect();
    let mut renderer = Renderer {
        index,
        refs: HashMap::new(),
        next_ref: 1,
        lines: Vec::new(),
    };
    let root = nodes
        .iter()
        .find(|node| node.parent_id.is_none())
        .or_else(|| nodes.first());
    if let Some(root) = root {
        renderer.walk(root, 0);
    }
    Snapshot {
        text: renderer.lines.join("\n"),
        refs: renderer.refs,
    }
}

struct Renderer<'a> {
    index: HashMap<String, &'a AxNode>,
    refs: HashMap<String, i64>,
    next_ref: usize,
    lines: Vec<String>,
}

impl Renderer<'_> {
    fn walk(&mut self, node: &AxNode, depth: usize) {
        if node.ignored {
            // 被忽略的节点不输出自身,子节点保持当前层级继续。
            for child_id in node.child_ids.as_deref().unwrap_or_default() {
                if let Some(child) = self.index.get(&ax_id_string(child_id)) {
                    self.walk(child, depth);
                }
            }
            return;
        }

        let role = ax_string(node.role.as_ref()).unwrap_or_default();
        let name = ax_string(node.name.as_ref()).unwrap_or_default();
        let value = ax_string(node.value.as_ref()).unwrap_or_default();

        // 纯结构节点(无角色语义、无名字、无 DOM 落点)不出行,子节点上提。
        let structural = matches!(role.as_str(), "" | "none" | "generic" | "InlineTextBox")
            && name.is_empty()
            && node.backend_dom_node_id.is_none();

        let mut child_depth = depth;
        if !structural {
            let mut line = String::with_capacity(48);
            line.push_str(&"  ".repeat(depth));
            line.push_str("- ");
            line.push_str(if role.is_empty() { "node" } else { &role });
            if !name.is_empty() {
                line.push_str(&format!(" \"{}\"", escape_name(&name)));
            }
            if let Some(backend_id) = node.backend_dom_node_id {
                let reference = format!("e{}", self.next_ref);
                self.next_ref += 1;
                self.refs
                    .insert(reference.clone(), backend_id.inner().to_owned());
                line.push_str(&format!(" [ref={reference}]"));
            }
            if !value.is_empty() {
                line.push_str(&format!(": {}", value.replace(['\n', '\r'], " ")));
            }
            self.lines.push(line);
            child_depth = depth + 1;
        }

        for child_id in node.child_ids.as_deref().unwrap_or_default() {
            if let Some(child) = self.index.get(&ax_id_string(child_id)) {
                self.walk(child, child_depth);
            }
        }
    }
}

fn ax_id_string(id: &AxNodeId) -> String {
    serde_json::to_value(id)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

fn ax_string(value: Option<&AxValue>) -> Option<String> {
    let raw = value?.value.as_ref()?;
    match raw {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

fn escape_name(name: &str) -> String {
    name.replace(['\n', '\r'], " ")
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// 是否是快照产出的元素引用(`e` 后跟纯数字)。
pub fn is_aria_ref(target: &str) -> bool {
    let Some(digits) = target.strip_prefix('e') else {
        return false;
    };
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

/// 归一化 ref(`@e6` / ` e6 ` → `e6`)。
pub fn normalize_ref(target: &str) -> String {
    let trimmed = target.trim();
    trimmed.strip_prefix('@').unwrap_or(trimmed).to_string()
}

/// 解析交互目标(ref 或 locator),返回 backendNodeId。
pub async fn resolve_element(
    page: &Page,
    refs: &HashMap<String, i64>,
    target: &str,
) -> Result<BackendNodeId, String> {
    let trimmed = target.trim();
    let bare = normalize_ref(trimmed);
    if is_aria_ref(&bare) {
        let backend = refs
            .get(&bare)
            .ok_or_else(|| format!("元素引用 {bare} 不存在或已失效(页面导航后需重新 snapshot)"))?;
        return Ok(backend_id(*backend));
    }
    let locator = trimmed.strip_prefix("text=").map(unquote);
    match locator {
        Some(text) => find_by_text(page, &text).await,
        None => find_by_css(page, trimmed).await,
    }
}

fn unquote(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() >= 2 {
        let bytes = trimmed.as_bytes();
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

/// 用 CDP 文本搜索定位元素(`text=...` 语义,CSS 之外的补充)。
async fn find_by_text(page: &Page, text: &str) -> Result<BackendNodeId, String> {
    let search = page
        .execute(PerformSearchParams::builder().query(text).build().unwrap())
        .await
        .map_err(|error| format!("文本搜索失败({text}): {error}"))?;
    let search_id = search.search_id.clone();
    let fetch = async {
        if search.result_count <= 0 {
            return Err(format!("未找到包含文本 {text:?} 的元素"));
        }
        let results = page
            .execute(GetSearchResultsParams::new(
                search.search_id.clone(),
                0,
                search.result_count,
            ))
            .await
            .map_err(|error| format!("取文本搜索结果失败: {error}"))?;
        results
            .node_ids
            .first()
            .cloned()
            .ok_or_else(|| format!("未找到包含文本 {text:?} 的元素"))
    }
    .await;
    // 搜索会话用完即弃。
    let _ = page
        .execute(
            DiscardSearchResultsParams::builder()
                .search_id(search_id)
                .build()
                .unwrap(),
        )
        .await;
    let node_id = fetch?;
    backend_from_node_id(page, node_id).await
}

/// 用 CSS 选择器定位元素(`#id`、`[aria-label=...]`、`.class` 等)。
async fn find_by_css(page: &Page, selector: &str) -> Result<BackendNodeId, String> {
    let document = page
        .execute(GetDocumentParams::builder().build())
        .await
        .map_err(|error| format!("取文档根节点失败: {error}"))?;
    let found = page
        .execute(
            QuerySelectorParams::builder()
                .node_id(document.root.node_id)
                .selector(selector)
                .build()
                .unwrap(),
        )
        .await
        .map_err(|error| format!("选择器未命中({selector}): {error}"))?;
    backend_from_node_id(page, found.node_id).await
}

async fn backend_from_node_id(page: &Page, node_id: NodeId) -> Result<BackendNodeId, String> {
    let described = page
        .execute(DescribeNodeParams::builder().node_id(node_id).build())
        .await
        .map_err(|error| format!("取节点信息失败: {error}"))?;
    Ok(described.node.backend_node_id)
}

fn backend_id(raw: i64) -> BackendNodeId {
    BackendNodeId::new(raw)
}

/// 滚动元素到视图内(失败静默:部分元素无需滚动)。
pub async fn scroll_into_view(page: &Page, backend_node_id: BackendNodeId) -> Result<(), String> {
    page.execute(
        ScrollIntoViewIfNeededParams::builder()
            .backend_node_id(backend_node_id)
            .build(),
    )
    .await
    .map(|_| ())
    .map_err(|error| format!("滚动到元素失败: {error}"))
}

/// 元素内容四边形的中心点(不可见/零尺寸时报错)。
pub async fn element_center(
    page: &Page,
    backend_node_id: BackendNodeId,
) -> Result<(f64, f64), String> {
    let quads = page
        .execute(
            GetContentQuadsParams::builder()
                .backend_node_id(backend_node_id)
                .build(),
        )
        .await
        .map_err(|error| format!("元素不可见或没有布局: {error}"))?;
    for quad in &quads.quads {
        let points = quad.inner();
        if points.len() != 8 {
            continue;
        }
        let area = quad_area(points);
        if area <= 1.0 {
            continue;
        }
        let x = (points[0] + points[2] + points[4] + points[6]) / 4.0;
        let y = (points[1] + points[3] + points[5] + points[7]) / 4.0;
        return Ok((x, y));
    }
    Err("元素不可见或没有尺寸".to_string())
}

fn quad_area(points: &[f64]) -> f64 {
    let mut area = 0.0;
    for index in 0..4 {
        let x1 = points[index * 2];
        let y1 = points[index * 2 + 1];
        let next = (index + 1) % 4;
        let x2 = points[next * 2];
        let y2 = points[next * 2 + 1];
        area += x1 * y2 - x2 * y1;
    }
    (area / 2.0).abs()
}

/// 点击元素:滚动到视图 + 坐标点击,带可操作性重试。
pub async fn click_backend(
    page: &Page,
    backend_node_id: BackendNodeId,
    button: MouseButton,
    click_count: i64,
) -> Result<(), String> {
    let mut last_error = String::new();
    for _ in 0..ACTION_RETRY_TIMES {
        let _ = scroll_into_view(page, backend_node_id).await;
        match element_center(page, backend_node_id).await {
            Ok((x, y)) => return click_point(page, x, y, button, click_count).await,
            Err(error) => {
                last_error = error;
                tokio::time::sleep(ACTION_RETRY_INTERVAL).await;
            }
        }
    }
    Err(format!("元素不可见或不可点击: {last_error}"))
}

/// 在视口坐标上执行一次完整点击(move → press → release)。
pub async fn click_point(
    page: &Page,
    x: f64,
    y: f64,
    button: MouseButton,
    click_count: i64,
) -> Result<(), String> {
    // 右键/中键的按下状态位。
    let buttons = match button {
        MouseButton::Right => 2,
        MouseButton::Middle => 4,
        _ => 0,
    };
    let base = DispatchMouseEventParams::builder()
        .x(x)
        .y(y)
        .button(button)
        .click_count(click_count);
    page.execute(
        base.clone()
            .r#type(DispatchMouseEventType::MouseMoved)
            .build()
            .unwrap(),
    )
    .await
    .map_err(|error| format!("点击失败({x},{y}): {error}"))?;
    page.execute(
        base.clone()
            .r#type(DispatchMouseEventType::MousePressed)
            .buttons(if buttons == 0 { 1 } else { buttons })
            .build()
            .unwrap(),
    )
    .await
    .map_err(|error| format!("点击失败({x},{y}): {error}"))?;
    page.execute(
        base.r#type(DispatchMouseEventType::MouseReleased)
            .build()
            .unwrap(),
    )
    .await
    .map_err(|error| format!("点击失败({x},{y}): {error}"))?;
    Ok(())
}

/// 悬停元素(滚动到视图 + 移动鼠标)。
pub async fn hover_backend(page: &Page, backend_node_id: BackendNodeId) -> Result<(), String> {
    let _ = scroll_into_view(page, backend_node_id).await;
    let (x, y) = element_center(page, backend_node_id).await?;
    hover_point(page, x, y).await
}

/// 悬停在视口坐标。
pub async fn hover_point(page: &Page, x: f64, y: f64) -> Result<(), String> {
    page.execute(
        DispatchMouseEventParams::builder()
            .x(x)
            .y(y)
            .r#type(DispatchMouseEventType::MouseMoved)
            .build()
            .unwrap(),
    )
    .await
    .map_err(|error| format!("悬停失败({x},{y}): {error}"))?;
    Ok(())
}

/// 聚焦元素。
pub async fn focus_backend(page: &Page, backend_node_id: BackendNodeId) -> Result<(), String> {
    page.execute(
        FocusParams::builder()
            .backend_node_id(backend_node_id)
            .build(),
    )
    .await
    .map_err(|error| format!("聚焦失败: {error}"))?;
    Ok(())
}

/// 填入文本(整体替换):聚焦 + 全选 + 整体插入。
pub async fn fill_backend(
    page: &Page,
    backend_node_id: BackendNodeId,
    value: &str,
) -> Result<(), String> {
    let _ = scroll_into_view(page, backend_node_id).await;
    focus_backend(page, backend_node_id).await?;
    // 全选已有内容(input/textarea);contenteditable 退化为追加。
    let _ = call_on_element(
        page,
        backend_node_id,
        "function() { if (typeof this.select === 'function') this.select(); }",
        false,
    )
    .await;
    insert_text(page, value).await
}

/// 在当前焦点处插入文本(触发 input 事件,不模拟按键)。
pub async fn insert_text(page: &Page, text: &str) -> Result<(), String> {
    page.execute(InsertTextParams::new(text.to_string()))
        .await
        .map_err(|error| format!("输入失败: {error}"))?;
    Ok(())
}

/// 输入文本(追加):ASCII 可映射字符按真实按键输入,其余(大写/中文等)
/// 走 insertText,保证中文输入也能工作。
pub async fn type_text(page: &Page, text: &str) -> Result<(), String> {
    for character in text.chars() {
        let as_str = character.to_string();
        match char_key_definition(character) {
            Some(_) => press_key(page, &as_str).await?,
            None => insert_text(page, &as_str).await?,
        }
    }
    Ok(())
}

fn char_key_definition(character: char) -> Option<()> {
    if character.is_ascii() && get_key_definition(character.to_string()).is_some() {
        Some(())
    } else {
        None
    }
}

/// 按下并释放一个按键。支持组合键(`Control+A`、`Shift+Enter`)与常用键名
/// (`Enter`/`Tab`/`Escape`/`ArrowDown`/`a`…)。
pub async fn press_key(page: &Page, key_spec: &str) -> Result<(), String> {
    let (modifiers, key_name) = parse_key_spec(key_spec);
    let definition = lookup_key(&key_name).ok_or_else(|| format!("无法识别的按键: {key_spec}"))?;

    let mut builder = DispatchKeyEventParams::builder();
    let down_type = if let Some(text) = definition.text {
        builder = builder.text(text);
        DispatchKeyEventType::KeyDown
    } else if definition.key.len() == 1 {
        builder = builder.text(definition.key);
        DispatchKeyEventType::KeyDown
    } else {
        DispatchKeyEventType::RawKeyDown
    };
    builder = builder
        .key(definition.key)
        .code(definition.code)
        .windows_virtual_key_code(definition.key_code)
        .native_virtual_key_code(definition.key_code);
    if modifiers != 0 {
        builder = builder.modifiers(modifiers);
    }
    page.execute(builder.clone().r#type(down_type).build().unwrap())
        .await
        .map_err(|error| format!("按键失败({key_spec}): {error}"))?;
    page.execute(builder.r#type(DispatchKeyEventType::KeyUp).build().unwrap())
        .await
        .map_err(|error| format!("按键失败({key_spec}): {error}"))?;
    Ok(())
}

/// 解析 `Control+Shift+A` 形式的按键串:返回 (modifiers, 主键名)。
fn parse_key_spec(key_spec: &str) -> (i64, String) {
    let mut modifiers = 0i64;
    let mut parts: Vec<&str> = key_spec.split('+').map(str::trim).collect();
    let main = parts.pop().unwrap_or(key_spec).to_string();
    for part in parts {
        match part.to_ascii_lowercase().as_str() {
            "alt" => modifiers |= 1,
            "control" | "ctrl" => modifiers |= 2,
            "meta" | "command" | "cmd" | "win" => modifiers |= 4,
            "shift" => modifiers |= 8,
            _ => {}
        }
    }
    (modifiers, main)
}

fn lookup_key(key_name: &str) -> Option<&'static chromiumoxide::keys::KeyDefinition> {
    if let Some(found) = get_key_definition(key_name) {
        return Some(found);
    }
    // 常见别名与大小写回退。
    let alias = match key_name.to_ascii_lowercase().as_str() {
        "esc" => Some("Escape"),
        "del" => Some("Delete"),
        "ins" => Some("Insert"),
        "space" | "spacebar" => Some(" "),
        "return" => Some("Enter"),
        "arrowleft" => Some("ArrowLeft"),
        "arrowup" => Some("ArrowUp"),
        "arrowright" => Some("ArrowRight"),
        "arrowdown" => Some("ArrowDown"),
        other => {
            if other.len() == 1 {
                // 单字符按名字再查一次("A" → "a" 之类由调用方处理)。
                None
            } else {
                None
            }
        }
    };
    alias.and_then(get_key_definition)
}

/// 读取并(必要时)切换勾选状态,返回切换后的状态。
pub async fn check_backend(
    page: &Page,
    backend_node_id: BackendNodeId,
    checked: bool,
) -> Result<bool, String> {
    let state = call_on_element(
        page,
        backend_node_id,
        "function() { return !!this.checked; }",
        false,
    )
    .await
    .unwrap_or(Value::Bool(false));
    let current = state.as_bool().unwrap_or(false);
    if current != checked {
        click_backend(page, backend_node_id, MouseButton::Left, 1).await?;
    }
    Ok(checked)
}

/// 选择 `<select>` 的选项(按 value 或可见文本匹配),返回选中值列表。
pub async fn select_backend(
    page: &Page,
    backend_node_id: BackendNodeId,
    values: &[String],
) -> Result<Vec<String>, String> {
    let values_json = serde_json::to_string(values).unwrap_or_else(|_| "[]".to_string());
    let script = format!(
        "function() {{\
         const wanted = {values_json}; \
         const wantedText = wanted.map(String); \
         if (!(this instanceof HTMLSelectElement)) \
             throw new Error('目标不是 <select> 元素'); \
         const picked = []; \
         for (const option of this.options) {{ \
             const match = wantedText.includes(option.value) || wantedText.includes(option.text); \
             option.selected = match; \
             if (match) picked.push(option.value); \
         }} \
         this.dispatchEvent(new Event('input', {{ bubbles: true }})); \
         this.dispatchEvent(new Event('change', {{ bubbles: true }})); \
         return picked; \
         }}"
    );
    let result = call_on_element(page, backend_node_id, &script, false).await?;
    let picked = result
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(picked)
}

/// 拖拽:从起点坐标按下,分步移动到终点后释放。
pub async fn drag(page: &Page, from: (f64, f64), to: (f64, f64)) -> Result<(), String> {
    hover_point(page, from.0, from.1).await?;
    let down = DispatchMouseEventParams::builder()
        .x(from.0)
        .y(from.1)
        .button(MouseButton::Left)
        .buttons(1)
        .click_count(1)
        .r#type(DispatchMouseEventType::MousePressed)
        .build()
        .unwrap();
    page.execute(down)
        .await
        .map_err(|error| format!("拖拽失败(按下): {error}"))?;
    const STEPS: usize = 6;
    for step in 1..=STEPS {
        let ratio = step as f64 / STEPS as f64;
        let x = from.0 + (to.0 - from.0) * ratio;
        let y = from.1 + (to.1 - from.1) * ratio;
        hover_point(page, x, y).await?;
    }
    let up = DispatchMouseEventParams::builder()
        .x(to.0)
        .y(to.1)
        .button(MouseButton::Left)
        .buttons(0)
        .click_count(1)
        .r#type(DispatchMouseEventType::MouseReleased)
        .build()
        .unwrap();
    page.execute(up)
        .await
        .map_err(|error| format!("拖拽失败(释放): {error}"))?;
    Ok(())
}

/// 鼠标滚轮(页面滚动)。
pub async fn wheel(page: &Page, x: f64, y: f64, delta_x: f64, delta_y: f64) -> Result<(), String> {
    page.execute(
        DispatchMouseEventParams::builder()
            .x(x)
            .y(y)
            .delta_x(delta_x)
            .delta_y(delta_y)
            .r#type(DispatchMouseEventType::MouseWheel)
            .build()
            .unwrap(),
    )
    .await
    .map_err(|error| format!("滚动失败: {error}"))?;
    Ok(())
}

/// 在当前鼠标位置滚动的兜底:先移到视口中心再滚。
pub async fn wheel_at_center(page: &Page, delta_x: f64, delta_y: f64) -> Result<(), String> {
    wheel(page, 720.0, 450.0, delta_x, delta_y).await
}

/// 对元素执行一段 JS 函数(`this` 绑定为元素),返回 JSON 值。
pub async fn call_on_element(
    page: &Page,
    backend_node_id: BackendNodeId,
    function: &str,
    await_promise: bool,
) -> Result<Value, String> {
    let resolved = page
        .execute(
            ResolveNodeParams::builder()
                .backend_node_id(backend_node_id)
                .build(),
        )
        .await
        .map_err(|error| format!("解析元素失败: {error}"))?;
    let object_id = resolved
        .object
        .object_id
        .clone()
        .ok_or_else(|| "元素已失效(可能被页面变更移除)".to_string())?;
    let result = page
        .execute(
            CallFunctionOnParams::builder()
                .object_id(object_id)
                .function_declaration(function)
                .await_promise(await_promise)
                .return_by_value(true)
                .build()
                .unwrap(),
        )
        .await
        .map_err(|error| format!("元素操作失败: {error}"))?;
    Ok(result.result.result.value.unwrap_or(Value::Null))
}

/// 页面求值(函数或表达式),返回 JSON 值。
///
/// 走 `Runtime.evaluate` 且**不指定 execution context**:缓存的旧上下文 id
/// 在页面导航后会报 "Cannot find context with specified id",省略 contextId
/// 让浏览器在当前默认上下文求值,天然跟随导航。
/// 传入的函数字符串("() => ...")会被包装成"检测函数则调用"的表达式。
pub async fn evaluate(page: &Page, expression: &str) -> Result<Value, String> {
    let wrapped = format!(
        "(function() {{ const __denia_value = ({expression}); \
         return typeof __denia_value === 'function' ? __denia_value() : __denia_value; }})()"
    );
    let params = EvaluateParams::builder()
        .expression(wrapped)
        .await_promise(true)
        .return_by_value(true)
        .build()
        .unwrap();
    let response = page
        .execute(params)
        .await
        .map_err(|error| format!("求值失败: {error}"))?;
    let result = response.result;
    if let Some(exception) = result.exception_details {
        let description = serde_json::to_value(&exception)
            .ok()
            .and_then(|value| {
                value
                    .get("exception")
                    .and_then(|exception| exception.get("description"))
                    .and_then(|text| text.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| exception.text.clone());
        return Err(format!("求值失败: {description}"));
    }
    Ok(result.result.value.unwrap_or(Value::Null))
}

/// 页面标题(取不到时为空串)。
pub async fn page_title(page: &Page) -> String {
    evaluate(page, "document.title")
        .await
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// 页面 URL(取不到时为空串)。
pub async fn page_url(page: &Page) -> String {
    page.url().await.ok().flatten().unwrap_or_default()
}
