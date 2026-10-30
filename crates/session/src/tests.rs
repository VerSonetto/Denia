use super::*;
use denia_core::task::{
    CheckRun, FileFingerprint, StaleReason, TaskOp, TaskOutcome, ValidationResult,
    VerificationState,
};

#[test]
fn failed_append_does_not_change_projections_or_retry_a_failed_buffer() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let session = store.create(&root, true).unwrap();
    session.flush().unwrap();
    let previous_mode = session.permission_mode();
    let previous_seq = session.inner.lock().unwrap().last_seq;
    let previous_bytes = std::fs::metadata(session.file()).unwrap().len();
    session.inner.lock().unwrap().writer = LogWriter::new(File::open(session.file()).unwrap());
    assert!(session.set_permission_mode(PermissionMode::Full).is_err());
    assert_eq!(session.permission_mode(), previous_mode);
    assert_eq!(session.inner.lock().unwrap().last_seq, previous_seq);
    assert!(session.events().is_empty());
    assert!(session.set_title("must not be written".into()).is_err());
    assert_eq!(session.title(), None);
    drop(session);
    let file = store
        .root()
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
        .join("session.jsonl");
    assert_eq!(std::fs::metadata(file).unwrap().len(), previous_bytes);
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

fn temp_root() -> PathBuf {
    // pid + 进程内原子序号:同一次测试里连续调用也保证互不撞名
    // (时钟精度不足时按时间戳命名会给出同一个目录)。
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "denia-session-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn agent_preset_folds_across_reload_and_rewind() {
    // preset 是会话事实:latest-wins,并且必须跨越"落盘 → 重新加载"
    // 存活——否则恢复的会话会悄悄换回默认预设。
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    assert_eq!(session.agent_preset(), None, "新会话未指定预设");
    session.set_agent_preset("minimal").unwrap();
    assert_eq!(session.agent_preset().as_deref(), Some("minimal"));
    session.set_agent_preset("explore").unwrap();
    drop(session);

    let loaded = store.load(&id).unwrap();
    assert_eq!(
        loaded.agent_preset().as_deref(),
        Some("explore"),
        "latest-wins 的预设必须从日志恢复"
    );
    let folded = loaded.derive_messages();
    assert!(folded.is_empty(), "预设事件不进模型历史");
    std::fs::remove_dir_all(&root).unwrap();
}

/// 会话列表摘要必须带上预设:新会话页要在不加载会话的前提下显示它
/// (活跃会话走内存折叠,冷启动走磁盘头部扫描,两条路径都要对)。
#[test]
fn session_list_carries_agent_preset() {
    let root = temp_root();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let store = SessionStore::open(&root).unwrap();
    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    session.set_agent_preset("minimal").unwrap();
    assert_eq!(
        store.list().unwrap()[0].agent_preset.as_deref(),
        Some("minimal"),
        "活跃会话的摘要从内存折叠读"
    );
    drop(session);

    // 冷启动:进程重开后建索引,摘要只能从日志头扫描得来。
    let reopened = SessionStore::open(&root).unwrap();
    let summaries = reopened.list().unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, id);
    assert_eq!(
        summaries[0].agent_preset.as_deref(),
        Some("minimal"),
        "冷启动后仍能从会话日志读出预设"
    );

    // 没记过预设的老会话:字段缺省,由前端落到部署默认值。
    let plain = reopened.create(&cwd, true).unwrap();
    let plain_id = plain.id().to_string();
    drop(plain);
    let fresh = SessionStore::open(&root).unwrap();
    let plain_summary = fresh
        .list()
        .unwrap()
        .into_iter()
        .find(|summary| summary.id == plain_id)
        .expect("plain session is listed");
    assert_eq!(plain_summary.agent_preset, None);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn create_append_load_round_trip() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    session
        .append(SessionEvent::UserMessage {
            text: "hello world, this is a prompt".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    session
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    drop(session);

    let loaded = store.load(&id).unwrap();
    assert_eq!(loaded.events().len(), 3);
    assert_eq!(loaded.events()[0].seq, 1);
    assert_eq!(loaded.events()[2].seq, 3);
    assert_eq!(loaded.header().cwd, cwd.to_string_lossy().to_string());

    // 摘要索引:list 不需要触碰文件内容。
    let summaries = store.list().unwrap();
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].id, id);
    assert!(
        summaries[0]
            .excerpt
            .as_deref()
            .unwrap()
            .starts_with("hello world")
    );

    store.delete(&id).unwrap();
    assert!(matches!(store.load(&id), Err(SessionError::NotFound(_))));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn read_page_streams_tail_and_before_without_full_load() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    for i in 1..=12u64 {
        session
            .append(SessionEvent::UserMessage {
                text: format!("message {i}"),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
    }
    drop(session);

    let tail = store.read_page(&id, None, 5).unwrap();
    assert_eq!(tail.total, 12);
    assert_eq!(tail.events.len(), 5);
    assert_eq!(tail.events.last().unwrap().seq, 12);
    assert!(tail.has_more_before);

    let before = store.read_page(&id, Some(7), 3).unwrap();
    assert_eq!(before.events.len(), 3);
    assert_eq!(before.events.first().unwrap().seq, 4);
    assert_eq!(before.events.last().unwrap().seq, 6);
    assert!(before.has_more_before);

    let head = store.read_page(&id, Some(4), 5).unwrap();
    assert_eq!(head.events.len(), 3);
    assert!(!head.has_more_before);

    assert!(matches!(
        store.read_page("00000000-0000-4000-8000-000000000000", None, 5),
        Err(SessionError::NotFound(_))
    ));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn read_page_display_granularity_ignores_chunks_and_aligns_turn_group() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    let chunk = |text: &str| SessionEvent::AssistantChunk {
        turn: 1,
        step: 1,
        chunk: denia_core::stream::StreamChunk::TextDelta {
            index: 0,
            text: text.into(),
        },
    };
    // 轮次 1:用户 + 3 条 chunk + 结算消息 + 收尾。
    session
        .append(SessionEvent::UserMessage {
            text: "u1\nsecond line".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    for _ in 0..3 {
        session.append(chunk("x")).unwrap();
    }
    session
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: Vec::new(),
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
    // 轮次 2:用户 + 工具 + 2 条 chunk + 结算消息 + 收尾。
    session
        .append(SessionEvent::UserMessage {
            text: "u2".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    session.append(SessionEvent::TurnStart { turn: 2 }).unwrap();
    session
        .append(SessionEvent::ToolCall {
            turn: 2,
            step: 1,
            call_id: "c1".into(),
            name: "bash".into(),
            arguments: "{}".into(),
        })
        .unwrap();
    session
        .append(SessionEvent::ToolResult {
            turn: 2,
            step: 1,
            call_id: "c1".into(),
            content: "ok".into(),
            is_error: false,
            error: None,
            error_identity: None,
            meta: None,
            replaces: None,
            truncation: None,
        })
        .unwrap();
    session.append(chunk("y")).unwrap();
    session.append(chunk("z")).unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            turn: 2,
            step: 1,
            blocks: Vec::new(),
            usage: None,
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        })
        .unwrap();
    session
        .append(SessionEvent::TurnEnd {
            turn: 2,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    drop(session);

    // 展示粒度:chunk 不计入 total,也不出现在窗口。
    let page = store.read_page(&id, None, 100).unwrap();
    assert_eq!(page.total, 10);
    assert!(
        page.events
            .iter()
            .all(|envelope| !matches!(envelope.event, SessionEvent::AssistantChunk { .. }))
    );
    assert!(!page.has_more_before);
    // 锚点为全会话非注入用户消息,预览压平换行。
    assert_eq!(page.anchors.len(), 2);
    assert_eq!(page.anchors[0].text, "u1 second line");
    assert_eq!(page.anchors[1].text, "u2");

    // 小窗口:最近锚点(u2)距尾部已达 limit,窗口回退到 u2——
    // 轮次 2 完整在窗口内(前端折叠依赖成对的 turn-start/turn-end)。
    let tail = store.read_page(&id, None, 4).unwrap();
    assert_eq!(tail.total, 10);
    assert_eq!(tail.events.len(), 6);
    assert_eq!(tail.events.first().unwrap().seq, page.anchors[1].seq);
    assert!(tail.has_more_before);
    assert!(tail.anchors.len() == 2, "anchors 不受窗口限制");

    // before 查询:eligible 区按展示粒度计数;最近锚点(u1)起的轮次组
    // 已达 limit,窗口回退到 u1 整组返回,锚点仍为全量。
    let before = store.read_page(&id, Some(page.anchors[1].seq), 2).unwrap();
    assert_eq!(before.total, 4);
    assert_eq!(before.events.len(), 4);
    assert_eq!(before.events.first().unwrap().seq, page.anchors[0].seq);
    assert!(!before.has_more_before);
    assert_eq!(before.anchors.len(), 2);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn read_page_aligns_window_to_earlier_anchor_behind_small_tail_turns() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    let user = |text: &str| SessionEvent::UserMessage {
        text: text.into(),
        injected: false,
        images: Vec::new(),
        channel: None,
    };
    // 轮次 1:u1 + 工具×3 对 + 结算 + 收尾(展示位次 0..9);
    // 轮次 2/3:各 4 条展示事件的小轮次。尾部小轮次合计不足 limit 时,
    // 边界会落在轮次 1 的工具段中间——窗口必须回退到 u1,否则切出
    // 无 turn-start 的残轮次,前端折叠失效。
    let turn = |turn: u32, tools: usize, session: &Session| {
        session.append(user(&format!("u{turn}"))).unwrap();
        session.append(SessionEvent::TurnStart { turn }).unwrap();
        for i in 0..tools {
            session
                .append(SessionEvent::ToolCall {
                    turn,
                    step: 1,
                    call_id: format!("c{turn}-{i}"),
                    name: "bash".into(),
                    arguments: "{}".into(),
                })
                .unwrap();
            session
                .append(SessionEvent::ToolResult {
                    turn,
                    step: 1,
                    call_id: format!("c{turn}-{i}"),
                    content: "ok".into(),
                    is_error: false,
                    error: None,
                    error_identity: None,
                    meta: None,
                    replaces: None,
                    truncation: None,
                })
                .unwrap();
        }
        session
            .append(SessionEvent::AssistantMessage {
                turn,
                step: 1,
                blocks: Vec::new(),
                usage: None,
                interrupted: false,
                source_event_seqs: Vec::new(),
                first_token_time: None,
            })
            .unwrap();
        session
            .append(SessionEvent::TurnEnd {
                turn,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
    };
    turn(1, 3, &session);
    turn(2, 0, &session);
    turn(3, 0, &session);
    drop(session);

    // total=18;limit=12 → 边界=6,正是轮次 1 第三个 toolcall(工具段中)。
    let page = store.read_page(&id, None, 12).unwrap();
    assert_eq!(page.total, 18);
    // 窗口回退到 u1:第一个轮次完整,而不是从边界切出残轮次。
    assert_eq!(page.events.len(), 18);
    assert_eq!(page.anchors[0].seq, page.events[0].seq);
    assert!(matches!(
        page.events[0].event,
        SessionEvent::UserMessage { .. }
    ));
    assert!(!page.has_more_before);

    // 尾部小窗口:边界落在轮次 2/3 之间,起点对齐 u2,完整返回尾两个轮次。
    let tail = store.read_page(&id, None, 7).unwrap();
    assert_eq!(tail.total, 18);
    assert_eq!(tail.events.len(), 8);
    assert_eq!(tail.events[0].seq, page.anchors[1].seq);
    assert!(tail.has_more_before);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn permission_mode_folds_and_persists() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let session = store.create(&cwd, true).unwrap();
    assert_eq!(session.permission_mode(), PermissionMode::AutoEdit);

    session
        .set_permission_mode(PermissionMode::ReadOnly)
        .unwrap();
    assert_eq!(session.permission_mode(), PermissionMode::ReadOnly);
    let id = session.id().to_string();
    drop(session);

    let loaded = store.load(&id).unwrap();
    assert_eq!(loaded.permission_mode(), PermissionMode::ReadOnly);

    loaded.set_permission_mode(PermissionMode::Full).unwrap();
    assert_eq!(loaded.permission_mode(), PermissionMode::Full);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn session_title_folds_persists_and_feeds_summary() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let session = store.create(&cwd, true).unwrap();
    assert_eq!(session.title(), None);
    session
        .append(SessionEvent::UserMessage {
            text: "帮我重构登录流程".into(),
            injected: false,
            channel: None,
            images: Vec::new(),
        })
        .unwrap();
    session.set_title("AI 生成标题·唯一串".into()).unwrap();
    assert_eq!(session.title().as_deref(), Some("AI 生成标题·唯一串"));
    let id = session.id().to_string();
    drop(session);

    // 重载折叠 + 摘要(内存索引与文件重读两条路径)都携带标题。
    let loaded = store.load(&id).unwrap();
    assert_eq!(loaded.title().as_deref(), Some("AI 生成标题·唯一串"));
    let list = store.list().unwrap();
    let summary = list.iter().find(|s| s.id == id).unwrap();
    assert_eq!(summary.title.as_deref(), Some("AI 生成标题·唯一串"));
    // 标题不进模型历史:model-visible == logged 的投影不受影响。
    assert!(
        loaded
            .derive_messages()
            .iter()
            .all(|m| !m.content.contains("AI 生成标题·唯一串"))
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn legacy_permission_values_map_to_new_modes() {
    // 旧日志三档值反序列化时落到语义等价的新档位(serde alias)。
    use denia_core::session::PermissionMode as PM;
    let cases = [
        ("read-only", PM::ReadOnly),
        ("workspace-write", PM::AutoEdit),
        ("auto-edit", PM::AutoEdit),
        ("plan", PM::Plan),
        ("danger-full-access", PM::Full),
        ("full", PM::Full),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            serde_json::from_str::<PM>(&format!("\"{raw}\"")).unwrap(),
            expected,
            "legacy value {raw} must map"
        );
    }
    assert!(PM::parse("plan").is_some());
    assert!(PM::parse("nonsense").is_none());
}

#[test]
fn goal_events_fold_last_wins_and_survive_reload() {
    use denia_core::session::{GoalOp, GoalStatus};
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let session = store.create(&root, true).unwrap();
    assert_eq!(session.goal(), None);
    assert_eq!(session.goal_tokens_used(), None);

    session
        .apply_goal(GoalOp::Set {
            objective: "修完回归".into(),
            token_budget: Some(1_000),
        })
        .unwrap();
    let goal = session.goal().expect("goal should exist");
    assert_eq!(goal.status, GoalStatus::Active);
    assert_eq!(goal.token_budget, Some(1_000));
    assert_eq!(goal.base_tokens, 0, "新会话激活基数为零");
    assert_eq!(session.goal_tokens_used(), Some(0));

    session.apply_goal(GoalOp::Round).unwrap();
    session.apply_goal(GoalOp::Pause).unwrap();
    assert_eq!(session.goal().unwrap().status, GoalStatus::Paused);
    assert_eq!(session.goal().unwrap().rounds_started, 1);

    // fork 种子继承:goal 事件随日志前缀复制到新会话。
    let fork = store.create(&root, true).unwrap();
    fork.seed_from(&session.events()).unwrap();
    let inherited = fork.goal().expect("fork should inherit goal");
    assert_eq!(inherited.objective, "修完回归");
    assert_eq!(inherited.rounds_started, 1);

    // 清除与重载持久化。
    session.apply_goal(GoalOp::Clear).unwrap();
    assert_eq!(session.goal(), None);
    let id = session.id().to_string();
    drop(session);
    let loaded = store.load(&id).unwrap();
    assert_eq!(loaded.goal(), None, "清除后的重载保持无目标");
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn index_refreshes_on_file_change() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    // 真实路径:server 以 Arc 持有并登记,list 从内存读摘要。
    let session = std::sync::Arc::new(store.create(&root, true).unwrap());
    store.track_session(&session);
    assert!(store.list().unwrap()[0].excerpt.is_none());

    session
        .append(SessionEvent::UserMessage {
            text: "fresh excerpt".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    let summary = store.list().unwrap();
    assert!(
        summary[0]
            .excerpt
            .as_deref()
            .unwrap()
            .contains("fresh excerpt"),
        "excerpt must refresh: {:?}",
        summary[0].excerpt
    );

    // 未登记会话(值语义):落盘边界后走文件 stat 失效检测路径。
    let session2 = store.create(&root, false).unwrap();
    session2
        .append(SessionEvent::UserMessage {
            text: "file-based excerpt".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    // 强制落盘:TurnEnd 边界会 flush。
    session2
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    drop(session2);
    let summaries = store.list().unwrap();
    assert!(
        summaries.iter().any(|s| s
            .excerpt
            .as_deref()
            .unwrap_or("")
            .contains("file-based excerpt")),
        "file-based excerpt must refresh via stat invalidation"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn torn_tail_is_truncated_and_open_turn_closed() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let session = store.create(&root, true).unwrap();
    let id = session.id().to_string();
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    session
        .append(SessionEvent::UserMessage {
            text: "hi".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    let file = session.file().to_path_buf();
    drop(session);

    // Simulate a crash mid-append: garbage after the committed prefix.
    {
        use std::io::Write as _;
        let mut f = OpenOptions::new().append(true).open(&file).unwrap();
        f.write_all(b"{\"seq\":3,\"time\":1,\"type\":\"user-mess")
            .unwrap();
        f.flush().unwrap();
    }

    let loaded = store.load(&id).unwrap();
    // Two good events + the synthetic turn-end close.
    assert_eq!(loaded.events().len(), 3);
    match &loaded.events()[2].event {
        SessionEvent::TurnEnd { turn, reason } => {
            assert_eq!(*turn, 1);
            assert_eq!(*reason, TurnEndReason::Interrupted);
        }
        other => panic!("expected turn-end, got {other:?}"),
    }
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn ids_are_validated_before_path_use() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    assert!(matches!(
        store.load("../escape"),
        Err(SessionError::InvalidId(_))
    ));
    assert!(matches!(store.delete(""), Err(SessionError::InvalidId(_))));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn derive_messages_delegates() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let session = store.create(&root, true).unwrap();
    session
        .append(SessionEvent::UserMessage {
            text: "ping".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    let messages = session.derive_messages();
    assert_eq!(messages, vec![ChatMessage::user("ping")]);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn turn_counter_and_system_prompt_tracking() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let session = store.create(&root, true).unwrap();
    assert_eq!(session.next_turn_number(), 1);
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    assert_eq!(session.next_turn_number(), 2);
    session
        .append(SessionEvent::SystemPrompt {
            turn: 1,
            step: 1,
            text: "sys-a".into(),
        })
        .unwrap();
    assert_eq!(session.last_system_prompt().as_deref(), Some("sys-a"));
    session
        .append(SessionEvent::SystemPrompt {
            turn: 1,
            step: 2,
            text: "sys-b".into(),
        })
        .unwrap();
    assert_eq!(session.last_system_prompt().as_deref(), Some("sys-b"));

    session
        .append(SessionEvent::UserMessage {
            text: "uno".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    session
        .append(SessionEvent::UserMessage {
            text: "dos".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    let after = session.events_after(1);
    assert_eq!(after.len(), 4, "seq > 1 应为 4 条,实际 {}", after.len());
    assert_eq!(after[0].seq, 2);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn rewind_truncates_log_and_writes_audit() {
    let root = temp_root();
    let dir = root.join("s");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = Session::create(&dir, "s".to_string(), &cwd, true, None).unwrap();
    session
        .append(SessionEvent::UserMessage {
            text: "first".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    session
        .append(SessionEvent::StepStart { turn: 1, step: 1 })
        .unwrap();
    session
        .append(SessionEvent::UserMessage {
            text: "second".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    session.append(SessionEvent::TurnStart { turn: 2 }).unwrap();

    let outcome = session.rewind(4).unwrap();
    assert_eq!(outcome.removed_events, 2);
    assert_eq!(outcome.to_message.as_deref(), Some("second"));
    assert_eq!(session.events().len(), 3);
    assert_eq!(session.events()[0].seq, 1);
    assert_eq!(session.events()[2].seq, 3);
    assert_eq!(session.next_turn_number(), 2);

    let audit = std::fs::read_to_string(dir.join("rewinds.jsonl")).unwrap();
    assert!(audit.contains("\"to_seq\":4"));
    std::fs::remove_dir_all(&root).unwrap();
}

/// 回退必须立刻反映在派生结果里。
///
/// 回退把日志截断后,长度可能与某个更早时刻**恰好相同**(本例:回到 2 条,
/// 而先前也出现过 2 条事件的状态)。缓存若被误判命中,模型就会看到本该
/// 消失的历史。这里锁住"截断后的派生结果只含保留的事件"。
#[test]
fn derive_cache_invalidates_after_rewind() {
    let root = temp_root();
    let dir = root.join("s");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = Session::create(&dir, "s".to_string(), &cwd, true, None).unwrap();
    session
        .append(SessionEvent::UserMessage {
            text: "first".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    session
        .append(SessionEvent::UserMessage {
            text: "second".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    // 在这一长度上派生一次并缓存。
    assert_eq!(session.derive_messages().len(), 2);

    session
        .append(SessionEvent::UserMessage {
            text: "third".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    assert_eq!(session.derive_messages().len(), 3);

    // 回退到第三条之前:长度回到 2,与最早那次派生时的长度相同。
    session.rewind(3).unwrap();
    let messages = session.derive_messages();
    assert_eq!(messages.len(), 2, "回退后不得命中回退前的派生缓存");
    assert_eq!(messages[1], ChatMessage::user("second"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn forked_session_replays_seed_with_lineage() {
    use denia_core::session::fork_cut_index;

    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();

    let source = store.create(&cwd, true).unwrap();
    let source_id = source.id().to_string();
    source.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    source
        .append(SessionEvent::UserMessage {
            text: "first".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    source
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![denia_core::stream::ContentBlock::Text { text: "hi".into() }],
            usage: None,
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        })
        .unwrap();
    source
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    // 未完成轮次:不应进入种子。
    source.append(SessionEvent::TurnStart { turn: 2 }).unwrap();
    source
        .append(SessionEvent::UserMessage {
            text: "running".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    let source_events = source.events();

    let cut = fork_cut_index(&source_events, None).unwrap();
    assert_eq!(cut, 4, "无锚点切到最后一个 turn-end(含)");
    let child = store
        .create_forked(&source_events, cut, &cwd, true, &source_id)
        .unwrap();

    // 种子回放:seq 重排连续、时间戳保留、派生历史与前缀逐字一致。
    let child_events = child.events();
    assert_eq!(child_events.len(), 4);
    for (index, envelope) in child_events.iter().enumerate() {
        assert_eq!(envelope.seq, index as u64 + 1);
        assert_eq!(envelope.time, source_events[index].time, "种子保留源时间戳");
    }
    assert_eq!(
        child.derive_messages(),
        denia_core::session::derive_messages(&source_events[..cut]),
    );
    // 未完成轮次的消息不属于子会话。
    assert!(child.first_prompt_excerpt(80).unwrap().contains("first"));
    let child_id = child.id().to_string();
    drop(child);

    // 血缘:header 与摘要都带 parent_session;重载后仍在。
    let reloaded = store.load(&child_id).unwrap();
    assert_eq!(
        reloaded.header().parent_session.as_deref(),
        Some(source_id.as_str())
    );
    let summary = store
        .list()
        .unwrap()
        .into_iter()
        .find(|s| s.id == child_id)
        .unwrap();
    assert_eq!(summary.parent_session.as_deref(), Some(source_id.as_str()));
    std::fs::remove_dir_all(&root).unwrap();
}

/* ---- 冷/热两层:浏览路径不驻留事件 ---- */

fn write_sample_log(dir: &Path, cwd: &Path) {
    let session = Session::create(dir, "s".to_string(), cwd, true, None).unwrap();
    session.set_agent_preset("minimal").unwrap();
    session.set_permission_mode(PermissionMode::Full).unwrap();
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    session
        .append(SessionEvent::UserMessage {
            text: "你好".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![ContentBlock::Text {
                text: "回复".into(),
            }],
            usage: Some(denia_core::stream::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: None,
                reasoning_tokens: None,
            }),
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
    drop(session);
}

#[test]
fn cold_open_keeps_aggregates_without_events() {
    let root = temp_root();
    let dir = root.join("s");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    write_sample_log(&dir, &cwd);

    let cold = Session::open_cold(&dir.join("session.jsonl")).unwrap();
    assert!(!cold.is_hot(), "冷打开不驻留事件");
    assert!(cold.events().is_empty());
    assert_eq!(
        cold.agent_preset().as_deref(),
        Some("minimal"),
        "聚合照常折叠"
    );
    assert_eq!(cold.permission_mode(), PermissionMode::Full);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn cold_aggregates_match_hot() {
    let root = temp_root();
    let dir = root.join("s");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    write_sample_log(&dir, &cwd);

    let hot = Session::load(&dir.join("session.jsonl")).unwrap();
    let cold = Session::open_cold(&dir.join("session.jsonl")).unwrap();
    assert_eq!(
        format!("{:?}", hot.context_breakdown()),
        format!("{:?}", cold.context_breakdown()),
        "meter 聚合冷热一致"
    );
    assert_eq!(
        format!("{:?}", hot.turn_token_usage()),
        format!("{:?}", cold.turn_token_usage()),
        "轮次 usage 冷热一致"
    );
    assert_eq!(hot.next_turn_number(), cold.next_turn_number());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn cooling_releases_history_and_preserves_summary_and_replay() {
    let root = temp_root();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let store = SessionStore::open(&root).unwrap();
    let session = Arc::new(store.create(&cwd, true).unwrap());
    session
        .append(SessionEvent::UserMessage {
            text: "keep this summary".into(),
            injected: false,
            channel: None,
            images: Vec::new(),
        })
        .unwrap();
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    session
        .append(SessionEvent::StepStart { turn: 1, step: 1 })
        .unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![ContentBlock::Text {
                text: "large answer".repeat(10_000),
            }],
            usage: Some(denia_core::stream::TokenUsage {
                input_tokens: 20,
                output_tokens: 10,
                cache_read_tokens: Some(5),
                reasoning_tokens: Some(3),
            }),
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        })
        .unwrap();
    session
        .append(SessionEvent::StepEnd { turn: 1, step: 1 })
        .unwrap();
    assert!(!session.cool().unwrap());
    session
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    store.track_session(&session);
    let surface = session.derive_surface();
    let weak_surface = Arc::downgrade(&surface);
    drop(surface);
    let meter = session.turn_token_usage();
    let pressure = session.context_pressure();
    let count = session.events().len();
    assert!(session.cool().unwrap());
    assert!(!session.cool().unwrap());
    assert!(weak_surface.upgrade().is_none());
    assert_eq!(session.resident_bytes(), 0);
    assert_eq!(session.turn_token_usage(), meter);
    assert_eq!(session.context_pressure(), pressure);
    assert_eq!(session.events_after(0).len(), count);
    assert_eq!(
        session.first_prompt_excerpt(80).as_deref(),
        Some("keep this summary")
    );
    assert_eq!(
        store.list().unwrap()[0].excerpt.as_deref(),
        Some("keep this summary")
    );
    {
        let inner = session.inner.lock().unwrap();
        assert_eq!(inner.events.capacity(), 0);
        assert_eq!(inner.offsets.capacity(), 0);
        assert_eq!(inner.pending_turn.capacity(), 0);
    }
    session.ensure_hot().unwrap();
    assert_eq!(session.events().len(), count);
    assert_eq!(session.turn_token_usage(), meter);
    assert!(session.resident_bytes() > 0);
    let appended = session
        .append(SessionEvent::UserMessage {
            text: "next".into(),
            injected: false,
            channel: None,
            images: Vec::new(),
        })
        .unwrap();
    assert_eq!(appended.seq, count as u64 + 1);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn usage_buffer_does_not_clone_message_bodies() {
    let root = temp_root();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = Session::create(&root.join("s"), "s".into(), &cwd, true, None).unwrap();
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    session
        .append(SessionEvent::StepStart { turn: 1, step: 1 })
        .unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![ContentBlock::Text {
                text: "x".repeat(1_000_000),
            }],
            usage: Some(denia_core::stream::TokenUsage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_tokens: None,
                reasoning_tokens: None,
            }),
            interrupted: false,
            source_event_seqs: (1..1000).collect(),
            first_token_time: None,
        })
        .unwrap();
    session.with_events(|events| assert_eq!(events.len(), 3));
    {
        let inner = session.inner.lock().unwrap();
        assert_eq!(inner.pending_turn.len(), 3);
        for envelope in &inner.pending_turn {
            if let SessionEvent::AssistantMessage {
                blocks,
                source_event_seqs,
                ..
            } = &envelope.event
            {
                assert!(blocks.is_empty());
                assert!(source_event_seqs.is_empty());
            }
        }
    }
    session
        .append(SessionEvent::StepEnd { turn: 1, step: 1 })
        .unwrap();
    session
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    assert_eq!(session.turn_token_usage().output_tokens, 5);
    assert_eq!(session.inner.lock().unwrap().pending_turn.capacity(), 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn settlement_only_scans_transient_suffix_and_preserves_history() {
    let root = temp_root();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = Session::create(&root.join("s"), "s".into(), &cwd, true, None).unwrap();
    for step in 0..4096 {
        session
            .append(SessionEvent::StepStart { turn: 1, step })
            .unwrap();
    }
    let prefix = session.events();
    {
        let mut inner = session.inner.lock().unwrap();
        inner.events.reserve(128);
    }
    let capacity = session.inner.lock().unwrap().events.capacity();
    for turn in 1..=3 {
        session.append(SessionEvent::TurnStart { turn }).unwrap();
        if turn != 1 {
            let start = session.events().len();
            for step in 1..=3 {
                session
                    .append(SessionEvent::AssistantChunk {
                        turn,
                        step,
                        chunk: denia_core::stream::StreamChunk::TextDelta {
                            index: 0,
                            text: "delta".into(),
                        },
                    })
                    .unwrap();
                // 非 transient 事件可与 chunk 交错,清扫不得误删。
                session
                    .append(SessionEvent::StepEnd { turn, step })
                    .unwrap();
            }
            assert_eq!(session.inner.lock().unwrap().transient_start, Some(start));
        }
        session
            .append(SessionEvent::TurnEnd {
                turn,
                reason: TurnEndReason::Completed,
            })
            .unwrap();
        let inner = session.inner.lock().unwrap();
        assert_eq!(inner.transient_start, None);
        assert_eq!(inner.transient_bytes, 0);
        assert_eq!(inner.events.capacity(), capacity);
        assert!(inner.events.iter().all(|e| !is_transient_event(&e.event)));
    }
    let events = session.events();
    assert_eq!(
        serde_json::to_value(&events[..prefix.len()]).unwrap(),
        serde_json::to_value(&prefix).unwrap()
    );
    assert_eq!(events.len(), 4096 + 6 + 6);
    let recovered = Session::load(session.file()).unwrap();
    assert_eq!(
        serde_json::to_value(recovered.events()).unwrap(),
        serde_json::to_value(&events).unwrap()
    );
    assert_eq!(recovered.inner.lock().unwrap().transient_start, None);
    let point = recovered
        .append(SessionEvent::UserMessage {
            text: "rewind point".into(),
            injected: false,
            channel: None,
            images: Vec::new(),
        })
        .unwrap();
    recovered
        .append(SessionEvent::AssistantChunk {
            turn: 4,
            step: 1,
            chunk: denia_core::stream::StreamChunk::TextDelta {
                index: 0,
                text: "discard".into(),
            },
        })
        .unwrap();
    recovered.rewind(point.seq).unwrap();
    assert_eq!(recovered.inner.lock().unwrap().transient_start, None);
    recovered
        .append(SessionEvent::AssistantChunk {
            turn: 4,
            step: 1,
            chunk: denia_core::stream::StreamChunk::TextDelta {
                index: 0,
                text: "new".into(),
            },
        })
        .unwrap();
    assert_eq!(
        recovered.inner.lock().unwrap().transient_start,
        Some(events.len())
    );
    recovered
        .append(SessionEvent::TurnEnd {
            turn: 4,
            reason: TurnEndReason::Completed,
        })
        .unwrap();
    assert_eq!(recovered.events().len(), events.len() + 1);
    assert!(recovered.cool().unwrap());
    recovered.ensure_hot().unwrap();
    assert_eq!(recovered.inner.lock().unwrap().transient_start, None);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn settled_step_releases_chunks_before_turn_end() {
    let root = temp_root();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = Session::create(&root.join("s"), "s".into(), &cwd, true, None).unwrap();
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    for _ in 0..1000 {
        session
            .append(SessionEvent::AssistantChunk {
                turn: 1,
                step: 1,
                chunk: denia_core::stream::StreamChunk::TextDelta {
                    index: 0,
                    text: "fragment".into(),
                },
            })
            .unwrap();
    }
    assert_eq!(session.events().len(), 1001);
    session
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![ContentBlock::Text {
                text: "settled".into(),
            }],
            usage: None,
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        })
        .unwrap();
    assert_eq!(session.events().len(), 2);
    let inner = session.inner.lock().unwrap();
    assert_eq!(inner.transient_bytes, 0);
    assert_eq!(inner.transient_start, None);
    assert!(inner.events.capacity() <= 64);
    drop(inner);
    assert!(!session.cool().unwrap());
    session
        .append(SessionEvent::AssistantChunk {
            turn: 1,
            step: 2,
            chunk: denia_core::stream::StreamChunk::TextDelta {
                index: 0,
                text: "next step".into(),
            },
        })
        .unwrap();
    assert_eq!(session.events_after(1002).len(), 1);
    std::fs::remove_dir_all(&root).unwrap();
}

/// Bug 回归:结算消息与工具调用必须在等待工具执行前就落盘。
///
/// 客户端断线重快照读的是磁盘分页(`store::read_page`,过滤 transient)。
/// 这两个事件若还留在 BufWriter 里,工具执行期间的一次重连就会把刚结算的
/// 正文和工具行从视图里抹掉,直到 ToolResult(原本的 flush 点)才追回来 ——
/// 用户看到的就是"AI 输出完一调工具,前面那段就没了"。
#[test]
fn settle_and_tool_call_reach_disk_before_tool_result() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    for _ in 0..64 {
        session
            .append(SessionEvent::AssistantChunk {
                turn: 1,
                step: 1,
                chunk: denia_core::stream::StreamChunk::TextDelta {
                    index: 0,
                    text: "fragment".into(),
                },
            })
            .unwrap();
    }
    session
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![ContentBlock::Text {
                text: "settled".into(),
            }],
            usage: None,
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        })
        .unwrap();
    session
        .append(SessionEvent::ToolCall {
            turn: 1,
            step: 1,
            call_id: "call-1".into(),
            name: "bash".into(),
            arguments: r#"{"command":"sleep 60"}"#.into(),
        })
        .unwrap();
    // 工具还在跑(尚无 ToolResult);此处不显式 flush,模拟重连后的分页读取。
    let page = store.read_page(&id, None, 100).unwrap();
    assert!(
        page.events.iter().any(|envelope| matches!(
            &envelope.event,
            SessionEvent::AssistantMessage { blocks, .. }
                if blocks.iter().any(|block| matches!(block, ContentBlock::Text { text } if text == "settled"))
        )),
        "结算消息必须在工具执行前对磁盘读者可见"
    );
    assert!(
        page.events
            .iter()
            .any(|envelope| matches!(&envelope.event, SessionEvent::ToolCall { call_id, .. } if call_id == "call-1")),
        "工具调用行必须在工具执行前对磁盘读者可见"
    );
    drop(session);
    std::fs::remove_dir_all(&root).unwrap();
}

/// 累计统计:折叠整份日志,且不受分页 `before`/`limit` 影响。
///
/// 前端状态栏直接吃它当基准 —— 只按窗口折叠的话,刷新/翻页/长会话裁剪
/// 都会让耗时、轮次、token 这些累计值缩水。
#[test]
fn read_page_totals_fold_whole_log_regardless_of_window() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    let usage = |input: u64, output: u64, cache: Option<u64>, reasoning: Option<u64>| {
        denia_core::stream::TokenUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache,
            reasoning_tokens: reasoning,
        }
    };
    let message = |turn: u32, step: u32, usage| SessionEvent::AssistantMessage {
        turn,
        step,
        blocks: vec![ContentBlock::Text {
            text: "答复".into(),
        }],
        usage,
        interrupted: false,
        source_event_seqs: Vec::new(),
        first_token_time: None,
    };

    // 轮次 1:1000 → 4000(3s),1 结算(10/5,缓存 2,推理 1),1 工具调用。
    session
        .append_with_time(SessionEvent::TurnStart { turn: 1 }, 1000)
        .unwrap();
    session
        .append_with_time(
            SessionEvent::UserMessage {
                text: "第一个问题".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            },
            1100,
        )
        .unwrap();
    session
        .append_with_time(
            SessionEvent::AssistantChunk {
                turn: 1,
                step: 1,
                chunk: denia_core::stream::StreamChunk::TextDelta {
                    index: 0,
                    text: "chunk".into(),
                },
            },
            2000,
        )
        .unwrap();
    session
        .append_with_time(message(1, 1, Some(usage(10, 5, Some(2), Some(1)))), 3000)
        .unwrap();
    session
        .append_with_time(
            SessionEvent::ToolCall {
                turn: 1,
                step: 1,
                call_id: "call-1".into(),
                name: "bash".into(),
                arguments: "{}".into(),
            },
            3100,
        )
        .unwrap();
    session
        .append_with_time(
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::Completed,
            },
            4000,
        )
        .unwrap();
    // 轮次 2:5000 → 6500(1.5s),1 结算(20/7,无缓存/推理)。
    session
        .append_with_time(SessionEvent::TurnStart { turn: 2 }, 5000)
        .unwrap();
    session
        .append_with_time(message(2, 1, Some(usage(20, 7, None, None))), 6000)
        .unwrap();
    session
        .append_with_time(
            SessionEvent::TurnEnd {
                turn: 2,
                reason: TurnEndReason::Completed,
            },
            6500,
        )
        .unwrap();
    drop(session);

    let page = store.read_page(&id, None, 100).unwrap();
    assert_eq!(page.totals.turn_ms, 4500);
    assert_eq!(page.totals.turns, 2);
    assert_eq!(page.totals.steps, 2);
    assert_eq!(page.totals.tool_calls, 1);
    assert_eq!(page.totals.input_tokens, 30);
    assert_eq!(page.totals.output_tokens, 12);
    assert_eq!(page.totals.cache_read_tokens, 2);
    assert_eq!(page.totals.reasoning_tokens, 1);

    // 分页窗口只影响 events,统计恒为全会话。
    let windowed = store.read_page(&id, Some(7), 1).unwrap();
    assert!(windowed.events.len() < page.events.len());
    assert_eq!(windowed.totals, page.totals);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn cold_events_after_replays_from_disk() {
    let root = temp_root();
    let dir = root.join("s");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    write_sample_log(&dir, &cwd);

    let cold = Session::open_cold(&dir.join("session.jsonl")).unwrap();
    assert!(cold.events_after(6).is_empty(), "游标在尾部:零回放零 IO");
    let replay = cold.events_after(1);
    assert_eq!(replay.len(), 5, "游标落后:从磁盘回放 seq>1 的事件");
    assert_eq!(replay[0].seq, 2);
    assert_eq!(replay[4].seq, 6);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn ensure_hot_promotes_and_append_continues_seq() {
    let root = temp_root();
    let dir = root.join("s");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    write_sample_log(&dir, &cwd);

    let cold = Session::open_cold(&dir.join("session.jsonl")).unwrap();
    cold.ensure_hot().unwrap();
    assert!(cold.is_hot(), "升级后事件驻留");
    assert_eq!(cold.events().len(), 6);
    let envelope = cold
        .append(SessionEvent::UserMessage {
            text: "继续".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    assert_eq!(envelope.seq, 7, "升级后 append 接续日志 seq");
    // 升级会补跑 load 修复与 meter 重算:聚合仍与磁盘一致。
    assert_eq!(cold.next_turn_number(), 2);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn orphan_turn_repair_deferred_to_ensure_hot() {
    let root = temp_root();
    let dir = root.join("s");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    {
        let session = Session::create(&dir, "s".to_string(), &cwd, true, None).unwrap();
        session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
        session
            .append(SessionEvent::UserMessage {
                text: "孤儿轮".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        // 无 turn-end:崩溃遗留的孤儿轮
        drop(session);
    }

    let cold = Session::open_cold(&dir.join("session.jsonl")).unwrap();
    assert_eq!(
        cold.events_after(0).len(),
        2,
        "冷打开不写盘修复:孤儿轮保持原样"
    );
    cold.ensure_hot().unwrap();
    assert!(
        cold.events()
            .iter()
            .any(|envelope| matches!(envelope.event, SessionEvent::TurnEnd { .. })),
        "升级时补合成 turn-end"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn rewind_on_cold_promotes_first() {
    let root = temp_root();
    let dir = root.join("s");
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    write_sample_log(&dir, &cwd);

    let cold = Session::open_cold(&dir.join("session.jsonl")).unwrap();
    let outcome = cold.rewind(4).unwrap();
    assert_eq!(outcome.removed_events, 3);
    assert!(cold.is_hot(), "破坏性写路径自动升级");
    assert_eq!(cold.events().len(), 3);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn transient_chunks_live_only_for_the_open_turn() {
    let root = temp_root();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let store = SessionStore::open(&root).unwrap();
    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    let chunk = SessionEvent::AssistantChunk {
        turn: 1,
        step: 1,
        chunk: denia_core::stream::StreamChunk::TextDelta {
            index: 0,
            text: "x".into(),
        },
    };

    // 轮次进行中:chunk 驻留(实时回放义务)。
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    session.append(chunk.clone()).unwrap();
    session.append(chunk.clone()).unwrap();
    assert_eq!(session.events().len(), 3, "打开轮次的 chunk 驻留");
    assert!(
        session
            .events_after(1)
            .iter()
            .any(|envelope| matches!(envelope.event, SessionEvent::AssistantChunk { .. }))
    );

    // 闭合:chunk 清扫出内存,磁盘仍完整记录(append-only 不变)。
    session
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: Vec::new(),
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
    assert_eq!(
        session.events().len(),
        3,
        "闭环后只剩 start/message/end,chunk 清扫"
    );
    let mut disk_events = 0;
    store
        .for_each_event(&id, |_| {
            disk_events += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(disk_events, 5, "日志文件逐条完整");

    // 游标落在被清扫的 chunk seq 上:按 seq 回放到后续非 chunk 事件。
    let replay = session.events_after(2);
    assert_eq!(replay[0].seq, 4);

    // 清扫后的空洞不影响 seq 接续。
    let envelope = session
        .append(SessionEvent::UserMessage {
            text: "下一轮".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    assert_eq!(envelope.seq, 6);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn rewind_locates_by_seq_despite_chunk_holes() {
    let root = temp_root();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let store = SessionStore::open(&root).unwrap();
    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    let chunk = || SessionEvent::AssistantChunk {
        turn: 1,
        step: 1,
        chunk: denia_core::stream::StreamChunk::TextDelta {
            index: 0,
            text: "x".into(),
        },
    };
    // 轮次 1:start(1) chunk(2) message(3) end(4);下一轮用户消息(5) + chunk(6)。
    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    session.append(chunk()).unwrap();
    session
        .append(SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: Vec::new(),
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
    session
        .append(SessionEvent::UserMessage {
            text: "第二问".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    session.append(chunk()).unwrap();
    assert_eq!(session.events().len(), 5, "事件表 seq 有空洞(2 缺席)");

    // 回退到 seq 5(用户消息)之前:chunk 空洞下按 seq 定位。
    let outcome = session.rewind(5).unwrap();
    assert_eq!(outcome.to_message.as_deref(), Some("第二问"));
    assert_eq!(outcome.removed_events, 2, "移除用户消息与其后 chunk");
    assert_eq!(session.events().len(), 3);
    assert_eq!(session.events().last().unwrap().seq, 4);

    // 磁盘同步截断:截断点之前的 chunk 行保留(append-only 历史),
    // 之后的用户消息与 chunk 行随尾部一并移除。
    let mut disk_events = 0;
    store
        .for_each_event(&id, |_| {
            disk_events += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(disk_events, 4);
    let envelope = session
        .append(SessionEvent::UserMessage {
            text: "重来".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    assert_eq!(envelope.seq, 5);
    std::fs::remove_dir_all(&root).unwrap();
}

/// 工具的实时输出增量是 transient:只服务"命令还在跑"的观感 ——
/// 不进派生历史、不占分页位次、轮次闭合即从内存回收,而日志里必须
/// 一条不少(append-only 是日志作为唯一真相的前提)。
#[test]
fn tool_output_chunks_stay_out_of_history_and_paging_but_stay_in_the_log() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();

    session.append(SessionEvent::TurnStart { turn: 1 }).unwrap();
    session
        .append(SessionEvent::UserMessage {
            text: "跑一下".into(),
            injected: false,
            images: Vec::new(),
            channel: None,
        })
        .unwrap();
    for text in ["line1\n", "line2\n"] {
        session
            .append(SessionEvent::ToolOutputChunk {
                call_id: "call-1".into(),
                stream: "stdout".into(),
                text: text.into(),
            })
            .unwrap();
    }
    session
        .append(SessionEvent::TurnEnd {
            turn: 1,
            reason: TurnEndReason::Completed,
        })
        .unwrap();

    assert_eq!(session.derive_messages().len(), 1, "只有用户消息进模型历史");
    assert!(
        !session
            .events()
            .iter()
            .any(|envelope| matches!(envelope.event, SessionEvent::ToolOutputChunk { .. })),
        "闭环轮次的临时事件不该继续驻留内存"
    );

    drop(session);
    let page = store.read_page(&id, None, 50).unwrap();
    assert!(
        !page
            .events
            .iter()
            .any(|envelope| matches!(envelope.event, SessionEvent::ToolOutputChunk { .. })),
        "分页的展示粒度不该被实时增量撑开"
    );
    assert_eq!(page.total, 3, "展示事件只有 turn-start/user/turn-end");

    let raw = std::fs::read_to_string(store.root().join(&id).join("session.jsonl")).unwrap();
    assert_eq!(
        raw.matches("tool-output-chunk").count(),
        2,
        "日志里必须完整保留每一条(append-only)"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

/// 一次验证结论(宿主执行链产物;冻结 `src/lib.rs` 的摘要,退出码决定结论)。
fn task_validation_event(
    session: &Session,
    revision: &str,
    exit_code: i32,
    at: u64,
) -> SessionEvent {
    let result = ValidationResult::from_check_runs(
        RevisionId::new(revision),
        vec![CheckRun {
            command: "cargo test".into(),
            exit_code: Some(exit_code),
            expect_exit_code: 0,
            workdir: ".".into(),
            fingerprints: vec![FileFingerprint {
                path: "src/lib.rs".into(),
                digest: "d1".into(),
            }],
        }],
        Some(session.id().to_string()),
        at,
    )
    .expect("有检查运行才构成结论");
    SessionEvent::Task {
        op: TaskOp::RecordValidation { result },
    }
}

/// 旧日志里逐字落盘的一行 `task` 事件(当年 `TaskOp` 的形状)。
fn legacy_task_line(seq: u64, op: &str) -> String {
    format!(
        "{{\"seq\":{seq},\"time\":{},\"type\":\"task\",\"op\":{op}}}",
        1_000 + seq
    )
}

/// 一条非注入的用户消息。
fn user_note(text: &str) -> SessionEvent {
    SessionEvent::UserMessage {
        text: text.into(),
        injected: false,
        images: Vec::new(),
        channel: None,
    }
}

/// 任务验证账本的事件源折叠:宿主写下通过结论 → 验收通过。热加载与冷加载
/// 两条路径必须折出逐字段相同的账本(冷态不驻留事件,只能靠解析时的聚合)。
#[test]
fn task_ledger_folds_across_hot_and_cold_loads() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    assert_eq!(session.task(), None, "没有任务事件就没有账本");

    session
        .append(user_note("修好解析器,跑通测试再说完事"))
        .unwrap();
    session
        .append(task_validation_event(&session, "rev-a", 0, 2_000))
        .unwrap();
    let state = session.task().expect("写下验证结论后账本在");
    assert_eq!(state.revision.as_str(), "rev-a");
    assert_eq!(
        state.session.as_deref(),
        Some(id.as_str()),
        "折叠方身份写进账本"
    );
    assert_eq!(state.outcome(), TaskOutcome::Passed, "通过结论即验收通过");
    let expected = session.task().unwrap();
    drop(session);

    let hot = store.load(&id).unwrap();
    assert_eq!(hot.task().as_ref(), Some(&expected), "热加载折出同一份账本");
    drop(hot);

    let cold = store.open_cold(&id).unwrap();
    assert!(!cold.is_hot());
    assert_eq!(
        cold.task().as_ref(),
        Some(&expected),
        "冷加载在解析时折出同一份账本(不靠 ensure_hot)"
    );
    drop(cold);
    drop(store);
    std::fs::remove_dir_all(&root).unwrap();
}

/// rewind 要把任务账本**同时**重置与重放(只清不重放会丢账本,只重放
/// 不清会留下被截掉的旧结论),并且被截断区间里出现过的 revision 身份保持
/// 报废:那些事件在日志里已经查不到,防线只能来自宿主交出的报废集合。
#[test]
fn rewind_refolds_task_ledger_and_retires_truncated_revisions() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = store.create(&cwd, true).unwrap();
    let dir = session.directory().to_path_buf();

    session
        .append(user_note("修好解析器,跑通测试再说完事"))
        .unwrap();
    session
        .append(task_validation_event(&session, "rev-a", 0, 2_000))
        .unwrap();
    assert_eq!(session.task().unwrap().outcome(), TaskOutcome::Passed);

    // 被截断的分支:第二轮又跑了一次检查(这次失败),然后回退掉。
    let second = session.append(user_note("再改一处导入顺序")).unwrap();
    session
        .append(task_validation_event(&session, "rev-a", 1, 2_100))
        .unwrap();
    assert_eq!(session.task().unwrap().outcome(), TaskOutcome::Failed);

    session.rewind(second.seq).unwrap();
    let state = session.task().expect("回退后账本还在");
    assert_eq!(state.revision.as_str(), "rev-a", "回退后回到被截断前的版本");
    assert_eq!(state.validations.len(), 1, "被截掉的结论不得残留");
    assert!(state.revision_retired, "被截断区间里出现过的身份已报废");
    assert_eq!(
        state.verification(),
        VerificationState::Unverified {
            reason: StaleReason::RevisionChanged
        }
    );
    assert_eq!(
        state.outcome(),
        TaskOutcome::Unverified,
        "旧分支的通过结论不得被继承"
    );

    // 重新取证:宿主换新身份。旧身份不复活,新结论照常生效。
    session
        .append(task_validation_event(&session, "rev-b", 0, 2_200))
        .unwrap();
    let state = session.task().unwrap();
    assert_eq!(state.revision.as_str(), "rev-b");
    assert!(!state.revision_retired, "换新身份之后账本重新可裁决");
    assert_eq!(state.outcome(), TaskOutcome::Passed);

    let audit = std::fs::read_to_string(dir.join("rewinds.jsonl")).unwrap();
    assert!(
        audit.contains(r#""retiredRevisions":["rev-a"]"#),
        "回退审计必须带上本次截掉的身份:{audit}"
    );
    drop(session);
    drop(store);
    std::fs::remove_dir_all(&root).unwrap();
}

/// 报废集合必须活过重启:`session.jsonl` 里已经没有被截断分支的痕迹,重开
/// (冷态读 → 写路径升级整份重读)之后仍然不继承旧身份的结论。
#[test]
fn retired_revisions_survive_a_reload() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();

    session
        .append(user_note("修好解析器,跑通测试再说完事"))
        .unwrap();
    session
        .append(task_validation_event(&session, "rev-a", 0, 2_000))
        .unwrap();
    let second = session.append(user_note("再改一处导入顺序")).unwrap();
    session
        .append(task_validation_event(&session, "rev-a", 1, 2_100))
        .unwrap();
    session.rewind(second.seq).unwrap();
    drop(session);

    let reopened = store.open_cold(&id).unwrap();
    let state = reopened.task().unwrap();
    assert_eq!(state.revision.as_str(), "rev-a");
    assert!(state.revision_retired, "重开之后旧分支身份仍然报废");
    assert_eq!(state.outcome(), TaskOutcome::Unverified);
    // append 会先把冷会话升级为热(整份重读),报废集合从 rewinds.jsonl 回来。
    reopened
        .append(task_validation_event(&reopened, "rev-b", 0, 2_200))
        .unwrap();
    assert_eq!(reopened.task().unwrap().outcome(), TaskOutcome::Passed);
    drop(reopened);
    drop(store);
    std::fs::remove_dir_all(&root).unwrap();
}

/// 旧日志里落过账的记账事件(`open` / `amend` / `record-change` / `close`)
/// 在裁剪掉记账层之后由 `TaskOp::Legacy` 兜底反序列化:整个会话照常打开,
/// 账本只认宿主写下的验证结论 —— 当年的目标、验收项与事实不再进裁决,
/// 但也不阻塞加载。
#[test]
fn legacy_task_events_in_an_old_log_still_load() {
    let root = temp_root();
    let store = SessionStore::open(&root).unwrap();
    let cwd = root.join("work");
    std::fs::create_dir_all(&cwd).unwrap();
    let session = store.create(&cwd, true).unwrap();
    let id = session.id().to_string();
    session
        .append(user_note("修好解析器,跑通测试再说完事"))
        .unwrap();
    let file = session.file().to_path_buf();
    drop(session);

    // 逐字写进当年落盘的那几行(不是今天 `TaskOp` 的形状)。
    {
        use std::io::Write as _;
        let mut f = OpenOptions::new().append(true).open(&file).unwrap();
        for line in [
            legacy_task_line(
                2,
                r#"{"kind":"open","task_id":"task-1","revision":"rev-legacy","goal":"修好解析器","requirements":[{"id":"req-1","kind":"acceptance","text":"cargo test 全绿","source":{"session":null,"seq":1,"time_ms":1001,"quote":"跑通测试"}}]}"#,
            ),
            legacy_task_line(
                3,
                r#"{"kind":"amend","notes":[{"kind":"question","id":"note-1","text":"要不要兼容旧格式?"}]}"#,
            ),
            legacy_task_line(
                4,
                r#"{"kind":"record-change","summary":"改了 src/lib.rs","fingerprints":[]}"#,
            ),
            legacy_task_line(5, r#"{"kind":"close"}"#),
        ] {
            writeln!(f, "{line}").unwrap();
        }
        f.flush().unwrap();
    }

    let reopened = store.load(&id).unwrap();
    let events = reopened.events();
    assert_eq!(events.len(), 5, "旧事件必须一条不少地读进来:{events:?}");
    assert!(
        matches!(events[2].event, SessionEvent::Task { op: TaskOp::Legacy }),
        "被裁掉的记账 op 由 Legacy 兜底:{:?}",
        events[2].event
    );
    assert_eq!(
        reopened.task(),
        None,
        "只有被裁掉的记账事件:折不出账本,也没把谁判成通过"
    );

    // 旧会话继续可用:宿主写下新的验证结论,账本按新语义折出结局。
    reopened
        .append(task_validation_event(&reopened, "rev-legacy", 0, 2_000))
        .unwrap();
    assert_eq!(
        reopened.task().unwrap().outcome(),
        TaskOutcome::Passed,
        "旧日志里的记账事件被忽略,新的验证结论照常生效"
    );
    drop(reopened);
    drop(store);
    std::fs::remove_dir_all(&root).unwrap();
}
