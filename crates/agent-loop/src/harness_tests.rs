use super::*;

async fn run(driver: &SessionDriver, session: &Arc<Session>) -> TurnEndReason {
    driver
        .run_turn(
            session,
            &selection(),
            "continue",
            vec![],
            vec![],
            vec![],
            true,
            CancellationToken::new(),
            noop_emit(),
        )
        .await
}

fn capped(mut chunks: Vec<StreamChunk>) -> Vec<StreamChunk> {
    *chunks.last_mut().unwrap() = StreamChunk::Finish {
        reason: FinishReason::MaxTokens,
    };
    chunks
}

#[tokio::test]
async fn continuation_is_bounded_and_preserves_partial_text() {
    let (driver, _) = driver(
        (0..4)
            .map(|_| MockScript::Chunks(capped(text_script("partial"))))
            .collect(),
    );
    let session = temp_session();
    assert_eq!(run(&driver, &session).await, TurnEndReason::MaxTokens);
    let events = session.events();
    assert_eq!(events.iter().filter(|e| matches!(&e.event, SessionEvent::RetryAttempt { code, .. } if code == "MAX_TOKENS_CONTINUATION")).count(), 3);
    assert_eq!(
        session
            .derive_messages()
            .iter()
            .filter(|m| m.content == "partial")
            .count(),
        4
    );
}

#[tokio::test]
async fn incomplete_call_is_not_executed_and_complete_call_executes_once() {
    let mut incomplete = capped(tool_script());
    for chunk in &mut incomplete {
        if let StreamChunk::BlockEnd {
            block: ContentBlock::ToolCall { incomplete, .. },
            ..
        } = chunk
        {
            *incomplete = true;
        }
    }
    let (driver, _) = driver(vec![
        MockScript::Chunks(incomplete),
        MockScript::Chunks(capped(tool_script())),
        MockScript::Chunks(text_script("done")),
    ]);
    let session = temp_session();
    assert_eq!(run(&driver, &session).await, TurnEndReason::Completed);
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::ToolResult { .. }))
            .count(),
        1
    );
}

#[tokio::test]
async fn no_progress_does_not_carry_across_user_turns() {
    let scripts = (0..2)
        .flat_map(|_| {
            (0..9)
                .map(|_| MockScript::Chunks(tool_script()))
                .chain(std::iter::once(MockScript::Chunks(text_script("done"))))
        })
        .collect();
    let (driver, _) = driver(scripts);
    let session = temp_session();
    assert_eq!(run(&driver, &session).await, TurnEndReason::Completed);
    assert_eq!(run(&driver, &session).await, TurnEndReason::Completed);
}

#[tokio::test]
async fn context_recovery_runs_once_and_stops_when_disabled_or_failed() {
    for enabled in [false, true] {
        let overflow = || {
            MockScript::Fail(LlmFailure::new(
                denia_core::error::codes::CONTEXT_WINDOW_EXCEEDED,
                "maximum context length exceeded",
            ))
        };
        let (driver, _) = driver(vec![
            overflow(),
            MockScript::Chunks(text_script("summary")),
            overflow(),
        ]);
        let driver = driver.with_compaction(crate::compact::CompactionSettings {
            compact_enabled: enabled,
            min_keep_tokens: 1,
            max_keep_tokens: 1,
            min_text_messages: 1,
            ..Default::default()
        });
        let session = temp_session();
        session
            .append(SessionEvent::UserMessage {
                text: "old evidence".repeat(100),
                injected: false,
                channel: None,
                images: vec![],
            })
            .unwrap();
        assert!(matches!(
            run(&driver, &session).await,
            TurnEndReason::Error { .. }
        ));
        let events = session.events();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e.event, SessionEvent::CompactionSummary { .. }))
                .count(),
            usize::from(enabled)
        );
        assert_eq!(events.iter().filter(|e| matches!(&e.event, SessionEvent::RetryAttempt { code, .. } if code == denia_core::error::codes::CONTEXT_WINDOW_EXCEEDED)).count(), 1);
    }
}

/// Real HTTP/SSE adapter -> history rejection -> tool -> continuation -> final.
#[tokio::test]
async fn mock_gateway_replay_tool_and_recovery_round_trip() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let captured = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let observed = captured.clone();
    let server = tokio::spawn(async move {
        for index in 0..5 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let body = loop {
                let mut buf = [0; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                assert!(n > 0);
                data.extend_from_slice(&buf[..n]);
                if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&data[..end]);
                    let length: usize = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap();
                    if data.len() >= end + 4 + length {
                        break serde_json::from_slice(&data[end + 4..end + 4 + length]).unwrap();
                    }
                }
            };
            observed.lock().unwrap().push(body);
            let (status, kind, body) = if index == 1 {
                (
                    "400 Bad Request",
                    "application/json",
                    r#"{"error":{"message":"Unknown field reasoning_content in messages"}}"#
                        .to_string(),
                )
            } else {
                let (delta, finish) = match index {
                    0 => (
                        serde_json::json!({"reasoning_content":"evidence", "tool_calls":[{"index":0,"id":"c1","type":"function","function":{"name":"echo","arguments":"{}"}}]}),
                        "tool_calls",
                    ),
                    2 => (serde_json::json!({"content":"partial"}), "length"),
                    _ => (serde_json::json!({"content":"done"}), "stop"),
                };
                (
                    "200 OK",
                    "text/event-stream",
                    format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        serde_json::json!({"choices":[{"delta":delta}]}),
                        serde_json::json!({"choices":[{"delta":{},"finish_reason":finish}]})
                    ),
                )
            };
            socket.write_all(format!("HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let session = temp_session();
    let settings = Arc::new(denia_settings::SettingsStore::open(session.directory()).unwrap());
    settings.register(denia_llm::OPENAI_SETTINGS_NS, denia_settings::NamespaceSpec {
        defaults: serde_json::json!({"providers":{"mock":{"baseURL":url,"defaultMaxTokens":8192,"models":[{"id":"mock-1","maxTokens":4096}]}}}),
        validate: Ok, secrets: &[], applies: denia_settings::Applies::Live,
    }, serde_json::json!({})).unwrap();
    let credentials =
        Arc::new(denia_credentials::CredentialStore::open(session.directory()).unwrap());
    let (driver, registry) = driver(vec![]);
    let adapter = Arc::new(denia_llm::OpenAiCompatAdapter::new(settings, credentials));
    assert_eq!(
        adapter
            .output_budget("mock", "mock-1", Some(2000))
            .await
            .unwrap(),
        2000
    );
    assert_eq!(
        adapter
            .output_budget("mock", "mock-1", Some(9000))
            .await
            .unwrap(),
        4096
    );
    assert_eq!(
        adapter
            .output_budget("mock", "uncatalogued", None)
            .await
            .unwrap(),
        8192
    );
    registry.unregister("mock");
    registry
        .register(&["mock".into()], adapter, denia_llm::RetryPolicy::default())
        .unwrap();
    assert_eq!(run(&driver, &session).await, TurnEndReason::Completed);
    assert_eq!(run(&driver, &session).await, TurnEndReason::Completed);
    tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 5);
    assert!(requests.iter().all(|r| r["max_tokens"] == 4096));
    let has_reasoning = |i: usize| {
        requests[i]["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m.get("reasoning_content").is_some())
    };
    assert!(has_reasoning(1));
    assert!(!has_reasoning(2) && !has_reasoning(3));
    assert!(has_reasoning(4), "new turn restores replay");
    assert_eq!(
        session
            .events()
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::ToolResult { .. }))
            .count(),
        1
    );
    assert!(
        session
            .derive_messages()
            .iter()
            .any(|m| m.reasoning_content.as_deref() == Some("evidence"))
    );
}
