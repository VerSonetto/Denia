//! 真实 Context7 服务器连通性验证(默认被 ignore:需要外网)。
//!
//! 跑法:
//! ```sh
//! cargo test -p denia-mcp --test context7 -- --ignored --nocapture
//! ```
//!
//! 需要 `CONTEXT7_API_KEY` 时设环境变量;不设也能跑(限流更紧)。

use std::collections::HashSet;

use denia_mcp::{McpManager, McpServerConfig};
use serde_json::json;

fn context7_config() -> McpServerConfig {
    let mut headers = std::collections::BTreeMap::new();
    if let Ok(key) = std::env::var("CONTEXT7_API_KEY")
        && !key.trim().is_empty()
    {
        headers.insert(
            "Authorization".to_string(),
            format!("Bearer {}", key.trim()),
        );
    }
    McpServerConfig {
        id: "context7".to_string(),
        transport: "http".to_string(),
        url: Some("https://mcp.context7.com/mcp".to_string()),
        headers,
        call_timeout_ms: Some(60_000),
        ..Default::default()
    }
}

/// 握手 → 列工具 → 调 `resolve-library-id`,确认真实链路可用。
#[tokio::test]
#[ignore = "需要外网访问 mcp.context7.com"]
async fn context7_http_transport_works_end_to_end() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[context7_config()]).await;

    let snapshot = manager.snapshot();
    let state = &snapshot.servers[0];
    assert_eq!(
        state.status,
        denia_mcp::McpServerStatus::Connected,
        "连接失败:{:?}",
        state.error
    );
    let names = snapshot.tool_names();
    assert!(
        names.contains(&"mcp__context7__resolve_library_id".to_string()),
        "工具清单缺少 resolve-library-id:{names:?}"
    );
    assert!(
        names.contains(&"mcp__context7__query_docs".to_string()),
        "工具清单缺少 query-docs:{names:?}"
    );

    // 真实调用:把 "react" 解析成 Context7 库 ID。
    let result = manager
        .call(
            "mcp__context7__resolve_library_id",
            json!({ "query": "how to use hooks", "libraryName": "react" }),
        )
        .await
        .expect("resolve-library-id 调用成功");
    assert!(!result.is_error, "业务错误:{}", result.text);
    assert!(
        result.text.contains("/react"),
        "响应里应含 react 的库 ID:\n{}",
        result.text
    );
    println!("resolve-library-id 返回前 400 字符:\n{}", &result.text[..400.min(result.text.len())]);
}
