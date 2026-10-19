//! 端到端验证:CDP 版浏览器中枢能否真正驱动浏览器(系统 Chrome/Edge)。
//!
//! 覆盖全部命令面的主要路径:
//! - 外网路径:navigate/snapshot(ref)/click/截图/多 tab/evaluate/状态;
//! - 本地 fixture(交互/弹窗/网络/视口/画面,不依赖外网);
//! - tab 归属清理。
//!
//! 需要本机安装 Chrome 或 Edge(Windows 10/11 自带):
//! `cargo test -p denia-browser --test e2e -- --ignored --nocapture --test-threads=1`

use std::path::PathBuf;
use std::time::Duration;

use denia_browser::{BrowserCommand, BrowserManager};

/// 临时 home(测试隔离,不碰真实 profile)。
fn temp_home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("denia-browser-e2e-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// 从 outcome 里取 value,失败则 panic 并带上错误。
fn value_of(outcome: &denia_browser::CommandOutcome, what: &str) -> serde_json::Value {
    assert!(
        outcome.ok,
        "{what} 失败: {:?}",
        outcome.error.as_ref().map(|e| &e.message)
    );
    outcome.value.clone().unwrap_or(serde_json::Value::Null)
}

/// 求值便捷:执行一段 JS 并返回 JSON。
async fn eval(manager: &BrowserManager, script: &str) -> serde_json::Value {
    let outcome = manager
        .execute(BrowserCommand::Evaluate {
            tab_id: None,
            expression: script.to_string(),
        })
        .await;
    value_of(&outcome, "evaluate")
}

/// 交互 fixture 页(本地 HTTP 服务,离线可跑)。
const FIXTURE_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><title>denia-e2e</title></head>
<body>
<h1 id="title">denia e2e</h1>
<input id="name" type="text" aria-label="name field">
<button id="greet">greet</button>
<p id="out">-</p>
<select id="pick"><option value="a">A</option><option value="b">B</option></select>
<input id="agree" type="checkbox">
<button id="alert">alert</button>
<div id="hoverbox" style="width:120px;height:50px;background:#ddd">hover</div>
<div id="drop" style="width:220px;height:90px;background:#eee">drop</div>
<div id="drag" style="width:90px;height:44px;background:#ccc">drag me</div>
<div style="height:1200px"></div>
<p id="footer">bottom</p>
<script>
  document.getElementById('greet').addEventListener('click', () => {
    document.getElementById('out').textContent = 'hello';
  });
  document.getElementById('alert').addEventListener('click', () => alert('hi from fixture'));
  window.__events = { downs: 0, ups: 0, hovers: 0, lastDown: null, lastUp: null };
  document.addEventListener('mousedown', (e) => { window.__events.downs++; window.__events.lastDown = [e.clientX, e.clientY]; });
  document.addEventListener('mouseup', (e) => { window.__events.ups++; window.__events.lastUp = [e.clientX, e.clientY]; });
  document.getElementById('hoverbox').addEventListener('mouseover', () => { window.__events.hovers++; });
  fetch('/api');
</script>
</body></html>"#;

/// 起一个最小 HTTP 服务(固定 HTML;/api 返回 JSON),返回 base URL。
async fn spawn_fixture() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fixture server");
    let addr = listener.local_addr().expect("fixture addr");
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buffer = [0u8; 2048];
                let _ = socket.read(&mut buffer).await;
                let request = String::from_utf8_lossy(&buffer);
                let (content_type, body) = if request.contains("/api") {
                    ("application/json", r#"{"ok":true,"source":"fixture"}"#)
                } else {
                    ("text/html; charset=utf-8", FIXTURE_HTML)
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    format!("http://127.0.0.1:{}", addr.port())
}

#[tokio::test]
#[ignore = "启动真实浏览器(系统 Chrome/Edge),手动运行"]
async fn drives_a_real_browser_end_to_end() {
    let manager = BrowserManager::new(temp_home("basic"));

    // 1. 导航到稳定页面:用 example.com(无外网时跳过)。
    let outcome = manager
        .execute(BrowserCommand::Navigate {
            url: "https://example.com".to_string(),
            tab_id: None,
        })
        .await;
    if !outcome.ok {
        eprintln!(
            "跳过:导航失败(可能是无网络): {:?}",
            outcome.error.as_ref().map(|e| &e.message)
        );
        return;
    }
    let value = value_of(&outcome, "navigate");
    println!("导航结果: {value}");
    assert!(
        value["url"]
            .as_str()
            .unwrap_or_default()
            .contains("example.com"),
        "导航应返回目标 URL: {value}"
    );
    if let Some(status) = value["status"].as_i64() {
        assert_eq!(status, 200, "example.com 应返回 200");
    }

    // 2. 快照:应产出带 [ref=eN] 的 aria 树。
    let outcome = manager
        .execute(BrowserCommand::Snapshot {
            tab_id: None,
            max_elements: None,
            include_hidden: None,
        })
        .await;
    let value = value_of(&outcome, "snapshot");
    let elements = value["elements"].as_str().unwrap_or_default().to_string();
    let preview: String = elements.chars().take(300).collect();
    println!("快照(前 300 字):\n{preview}");
    assert!(
        elements.contains("[ref="),
        "AI 模式快照应包含 [ref=eN] 标记,实际: {elements}"
    );

    // 3. 用 ref 点击页面里的链接(ref 机制的核心验证)。
    let clickable_ref = elements
        .lines()
        .find(|line| {
            (line.contains("link ") || line.contains("button ")) && line.contains("[ref=")
        })
        .and_then(|line| {
            let start = line.find("[ref=")? + 5;
            let rest = &line[start..];
            Some(rest.split(']').next()?.to_string())
        })
        .or_else(|| {
            elements
                .split("[ref=")
                .nth(1)
                .and_then(|rest| rest.split(']').next())
                .map(str::to_string)
        });
    let reference = clickable_ref.expect("快照里应至少有一个 link/button 元素");
    println!("尝试点击 ref: {reference}");
    let outcome = manager
        .execute(BrowserCommand::Click {
            tab_id: None,
            r#ref: Some(reference.clone()),
            locator: None,
            x: None,
            y: None,
            button: None,
            double_click: None,
        })
        .await;
    println!(
        "点击结果 ok={} value={:?} error={:?}",
        outcome.ok,
        outcome.value,
        outcome.error.as_ref().map(|e| &e.message)
    );
    assert!(
        outcome.ok,
        "用 snapshot 产出的 ref 点击可交互元素应成功;ref={reference}, error={:?}",
        outcome.error.as_ref().map(|e| &e.message)
    );

    // 点击链接可能触发导航:等页面稳定(容忍导航窗口,让后续断言更稳)。
    let _ = manager
        .execute(BrowserCommand::WaitFor {
            tab_id: None,
            selector: None,
            text: Some("Example".to_string()),
            text_gone: None,
            timeout_ms: Some(8000),
        })
        .await;

    // 4. 截图:应返回 base64 PNG。
    let outcome = manager
        .execute(BrowserCommand::Screenshot {
            tab_id: None,
            full_page: Some(false),
            clip: None,
        })
        .await;
    assert!(outcome.ok, "截图应成功: {:?}", outcome.error);
    let image = outcome.image.expect("截图应返回 image 载荷");
    assert_eq!(image.mime_type, "image/png");
    assert!(image.base64.len() > 1000, "PNG base64 应有实际内容");
    println!("截图 base64 长度: {}", image.base64.len());

    // 5. 多 tab:新建 → 列表应见 2 个 → 关闭。
    let outcome = manager
        .execute(BrowserCommand::NewTab {
            url: Some("https://example.com".to_string()),
        })
        .await;
    let value = value_of(&outcome, "newTab");
    let new_tab = value["tabId"].as_str().unwrap_or_default().to_string();
    assert!(!new_tab.is_empty(), "newTab 应返回 tabId");

    let outcome = manager.execute(BrowserCommand::List).await;
    let value = value_of(&outcome, "list");
    let tabs = value["tabs"].as_array().cloned().unwrap_or_default();
    println!("当前标签页数: {}", tabs.len());
    assert!(tabs.len() >= 2, "应至少有 2 个标签页");

    let outcome = manager
        .execute(BrowserCommand::Close {
            tab_id: Some(new_tab.clone()),
        })
        .await;
    assert!(outcome.ok, "关闭标签页应成功: {:?}", outcome.error);

    // 6. 求值:页面 JS 逃生口。
    let value = eval(&manager, "() => document.title").await;
    println!("页面标题: {value}");
    assert!(!value.is_null(), "标题应可取到,实际: {value}");

    // 7. 状态快照(面板轮询用)。
    let state = manager.snapshot_state().await;
    println!("面板状态: {state}");
    assert_eq!(state["running"], true);

    println!("==> 端到端全部通过");
}

/// 本地 fixture 上的完整交互路径(离线可跑,覆盖大部分命令)。
#[tokio::test]
#[ignore = "启动真实浏览器(系统 Chrome/Edge),手动运行"]
async fn covers_interactions_on_local_fixture() {
    /// 从快照文本里按标签找一个 ref。
    fn ref_of(elements: &str, label: &str) -> String {
        elements
            .lines()
            .find(|line| line.contains(label) && line.contains("[ref="))
            .and_then(|line| {
                let start = line.find("[ref=")? + 5;
                Some(line[start..].split(']').next()?.to_string())
            })
            .unwrap_or_else(|| panic!("快照里找不到 {label}: {elements}"))
    }

    let base = spawn_fixture().await;
    let manager = BrowserManager::new(temp_home("interactions"));

    // navigate
    let outcome = manager
        .execute(BrowserCommand::Navigate {
            url: base.clone(),
            tab_id: None,
        })
        .await;
    let value = value_of(&outcome, "navigate");
    assert!(
        value["url"]
            .as_str()
            .unwrap_or_default()
            .contains("127.0.0.1"),
        "导航应返回目标 URL: {value}"
    );

    // snapshot:建立 ref 表
    let outcome = manager
        .execute(BrowserCommand::Snapshot {
            tab_id: None,
            max_elements: None,
            include_hidden: None,
        })
        .await;
    let value = value_of(&outcome, "snapshot");
    let elements = value["elements"].as_str().unwrap_or_default().to_string();
    assert!(elements.contains("[ref="), "快照应包含 ref: {elements}");
    println!("fixture 快照:\n{elements}");

    // fill:整体替换
    let name_ref = ref_of(&elements, "name field");
    let outcome = manager
        .execute(BrowserCommand::Fill {
            tab_id: None,
            r#ref: name_ref.clone(),
            value: "denia".to_string(),
        })
        .await;
    assert!(outcome.ok, "fill 应成功: {:?}", outcome.error);
    let value = eval(&manager, "() => document.getElementById('name').value").await;
    assert_eq!(value.as_str(), Some("denia"), "fill 后应写入值");

    // type:追加
    let outcome = manager
        .execute(BrowserCommand::Type {
            tab_id: None,
            r#ref: Some(name_ref.clone()),
            text: "123".to_string(),
        })
        .await;
    assert!(outcome.ok, "type 应成功: {:?}", outcome.error);
    let value = eval(&manager, "() => document.getElementById('name').value").await;
    assert_eq!(value.as_str(), Some("denia123"), "type 应追加文本");

    // press:普通键与组合键
    let outcome = manager
        .execute(BrowserCommand::Press {
            tab_id: None,
            key: "Enter".to_string(),
            r#ref: Some(name_ref.clone()),
        })
        .await;
    assert!(outcome.ok, "press Enter 应成功: {:?}", outcome.error);
    let outcome = manager
        .execute(BrowserCommand::Press {
            tab_id: None,
            key: "Control+A".to_string(),
            r#ref: Some(name_ref.clone()),
        })
        .await;
    assert!(outcome.ok, "组合键 press 应成功: {:?}", outcome.error);

    // click:ref 路径
    let greet_ref = ref_of(&elements, "greet");
    let outcome = manager
        .execute(BrowserCommand::Click {
            tab_id: None,
            r#ref: Some(greet_ref),
            locator: None,
            x: None,
            y: None,
            button: None,
            double_click: None,
        })
        .await;
    assert!(outcome.ok, "ref 点击应成功: {:?}", outcome.error);
    let value = eval(&manager, "() => document.getElementById('out').textContent").await;
    assert_eq!(value.as_str(), Some("hello"), "点击后应更新文本");

    // click:坐标路径
    let outcome = manager
        .execute(BrowserCommand::Click {
            tab_id: None,
            r#ref: None,
            locator: None,
            x: Some(5.0),
            y: Some(5.0),
            button: None,
            double_click: None,
        })
        .await;
    assert!(outcome.ok, "坐标点击应成功: {:?}", outcome.error);

    // select:locator(CSS)路径
    let outcome = manager
        .execute(BrowserCommand::Select {
            tab_id: None,
            r#ref: "#pick".to_string(),
            values: vec!["b".to_string()],
        })
        .await;
    let value = value_of(&outcome, "select");
    assert_eq!(value["selected"][0].as_str(), Some("b"), "应选中 b");
    let value = eval(&manager, "() => document.getElementById('pick').value").await;
    assert_eq!(value.as_str(), Some("b"), "select 后值应为 b");

    // check
    let outcome = manager
        .execute(BrowserCommand::Check {
            tab_id: None,
            r#ref: "#agree".to_string(),
            checked: Some(true),
        })
        .await;
    let value = value_of(&outcome, "check");
    assert_eq!(value["checked"].as_bool(), Some(true));
    let value = eval(&manager, "() => document.getElementById('agree').checked").await;
    assert_eq!(value.as_bool(), Some(true), "复选框应被勾选");

    // hover
    let outcome = manager
        .execute(BrowserCommand::Hover {
            tab_id: None,
            r#ref: Some("#hoverbox".to_string()),
            x: None,
            y: None,
        })
        .await;
    assert!(outcome.ok, "hover 应成功: {:?}", outcome.error);
    let value = eval(&manager, "() => window.__events.hovers").await;
    assert!(value.as_i64().unwrap_or(0) >= 1, "hover 应触发 mouseover");

    // scroll:元素滚入视图
    let outcome = manager
        .execute(BrowserCommand::Scroll {
            tab_id: None,
            r#ref: Some("#footer".to_string()),
            x: None,
            y: None,
            delta_x: None,
            delta_y: None,
        })
        .await;
    assert!(outcome.ok, "scroll 到元素应成功: {:?}", outcome.error);
    let value = eval(&manager, "() => window.scrollY").await;
    assert!(
        value.as_f64().unwrap_or(0.0) > 100.0,
        "滚动后 scrollY 应增大: {value}"
    );

    // scroll:滚轮
    let before = eval(&manager, "() => window.scrollY").await;
    let outcome = manager
        .execute(BrowserCommand::Scroll {
            tab_id: None,
            r#ref: None,
            x: Some(200.0),
            y: Some(200.0),
            delta_x: None,
            delta_y: Some(200.0),
        })
        .await;
    assert!(outcome.ok, "滚轮应成功: {:?}", outcome.error);
    let after = eval(&manager, "() => window.scrollY").await;
    assert!(
        after.as_f64().unwrap_or(0.0) >= before.as_f64().unwrap_or(0.0),
        "滚轮后 scrollY 不应回退: {before} -> {after}"
    );

    // drag:ref → ref
    let outcome = manager
        .execute(BrowserCommand::Drag {
            tab_id: None,
            from_ref: Some("#drag".to_string()),
            to_ref: Some("#drop".to_string()),
            from: None,
            to: None,
        })
        .await;
    assert!(outcome.ok, "drag 应成功: {:?}", outcome.error);
    let value = eval(&manager, "() => window.__events").await;
    assert!(value["downs"].as_i64().unwrap_or(0) >= 1, "drag 应产生 mousedown");
    assert!(value["ups"].as_i64().unwrap_or(0) >= 1, "drag 应产生 mouseup");

    // elementInfo
    let outcome = manager
        .execute(BrowserCommand::ElementInfo {
            tab_id: None,
            x: 5.0,
            y: 5.0,
        })
        .await;
    let value = value_of(&outcome, "elementInfo");
    assert!(
        value.is_object() || value.is_null(),
        "elementInfo 应返回对象或 null: {value}"
    );

    // waitFor:selector / text / textGone
    let outcome = manager
        .execute(BrowserCommand::WaitFor {
            tab_id: None,
            selector: Some("#footer".to_string()),
            text: None,
            text_gone: None,
            timeout_ms: Some(5000),
        })
        .await;
    assert!(outcome.ok, "waitFor selector 应成功: {:?}", outcome.error);
    let outcome = manager
        .execute(BrowserCommand::WaitFor {
            tab_id: None,
            selector: None,
            text: Some("bottom".to_string()),
            text_gone: None,
            timeout_ms: Some(5000),
        })
        .await;
    assert!(outcome.ok, "waitFor text 应成功: {:?}", outcome.error);
    let outcome = manager
        .execute(BrowserCommand::WaitFor {
            tab_id: None,
            selector: None,
            text: None,
            text_gone: Some("never-exists-xyz".to_string()),
            timeout_ms: Some(5000),
        })
        .await;
    assert!(outcome.ok, "waitFor textGone 应成功: {:?}", outcome.error);

    // getState
    let outcome = manager.execute(BrowserCommand::GetState { tab_id: None }).await;
    let value = value_of(&outcome, "getState");
    assert!(
        value["tabs"]
            .as_array()
            .map(|tabs| !tabs.is_empty())
            .unwrap_or(false),
        "getState 应列出标签页"
    );

    println!("==> 本地交互全部通过");
}

/// 弹窗 / 网络抓包 / 视口 / 截图 / 历史 / 画面(本地 fixture,离线可跑)。
#[tokio::test]
#[ignore = "启动真实浏览器(系统 Chrome/Edge),手动运行"]
async fn covers_dialogs_network_and_viewport() {
    let base = spawn_fixture().await;
    let manager = BrowserManager::new(temp_home("dialogs"));

    let outcome = manager
        .execute(BrowserCommand::Navigate {
            url: base.clone(),
            tab_id: None,
        })
        .await;
    assert!(outcome.ok, "导航应成功: {:?}", outcome.error);

    // 网络抓包:等 /api 请求出现(页面内 fetch 是异步的)。
    let mut api_id = None;
    for _ in 0..30 {
        let outcome = manager
            .execute(BrowserCommand::NetworkList {
                tab_id: None,
                url_filter: Some("/api".to_string()),
                max: Some(10),
            })
            .await;
        let value = value_of(&outcome, "networkList");
        let requests = value["requests"].as_array().cloned().unwrap_or_default();
        if let Some(found) = requests
            .iter()
            .find(|entry| entry["url"].as_str().unwrap_or("").contains("/api"))
            && (found.get("status").is_some() || found.get("failed").is_some())
        {
            api_id = found["requestId"].as_str().map(str::to_string);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if api_id.is_none() {
        let dump = manager
            .execute(BrowserCommand::NetworkList {
                tab_id: None,
                url_filter: None,
                max: Some(50),
            })
            .await;
        println!("网络诊断(未捕获 /api,全量条目): {:?}", dump.value);
        println!("面板状态: {}", manager.snapshot_state().await);
    }
    let api_id = api_id.expect("应捕获 /api 请求");
    println!("捕获请求: {api_id}");

    // 响应体
    let outcome = manager
        .execute(BrowserCommand::NetworkGetBody {
            tab_id: None,
            request_id: api_id.clone(),
            body_kind: None,
        })
        .await;
    let value = value_of(&outcome, "networkGetBody");
    assert!(
        value["body"].as_str().unwrap_or("").contains("fixture"),
        "响应体应来自 fixture: {value}"
    );

    // 请求体(bodyKind=request;GET 无请求体 → null 或 not_found,均为预期)
    let outcome = manager
        .execute(BrowserCommand::NetworkGetBody {
            tab_id: None,
            request_id: api_id.clone(),
            body_kind: Some("request".to_string()),
        })
        .await;
    println!("请求体查询: ok={} value={:?}", outcome.ok, outcome.value);

    // 视口设置/重置
    let outcome = manager
        .execute(BrowserCommand::ViewportSet {
            tab_id: None,
            width: 800,
            height: 600,
        })
        .await;
    assert!(outcome.ok, "设置视口应成功: {:?}", outcome.error);
    let size = eval(&manager, "() => [window.innerWidth, window.innerHeight]").await;
    let width = size[0].as_i64().unwrap_or(0);
    let height = size[1].as_i64().unwrap_or(0);
    println!("视口: {width}x{height}");
    assert!(
        (width - 800).abs() <= 2 && (height - 600).abs() <= 2,
        "视口应接近 800x600,实际 {width}x{height}"
    );

    let outcome = manager
        .execute(BrowserCommand::ViewportReset { tab_id: None })
        .await;
    assert!(outcome.ok, "重置视口应成功: {:?}", outcome.error);
    let size = eval(&manager, "() => window.innerWidth").await;
    assert!(
        size.as_i64().unwrap_or(0) > 1000,
        "重置后视口应回到默认宽度(1440),实际 {size}"
    );

    // 截图:整页 + clip
    let outcome = manager
        .execute(BrowserCommand::Screenshot {
            tab_id: None,
            full_page: Some(true),
            clip: None,
        })
        .await;
    assert!(outcome.ok, "整页截图应成功: {:?}", outcome.error);
    let image = outcome.image.as_ref().expect("应有图片");
    assert!(image.base64.len() > 1000);
    println!("整页截图 base64: {}", image.base64.len());

    let outcome = manager
        .execute(BrowserCommand::Screenshot {
            tab_id: None,
            full_page: None,
            clip: Some(denia_browser::ClipRect {
                x: 0.0,
                y: 0.0,
                width: 120.0,
                height: 80.0,
            }),
        })
        .await;
    assert!(outcome.ok, "clip 截图应成功: {:?}", outcome.error);

    // 历史:前进/后退/刷新
    let second = format!("{base}/page2");
    let outcome = manager
        .execute(BrowserCommand::Navigate {
            url: second.clone(),
            tab_id: None,
        })
        .await;
    assert!(outcome.ok, "二次导航应成功: {:?}", outcome.error);

    let outcome = manager.execute(BrowserCommand::Back { tab_id: None }).await;
    let value = value_of(&outcome, "back");
    let back_url = value["url"].as_str().unwrap_or_default();
    println!("back → {back_url}");
    assert!(!back_url.contains("page2"), "回退后不应在 page2: {back_url}");

    let outcome = manager
        .execute(BrowserCommand::Forward { tab_id: None })
        .await;
    let value = value_of(&outcome, "forward");
    assert!(
        value["url"].as_str().unwrap_or_default().contains("page2"),
        "前进后应回到 page2: {value}"
    );

    let outcome = manager
        .execute(BrowserCommand::Reload { tab_id: None })
        .await;
    assert!(outcome.ok, "刷新应成功: {:?}", outcome.error);

    // 弹窗:点击触发 alert → 竞速返回或正常返回;getDialog 确认挂起 → handleDialog
    let outcome = manager
        .execute(BrowserCommand::Click {
            tab_id: None,
            r#ref: Some("#alert".to_string()),
            locator: None,
            x: None,
            y: None,
            button: None,
            double_click: None,
        })
        .await;
    println!("alert 点击: ok={} value={:?}", outcome.ok, outcome.value);
    let outcome = manager
        .execute(BrowserCommand::GetDialog { tab_id: None })
        .await;
    let dialog = outcome.dialog.clone().unwrap_or(serde_json::Value::Null);
    assert!(!dialog.is_null(), "点击 alert 后应有挂起弹窗: {outcome:?}");
    println!("挂起弹窗: {dialog}");

    let outcome = manager
        .execute(BrowserCommand::HandleDialog {
            tab_id: None,
            accept: true,
            prompt_text: None,
        })
        .await;
    assert!(outcome.ok, "处置弹窗应成功: {:?}", outcome.error);

    // 页面恢复
    let value = eval(&manager, "() => 1 + 1").await;
    assert_eq!(value.as_i64(), Some(2), "处置弹窗后页面应恢复");

    // 画面:screencast 帧事件
    let mut receiver = manager.subscribe();
    let outcome = manager
        .execute(BrowserCommand::StartScreencast { tab_id: None })
        .await;
    assert!(outcome.ok, "开启画面应成功: {:?}", outcome.error);
    let mut got_frame = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), receiver.recv()).await {
            Ok(Ok(denia_browser::BrowserEvent::Frame { data, .. })) => {
                assert!(!data.is_empty(), "帧数据不应为空");
                got_frame = true;
                break;
            }
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => break,
            Err(_) => continue,
        }
    }
    assert!(got_frame, "screencast 应产生帧事件");

    let outcome = manager
        .execute(BrowserCommand::StopScreencast { tab_id: None })
        .await;
    assert!(outcome.ok, "停止画面应成功: {:?}", outcome.error);

    // 激活 tab
    let outcome = manager.execute(BrowserCommand::List).await;
    let tabs = value_of(&outcome, "list")["tabs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let first_tab = tabs[0]["tabId"].as_str().unwrap_or_default().to_string();
    let outcome = manager
        .execute(BrowserCommand::Activate { tab_id: first_tab })
        .await;
    assert!(outcome.ok, "激活 tab 应成功: {:?}", outcome.error);

    println!("==> 弹窗/网络/视口/截图/历史/画面全部通过");
}

/// 真实浏览器下的 **tab 归属**：每个 owner 只清理自己的 tab。
///
/// 用 data URL 不依赖外网。
#[tokio::test]
#[ignore = "启动真实浏览器(系统 Chrome/Edge),手动运行"]
async fn closes_only_the_owning_sessions_tabs() {
    let manager = BrowserManager::new(temp_home("ownership"));

    // child-a 与 child-b 各开一个 tab（归属执行）。
    let first = manager
        .execute_owned(
            BrowserCommand::NewTab {
                url: Some("data:text/html,<title>owned-a</title>a".to_string()),
            },
            Some("child-a"),
        )
        .await;
    let first_tab = value_of(&first, "child-a newTab")["tabId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!first_tab.is_empty());
    let second = manager
        .execute_owned(
            BrowserCommand::NewTab {
                url: Some("data:text/html,<title>owned-b</title>b".to_string()),
            },
            Some("child-b"),
        )
        .await;
    let second_tab = value_of(&second, "child-b newTab")["tabId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(!second_tab.is_empty());
    assert_ne!(first_tab, second_tab, "两次新建必须是两个不同的 tab");
    assert_eq!(
        manager.tab_owners_for_test().owned("child-a"),
        vec![first_tab.clone()]
    );
    assert_eq!(
        manager.tab_owners_for_test().owned("child-b"),
        vec![second_tab.clone()]
    );

    // 只关 child-a：child-b 的 tab 必须还在。
    assert_eq!(manager.close_owned("child-a").await, 1);
    let outcome = manager.execute(BrowserCommand::List).await;
    let tabs = value_of(&outcome, "list")["tabs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let ids: Vec<String> = tabs
        .iter()
        .filter_map(|tab| tab["tabId"].as_str().map(str::to_string))
        .collect();
    assert!(
        !ids.contains(&first_tab),
        "child-a 的 tab 必须已关闭：{ids:?}"
    );
    assert!(
        ids.contains(&second_tab),
        "child-b 的 tab 不得被顺手清掉：{ids:?}"
    );
    // 重复清理是幂等的；无关 owner 关不到任何东西。
    assert_eq!(manager.close_owned("child-a").await, 0);
    assert_eq!(manager.close_owned("parent").await, 0);
    let outcome = manager.execute(BrowserCommand::List).await;
    let tabs = value_of(&outcome, "list")["tabs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    // 只剩 child-b 的 tab：启动不再预建空白页（那会让「最后一个 tab 关闭即
    // 回收」永远触发不了），child-a 的已关，两者都不得残留。
    assert_eq!(tabs.len(), 1, "应只剩 child-b 的 tab:{tabs:?}");

    println!("==> tab 归属清理通过（只关自己名下的 tab）");
}

/// 回收回归:实例不得在“用完”之后留在后台。
///
/// 验的是真实代价:残留的 Chrome 占住 profile 单例锁,下一次冷启动会直接
/// 报 `ExitStatus(21)` —— 那就是“浏览器启动不了了”的根因。判定不用看控制面
/// 自说的状态,而是直接问操作系统:那个 profile 还被活进程占着吗。
#[tokio::test]
#[ignore = "启动真实浏览器(系统 Chrome/Edge),手动运行"]
async fn reclaims_browser_process_when_done() {
    let home = temp_home("reclaim");
    let profile = denia_browser::default_profile_dir(&home);
    let manager = BrowserManager::new(home.clone());

    // 1. 开两个 tab,占住 profile。
    let first = manager
        .execute(BrowserCommand::NewTab {
            url: Some("data:text/html,<title>one</title>one".to_string()),
        })
        .await;
    let first_tab = value_of(&first, "newTab one")["tabId"]
        .as_str()
        .expect("tabId")
        .to_string();
    let second = manager
        .execute(BrowserCommand::NewTab {
            url: Some("data:text/html,<title>two</title>two".to_string()),
        })
        .await;
    let second_tab = value_of(&second, "newTab two")["tabId"]
        .as_str()
        .expect("tabId")
        .to_string();
    assert!(
        denia_browser::browser_profile_held(&profile),
        "前置:实例在跑时应占住 profile"
    );
    // 正向对照:残留计数在「在跑」时必须非空 —— 否则后面那几条
    // `leftovers.is_empty()` 断言可能是过滤器写坏了的假通过。
    let running = processes_on_profile(&profile);
    assert!(
        !running.is_empty(),
        "前置:在跑时应能数到浏览器进程(否则收尾断言无判别力)"
    );
    println!("在跑的浏览器进程(含 Chrome 子进程):{running:?}");

    // 2. 还剩一个 tab 时 close 不会触发回收(不得误伤还在用的会话)。
    manager
        .execute(BrowserCommand::Close {
            tab_id: Some(first_tab.clone()),
        })
        .await;
    assert!(
        manager.is_running().await,
        "还剩 {second_tab} 未关时不得提前回收"
    );

    // 3. 最后一个 tab 关掉 → 自动回收。
    manager
        .execute(BrowserCommand::Close {
            tab_id: Some(second_tab.clone()),
        })
        .await;
    let reclaimed = wait_until(|| async { !denia_browser::browser_profile_held(&profile) }).await;
    assert!(
        reclaimed,
        "最后一个 tab 关闭后必须回收进程并释放 profile"
    );
    assert!(!manager.is_running().await, "控制面也不得再报在跑");
    // 主进程退了不算收干净:Chrome 的 gpu/network/utility/crashpad 子进程会
    // 在父进程死后继续占资源。这一步按命令行里的 profile 路径数残留。
    let leftovers = processes_on_profile(&profile);
    assert!(
        leftovers.is_empty(),
        "回收后不得有任何还挂着该 profile 的浏览器进程:{leftovers:?}"
    );

    // 4. 回收后能冷启动起来(这就是残留时报 ExitStatus(21) 的那一步)。
    let reopened = manager
        .execute(BrowserCommand::NewTab {
            url: Some("data:text/html,<title>again</title>".to_string()),
        })
        .await;
    assert!(
        reopened.ok,
        "回收后必须能再次拉起: {:?}",
        reopened.error
    );
    assert!(
        denia_browser::browser_profile_held(&profile),
        "重新拉起应重新占用 profile"
    );

    // 5. 显式回收:模型收尾靠这一条。
    let outcome = manager
        .execute(BrowserCommand::ShutdownBrowser)
        .await;
    let value = value_of(&outcome, "shutdownBrowser");
    assert_eq!(value["released"].as_bool(), Some(true), "应报回收了一个实例");
    assert!(
        wait_until(|| async { !denia_browser::browser_profile_held(&profile) }).await,
        "shutdownBrowser 之后 profile 必须自尤"
    );

    // 6. 善后命令不得反过来拉起实例:本来就没跑 → 原地返回,不开进程。
    let again = manager
        .execute(BrowserCommand::ShutdownBrowser)
        .await;
    let value = value_of(&again, "重复 shutdownBrowser");
    assert_eq!(value["released"].as_bool(), Some(false), "重复回收是空操作");
    let listed = manager.execute(BrowserCommand::List).await;
    let value = value_of(&listed, "list");
    assert_eq!(
        value["tabs"].as_array().map(Vec::len),
        Some(0),
        "未运行时 list 应返空 tab"
    );
    assert_eq!(value["running"].as_bool(), Some(false));
    manager
        .execute(BrowserCommand::Close {
            tab_id: Some("t99".to_string()),
        })
        .await;
    assert!(
        !denia_browser::browser_profile_held(&profile),
        "未运行时的 close 不得把实例拉回来"
    );
    assert!(!manager.is_running().await);
    let leftovers = processes_on_profile(&profile);
    assert!(
        leftovers.is_empty(),
        "收尾路径不得留下任何浏览器进程:{leftovers:?}"
    );

    println!("==> 回收回归全部通过(零 tab 自动回收 + 显式 shutdownBrowser + 善后不拉起 + 无残留进程)");
}

/// 还挂着这个 profile 的浏览器进程(`PID:名字`)。
///
/// 只看命令行里带该 profile 路径的进程 —— 用户自己开的 Chrome 不带这个
/// 路径,不会被误报。
#[cfg(windows)]
fn processes_on_profile(profile: &std::path::Path) -> Vec<String> {
    // 注意:不要把反斜杠双写 —— PowerShell 的 `-like` 不转义 `\`,双写后永远匹配不上。
    let marker = profile.to_string_lossy().to_string();
    let script = format!(
        "$ErrorActionPreference='Stop'; Get-CimInstance Win32_Process | Where-Object {{ $_.CommandLine -like '*{marker}*' }} | ForEach-Object {{ \"$($_.ProcessId):$($_.Name)\" }}"
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .expect("query processes");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        // 只算浏览器镜像:查询用的 powershell 自身命令行里带着同一个
        // marker,不滤会把自己数成残留。
        .filter(|line| {
            let name = line.rsplit(':').next().unwrap_or_default().to_ascii_lowercase();
            name == "chrome.exe" || name == "msedge.exe" || name.ends_with("chrome.exe") || name.ends_with("msedge.exe")
        })
        .map(str::to_string)
        .collect()
}

#[cfg(not(windows))]
fn processes_on_profile(_profile: &std::path::Path) -> Vec<String> {
    // 非 Windows 上跳过这一项:没有同一套命令行查询手段,主断言(占用释放)
    // 仍然跑。宁可少一项覆盖,不要在跨平台测试里藏假通过。
    Vec::new()
}

/// 轮询等到条件成立(最多 ~10s);回收是异步的,不能直接断言。
async fn wait_until<F, Fut>(predicate: F) -> bool
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..100 {
        if predicate().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

