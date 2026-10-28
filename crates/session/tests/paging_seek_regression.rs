//! Compatibility oracle and reproducible long-log benchmark for seek-based paging.
//!
//! Run the benchmark with:
//! cargo test -p denia-session --release --test paging_seek_regression paging_seek_long_log_benchmark -- --ignored --nocapture

use denia_core::session::{
    SESSION_FORMAT_VERSION, SessionEnvelope, SessionEvent, SessionHeader, SessionHeaderKind,
    TurnEndReason,
};
use denia_core::stream::{ContentBlock, StreamChunk, TokenUsage};
use denia_session::{SessionAnchor, SessionError, SessionPage, SessionStore, SessionTotals};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Fixture {
    home: PathBuf,
    file: PathBuf,
    header: SessionHeader,
}

impl Fixture {
    fn new() -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let home = std::env::temp_dir().join(format!("denia-paging-seek-{id}"));
        let dir = home.join("sessions").join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("session.jsonl");
        let header = SessionHeader {
            kind: SessionHeaderKind::Session,
            version: SESSION_FORMAT_VERSION,
            id,
            created_at: 123,
            cwd: home.to_string_lossy().into_owned(),
            sandbox: true,
            parent_session: None,
            subagent: None,
        };
        Self { home, file, header }
    }

    fn log(&self, lines: &[String], separator: &str) -> String {
        let mut log = serde_json::to_string(&self.header).unwrap();
        log.push_str(separator);
        log.push_str(&lines.join(separator));
        log
    }

    fn store(&self) -> SessionStore {
        SessionStore::open(&self.home).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn line(seq: u64, event: SessionEvent) -> String {
    serde_json::to_string(&SessionEnvelope {
        seq,
        time: seq * 7,
        event,
    })
    .unwrap()
}

fn user(text: impl Into<String>, injected: bool) -> SessionEvent {
    SessionEvent::UserMessage {
        text: text.into(),
        injected,
        channel: None,
        images: Vec::new(),
    }
}

fn assistant(turn: u32) -> SessionEvent {
    SessionEvent::AssistantMessage {
        turn,
        step: 1,
        blocks: vec![ContentBlock::Text {
            text: "settled response".into(),
        }],
        usage: Some(TokenUsage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: Some(30),
            reasoning_tokens: Some(4),
        }),
        interrupted: false,
        source_event_seqs: Vec::new(),
        first_token_time: Some(10),
    }
}

fn transient(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::AssistantChunk { .. } | SessionEvent::ToolOutputChunk { .. }
    )
}

fn preview(text: &str) -> String {
    let collapsed: String = text
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let trimmed = collapsed.trim();
    let mut result: String = trimmed.chars().take(240).collect();
    if trimmed.chars().count() > 240 {
        result.push('…');
    }
    result
}

#[derive(Default)]
struct ReadCounts {
    bytes: u64,
    decodes: u64,
}

// Intentionally retains the pre-seek algorithm, including next_log_event's
// physical-first-line header rule and termination on the first serde failure.
fn legacy_next(
    reader: &mut BufReader<File>,
    header: &mut Option<SessionHeader>,
    line: &mut String,
    line_no: &mut usize,
    counts: &mut ReadCounts,
) -> Result<Option<SessionEnvelope>, SessionError> {
    loop {
        line.clear();
        let read = reader.read_line(line)?;
        counts.bytes += read as u64;
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
        let envelope = serde_json::from_str(trimmed).ok();
        counts.decodes += u64::from(envelope.is_some());
        return Ok(envelope);
    }
}

fn legacy_page(
    file: &Path,
    before: Option<u64>,
    limit: usize,
) -> Result<(SessionPage, ReadCounts), SessionError> {
    let limit = limit.clamp(1, 1000);
    let eligible = |seq| before.is_none_or(|cut| seq < cut);
    let mut counts = ReadCounts::default();
    let mut reader = BufReader::new(File::open(file)?);
    let mut header = None;
    let mut line = String::new();
    let mut line_no = 0;
    let mut total = 0;
    let mut anchors = Vec::new();
    let mut anchor_at = Vec::new();
    let mut totals = SessionTotals::default();
    let mut open_turn = None;
    while let Some(envelope) = legacy_next(
        &mut reader,
        &mut header,
        &mut line,
        &mut line_no,
        &mut counts,
    )? {
        totals.fold(&envelope, &mut open_turn);
        if transient(&envelope.event) {
            continue;
        }
        if let SessionEvent::UserMessage {
            text,
            injected: false,
            ..
        } = &envelope.event
        {
            anchors.push(SessionAnchor {
                seq: envelope.seq,
                text: preview(text),
            });
            if eligible(envelope.seq) {
                anchor_at.push((total, envelope.seq));
            }
        }
        if eligible(envelope.seq) {
            total += 1;
        }
    }
    let header = header.ok_or_else(|| SessionError::Corrupt("missing header".into()))?;
    let boundary = total.saturating_sub(limit as u64);
    let start_index = match anchor_at.binary_search_by(|(index, _)| index.cmp(&boundary)) {
        Ok(pos) => anchor_at[pos].0,
        Err(0) => boundary,
        Err(insert) => anchor_at[insert - 1].0,
    };
    let mut reader = BufReader::new(File::open(file)?);
    let mut header2 = None;
    let mut line_no = 0;
    let mut events = Vec::new();
    let mut index = 0;
    while let Some(envelope) = legacy_next(
        &mut reader,
        &mut header2,
        &mut line,
        &mut line_no,
        &mut counts,
    )? {
        if transient(&envelope.event) || !eligible(envelope.seq) {
            continue;
        }
        if index >= start_index {
            events.push(envelope);
        }
        index += 1;
    }
    Ok((
        SessionPage {
            header,
            events,
            total,
            has_more_before: start_index > 0,
            anchors,
            totals,
        },
        counts,
    ))
}

fn assert_same_page(actual: &SessionPage, expected: &SessionPage) {
    assert!(actual.header == expected.header, "header differs");
    assert!(actual.events == expected.events, "page events differ");
    assert_eq!(actual.total, expected.total);
    assert_eq!(actual.has_more_before, expected.has_more_before);
    assert_eq!(actual.totals, expected.totals);
    assert_eq!(actual.anchors.len(), expected.anchors.len());
    for (actual, expected) in actual.anchors.iter().zip(&expected.anchors) {
        assert_eq!(actual.seq, expected.seq);
        assert!(actual.text == expected.text, "anchor preview differs");
    }
}

fn compare_queries(fixture: &Fixture, cuts: &[Option<u64>], limits: &[usize]) {
    let original = std::fs::read(&fixture.file).unwrap();
    let store = fixture.store();
    for &before in cuts {
        for &limit in limits {
            let expected = legacy_page(&fixture.file, before, limit);
            let actual = store.read_page(&fixture.header.id, before, limit);
            match (actual, expected) {
                (Ok(actual), Ok((expected, _))) => assert_same_page(&actual, &expected),
                (Err(actual), Err(expected)) => {
                    assert_eq!(actual.to_string(), expected.to_string());
                }
                _ => panic!("legacy/seek result differs: before={before:?}, limit={limit}"),
            }
        }
    }
    assert!(std::fs::read(&fixture.file).unwrap() == original);
}

#[test]
fn paging_seek_matches_legacy_for_turn_boundaries_chunks_and_global_totals() {
    let fixture = Fixture::new();
    let mut events = vec![
        SessionEvent::SystemPrompt {
            turn: 0,
            step: 0,
            text: "system\n\"type\":\"user-message\"".repeat(512),
        },
        user("injected preface", true),
    ];
    for turn in 1..=8 {
        events.push(user(format!(" \n问题 {turn} {}\r\n", "界🙂".repeat(160)), false));
        events.push(SessionEvent::TurnStart { turn });
        for index in 0..turn * 3 {
            events.push(SessionEvent::AssistantChunk {
                turn,
                step: 1,
                chunk: StreamChunk::TextDelta {
                    index,
                    text: "delta\n\"type\":\"tool-call\"".repeat(20),
                },
            });
            events.push(SessionEvent::ToolOutputChunk {
                call_id: format!("call-{turn}"),
                stream: "stdout".into(),
                text: "output".repeat(40),
            });
        }
        for tool in 0..turn {
            events.push(SessionEvent::ToolCall {
                turn,
                step: 1,
                call_id: format!("call-{turn}-{tool}"),
                name: "read_file".into(),
                arguments: "{}".into(),
            });
            events.push(SessionEvent::ToolResult {
                turn,
                step: 1,
                call_id: format!("call-{turn}-{tool}"),
                content: "old tool output".repeat(50),
                is_error: false,
                error: None,
                error_identity: None,
                meta: Some(serde_json::json!({"nested": [1, "value", null]})),
                truncation: None,
                replaces: None,
            });
        }
        events.push(assistant(turn));
        events.push(SessionEvent::TurnEnd {
            turn,
            reason: TurnEndReason::Completed,
        });
    }
    let lines: Vec<_> = events
        .into_iter()
        .enumerate()
        .map(|(index, event)| line(index as u64 + 1, event))
        .collect();
    std::fs::write(&fixture.file, fixture.log(&lines, "\r\n \t\r\n")).unwrap();
    compare_queries(
        &fixture,
        &[None, Some(0), Some(2), Some(35), Some(200), Some(u64::MAX)],
        &[0, 1, 4, 13, 1000, usize::MAX],
    );
    let page = fixture.store().read_page(&fixture.header.id, Some(35), 1).unwrap();
    assert_eq!(page.anchors.len(), 8);
    assert_eq!(page.totals.turns, 8);
    assert_eq!(page.totals.steps, 8);
    assert_eq!(page.totals.tool_calls, 36);
    assert_eq!(page.totals.input_tokens, 800);
    assert_eq!(page.totals.output_tokens, 160);
    assert_eq!(page.totals.cache_read_tokens, 240);
    assert_eq!(page.totals.reasoning_tokens, 32);
}

#[test]
fn paging_seek_matches_legacy_without_anchors_and_with_nonmonotonic_sequences() {
    let fixture = Fixture::new();
    let lines: Vec<_> = (0..1207)
        .map(|index| line(index + 1, user(format!("injected {index}"), true)))
        .collect();
    std::fs::write(&fixture.file, fixture.log(&lines, "\n")).unwrap();
    compare_queries(
        &fixture,
        &[None, Some(0), Some(3), Some(1003)],
        &[0, 1, 7, 1000, usize::MAX],
    );
    let lines = vec![
        line(40, user("not eligible for before=30", false)),
        line(10, SessionEvent::TurnStart { turn: 1 }),
        line(50, user("another ineligible anchor", false)),
        line(20, user("eligible anchor", false)),
        line(60, assistant(1)),
        line(15, user("injected but eligible", true)),
        line(10, user("duplicate eligible seq", false)),
        line(5, SessionEvent::TurnEnd { turn: 1, reason: TurnEndReason::Completed }),
    ];
    std::fs::write(&fixture.file, fixture.log(&lines, "\n")).unwrap();
    compare_queries(&fixture, &[None, Some(10), Some(30), Some(51)], &[0, 1, 3, 50]);
}

#[test]
fn paging_seek_stops_at_unknown_or_malformed_events_without_repairing_the_log() {
    let fixture = Fixture::new();
    let prefix = vec![
        line(1, user("valid prefix", false)),
        line(2, SessionEvent::TurnStart { turn: 1 }),
        line(3, assistant(1)),
    ];
    let invalid = [
        r#"{"seq":4,"time":28,"type":"retired-event","text":"unknown"}"#,
        r#"{"seq":4,"time":28,"type":"turn-end","turn":1,"reason": "#,
        r#"{"seq":4,"time":28,"type":"turn-start","turn":"bad"}"#,
        r#"{"seq":4,"time":28,"type":"assistant-message","turn":1,"step":1,"blocks":[{"type":"text","text":2}]}"#,
        r#"{"seq":4,"time":28,"type":"assistant-message","turn":1,"step":1,"blocks":[],"usage":{"inputTokens":-1,"outputTokens":0}}"#,
        r#"{"seq":4,"time":28,"type":"assistant-chunk","turn":1,"step":1,"chunk":{"type":"unknown-chunk"}}"#,
        r#"{"seq":4,"time":28,"type":"tool-output-chunk","call_id":"c","text":"missing stream"}"#,
        r#"{"seq":"bad","time":28,"type":"user-message","text":"bad seq"}"#,
        r#"{"seq":4,"time":28,"type":"user-message","text":"bad image","images":[{}]}"#,
        r#"{"seq":4,"time":28,"type":"tool-result","turn":1,"step":1,"call_id":"c","content":"bad truncation","is_error":false,"truncation":{"total_chars":true}}"#,
    ];
    for invalid in invalid {
        let mut lines = prefix.clone();
        lines.push(invalid.into());
        lines.push(line(5, user("must never be stitched onto the prefix", false)));
        lines.push(line(6, assistant(2)));
        lines.push(line(7, SessionEvent::TurnEnd { turn: 1, reason: TurnEndReason::Completed }));
        let log = fixture.log(&lines, "\n");
        std::fs::write(&fixture.file, &log).unwrap();
        compare_queries(&fixture, &[None, Some(2), Some(6)], &[0, 1, 1000]);
        let page = fixture.store().read_page(&fixture.header.id, None, 1).unwrap();
        assert_eq!(page.total, 3);
        assert_eq!(page.events.len(), 3);
        assert_eq!(page.anchors.len(), 1);
        assert_eq!(page.totals.turns, 0);
        assert_eq!(page.totals.steps, 1);
        assert!(std::fs::read_to_string(&fixture.file).unwrap() == log);
    }
}

#[test]
fn paging_seek_preserves_header_empty_line_and_unterminated_final_line_behavior() {
    let fixture = Fixture::new();
    let valid = fixture.log(&[line(1, user("valid without final newline", false))], "\n");
    let cases = [
        String::new(),
        "\n\n".into(),
        "{broken header\n".into(),
        serde_json::to_string(&fixture.header).unwrap(),
        format!("\n{valid}"),
        valid.clone(),
        format!("{valid}\n\n\t \r\n"),
    ];
    for log in cases {
        std::fs::write(&fixture.file, log).unwrap();
        compare_queries(&fixture, &[None, Some(0), Some(2)], &[0, 1, 1000]);
    }
}

struct SyntheticLog {
    bytes: u64,
    offsets: Vec<u64>,
}

fn write_synthetic(fixture: &Fixture, turns: u32, chunks: u32) -> SyntheticLog {
    let mut writer = BufWriter::new(File::create(&fixture.file).unwrap());
    let header = serde_json::to_vec(&fixture.header).unwrap();
    writer.write_all(&header).unwrap();
    writer.write_all(b"\n").unwrap();
    let mut bytes = header.len() as u64 + 1;
    let mut offsets = Vec::new();
    let mut push = |event| {
        offsets.push(bytes);
        let line = line(offsets.len() as u64, event);
        writer.write_all(line.as_bytes()).unwrap();
        writer.write_all(b"\n").unwrap();
        bytes += line.len() as u64 + 1;
    };
    let chunk = "synthetic delta ".repeat(128);
    let prompt = "synthetic system prompt ".repeat(2731);
    let output = "synthetic old tool output ".repeat(1311);
    for turn in 1..=turns {
        push(user(format!("synthetic prompt {turn}"), false));
        push(SessionEvent::TurnStart { turn });
        push(SessionEvent::SystemPrompt { turn, step: 1, text: prompt.clone() });
        push(SessionEvent::ToolCall {
            turn,
            step: 1,
            call_id: format!("call-{turn}"),
            name: "read_file".into(),
            arguments: "{}".into(),
        });
        for index in 0..chunks {
            if index % 2 == 0 {
                push(SessionEvent::AssistantChunk {
                    turn,
                    step: 1,
                    chunk: StreamChunk::TextDelta { index, text: chunk.clone() },
                });
            } else {
                push(SessionEvent::ToolOutputChunk {
                    call_id: format!("call-{turn}"),
                    stream: "stdout".into(),
                    text: chunk.clone(),
                });
            }
        }
        push(SessionEvent::ToolResult {
            turn,
            step: 1,
            call_id: format!("call-{turn}"),
            content: output.clone(),
            is_error: false,
            error: None,
            error_identity: None,
            meta: None,
            truncation: None,
            replaces: None,
        });
        push(assistant(turn));
        push(SessionEvent::TurnEnd { turn, reason: TurnEndReason::Completed });
    }
    writer.flush().unwrap();
    SyntheticLog { bytes, offsets }
}

fn median(samples: &mut [Duration]) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[test]
#[ignore = "reproducible ~150 MiB paging benchmark; run explicitly in release mode"]
fn paging_seek_long_log_benchmark() {
    const TURNS: u32 = 512;
    const CHUNKS: u32 = 96;
    const REPEATS: usize = 3;
    let fixture = Fixture::new();
    let synthetic = write_synthetic(&fixture, TURNS, CHUNKS);
    let store = fixture.store();
    let events_per_turn = u64::from(CHUNKS) + 7;
    println!(
        "paging_seek fixture_bytes={} log_events={} turns={} chunks_per_turn={} repeats={}",
        synthetic.bytes, synthetic.offsets.len(), TURNS, CHUNKS, REPEATS,
    );
    let queries = [
        ("tail", None),
        ("middle", Some(events_per_turn * u64::from(TURNS / 2) + 1)),
        ("early", Some(events_per_turn * u64::from(TURNS / 8) + 1)),
    ];
    for (name, before) in queries {
        let expected = legacy_page(&fixture.file, before, 120).unwrap();
        let actual = store.read_page(&fixture.header.id, before, 120).unwrap();
        assert_same_page(&actual, &expected.0);
        assert_eq!(actual.anchors.len(), TURNS as usize);
        assert_eq!(actual.totals.turns, u64::from(TURNS));
        let first = actual.events.first().unwrap().seq;
        let last = actual.events.last().unwrap().seq;
        let window_start = synthetic.offsets[first as usize - 1];
        let window_end = synthetic.offsets.get(last as usize).copied().unwrap_or(synthetic.bytes);
        let window_bytes = window_end - window_start;
        let mut legacy_times = Vec::new();
        let mut seek_times = Vec::new();
        for _ in 0..REPEATS {
            let started = Instant::now();
            let (expected, _) = legacy_page(&fixture.file, before, 120).unwrap();
            legacy_times.push(started.elapsed());
            let started = Instant::now();
            let actual = store.read_page(&fixture.header.id, before, 120).unwrap();
            seek_times.push(started.elapsed());
            assert_same_page(&actual, &expected);
        }
        let legacy_time = median(&mut legacy_times);
        let seek_time = median(&mut seek_times);
        println!(
            "paging_seek query={name} page_events={} eligible_total={} anchors={} legacy_ms={:.3} seek_ms={:.3} speedup={:.2} legacy_logical_bytes={} seek_logical_bytes={} legacy_full_decodes={} seek_full_decodes={} window_bytes={window_bytes}",
            actual.events.len(), actual.total, actual.anchors.len(),
            legacy_time.as_secs_f64() * 1000.0, seek_time.as_secs_f64() * 1000.0,
            legacy_time.as_secs_f64() / seek_time.as_secs_f64(),
            expected.1.bytes, synthetic.bytes + window_bytes,
            expected.1.decodes, synthetic.offsets.len() as u64 + last - first + 1,
        );
    }
    assert_eq!(std::fs::metadata(&fixture.file).unwrap().len(), synthetic.bytes);
}
