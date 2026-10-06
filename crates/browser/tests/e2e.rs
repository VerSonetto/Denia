//! 端到端验证:Playwright 移植后的命令面能否真正驱动浏览器。
//!
//! 这个测试直接走 [`BrowserManager::execute`](与工具/面板同一条路径),
//! 覆盖:启动 → 导航 → 快照(ref)→ 点击/填值 → 截图 → 多 tab → 关闭。
//!
//! 需要本机已安装 Playwright 浏览器(首次运行会自动下载):
//! `cargo test -p denia-browser --test e2e -- --ignored --nocapture`

use std::path::PathBuf;

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

#[tokio::test]
#[ignore = "需要本机安装 Playwright 浏览器(约 200MB),手动运行"]
async fn drives_a_real_browser_end_to_end() {
    let manager = BrowserManager::new(temp_home("basic"));

    // 1. 导航到本地 data URL 之外的稳定页面:用 example.com(无外网时跳过)。
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
    assert_eq!(value["status"], 200, "example.com 应返回 200");

    // 2. 快照:应产出带 [ref=eN] 的 aria 树。
    let outcome = manager
        .execute(BrowserCommand::Snapshot {
            tab_id: None,
            max_elements: None,
            include_hidden: None,
        })
        .await;
    let value = value_of(&outcome, "snapshot");
    let elements = value["elements"].as_str().unwrap_or_default();
    println!("快照(前 300 字):\n{}", &elements[..elements.len().min(300)]);
    assert!(
        elements.contains("[ref="),
        "AI 模式快照应包含 [ref=eN] 标记,实际: {elements}"
    );

    // 3. 用 ref 点击页面里的链接(ref 机制的核心验证)。
    //    挑一个真正可点击的元素(link/button),而不是 generic 容器——
    //    后者可能因不可见/无尺寸而合理地过不了 Playwright 的可操作性检查。
    let clickable_ref = elements
        .lines()
        .find(|line| (line.contains("link ") || line.contains("button ")) && line.contains("[ref="))
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
    if let Some(reference) = clickable_ref {
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
    } else {
        panic!("快照里应至少有一个 link/button 元素");
    }

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
    let outcome = manager
        .execute(BrowserCommand::Evaluate {
            tab_id: None,
            expression: "() => document.title".to_string(),
        })
        .await;
    let value = value_of(&outcome, "evaluate");
    println!("页面标题: {value}");
    assert!(
        value.as_str().unwrap_or_default().contains("Example"),
        "标题应包含 Example,实际: {value}"
    );

    // 7. 状态快照(面板轮询用)。
    let state = manager.snapshot_state().await;
    println!("面板状态: {state}");
    assert_eq!(state["running"], true);

    println!("==> 端到端全部通过");
}

/// 真实浏览器下的 **tab 归属**：每个 owner 只清理自己的 tab（计划 6.4/L04）。
///
/// 用 data URL 不依赖外网。
#[tokio::test]
#[ignore = "需要本机安装 Playwright 浏览器(约 200MB),手动运行"]
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
    assert_eq!(tabs.len(), 1, "只剩 child-b 的 tab");

    println!("==> tab 归属清理通过（只关自己名下的 tab）");
}
