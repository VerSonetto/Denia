//! 工具层统一基建:输出预算、宽容参数解析、错误输出规范。
//!
//! 所有工具共享同一套约定,保证模型看到一致的失败形态与截断语义:
//! - **输出预算**:字符级截断只有一个权威出口([`apply_output_budget`]),
//!   工具内部只做业务级限量(行数/条数/命中数),不再各自持有字符 cap;
//! - **宽容解析**([`parse_tool_args`]):取第一个 JSON 值忽略尾部垃圾,
//!   字符串数字自动转整数,常见路径别名自动提升——常见错误调用不硬失败;
//! - **错误规范**([`tool_error`]):统一 `[工具错误] + 建议` 格式,
//!   建议必须可执行(模型据此自纠,而不是重试同样的调用)。

use crate::ToolOutput;
use denia_core::session::TruncationInfo;

/// 工具结果统一输出预算(字符数)。
///
/// 这是截断的唯一权威:工具内部不再各自持有字符上限,所有超长输出都在
/// 落盘前的这一层收敛,截断提示的格式全仓一致。
pub const OUTPUT_BUDGET_CHARS: usize = 32_000;

/// 截断提示(模型可见,附在截断后的内容尾部)。
fn truncation_notice(total: usize, shown: usize) -> String {
    format!(
        "\n\n…[输出已截断:共 {total} 字符,仅显示前 {shown} 字符。请缩小范围(更精确的 pattern/path),或用 offset/limit 分页获取剩余内容]"
    )
}

/// 应用输出预算:未超限原样返回 `(text, None)`;超限返回截断文本与
/// 截断事实(`TruncationInfo`)。按字符(char)截断,不撕 UTF-8。
pub fn apply_output_budget(text: &str) -> (String, Option<TruncationInfo>) {
    let total = text.chars().count();
    if total <= OUTPUT_BUDGET_CHARS {
        return (text.to_string(), None);
    }
    let shown: String = text.chars().take(OUTPUT_BUDGET_CHARS).collect();
    let notice = truncation_notice(total, OUTPUT_BUDGET_CHARS);
    (
        format!("{shown}{notice}"),
        Some(TruncationInfo {
            total_chars: total as u64,
            shown_chars: OUTPUT_BUDGET_CHARS as u64,
        }),
    )
}

/// 数值型参数字段:宽容解析时允许 `"offset": "10"` 这类字符串数字。
pub const NUMERIC_FIELDS: &[&str] = &[
    "offset",
    "limit",
    "max_results",
    "max_matches",
    "timeout_ms",
    "max_depth",
];

/// `path` 参数的常见别名:模型偶尔把路径放在 file/filepath/filename 里。
pub const PATH_ALIASES: &[&str] = &["file", "filepath", "filename", "file_path"];

/// 解析出第一个 JSON 值(忽略尾部垃圾)。
fn first_json_value(raw: &str) -> Result<serde_json::Value, String> {
    let mut iter = serde_json::Deserializer::from_str(raw.trim()).into_iter::<serde_json::Value>();
    match iter.next() {
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(error.to_string()),
        None => Err("参数为空".into()),
    }
}

/// 数值字段宽容转换:表内字段的字符串数字转成真正的数字。
/// 只动已知数值字段,不碰自由文本(如 content),避免把 "123" 这类
/// 正文误转成 JSON 数字。
fn coerce_numeric_fields(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    for key in NUMERIC_FIELDS {
        if let Some(item) = object.get_mut(*key)
            && item.is_string()
        {
            if let Ok(number) = item.as_str().unwrap().trim().parse::<i64>() {
                *item = serde_json::json!(number);
            }
        }
    }
}

/// path 别名提升:没有 `path` 但有别名键时,把别名值挪到 `path`。
fn promote_path_alias(value: &mut serde_json::Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    if object.contains_key("path") {
        return;
    }
    for alias in PATH_ALIASES {
        if let Some(item) = object.remove(*alias) {
            object.insert("path".into(), item);
            return;
        }
    }
}

/// 工具参数统一解析入口(宽容):
/// 1. 取第一个 JSON 值,忽略尾部垃圾(模型偶尔在参数后吐多余字符);
/// 2. 数值字段字符串数字强转(`"offset":"10"` → 10);
/// 3. `path` 别名提升(file/filepath/filename → path);
/// 4. 反序列化到目标类型。
pub fn parse_tool_args<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, String> {
    let mut value = first_json_value(raw)?;
    coerce_numeric_fields(&mut value);
    promote_path_alias(&mut value);
    serde_json::from_value(value).map_err(|error| error.to_string())
}

/// 统一工具错误输出:`[工具错误] {原因}\n建议:{hint}`。
/// 建议必须可执行,模型据此修正下一次调用。
pub fn tool_error(reason: impl std::fmt::Display, hint: impl std::fmt::Display) -> ToolOutput {
    ToolOutput {
        content: format!("[工具错误] {reason}\n建议:{hint}"),
        is_error: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_passes_short_text_through() {
        let (out, info) = apply_output_budget("short");
        assert_eq!(out, "short");
        assert!(info.is_none());
    }

    #[test]
    fn budget_truncates_and_reports() {
        let text = "字".repeat(OUTPUT_BUDGET_CHARS + 100);
        let (out, info) = apply_output_budget(&text);
        let info = info.expect("must truncate");
        assert_eq!(info.total_chars, (OUTPUT_BUDGET_CHARS + 100) as u64);
        assert_eq!(info.shown_chars, OUTPUT_BUDGET_CHARS as u64);
        assert!(out.contains("输出已截断"));
        assert!(out.contains(&format!("共 {} 字符", OUTPUT_BUDGET_CHARS + 100)));
        // 截断按字符计,不撕多字节 UTF-8:主体 + 提示正好是预算 + 提示长度。
        let notice = truncation_notice(OUTPUT_BUDGET_CHARS + 100, OUTPUT_BUDGET_CHARS);
        assert_eq!(
            out.chars().count(),
            OUTPUT_BUDGET_CHARS + notice.chars().count()
        );
        assert!(out.starts_with(&"字".repeat(100)));
    }

    #[test]
    fn lenient_parse_coerces_string_numbers_and_aliases() {
        #[derive(serde::Deserialize)]
        struct Args {
            path: String,
            offset: u64,
        }
        // 字符串数字 + 别名(file → path)+ 尾部垃圾,三层宽容全部生效。
        let parsed: Args =
            parse_tool_args(r#"{"file": "src/lib.rs", "offset": "12"} 完毕"#).unwrap();
        assert_eq!(parsed.path, "src/lib.rs");
        assert_eq!(parsed.offset, 12);
    }

    #[test]
    fn numeric_coercion_never_touches_free_text() {
        let value: serde_json::Value =
            parse_tool_args(r#"{"path":"a.txt","content":"123","offset":"3"}"#).unwrap();
        assert_eq!(value["content"], serde_json::json!("123"));
        assert_eq!(value["offset"], serde_json::json!(3));
    }

    #[test]
    fn path_alias_does_not_override_explicit_path() {
        let value: serde_json::Value =
            parse_tool_args(r#"{"path":"real.txt","file":"other.txt"}"#).unwrap();
        assert_eq!(value["path"], serde_json::json!("real.txt"));
    }

    #[test]
    fn bad_json_is_an_error() {
        let result: Result<serde_json::Value, String> = parse_tool_args("not json");
        assert!(result.is_err());
    }

    #[test]
    fn tool_error_format_is_actionable() {
        let out = tool_error("文件不存在", "先确认路径;相对路径锚定会话工作区");
        assert!(out.is_error);
        assert!(out.content.starts_with("[工具错误] 文件不存在\n建议:"));
    }
}
