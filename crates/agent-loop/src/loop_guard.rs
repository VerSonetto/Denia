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
    pub fn new() -> Self {
        Self::default()
    }

    /// 每个 turn 开始时重置(检测范围是"本轮内连续输出")。
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// 观察一条已落盘的 assistant 消息;返回 `Some(repeats)` 表示触发死循环,
    /// `repeats` 是触发线的连续重复次数(= 阈值)。
    pub fn observe(&mut self, blocks: &[ContentBlock]) -> Option<u32> {
        let mut triggered: Option<u32> = None;
        // 文本线:空文本(纯工具调用轮)不动计数——它不是"不同的内容"。
        let text = normalized_text(blocks);
        if !text.is_empty() {
            if self.last_text.as_deref() == Some(text.as_str()) {
                self.text_run += 1;
            } else {
                self.last_text = Some(text);
                self.text_run = 1;
            }
            if self.text_run >= LOOP_THRESHOLD {
                triggered = Some(self.text_run);
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
            if self.tools_run >= LOOP_THRESHOLD {
                triggered = Some(triggered.unwrap_or(self.tools_run).max(self.tools_run));
            }
        }
        triggered
    }
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

/// 工具指纹:全部工具调用的 `name(arguments)` 序列。无调用返回空。
fn tool_signature(blocks: &[ContentBlock]) -> String {
    let mut signature = String::new();
    for block in blocks {
        if let ContentBlock::ToolCall {
            name, arguments, ..
        } = block
        {
            signature.push_str(name);
            signature.push('(');
            signature.push_str(arguments.trim());
            signature.push_str(");");
        }
    }
    signature
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
        assert!(guard.observe(&[text_block("你好")]).is_none());
        assert!(guard.observe(&[text_block("你好")]).is_none());
        assert!(guard.observe(&[text_block("你好")]).is_none());
    }

    #[test]
    fn identical_text_fourth_time_triggers() {
        // 三次以上(第 4 次出现)才判定死循环。
        let mut guard = LoopGuard::new();
        for _ in 0..3 {
            assert!(guard.observe(&[text_block("你好")]).is_none());
        }
        assert_eq!(guard.observe(&[text_block("你好")]), Some(4));
    }

    #[test]
    fn text_normalizes_surrounding_whitespace() {
        let mut guard = LoopGuard::new();
        for text in ["你好\n", "  你好", "你好 "] {
            assert!(guard.observe(&[text_block(text)]).is_none());
        }
        assert_eq!(guard.observe(&[text_block("你好")]), Some(4));
    }

    #[test]
    fn different_text_resets_the_run() {
        let mut guard = LoopGuard::new();
        assert!(guard.observe(&[text_block("A")]).is_none());
        assert!(guard.observe(&[text_block("A")]).is_none());
        assert!(guard.observe(&[text_block("B")]).is_none());
        // 重置后重新累计:3 条 A 仍放行,第 4 条触发。
        assert!(guard.observe(&[text_block("A")]).is_none());
        assert!(guard.observe(&[text_block("A")]).is_none());
        assert!(guard.observe(&[text_block("A")]).is_none());
        assert_eq!(guard.observe(&[text_block("A")]), Some(4));
    }

    #[test]
    fn identical_tool_calls_fourth_time_triggers_even_with_varying_text() {
        let mut guard = LoopGuard::new();
        let call = [text_block("我看看"), tool_block("bash", r#"{"command":"ls"}"#)];
        for _ in 0..3 {
            assert!(guard.observe(&call).is_none());
        }
        // 文本+工具都相同,第 4 次触发。
        assert_eq!(guard.observe(&call), Some(4));
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
            if round == 3 {
                assert_eq!(hit, Some(4));
            } else {
                assert!(hit.is_none());
            }
        }
    }

    #[test]
    fn differing_arguments_do_not_trigger() {
        let mut guard = LoopGuard::new();
        for i in 0..5 {
            let blocks = [tool_block("bash", &format!(r#"{{"command":"echo {i}"}}"#))];
            assert!(guard.observe(&blocks).is_none(), "iteration {i}");
        }
    }

    #[test]
    fn empty_turns_do_not_poison_the_lines() {
        // 空消息(无文本无调用,罕见)不应清掉已累计的 run 也不应触发。
        let mut guard = LoopGuard::new();
        assert!(guard.observe(&[text_block("A")]).is_none());
        assert!(guard.observe(&[text_block("A")]).is_none());
        assert!(guard.observe(&[]).is_none());
        assert!(guard.observe(&[text_block("A")]).is_none());
        assert_eq!(guard.observe(&[text_block("A")]), Some(4));
    }

    #[test]
    fn reset_clears_everything() {
        let mut guard = LoopGuard::new();
        assert!(guard.observe(&[text_block("A")]).is_none());
        assert!(guard.observe(&[text_block("A")]).is_none());
        guard.reset();
        assert!(guard.observe(&[text_block("A")]).is_none());
    }
}
