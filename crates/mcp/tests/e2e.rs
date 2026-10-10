//! 端到端:用 fixture 服务器跑真实的 stdio MCP 握手、工具清单与调用。
//!
//! fixture 是同 crate 的 bin(`denia-mcp-fixture`),测试用
//! `env!("CARGO_BIN_EXE_...")` 定位可执行文件,跨平台无需外部命令。

use std::collections::HashSet;

use denia_mcp::{McpManager, McpServerConfig, McpServerStatus, PageCall, PagedError, PagedResult};
use serde_json::{Value, json};

fn fixture_config(id: &str) -> McpServerConfig {
    McpServerConfig {
        id: id.to_string(),
        transport: "stdio".to_string(),
        command: env!("CARGO_BIN_EXE_denia-mcp-fixture").to_string(),
        ..Default::default()
    }
}

/// 走一次分页调用:`result_id` 为 `None` 且 `offset>0` 时按旧式续读发。
async fn page(
    manager: &McpManager,
    qualified: &str,
    offset: usize,
    limit: usize,
    arguments: Value,
    result_id: Option<&str>,
    session: &str,
) -> PagedResult {
    manager
        .call_paged(&PageCall {
            qualified,
            arguments: &arguments,
            offset,
            limit,
            result_id,
            legacy_resume: result_id.is_none() && offset > 0,
            session: Some(session),
        })
        .await
        .expect("call succeeds")
}

#[tokio::test]
async fn connects_lists_tools_and_calls_them() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;

    let snapshot = manager.snapshot();
    let state = &snapshot.servers[0];
    assert_eq!(
        state.status,
        McpServerStatus::Connected,
        "{:?}",
        state.error
    );
    // 工具名带服务器前缀,模型写得出、派发认得回。
    let names = snapshot.tool_names();
    assert!(names.contains(&"mcp__fx__echo".to_string()), "{names:?}");
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

/// 分页:长结果按字符切片;续读靠**结果身份**(令牌),不靠「参数一样」猜。
#[tokio::test]
async fn long_results_page_by_chars() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;

    // 新调用(不带令牌):真实执行,产出 100 字符,取前 30,并拿到结果身份。
    let first = page(
        &manager,
        "mcp__fx__big",
        0,
        30,
        json!({ "chars": 100 }),
        None,
        "s1",
    )
    .await;
    assert_eq!(first.total_chars, 100);
    assert_eq!(first.shown_chars, 30);
    assert!(first.has_more);
    assert!(!first.from_cache);
    let id = first.result_id.clone().expect("可续读的结果必须带身份");

    // 续读:带上令牌 + 更大的 offset → 复用那一次结果,不再执行工具。
    let second = page(
        &manager,
        "mcp__fx__big",
        30,
        30,
        json!({ "chars": 100 }),
        Some(&id),
        "s1",
    )
    .await;
    assert!(second.from_cache, "续读必须复用上次结果");
    assert_eq!(second.offset, 30);
    assert_eq!(second.text, "A".repeat(30));
    assert!(second.has_more);
    assert_eq!(second.result_id.as_deref(), Some(id.as_str()));

    // 末页:has_more 为假。
    let last = page(
        &manager,
        "mcp__fx__big",
        90,
        30,
        json!({ "chars": 100 }),
        Some(&id),
        "s1",
    )
    .await;
    assert_eq!(last.shown_chars, 10);
    assert!(!last.has_more);

    // 参数变了(不同 chars)且没带令牌 → 新调用,真实执行,另有一个身份。
    let other = page(
        &manager,
        "mcp__fx__big",
        0,
        10,
        json!({ "chars": 5 }),
        None,
        "s1",
    )
    .await;
    assert!(!other.from_cache);
    assert_eq!(other.total_chars, 5);
    assert_ne!(other.result_id.as_deref(), Some(id.as_str()));
}

/// 同参数、同样不带令牌的第二次调用是**新调用** → 必须真实执行。
///
/// fixture 的 `count` 工具回的是本进程累计的调用次数,所以「到底跑没跑」
/// 是可以直接观测的,不靠推断。
#[tokio::test]
async fn identical_call_executes_again() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;

    let first = page(&manager, "mcp__fx__count", 0, 1_000, json!({}), None, "s1").await;
    assert_eq!(first.text, "1");
    assert!(!first.from_cache);

    let second = page(&manager, "mcp__fx__count", 0, 1_000, json!({}), None, "s1").await;
    assert!(!second.from_cache);
    assert_eq!(second.text, "2", "同参数的新调用必须真实执行");
}

/// 首次业务失败(`isError=true`)后翻页仍保留错误状态。
#[tokio::test]
async fn error_state_survives_paging() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;

    let first = page(
        &manager,
        "mcp__fx__big",
        0,
        30,
        json!({ "chars": 100, "isError": true }),
        None,
        "s1",
    )
    .await;
    assert!(first.is_error, "首次失败必须如实上报");
    assert!(first.has_more);
    let id = first.result_id.clone().expect("结果身份");

    let second = page(
        &manager,
        "mcp__fx__big",
        30,
        30,
        json!({ "chars": 100, "isError": true }),
        Some(&id),
        "s1",
    )
    .await;
    assert!(second.from_cache, "续读走缓存");
    assert!(second.is_error, "翻页不能把错误状态抹掉");
    assert_eq!(second.text, "A".repeat(30));
}

/// 结果不跨会话复用:令牌只在产生它的会话里有效。
#[tokio::test]
async fn results_do_not_leak_across_sessions() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;

    let first = page(
        &manager,
        "mcp__fx__big",
        0,
        30,
        json!({ "chars": 100 }),
        None,
        "s1",
    )
    .await;
    let id = first.result_id.clone().expect("结果身份");

    // 另一个会话拿这个令牌 → 明确拒绝,不重跑工具。
    let foreign = manager
        .call_paged(&PageCall {
            qualified: "mcp__fx__big",
            arguments: &json!({ "chars": 100 }),
            offset: 30,
            limit: 30,
            result_id: Some(&id),
            legacy_resume: false,
            session: Some("s2"),
        })
        .await
        .expect_err("跨会话续读必须失败");
    assert_eq!(foreign, PagedError::ForeignSession { result_id: id.clone() });

    // 旧式 offset 续读同样按会话隔离:s2 名下没有那次结果。
    let legacy = manager
        .call_paged(&PageCall {
            qualified: "mcp__fx__big",
            arguments: &json!({ "chars": 100 }),
            offset: 30,
            limit: 30,
            result_id: None,
            legacy_resume: true,
            session: Some("s2"),
        })
        .await
        .expect_err("跨会话不得命中别人的结果");
    assert_eq!(legacy, PagedError::LostResult);

    // 原会话仍能正常续读。
    let same = page(
        &manager,
        "mcp__fx__big",
        30,
        30,
        json!({ "chars": 100 }),
        Some(&id),
        "s1",
    )
    .await;
    assert!(same.from_cache);
    assert_eq!(same.text, "A".repeat(30));
}

/// 旧式 `offset` 续读:能唯一确定原结果时复用,并把身份补给模型。
#[tokio::test]
async fn legacy_offset_resume_reuses_result_without_rerunning() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;

    let first = page(&manager, "mcp__fx__count", 0, 10, json!({}), None, "s1").await;
    assert_eq!(first.text, "1");

    // 不带令牌、只用 offset>0(旧写法)→ 复用那次结果,不重跑。
    let legacy = page(&manager, "mcp__fx__count", 1, 10, json!({}), None, "s1").await;
    assert!(legacy.from_cache, "旧式续读不得重跑工具");
    assert_eq!(legacy.offset, 1);
    assert_eq!(legacy.text, "");
    assert_eq!(legacy.result_id.as_deref(), first.result_id.as_deref());

    // 计数证明旧式续读没有真的执行工具。
    let third = page(&manager, "mcp__fx__count", 0, 10, json!({}), None, "s1").await;
    assert_eq!(third.text, "2", "旧式续读不该产生新的一次外部调用");
}

/// 旧式 `offset` 续读找不到原结果时报错提示迁移,**不静默重跑**。
#[tokio::test]
async fn legacy_offset_without_result_is_refused_not_rerun() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;

    let refused = manager
        .call_paged(&PageCall {
            qualified: "mcp__fx__count",
            arguments: &json!({}),
            offset: 1,
            limit: 10,
            result_id: None,
            legacy_resume: true,
            session: Some("s1"),
        })
        .await
        .expect_err("没有原结果时必须报错");
    assert_eq!(refused, PagedError::LostResult);

    // 计数证明被拒绝的那次没有执行工具。
    let after = page(&manager, "mcp__fx__count", 0, 10, json!({}), None, "s1").await;
    assert_eq!(after.text, "1", "拒绝续读不得变成偷偷重跑");
}

/// 令牌失效(服务器重连后旧结果作废)时报错,而不是重新执行。
#[tokio::test]
async fn stale_result_id_is_reported_not_rerun() {
    let manager = McpManager::new(HashSet::new());
    manager.reload(&[fixture_config("fx")]).await;
    let first = page(&manager, "mcp__fx__count", 0, 10, json!({}), None, "s1").await;
    let id = first.result_id.clone().expect("结果身份");

    // 配置变更重建连接:旧结果的身份随之作废。
    manager.reload(&[fixture_config("fx")]).await;
    let stale = manager
        .call_paged(&PageCall {
            qualified: "mcp__fx__count",
            arguments: &json!({}),
            offset: 1,
            limit: 10,
            result_id: Some(&id),
            legacy_resume: false,
            session: Some("s1"),
        })
        .await
        .expect_err("作废的令牌必须报错");
    assert_eq!(stale, PagedError::UnknownResult { result_id: id });
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
    manager
        .reload(&[disabled, broken, fixture_config("ok")])
        .await;

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
    let names = manager.snapshot().tool_names();
    assert!(names.contains(&"mcp__fx__echo".to_string()), "{names:?}");
    assert!(names.contains(&"mcp__fx__big".to_string()), "{names:?}");

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
