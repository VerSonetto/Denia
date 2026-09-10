//! 模型请求失败的分类处置与报错摘要。
//!
//! 两层职责:
//! - **自纠注入**([`feedback_eligible`] / [`feedback_text`]):模型输出形态
//!   问题(被提供方拒绝)以 injected 用户消息回注,让模型自己纠正;
//! - **用户可读摘要**([`summarize_failure`]):turn 终止前把 `LlmFailure`
//!   的 message 重写为中文摘要——结论、已重试次数、分类建议、原始信息,
//!   前端 turn-chrome 直接展示,用户不用再猜 `[CODE] raw-payload`。

use denia_core::error::{codes, LlmFailure};

/// 反馈注入的适用范围(模型输出/请求形态问题,模型自纠有意义):
/// 提供方抖动(TRANSPORT/TIMEOUT/SERVER/RATE_LIMIT 等)与配置/凭据问题
/// (AUTH/MISSING_CREDENTIAL 等)不在此列——前者由退避重试处理,后者
/// 直接 error 终止(dsh 语义),都不浪费注入配额。
pub(crate) fn feedback_eligible(code: &str) -> bool {
    matches!(
        code,
        codes::INVALID_REQUEST
            | codes::MALFORMED_RESPONSE
            | codes::UNSUPPORTED_CONTENT
            | codes::UNSUPPORTED_REASONING_EFFORT
    )
}

/// 反馈注入的适用次数上限(每轮)。
pub(crate) const MAX_FEEDBACK: u32 = 2;

pub(crate) fn feedback_text(failure: &LlmFailure) -> String {
    // 归因必须分开:INVALID_REQUEST / UNSUPPORTED_* 是"请求形态"问题,让模型改自己
    // 的输出有意义;而响应流本身畸形/中断(多为网关)时再去指责"工具参数格式"是
    // 错误归因——实测模型照着改了三遍也没用,只是白烧轮次。
    let advice = match failure.code.as_str() {
        codes::MALFORMED_RESPONSE | codes::EMPTY_RESPONSE | codes::STREAM_CLOSED => {
            "这一条通常是上游响应流本身断掉或格式异常,不是你上一条输出的问题。请换一种表达重做上一步:简化工具调用的参数结构,或先用文字说明再发起调用,不要原样重复。"
        }
        _ => "请检查并纠正上一条输出(尤其是工具参数格式)后继续,不要原样重复。",
    };
    format!(
        "[harness] 上一次模型请求被提供方拒绝({code}:{message})。{advice}",
        code = failure.code,
        message = failure.message,
    )
}

/// 按 code 生成"结论 + 建议"的中文摘要;`retries` 是本 step 已自动重试的
/// 次数(0 = 没有重试就失败)。
fn classify(code: &str, retries: u32) -> (&'static str, &'static str) {
    match code {
        codes::AUTH | codes::INVALID_CREDENTIAL | codes::MISSING_CREDENTIAL => (
            "提供方认证失败。",
            "请检查模型设置中的 API 凭据是否正确、是否过期。",
        ),
        codes::QUOTA => (
            "账户配额或余额不足。",
            "请到提供方控制台确认额度,或更换模型/提供方。",
        ),
        codes::RATE_LIMIT => (
            "触发提供方限流。",
            if retries > 0 {
                "已自动退避重试仍未成功,请稍后重试,或降低请求频率。"
            } else {
                "请稍后重试,或降低请求频率。"
            },
        ),
        codes::SERVER => (
            "提供方服务端错误。",
            "已自动重试仍未恢复,请稍后重试;持续失败可更换模型验证是否为提供方故障。",
        ),
        codes::TIMEOUT => (
            "请求超时,提供方长时间未响应。",
            "请检查网络连接;问题持续时尝试更换提供方或模型。",
        ),
        codes::TRANSPORT => (
            "网络连接失败,无法到达提供方。",
            "请检查网络与代理设置;问题持续时尝试更换提供方。",
        ),
        codes::CONTEXT_WINDOW_EXCEEDED => (
            "请求超出模型上下文窗口。",
            "请手动压缩上下文(上下文面板),或精简会话内容后重试。",
        ),
        codes::EMPTY_RESPONSE | codes::STREAM_CLOSED | codes::MALFORMED_RESPONSE => (
            "提供方返回异常响应(空响应/流中断/格式错误)。",
            "多为网关或提供方抖动:可稍后重试;持续出现请更换提供方或模型。",
        ),
        codes::UNSUPPORTED_CONTENT => (
            "请求包含当前模型不支持的内容。",
            "请检查是否发送了该模型不支持的输入(如图片、文件);或更换支持的模型。",
        ),
        codes::UNSUPPORTED_REASONING_EFFORT => (
            "当前模型不支持所选推理强度。",
            "请在模型设置中调整推理强度或更换模型。",
        ),
        codes::INVALID_REQUEST => (
            "请求被提供方拒绝,自动纠错未能解决。",
            "可能与当前模型的能力或参数限制有关,请检查模型配置,或更换模型。",
        ),
        codes::NO_ADAPTER => (
            "该提供方没有可用的适配器。",
            "请检查提供方拼写与模型路由配置。",
        ),
        _ => (
            "发生未知错误。",
            "请查看原始信息定位问题;必要时重启开发实例或报告此错误。",
        ),
    }
}

/// turn 终止前把 failure 重写为用户可读的中文摘要:
/// `{结论}{已自动重试 N 次。}{建议}(原始信息:{provider message})`。
/// code/status/Retry-After 等结构化字段原样保留,只重写 message。
pub(crate) fn summarize_failure(failure: &LlmFailure, retries: u32) -> LlmFailure {
    let (conclusion, hint) = classify(&failure.code, retries);
    // 分类表里只有限流/服务端两条把"已自动重试"写进了建议文案(它们确属可重试码),
    // 其余一律由计数机制按事实补句——响应异常类不在可重试集,不能凭空声称重试过。
    let needs_count = !matches!(
        failure.code.as_str(),
        codes::RATE_LIMIT | codes::SERVER
    ) && retries > 0;
    let count_note = if needs_count {
        format!("已自动重试 {retries} 次。")
    } else {
        String::new()
    };
    let raw = failure.message.trim();
    let mut message = format!("{conclusion}{count_note}{hint}");
    if !raw.is_empty() {
        message.push_str(&format!("(原始信息:{})", truncate_raw(raw)));
    }
    let mut summarized = failure.clone();
    summarized.message = message;
    summarized
}

/// 原始信息过长时截断,摘要保持一行内可读。
fn truncate_raw(raw: &str) -> String {
    if raw.chars().count() <= 300 {
        return raw.to_string();
    }
    let head: String = raw.chars().take(300).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_failure_gets_credential_hint() {
        let failure = LlmFailure::new(codes::AUTH, "invalid api key");
        let summarized = summarize_failure(&failure, 0);
        assert_eq!(summarized.code, codes::AUTH);
        assert!(summarized.message.contains("认证失败"));
        assert!(summarized.message.contains("凭据"));
        assert!(summarized.message.contains("原始信息:invalid api key"));
        assert!(!summarized.message.contains("已自动重试"));
    }

    #[test]
    fn context_window_failure_points_to_compaction() {
        let failure = LlmFailure::new(codes::CONTEXT_WINDOW_EXCEEDED, "max tokens exceeded");
        let summarized = summarize_failure(&failure, 0);
        assert!(summarized.message.contains("上下文窗口"));
        assert!(summarized.message.contains("压缩"));
    }

    #[test]
    fn retried_failures_mention_the_count() {
        let failure = LlmFailure::new(codes::TIMEOUT, "request timed out");
        let summarized = summarize_failure(&failure, 5);
        assert!(summarized.message.contains("已自动重试 5 次"), "{}", summarized.message);
        let zero = summarize_failure(&failure, 0);
        assert!(!zero.message.contains("已自动重试"));
    }

    #[test]
    fn oversized_raw_message_is_trimmed() {
        let raw = "x".repeat(1000);
        let failure = LlmFailure::new(codes::UNKNOWN, raw);
        let summarized = summarize_failure(&failure, 0);
        assert!(summarized.message.chars().count() < 600, "{}", summarized.message.len());
        assert!(summarized.message.contains("…"));
    }

    #[test]
    fn feedback_channel_unchanged() {
        assert!(feedback_eligible(codes::MALFORMED_RESPONSE));
        assert!(!feedback_eligible(codes::AUTH));
        assert!(!feedback_eligible(codes::RATE_LIMIT));
        let text = feedback_text(&LlmFailure::new(codes::MALFORMED_RESPONSE, "bad"));
        assert!(text.contains("[harness]"));
        assert!(text.contains("MALFORMED_RESPONSE"));
    }

    #[test]
    fn feedback_for_stream_anomalies_does_not_blame_tool_arguments() {
        // 响应流畸形多为网关问题,不能指责"工具参数格式"(实测白烧两轮自纠)。
        let text = feedback_text(&LlmFailure::new(codes::MALFORMED_RESPONSE, "bad payload"));
        assert!(!text.contains("工具参数格式"), "{text}");
        assert!(text.contains("上游响应流"), "{text}");
        // 请求形态类错误仍保留原归因。
        let invalid = feedback_text(&LlmFailure::new(codes::INVALID_REQUEST, "bad args"));
        assert!(invalid.contains("工具参数格式"), "{invalid}");
    }

    #[test]
    fn stream_anomaly_summary_never_claims_retries_that_did_not_happen() {
        // MALFORMED_RESPONSE 不在可重试集:没有重试就不能说"已自动重试"。
        let failure = LlmFailure::new(codes::MALFORMED_RESPONSE, "malformed SSE payload: x");
        let summarized = summarize_failure(&failure, 0);
        assert!(!summarized.message.contains("已自动重试"), "{}", summarized.message);
        assert!(summarized.message.contains("原始信息"), "{}", summarized.message);
    }
}
