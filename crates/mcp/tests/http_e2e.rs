//! 端到端:用本地 HTTP fixture 验证 Streamable HTTP 与 SSE 两种传输。
//!
//! fixture 起在随机端口,第一行 stdout 输出端口号;测试据此拼 URL。
//! 两种传输共用同一套工具(echo / big),因此"能连上 / 能列工具 /
//! 能调用 / 分页"的行为应当与 stdio 完全一致——这正是把传输抽成
//! trait 的目的:传输差异不泄漏到协议层。

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use denia_mcp::{McpManager, McpServerConfig, McpServerStatus};
use serde_json::json;

/// 起一个 HTTP fixture,返回它的 base URL(如 `http://127.0.0.1:54321`)。
///
/// 返回的 `FixtureProcess` 在 drop 时 kill 子进程:测试结束(哪怕断言失败
/// panic)也不会留下占着端口的僵尸进程。
fn spawn_http_fixture() -> (FixtureProcess, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_denia-mcp-http-fixture"))
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn http fixture");
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read port line");
    let port = line.trim().to_string();
    (
        FixtureProcess(Some(child)),
        format!("http://127.0.0.1:{port}"),
    )
}

/// fixture 子进程的 RAII 守卫:drop 即 kill,不留残留进程。
struct FixtureProcess(Option<std::process::Child>);

impl Drop for FixtureProcess {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn http_config(id: &str, base: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.to_string(),
        transport: "http".to_string(),
        url: Some(format!("{base}/mcp")),
        ..Default::default()
    }
}

fn sse_config(id: &str, base: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.to_string(),
        transport: "sse".to_string(),
        url: Some(format!("{base}/sse")),
        ..Default::default()
    }
}

#[tokio::test]
async fn http_transport_lists_and_calls_tools() {
    let (_fixture, base) = spawn_http_fixture();
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[http_config("web", &base)]).await;

    let snapshot = manager.snapshot();
    let state = &snapshot.servers[0];
    assert_eq!(state.status, McpServerStatus::Connected, "{:?}", state.error);
    let names = snapshot.tool_names();
    assert!(names.contains(&"mcp__web__echo".to_string()), "{names:?}");
    assert!(names.contains(&"mcp__web__big".to_string()), "{names:?}");

    let result = manager
        .call("mcp__web__echo", json!({ "text": "通过 HTTP" }))
        .await
        .expect("echo over http");
    assert_eq!(result.text, "通过 HTTP");
}

#[tokio::test]
async fn sse_transport_lists_and_calls_tools() {
    let (_fixture, base) = spawn_http_fixture();
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[sse_config("stream", &base)]).await;

    let snapshot = manager.snapshot();
    let state = &snapshot.servers[0];
    assert_eq!(state.status, McpServerStatus::Connected, "{:?}", state.error);
    let names = snapshot.tool_names();
    assert!(
        names.contains(&"mcp__stream__echo".to_string()),
        "{names:?}"
    );

    let result = manager
        .call("mcp__stream__echo", json!({ "text": "通过 SSE" }))
        .await
        .expect("echo over sse");
    assert_eq!(result.text, "通过 SSE");
}

/// 分页是工具层行为,与传输无关:HTTP 上同样按字符切片。
#[tokio::test]
async fn http_results_page_by_chars() {
    let (_fixture, base) = spawn_http_fixture();
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[http_config("web", &base)]).await;

    let first = manager
        .call_paged("mcp__web__big", 0, 30, &json!({ "chars": 100 }))
        .await
        .expect("call");
    assert_eq!(first.total_chars, 100);
    assert_eq!(first.shown_chars, 30);
    assert!(first.has_more);

    let next = manager
        .call_paged("mcp__web__big", 30, 30, &json!({ "chars": 100 }))
        .await
        .expect("call");
    assert!(next.from_cache, "翻页不重跑工具");
    assert_eq!(next.text, "A".repeat(30));
}

/// 三种传输可以并存:同一个 manager 同时管 stdio / http / sse。
#[tokio::test]
async fn mixed_transports_coexist() {
    let (_fixture, base) = spawn_http_fixture();
    let stdio = McpServerConfig {
        id: "local".to_string(),
        transport: "stdio".to_string(),
        command: env!("CARGO_BIN_EXE_denia-mcp-fixture").to_string(),
        ..Default::default()
    };
    let manager = McpManager::new(HashSet::new());
    manager
        .reload(&[stdio, http_config("web", &base), sse_config("stream", &base)])
        .await;

    let names = manager.snapshot().tool_names();
    for expected in [
        "mcp__local__echo",
        "mcp__web__echo",
        "mcp__stream__echo",
    ] {
        assert!(names.contains(&expected.to_string()), "{expected} 缺失:{names:?}");
    }
}

/// 连不上(端口没人听)→ 该服务器标 error,不拖垮别的服务器。
#[tokio::test]
async fn unreachable_http_server_is_isolated() {
    let (_fixture, base) = spawn_http_fixture();
    // 换一个几乎不可能被占用的端口。
    let dead = base.replace(
        &base[base.rfind(':').unwrap()..],
        ":1",
    );
    let manager = McpManager::new(HashSet::new());
    manager
        .reload(&[http_config("dead", &dead), http_config("web", &base)])
        .await;
    let snapshot = manager.snapshot();
    let dead_state = snapshot
        .servers
        .iter()
        .find(|s| s.id == "dead")
        .expect("dead present");
    assert_eq!(dead_state.status, McpServerStatus::Error);
    assert!(dead_state.error.is_some());
    // 另一个照常可用。
    assert!(snapshot.tool_names().contains(&"mcp__web__echo".to_string()));
}

/// 不支持的传输:明确失败并给出可执行提示,而不是静默当 stdio 处理。
#[tokio::test]
async fn unsupported_transport_is_reported() {
    let manager = McpManager::new(HashSet::new());
    let config = McpServerConfig {
        id: "weird".to_string(),
        transport: "websocket".to_string(),
        url: Some("ws://127.0.0.1:1".to_string()),
        ..Default::default()
    };
    manager.reload(&[config]).await;
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.servers[0].status, McpServerStatus::Error);
    let message = snapshot.servers[0].error.clone().unwrap_or_default();
    assert!(message.contains("不支持的传输方式"), "{message}");
}

