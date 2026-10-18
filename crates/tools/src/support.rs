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
    let reserved = truncation_notice(total, OUTPUT_BUDGET_CHARS)
        .chars()
        .count();
    let shown_chars = OUTPUT_BUDGET_CHARS.saturating_sub(reserved);
    let shown: String = text.chars().take(shown_chars).collect();
    let notice = truncation_notice(total, shown_chars);
    (
        format!("{shown}{notice}"),
        Some(TruncationInfo {
            total_chars: total as u64,
            shown_chars: shown_chars as u64,
        }),
    )
}

/// 数值型参数字段:宽容解析时允许 `"offset": "10"` 这类字符串数字。
///
/// 不含 `max_depth`:子代理的委派深度参数已移除（子代理禁止派遣子代理），
/// 它会在 schema 校验处直接报 `subagent/depth-config-removed`。
pub const NUMERIC_FIELDS: &[&str] = &[
    "offset",
    "limit",
    "max_results",
    "max_matches",
    "timeout_ms",
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
            && let Ok(number) = item.as_str().unwrap().trim().parse::<i64>()
        {
            *item = serde_json::json!(number);
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
    serde_json::from_value(value).map_err(|error| humanize_serde_error(&error.to_string()))
}

/// 参数解析失败的中文化:把 serde 的英文错误归成模型可直接自纠的中文三句型。
///
/// 覆盖三类,其余原样返回英文(至少保留 serde 的原始定位信息):
/// - 缺参:`missing field \`x\`` → `缺少参数 \`x\``;
/// - 多参:`unknown field \`y\`, expected ...` → `不支持参数 \`y\``;
/// - 类型错:`invalid type: string "5", expected u64` →
///   `参数类型不正确:应为 number,实际为 string`。
///
/// 类型错的英文原文不带字段名(字段定位需要 serde_path_to_error,不为此
/// 引依赖),所以类型错只报"期望/实际"的类型对,模型对照工具参数定义
/// 即可定位。
pub fn humanize_serde_error(error: &str) -> String {
    if let Some(field) = backticked_field(error, "missing field `") {
        return format!("缺少参数 `{field}`");
    }
    if let Some(field) = backticked_field(error, "unknown field `") {
        return format!("不支持参数 `{field}`");
    }
    if let Some(rest) = error.strip_prefix("invalid type: ")
        // 用 rsplit:值本身可能自带 ", expected " 子串(字符串内容),
        // 真正的分隔符是最后一个。
        && let Some((actual, expected)) = rest.rsplit_once(", expected ")
    {
        return format!(
            "参数类型不正确:应为 {},实际为 {}",
            json_type_name(expected),
            json_type_name(actual)
        );
    }
    error.to_string()
}

/// 取前缀之后、下一个反引号之前的字段名(`missing/unknown field` 的固定形态)。
fn backticked_field<'a>(error: &'a str, prefix: &str) -> Option<&'a str> {
    error.strip_prefix(prefix)?.split('`').next()
}

/// 把 serde 的类型描述归一成 JSON 侧类型名(string/number/array/object…)。
fn json_type_name(raw: &str) -> &str {
    let text = raw.strip_prefix("a ").unwrap_or(raw);
    match text.split([' ', '`']).next().unwrap_or(text) {
        "string" => "string",
        "bool" | "boolean" => "boolean",
        "u8" | "u16" | "u32" | "u64" | "u128" | "usize" | "i8" | "i16" | "i32" | "i64"
        | "i128" | "isize" | "f32" | "f64" | "integer" | "number" | "floating" => "number",
        "unit" => "null",
        "sequence" | "array" => "array",
        "map" | "object" => "object",
        other => other,
    }
}

/// 统一工具错误输出:`[工具错误] {原因}\n建议:{hint}`。
/// 建议必须可执行,模型据此修正下一次调用。
///
/// 不带 `report`:走到这里说明工具还没来得及产出任何执行事实。真正跑过
/// 命令又失败的路径(如 bash 启动失败 / 超时)自行挂 `report`。
pub fn tool_error(reason: impl std::fmt::Display, hint: impl std::fmt::Display) -> ToolOutput {
    ToolOutput {
        content: format!("[工具错误] {reason}\n建议:{hint}"),
        is_error: true,
        ..Default::default()
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
        assert!(info.shown_chars < OUTPUT_BUDGET_CHARS as u64);
        assert!(out.contains("输出已截断"));
        assert!(out.contains(&format!("共 {} 字符", OUTPUT_BUDGET_CHARS + 100)));
        // 提示也必须包含在模型可见预算内。
        assert!(out.chars().count() <= OUTPUT_BUDGET_CHARS);
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

    #[test]
    fn serde_errors_are_humanized() {
        // 三类英文原文与中文三句型的逐一对应(原文取自 serde_json 实测输出)。
        assert_eq!(
            humanize_serde_error("missing field `pattern`"),
            "缺少参数 `pattern`"
        );
        assert_eq!(
            humanize_serde_error("unknown field `pathh`, expected `path`"),
            "不支持参数 `pathh`"
        );
        assert_eq!(
            humanize_serde_error("invalid type: string \"xyz\", expected u64"),
            "参数类型不正确:应为 number,实际为 string"
        );
        assert_eq!(
            humanize_serde_error("invalid type: integer `123`, expected a string"),
            "参数类型不正确:应为 string,实际为 number"
        );
        assert_eq!(
            humanize_serde_error("invalid type: null, expected a string"),
            "参数类型不正确:应为 string,实际为 null"
        );
        assert_eq!(
            humanize_serde_error("invalid type: sequence, expected a string"),
            "参数类型不正确:应为 string,实际为 array"
        );
        // 归不了类的原样保留(JSON 语法错等),不吞掉 serde 的定位信息。
        assert_eq!(
            humanize_serde_error("expected ident at line 1 column 2"),
            "expected ident at line 1 column 2"
        );
    }

    #[test]
    fn parse_tool_args_surfaces_humanized_errors() {
        // 字段只经 serde 反序列化构造,测试只走错误路径,不需要读取。
        #[derive(serde::Deserialize, Debug)]
        #[allow(dead_code)]
        struct Args {
            path: String,
        }
        // 缺参端到端:错误路径不再吐英文。
        let error = parse_tool_args::<Args>("{}").unwrap_err();
        assert_eq!(error, "缺少参数 `path`");
        // 数字字段给成非数字字符串(宽容转换失败后的兜底)。
        #[derive(serde::Deserialize, Debug)]
        #[allow(dead_code)]
        struct NumArgs {
            offset: u64,
        }
        let error = parse_tool_args::<NumArgs>(r#"{"offset": "abc"}"#).unwrap_err();
        assert_eq!(error, "参数类型不正确:应为 number,实际为 string");
        // 多参(deny_unknown_fields 的 struct)。
        #[derive(serde::Deserialize, Debug)]
        #[allow(dead_code)]
        #[serde(deny_unknown_fields)]
        struct StrictArgs {
            path: String,
        }
        let error = parse_tool_args::<StrictArgs>(r#"{"path": "a", "extra": 1}"#).unwrap_err();
        assert_eq!(error, "不支持参数 `extra`");
    }
}
