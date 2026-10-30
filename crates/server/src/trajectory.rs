//! 会话轨迹与日志查询：把 append-only 会话日志投影成**结构化、可检索、已脱敏**
//! 的记录，供模型自查与错误定位；另用一份轻量辅助日志补记队列发送与子代理
//! 定义等不落在 SessionEvent 里的事实。
//!
//! 设计约束：
//! - **不复制会话日志**：用户消息/模型输出/工具调用/错误都直接从
//!   `session.jsonl` 投影，避免第二份事实源漂移；
//! - **默认摘要**：工具参数与结果按字符截断并脱敏，`RequestHeader` 等大块
//!   内容不返回；查询上限由工具参数与硬上限双重约束；
//! - **权限按关系判定**：会话只能查自己、直接父代理与直接子代理；越界拒绝，
//!   不看模型给的任意 id。

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use denia_core::session::{SessionEnvelope, SessionEvent, SessionHeader, TurnEndReason};
use denia_core::stream::ContentBlock;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 单条记录里文本字段的默认上限（字符）。
const DEFAULT_TEXT_LIMIT: usize = 600;
/// 辅助轨迹文件的体积上限；超出后整体轮换，避免长期使用无限增长。
const AUX_FILE_LIMIT: u64 = 1_048_576;
/// 工具查询条数硬上限。
pub const TRACE_LIMIT_MAX: usize = 200;
/// 工具查询条数默认值。
pub const TRACE_LIMIT_DEFAULT: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceError {
    /// tool / turn / retry / api。
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
}

/// 一条轨迹记录。字段按"先定位、再看事件、最后看错误"排列。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceRecord {
    pub time: u64,
    pub session: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// user / queue / assistant / tool_call / tool_result / turn / state / subagent / error。
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<TraceError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl TraceRecord {
    pub fn new(session: &str, time: u64, kind: &str) -> Self {
        Self {
            time,
            session: session.to_string(),
            seq: None,
            kind: kind.to_string(),
            role: None,
            agent_id: None,
            parent_session: None,
            turn: None,
            step: None,
            operation: None,
            tool: None,
            call_id: None,
            text: None,
            error: None,
            data: None,
        }
    }

    /// 以当前时刻构造记录（辅助轨迹用）。
    pub fn now(session: &str, kind: &str) -> Self {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0);
        Self::new(session, time, kind)
    }

    /// 写盘前的统一清洗：文本、错误消息与 data 里的字符串都要脱敏并截断。
    ///
    /// 辅助轨迹的 `text` 可能来自用户 prompt 或代理消息，不能假定上游已经
    /// 处理过。会话日志投影路径在 map 里逐条脱敏；这里兜住辅助路径。
    pub fn sanitized(mut self) -> Self {
        if let Some(text) = &mut self.text {
            *text = clip(&redact_secrets(text), DEFAULT_TEXT_LIMIT);
        }
        if let Some(error) = &mut self.error {
            error.message = clip(&redact_secrets(&error.message), DEFAULT_TEXT_LIMIT);
            if let Some(context) = &mut error.context {
                sanitize_value(context);
            }
        }
        if let Some(data) = &mut self.data {
            sanitize_value(data);
        }
        self
    }
}

fn sanitize_value(value: &mut Value) {
    match value {
        Value::String(text) => {
            *text = clip(&redact_secrets(text), DEFAULT_TEXT_LIMIT);
        }
        Value::Array(items) => {
            for item in items {
                sanitize_value(item);
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                sanitize_value(item);
            }
        }
        _ => {}
    }
}

/// 查询过滤条件（工具参数与 API 共用）。
#[derive(Debug, Clone, Default)]
pub struct TraceFilter {
    pub kinds: Option<HashSet<String>>,
    pub errors_only: bool,
    pub query: Option<String>,
    pub since: Option<u64>,
    pub limit: usize,
    pub text_limit: usize,
}

impl TraceFilter {
    pub fn from_args(args: &SessionTraceArgs) -> Self {
        let kinds = args
            .kind
            .as_ref()
            .map(|items| items.iter().map(|item| item.to_lowercase()).collect());
        Self {
            kinds,
            errors_only: args.level.as_deref() == Some("error"),
            query: args
                .query
                .as_ref()
                .map(|text| text.trim().to_lowercase())
                .filter(|text| !text.is_empty()),
            since: args.since,
            limit: args
                .limit
                .unwrap_or(TRACE_LIMIT_DEFAULT)
                .clamp(1, TRACE_LIMIT_MAX),
            text_limit: args
                .text_limit
                .unwrap_or(DEFAULT_TEXT_LIMIT)
                .clamp(120, 4000),
        }
    }

    pub(crate) fn accepts(&self, record: &TraceRecord) -> bool {
        if self.errors_only && record.kind != "error" {
            return false;
        }
        if let Some(kinds) = &self.kinds
            && !kinds.contains(&record.kind)
        {
            return false;
        }
        if let Some(since) = self.since
            && record.time < since
        {
            return false;
        }
        if let Some(query) = &self.query {
            let mut haystack = String::new();
            if let Some(text) = &record.text {
                haystack.push_str(text);
            }
            if let Some(tool) = &record.tool {
                haystack.push(' ');
                haystack.push_str(tool);
            }
            if let Some(operation) = &record.operation {
                haystack.push(' ');
                haystack.push_str(operation);
            }
            if let Some(error) = &record.error {
                haystack.push(' ');
                haystack.push_str(&error.message);
                if let Some(code) = &error.code {
                    haystack.push(' ');
                    haystack.push_str(code);
                }
            }
            if !haystack.to_lowercase().contains(query) {
                return false;
            }
        }
        true
    }
}

/// 查询参数（工具 schema 与 API query 的公共形状）。
pub use denia_tools::runtime_command::SessionTraceArgs;

/// 查询结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TraceOutput {
    pub session: String,
    pub role: String,
    pub parent_session: Option<String>,
    pub records: Vec<TraceRecord>,
    /// 过滤后命中的总条数（可能大于 `records.len()`）。
    pub matched: usize,
    /// 扫描的事件数（含被过滤掉的）。
    pub scanned: usize,
    /// 是否因为 limit 截断。
    pub truncated: bool,
    pub note: String,
}

/// 增量投影器：按时间顺序喂事件，内部维护 tool_call → name 映射。
pub struct TraceProjector<'a> {
    session_id: &'a str,
    header: &'a SessionHeader,
    filter: &'a TraceFilter,
    tools: HashMap<String, String>,
    records: VecDeque<TraceRecord>,
    scanned: usize,
    matched: usize,
}

impl<'a> TraceProjector<'a> {
    pub fn new(session_id: &'a str, header: &'a SessionHeader, filter: &'a TraceFilter) -> Self {
        Self {
            session_id,
            header,
            filter,
            tools: HashMap::new(),
            records: VecDeque::new(),
            scanned: 0,
            matched: 0,
        }
    }

    pub fn push(&mut self, envelope: SessionEnvelope) {
        self.scanned += 1;
        let Some(record) = self.map(&envelope) else {
            return;
        };
        if !self.filter.accepts(&record) {
            return;
        }
        self.matched += 1;
        if self.records.len() == self.filter.limit {
            self.records.pop_front();
        }
        self.records.push_back(record);
    }

    pub fn finish(self) -> TraceOutput {
        let role = if self.header.subagent.is_some() {
            "subagent"
        } else {
            "main"
        };
        TraceOutput {
            session: self.session_id.to_string(),
            role: role.to_string(),
            parent_session: self.header.parent_session.clone(),
            truncated: self.matched > self.records.len(),
            records: self.records.into_iter().collect(),
            matched: self.matched,
            scanned: self.scanned,
            note: "记录来自会话日志的结构化投影；文本已脱敏并按上限截断。locator: session+seq(+turn/step/callId)。"
                .to_string(),
        }
    }

    fn base(&self, envelope: &SessionEnvelope, kind: &str) -> TraceRecord {
        let mut record = TraceRecord::new(self.session_id, envelope.time, kind);
        record.seq = Some(envelope.seq);
        record.role = Some(if self.header.subagent.is_some() {
            "subagent".into()
        } else {
            "main".into()
        });
        record.agent_id = Some(self.session_id.to_string());
        record.parent_session = self.header.parent_session.clone();
        record
    }

    fn map(&mut self, envelope: &SessionEnvelope) -> Option<TraceRecord> {
        let text_limit = self.filter.text_limit;
        let redact = |text: &str| clip(&redact_secrets(text), text_limit);
        match &envelope.event {
            SessionEvent::UserMessage {
                text,
                injected,
                channel,
                images,
            } => {
                // 注入通道（工作区指令/技能目录/反馈）不是用户输入，默认不进
                // 轨迹；真需要排查时可用原始日志。
                if *injected {
                    return None;
                }
                let queue = channel
                    .as_deref()
                    .is_some_and(|value| value.starts_with("steering:"));
                let mut record = self.base(envelope, if queue { "queue" } else { "user" });
                record.operation = Some(if queue {
                    "steer_delivered".into()
                } else {
                    "user_message".into()
                });
                record.text = Some(redact(text));
                record.data = Some(json!({
                    "images": images.len(),
                    "channel": channel,
                }));
                Some(record)
            }
            SessionEvent::AssistantMessage {
                turn,
                step,
                blocks,
                usage,
                interrupted,
                ..
            } => {
                let text = blocks
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                if text.trim().is_empty() {
                    return None;
                }
                let mut record = self.base(envelope, "assistant");
                record.turn = Some(*turn);
                record.step = Some(*step);
                record.operation = Some("assistant_message".into());
                record.text = Some(redact(&text));
                record.data = Some(json!({
                    "interrupted": interrupted,
                    "usage": usage,
                }));
                Some(record)
            }
            SessionEvent::ToolCall {
                turn,
                step,
                call_id,
                name,
                arguments,
            } => {
                self.tools.insert(call_id.clone(), name.clone());
                let mut record = self.base(envelope, "tool_call");
                record.turn = Some(*turn);
                record.step = Some(*step);
                record.operation = Some("tool_call".into());
                record.tool = Some(name.clone());
                record.call_id = Some(call_id.clone());
                record.text = Some(redact(arguments));
                Some(record)
            }
            SessionEvent::ToolResult {
                turn,
                step,
                call_id,
                content,
                is_error,
                error,
                error_identity,
                truncation,
                ..
            } => {
                let tool = self.tools.get(call_id).cloned();
                let kind = if *is_error { "error" } else { "tool_result" };
                let mut record = self.base(envelope, kind);
                record.turn = Some(*turn);
                record.step = Some(*step);
                record.operation = Some("tool_result".into());
                record.tool = tool;
                record.call_id = Some(call_id.clone());
                record.text = Some(redact(content));
                if *is_error {
                    record.error = Some(TraceError {
                        kind: "tool".into(),
                        code: error_identity
                            .as_ref()
                            .map(|identity| identity.code.clone())
                            .or_else(|| error.clone()),
                        message: redact(content),
                        context: Some(json!({
                            "callId": call_id,
                            "tool": record.tool,
                            "errorIdentity": error_identity,
                            "truncation": truncation,
                        })),
                    });
                }
                Some(record)
            }
            SessionEvent::TurnEnd { turn, reason } => {
                let mut record = self.base(envelope, "turn");
                record.turn = Some(*turn);
                record.operation = Some("turn_end".into());
                match reason {
                    TurnEndReason::Error { failure } => {
                        record.kind = "error".into();
                        record.error = Some(TraceError {
                            kind: "turn".into(),
                            code: Some(failure.code.clone()),
                            message: clip(&redact_secrets(&failure.message), text_limit),
                            context: Some(json!({"turn": turn})),
                        });
                    }
                    TurnEndReason::LoopDetected { repeats } => {
                        record.kind = "error".into();
                        record.error = Some(TraceError {
                            kind: "turn".into(),
                            code: Some("loop-detected".into()),
                            message: format!("检测到连续重复输出（{repeats} 次），轮次被强制结束"),
                            context: Some(json!({"turn": turn, "repeats": repeats})),
                        });
                    }
                    TurnEndReason::Interrupted => {
                        record.kind = "error".into();
                        record.error = Some(TraceError {
                            kind: "turn".into(),
                            code: Some("interrupted".into()),
                            message: "检测到崩溃残留的未闭合轮次，已合成闭合".into(),
                            context: Some(json!({"turn": turn})),
                        });
                    }
                    TurnEndReason::Aborted { cause } => {
                        record.operation = Some("turn_aborted".into());
                        record.data = Some(json!({"cause": cause}));
                    }
                    _ => {}
                }
                Some(record)
            }
            SessionEvent::RetryAttempt {
                turn,
                step,
                attempt,
                code,
                message,
                delay_ms,
            } => {
                let mut record = self.base(envelope, "error");
                record.turn = Some(*turn);
                record.step = Some(*step);
                record.operation = Some("retry".into());
                record.error = Some(TraceError {
                    kind: "retry".into(),
                    code: Some(code.clone()),
                    message: clip(&redact_secrets(message), text_limit),
                    context: Some(json!({"attempt": attempt, "delayMs": delay_ms})),
                });
                Some(record)
            }
            SessionEvent::AgentInbox { id, text, source } => {
                let mut record = self.base(envelope, "subagent");
                record.operation = Some("inbox".into());
                record.call_id = Some(id.clone());
                record.text = Some(redact(text));
                record.data = Some(json!({"source": source}));
                Some(record)
            }
            SessionEvent::AgentDelivery { id, text, source } => {
                let mut record = self.base(envelope, "subagent");
                record.operation = Some("delivery".into());
                record.call_id = Some(id.clone());
                record.text = Some(redact(text));
                record.data = Some(json!({"source": source}));
                Some(record)
            }
            SessionEvent::Goal { op } => Some(self.state(envelope, "goal", json!({"op": op}))),
            SessionEvent::Task { op } => Some(self.state(envelope, "task", json!({"op": op}))),
            SessionEvent::PermissionMode { mode } => {
                Some(self.state(envelope, "permission_mode", json!({"mode": mode})))
            }
            SessionEvent::SessionTitle { title } => {
                Some(self.state(envelope, "session_title", json!({"title": redact(title)})))
            }
            SessionEvent::AgentPreset { preset } => {
                Some(self.state(envelope, "agent_preset", json!({"preset": preset})))
            }
            SessionEvent::CompactionSummary {
                turn,
                pre_tokens,
                post_tokens,
                ..
            } => Some(self.state(
                envelope,
                "compaction",
                json!({"turn": turn, "preTokens": pre_tokens, "postTokens": post_tokens}),
            )),
            SessionEvent::CommandRun { name, text } => {
                let mut record = self.base(envelope, "state");
                record.operation = Some("command_run".into());
                record.text = Some(redact(text));
                record.data = Some(json!({"name": name}));
                Some(record)
            }
            SessionEvent::TodoWrite { todos } => {
                Some(self.state(envelope, "todo_write", json!({"count": todos.len()})))
            }
            _ => None,
        }
    }

    fn state(&self, envelope: &SessionEnvelope, operation: &str, data: Value) -> TraceRecord {
        let mut record = self.base(envelope, "state");
        record.operation = Some(operation.to_string());
        record.data = Some(data);
        record
    }
}

/// 读取并投影一份会话日志（冷路径：流式扫盘，不驻留全部事件）。
pub fn project_cold(
    store: &denia_session::SessionStore,
    session_id: &str,
    filter: &TraceFilter,
) -> Result<TraceOutput, String> {
    let header = store
        .read_log_header(session_id)
        .map_err(|error| error.to_string())?;
    let mut projector = TraceProjector::new(session_id, &header, filter);
    store
        .for_each_event(session_id, |envelope| {
            projector.push(envelope);
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    Ok(projector.finish())
}

/// 读取并投影内存中的热会话（包含当前轮尚未 flush 的尾部事件）。
pub fn project_hot(session: &denia_session::Session, filter: &TraceFilter) -> TraceOutput {
    let header = session.header();
    let mut projector = TraceProjector::new(session.id(), &header, filter);
    for envelope in session.events() {
        projector.push(envelope);
    }
    projector.finish()
}

/// 轻量辅助轨迹：记录不落在 SessionEvent 里的动作（队列提交、子代理定义等）。
///
/// 存 `<home>/traces/<session>.jsonl`，单文件超过 1 MiB 时轮换为 `.old`。
/// 写入是低频同步 append：这些动作一次轮次最多几条，不会成为热路径。
pub struct TraceStore {
    root: PathBuf,
}

impl TraceStore {
    pub fn new(home: &Path) -> Self {
        Self {
            root: home.join("traces"),
        }
    }

    pub fn record(&self, record: &TraceRecord) {
        if !valid_session_id(&record.session) {
            return;
        }
        let safe = record.clone().sanitized();
        let Ok(line) = serde_json::to_string(&safe) else {
            return;
        };
        let dir = &self.root;
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let path = dir.join(format!("{}.jsonl", record.session));
        if let Ok(meta) = std::fs::metadata(&path)
            && meta.len() > AUX_FILE_LIMIT
        {
            let _ = std::fs::rename(&path, dir.join(format!("{}.old.jsonl", record.session)));
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            let _ = file.write_all(line.as_bytes());
            let _ = file.write_all(b"\n");
        }
    }

    /// 读取该会话最近 `limit` 条辅助记录（时间正序）。
    pub fn read(&self, session: &str, limit: usize) -> Vec<TraceRecord> {
        if !valid_session_id(session) {
            return Vec::new();
        }
        let path = self.root.join(format!("{session}.jsonl"));
        let Ok(file) = std::fs::File::open(path) else {
            return Vec::new();
        };
        let mut records: VecDeque<TraceRecord> = VecDeque::new();
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(record) = serde_json::from_str::<TraceRecord>(&line) else {
                continue;
            };
            if records.len() == limit {
                records.pop_front();
            }
            records.push_back(record);
        }
        records.into_iter().collect()
    }
}

fn valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
}

fn clip(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let clipped: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        format!("{clipped}…[截断]")
    } else {
        clipped
    }
}

/// 对辅助轨迹记录应用与会话投影相同的脱敏与截断。
///
/// 辅助记录由多个入口写入（队列提交、`send_message`、子代理定义等），其中
/// 文本可能包含用户输入里的密钥；读取时统一处理，兼容修复前已落盘的旧
/// 记录。返回新记录，不修改磁盘事实。
pub fn sanitize_record(mut record: TraceRecord, text_limit: usize) -> TraceRecord {
    if let Some(text) = record.text.take() {
        record.text = Some(clip(&redact_secrets(&text), text_limit));
    }
    if let Some(error) = record.error.as_mut() {
        error.message = clip(&redact_secrets(&error.message), text_limit);
        if let Some(context) = error.context.take() {
            error.context = Some(redact_json(context, text_limit));
        }
    }
    if let Some(data) = record.data.take() {
        record.data = Some(redact_json(data, text_limit));
    }
    record
}

fn redact_json(value: Value, text_limit: usize) -> Value {
    match value {
        Value::String(text) => Value::String(clip(&redact_secrets(&text), text_limit)),
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| redact_json(item, text_limit))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, item)| (key, redact_json(item, text_limit)))
                .collect(),
        ),
        other => other,
    }
}

fn redact_secrets(text: &str) -> String {
    static KEY_RE: OnceLock<Regex> = OnceLock::new();
    static TOKEN_RE: OnceLock<Regex> = OnceLock::new();
    static BEARER_RE: OnceLock<Regex> = OnceLock::new();
    let key_re = KEY_RE.get_or_init(|| {
        Regex::new(r#"(?i)(api[_-]?key|access[_-]?token|refresh[_-]?token|secret|password|authorization)\s*[:=]\s*["']?([^\s"',;}]{3,})"#)
            .expect("valid key regex")
    });
    let token_re = TOKEN_RE.get_or_init(|| {
        Regex::new(
            r"(?:sk-[A-Za-z0-9_-]{12,}|gh[pousr]_[A-Za-z0-9]{20,}|xox[baprs]-[A-Za-z0-9-]{10,})",
        )
        .expect("valid token regex")
    });
    let bearer_re = BEARER_RE.get_or_init(|| {
        Regex::new(r"(?i)bearer\s+[A-Za-z0-9._~+/=\-]{8,}").expect("valid bearer regex")
    });
    // 先处理 Bearer：否则 `Authorization: Bearer <token>` 会被键值规则
    // 先吃掉 `Bearer` 这个词，留下后面的 token 不再匹配。
    let out = bearer_re
        .replace_all(text, "Bearer [REDACTED]")
        .into_owned();
    let out = key_re.replace_all(&out, "$1=[REDACTED]").into_owned();
    token_re.replace_all(&out, "[REDACTED]").into_owned()
}

/// 会话关系权限：只能查自己、直接父代理、直接子代理。
pub fn related(current: &str, target: &str, parent_of: &HashMap<String, String>) -> bool {
    if current == target {
        return true;
    }
    if parent_of
        .get(target)
        .is_some_and(|parent| parent == current)
    {
        return true;
    }
    if parent_of
        .get(current)
        .is_some_and(|parent| parent == target)
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::config::LlmCallConfig;
    use denia_core::session::{RequestHeaderReason, RequestHeaderSnapshot, SessionHeaderKind};

    fn header() -> SessionHeader {
        SessionHeader {
            kind: SessionHeaderKind::Session,
            version: 1,
            id: "trace-session".into(),
            created_at: 0,
            cwd: "/tmp".into(),
            sandbox: false,
            parent_session: None,
            subagent: None,
        }
    }

    fn envelope(seq: u64, event: SessionEvent) -> SessionEnvelope {
        SessionEnvelope {
            seq,
            time: seq,
            event,
        }
    }

    fn filter() -> TraceFilter {
        TraceFilter {
            limit: TRACE_LIMIT_MAX,
            text_limit: DEFAULT_TEXT_LIMIT,
            ..Default::default()
        }
    }

    #[test]
    fn redacts_common_secret_shapes() {
        let text =
            r#"api_key="sk-abcdefghijklmnopqrstuvwxyz" Authorization: Bearer abcdefghijklmnop"#;
        let redacted = redact_secrets(text);
        assert!(
            !redacted.contains("sk-abcdefghijklmnopqrstuvwxyz"),
            "{redacted}"
        );
        assert!(!redacted.contains("abcdefghijklmnop"), "{redacted}");
        assert!(redacted.contains("[REDACTED]"), "{redacted}");
    }

    #[test]
    fn auxiliary_records_are_sanitized_before_exposure() {
        let mut record = TraceRecord::new("trace-session", 1, "subagent");
        record.text = Some("api_key=supersecret123 sk-abcdefghijklmnopqrstuvwxyz".into());
        record.data = Some(json!({"note": "Authorization: Bearer abcdefghijklmnop"}));
        let sanitized = sanitize_record(record, DEFAULT_TEXT_LIMIT);
        let serialized = serde_json::to_string(&sanitized).unwrap();
        assert!(!serialized.contains("supersecret123"), "{serialized}");
        assert!(
            !serialized.contains("sk-abcdefghijklmnopqrstuvwxyz"),
            "{serialized}"
        );
        assert!(!serialized.contains("abcdefghijklmnop"), "{serialized}");
        assert!(serialized.contains("[REDACTED]"), "{serialized}");
    }

    #[test]
    fn clips_long_text_with_marker() {
        assert_eq!(clip("abcdef", 3), "abc…[截断]");
        assert_eq!(clip("abc", 3), "abc");
    }

    #[test]
    fn limit_keeps_latest_records_and_reports_truncation() {
        let header = header();
        let mut filter = filter();
        filter.limit = 2;
        let mut projector = TraceProjector::new("trace-session", &header, &filter);
        for index in 1..=5 {
            projector.push(envelope(
                index,
                SessionEvent::UserMessage {
                    text: format!("消息-{index}"),
                    injected: false,
                    channel: None,
                    images: Vec::new(),
                },
            ));
        }
        let output = projector.finish();
        assert_eq!(output.matched, 5);
        assert_eq!(output.records.len(), 2);
        assert!(output.truncated);
        assert_eq!(output.records[0].text.as_deref(), Some("消息-4"));
        assert_eq!(output.records[1].text.as_deref(), Some("消息-5"));
    }

    #[test]
    fn project_turn_failures_and_retries_with_locator_and_types() {
        let header = header();
        let filter = filter();
        let mut projector = TraceProjector::new("trace-session", &header, &filter);
        projector.push(envelope(
            1,
            SessionEvent::TurnEnd {
                turn: 1,
                reason: TurnEndReason::LoopDetected { repeats: 3 },
            },
        ));
        projector.push(envelope(
            2,
            SessionEvent::TurnEnd {
                turn: 2,
                reason: TurnEndReason::Interrupted,
            },
        ));
        projector.push(envelope(
            3,
            SessionEvent::RetryAttempt {
                turn: 3,
                step: 1,
                attempt: 2,
                code: "provider/rate-limited".into(),
                message: "Authorization: Bearer abcdefghijklmnop".into(),
                delay_ms: 500,
            },
        ));
        let serialized = serde_json::to_string(&projector.finish()).unwrap();
        assert!(serialized.contains("loop-detected"), "{serialized}");
        assert!(serialized.contains("interrupted"), "{serialized}");
        assert!(serialized.contains("provider/rate-limited"), "{serialized}");
        assert!(serialized.contains("\"kind\":\"turn\""), "{serialized}");
        assert!(serialized.contains("\"kind\":\"retry\""), "{serialized}");
        assert!(!serialized.contains("abcdefghijklmnop"), "{serialized}");
    }

    #[test]
    fn request_headers_are_not_projected() {
        let header = header();
        let filter = filter();
        let mut projector = TraceProjector::new("trace-session", &header, &filter);
        projector.push(envelope(
            1,
            SessionEvent::RequestHeader {
                turn: 1,
                step: 1,
                header: RequestHeaderSnapshot {
                    config: LlmCallConfig {
                        provider: "p".into(),
                        model: "m".into(),
                        reasoning_effort: None,
                        temperature: None,
                        max_tokens: None,
                        stop: Vec::new(),
                    },
                    system: Some("SECRET SYSTEM PROMPT".into()),
                    tools: Vec::new(),
                },
                reason: RequestHeaderReason::Initial,
                starts_series: false,
            },
        ));
        let output = projector.finish();
        assert_eq!(output.scanned, 1);
        assert!(output.records.is_empty());
        assert!(
            !serde_json::to_string(&output)
                .unwrap()
                .contains("SECRET SYSTEM PROMPT")
        );
    }

    #[test]
    fn auxiliary_records_are_sanitized_before_persisting() {
        let mut record = TraceRecord::now("s1", "queue");
        record.text = Some("api_key=supersecret123".into());
        record.data = Some(json!({"nested": ["Bearer abcdefghijklmnop"]}));
        let safe = record.sanitized();
        assert!(!safe.text.unwrap().contains("supersecret123"));
        assert!(!safe.data.unwrap().to_string().contains("abcdefghijklmnop"));
    }

    #[test]
    fn relation_allows_self_parent_and_child_only() {
        let parents = HashMap::from([
            ("child".to_string(), "main".to_string()),
            ("other".to_string(), "elsewhere".to_string()),
        ]);
        assert!(related("main", "main", &parents));
        assert!(related("main", "child", &parents));
        assert!(related("child", "main", &parents));
        assert!(!related("main", "other", &parents));
        assert!(!related("child", "other", &parents));
    }
}
