//! 死循环检测器:模型连续输出完全相同的内容时强制中断本轮。
//!
//! 两种独立形态,任一命中即触发:
//! 1. **文本循环**:assistant 回复文本(全部 Text 块拼接、trim 归一)完全相同;
//! 2. **工具循环**:assistant 的工具调用序列(名称 + 参数)完全相同——
//!    实践中最常见的死循环是模型反复发起同样的调用。
//!
//! 语义(对齐"连续输出 3 次以上相同内容才判定"):
//! **放行前 3 次**——出现 1、2、3 次相同内容都正常处理(工具照常执行),
//! 第 4 次出现相同内容时才强制中断([`LOOP_THRESHOLD`] = 4,含当前条)。
//!
//! 指纹**跨 turn 记忆**(driver 按会话持有):纯文本回复的 turn 在第一条
//! 无调用消息就闭合,"用户手动继续后模型又重复"的场景只能靠跨轮记忆
//! 抓住——这正是"手动继续"按钮要对抗的循环。空消息(无文本无调用)
//! 不动任何检测线:它不是"不同的内容",不应重置连续计数。

use denia_core::stream::ContentBlock;

/// 触发阈值:同一条内容连续出现的第 N 次(含当前条)判定为死循环。
/// 即"三次以内放行、三次以上(第 4 次)才拦"。
pub const LOOP_THRESHOLD: u32 = 4;

/// 提醒阈值:连续重复达到这个次数时先注入提醒让模型自纠(不中断)。
///
/// 两级处置:提醒在前、中断在后。放行语义是"前 3 次
/// 正常处理",所以第 3 次出现时给提醒——模型还有一次机会换策略,
/// 第 4 次才中断。
pub const LOOP_WARN_THRESHOLD: u32 = 3;

/// 提醒文案(注入给模型)。
pub const LOOP_WARN_TEXT: &str = "检测到重复:你已连续多次输出相同的内容或发起相同的工具调用。\n\
    不要原样重复同一次调用,除非用户明确要求重试。\n\
    请用已有结果换一个下一步(改参数、换工具、缩小范围),或说明当前卡在哪里、需要用户提供什么。";

/// 死循环检测状态(turn 内)。
#[derive(Debug, Default)]
pub struct LoopGuard {
    /// 文本指纹线:最近一次的归一文本与连续出现次数。
    last_text: Option<String>,
    text_run: u32,
    /// 工具指纹线:最近一次的调用签名与连续出现次数。
    last_tools: Option<String>,
    tools_run: u32,
}

impl LoopGuard {
    /// 仅测试使用:生产路径走 `HashMap::remove(..).unwrap_or_default()`。
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// 每个 turn 开始时重置(检测范围是"本轮内连续输出")。
    ///
    /// 生产路径不需要它——driver 在轮次开始时把 guard 从表里 `remove`
    /// 出来,天然就是"每轮一份"。
    #[cfg(test)]
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// 观察一条已落盘的 assistant 消息,返回本轮应采取的处置。
    ///
    /// 两级语义(先提醒、后中断):
    /// - [`LoopVerdict::Warn`] — 连续重复达到 [`LOOP_WARN_THRESHOLD`] 次,
    ///   注入一条提醒让模型自纠(**不中断工作**);
    /// - [`LoopVerdict::Block`] — 连续重复达到 [`LOOP_THRESHOLD`] 次,
    ///   强制中断本轮(提醒无效时的兜底)。
    ///
    /// 提醒只在"恰好跨过提醒线"的那一次给出(不是每次重复都给),
    /// 避免刷屏;中断线每轮最多触发一次。
    pub fn observe(&mut self, blocks: &[ContentBlock]) -> LoopVerdict {
        let mut verdict = LoopVerdict::Continue;
        let mut warn: Option<u32> = None;
        let mut block: Option<u32> = None;

        // 文本线:空文本(纯工具调用轮)不动计数——它不是"不同的内容"。
        let text = normalized_text(blocks);
        if !text.is_empty() {
            if self.last_text.as_deref() == Some(text.as_str()) {
                self.text_run += 1;
            } else {
                self.last_text = Some(text);
                self.text_run = 1;
            }
            if self.text_run == LOOP_WARN_THRESHOLD {
                warn = Some(self.text_run);
            }
            if self.text_run >= LOOP_THRESHOLD {
                block = Some(self.text_run);
            }
        }
        // 工具线:无调用同样不动计数。
        let tools = tool_signature(blocks);
        if !tools.is_empty() {
            if self.last_tools.as_deref() == Some(tools.as_str()) {
                self.tools_run += 1;
            } else {
                self.last_tools = Some(tools);
                self.tools_run = 1;
            }
            if self.tools_run == LOOP_WARN_THRESHOLD {
                warn = Some(warn.unwrap_or(0).max(self.tools_run));
            }
            if self.tools_run >= LOOP_THRESHOLD {
                block = Some(block.unwrap_or(0).max(self.tools_run));
            }
        }

        if let Some(repeats) = block {
            verdict = LoopVerdict::Block { repeats };
        } else if let Some(repeats) = warn {
            verdict = LoopVerdict::Warn { repeats };
        }
        verdict
    }
}

/// [`LoopGuard::observe`] 的处置结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopVerdict {
    /// 正常:没有达到任何阈值。
    Continue,
    /// 达到提醒线:注入提醒让模型自纠,工作继续。
    Warn { repeats: u32 },
    /// 达到中断线:强制结束本轮。
    Block { repeats: u32 },
}

/// 文本指纹:全部 Text 块拼接并 trim。空文本(纯工具调用轮)不算。
fn normalized_text(blocks: &[ContentBlock]) -> String {
    let mut text = String::new();
    for block in blocks {
        if let ContentBlock::Text { text: part } = block {
            text.push_str(part);
        }
    }
    text.trim().to_string()
}

/// 工具指纹:全部工具调用的 `name(规范化参数)` 序列。无调用返回空。
///
/// **参数做键排序规范化**:`{"a":1,"b":2}` 与
/// `{"b":2,"a":1}` 是语义相同的调用,必须判为同一指纹,否则模型换个字段
/// 顺序重复调用就绕过了检测——这是真实存在的漏检路径。
fn tool_signature(blocks: &[ContentBlock]) -> String {
    let mut signature = String::new();
    for block in blocks {
        if let ContentBlock::ToolCall {
            name, arguments, ..
        } = block
        {
            signature.push_str(name);
            signature.push('(');
            signature.push_str(&normalize_arguments(arguments));
            signature.push_str(");");
        }
    }
    signature
}

/// 把工具参数规范化成稳定形式:解析 JSON 后按键排序重新序列化。
///
/// 解析失败(截断的 JSON、非 JSON 参数)时退化为"去掉全部空白"的原串——
/// 这仍能抓住"只有空白差异"的重复,且不会因为解析失败而漏检。
fn normalize_arguments(arguments: &str) -> String {
    let trimmed = arguments.trim();
    match serde_json::from_str::<serde_json::Value>(trimmed) {
        Ok(value) => stable_json(&value),
        Err(_) => trimmed.split_whitespace().collect::<Vec<_>>().join(""),
    }
}

/// 稳定 JSON 序列化:对象键排序、数组保序、其余原样。
fn stable_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner: Vec<String> = keys
                .iter()
                .map(|key| {
                    format!(
                        "{}:{}",
                        serde_json::Value::String((*key).clone()),
                        stable_json(&map[*key])
                    )
                })
                .collect();
            format!("{{{}}}", inner.join(","))
        }
        serde_json::Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(stable_json).collect();
            format!("[{}]", inner.join(","))
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text {
            text: text.to_string(),
        }
    }

    fn tool_block(name: &str, arguments: &str) -> ContentBlock {
        ContentBlock::ToolCall {
            id: format!("call_{name}"),
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }

    #[test]
    fn identical_text_three_times_still_passes() {
        // 放行语义:出现 3 次相同内容都正常处理,不触发。
        let mut guard = LoopGuard::new();
        assert_eq!(guard.observe(&[text_block("你好")]), LoopVerdict::Continue);
        assert_eq!(guard.observe(&[text_block("你好")]), LoopVerdict::Continue);
        // 第 3 次:达到提醒线,先提醒(不中断)。
        assert_eq!(
            guard.observe(&[text_block("你好")]),
            LoopVerdict::Warn { repeats: 3 }
        );
    }

    #[test]
    fn identical_text_fourth_time_triggers() {
        // 三次以上(第 4 次出现)才判定死循环。
        let mut guard = LoopGuard::new();
        for _ in 0..3 {
            let _ = guard.observe(&[text_block("你好")]);
        }
        assert_eq!(
            guard.observe(&[text_block("你好")]),
            LoopVerdict::Block { repeats: 4 }
        );
    }

    #[test]
    fn text_normalizes_surrounding_whitespace() {
        let mut guard = LoopGuard::new();
    #[test]
    fn text_normalizes_surrounding_whitespace() {
        let mut guard = LoopGuard::new();
        // 三个变体 trim 后都是 "你好",属同一内容:前两次放行。
        assert_eq!(guard.observe(&[text_block("你好\n")]), LoopVerdict::Continue);
        assert_eq!(guard.observe(&[text_block("  你好")]), LoopVerdict::Continue);
        // 第 3 次达到提醒线。
        assert_eq!(
            guard.observe(&[text_block("你好 ")]),
            LoopVerdict::Warn { repeats: 3 }
        );
        assert_eq!(
            guard.observe(&[text_block("你好")]),
            LoopVerdict::Block { repeats: 4 }
        );
    }
    }

    #[test]
    fn different_text_resets_the_run() {
        let mut guard = LoopGuard::new();
        assert_eq!(guard.observe(&[text_block("A")]), LoopVerdict::Continue);
        assert_eq!(guard.observe(&[text_block("A")]), LoopVerdict::Continue);
        assert_eq!(guard.observe(&[text_block("B")]), LoopVerdict::Continue);
        // 重置后重新累计:A 第 3 次给提醒,第 4 次中断。
        assert_eq!(guard.observe(&[text_block("A")]), LoopVerdict::Continue);
        assert_eq!(guard.observe(&[text_block("A")]), LoopVerdict::Continue);
        assert_eq!(
            guard.observe(&[text_block("A")]),
            LoopVerdict::Warn { repeats: 3 }
        );
        assert_eq!(
            guard.observe(&[text_block("A")]),
            LoopVerdict::Block { repeats: 4 }
        );
    }

    #[test]
    fn identical_tool_calls_fourth_time_triggers_even_with_varying_text() {
        let mut guard = LoopGuard::new();
        let call = [text_block("我看看"), tool_block("bash", r#"{"command":"ls"}"#)];
        for _ in 0..3 {
            let _ = guard.observe(&call);
        }
        // 文本+工具都相同,第 4 次触发。
        assert_eq!(guard.observe(&call), LoopVerdict::Block { repeats: 4 });
    }

    #[test]
    fn same_tools_with_varying_text_still_trigger_on_tool_line() {
        // 文本每轮都变,但工具调用完全相同:工具线抓住(第 4 次)。
        let mut guard = LoopGuard::new();
        for round in 0..4 {
            let blocks = [
                text_block(&format!("第 {round} 次尝试说明")),
                tool_block("read_file", r#"{"path":"a.rs"}"#),
            ];
            let hit = guard.observe(&blocks);
            match round {
                3 => assert_eq!(hit, LoopVerdict::Block { repeats: 4 }),
                2 => assert_eq!(hit, LoopVerdict::Warn { repeats: 3 }),
                _ => assert_eq!(hit, LoopVerdict::Continue),
            }
        }
    }

    #[test]
    fn differing_arguments_do_not_trigger() {
        let mut guard = LoopGuard::new();
        for i in 0..5 {
            let blocks = [tool_block("bash", &format!(r#"{{"command":"echo {i}"}}"#))];
            assert_eq!(
                guard.observe(&blocks),
                LoopVerdict::Continue,
                "iteration {i}"
            );
        }
    }

    #[test]
    fn empty_turns_do_not_poison_the_lines() {
        // 空消息(无文本无调用,罕见)不应清掉已累计的 run 也不应触发。
        let mut guard = LoopGuard::new();
        assert_eq!(guard.observe(&[text_block("A")]), LoopVerdict::Continue);
        assert_eq!(guard.observe(&[text_block("A")]), LoopVerdict::Continue);
        assert_eq!(guard.observe(&[]), LoopVerdict::Continue);
        // 空消息不重置计数:A 的第 3 次仍然给提醒。
        assert_eq!(
            guard.observe(&[text_block("A")]),
            LoopVerdict::Warn { repeats: 3 }
        );
        assert_eq!(
            guard.observe(&[text_block("A")]),
            LoopVerdict::Block { repeats: 4 }
        );
    }

    #[test]
    fn reset_clears_everything() {
        let mut guard = LoopGuard::new();
        assert_eq!(guard.observe(&[text_block("A")]), LoopVerdict::Continue);
        assert_eq!(guard.observe(&[text_block("A")]), LoopVerdict::Continue);
        guard.reset();
        assert_eq!(guard.observe(&[text_block("A")]), LoopVerdict::Continue);
    }
}

#[cfg(test)]
mod key_order_tests {
    use super::*;

    fn tool_block(name: &str, arguments: &str) -> ContentBlock {
        ContentBlock::ToolCall {
            id: format!("call_{name}"),
            name: name.to_string(),
            arguments: arguments.to_string(),
        }
    }

    #[test]
    fn reordered_object_keys_are_the_same_signature() {
        // 真实漏检路径:模型换个字段顺序重复调用,必须判为同一指纹。
        let a = [tool_block("read_file", r#"{"path":"a.rs","limit":10}"#)];
        let b = [tool_block("read_file", r#"{"limit":10,"path":"a.rs"}"#)];
        assert_eq!(tool_signature(&a), tool_signature(&b));
    }

    #[test]
    fn nested_object_keys_are_normalized() {
        let a = [tool_block("x", r#"{"a":{"y":1,"x":2},"b":3}"#)];
        let b = [tool_block("x", r#"{"b":3,"a":{"x":2,"y":1}}"#)];
        assert_eq!(tool_signature(&a), tool_signature(&b));
    }

    #[test]
    fn array_order_is_preserved() {
        // 数组是**有序**的:[1,2] 与 [2,1] 语义不同,不能判为同一指纹。
        let a = [tool_block("x", r#"{"v":[1,2]}"#)];
        let b = [tool_block("x", r#"{"v":[2,1]}"#)];
        assert_ne!(tool_signature(&a), tool_signature(&b));
    }

    #[test]
    fn reordered_keys_reach_the_block_threshold() {
        // 端到端:交替两种键顺序,第 4 次必须被判定为循环。
        let mut guard = LoopGuard::new();
        let variants = [
            r#"{"path":"a.rs","limit":10}"#,
            r#"{"limit":10,"path":"a.rs"}"#,
        ];
        let mut last = LoopVerdict::Continue;
        for i in 0..4 {
            last = guard.observe(&[tool_block("read_file", variants[i % 2])]);
        }
        assert_eq!(last, LoopVerdict::Block { repeats: 4 });
    }

    #[test]
    fn invalid_json_falls_back_to_whitespace_normalization() {
        let a = [tool_block("x", "{ broken  json")];
        let b = [tool_block("x", "{  broken json")];
        assert_eq!(tool_signature(&a), tool_signature(&b));
    }

    #[test]
    fn different_values_stay_different() {
        let a = [tool_block("read_file", r#"{"path":"a.rs"}"#)];
        let b = [tool_block("read_file", r#"{"path":"b.rs"}"#)];
        assert_ne!(tool_signature(&a), tool_signature(&b));
    }
}
