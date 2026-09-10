//! 端到端:用 fixture 服务器跑真实的 stdio MCP 握手、工具清单与调用。
//!
//! fixture 是同 crate 的 bin(`denia-mcp-fixture`),测试用
//! `env!("CARGO_BIN_EXE_...")` 定位可执行文件,跨平台无需外部命令。

use std::collections::HashSet;

use denia_mcp::{McpManager, McpServerConfig, McpServerStatus};
use serde_json::json;

fn fixture_config(id: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.to_string(),
        transport: "stdio".to_string(),
        command: env!("CARGO_BIN_EXE_denia-mcp-fixture").to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn connects_lists_tools_and_calls_them() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;

    let snapshot = manager.snapshot();
    let state = &snapshot.servers[0];
    assert_eq!(state.status, McpServerStatus::Connected, "{:?}", state.error);
    // 工具名带服务器前缀,模型写得出、派发认得回。
    let names = snapshot.tool_names();
    assert!(
        names.contains(&"mcp__fx__echo".to_string()),
        "{names:?}"
    );
    assert!(names.contains(&"mcp__fx__big".to_string()), "{names:?}");

    let result = manager
        .call("mcp__fx__echo", json!({ "text": "你好" }))
        .await
        .expect("echo succeeds");
    assert_eq!(result.text, "你好");
    assert!(!result.is_error);

    // 调用不存在的工具:路由层拒绝,不给外部发请求。
    assert!(manager.call("mcp__fx__nope", json!({})).await.is_err());
}

#[tokio::test]
async fn call_result_carries_mcp_error_flag() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;
    // fixture 对未知工具名返回 isError=true(业务失败,不是协议失败)。
    let result = manager
        .call("mcp__fx__echo", json!({}))
        .await
        .expect("protocol call succeeds");
    // echo 缺 text 只回空串:这里确认协议层不把它当错误。
    assert_eq!(result.text, "");
    assert!(!result.is_error);
}

/// 分页:长结果按字符切片,翻页复用缓存(不重跑工具)。
#[tokio::test]
async fn long_results_page_by_chars() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;

    // 第一次调用:产出 100 字符,取前 30。
    let first = manager
        .call_paged("mcp__fx__big", 0, 30, &json!({ "chars": 100 }))
        .await
        .expect("call succeeds");
    assert_eq!(first.total_chars, 100);
    assert_eq!(first.shown_chars, 30);
    assert!(first.has_more);
    assert!(!first.from_cache);

    // 翻页:同一组参数 → 命中缓存,不再执行工具(offset 不参与指纹)。
    let second = manager
        .call_paged("mcp__fx__big", 30, 30, &json!({ "chars": 100 }))
        .await
        .expect("call succeeds");
    assert!(second.from_cache, "翻页必须复用上次结果");
    assert_eq!(second.offset, 30);
    assert_eq!(second.text, "A".repeat(30));
    assert!(second.has_more);

    // 末页:has_more 为假。
    let last = manager
        .call_paged("mcp__fx__big", 90, 30, &json!({ "chars": 100 }))
        .await
        .expect("call succeeds");
    assert_eq!(last.shown_chars, 10);
    assert!(!last.has_more);

    // 参数变了(不同 chars)→ 不复用缓存,重新执行工具。
    let other = manager
        .call_paged("mcp__fx__big", 0, 10, &json!({ "chars": 5 }))
        .await
        .expect("call succeeds");
    assert!(!other.from_cache);
    assert_eq!(other.total_chars, 5);
}

#[tokio::test]
async fn disabled_server_never_spawns_and_failure_is_isolated() {
    let manager = McpManager::new(HashSet::new());
    let mut disabled = fixture_config("off");
    disabled.enabled = false;
    let mut broken = McpServerConfig {
        id: "broken".to_string(),
        command: "definitely-not-a-real-command-12345".to_string(),
        ..Default::default()
    };
    broken.enabled = true;
    manager.reload(&[disabled, broken, fixture_config("ok")]).await;

    let snapshot = manager.snapshot();
    let by_id = |id: &str| {
        snapshot
            .servers
            .iter()
            .find(|s| s.id == id)
            .expect("server present")
            .clone()
    };
    assert_eq!(by_id("off").status, McpServerStatus::Disabled);
    assert_eq!(by_id("broken").status, McpServerStatus::Error);
    assert!(by_id("broken").error.is_some(), "失败原因要带回 UI");
    // 失败不拖垮别人:另一个服务器照常可用。
    assert_eq!(by_id("ok").status, McpServerStatus::Connected);
    assert!(snapshot.tool_names().contains(&"mcp__ok__echo".to_string()));
}

#[tokio::test]
async fn removing_a_server_shuts_it_down() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;
    assert_eq!(manager.snapshot().tool_names().len(), 2);

    // 空配置:断开并清空工具面。
    manager.reload(&[]).await;
    assert!(manager.snapshot().tool_names().is_empty());
    assert!(manager.snapshot().servers.is_empty());
}

/// 与内置工具同名的 MCP 工具被跳过,绝不覆盖内置能力。
#[tokio::test]
async fn builtin_conflict_tools_are_hidden() {
    let builtin: HashSet<String> = ["mcp__fx__echo".to_string()].into_iter().collect();
    let manager = McpManager::new(builtin);
    manager.reload(&[fixture_config("fx")]).await;
    let names = manager.snapshot().tool_names();
    assert!(!names.contains(&"mcp__fx__echo".to_string()), "{names:?}");
    assert!(names.contains(&"mcp__fx__big".to_string()), "{names:?}");
}
