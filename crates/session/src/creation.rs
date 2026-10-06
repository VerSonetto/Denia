//! Session creation responsibilities.
use super::*;

impl Session {
    /// Creates a fresh session file (header line only) inside `dir`.
    /// `id` becomes both the header id and, by store convention, the
    /// directory name. `sandbox` confines file tools to `cwd`.
    /// `parent_session` 记录分支血缘(非分支会话为 `None`)。
    pub fn create(
        dir: &Path,
        id: String,
        cwd: &Path,
        sandbox: bool,
        parent_session: Option<String>,
    ) -> Result<Session, SessionError> {
        Self::create_described(dir, id, cwd, sandbox, parent_session, None)
    }

    pub(super) fn create_described(
        dir: &Path,
        id: String,
        cwd: &Path,
        sandbox: bool,
        parent_session: Option<String>,
        subagent: Option<denia_core::session::SubagentDescriptor>,
    ) -> Result<Session, SessionError> {
        std::fs::create_dir_all(dir)?;
        let file = dir.join("session.jsonl");
        let header = SessionHeader {
            kind: SessionHeaderKind::Session,
            version: SESSION_FORMAT_VERSION,
            id,
            created_at: now_millis(),
            cwd: cwd.to_string_lossy().to_string(),
            sandbox,
            parent_session,
            subagent,
        };
        {
            let mut handle = File::create(&file)?;
            let header_line = serde_json::to_string(&header)?;
            writeln!(handle, "{}", header_line)?;
            handle.flush()?;
            let base_offset = header_line.len() as u64 + 1;
            let writer = open_append_writer(&file)?;
            Ok(Session {
                header,
                file,
                inner: Mutex::new(SessionInner {
                    events: Vec::new(),
                    writer,
                    offsets: Vec::new(),
                    base_offset,
                    next_offset: base_offset,
                    last_turn: 0,
                    last_system_prompt: None,
                    meter: ContextMeter::new(),
                    pending_turn: Vec::new(),
                    permission_mode: PermissionMode::AutoEdit,
                    agent_preset: None,
                    goal: None,
                    title: None,
                    derived_surface: None,
                    derived_revision: 0,
                    log_revision: 0,
                    cold: false,
                    last_seq: 0,
                    resident_bytes: 0,
                    transient_bytes: 0,
                    first_prompt_excerpt: None,
                    history_projection: None,
                }),
            })
        }
    }
}
