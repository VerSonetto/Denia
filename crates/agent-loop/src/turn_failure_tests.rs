use super::*;
use denia_tools::capabilities::{AgentRuntime, SubagentPrompt};

struct StartupRuntime {
    session: Arc<Session>,
    trace: Mutex<Vec<&'static str>>,
    pending: Mutex<VecDeque<(String, String)>>,
    snapshot_error: Option<String>,
    drain_error: Option<String>,
    enqueue_during_validation: bool,
}

impl StartupRuntime {
    fn new(session: Arc<Session>, snapshot_error: Option<&str>) -> Self {
        let runtime = Self {
            session,
            trace: Mutex::new(Vec::new()),
            pending: Mutex::new(VecDeque::new()),
            snapshot_error: snapshot_error.map(str::to_string),
            drain_error: None,
            enqueue_during_validation: false,
        };
        runtime.enqueue("dispatch", "delegated input");
        runtime
    }

    fn enqueue(&self, id: &str, text: &str) {
        self.session
            .append(SessionEvent::AgentInbox {
                id: id.to_string(),
                text: text.to_string(),
                source: "parent".to_string(),
            })
            .unwrap();
        self.pending
            .lock()
            .unwrap()
            .push_back((id.to_string(), text.to_string()));
    }
}

#[async_trait]
impl AgentRuntime for StartupRuntime {
    async fn execute_command(
        &self,
        _command: denia_tools::runtime_command::RuntimeCommand,
        _ctx: &denia_tools::ToolContext,
    ) -> Result<serde_json::Value, String> {
        Err("fixture runtime has no commands".to_string())
    }

    async fn context(&self, session: &str, _cwd: &std::path::Path) -> Result<Vec<String>, String> {
        assert_eq!(session, self.session.id());
        self.trace.lock().unwrap().push("context");
        Ok(Vec::new())
    }

    async fn drain(&self, session: &str) -> Result<Vec<String>, String> {
        assert_eq!(session, self.session.id());
        self.trace.lock().unwrap().push("drain");
        if let Some(error) = &self.drain_error {
            return Err(error.clone());
        }
        let pending = std::mem::take(&mut *self.pending.lock().unwrap());
        for (id, text) in pending {
            self.session
                .append(SessionEvent::AgentDelivery {
                    id,
                    text,
                    source: "parent".to_string(),
                })
                .map_err(|error| error.to_string())?;
        }
        self.session.flush().map_err(|error| error.to_string())?;
        Ok(Vec::new())
    }

    async fn subagent_prompt(&self, session: &str) -> Result<Option<SubagentPrompt>, String> {
        assert_eq!(session, self.session.id());
        self.trace.lock().unwrap().push("snapshot");
        if self.enqueue_during_validation {
            self.enqueue("during-validation", "later input");
        }
        if let Some(error) = &self.snapshot_error {
            return Err(error.clone());
        }
        Ok(Some(SubagentPrompt {
            name: Some("fixture".to_string()),
            instructions: "fixture child".to_string(),
            effective_tools: Vec::new(),
            permission_ceiling: Some("inherit".to_string()),
            parent_preset_persona: None,
            legacy: false,
        }))
    }
}

fn child_session() -> Arc<Session> {
    let root = std::env::temp_dir().join(format!("denia-child-startup-{}", uuid::Uuid::new_v4()));
    let store = denia_session::SessionStore::open(&root).unwrap();
    let mut descriptor = denia_core::session::SubagentDescriptor::legacy(
        "fixture",
        1,
        "inline",
        selection(),
        None,
        Some(Vec::new()),
    );
    descriptor.snapshot_version = 1;
    descriptor.effective_tools = Some(Vec::new());
    descriptor.instructions_ref = Some("instructions-v1.txt".to_string());
    descriptor.instructions_hash = Some("expected-hash".to_string());
    Arc::new(
        store
            .create_subagent(&root, true, "parent", descriptor)
            .unwrap(),
    )
}

fn lifecycle(events: &[SessionEnvelope], turn: u32) -> Vec<SessionEvent> {
    events
        .iter()
        .filter_map(|envelope| {
            let event_turn = match &envelope.event {
                SessionEvent::TurnStart { turn }
                | SessionEvent::TurnEnd { turn, .. }
                | SessionEvent::StepStart { turn, .. }
                | SessionEvent::StepEnd { turn, .. } => *turn,
                _ => return None,
            };
            (event_turn == turn).then(|| envelope.event.clone())
        })
        .collect()
}

fn disk_events(session: &Session) -> Vec<SessionEnvelope> {
    // 不经 Session::load，避免加载期合成终态掩盖实际未落盘的收尾。
    std::fs::read_to_string(session.file())
        .unwrap()
        .lines()
        .skip(1)
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
async fn snapshot_startup_rejection_drains_once_before_validation_and_logs_error() {
    let session = child_session();
    let runtime = Arc::new(StartupRuntime::new(
        session.clone(),
        Some("instructions hash mismatch"),
    ));
    let (driver, requests) =
        message_recording_driver(vec![MockScript::Chunks(text_script("unused"))]);
    let driver = driver.with_runtime(runtime.clone());

    let reason = run_one_turn(&driver, &session, "delegated turn").await;
    let TurnEndReason::Error { failure } = &reason else {
        panic!("快照校验失败必须拒绝启动:{reason:?}");
    };
    assert_eq!(failure.code, codes::UNKNOWN);
    assert!(failure.message.contains("子代理运行快照不可用"));
    assert!(failure.message.contains("instructions hash mismatch"));
    assert_eq!(*runtime.trace.lock().unwrap(), vec!["drain", "snapshot"]);
    assert!(runtime.pending.lock().unwrap().is_empty());
    assert!(
        requests.lock().unwrap().is_empty(),
        "拒绝启动后不得请求模型"
    );

    let events = disk_events(&session);
    assert_eq!(
        lifecycle(&events, 1),
        vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::TurnEnd { turn: 1, reason },
        ]
    );
    let turn_start = events
        .iter()
        .position(|event| matches!(event.event, SessionEvent::TurnStart { turn: 1 }))
        .unwrap();
    let delivery = events
        .iter()
        .position(|event| matches!(&event.event, SessionEvent::AgentDelivery { id, .. } if id == "dispatch"))
        .expect("本轮派遣消息必须已结算");
    let turn_end = events
        .iter()
        .position(|event| matches!(event.event, SessionEvent::TurnEnd { turn: 1, .. }))
        .unwrap();
    assert!(turn_start < delivery && delivery < turn_end);
}

#[tokio::test]
async fn snapshot_rejection_leaves_messages_arriving_during_validation_pending() {
    let session = child_session();
    let mut runtime = StartupRuntime::new(session.clone(), Some("snapshot missing"));
    runtime.enqueue_during_validation = true;
    let runtime = Arc::new(runtime);
    let (driver, requests) = message_recording_driver(Vec::new());
    let driver = driver.with_runtime(runtime.clone());

    let reason = run_one_turn(&driver, &session, "delegated turn").await;
    assert!(matches!(reason, TurnEndReason::Error { .. }));
    assert_eq!(*runtime.trace.lock().unwrap(), vec!["drain", "snapshot"]);
    assert_eq!(
        *runtime.pending.lock().unwrap(),
        VecDeque::from([("during-validation".to_string(), "later input".to_string())])
    );
    assert!(requests.lock().unwrap().is_empty());
    let events = disk_events(&session);
    let delivered: Vec<&str> = events
        .iter()
        .filter_map(|event| match &event.event {
            SessionEvent::AgentDelivery { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(delivered, vec!["dispatch"]);
    assert_eq!(
        lifecycle(&events, 1),
        vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::TurnEnd { turn: 1, reason },
        ]
    );
}

#[tokio::test]
async fn valid_child_keeps_background_drain_for_later_messages() {
    let session = child_session();
    let mut runtime = StartupRuntime::new(session.clone(), None);
    runtime.enqueue_during_validation = true;
    let runtime = Arc::new(runtime);
    let (driver, requests) =
        message_recording_driver(vec![MockScript::Chunks(text_script("done"))]);
    let driver = driver.with_runtime(runtime.clone());

    assert_eq!(
        run_one_turn(&driver, &session, "delegated turn").await,
        TurnEndReason::Completed
    );
    assert_eq!(
        *runtime.trace.lock().unwrap(),
        vec!["drain", "snapshot", "context", "drain"]
    );
    assert!(runtime.pending.lock().unwrap().is_empty());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    for text in ["delegated input", "later input"] {
        assert!(
            requests[0]
                .iter()
                .any(|message| message.content.contains(text))
        );
    }
    let events = disk_events(&session);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event.event, SessionEvent::AgentDelivery { .. }))
            .count(),
        2
    );
    assert_eq!(
        lifecycle(&events, 1),
        vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::StepStart { turn: 1, step: 1 },
            SessionEvent::StepEnd { turn: 1, step: 1 },
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
        ]
    );
}

#[tokio::test]
async fn startup_drain_failure_closes_turn_without_validating_or_opening_a_step() {
    let session = child_session();
    let mut runtime = StartupRuntime::new(session.clone(), None);
    runtime.drain_error = Some("delivery append failed".to_string());
    let runtime = Arc::new(runtime);
    let (driver, requests) = message_recording_driver(Vec::new());
    let driver = driver.with_runtime(runtime.clone());

    let reason = run_one_turn(&driver, &session, "delegated turn").await;
    assert_eq!(
        reason,
        TurnEndReason::Error {
            failure: LlmFailure::new(codes::UNKNOWN, "delivery append failed"),
        }
    );
    assert_eq!(*runtime.trace.lock().unwrap(), vec!["drain"]);
    assert_eq!(runtime.pending.lock().unwrap().len(), 1);
    assert!(requests.lock().unwrap().is_empty());
    assert_eq!(
        lifecycle(&disk_events(&session), 1),
        vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::TurnEnd { turn: 1, reason },
        ]
    );
}

#[tokio::test]
async fn dispatch_configuration_error_closes_open_step_and_turn() {
    let (driver, requests) = message_recording_driver(Vec::new());
    let session = temp_session();
    let mut missing_route = selection();
    missing_route.provider = "unregistered".to_string();
    let emitted = Arc::new(Mutex::new(Vec::<SessionEnvelope>::new()));
    let emit = {
        let emitted = emitted.clone();
        Arc::new(move |event: &SessionEnvelope| emitted.lock().unwrap().push(event.clone()))
    };

    let reason = driver
        .run_turn(
            &session,
            &missing_route,
            "start execution",
            Vec::new(),
            Vec::new(),
            Vec::new(),
            false,
            CancellationToken::new(),
            emit,
        )
        .await;
    let TurnEndReason::Error { failure } = &reason else {
        panic!("缺少路由必须返回原始执行错误:{reason:?}");
    };
    assert_eq!(failure.code, codes::NO_ADAPTER);
    assert_eq!(failure.message, "no adapter registered");
    assert!(requests.lock().unwrap().is_empty());
    let expected = vec![
        SessionEvent::TurnStart { turn: 1 },
        SessionEvent::StepStart { turn: 1, step: 1 },
        SessionEvent::StepEnd { turn: 1, step: 1 },
        SessionEvent::TurnEnd { turn: 1, reason },
    ];
    assert_eq!(lifecycle(&session.events(), 1), expected);
    assert_eq!(lifecycle(&disk_events(&session), 1), expected);
    assert_eq!(lifecycle(&emitted.lock().unwrap(), 1), expected);
}

fn failure_state(driver: &SessionDriver, session: &Arc<Session>, turn: u32) -> TurnState {
    let mut state = TurnState::new(
        driver,
        session.clone(),
        noop_emit(),
        selection(),
        false,
        CancellationToken::new(),
        loop_guard::LoopGuard::default(),
        driver.read_state_for(session.id()),
    );
    state.turn = turn;
    state
}

#[test]
fn error_cleanup_closes_only_current_open_steps_and_is_idempotent() {
    let (driver, _) = driver(Vec::new());
    let session = temp_session();
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    session
        .append(SessionEvent::StepStart { turn: 1, step: 7 })
        .unwrap();
    let started_after = session.with_events(crate::injections::last_seq);
    session.append(SessionEvent::TurnStart { turn: 2 }).unwrap();
    session
        .append(SessionEvent::StepStart { turn: 2, step: 1 })
        .unwrap();
    session
        .append(SessionEvent::StepEnd { turn: 2, step: 1 })
        .unwrap();
    session
        .append(SessionEvent::StepStart { turn: 2, step: 2 })
        .unwrap();
    session
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    let state = failure_state(&driver, &session, 2);
    let failure = LlmFailure::new(codes::UNKNOWN, "execution failed");
    let reason = close_failed_turn(&state, started_after, failure.clone()).unwrap();
    assert_eq!(reason, TurnEndReason::Error { failure });
    assert_eq!(
        lifecycle(&disk_events(&session), 2),
        vec![
            SessionEvent::TurnStart { turn: 2 },
            SessionEvent::StepStart { turn: 2, step: 1 },
            SessionEvent::StepEnd { turn: 2, step: 1 },
            SessionEvent::StepStart { turn: 2, step: 2 },
            SessionEvent::StepEnd { turn: 2, step: 2 },
            SessionEvent::TurnEnd { turn: 2, reason },
        ]
    );
    assert_eq!(
        lifecycle(&disk_events(&session), 1),
        vec![
            SessionEvent::TurnStart { turn: 1 },
            SessionEvent::StepStart { turn: 1, step: 7 },
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
        ]
    );
    let settled = disk_events(&session);
    let late_failure = LlmFailure::new(codes::STORAGE, "later append failed");
    assert_eq!(
        close_failed_turn(&state, started_after, late_failure.clone()).unwrap(),
        TurnEndReason::Error {
            failure: late_failure,
        }
    );
    assert_eq!(disk_events(&session), settled, "已有终态不得重复写入");
}

#[test]
fn error_cleanup_without_a_new_turn_start_does_not_close_historical_turn() {
    let (driver, _) = driver(Vec::new());
    let session = temp_session();
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    session
        .append(SessionEvent::StepStart { turn: 1, step: 1 })
        .unwrap();
    let started_after = session.with_events(crate::injections::last_seq);
    session
        .append(SessionEvent::UserMessage {
            text: "input whose turn-start failed".to_string(),
            injected: false,
            channel: None,
            images: Vec::new(),
        })
        .unwrap();
    session.flush().unwrap();
    let before = disk_events(&session);
    let state = failure_state(&driver, &session, 1);
    let failure = LlmFailure::new(codes::STORAGE, "turn-start append failed");
    assert_eq!(
        close_failed_turn(&state, started_after, failure.clone()).unwrap(),
        TurnEndReason::Error { failure }
    );
    assert_eq!(disk_events(&session), before);
}
