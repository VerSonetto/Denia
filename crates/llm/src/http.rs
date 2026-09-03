//! Shared HTTP error mapping for chat-completions endpoints.

use denia_core::error::{LlmFailure, codes};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use reqwest::StatusCode;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct WireErrorBody {
    #[serde(default)]
    error: Option<WireErrorDetail>,
}

#[derive(Debug, Deserialize)]
struct WireErrorDetail {
    #[serde(default)]
    message: Option<String>,
}

const MESSAGE_CAP: usize = 500;

/// Maps one failed HTTP response onto the failure taxonomy.
pub fn http_error_failure(status: StatusCode, body: &str, headers: &HeaderMap) -> LlmFailure {
    let message = error_message(body);
    let code = match status.as_u16() {
        401 | 403 => codes::AUTH,
        429 => {
            if message.to_lowercase().contains("quota") {
                codes::QUOTA
            } else {
                codes::RATE_LIMIT
            }
        }
        400 => {
            let lower = message.to_lowercase();
            if lower.contains("context") || lower.contains("length") || lower.contains("tokens") {
                codes::CONTEXT_WINDOW_EXCEEDED
            } else {
                codes::INVALID_REQUEST
            }
        }
        408 => codes::TIMEOUT,
        _ if status.is_server_error() => codes::SERVER,
        _ => codes::INVALID_REQUEST,
    };
    let mut failure = LlmFailure::new(code, message).with_status(status.as_u16());
    if let Some(ms) = retry_after_ms(headers) {
        failure = failure.with_retry_after_ms(ms);
    }
    if let Some(id) = headers
        .get("x-request-id")
        .or_else(|| headers.get("x-deepseek-request-id"))
        .and_then(|v| v.to_str().ok())
    {
        failure = failure.with_request_id(id);
    }
    failure
}

fn error_message(body: &str) -> String {
    let message = serde_json::from_str::<WireErrorBody>(body)
        .ok()
        .and_then(|parsed| parsed.error)
        .and_then(|detail| detail.message)
        .unwrap_or_else(|| {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                "the provider returned no error body".to_string()
            } else {
                trimmed.to_string()
            }
        });
    let mut chars: Vec<char> = message.chars().take(MESSAGE_CAP).collect();
    if message.chars().count() > MESSAGE_CAP {
        chars.push('…');
    }
    chars.into_iter().collect()
}

/// Parses `Retry-After` as delta seconds or an HTTP date(对齐 dsh:
/// 秒数与 IMF-fixdate 都支持;其他 HTTP-date 变体(generic date 等)
/// 实际不会由网关产生,解析失败即 None)。
fn retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if let Some(seconds) = value.parse::<u64>().ok() {
        return Some(seconds.saturating_mul(1000));
    }
    let then = parse_imf_fixdate(value)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    then.checked_sub(now).map(|secs| secs.saturating_mul(1000))
}

/// 解析 `Sun, 06 Nov 1994 08:49:37 GMT` 形态的 IMF-fixdate 为 unix 秒。
fn parse_imf_fixdate(value: &str) -> Option<u64> {
    // 固定布局:ddd, DD Mon YYYY HH:MM:SS GMT(29 字符)。
    let b = value.as_bytes();
    if b.len() != 29 || !b[25..29].eq_ignore_ascii_case(b"GMT") || b[3] != b',' || b[4] != b' '
        || b[7] != b' ' || b[11] != b' ' || b[16] != b' ' || b[19] != b':' || b[22] != b':'
    {
        return None;
    }
    let day = value[5..7].parse::<u32>().ok()?;
    let month = match &value[8..11] {
        "Jan" => 1u32, "Feb" => 2, "Mar" => 3, "Apr" => 4, "May" => 5, "Jun" => 6,
        "Jul" => 7, "Aug" => 8, "Sep" => 9, "Oct" => 10, "Nov" => 11, "Dec" => 12,
        _ => return None,
    };
    let year = value[12..16].parse::<i64>().ok()?;
    let hour = value[17..19].parse::<u32>().ok()?;
    let min = value[20..22].parse::<u32>().ok()?;
    let sec = value[23..25].parse::<u32>().ok()?;
    if day < 1 || day > 31 || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    // 用 days-since-epoch 的民用历法换算(公历,公元 1970 起,忽略闰秒)。
    fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) as i64 / 5 + d as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }
    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour as i64 * 3_600 + min as i64 * 60 + sec as i64) as u64)
}

/// Maps a reqwest send/transport error onto the taxonomy.
pub fn transport_failure(error: reqwest::Error) -> LlmFailure {
    if error.is_timeout() {
        LlmFailure::new(codes::TIMEOUT, format!("request timed out: {error}"))
    } else {
        LlmFailure::new(codes::TRANSPORT, format!("transport error: {error}"))
    }
}

/// The [`transport_failure`] snapshot wrapped as the live error form.
pub fn transport_error(error: reqwest::Error) -> denia_core::error::LlmError {
    denia_core::error::LlmError::from_failure(transport_failure(error))
}
