//! Low-level log-file plumbing: stat stamps, transient-event filtering,
//! usage-only envelope projection and the shared line reader.

use super::log_writer::LogWriter;
use super::{SessionEnvelope, SessionError, SessionEvent, SessionHeader};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 文件尺寸/修改时间戳:索引失效校验的签名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileStamp {
    pub(crate) size: u64,
    pub(crate) mtime_ms: u64,
}

pub(crate) fn file_stamp(path: &Path) -> Option<FileStamp> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(FileStamp {
        size: meta.len(),
        mtime_ms: mtime,
    })
}

pub(crate) fn open_append_writer(file: &Path) -> Result<LogWriter, SessionError> {
    Ok(LogWriter::open(file)?)
}

/// 临时流事件:高频、体量大、只服务实时流式回放。
///
/// 内存事件表**不驻留已闭合轮次的临时事件** —— 派生面(token-meter 与
/// `derive_surface`)只折叠 user/assistant/tool-result,闭环轮次的历史
/// 重建只依赖结算的 `assistant-message`(与分页端点的展示粒度同口径),
/// 临时事件只在当前未结算的步骤里驻留,消息结算或 `turn-end` 时清扫。日志
/// 文件仍完整记录每一条(append-only 不变)。
///
/// 判定只有这一处:凡是"只服务实时观感、不进模型历史"的高频事件都登记在
/// 这里,分页(`store::read_page`)与加载(`recovery`)都按它过滤 —— 新增
/// 这类事件时不必再去各处补 `matches!`。
pub(crate) fn is_transient_event(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::AssistantChunk { .. } | SessionEvent::ToolOutputChunk { .. }
    )
}

pub(crate) fn usage_envelope(envelope: &SessionEnvelope) -> Option<SessionEnvelope> {
    let event = match &envelope.event {
        SessionEvent::TurnStart { .. }
        | SessionEvent::TurnEnd { .. }
        | SessionEvent::StepStart { .. }
        | SessionEvent::StepEnd { .. } => envelope.event.clone(),
        SessionEvent::AssistantMessage {
            turn, step, usage, ..
        } => SessionEvent::AssistantMessage {
            turn: *turn,
            step: *step,
            blocks: Vec::new(),
            usage: *usage,
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        },
        _ => return None,
    };
    Some(SessionEnvelope {
        seq: envelope.seq,
        time: envelope.time,
        event,
    })
}

/// 逐行读下一条可解析的日志事件:首个非空行产出会话头(存入 `header`,
/// 不作为事件返回),空行跳过,损坏行跳过(torn-tail 容忍),文件读尽返回
/// `None`。分页的两遍扫描共用,保证对同一文件产出一致的事件序列。
pub(crate) fn next_log_event(
    reader: &mut BufReader<File>,
    header: &mut Option<SessionHeader>,
    line: &mut String,
    line_no: &mut usize,
) -> Result<Option<SessionEnvelope>, SessionError> {
    loop {
        line.clear();
        let read = reader.read_line(line)?;
        if read == 0 {
            return Ok(None);
        }
        *line_no += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if *line_no == 1 {
            *header = Some(
                serde_json::from_str(trimmed)
                    .map_err(|e| SessionError::Corrupt(format!("bad header: {e}")))?,
            );
            continue;
        }
        return Ok(serde_json::from_str(trimmed).ok());
    }
}
