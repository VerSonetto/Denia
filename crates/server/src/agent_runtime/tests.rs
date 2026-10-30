use super::handoff::{TerminalBasis, TerminalOutcome};
use super::*;

#[cfg(test)]
fn validate_wait_timeout(args: &Value) -> Result<(), String> {
    let mut args = args.clone();
    args["id"] = json!("test");
    denia_tools::runtime_command::RuntimeCommand::parse("job_output", args).map(|_| ())
}

use denia_core::{
    error::LlmError,
    stream::{ContentBlock, FinishReason, StreamChunk},
};
use denia_llm::{
    ChunkStream, GenerateRequest, LlmAdapter, LlmModelInfo, LlmResolvedModelInfo, ProviderInfo,
};
struct Mock;
#[async_trait]
impl LlmAdapter for Mock {
    fn provider_info(&self, p: &str) -> ProviderInfo {
        ProviderInfo {
            id: p.into(),
            name: "测试".into(),
        }
    }
    async fn list_models(&self, _: &str) -> Result<Vec<LlmModelInfo>, LlmError> {
        Ok(Vec::new())
    }
    async fn resolve_model(&self, p: &str, m: &str) -> Result<LlmResolvedModelInfo, LlmError> {
        Ok(LlmResolvedModelInfo {
            info: LlmModelInfo {
                provider: p.into(),
                id: m.into(),
                name: m.into(),
                description: None,
                input_modalities: vec!["text".into()],
            },
            context_window: Some(100_000),
            default_max_tokens: Some(4000),
            reasoning: None,
        })
    }
    async fn stream(&self, _: &str, request: &GenerateRequest) -> Result<ChunkStream, LlmError> {
        // 不再全局断言"必有 skill"：受限子代理（如只授予 bash/jobs 的
        // 定义）本来就不该看到 skill。需要它的分支各自断言。
        if request.model == "hold" {
            return Ok(Box::pin(futures::stream::pending()));
        }
        if request.model == "background-agent-parent" {
            let results: Vec<_> = request
                .messages
                .iter()
                .filter(|message| message.role == denia_core::message::ChatRole::Tool)
                .collect();
            let call = match results.len() {
                0 => Some((
                    "spawn_agent",
                    json!({"prompt":"继续后台调查","model":"hold","run_in_background":false}),
                )),
                1 => Some((
                    "wait_agent",
                    json!({"target":serde_json::from_str::<Value>(&results[0].content).unwrap()["childId"],"timeout_ms":600_000}),
                )),
                _ => None,
            };
            if let Some((name, args)) = call {
                return Ok(Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::ToolCall {
                            id: format!("background-call-{}", results.len()),
                            name: name.into(),
                            arguments: args.to_string(),
                            incomplete: false,
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                    }),
                ])));
            }
            let completed = request
                .messages
                .iter()
                .any(|message| message.content.contains("[子代理执行结束]"));
            return Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text {
                        text: if completed {
                            "后台结果已自动返回"
                        } else {
                            "任务仍在后台运行，我先回复当前进度"
                        }
                        .into(),
                    },
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                }),
            ])));
        }
        // —— 执行型子代理的真实副作用链路（无需真实模型）——
        // exec-parent 派一个 develop 子代理；子代理读→写→跑命令，然后收尾。
        if request.model == "exec-parent" {
            let results = request
                .messages
                .iter()
                .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                .count();
            if results == 0 {
                return Ok(tool_call_stream(
                    "exec-spawn",
                    "spawn_agent",
                    json!({"prompt":"SUBTASK-EXEC","description":"开发子代理","model":"exec-child"}),
                ));
            }
            return Ok(text_stream("子代理已完成任务"));
        }
        if request.model == "exec-child" {
            // 子代理的有效工具来自**冻结授权**：develop 含写与命令。
            assert!(
                request.tools.iter().any(|s| s.name == "write_file"),
                "develop 子代理必须有 write_file"
            );
            assert!(
                request.tools.iter().any(|s| s.name == "bash"),
                "develop 子代理必须有 bash"
            );
            for denied in ["spawn_agent", "fork_agent", "exit_plan", "get_goal"] {
                assert!(
                    !request.tools.iter().any(|s| s.name == denied),
                    "{denied} 不得出现在子代理工具面"
                );
            }
            let results = request
                .messages
                .iter()
                .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                .count();
            let call = match results {
                0 => Some(("read_file", json!({"path": "existing.txt"}))),
                1 => Some((
                    "write_file",
                    json!({"path": "child-wrote.txt","content":"子代理写入\n"}),
                )),
                2 => Some(("bash", json!({"command":"echo child-ran > child-ran.txt"}))),
                // 没被授予 job_start 的子代理请求后台执行：必须被拒，且不能
                // 真的起一个后台任务（只禁 job_start 不是收口）。
                3 => Some((
                    "bash",
                    json!({"command":"echo should-not-run","run_in_background":true}),
                )),
                _ => None,
            };
            if let Some((name, args)) = call {
                return Ok(tool_call_stream(&format!("exec-{results}"), name, args));
            }
            return Ok(text_stream("开发子代理完成"));
        }
        // explore 子代理：模型幻觉调用写工具与命令，必须被拒且文件无变化。
        if request.model == "explore-parent" {
            let results = request
                .messages
                .iter()
                .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                .count();
            if results == 0 {
                return Ok(tool_call_stream(
                    "explore-spawn",
                    "spawn_agent",
                    json!({"prompt":"SUBTASK-EXPLORE","profile_id":"builtin:explore","model":"explore-child"}),
                ));
            }
            return Ok(text_stream("探索子代理已回结论"));
        }
        if request.model == "explore-child" {
            // explore 的冻结授权只读：写与命令既不在 schema 里，也不可执行。
            for denied in ["write_file", "edit", "bash", "job_start"] {
                assert!(
                    !request.tools.iter().any(|s| s.name == denied),
                    "explore 子代理的工具面不得含 {denied}"
                );
            }
            assert!(
                request.tools.iter().any(|s| s.name == "read_file"),
                "explore 子代理必须有 read_file"
            );
            let results = request
                .messages
                .iter()
                .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                .count();
            let call = match results {
                0 => Some((
                    "write_file",
                    json!({"path": "explore-wrote.txt","content":"不应写入\n"}),
                )),
                1 => Some(("bash", json!({"command":"echo nope > explore-wrote.txt"}))),
                _ => None,
            };
            if let Some((name, args)) = call {
                return Ok(tool_call_stream(&format!("explore-{results}"), name, args));
            }
            return Ok(text_stream("只读探索完成"));
        }
        // 子代理遗留后台任务：正常结束时必须被取消并写进结果通知。
        if request.model == "jobs-parent" {
            let results = request
                .messages
                .iter()
                .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                .count();
            if results == 0 {
                return Ok(tool_call_stream(
                    "jobs-spawn",
                    "spawn_agent",
                    json!({
                        "prompt": "SUBTASK-JOBS",
                        "description": "带后台任务的子代理",
                        "model": "jobs-child",
                        "inline": {
                            "name": "后台任务员",
                            "description": "启动一个长任务后收尾",
                            "tools": {"mode": "allowlist", "names": ["bash", "job_start", "job_list"]},
                            "model": {"mode": "inherit"},
                            "permissionCeiling": "inherit"
                        }
                    }),
                ));
            }
            return Ok(text_stream("子代理已回结果"));
        }
        if request.model == "jobs-child" {
            // 冻结授权里有 job_start，因此 run_in_background 参数也必须在。
            assert!(
                request.tools.iter().any(|s| s.name == "job_start"),
                "被授予的 job_start 必须可见"
            );
            assert!(
                request.tools.iter().any(|s| s.name == "bash"),
                "被授予的 bash 必须可见"
            );
            assert!(
                !request.tools.iter().any(|s| s.name == "job_kill"),
                "未授予的 job_kill 不得出现"
            );
            let results = request
                .messages
                .iter()
                .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                .count();
            if results == 0 {
                return Ok(tool_call_stream(
                    "jobs-0",
                    "job_start",
                    json!({"command": LONG_RUNNING_PROBE, "label": "leftover-probe"}),
                ));
            }
            return Ok(text_stream("子代理启动后台任务后收尾"));
        }
        if request.model == "legacy-child" {
            // 旧子代理建立投影后：旧自动注入不得再进模型请求，任务保留。
            assert!(
                !request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("GLOBAL-SENTINEL")),
                "旧自动注入不得进入模型请求：{:?}",
                request
                    .messages
                    .iter()
                    .map(|message| message.content.clone())
                    .collect::<Vec<_>>()
            );
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| message.content.contains("调查登录流程")),
                "child 自己的任务投递必须保留"
            );
            // H06：续跑不得扩大快照授权（旧描述符 → 保守只读集合）。
            for forbidden in ["bash", "write_file", "edit", "job_start", "spawn_agent"] {
                assert!(
                    !request.tools.iter().any(|schema| schema.name == forbidden),
                    "续跑旧子代理不得扩权到 {forbidden}：{:?}",
                    request
                        .tools
                        .iter()
                        .map(|schema| schema.name.clone())
                        .collect::<Vec<_>>()
                );
            }
            assert!(request.tools.iter().all(|schema| {
                denia_core::subagent::LEGACY_READ_ONLY_TOOLS.contains(&schema.name.as_str())
            }));
            return Ok(text_stream("旧子代理已按投影继续"));
        }
        if request.model == "orchestrator" {
            let results: Vec<_> = request
                .messages
                .iter()
                .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                .collect();
            let call = match results.len() {
                0 => Some((
                    "spawn_agent",
                    json!({"prompt":"执行子任务","model":"done","description":"接口验证子代理"}),
                )),
                1 => Some((
                    "bash",
                    json!({"command":"echo runtime-e2e","run_in_background":true}),
                )),
                2 => Some((
                    "job_output",
                    json!({"id":serde_json::from_str::<Value>(&results[1].content).unwrap()["id"],"wait":true}),
                )),
                3 => Some(("skill", json!({"action":"load","name":"runtime-test"}))),
                _ => None,
            };
            if let Some((name, args)) = call {
                return Ok(Box::pin(futures::stream::iter(vec![
                    Ok(StreamChunk::BlockEnd {
                        index: 0,
                        block: ContentBlock::ToolCall {
                            id: format!("call-{}", results.len()),
                            name: name.into(),
                            arguments: args.to_string(),
                            incomplete: false,
                        },
                    }),
                    Ok(StreamChunk::Finish {
                        reason: FinishReason::ToolCalls,
                    }),
                ])));
            }
        }
        Ok(Box::pin(futures::stream::iter(vec![
            Ok(StreamChunk::BlockEnd {
                index: 0,
                block: ContentBlock::Text {
                    text: "子代理测试完成".into(),
                },
            }),
            Ok(StreamChunk::Finish {
                reason: FinishReason::Stop,
            }),
        ])))
    }
}
/// 长跑探针：足够久，保证子代理结束的那一刻它仍在运行（用于验证
/// "子代理不得遗留后台任务"）。
const LONG_RUNNING_PROBE: &str = if cfg!(windows) {
    "ping -n 30 127.0.0.1 > NUL"
} else {
    "sleep 30"
};

/// 单步工具调用流：调用一次工具并结束本轮 step。
fn tool_call_stream(id: &str, name: &str, args: Value) -> ChunkStream {
    Box::pin(futures::stream::iter(vec![
        Ok(StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments: args.to_string(),
                incomplete: false,
            },
        }),
        Ok(StreamChunk::Finish {
            reason: FinishReason::ToolCalls,
        }),
    ]))
}

/// 纯文本回复流（轮次闭合）。
fn text_stream(text: &str) -> ChunkStream {
    Box::pin(futures::stream::iter(vec![
        Ok(StreamChunk::BlockEnd {
            index: 0,
            block: ContentBlock::Text {
                text: text.to_string(),
            },
        }),
        Ok(StreamChunk::Finish {
            reason: FinishReason::Stop,
        }),
    ]))
}

async fn setup() -> (crate::state::AppState, ToolContext) {
    let home = std::env::temp_dir().join(format!("denia-runtime-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let state = crate::state::build_state(&home, false, 43654)
        .await
        .unwrap();
    state
        .registry
        .register(
            &["runtime-test".into()],
            Arc::new(Mock),
            denia_llm::RetryPolicy::default(),
        )
        .unwrap();
    let session = state.sessions.create(&home, true).unwrap();
    let id = session.id().to_string();
    drop(session);
    let live = state.live.get_or_load(&state.sessions, &id).unwrap();
    live.running.store(true, Ordering::SeqCst); // 父轮次由测试控制，通知先排队。
    let ctx = ToolContext {
        output_store: None,
        session_id: Some(id),
        selection: Some(ModelSelection {
            provider: "runtime-test".into(),
            model: "done".into(),
            reasoning_effort: None,
        }),
        cwd: home,
        cancel: CancellationToken::new(),
        confined: true,
        vision_supported: false,
        emit_event: None,
        file_history: None,
        permission_mode: denia_core::session::PermissionMode::AutoEdit,
        ask: None,
        call_id: None,
        goal_reader: None,
        task_ledger: None,
        read_state: None,
    };
    (state, ctx)
}
async fn settle(runtime: &Runtime, id: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while runtime.is_active(id) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("子代理应在测试时限内结束");
}

#[test]
fn background_waits_keep_legacy_timeout_validation() {
    assert!(validate_wait_timeout(&json!({})).is_ok());
    assert!(validate_wait_timeout(&json!({"timeout_ms":600_000})).is_ok());
    for timeout in [json!(0), json!(-1), json!("30000")] {
        assert!(validate_wait_timeout(&json!({"timeout_ms":timeout})).is_err());
    }
    let pending = background_result(json!({"output":"partial"}), true);
    assert_eq!(pending["status"], "pending");
    assert_eq!(pending["notification"], "automatic");
    assert_eq!(pending["output"], "partial");
    let ready = background_result(json!({"job":{"status":"failed"}}), false);
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["job"]["status"], "failed");
    assert!(ready.get("next_action").is_none());
}

#[tokio::test]
async fn background_agent_wait_allows_parent_reply_and_automatic_followup() {
    use tower::ServiceExt;

    let (state, ctx) = setup().await;
    let owner = ctx.session_id.as_deref().unwrap().to_string();
    let live = state.live.get(&owner).unwrap();
    live.running.store(false, Ordering::SeqCst);
    let state = Arc::new(state);
    let response = crate::api::router()
            .with_state(state.clone())
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(format!("/api/sessions/{owner}/prompt"))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(
                        json!({"prompt":"后台调查并先回复进度","provider":"runtime-test","model":"background-agent-parent"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let replied = live.session.events().iter().any(|event| {
                    matches!(&event.event, SessionEvent::AssistantMessage { blocks, .. }
                        if blocks.iter().any(|block| matches!(block, ContentBlock::Text { text } if text.contains("我先回复当前进度"))))
                });
                if replied && !live.running.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("等待子代理不得阻止父会话先回复");
    let children = state.runtime.list(&owner);
    let child = children[0]["id"].as_str().unwrap();
    assert!(state.runtime.is_active(child));
    let results: Vec<_> = live
        .session
        .events()
        .into_iter()
        .filter_map(|event| match event.event {
            SessionEvent::ToolResult {
                content,
                is_error: false,
                ..
            } => Some(serde_json::from_str::<Value>(&content).unwrap()),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|result| result["status"] == "pending"));
    assert_eq!(results[1]["messages"], json!([]));
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while state
            .live
            .get(child)
            .unwrap()
            .cancel
            .lock()
            .unwrap()
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    state.runtime.interrupt(&owner, child).await.unwrap();
    settle(&state.runtime, child).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let replied = live.session.events().iter().any(|event| {
                    matches!(&event.event, SessionEvent::AssistantMessage { blocks, .. }
                        if blocks.iter().any(|block| matches!(block, ContentBlock::Text { text } if text.contains("后台结果已自动返回"))))
                });
                if replied && !live.running.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("子代理结束后应自动唤醒父会话回复");
    let delivered = live
            .session
            .events()
            .iter()
            .filter(|event| matches!(&event.event, SessionEvent::AgentDelivery { source, .. } if source == "subagent-settled"))
            .count();
    assert_eq!(delivered, 1);
    let ready = state
        .runtime
        .execute("wait_agent", json!({"target":child}), &ctx)
        .await
        .unwrap();
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["running"], false);
}

#[tokio::test]
async fn background_job_output_returns_pending_and_resumes_idle_parent() {
    let (state, ctx) = setup().await;
    let owner = ctx.session_id.as_deref().unwrap();
    state
        .runtime
        .human_turn(owner, ctx.selection.as_ref().unwrap());
    let command = if denia_tools::shell::shell_runtime().dialect == "bash" {
        "sleep 2; printf background-job-result"
    } else {
        "Start-Sleep -Seconds 2; Write-Output background-job-result"
    };
    let started = state
        .runtime
        .execute("job_start", json!({"command":command}), &ctx)
        .await
        .unwrap();
    let job = started["id"].as_str().unwrap();
    let pending = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        state.runtime.execute(
            "job_output",
            json!({"id":job,"wait":true,"timeout_ms":600_000}),
            &ctx,
        ),
    )
    .await
    .expect("读取后台输出不得等待任务结束")
    .unwrap();
    assert_eq!(pending["status"], "pending");
    assert!(pending["job"]["finishedAt"].is_null());
    let live = state.live.get(owner).unwrap();
    live.running.store(false, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let events = live.session.events();
            let delivered = events.iter().any(|event| {
                matches!(&event.event, SessionEvent::AgentDelivery { source, text, .. }
                        if source == "job-completed" && text.contains("background-job-result"))
            });
            let replied = events
                .iter()
                .any(|event| matches!(event.event, SessionEvent::AssistantMessage { .. }));
            if delivered && replied && !live.running.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("后台完成结果应自动唤醒父会话");
    let ready = state
        .runtime
        .execute("job_output", json!({"id":job,"wait":true}), &ctx)
        .await
        .unwrap();
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["job"]["exitCode"], 0);
    assert!(
        ready["output"]
            .as_str()
            .unwrap()
            .contains("background-job-result")
    );
    assert_eq!(
        ready,
        state
            .runtime
            .execute("job_output", json!({"id":job}), &ctx)
            .await
            .unwrap()
    );
    assert_eq!(live.session.events().iter().filter(|event| {
            matches!(&event.event, SessionEvent::AgentDelivery { source, .. } if source == "job-completed")
        }).count(), 1);
    state
        .runtime
        .inner
        .paused
        .lock()
        .unwrap()
        .insert(owner.into());
    let turns = live.session.next_turn_number();
    state
        .runtime
        .enqueue(
            owner,
            "paused-completion".into(),
            "后台结果已保存".into(),
            "job-completed".into(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(live.session.next_turn_number(), turns);
    assert!(!live.session.events().iter().any(|event| {
        matches!(&event.event, SessionEvent::AgentDelivery { id, .. } if id == "paused-completion")
    }));
}

#[tokio::test]
async fn spawn_resume_lineage_and_exactly_once_delivery() {
    let (state, ctx) = setup().await;
    let owner = ctx.session_id.as_ref().unwrap();
    let result = state
        .runtime
        .execute(
            "spawn_agent",
            json!({"prompt":"检查实现","description":"测试子代理"}),
            &ctx,
        )
        .await
        .unwrap();
    let id = result["childId"].as_str().unwrap();
    settle(&state.runtime, id).await;
    let live = state.live.get(id).unwrap();
    assert_eq!(
        live.session.header().parent_session.as_deref(),
        Some(owner.as_str())
    );
    assert!(live.session.header().subagent.is_some());
    assert!(
        state
            .sessions
            .list()
            .unwrap()
            .iter()
            .find(|s| s.id == id)
            .unwrap()
            .excerpt
            .is_some()
    );
    assert!(
        state
            .runtime
            .execute(
                "send_message",
                json!({"target":"foreign","message":"不允许"}),
                &ctx
            )
            .await
            .is_err()
    );
    let first = live.session.next_turn_number();
    state
        .runtime
        .execute(
            "send_message",
            json!({"target":id,"message":"继续检查"}),
            &ctx,
        )
        .await
        .unwrap();
    settle(&state.runtime, id).await;
    assert!(live.session.next_turn_number() > first);
    let events = live.session.events();
    let delivered = events
        .iter()
        .filter(|e| matches!(e.event, SessionEvent::AgentDelivery { .. }))
        .count();
    assert_eq!(delivered, 2);
    state.runtime.drain(id).await.unwrap();
    assert_eq!(
        live.session
            .events()
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::AgentDelivery { .. }))
            .count(),
        2
    );
    // 冷恢复重建目录和收件箱，从持久化领取事件排除已投递项。
    let restored = Runtime::new(
        &state.home,
        state.sessions.clone(),
        state.live.clone(),
        state.settings.clone(),
        state.events.clone(),
        state.registry.clone(),
        state.workspaces.clone(),
        state.subagent_profiles.clone(),
    )
    .unwrap();
    assert_eq!(restored.list(owner).as_array().unwrap().len(), 1);
    restored.drain(id).await.unwrap();
    assert_eq!(
        live.session
            .events()
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::AgentDelivery { .. }))
            .count(),
        2
    );
}
/// 子代理的能力来自**派遣时解析的定义**：默认走 develop（有写能力），
/// explore 只读；硬禁项（派遣/会话主控/宿主配置）任何定义都拿不到；
/// 旧参数路径按历史只读上限保守解释，绝不自动变全工具。
#[tokio::test]
async fn subagents_follow_profile_capability_and_hard_denied_rules() {
    let (state, ctx) = setup().await;
    // ① 省略 profile_id/inline = 默认 develop 定义。
    let result = state
        .runtime
        .execute(
            "spawn_agent",
            json!({"prompt":"实现功能","description":"开发子代理"}),
            &ctx,
        )
        .await
        .unwrap();
    let id = result["childId"].as_str().unwrap();
    assert_eq!(result["profile"]["qualifiedId"], "builtin:develop");
    let live = state.live.get(id).unwrap();
    let descriptor = live.session.header().subagent.clone().unwrap();
    assert_eq!(
        descriptor.snapshot_version,
        denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION
    );
    assert!(
        !descriptor.delegation_allowed,
        "delegationAllowed 必须恒为 false（审计字段）"
    );
    assert_eq!(
        descriptor.instruction_scope.as_deref(),
        Some("project-only")
    );
    assert_eq!(
        descriptor.created_permission_mode.as_deref(),
        Some("auto-edit")
    );
    let tools = descriptor.effective_tools.clone().unwrap();
    for expected in ["write_file", "edit", "bash", "todo_write", "read_file"] {
        assert!(
            tools.contains(&expected.to_string()),
            "develop 应含 {expected}"
        );
    }
    for forbidden in [
        "spawn_agent",
        "fork_agent",
        "list_agents",
        "wait_agent",
        "interrupt_agent",
        "exit_plan",
        "get_goal",
        "update_goal",
        "create_preset",
    ] {
        assert!(
            !tools.contains(&forbidden.to_string()),
            "{forbidden} 是硬禁项，任何定义都不得授予"
        );
    }
    // 角色正文不放 header，只存引用与 hash。
    assert_eq!(
        descriptor.instructions_ref.as_deref(),
        Some("subagent.json")
    );
    assert!(descriptor.instructions_hash.is_some());
    assert!(descriptor.persona.is_none() && descriptor.allowed_tools.is_none());

    // ② explore 定义：只读上限，且不授予 browser/bash/写工具。
    let explore = state
        .runtime
        .execute(
            "spawn_agent",
            json!({"prompt":"调查","profile_id":"builtin:explore"}),
            &ctx,
        )
        .await
        .unwrap();
    let explore_live = state
        .live
        .get(explore["childId"].as_str().unwrap())
        .unwrap();
    let explore_desc = explore_live.session.header().subagent.clone().unwrap();
    assert_eq!(
        explore_desc.permission_ceiling,
        denia_core::subagent::PermissionCeiling::ReadOnly
    );
    let explore_tools = explore_desc.effective_tools.clone().unwrap();
    for forbidden in ["bash", "write_file", "edit", "browser", "ask", "job_start"] {
        assert!(
            !explore_tools.contains(&forbidden.to_string()),
            "explore 不得含 {forbidden}"
        );
    }
    assert!(explore_tools.contains(&"read_file".to_string()));
    assert!(explore_tools.contains(&"send_message".to_string()));

    // ③ 显式请求父未授予/硬禁的工具 → 逐项报错，不静默少给。
    let denied = state
        .runtime
        .execute(
            "spawn_agent",
            json!({"prompt":"越权","inline":{
                "name":"越权","description":"尝试拿硬禁工具",
                "tools":{"mode":"allowlist","names":["read_file","spawn_agent"]}
            }}),
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(denied.contains("subagent/tool-hard-denied"), "{denied}");

    // ④ inline 临时定义：不落盘、工具严格匹配。
    let inline = state
        .runtime
        .execute(
            "spawn_agent",
            json!({"prompt":"查缓存","inline":{
                "name":"缓存调查员",
                "description":"专注缓存失效链路",
                "instructions":"不修改文件",
                "tools":{"mode":"allowlist","names":["read_file","grep"]},
                "permissionCeiling":"read-only"
            }}),
            &ctx,
        )
        .await
        .unwrap();
    assert_eq!(
        inline["profile"]["qualifiedId"].as_str().unwrap(),
        "inline:缓存调查员"
    );
    assert_eq!(inline["profile"]["toolCount"], 2);
    let inline_live = state.live.get(inline["childId"].as_str().unwrap()).unwrap();
    let inline_desc = inline_live.session.header().subagent.clone().unwrap();
    assert_eq!(
        inline_desc.effective_tools.clone().unwrap(),
        vec!["grep".to_string(), "read_file".to_string()]
    );

    // ⑤ 旧参数路径：显式列表最多与历史只读上限相交，绝不自动变全工具。
    let legacy = state
        .runtime
        .execute(
            "spawn_agent",
            json!({"prompt":"旧参数","allowed_tools":["read_file","write_file","bash"]}),
            &ctx,
        )
        .await
        .unwrap();
    let legacy_live = state.live.get(legacy["childId"].as_str().unwrap()).unwrap();
    let legacy_desc = legacy_live.session.header().subagent.clone().unwrap();
    let legacy_tools = legacy_desc.effective_tools.clone().unwrap();
    assert!(legacy_tools.contains(&"read_file".to_string()));
    assert!(
        !legacy_tools.contains(&"write_file".to_string())
            && !legacy_tools.contains(&"bash".to_string()),
        "旧 allowed_tools 不得放开写与命令:{legacy_tools:?}"
    );
    assert_eq!(legacy_desc.snapshot_version, 1);
    // 旧 persona 映射为角色补充（走快照文件）。
    assert!(legacy_desc.instructions_ref.is_some());

    // ⑥ 未知工具名明确失败（不静默忽略）。
    assert!(
        state
            .runtime
            .execute(
                "spawn_agent",
                json!({"prompt":"未知","allowed_tools":["nope"]}),
                &ctx
            )
            .await
            .is_err()
    );
}

/// 子代理禁止派遣子代理：模型入口、直接 Runtime 入口、恢复路径一律拒绝，
/// 且不产生新会话、不占槽、不发模型请求。
#[tokio::test]
async fn subagents_cannot_delegate_from_any_entry() {
    let (state, ctx) = setup().await;
    let child = state
        .runtime
        .execute(
            "spawn_agent",
            json!({"prompt":"调查","profile_id":"builtin:explore"}),
            &ctx,
        )
        .await
        .unwrap();
    let child_id = child["childId"].as_str().unwrap().to_string();
    settle(&state.runtime, &child_id).await;
    let before = state.sessions.list().unwrap().len();
    let mut child_ctx = ctx.clone();
    child_ctx.session_id = Some(child_id.clone());
    let error = state
        .runtime
        .execute("spawn_agent", json!({"prompt":"再派一个"}), &child_ctx)
        .await
        .unwrap_err();
    assert!(
        error.contains("subagent/delegation-forbidden"),
        "子代理派遣必须返回稳定 code:{error}"
    );
    // fork 同样拒绝。
    assert!(
        state
            .runtime
            .execute("fork_agent", json!({"prompt":"fork"}), &child_ctx)
            .await
            .is_err()
    );
    // 旧参数里伪造 max_depth 也不放宽：直接报已移除。
    let depth = state
        .runtime
        .execute("spawn_agent", json!({"prompt":"x","max_depth":3}), &ctx)
        .await
        .unwrap_err();
    assert!(depth.contains("max_depth"), "{depth}");
    assert_eq!(
        state.sessions.list().unwrap().len(),
        before,
        "拒绝路径不得创建会话"
    );
    assert_eq!(state.runtime.inner.admission.reserved_count(), 0);
}

/// 普通用户分支有 parent_session 但没有 subagent 描述符：仍可正常派遣，
/// 禁止规则不能以"有父会话"代替"是子代理"。
#[tokio::test]
async fn user_fork_with_parent_session_can_still_delegate() {
    let (state, ctx) = setup().await;
    let parent_id = ctx.session_id.clone().unwrap();
    // 普通用户分支：create_forked 写 parent_session，不写 subagent。
    let fork = state
        .sessions
        .create_forked(&[], 0, ctx.cwd.as_path(), true, &parent_id)
        .unwrap();
    assert!(fork.header().subagent.is_none());
    let mut fork_ctx = ctx.clone();
    fork_ctx.session_id = Some(fork.id().to_string());
    let result = state
        .runtime
        .execute("spawn_agent", json!({"prompt":"正常派遣"}), &fork_ctx)
        .await;
    assert!(result.is_ok(), "普通分支必须仍可派遣:{result:?}");
}

/// 并发上限走新的 subagent-policy 命名空间；创建与唤醒共用同一份槽位账。
/// 子代理未在期限内完成 `want` 个工具结果就失败（防挂死）。
async fn wait_for_tool_results(state: &crate::state::AppState, child: &str, want: usize) {
    for _ in 0..1000 {
        let done = state
            .live
            .get(child)
            .map(|live| {
                let count = live
                    .session
                    .events()
                    .iter()
                    .filter(|event| matches!(event.event, SessionEvent::ToolResult { .. }))
                    .count();
                count >= want && !live.running.load(Ordering::SeqCst)
            })
            .unwrap_or(false);
        if done {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("子代理未在期限内产出 {want} 个工具结果");
}

/// 等某个 child 真正开始运行（取消令牌已挂上）。
async fn wait_for_running(state: &crate::state::AppState, id: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state
            .live
            .get(id)
            .is_none_or(|live| live.cancel.lock().unwrap().is_none())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

/// 等父会话的第一个子代理出现并返回其 id。
async fn wait_for_child(state: &crate::state::AppState, parent: &str) -> String {
    for _ in 0..500 {
        if let Some(entry) = state
            .runtime
            .list(parent)
            .as_array()
            .and_then(|rows| rows.first())
            .cloned()
        {
            return entry["id"].as_str().unwrap().to_string();
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("子代理未创建");
}

/// T02 / T01：子代理的执行能力严格来自**派遣时冻结的授权**。
///
/// - develop 子代理真实落盘并真的跑了命令（断言文件与命令副作用，不是
///   只断言 schema 字符串）；
/// - explore 子代理（只读上限）即使模型幻觉调用 write_file/bash，schema 里
///   没有、执行被拒、文件无任何变化。
#[tokio::test]
async fn child_execution_side_effects_follow_the_frozen_grant() {
    let (state, _) = setup().await;
    let state = Arc::new(state);
    let workspace = state.home.join("devfixture");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("existing.txt"), "旧内容\n").unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::api::router().with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    let new_session = |cwd: std::path::PathBuf| {
        let client = client.clone();
        let base = base.clone();
        async move {
            let created: Value = client
                .post(format!("{base}/api/sessions"))
                .json(&json!({"cwd": cwd}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            created["session"]["id"].as_str().unwrap().to_string()
        }
    };

    // —— develop：真写、真跑 ——
    let dev_parent = new_session(workspace.clone()).await;
    // 完全访问档：执行型子代理仍走正常权限引擎，这里避免测试卡在审批上。
    assert!(
        client
            .put(format!("{base}/api/sessions/{dev_parent}/permission"))
            .json(&json!({"mode": "full"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(
        client
            .post(format!("{base}/api/sessions/{dev_parent}/prompt"))
            .json(&json!({"prompt":"委派开发任务","provider":"runtime-test","model":"exec-parent"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let dev_child = wait_for_child(&state, &dev_parent).await;
    wait_for_tool_results(&state, &dev_child, 4).await;
    assert_eq!(
        std::fs::read_to_string(workspace.join("child-wrote.txt")).unwrap(),
        "子代理写入\n",
        "develop 子代理必须真实落盘"
    );
    assert!(
        workspace.join("child-ran.txt").is_file(),
        "develop 子代理必须真的执行了命令"
    );
    // 第 4 条调用是 bash + run_in_background：子代理没有被授予 job_start，
    // 执行器必须拒绝，并且不得留下任何后台任务。
    let background_result = state
        .live
        .get(&dev_child)
        .unwrap()
        .session
        .events()
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .nth(3)
        .expect("第 4 条工具结果存在");
    assert!(
        background_result.1 && background_result.0.contains("job_start"),
        "未授予 job_start 时 run_in_background 必须被拒：{background_result:?}"
    );
    assert!(
        state.runtime.jobs().list(&dev_child).is_empty(),
        "被拒的后台命令不得在 jobs 注册表里留下任务"
    );
    // 子代理不能派遣子代理：它的工具面里没有派遣入口。
    let dev_tools = state
        .live
        .get(&dev_child)
        .unwrap()
        .session
        .header()
        .subagent
        .clone()
        .unwrap()
        .effective_tools
        .unwrap();
    assert!(!dev_tools.iter().any(|name| name == "spawn_agent"));

    // —— explore：只读上限，幻觉调用也被拒且文件不变 ——
    let explore_parent = new_session(workspace.clone()).await;
    assert!(
        client
            .put(format!("{base}/api/sessions/{explore_parent}/permission"))
            .json(&json!({"mode": "full"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(
        client
            .post(format!("{base}/api/sessions/{explore_parent}/prompt"))
            .json(
                &json!({"prompt":"委派只读探索","provider":"runtime-test","model":"explore-parent"})
            )
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let explore_child = wait_for_child(&state, &explore_parent).await;
    wait_for_tool_results(&state, &explore_child, 2).await;
    let outcomes: Vec<(String, bool)> = state
        .live
        .get(&explore_child)
        .unwrap()
        .session
        .events()
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::ToolResult {
                content, is_error, ..
            } => Some((content.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(outcomes.len(), 2, "幻觉调用必须各有一条结果为证");
    for (content, is_error) in &outcomes {
        assert!(*is_error, "explore 的写与命令必须被拒，实际结果：{content}");
    }
    assert!(
        outcomes
            .iter()
            .any(|(content, _)| content.contains("subagent/tool-not-granted")
                || content.contains("硬禁用")),
        "拒绝理由必须可判定：{outcomes:?}"
    );
    assert!(
        !workspace.join("explore-wrote.txt").exists(),
        "只读子代理不得留下任何文件变化"
    );
}

/// §10.4/§10.5：子代理正常结束时清理它自己的后台任务并在结果里注明；
/// 完成通知必须带 profile、终态、本次用量与结果引用、并明示截断。
#[tokio::test]
async fn settled_child_cancels_leftover_jobs_and_reports_a_rich_notice() {
    let (state, _) = setup().await;
    let state = Arc::new(state);
    let workspace = state.home.join("jobsfixture");
    std::fs::create_dir_all(&workspace).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::api::router().with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    let created: Value = client
        .post(format!("{base}/api/sessions"))
        .json(&json!({"cwd": workspace}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let parent = created["session"]["id"].as_str().unwrap().to_string();
    assert!(
        client
            .put(format!("{base}/api/sessions/{parent}/permission"))
            .json(&json!({"mode": "full"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(
            client
                .post(format!("{base}/api/sessions/{parent}/prompt"))
                .json(&json!({"prompt":"委派带后台任务的子任务","provider":"runtime-test","model":"jobs-parent"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
    let child = wait_for_child(&state, &parent).await;
    // 等子代理结束，并且等它启动的后台任务被清理 + 通知送达父会话。
    let mut notice = String::new();
    for _ in 0..1500 {
        let delivered = state.live.get(&parent).map(|live| {
            live.session
                .events()
                .iter()
                .filter_map(|event| match &event.event {
                    SessionEvent::AgentInbox { text, source, .. }
                    | SessionEvent::AgentDelivery { text, source, .. }
                        if source == "subagent-settled" && text.contains(&child) =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .next_back()
        });
        if let Some(Some(text)) = delivered {
            notice = text;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!notice.is_empty(), "子代理结束通知必须送达父会话");
    assert!(notice.contains("子代理执行结束"), "{notice}");
    assert!(
        notice.contains("inline:后台任务员"),
        "通知必须带 profile：{notice}"
    );
    assert!(
        notice.contains("结果：已完成"),
        "通知必须带明确终态：{notice}"
    );
    assert!(
        notice.contains("本次子代理用量："),
        "通知必须带子代理自身用量：{notice}"
    );
    assert!(
        notice.contains("结果引用："),
        "通知必须带结果引用：{notice}"
    );
    assert!(
        notice.contains("[已取消的子代理后台工作]"),
        "遗留的后台任务必须被取消并注明：{notice}"
    );
    // 清理与结果投递已解耦:通知到达不等于清理已完成——但清理必须有界
    // 完成,否则"只释放槽位、进程继续写文件"就是最坏结果。
    let mut cleaned = false;
    for _ in 0..300 {
        if state
            .runtime
            .jobs()
            .list(&child)
            .iter()
            .all(|job| job.finished_at.is_some())
        {
            cleaned = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(cleaned, "子代理结束后不得留下未完成的后台任务");
}

/// §10.1 崩溃窗口：派遣的每一步失败都必须回滚本次新增的会话、槽位与目录
/// 条目；而"已入队"之后失败**不**回滚——child 的会话、首条 inbox 与完成
/// 通知是持久事实，工具结果丢了也不能删掉正在运行的 child。
#[tokio::test]
async fn delegate_rolls_back_every_pre_enqueue_window_and_keeps_post_enqueue_state() {
    for point in ["create", "snapshot", "pre-enqueue", "enqueue"] {
        let (state, ctx) = setup().await;
        let parent = ctx.session_id.clone().unwrap();
        state.runtime.set_test_fault(point);
        let before = state.sessions.list().unwrap().len();
        let result = state
            .runtime
            .execute("spawn_agent", json!({"prompt":"注入故障"}), &ctx)
            .await;
        assert!(result.is_err(), "{point}：派遣必须失败");
        assert_eq!(
            state.sessions.list().unwrap().len(),
            before,
            "{point}：不得留下新会话"
        );
        assert_eq!(
            state.runtime.inner.admission.reserved_count(),
            0,
            "{point}：槽位必须释放"
        );
        assert!(
            state.runtime.list(&parent).as_array().unwrap().is_empty(),
            "{point}：目录里不得残留 child"
        );
    }

    // 返回 pending 之前失败：不回滚，且完成通知仍能送达（结果不丢）。
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    state.runtime.set_test_fault("post-enqueue");
    let error = state
        .runtime
        .execute("spawn_agent", json!({"prompt":"注入故障"}), &ctx)
        .await
        .unwrap_err();
    assert!(error.contains("返回 pending 前"), "{error}");
    let child = wait_for_child(&state, &parent).await;
    assert!(
        state.sessions.exists(&child),
        "入队后失败不得删除已存在的 child 会话"
    );
    // child 的首条委派消息是持久事实。
    assert!(
            state
                .live
                .get(&child)
                .unwrap()
                .session
                .events()
                .iter()
                .any(|event| matches!(&event.event, SessionEvent::AgentInbox { source, .. } if source.starts_with("agent:"))),
            "委派消息必须已持久化"
        );
    for _ in 0..1000 {
        let delivered = state.live.get(&parent).is_some_and(|live| {
                live.session.events().iter().any(|event| {
                    matches!(&event.event, SessionEvent::AgentInbox { source, .. } if source == "subagent-settled")
                })
            });
        if delivered {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
            state.live.get(&parent).unwrap().session.events().iter().any(
                |event| matches!(&event.event, SessionEvent::AgentInbox { source, .. } if source == "subagent-settled")
            ),
            "工具结果丢失不能导致完成通知丢失"
        );
}

/// 唤醒不得因"目标正在跑"而丢失。
///
/// 这条锁死了主代理"卡在 wait_agent 等子代理、子代理此刻结束"的交错：
/// 子代理 enqueue 触发 wake 时主代理 `running == true`，resume_pending 的
/// CAS 失败。以前这里直接 return，唤醒意图被静默丢弃，且主代理那轮
/// on_idle 的 inbox 扫描发生在投递之前的概率很大 —— 结果就是"子代理已经
/// 完成，主代理一直不醒，必须用户再发一条消息"。
#[tokio::test]
async fn wake_is_deferred_instead_of_dropped_when_target_is_running() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let live = state.live.get(&parent).unwrap();

    // 主代理正在跑（模拟 wait_agent 那一轮）：CAS 失败才走挂起分支。
    live.running.store(true, Ordering::SeqCst);
    assert!(
        live.running.load(Ordering::SeqCst),
        "测试前提：会话处于运行中"
    );

    let runtime = state.runtime.clone();
    // resume_pending 从这里取模型上下文；不登记的话它会在取 selection
    // 时提前返回，根本走不到 running CAS 那条分支，测试就成了假阳性。
    runtime.inner.selections.lock().unwrap().insert(
        parent.clone(),
        ModelSelection {
            provider: "runtime-test".into(),
            model: "done".into(),
            reasoning_effort: None,
        },
    );
    // 投递一条消息：wake 会因 running 失败而挂起。
    runtime
        .enqueue(
            &parent,
            "m1".into(),
            "结果".into(),
            "subagent-settled".into(),
        )
        .await
        .unwrap();

    // wake 是 spawn 出去的异步任务，等它真正跑到 CAS 分支。
    let mut deferred = false;
    for _ in 0..200 {
        if runtime
            .inner
            .deferred_wakes
            .lock()
            .unwrap()
            .contains(&parent)
        {
            deferred = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(deferred, "目标正在跑时必须登记挂起唤醒，而不是静默丢弃");

    // 主代理这一轮结束 → on_idle 消费挂起项。
    // 消费是异步的(wake 在 spawn 的任务里真正推进):等它落地,而不是
    // 要求同步清表。
    live.running.store(false, Ordering::SeqCst);
    runtime.on_idle(&parent);
    let mut consumed = false;
    for _ in 0..200 {
        if !runtime
            .inner
            .deferred_wakes
            .lock()
            .unwrap()
            .contains(&parent)
        {
            consumed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(consumed, "会话空闲后必须消费自己的挂起唤醒");
}

/// 挂起唤醒必须在目标空闲后**真正跑起来**，而不只是被清掉标记。
///
/// 只验标记会被"清表但没 wake"这种假修复骗过去，所以这里断言投递事件
/// 真的进入了模型可见历史（AgentDelivery）。
#[tokio::test]
async fn deferred_wake_actually_delivers_pending_message_after_target_goes_idle() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let live = state.live.get(&parent).unwrap();
    live.running.store(true, Ordering::SeqCst);

    let runtime = state.runtime.clone();
    runtime.inner.selections.lock().unwrap().insert(
        parent.clone(),
        ModelSelection {
            provider: "runtime-test".into(),
            model: "done".into(),
            reasoning_effort: None,
        },
    );
    runtime
        .enqueue(
            &parent,
            "m1".into(),
            "子代理已完成".into(),
            "subagent-settled".into(),
        )
        .await
        .unwrap();

    live.running.store(false, Ordering::SeqCst);
    runtime.on_idle(&parent);

    // wake 是 spawn 出去的异步任务，给它落地时间。
    let mut delivered = false;
    for _ in 0..200 {
        if state.live.get(&parent).is_some_and(|live| {
            live.session
                .events()
                .iter()
                .any(|event| matches!(event.event, SessionEvent::AgentDelivery { .. }))
        }) {
            delivered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        delivered,
        "挂起唤醒必须在目标空闲后真正把待收消息投进模型历史，不能只是清掉标记"
    );
}

/// §8.2：子代理目录有**独立**字节预算，超限按 UTF-8 边界截断并明示；
/// 未列出的定义仍可用 profile_id 指定（截断不等于禁用）。
#[tokio::test]
async fn subagent_catalog_respects_its_own_byte_budget() {
    use denia_tools::capabilities::AgentRuntime;
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let mut profile = denia_core::subagent::builtin_subagent_profiles()[0].clone();
    profile.id = "budget-probe".to_string();
    profile.name = "预算探针".to_string();
    profile.description = "用途".repeat(300);
    state
        .subagent_profiles
        .create(
            denia_core::subagent::ProfileWriteScope::User,
            None,
            profile,
            "角色".repeat(50),
        )
        .unwrap();
    state
        .settings
        .update("runtime", json!({"subagentCatalogMaxBytes": 1024}), None)
        .unwrap();

    let text = state
        .runtime
        .subagent_catalog(&parent)
        .await
        .unwrap()
        .expect("有可用定义时必须注入目录");
    assert!(
        text.contains("预算截断"),
        "超限必须明示截断而不是静默丢弃：{}",
        &text[..text.len().min(200)]
    );
    assert!(
        text.len() <= 1024 + 200,
        "截断后长度应贴近预算（含一段说明）：{}",
        text.len()
    );
    // 未列出的定义仍可指定：目录截断不影响派遣解析。
    assert!(
        state
            .subagent_profiles
            .resolve(None, "user:budget-probe")
            .is_ok()
    );

    // 提高预算后目录恢复完整（不是永久截断）。
    state
        .settings
        .update(
            "runtime",
            json!({"subagentCatalogMaxBytes": 1_048_576}),
            None,
        )
        .unwrap();
    let full = state
        .runtime
        .subagent_catalog(&parent)
        .await
        .unwrap()
        .unwrap();
    assert!(!full.contains("预算截断"));
    assert!(full.contains("user:budget-probe"));
}

/// §6.4/L04：停止 child 时只关闭**它自己**名下的浏览器 tab。
#[tokio::test]
async fn stopping_a_child_closes_only_its_own_browser_tabs() {
    use std::sync::Mutex as StdMutex;

    #[derive(Default)]
    struct RecordingHub {
        closed: StdMutex<Vec<String>>,
    }
    #[async_trait]
    impl denia_tools::BrowserExecute for RecordingHub {
        async fn execute(
            &self,
            _command: denia_browser::BrowserCommand,
        ) -> denia_browser::CommandOutcome {
            denia_browser::CommandOutcome::ok_value(Value::Null, 0)
        }
        async fn close_owned(&self, owner: &str) -> usize {
            self.closed.lock().unwrap().push(owner.to_string());
            1
        }
    }

    let (state, mut ctx) = setup().await;
    ctx.selection.as_mut().unwrap().model = "hold".into();
    let hub = Arc::new(RecordingHub::default());
    state.runtime.attach_browser(hub.clone());
    let parent = ctx.session_id.clone().unwrap();
    let child = state
        .runtime
        .execute("spawn_agent", json!({"prompt":"占用浏览器"}), &ctx)
        .await
        .unwrap()["childId"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_running(&state, &child).await;
    state.runtime.interrupt(&parent, &child).await.unwrap();
    assert_eq!(
        hub.closed.lock().unwrap().as_slice(),
        std::slice::from_ref(&child),
        "只允许关闭该 child 名下的 tab"
    );
    assert!(
        !hub.closed.lock().unwrap().contains(&parent),
        "父代理的 tab 不得被顺手清掉"
    );
    settle(&state.runtime, &child).await;
}

/// §9.4：旧子代理首次继续前建立模型历史投影（在线与冷恢复一致），
/// 不再只能"查看后重新派遣"。
#[tokio::test]
async fn legacy_child_is_projected_and_can_continue() {
    use denia_core::config::ModelSelection;
    use denia_core::session::{SessionEvent, SubagentDescriptor};

    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let selection = ModelSelection {
        provider: "runtime-test".into(),
        model: "legacy-child".into(),
        reasoning_effort: None,
    };
    let child = state
        .sessions
        .create_subagent(
            &ctx.cwd,
            true,
            &parent,
            SubagentDescriptor::legacy("旧子代理", 1, "default", selection.clone(), None, None),
        )
        .unwrap();
    let child_id = child.id().to_string();
    // 旧日志：父系统提示 + 混入全局规则的自动注入 + 任务投递 + 一轮对话。
    child.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    child
        .append(SessionEvent::SystemPrompt {
            turn: 1,
            step: 1,
            text: "父的系统提示".into(),
        })
        .unwrap();
    child
        .append(SessionEvent::UserMessage {
            text: "全局规则哨兵 GLOBAL-SENTINEL".into(),
            injected: true,
            images: Vec::new(),
            channel: Some("workspace-instructions".into()),
        })
        .unwrap();
    child
        .append(SessionEvent::AgentDelivery {
            id: "m1".into(),
            text: "[父代理 委派任务]\n调查登录流程".into(),
            source: format!("agent:{parent}"),
        })
        .unwrap();
    child
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![denia_core::stream::ContentBlock::Text {
                text: "先看登录入口".into(),
            }],
            usage: None,
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        })
        .unwrap();
    child
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: denia_core::session::TurnEndReason::Completed,
        })
        .unwrap();
    child.flush().unwrap();
    // 目录与父子关系：与真实派遣一致。
    state.runtime.inner.children.lock().unwrap().insert(
        child_id.clone(),
        Child {
            id: child_id.clone(),
            parent_id: parent.clone(),
            descriptor: child.header().subagent.clone().unwrap(),
        },
    );

    state.runtime.resume_pending(&child_id).await.unwrap();

    // 投影已建立并落盘；父代理不再收到"无法继续"的诊断。
    let live = state.live.get(&child_id).expect("child 已加载");
    assert!(
        live.session.history_projection().is_some(),
        "首次继续必须建立历史投影"
    );
    let projection_path = state
        .sessions
        .root()
        .join(&child_id)
        .join(denia_session::HISTORY_PROJECTION_FILE);
    assert!(projection_path.is_file(), "投影必须落盘");
    assert!(
            !state
                .live
                .get(&parent)
                .unwrap()
                .session
                .events()
                .iter()
                .any(|event| matches!(&event.event, SessionEvent::AgentInbox { source, .. } if source == "subagent-legacy")),
            "不再走'拒绝续跑'分支"
        );
    let model_view_has_sentinel = live.session.with_model_events(|events| {
        events.iter().any(|item| {
            matches!(
                &item.event,
                SessionEvent::UserMessage { text, .. } if text.contains("GLOBAL-SENTINEL")
            )
        })
    });
    assert!(!model_view_has_sentinel, "投影后旧注入退出模型历史");
    let migration = state.runtime.list(&parent);
    let entry = migration
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == child_id.as_str())
        .cloned()
        .unwrap();
    assert_eq!(entry["migration"]["needsRedispatch"], false);
    assert_eq!(entry["migration"]["projection"], "applied");

    // 真的继续一轮：模型请求里不能再出现旧全局规则。
    state
        .runtime
        .enqueue(
            &child_id,
            "continue-1".into(),
            "继续调查".into(),
            format!("agent:{parent}"),
        )
        .await
        .unwrap();
    for _ in 0..500 {
        let done = state.live.get(&child_id).is_some_and(|live| {
                live.session.events().iter().any(|event| {
                    matches!(&event.event, SessionEvent::AssistantMessage { blocks, .. }
                        if blocks.iter().any(|block| matches!(block, denia_core::stream::ContentBlock::Text { text } if text.contains("已按投影继续"))))
                })
            });
        if done {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let continued = state.live.get(&child_id).unwrap();
    assert!(
            continued.session.events().iter().any(|event| matches!(
                &event.event,
                SessionEvent::AssistantMessage { blocks, .. }
                    if blocks.iter().any(|block| matches!(block, denia_core::stream::ContentBlock::Text { text } if text.contains("已按投影继续")))
            )),
            "旧子代理必须能真的继续运行"
        );
}

/// T11：child 可以给**直接父代理**发消息，但发给兄弟或无关会话必须被拒。
#[tokio::test]
async fn child_messages_parent_but_not_siblings_or_strangers() {
    use denia_tools::capabilities::AgentRuntime;
    let (state, mut ctx) = setup().await;
    ctx.selection.as_mut().unwrap().model = "hold".into();
    let parent = ctx.session_id.clone().unwrap();
    let first = state
        .runtime
        .execute("spawn_agent", json!({"prompt":"第一个"}), &ctx)
        .await
        .unwrap()["childId"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_running(&state, &first).await;
    let second = state
        .runtime
        .execute("spawn_agent", json!({"prompt":"第二个"}), &ctx)
        .await
        .unwrap()["childId"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_running(&state, &second).await;

    // 子 → 直接父：允许。
    let mut child_ctx = ctx.clone();
    child_ctx.session_id = Some(first.clone());
    state
        .runtime
        .execute(
            "send_message",
            json!({"target": parent, "message": "汇报进度"}),
            &child_ctx,
        )
        .await
        .unwrap();
    // 子 → 兄弟：拒绝（只有直接父/子关系可通信）。
    let sibling = state
        .runtime
        .execute(
            "send_message",
            json!({"target": second, "message": "越级"}),
            &child_ctx,
        )
        .await;
    assert!(sibling.is_err(), "兄弟之间不得直接通信：{sibling:?}");
    // 子 → 无关会话：拒绝。
    let stranger = state
        .sessions
        .create(&ctx.cwd, true)
        .unwrap()
        .id()
        .to_string();
    let unrelated = state
        .runtime
        .execute(
            "send_message",
            json!({"target": stranger, "message": "无关"}),
            &child_ctx,
        )
        .await;
    assert!(unrelated.is_err(), "无关会话不得被子代理发消息");
    state.runtime.interrupt(&parent, &first).await.unwrap();
    state.runtime.interrupt(&parent, &second).await.unwrap();
    settle(&state.runtime, &first).await;
    settle(&state.runtime, &second).await;
}

/// T03：verify 轨道——授权是"只读+命令"（无写工具），角色正文要求如实
/// 报告失败证据；子代理不能靠伪装成功收尾。
#[tokio::test]
async fn verify_track_grants_commands_without_write_tools() {
    use denia_tools::capabilities::AgentRuntime;
    let (state, mut ctx) = setup().await;
    ctx.selection.as_mut().unwrap().model = "hold".into();
    let parent = ctx.session_id.clone().unwrap();
    let child = state
        .runtime
        .execute(
            "spawn_agent",
            json!({"prompt":"运行测试并如实报告失败","profile_id":"builtin:verify"}),
            &ctx,
        )
        .await
        .unwrap()["childId"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_running(&state, &child).await;
    let prompt = state
        .runtime
        .subagent_prompt(&child)
        .await
        .unwrap()
        .expect("verify child 必须有运行快照");
    assert!(
        prompt.effective_tools.iter().any(|name| name == "bash"),
        "verify 必须能运行真实命令：{:?}",
        prompt.effective_tools
    );
    for forbidden in ["write_file", "edit", "job_start"] {
        assert!(
            !prompt.effective_tools.iter().any(|name| name == forbidden),
            "verify 不得拿到 {forbidden}：{:?}",
            prompt.effective_tools
        );
    }
    assert!(
        !prompt
            .effective_tools
            .iter()
            .any(|name| name.starts_with("mcp__")),
        "verify 默认不拿 MCP 工具"
    );
    assert!(prompt.instructions.contains("失败就报失败"));
    assert_eq!(prompt.permission_ceiling.as_deref(), Some("inherit"));
    state.runtime.interrupt(&parent, &child).await.unwrap();
    settle(&state.runtime, &child).await;
}

#[tokio::test]
async fn admission_interrupt_and_owner_cleanup() {
    let (state, mut ctx) = setup().await;
    ctx.selection.as_mut().unwrap().model = "hold".into();
    state
        .settings
        .update(
            crate::subagents::SETTINGS_NS,
            json!({"maxConcurrentRuns":1}),
            None,
        )
        .unwrap();
    let result = state
        .runtime
        .execute("spawn_agent", json!({"prompt":"保持运行"}), &ctx)
        .await
        .unwrap();
    let id = result["childId"].as_str().unwrap();
    assert!(
        state
            .runtime
            .execute("spawn_agent", json!({"prompt":"超额"}), &ctx)
            .await
            .is_err()
    );
    assert!(
        state
            .runtime
            .remove_owner(ctx.session_id.as_deref().unwrap())
            .await
            .is_err()
    );
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while state
            .live
            .get(id)
            .is_none_or(|l| l.cancel.lock().unwrap().is_none())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    state
        .runtime
        .interrupt(ctx.session_id.as_deref().unwrap(), id)
        .await
        .unwrap();
    settle(&state.runtime, id).await;
    state
        .runtime
        .remove_owner(ctx.session_id.as_deref().unwrap())
        .await
        .unwrap();
    assert!(state.sessions.load(id).is_err());
}

/// goal 模式端到端:设置目标后空闲会话自动续跑,跑到轮次上限自动停;
/// 状态转换 fail loud;清除后回归无目标。
#[tokio::test]
async fn goal_rounds_auto_continue_and_stop_at_max_rounds() {
    let (state, _) = setup().await;
    state
        .settings
        .update("goals", json!({"maxRounds": 2}), None)
        .unwrap();
    let state = Arc::new(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::api::router().with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    let created: Value = client
        .post(format!("{base}/api/sessions"))
        .json(&json!({"cwd": state.home}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["session"]["id"].as_str().unwrap().to_string();
    let goal_url = format!("{base}/api/sessions/{id}/goal");

    // 预热一轮:goal 续跑的 selection 依赖请求头回推(先跑过普通轮)。
    let status = client
        .post(format!("{base}/api/sessions/{id}/prompt"))
        .json(&json!({"prompt":"预热","provider":"runtime-test","model":"done"}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, axum::http::StatusCode::ACCEPTED);
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let live = state.live.get(&id).unwrap();
            if !live.running.load(Ordering::SeqCst) && live.session.next_turn_number() > 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();

    // 初始:无目标;非法转换 fail loud。
    let view: Value = client
        .get(&goal_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(view["goal"].is_null());
    assert_eq!(
        client
            .post(&goal_url)
            .json(&json!({"action":"pause"}))
            .send()
            .await
            .unwrap()
            .status(),
        axum::http::StatusCode::BAD_REQUEST
    );

    // 设置目标:空闲会话立即自动续跑,跑到轮次上限(maxRounds=2)停止。
    // token 预算给得足够宽:本测试钉的是**轮次上限**(跑满两轮后不再
    // 自动续跑)。“缺 usage 转保守估算”之后,一轮 mock 轮的估算用量会
    // 达到数万 token——原先那个 123 的小预算在任何真实计费下都撑不过
    // 一轮,会把这条断言挟持成预算耗尽路径。
    client
        .post(&goal_url)
        .json(&json!({"action":"set","objective":"验证 goal 续跑","tokenBudget":10_000_000}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            let live = state.live.get(&id).unwrap();
            let rounds = live.session.goal().map(|g| g.rounds_started).unwrap_or(0);
            if rounds >= 2 && !live.running.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let live = state.live.get(&id).unwrap();
    let goal = live.session.goal().unwrap();
    assert_eq!(goal.rounds_started, 2, "跑满两轮后必须停止");
    assert_eq!(
        goal.status,
        denia_core::session::GoalStatus::Active,
        "轮次耗尽保持 active(不自动续,但状态不谎报)"
    );
    let events = live.session.events();
    // 目标状态块(channel=goal)在每轮内容变化时注入一次:轮次与
    // 用量随轮推进,状态块逐轮更新;不再有单独的 goal-round 消息。
    let injected = events
        .iter()
        .filter(|e| {
            matches!(
                &e.event,
                SessionEvent::UserMessage {
                    channel: Some(c),
                    injected: true,
                    ..
                } if c == "goal"
            )
        })
        .count();
    assert_eq!(injected, 2, "每轮一条 goal 状态注入");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(&e.event, SessionEvent::Goal { op: GoalOp::Round }))
            .count(),
        2
    );
    assert!(events.iter().any(|e| matches!(
        &e.event,
        SessionEvent::UserMessage { text, .. }
            if text.contains("[denia 目标]") && text.contains("已发起续跑轮数:1")
    )));
    assert!(events.iter().any(|e| matches!(
        &e.event,
        SessionEvent::UserMessage { text, .. } if text.contains("已发起续跑轮数:2")
    )));

    // 暂停/恢复/清除的状态转换。
    let view: Value = client
        .post(&goal_url)
        .json(&json!({"action":"pause"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["goal"]["status"], "paused");
    assert_eq!(
        client
            .post(&goal_url)
            .json(&json!({"action":"pause"}))
            .send()
            .await
            .unwrap()
            .status(),
        axum::http::StatusCode::BAD_REQUEST
    );
    let view: Value = client
        .post(&goal_url)
        .json(&json!({"action":"resume"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["goal"]["status"], "active");
    let view: Value = client
        .post(&goal_url)
        .json(&json!({"action":"clear"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(view["goal"].is_null());
    // 预算超出全局上限:拒绝(fail loud)。
    client
        .post(&goal_url)
        .json(&json!({"action":"set","objective":"预算校验","tokenBudget":999_999_999}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .expect_err("预算超上限必须失败");
    server.abort();
}
/// 暂停后恢复不得被同一条旧 `TurnEnd{Aborted}` 打回 paused。
///
/// 缺陷形状:判定"上一轮结局"取整份日志里最后一条 `TurnEnd`,没有水位
/// 也没有已处理标记;日志 append-only,暂停前那条 `Aborted` 一直留在那里,
/// 于是 resume 之后 `continue_goal` 读到的还是它,又落一次 Pause——判据
/// 不变,恢复必然立刻失效。
///
/// 反向语义同时覆盖:水位之后的**新** Aborted 必须仍然暂停目标(连续
/// 两次手动停止都要生效)。
#[tokio::test]
async fn goal_resume_is_not_reverted_by_stale_abort() {
    let (state, _) = setup().await;
    let state = Arc::new(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::api::router().with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();

    // 轮询到条件成立;超时即失败,并把当前目标状态带进报错。
    async fn wait_until(
        state: &crate::state::AppState,
        id: &str,
        label: &str,
        check: impl Fn(&crate::state::LiveSession) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let live = state.live.get(id).unwrap();
            if check(&live) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "等待超时:{label};当前目标 = {:?}",
                live.session.goal()
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    let created: Value = client
        .post(format!("{base}/api/sessions"))
        .json(&json!({"cwd": state.home}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["session"]["id"].as_str().unwrap().to_string();
    let goal_url = format!("{base}/api/sessions/{id}/goal");

    // 目标轮用 hold 模型(流永不产出):轮次挂住,便于手动停止出一个
    // 真实的 `TurnEnd{Aborted}`。
    client
        .post(&goal_url)
        .json(&json!({
            "action": "set",
            "objective": "验证暂停恢复不被旧结局打回",
            "provider": "runtime-test",
            "model": "hold"
        }))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    wait_until(&state, &id, "目标设置后自动开跑第一轮", |live| {
        live.running.load(Ordering::SeqCst)
            && live.session.goal().is_some_and(|g| g.rounds_started == 1)
    })
    .await;

    // 第一次手动停止:水位之后的 Aborted → 目标自动暂停(原有语义)。
    client
        .post(format!("{base}/api/sessions/{id}/cancel"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    wait_until(&state, &id, "手动停止后目标自动暂停", |live| {
        live.session
            .goal()
            .is_some_and(|g| matches!(g.status, denia_core::session::GoalStatus::Paused))
    })
    .await;

    // 恢复:旧 Aborted 不得再把目标打回 paused——恢复必须真的开新轮。
    let view: Value = client
        .post(&goal_url)
        .json(&json!({"action":"resume"}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(view["goal"]["status"], "active");
    wait_until(&state, &id, "恢复后开新轮", |live| {
        live.session.goal().is_some_and(|g| g.rounds_started == 2)
    })
    .await;
    let view: Value = client
        .get(&goal_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        view["goal"]["status"], "active",
        "恢复之后不得再被同一条旧 Aborted 暂停:{}",
        view["goal"]
    );

    // 第二次手动停止:水位之后的新 Aborted 仍必须暂停目标。
    client
        .post(format!("{base}/api/sessions/{id}/cancel"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    wait_until(&state, &id, "第二次手动停止仍暂停目标", |live| {
        live.session
            .goal()
            .is_some_and(|g| matches!(g.status, denia_core::session::GoalStatus::Paused))
    })
    .await;
    let live = state.live.get(&id).unwrap();
    assert_eq!(
        live.session.goal().map(|g| g.rounds_started),
        Some(2),
        "暂停态不得再推进轮次"
    );
    assert_eq!(
        live.session
            .events()
            .iter()
            .filter(|e| matches!(
                &e.event,
                SessionEvent::TurnEnd {
                    reason: TurnEndReason::Aborted { .. },
                    ..
                }
            ))
            .count(),
        2,
        "两次手动停止各留一条 Aborted"
    );
    server.abort();
}
/// `on_idle` 的 paused 判定必须与续跑分类共用同一水位:resume 走的是
/// 自动续跑路径(不经过 `human_turn`,清不掉 paused),若仍按"整份日志
/// 最后一条 TurnEnd"判定,暂停前那条 Aborted 会把已恢复的会话一直标成
/// "用户刚停过",新轮次的唤醒全被挡住。
#[tokio::test]
async fn on_idle_pause_flag_ignores_turn_ends_before_last_goal_op() {
    let (state, ctx) = setup().await;
    let owner = ctx.session_id.as_deref().unwrap();
    let live = state.live.get(owner).unwrap();
    let status = || live.session.goal().map(|goal| goal.status);

    // 目标 active → 一轮被用户手动停止 → 目标暂停(Aborted 留在水位之前)。
    live.session
        .apply_goal(GoalOp::Set {
            objective: "验证 paused 水位".into(),
            token_budget: None,
        })
        .unwrap();
    live.session
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Aborted { cause: None },
        })
        .unwrap();
    live.session.apply_goal(GoalOp::Pause).unwrap();
    assert_eq!(status(), Some(denia_core::session::GoalStatus::Paused));

    // resume 落成新水位:它之前那条 Aborted 不得再设置 paused 标记。
    live.session.apply_goal(GoalOp::Resume).unwrap();
    assert_eq!(status(), Some(denia_core::session::GoalStatus::Active));
    state.runtime.on_idle(owner);
    assert!(
        !state.runtime.inner.paused.lock().unwrap().contains(owner),
        "水位之前的旧 Aborted 不得再设置 paused 标记"
    );

    // 水位之后的新 Aborted 仍然生效:连续两次手动停止都要能暂停。
    live.session
        .append(SessionEvent::TurnEnd {
            turn: 2,
            reason: TurnEndReason::Aborted { cause: None },
        })
        .unwrap();
    state.runtime.on_idle(owner);
    assert!(
        state.runtime.inner.paused.lock().unwrap().contains(owner),
        "水位之后的新 Aborted 必须仍然设置 paused 标记"
    );
}
#[tokio::test]
async fn cancelled_start_leaves_no_child_and_config_rejects_invalid() {
    let (state, ctx) = setup().await;
    ctx.cancel.cancel();
    assert!(
        state
            .runtime
            .execute("spawn_agent", json!({"prompt":"不应创建"}), &ctx)
            .await
            .is_err()
    );
    assert!(
        state
            .runtime
            .list(ctx.session_id.as_deref().unwrap())
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        state
            .settings
            .update("runtime", json!({"maxDepth":0}), None)
            .is_err()
    );
    // 项目记忆预算:越界拒绝;布尔开关与合法预算放行。
    assert!(
        state
            .settings
            .update("runtime", json!({"projectMemoryMaxBytes":0}), None)
            .is_err()
    );
    assert!(
        state
            .settings
            .update(
                "runtime",
                json!({"projectMemoryMaxBytes":25600,"memoryEnabled":false}),
                None
            )
            .is_ok()
    );
}

#[tokio::test]
async fn http_prompt_tools_jobs_skills_and_child_controls() {
    let (state, _) = setup().await;
    std::fs::create_dir_all(state.home.join("skills/runtime-test")).unwrap();
    std::fs::write(
        state.home.join("skills/runtime-test/SKILL.md"),
        "---\nname: runtime-test\ndescription: 接口验证技能\n---\n读取并验证运行时结果。",
    )
    .unwrap();
    let state = Arc::new(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::api::router().with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    let created: Value = client
        .post(format!("{base}/api/sessions"))
        .json(&json!({"cwd":state.home}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["session"]["id"].as_str().unwrap();
    assert_eq!(client.post(format!("{base}/api/sessions/{id}/prompt")).json(&json!({"prompt":"验证三个系统","provider":"runtime-test","model":"orchestrator","skills":["runtime-test"]})).send().await.unwrap().status(),axum::http::StatusCode::ACCEPTED);
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            let live = state.live.get(id).unwrap();
            let count = live
                .session
                .events()
                .iter()
                .filter(|e| matches!(e.event, SessionEvent::ToolResult { .. }))
                .count();
            if count >= 4 && !live.running.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    let live = state.live.get(id).unwrap();
    let events = live.session.events();
    assert!(
        !events
            .iter()
            .any(|e| matches!(e.event, SessionEvent::ToolResult { is_error: true, .. })),
        "工具链不得出错：{events:?}"
    );
    let children: Value = client
        .get(format!("{base}/api/sessions/{id}/agents"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let child = children["agents"][0]["id"].as_str().unwrap();
    assert!(
        !client
            .post(format!("{base}/api/sessions/{child}/prompt"))
            .json(&json!({"prompt":"直接写入"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let jobs: Value = client
        .get(format!("{base}/api/sessions/{id}/jobs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let job = jobs["jobs"][0]["id"].as_str().unwrap();
    state
        .runtime
        .jobs()
        .wait(job, id, 10_000, &CancellationToken::new())
        .await
        .unwrap();
    let output: Value = client
        .get(format!("{base}/api/sessions/{id}/jobs/{job}/output"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(output["output"].as_str().unwrap().contains("runtime-e2e"));
    assert!(
        !client
            .get(format!("{base}/api/sessions/{child}/jobs/{job}/output"))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let skills: Value = client
        .get(format!("{base}/api/sessions/{id}/skills"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        skills["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "runtime-test")
    );
    // skill load 的 SKILL.md 全文直接随工具结果返回（一次进历史），
    // 能力上下文不再携带已加载正文。
    let live = state.live.get(id).unwrap();
    let events = live.session.events();
    let skill_call = events
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEvent::ToolCall { call_id, name, .. } if name == "skill" => {
                Some(call_id.clone())
            }
            _ => None,
        })
        .expect("skill load 工具调用应已落库");
    let load_result = events
        .iter()
        .rev()
        .find_map(|e| match &e.event {
            SessionEvent::ToolResult {
                call_id,
                content,
                is_error: false,
                ..
            } if *call_id == skill_call => Some(content.clone()),
            _ => None,
        })
        .expect("skill load 工具结果应已落库");
    assert!(
        load_result.contains("读取并验证运行时结果。"),
        "SKILL.md 正文应随工具结果返回：{load_result}"
    );
    let context = state
        .runtime
        .context(id, &state.home)
        .await
        .unwrap()
        .join("\n");
    assert!(
        !context.contains("读取并验证运行时结果。"),
        "能力上下文不应再携带技能正文：{context}"
    );
    server.abort();
}

/// 项目记忆索引注入(运行时接线):选择层按触碰路径与最近用户消息裁到
/// 相关条目,被省略的条目在渲染层留全文指针;空桶不注入。
#[tokio::test]
async fn memory_index_injection_carries_the_whole_index() {
    let home = std::env::temp_dir().join(format!("denia-memory-inject-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let state = crate::state::build_state(&home, false, 43654)
        .await
        .unwrap();
    let cwd = home.join("ws");
    std::fs::create_dir_all(&cwd).unwrap();
    let root = crate::project_memory::memory_root(&home, &cwd);
    std::fs::create_dir_all(&root).unwrap();

    // 空桶不注入——目录路径已由装配阶段内联进「# 项目记忆」段。
    assert!(
        state
            .runtime
            .project_memory_index(&cwd)
            .await
            .unwrap()
            .is_none()
    );

    std::fs::write(
            root.join(crate::project_memory::MEMORY_INDEX_FILE),
            "# 项目记忆\n- [注入通道](injections.md) — 每步注入与幂等基准\n- [面板配色](frontend-css.md) — 设置页样式\n",
        )
        .unwrap();
    // 整份索引都在正文里:不再按触碰路径或用户消息筛条目。
    let block = state
        .runtime
        .project_memory_index(&cwd)
        .await
        .unwrap()
        .expect("有索引时必须注入");
    assert!(block.contains("injections.md"), "{block}");
    assert!(block.contains("frontend-css.md"), "{block}");
    assert!(!block.contains("另有"), "整份注入不留省略指针:{block}");
    assert!(block.contains("/MEMORY.md"), "{block}");

    // 正文只由索引文本决定:同一索引反复调用必须逐字相同,否则通道的
    // 幂等比较会在每个 step 都判成“变了”而重发。
    let again = state
        .runtime
        .project_memory_index(&cwd)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(block, again);

    // 索引本身变了,正文才变。
    std::fs::write(
        root.join(crate::project_memory::MEMORY_INDEX_FILE),
        "- [只留一条](only.md) — 新索引\n",
    )
    .unwrap();
    let changed = state
        .runtime
        .project_memory_index(&cwd)
        .await
        .unwrap()
        .unwrap();
    assert!(changed.contains("only.md"), "{changed}");
    assert_ne!(block, changed);

    std::fs::remove_dir_all(home).unwrap();
}

/// 交接信息的**单一来源**:同一个子代理上,`settle` 注入父会话的通知文本与
/// `wait_agent` 的返回对象必须由同一份结构渲染(逐项对得上);并且"子代理
/// 自称完成"与"宿主验证"必须能分开读出来 —— 默认 develop 子代理没有账本
/// 工具,`hostVerified` 必须是 false,未验证提醒必须落在注入文本里。
#[tokio::test]
async fn settlement_notice_and_wait_agent_agree_on_the_same_handoff() {
    use denia_tools::capabilities::AgentRuntime;

    let (state, base_ctx) = setup().await;
    let state = Arc::new(state);
    let workspace = state.home.join("handofffixture");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("existing.txt"), "旧内容\n").unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = crate::api::router().with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    let created: Value = client
        .post(format!("{base}/api/sessions"))
        .json(&json!({"cwd": workspace}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let parent = created["session"]["id"].as_str().unwrap().to_string();
    assert!(
        client
            .put(format!("{base}/api/sessions/{parent}/permission"))
            .json(&json!({"mode": "full"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    assert!(
        client
            .post(format!("{base}/api/sessions/{parent}/prompt"))
            .json(&json!({"prompt":"委派开发任务","provider":"runtime-test","model":"exec-parent"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let child = wait_for_child(&state, &parent).await;
    wait_for_tool_results(&state, &child, 4).await;

    // 等完成通知真的落进父会话:AgentInbox / AgentDelivery 里的那段文本就是
    // 被注入父上下文的那份(注入通道不另改)。
    let mut notice = String::new();
    for _ in 0..1500 {
        let delivered = state.live.get(&parent).map(|live| {
            live.session
                .events()
                .iter()
                .filter_map(|event| match &event.event {
                    SessionEvent::AgentInbox { text, source, .. }
                    | SessionEvent::AgentDelivery { text, source, .. }
                        if source == "subagent-settled" && text.contains(&child) =>
                    {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .next_back()
        });
        if let Some(Some(text)) = delivered {
            notice = text;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(!notice.is_empty(), "子代理结束通知必须送达父会话");

    let mut ctx = base_ctx.clone();
    ctx.session_id = Some(parent.clone());
    let ready = state
        .runtime
        .execute("wait_agent", json!({"target":child}), &ctx)
        .await
        .unwrap();
    // 既有契约不变(向后兼容:只加字段,不改既有字段的取值)。
    assert_eq!(ready["status"], "ready");
    assert_eq!(ready["running"], false);
    assert_eq!(ready["id"], json!(child));

    // 终态枚举能说出这是子代理自己说的完成,而不是宿主验收过了。
    assert_eq!(ready["terminal"]["outcome"], "completed");
    assert_eq!(ready["terminal"]["basis"], "self_reported");
    assert_eq!(ready["terminal"]["hostVerified"], false);
    assert_eq!(ready["terminal"]["finalized"], true);
    assert_eq!(ready["terminal"]["reason"], "已完成");

    // 用量是结构化字段,不是拼进文本的字符串。
    // 用量是结构化字段,不是拼进文本的字符串:五个分量 + 总数都在。
    for key in [
        "uncachedInputTokens",
        "outputTokens",
        "cacheReadTokens",
        "cacheWriteTokens",
        "reasoningTokens",
        "totalTokens",
    ] {
        assert!(
            ready["usage"][key].is_u64(),
            "usage.{key} 必须是数字字段:{ready}"
        );
    }
    let total = ready["usage"]["totalTokens"].as_u64().unwrap();

    // 变更文件:真写过的在里面;只读的文件不在;口径写清楚。
    let changed: Vec<&str> = ready["changedPaths"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(changed.contains(&"child-wrote.txt"), "{ready}");
    assert!(
        !changed.contains(&"existing.txt"),
        "只读没改的文件不得计入:{ready}"
    );
    assert!(
        ready["changedPathsNote"].as_str().unwrap().contains("bash"),
        "{ready}"
    );
    // 没有产物时给空数组,不报错、不给 null。
    assert!(ready["artifacts"].is_array(), "{ready}");

    // 默认 develop 子代理没有账本工具:拿不到结论给 null,原因与提醒都写明。
    assert!(ready["verification"].is_null(), "{ready}");
    let note = ready["verificationNote"].as_str().unwrap();
    assert!(note.starts_with('无'), "{ready}");
    let advisories: Vec<&str> = ready["advisories"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        advisories.iter().any(|line| line.contains("[未验证]")),
        "{ready}"
    );

    // —— 两处同源:通知文本与返回对象逐项对得上 ——
    assert!(
        notice.contains(&format!(
            "子代理：{child}（{}）",
            ready["profile"].as_str().unwrap()
        )),
        "{notice}"
    );
    assert!(
        notice.contains(&format!(
            "终态：{}",
            ready["terminal"]["outcome"].as_str().unwrap()
        )),
        "{notice}"
    );
    assert!(
        notice.contains(&format!("本次子代理用量：{total} tokens")),
        "通知里的用量必须与 wait_agent 的 usage 同源:{notice}"
    );
    assert!(notice.contains("child-wrote.txt"), "{notice}");
    for line in &advisories {
        assert!(
            notice.contains(line),
            "父侧注入文本必须带上这条标注:{line}\n{notice}"
        );
    }
    std::fs::remove_dir_all(state.home.clone()).ok();
}

/// 有账本且验收通过时,"宿主验证"必须为真 —— 与"自称完成"分开。
///
/// 这里直接给子会话塞一条账本事件(它就是宿主执行链的产物)再算交接信息,
/// 断言终态依据从 `self_reported` 变成 `host_verified`,且不再出现未验证提醒。
/// 账本现在只有验证结论这一类记录:当前 revision 跟着最新一条结论走,不再有
/// `open` / `close` 那样的开账与收口事件。
#[tokio::test]
async fn handoff_marks_host_verified_when_the_ledger_concludes_passed() {
    use denia_core::config::ModelSelection;
    use denia_core::session::{SessionEvent, SubagentDescriptor};
    use denia_core::task::{CheckRun, FileFingerprint, RevisionId, TaskOp, ValidationResult};

    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let selection = ModelSelection {
        provider: "runtime-test".into(),
        model: "ledger-child".into(),
        reasoning_effort: None,
    };
    let session = state
        .sessions
        .create_subagent(
            &ctx.cwd,
            true,
            &parent,
            SubagentDescriptor::legacy("账本子代理", 1, "default", selection, None, None),
        )
        .unwrap();
    let child_id = session.id().to_string();
    let fingerprint = FileFingerprint {
        path: "src/lib.rs".into(),
        digest: "d1".into(),
    };
    let result = ValidationResult::from_check_runs(
        RevisionId::new("rev-a"),
        vec![CheckRun {
            command: "cargo test".into(),
            exit_code: Some(0),
            expect_exit_code: 0,
            workdir: ".".into(),
            fingerprints: vec![fingerprint],
        }],
        Some(child_id.clone()),
        1,
    )
    .expect("有检查运行才构成结论");
    session
        .append(SessionEvent::Task {
            op: TaskOp::RecordValidation { result },
        })
        .unwrap();
    session
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    session.flush().unwrap();

    let descriptor = session.header().subagent.clone().unwrap();
    let child = Child {
        id: child_id.clone(),
        parent_id: parent.clone(),
        descriptor,
    };
    // 冷加载的实例(事件不驻留)也要能算出同一份交接信息:日志走磁盘回放,
    // 不能把"读不到事件"当成"什么都没做"。
    let live = state.live.get_or_load(&state.sessions, &child_id).unwrap();
    let handoff = subagent_handoff(Some(&child), &live.session, false);

    assert_eq!(
        handoff.terminal.outcome,
        TerminalOutcome::Completed,
        "{:?}",
        handoff.terminal
    );
    assert_eq!(handoff.terminal.basis, TerminalBasis::HostVerified);
    assert!(handoff.terminal.host_verified, "{:?}", handoff.terminal);
    assert!(matches!(
        handoff.verification,
        Some(VerificationState::Verified {
            verdict: ValidationVerdict::Passed,
            ..
        })
    ));
    assert!(
        handoff.verification_note.contains("验收通过"),
        "{}",
        handoff.verification_note
    );
    assert!(handoff.advisories.is_empty(), "{:?}", handoff.advisories);
    // 变更范围不再有账本来源(记账层已删):只从子会话日志归集。这里没有
    // 成功的写调用,所以给空 + 口径,而不是为了填满从别处补一条。
    assert!(
        handoff.changed_paths.is_empty(),
        "{:?}",
        handoff.changed_paths
    );
    assert!(
        handoff
            .changed_paths_note
            .contains("没有执行成功的 write_file / edit"),
        "{}",
        handoff.changed_paths_note
    );
    let notice = handoff.notice_text("正文", "", "");
    assert!(notice.contains("宿主验证 有"), "{notice}");
    assert!(notice.contains("验收通过"), "{notice}");
    assert!(!notice.contains("[未验证]"), "{notice}");
    std::fs::remove_dir_all(state.home.clone()).ok();
}

/// 日志归集的三条边界:执行成功的写算改动、**被拒的写不算改动**、产物引用
/// 按 `outputArtifact` 读出来;无内容时给空数组 / `None` + 口径,不报错。
#[tokio::test]
async fn changed_paths_and_artifacts_follow_the_session_log() {
    use denia_core::config::ModelSelection;
    use denia_core::session::{SessionEvent, SubagentDescriptor};

    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let selection = ModelSelection {
        provider: "runtime-test".into(),
        model: "log-child".into(),
        reasoning_effort: None,
    };
    let session = state
        .sessions
        .create_subagent(
            &ctx.cwd,
            true,
            &parent,
            SubagentDescriptor::legacy("日志子代理", 1, "default", selection, None, None),
        )
        .unwrap();
    let child_id = session.id().to_string();
    let call = |call_id: &str, name: &str, arguments: Value| SessionEvent::ToolCall {
        turn: 1,
        step: 1,
        call_id: call_id.into(),
        name: name.into(),
        arguments: arguments.to_string(),
    };
    session
        .append(call(
            "c1",
            "write_file",
            json!({"path": "child-a.txt", "content": "x"}),
        ))
        .unwrap();
    session
        .append(SessionEvent::ToolResult {
            turn: 1,
            step: 1,
            call_id: "c1".into(),
            content: "已写入 child-a.txt".into(),
            is_error: false,
            error: None,
            error_identity: None,
            meta: None,
            truncation: None,
            replaces: None,
        })
        .unwrap();
    // 被拒的写:调用在日志里,但它没有改动任何东西。
    session
        .append(call(
            "c2",
            "write_file",
            json!({"path": "refused.txt", "content": "x"}),
        ))
        .unwrap();
    session
        .append(SessionEvent::ToolResult {
            turn: 1,
            step: 1,
            call_id: "c2".into(),
            content: "子代理不能使用 write_file".into(),
            is_error: true,
            error: Some("tool-not-granted".into()),
            error_identity: None,
            meta: None,
            truncation: None,
            replaces: None,
        })
        .unwrap();
    // bash:命令间接改动推断不出来(不计入 changedPaths),但产物引用在 meta 里。
    session
        .append(call("c3", "bash", json!({"command": "echo hi"})))
        .unwrap();
    session
        .append(SessionEvent::ToolResult {
            turn: 1,
            step: 1,
            call_id: "c3".into(),
            content: "退出码: 0\nhi\n".into(),
            is_error: false,
            error: None,
            error_identity: None,
            meta: Some(json!({
                "exit_code": 0,
                "end_reason": "completed",
                "outputArtifact": {
                    "output_id": "art-7",
                    "streams": {},
                    "complete": true,
                    "storage_error": null,
                },
            })),
            truncation: None,
            replaces: None,
        })
        .unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![denia_core::stream::ContentBlock::Text {
                text: "写完了".into(),
            }],
            usage: None,
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        })
        .unwrap();
    session
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    session.flush().unwrap();

    let child = Child {
        id: child_id.clone(),
        parent_id: parent.clone(),
        descriptor: session.header().subagent.clone().unwrap(),
    };
    let live = state.live.get_or_load(&state.sessions, &child_id).unwrap();
    let handoff = subagent_handoff(Some(&child), &live.session, false);

    assert_eq!(handoff.changed_paths, vec!["child-a.txt".to_string()]);
    assert!(
        !handoff.changed_paths.contains(&"refused.txt".to_string()),
        "被拒的写不算改动"
    );
    assert_eq!(handoff.artifacts.len(), 1, "{:?}", handoff.artifacts);
    assert_eq!(handoff.artifacts[0].output_id, "art-7");
    assert_eq!(handoff.artifacts[0].tool, "bash");
    assert_eq!(handoff.artifacts[0].complete, Some(true));
    // 没有账本:验证字段给 `None` + 原因,而不是报错。
    assert!(handoff.verification.is_none());
    assert!(!handoff.verification_note.is_empty());
    // 判定沿用子代理自己的终态,但明确标出宿主没有验证结论。
    assert_eq!(handoff.terminal.outcome, TerminalOutcome::Completed);
    assert!(!handoff.terminal.host_verified);
    std::fs::remove_dir_all(state.home.clone()).ok();
}

/// 空值的那一侧:`changedPaths` / `artifacts` 在"确实什么都没发生"时必须是
/// **空数组 + 口径说明**,不是 `null`、不报错 —— "没有改动"与"读不到改动"
/// 必须能分开读,否则父代理会把"读不到"当成"没改"。
#[tokio::test]
async fn handoff_reports_empty_changes_and_artifacts_without_failing() {
    use denia_core::config::ModelSelection;
    use denia_core::session::{SessionEvent, SubagentDescriptor};

    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let selection = ModelSelection {
        provider: "runtime-test".into(),
        model: "quiet-child".into(),
        reasoning_effort: None,
    };
    let session = state
        .sessions
        .create_subagent(
            &ctx.cwd,
            true,
            &parent,
            SubagentDescriptor::legacy("只读子代理", 1, "default", selection, None, None),
        )
        .unwrap();
    let child_id = session.id().to_string();
    // 只读的一轮:没有写调用、没有产物、没有账本。
    session
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![denia_core::stream::ContentBlock::Text {
                text: "看完了,没问题".into(),
            }],
            usage: None,
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        })
        .unwrap();
    session
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    session.flush().unwrap();

    let child = Child {
        id: child_id.clone(),
        parent_id: parent.clone(),
        descriptor: session.header().subagent.clone().unwrap(),
    };
    let live = state.live.get_or_load(&state.sessions, &child_id).unwrap();
    let handoff = subagent_handoff(Some(&child), &live.session, false);

    assert!(
        handoff.changed_paths.is_empty(),
        "{:?}",
        handoff.changed_paths
    );
    assert!(handoff.artifacts.is_empty(), "{:?}", handoff.artifacts);
    assert!(
        handoff.changed_paths_note.contains("没有可识别的落盘改动"),
        "{}",
        handoff.changed_paths_note
    );
    assert!(
        handoff.changed_paths_note.contains("bash"),
        "口径必须说清哪些改动推断不出来:{}",
        handoff.changed_paths_note
    );
    assert!(handoff.verification.is_none());
    let notice = handoff.notice_text("正文", "", "");
    assert!(notice.contains("变更文件：（无）"), "{notice}");
    assert!(notice.contains("产物：（无）"), "{notice}");
    assert!(notice.contains("没有可识别的落盘改动"), "{notice}");
    // 顺手钉住 wire 形状:空就是空数组,验证拿不到就是 null。
    let fields = handoff.fields();
    assert_eq!(fields["changedPaths"], json!([]));
    assert_eq!(fields["artifacts"], json!([]));
    assert!(fields["verification"].is_null());
    std::fs::remove_dir_all(state.home.clone()).ok();
}
#[test]
fn execution_budget_config_defaults_to_unlimited() {
    let defaults = GoalsConfig::default();
    assert_eq!(defaults.max_execution_steps, 0);
    assert_eq!(defaults.max_execution_requests, 0);
    assert_eq!(defaults.max_execution_active_ms, 0);
    assert!(validate_goals_config(json!({})).is_ok());
    // 0 = 显式关闭某一道边界(其余两道仍生效),允许。
    assert!(validate_goals_config(json!({"maxExecutionSteps": 0})).is_ok());
    assert!(validate_goals_config(json!({"maxExecutionSteps": 100_001})).is_err());
    assert!(validate_goals_config(json!({"maxExecutionRequests": 1_000_001})).is_err());
    assert!(validate_goals_config(json!({"maxExecutionActiveMs": 86_400_001})).is_err());
    assert!(validate_goals_config(json!({"maxExecutionSteps": "128"})).is_err());
}

#[tokio::test]
async fn driver_reads_the_execution_budget_from_runtime_config() {
    let (state, _ctx) = setup().await;
    // 上限来源已经装在驱动器上:默认不限(0 映射成 u64::MAX)。
    assert_eq!(
        state.driver.execution_limits(),
        state.runtime.execution_limits()
    );
    assert_eq!(state.driver.execution_limits().max_steps, u64::MAX);
    // 改配置 → 下一次取快照就是新值(在途轮次保持自己的快照,由
    // `turn::budget_tests` 的边界测试钉住)。
    state
        .settings
        .update("goals", json!({"maxExecutionSteps": 7}), None)
        .unwrap();
    assert_eq!(state.driver.execution_limits().max_steps, 7);
    assert_eq!(state.runtime.execution_limits().max_requests, u64::MAX);
    std::fs::remove_dir_all(state.home.clone()).ok();
}

/// 某个子代理在父会话里的完成通知正文(收件箱里的最后一条)。
fn last_settled_notice(
    state: &crate::state::AppState,
    parent: &str,
    child: &str,
) -> Option<String> {
    state.live.get(parent).and_then(|live| {
        live.session
            .events()
            .iter()
            .filter_map(|event| match &event.event {
                SessionEvent::AgentInbox { text, source, .. }
                    if source == "subagent-settled" && text.contains(child) =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
            .next_back()
    })
}

/// 某个子代理在父会话里的完成通知条数(收件箱;已领取的也计)。
fn settled_notice_count(state: &crate::state::AppState, parent: &str, child: &str) -> usize {
    state
        .live
        .get(parent)
        .map(|live| {
            live.session
                .events()
                .iter()
                .filter(|event| {
                    matches!(&event.event, SessionEvent::AgentInbox { text, source, .. }
                            if source == "subagent-settled" && text.contains(child))
                })
                .count()
        })
        .unwrap_or(0)
}
/// 等父会话收件箱累积到 `min` 条完成通知。投递由后台 worker 异步推进,
/// 断言"新增了几条"之前必须先把基线等出来,否则读到 0 会把上一轮的通知
/// 算到这一轮头上。
async fn wait_for_settled_count(
    state: &crate::state::AppState,
    parent: &str,
    child: &str,
    min: usize,
) {
    for _ in 0..500 {
        if settled_notice_count(state, parent, child) >= min {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("父代理未在期限内收到至少 {min} 条子代理完成通知");
}

async fn wait_for_settled_notice(
    state: &crate::state::AppState,
    parent: &str,
    child: &str,
    needle: &str,
) -> String {
    for _ in 0..500 {
        if let Some(text) = last_settled_notice(state, parent, child)
            && text.contains(needle)
        {
            return text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("父代理未在期限内收到包含 {needle:?} 的子代理完成通知");
}

fn completion_is_in_inbox(live: &crate::state::LiveSession, id: &str) -> bool {
    live.session.events().iter().any(|event| {
            matches!(&event.event, SessionEvent::AgentInbox { id: delivered, .. } if delivered == id)
        })
}

fn completion_is_delivered(live: &crate::state::LiveSession, id: &str) -> bool {
    live.session.events().iter().any(|event| {
        matches!(
            &event.event,
            SessionEvent::AgentInbox { id: delivered, .. }
                | SessionEvent::AgentDelivery { id: delivered, .. }
                if delivered == id
        )
    })
}

/// 模型正在输出或执行工具时，完成结果必须停在队列里；动作收尾后
/// 才能进入收件箱——不能把结果插进正在进行的动作里。
#[tokio::test]
async fn completion_waits_for_streaming_and_tool_actions() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let live = state.live.get(&parent).unwrap();
    let runtime = state.runtime.clone();
    let id = "action-boundary".to_string();
    runtime.inner.delivery.push(
        Completion {
            child: parent.clone(),
            target: parent.clone(),
            id: id.clone(),
            legacy_id: id.clone(),
            text: "[后台任务完成] 结果".into(),
            source: "job-completed".into(),
        },
        false,
    );

    let busy = [
        SessionEvent::AssistantChunk {
            turn: 1,
            step: 1,
            chunk: StreamChunk::TextDelta {
                index: 0,
                text: "生成中".into(),
            },
        },
        SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![ContentBlock::ToolCall {
                id: "call".into(),
                name: "bash".into(),
                arguments: "{}".into(),
                incomplete: false,
            }],
            usage: None,
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        },
        SessionEvent::ToolCall {
            turn: 1,
            step: 1,
            call_id: "call".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        },
    ];
    for event in busy {
        runtime.observe_activity(&parent, &event);
        runtime.inner.delivery.expedite(&parent);
        runtime.retry_deliveries().await;
        assert!(
            !completion_is_in_inbox(&live, &id),
            "模型动作进行中不得投递完成结果"
        );
    }

    runtime.observe_activity(&parent, &SessionEvent::StepEnd { turn: 1, step: 1 });
    for _ in 0..200 {
        if completion_is_in_inbox(&live, &id) {
            break;
        }
        runtime.retry_deliveries().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        completion_is_in_inbox(&live, &id),
        "动作结束后完成结果必须立刻投递"
    );
}

/// 模型不在运行时(空闲/未响应)不等待任何动作边界：结果立刻入队并唤醒。
#[tokio::test]
async fn completion_delivers_immediately_when_model_is_unresponsive() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let live = state.live.get(&parent).unwrap();
    live.running.store(false, Ordering::SeqCst);
    let runtime = state.runtime.clone();
    runtime
        .inner
        .selections
        .lock()
        .unwrap()
        .insert(parent.clone(), ctx.selection.clone().unwrap());
    let id = "idle-delivery".to_string();
    runtime.inner.delivery.push(
        Completion {
            child: parent.clone(),
            target: parent.clone(),
            id: id.clone(),
            legacy_id: id.clone(),
            text: "[子代理执行结束] 空闲投递".into(),
            source: "subagent-settled".into(),
        },
        false,
    );
    runtime.retry_deliveries().await;
    for _ in 0..200 {
        if completion_is_delivered(&live, &id) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        completion_is_delivered(&live, &id),
        "未响应的模型必须立刻收到完成结果"
    );
}

/// 后台任务走同一条队列：任务结束时模型正在输出/执行工具，结果必须
/// 等动作收尾；旧路径绕过队列直接 enqueue 会在动作中途插队。
#[tokio::test]
async fn job_completion_respects_the_model_action_boundary() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let live = state.live.get(&parent).unwrap();
    let runtime = state.runtime.clone();
    runtime.observe_activity(
        &parent,
        &SessionEvent::AssistantChunk {
            turn: 1,
            step: 1,
            chunk: StreamChunk::TextDelta {
                index: 0,
                text: "执行中".into(),
            },
        },
    );
    let started = runtime
        .execute("job_start", json!({"command":"echo job-boundary"}), &ctx)
        .await
        .unwrap();
    let job_id = started["id"].as_str().unwrap().to_string();
    for _ in 0..500 {
        let jobs = runtime.execute("job_list", json!({}), &ctx).await.unwrap();
        if jobs.as_array().is_some_and(|rows| {
            rows.iter()
                .any(|row| row["id"] == job_id && !row["finishedAt"].is_null())
        }) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let notice_id = format!("job:{job_id}");
    assert!(
        !completion_is_in_inbox(&live, &notice_id),
        "模型动作进行中不得投递后台任务结果"
    );

    runtime.observe_activity(&parent, &SessionEvent::StepEnd { turn: 1, step: 1 });
    for _ in 0..200 {
        if completion_is_in_inbox(&live, &notice_id) {
            break;
        }
        runtime.retry_deliveries().await;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(
        completion_is_in_inbox(&live, &notice_id),
        "动作结束后后台任务结果必须投递"
    );
}

/// 完成通知不受普通消息"连续唤醒上限"的限制:额度耗尽也必须把结果
/// 送给主代理。
///
/// 锁死现象:同一用户轮次后分批完成超过 `maxConsecutiveWakes` 次,
/// 第 N+1 条完成消息只入队不唤醒,必须用户再发一条消息才收到。
#[tokio::test]
async fn completion_wake_bypasses_message_loop_guard() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let live = state.live.get(&parent).unwrap();
    live.running.store(false, Ordering::SeqCst);
    let runtime = state.runtime.clone();
    let selection = ModelSelection {
        provider: "runtime-test".into(),
        model: "done".into(),
        reasoning_effort: None,
    };
    runtime
        .inner
        .selections
        .lock()
        .unwrap()
        .insert(parent.clone(), selection.clone());
    // 普通消息的唤醒额度已经耗尽。
    runtime
        .inner
        .wakes
        .lock()
        .unwrap()
        .insert(parent.clone(), runtime.config().max_consecutive_wakes);

    let child = state
        .sessions
        .create_subagent(
            &ctx.cwd,
            true,
            &parent,
            SubagentDescriptor::legacy("回调子代理", 1, "default", selection, None, None),
        )
        .unwrap();
    let child_id = child.id().to_string();
    let notice_id = format!("settled:{child_id}:turn:1:seq:9");
    runtime.inner.delivery.push(
        Completion {
            child: child_id.clone(),
            target: parent.clone(),
            id: notice_id.clone(),
            legacy_id: format!("settled:{child_id}:2"),
            text: "[子代理执行结束] 分批完成".into(),
            source: "subagent-settled".into(),
        },
        false,
    );
    runtime.retry_deliveries().await;

    let mut consumed = false;
    for _ in 0..300 {
        let live = state.live.get(&parent).unwrap();
        let events = live.session.events();
        let delivered = events.iter().any(|event| {
                matches!(&event.event, SessionEvent::AgentDelivery { id, .. } if id == &notice_id)
            });
        let turns = events
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::TurnStart { .. }))
            .count();
        if delivered && turns == 1 && !live.running.load(Ordering::SeqCst) {
            consumed = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(
        consumed,
        "唤醒额度耗尽后,子代理完成通知仍必须自动唤醒主代理并被消费"
    );
}

#[tokio::test]
async fn completion_batches_resume_without_another_user_prompt() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.as_deref().unwrap();
    let live = state.live.get(parent).unwrap();
    live.running.store(false, Ordering::SeqCst);
    state
        .runtime
        .inner
        .selections
        .lock()
        .unwrap()
        .insert(parent.into(), ctx.selection.clone().unwrap());
    state
        .runtime
        .inner
        .wakes
        .lock()
        .unwrap()
        .insert(parent.into(), state.runtime.config().max_consecutive_wakes);
    // 每轮空闲都与前一唤醒任务的退出交错，通知次数超过普通唤醒额度。
    for batch in 0..8 {
        let id = format!("completion-batch-{batch}");
        state
            .runtime
            .enqueue(
                parent,
                id.clone(),
                "子代理已完成".into(),
                "subagent-settled".into(),
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let delivered = live.session.events().iter().any(|event| {
                        matches!(&event.event, SessionEvent::AgentDelivery { id: delivered, .. } if delivered == &id)
                    });
                    if delivered && !live.running.load(Ordering::SeqCst) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }).await.expect("每批完成通知都必须自行启动并结束主代理轮次");
    }
    let events = live.session.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::TurnStart { .. }))
            .count(),
        8
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::AgentDelivery { .. }))
            .count(),
        8
    );
    assert!(!events.iter().any(|event| matches!(
        event.event,
        SessionEvent::UserMessage {
            injected: false,
            ..
        }
    )));
}

/// 普通代理消息的防循环护栏仍然有效:额度耗尽后不再自动开轮。
#[tokio::test]
async fn ordinary_message_wake_still_respects_message_loop_guard() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let live = state.live.get(&parent).unwrap();
    live.running.store(false, Ordering::SeqCst);
    let runtime = state.runtime.clone();
    runtime.inner.selections.lock().unwrap().insert(
        parent.clone(),
        ModelSelection {
            provider: "runtime-test".into(),
            model: "done".into(),
            reasoning_effort: None,
        },
    );
    runtime
        .inner
        .wakes
        .lock()
        .unwrap()
        .insert(parent.clone(), runtime.config().max_consecutive_wakes);
    runtime
        .enqueue(
            &parent,
            "m1".into(),
            "普通消息".into(),
            "session-note".into(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !live.running.load(Ordering::SeqCst),
        "额度耗尽时普通消息不得再开轮"
    );
    assert!(
        live.session
            .events()
            .iter()
            .all(|event| !matches!(event.event, SessionEvent::TurnStart { .. })),
        "普通消息的防循环护栏不得被完成通知的旁路改掉"
    );
}

/// 完成通知的瞬时投递失败必须自动补投,且不得重复入队。
#[tokio::test]
async fn completion_notice_retries_after_transient_enqueue_failure() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let runtime = state.runtime.clone();
    let child = state
        .sessions
        .create_subagent(
            &ctx.cwd,
            true,
            &parent,
            SubagentDescriptor::legacy(
                "重试子代理",
                1,
                "default",
                ctx.selection.clone().unwrap(),
                None,
                None,
            ),
        )
        .unwrap();
    let child_id = child.id().to_string();
    let notice_id = format!("settled:{child_id}:turn:1:seq:5");
    runtime.set_test_fault("completion-enqueue");
    runtime.inner.delivery.push(
        Completion {
            child: child_id.clone(),
            target: parent.clone(),
            id: notice_id.clone(),
            legacy_id: format!("settled:{child_id}:2"),
            text: format!("[子代理执行结束] {child_id}"),
            source: "subagent-settled".into(),
        },
        false,
    );
    runtime.retry_deliveries().await;
    assert_eq!(
        settled_notice_count(&state, &parent, &child_id),
        0,
        "首次投递失败时不得留下半截状态"
    );

    runtime
        .inner
        .faults
        .lock()
        .unwrap()
        .remove("completion-enqueue");
    let mut delivered = false;
    for _ in 0..300 {
        runtime.retry_deliveries().await;
        if settled_notice_count(&state, &parent, &child_id) == 1 {
            delivered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    assert!(delivered, "瞬时失败后必须自动补投");
    // 幂等:已入队的通知再次补投不得重复。
    runtime.retry_deliveries().await;
    assert_eq!(settled_notice_count(&state, &parent, &child_id), 1);
}

/// 真实冷启动恢复:子代理终态已落盘、完成通知还没入队(崩溃窗口),
/// 新的 Runtime 必须自己发现并补投,不能等用户发消息。
#[tokio::test]
async fn restart_recovery_delivers_unnotified_completion() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let selection = ctx.selection.clone().unwrap();
    let child = state
        .sessions
        .create_subagent(
            &ctx.cwd,
            true,
            &parent,
            SubagentDescriptor::legacy("崩溃子代理", 1, "default", selection.clone(), None, None),
        )
        .unwrap();
    let child_id = child.id().to_string();
    child
        .append(SessionEvent::AgentInbox {
            id: "m1".into(),
            text: "[父代理 委派任务]\n调查".into(),
            source: format!("agent:{parent}"),
        })
        .unwrap();
    child
        .append(SessionEvent::AgentDelivery {
            id: "m1".into(),
            text: "[父代理 委派任务]\n调查".into(),
            source: format!("agent:{parent}"),
        })
        .unwrap();
    child.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    child
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![ContentBlock::Text {
                text: "已查完".into(),
            }],
            usage: None,
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        })
        .unwrap();
    child
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    child.flush().unwrap();

    // 新的运行时(等价于进程重启):目录从 header 重建,收件箱为空。
    let restored = Runtime::new(
        &state.home,
        state.sessions.clone(),
        state.live.clone(),
        state.settings.clone(),
        state.events.clone(),
        state.registry.clone(),
        state.workspaces.clone(),
        state.subagent_profiles.clone(),
    )
    .unwrap();
    restored
        .inner
        .selections
        .lock()
        .unwrap()
        .insert(parent.clone(), selection);
    state
        .live
        .get(&parent)
        .unwrap()
        .running
        .store(false, Ordering::SeqCst);
    restored.attach(&state.driver);

    let notice = wait_for_settled_notice(&state, &parent, &child_id, "子代理执行结束").await;
    assert!(notice.contains("结果：已完成"), "{notice}");
    // 幂等:重跑恢复不得重复通知同一次终态。
    restored.recover_deliveries().await.unwrap();
    assert_eq!(settled_notice_count(&state, &parent, &child_id), 1);
}

/// 崩溃窗口的另一半:收件箱已落盘、唤醒还没发生,重启后必须自动唤醒
/// 父代理并把消息投进模型历史。
#[tokio::test]
async fn restart_recovery_wakes_parent_for_pending_inbox() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let selection = ctx.selection.clone().unwrap();
    let notice_id = "settled:orphan:turn:2:seq:7";
    let live = state.live.get(&parent).unwrap();
    live.session
        .append(SessionEvent::AgentInbox {
            id: notice_id.into(),
            text: "[子代理执行结束] 补投路径".into(),
            source: "subagent-settled".into(),
        })
        .unwrap();
    live.session.flush().unwrap();
    live.running.store(false, Ordering::SeqCst);

    let restored = Runtime::new(
        &state.home,
        state.sessions.clone(),
        state.live.clone(),
        state.settings.clone(),
        state.events.clone(),
        state.registry.clone(),
        state.workspaces.clone(),
        state.subagent_profiles.clone(),
    )
    .unwrap();
    restored
        .inner
        .selections
        .lock()
        .unwrap()
        .insert(parent.clone(), selection);
    restored.attach(&state.driver);

    let mut delivered = false;
    for _ in 0..300 {
        if state.live.get(&parent).unwrap().session.events().iter().any(
                |event| matches!(&event.event, SessionEvent::AgentDelivery { id, .. } if id == notice_id),
            ) {
                delivered = true;
                break;
            }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(delivered, "重启后必须自动唤醒父代理并领取待收消息");
}

/// 启动快照不可用 → 拒绝启动 → 失败终态必须通知父代理,且不得因为
/// 派遣消息还挂在收件箱而被当成"仍在运行"。
#[tokio::test]
async fn snapshot_failure_settles_child_and_notifies_parent_once() {
    let (state, ctx) = setup().await;
    let parent = ctx.session_id.clone().unwrap();
    let runtime = state.runtime.clone();
    let child = runtime
        .execute("spawn_agent", json!({"prompt":"占用","model":"hold"}), &ctx)
        .await
        .unwrap()["childId"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_running(&state, &child).await;
    // 先让子代理正常收场,再破坏它的运行快照。
    runtime.interrupt(&parent, &child).await.unwrap();
    settle(&runtime, &child).await;
    // 投递是异步的:先把上一轮的通知等进收件箱,再取基线。
    wait_for_settled_count(&state, &parent, &child, 1).await;
    let before = settled_notice_count(&state, &parent, &child);
    std::fs::write(
        state.sessions.root().join(&child).join("subagent.json"),
        "{\"broken\":true}",
    )
    .unwrap();

    runtime
        .execute(
            "send_message",
            json!({"target":child,"message":"继续"}),
            &ctx,
        )
        .await
        .unwrap();
    let notice = wait_for_settled_notice(&state, &parent, &child, "拒绝启动").await;
    assert!(notice.contains("子代理执行结束"), "{notice}");
    assert!(
        notice.contains("失败") || notice.contains("错误"),
        "必须有明确失败终态:{notice}"
    );
    // 明确结算:不再停在"待处理",查询立即给一致快照。
    let ready = runtime
        .execute("wait_agent", json!({"target":child}), &ctx)
        .await
        .unwrap();
    assert_eq!(ready["status"], "ready", "{ready}");
    assert_eq!(ready["running"], false);
    assert_eq!(
        settled_notice_count(&state, &parent, &child),
        before + 1,
        "同一轮失败只允许一条完成通知"
    );
}
