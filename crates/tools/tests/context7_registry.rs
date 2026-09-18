//! 通过真实 MCP 服务器(Context7)验证"注册 → 调用"全链路。
//!
//! 与 `context7.rs` 的区别:这里额外断言工具已进 **driver 的工具注册表**
//! (模型可派发的那一侧),而不只是 manager 的快照。默认 ignore(需外网)。
//!
//! 跑法:
//! ```sh
//! cargo test -p denia-tools --test context7_registry -- --ignored --nocapture
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use denia_mcp::{McpManager, McpServerConfig};

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

/// 注册 MCP 工具 → 从 registry 里 get → 通过 McpTool 执行一次真实调用。
#[tokio::test]
#[ignore = "需要外网访问 mcp.context7.com"]
async fn context7_tool_is_dispatchable_through_registry() {
    let manager = Arc::new(McpManager::new(HashSet::new()));
    manager.reload(&[context7_config()]).await;

    // 用与 server 侧相同的入口把 MCP 工具并进注册表。
    let mut registry = denia_tools::ToolRegistry::default();
    denia_tools::register_mcp_tools(&mut registry, &manager);

    let names: Vec<String> = registry
        .schemas()
        .into_iter()
        .map(|schema| schema.name)
        .collect();
    assert!(
        names.contains(&"mcp__context7__resolve_library_id".to_string()),
        "注册表缺少工具:{names:?}"
    );

    // schema 里必须带分页字段(用户要求:长结果要能翻页)。
    let schema = registry
        .get("mcp__context7__resolve_library_id")
        .expect("tool present");
    let params = schema.schema().parameters.to_string();
    assert!(params.contains("offset"), "schema 缺少 offset:{params}");
    assert!(params.contains("limit"), "schema 缺少 limit:{params}");

    // 真实执行一次:通过工具对象的 execute 走完整链路(含分页封装)。
    let tool = registry
        .get("mcp__context7__resolve_library_id")
        .expect("tool present");
    let ctx = denia_tools::ToolContext {
        session_id: None,
        selection: None,
        cwd: std::env::current_dir().unwrap_or_default(),
        cancel: tokio_util::sync::CancellationToken::new(),
        confined: true,
        vision_supported: false,
        emit_event: None,
        file_history: None,
        permission_mode: denia_core::session::PermissionMode::AutoEdit,
        ask: None,
        call_id: None,
        goal_reader: None,
        read_state: None,
    };
    let output = tool
        .execute(
            r#"{"query":"routing middleware","libraryName":"next.js"}"#,
            &ctx,
        )
        .await;
    assert!(!output.is_error, "调用失败:{}", output.content);
    assert!(
        output.content.contains("/vercel"),
        "响应里应含 vercel 的库 ID:\n{}",
        &output.content[..600.min(output.content.len())]
    );
    println!(
        "工具输出前 500 字符:\n{}",
        &output.content[..500.min(output.content.len())]
    );
}
