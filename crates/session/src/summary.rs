//! Session summary/index types: list rows, page totals and anchors, plus
//! the streaming file-summary reader that feeds the in-memory index.

use super::log_reader::{FileStamp, file_stamp};
use super::{Session, SessionEnvelope, SessionEvent, SessionHeader};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// 摘要重读上限:防止异常的超长行拖垮列表。
pub(crate) const SUMMARY_SCAN_LIMIT: usize = 64 * 1024;

/// One row of the session list.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub created_at: u64,
    pub excerpt: Option<String>,
    /// AI 生成的会话标题(第一轮用户消息后后台产出);展示优先于 excerpt。
    pub title: Option<String>,
    pub cwd: Option<String>,
    pub sandbox: Option<bool>,
    /// Whether the recorded working directory still exists; sessions with a
    /// dead cwd refuse new prompts.
    pub cwd_alive: bool,
    /// 分支血缘:父会话 id(非分支会话为 `None`)。侧栏据此把子会话
    /// 嵌套在源会话之下。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<denia_core::session::SubagentDescriptor>,
    /// 会话运行的 agent preset(创建时写入日志);`None` = 旧会话或未指定。
    /// 新会话页与侧栏据此显示预设,不必为一行摘要加载整个会话。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_preset: Option<String>,
}

/// 轮次轴锚点:一条非注入 user-message 的定位与预览文本。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionAnchor {
    pub seq: u64,
    pub text: String,
}

/// 锚点预览截断长度:轮次轴悬浮预览够用即可,不随正文长度膨胀响应。
pub(crate) const ANCHOR_PREVIEW_CHARS: usize = 240;

pub(crate) fn anchor_preview(text: &str) -> String {
    let collapsed: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = collapsed.trim();
    if trimmed.chars().count() <= ANCHOR_PREVIEW_CHARS {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(ANCHOR_PREVIEW_CHARS).collect();
    format!("{cut}…")
}

/// 全会话累计统计:整份日志折叠出的展示总量,与分页窗口无关。
///
/// 口径与前端 `deriveStats`(`web/src/stats.ts`)逐项对齐 —— 轮次/耗时看
/// `turn-start`→`turn-end` 配对,步数与 token 看结算的 `assistant-message`,
/// 工具次数看 `tool-call`。带上它是因为分页只给窗口:前端若自己从窗口折叠,
/// 刷新、翻页、长会话的实时裁剪都会让状态栏的累计数字缩水。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTotals {
    /// 已闭合轮次的墙钟时长之和(毫秒)。
    pub turn_ms: u64,
    pub turns: u64,
    pub steps: u64,
    pub tool_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub reasoning_tokens: u64,
}

impl SessionTotals {
    /// 折叠一条日志事件。`open_turn` 是跨事件保持的配对状态(未闭合轮次的
    /// turn 号与起点时刻),由调用方持有。
    pub fn fold(&mut self, envelope: &SessionEnvelope, open_turn: &mut Option<(u32, u64)>) {
        match &envelope.event {
            SessionEvent::TurnStart { turn } => *open_turn = Some((*turn, envelope.time)),
            SessionEvent::TurnEnd { turn, .. } => {
                // 配不上起点(截断遗留)只丢这一轮的时长,不编造。
                if let Some((started, at)) = *open_turn
                    && started == *turn
                {
                    self.turns += 1;
                    self.turn_ms += envelope.time.saturating_sub(at);
                }
                *open_turn = None;
            }
            SessionEvent::AssistantMessage { usage, .. } => {
                self.steps += 1;
                if let Some(usage) = usage {
                    self.input_tokens += usage.input_tokens;
                    self.output_tokens += usage.output_tokens;
                    self.cache_read_tokens += usage.cache_read_tokens.unwrap_or(0);
                    self.reasoning_tokens += usage.reasoning_tokens.unwrap_or(0);
                }
            }
            SessionEvent::ToolCall { .. } => self.tool_calls += 1,
            _ => {}
        }
    }
}

/// 一次分页读取的会话事件窗口:只保留 `limit` 条,后端不驻留全量历史。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionPage {
    pub header: SessionHeader,
    pub events: Vec<SessionEnvelope>,
    pub total: u64,
    #[serde(rename = "hasMoreBefore")]
    pub has_more_before: bool,
    /// 全会话非注入 user-message 锚点(与 `before` 无关,恒为全量)。
    pub anchors: Vec<SessionAnchor>,
    /// 全会话累计统计(与 `before`/`limit` 无关,恒为全量)。
    pub totals: SessionTotals,
}

/// 索引条目:启动/失效时从文件摘要得到,list 只读它。
#[derive(Debug, Clone)]
pub(crate) struct IndexEntry {
    pub(crate) meta: SessionMeta,
    pub(crate) stamp: FileStamp,
    /// 该条目何时进入内存索引;列表节流后用于对“刚创建/刚加载”的会话
    /// 仍做一次 stat,避免创建后立刻刷新列表时摘要滞后。
    pub(crate) indexed_at: u64,
}

#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub created_at: u64,
    pub excerpt: Option<String>,
    pub title: Option<String>,
    pub cwd: String,
    pub sandbox: bool,
    pub parent_session: Option<String>,
    pub subagent: Option<denia_core::session::SubagentDescriptor>,
    pub agent_preset: Option<String>,
}

pub(crate) fn meta_of(session: &Session) -> SessionMeta {
    SessionMeta {
        id: session.id().to_string(),
        created_at: session.header().created_at,
        excerpt: session
            .header()
            .subagent
            .as_ref()
            .map(|s| s.label.clone())
            .or_else(|| session.first_prompt_excerpt(80)),
        title: session.title(),
        cwd: session.header().cwd.clone(),
        sandbox: session.header().sandbox,
        parent_session: session.header().parent_session.clone(),
        subagent: session.header().subagent.clone(),
        agent_preset: session.agent_preset(),
    }
}

pub(crate) fn summary_of(session: &Session) -> SessionSummary {
    summary_of_meta(&meta_of(session))
}

pub(crate) fn summary_of_meta(meta: &SessionMeta) -> SessionSummary {
    SessionSummary {
        id: meta.id.clone(),
        created_at: meta.created_at,
        excerpt: meta.excerpt.clone(),
        title: meta.title.clone(),
        cwd: Some(meta.cwd.clone()),
        sandbox: Some(meta.sandbox),
        cwd_alive: Path::new(&meta.cwd).is_dir(),
        parent_session: meta.parent_session.clone(),
        subagent: meta.subagent.clone(),
        agent_preset: meta.agent_preset.clone(),
    }
}

/// 摘要读取:header + 首条用户消息 + 标题事件(流式逐行,两个目标都命中
/// 即停,上限 [`SUMMARY_SCAN_LIMIT`]),跳过损坏行。标题事件落在第一轮
/// 闭合之后,所以不能在首条用户消息处提前停止。
pub(crate) fn read_summary(file: &Path) -> Option<(SessionMeta, FileStamp)> {
    let stamp = file_stamp(file)?;
    let mut reader = BufReader::new(File::open(file).ok()?);
    let mut bytes_read = 0usize;
    let mut first_line: Option<String> = None;
    let mut excerpt: Option<String> = None;
    let mut title: Option<String> = None;
    let mut agent_preset: Option<String> = None;
    loop {
        if bytes_read >= SUMMARY_SCAN_LIMIT {
            break;
        }
        let mut line = String::new();
        let read = reader.read_line(&mut line).ok()?;
        if read == 0 {
            break;
        }
        bytes_read += read;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if first_line.is_none() {
            first_line = Some(trimmed.to_string());
            continue;
        }
        let envelope: SessionEnvelope = match serde_json::from_str(trimmed) {
            Ok(envelope) => envelope,
            Err(_) => continue,
        };
        match envelope.event {
            SessionEvent::UserMessage {
                text,
                injected: false,
                ..
            } => {
                excerpt = Some(excerpt_text(&text, 80));
                if title.is_some() {
                    break;
                }
            }
            SessionEvent::SessionTitle { title: t } => {
                title = Some(t);
                if excerpt.is_some() {
                    break;
                }
            }
            // 预设事件落在会话创建处(远在摘要扫描目标之前);latest-wins,
            // 但这里只做"是否已扫到"的短路,继续扫其余目标。
            SessionEvent::AgentPreset { preset } => {
                agent_preset = Some(preset);
            }
            _ => {}
        }
    }
    let header: SessionHeader = serde_json::from_str(first_line.as_deref()?).ok()?;
    let id = file.parent()?.file_name()?.to_string_lossy().to_string();
    Some((
        SessionMeta {
            id,
            created_at: header.created_at,
            excerpt: header
                .subagent
                .as_ref()
                .map(|s| s.label.clone())
                .or(excerpt),
            title,
            cwd: header.cwd,
            sandbox: header.sandbox,
            parent_session: header.parent_session,
            subagent: header.subagent,
            agent_preset,
        },
        stamp,
    ))
}

pub(crate) fn excerpt_text(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    let mut chars = trimmed.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}
