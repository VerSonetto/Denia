//! dsh `@deepseek-ai/dsh-token-meter` 三层投影的 Rust 镜像。
//!
//! 1. `ContextBreakdown` —— **字符类别密度的启发式拆分** system / tools /
//!    message 三行,并在存在 provider 锚点时**校准**:固定成本行(系统 /
//!    工具)原样、每轮稳定,真实总量减去固定成本后的余额归对话消息行,
//!    三数整数和恒等于 `context_pressure` 的 projected 值 —— 面板不会再
//!    出现"明细加不出总数",也不会让不变的工具行随缩放漂移。
//! 2. `TurnTokenUsage` —— **精确**,provider 上报的 usage 累加。`usage`
//!    缺失或 lifecycle 缺段的轮次不进总和(同 dsh `deriveTurnTokenUsage`:
//!    任何不完整则整个 Turn 跳过)。
//! 3. `ContextPressure` —— **锚点 + 表面增量**(wire 形状对齐 dsh)。最近
//!    一次 provider usage 提供精确的 prompt 侧锚点;锚点之后消息的启发式
//!    增量叠加出 `projectedTokens`(回答下一次请求的 prompt 规模);路由
//!    容量来自 `request/context` 记录。没有 usage 样本就没有锚点,不做
//!    启发式兜底 —— provider 没报数就不显示占用,同 dsh。

use std::collections::HashMap;

use denia_core::message::{ChatMessage, ChatRole, ImageData, ToolCallRef};
use denia_core::session::{INTERRUPTED_TOOL_RESULT, SessionEnvelope, SessionEvent};
use denia_core::stream::{ContentBlock, TokenUsage};

mod account;
mod budget;

pub use account::{AccountedUsage, COMPACTION_USAGE_CODE, USAGE_ESTIMATED_CODE};
pub use budget::{
    BudgetBoundary, BudgetRefusal, Clock, EXECUTION_METERING_VERSION, ExecutionBudget,
    ExecutionCounters, ExecutionLimits, RequestCall, RequestKind, RequestSource,
};

/// 固定密度启发式:非 CJK 字符按每 4 字符 1 token(ASCII 下字符数 == 字节数)。
pub const CHARS_PER_TOKEN: u64 = 4;
/// CJK 字符(表意文字/假名/谚文/全角符号)约每字 1 token。
pub const CJK_CHARS_PER_TOKEN: u64 = 1;
/// 内容块结构开销(JSON 框与类型标签)。
pub const BLOCK_OVERHEAD: u64 = 4;
/// 每条消息角色框开销。
pub const ROLE_OVERHEAD: u64 = 4;

/// 带图工具结果的合并文案,**必须与 `derive_surface` 里的那一行逐字一致**
/// (等 tool-result 期间注入的截图并进 tool-result 时拼在正文之后)。
/// 口径一致性由 `meter_surface_matches_derived_surface` 一系测试钉住:文案
/// 分叉会立刻让表面总量对不上派生面。
const TOOL_IMAGE_NOTE: &str = "\n[附:工具产生的截图已附在本条结果]";

/// 一段上下文的 token 组成(纯估算)。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextBreakdown {
    pub system_tokens: u64,
    pub tools_tokens: u64,
    pub message_tokens: u64,
}

/// 一个 Turn 上累加的精确 usage(同 dsh `TurnTokenUsage`)。
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnTokenUsage {
    pub uncached_input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
}

impl TurnTokenUsage {
    /// 计费口径的全量 token(五分量之和);goal 预算记账用同一口径。
    pub fn total(&self) -> u64 {
        self.uncached_input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
            .saturating_add(self.reasoning_tokens)
    }
}

/// 上下文压力投影(对齐 dsh `contextPressure` 的 wire 形状,字段各自
/// last-wins、可缺省,None 时不序列化)。
///
/// - `contextWindow`:最新 `request/context` 记录的路由容量。
/// - `pressureTokens`:最近一次 provider usage 的 prompt 侧总量
///   (input + cache_read + cache_write,不含输出);没有 usage 样本就没有
///   该字段 —— dsh 语义:provider 没报数就不显示占用,不做启发式兜底。
/// - `projectedTokens`:锚点加上锚点之后表面的启发式增量,回答"下一次
///   请求的 prompt 有多大",而不是"上一次有多大"。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextPressure {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projected_tokens: Option<u64>,
}

/// 判断 CJK 类字符(表意文字/假名/谚文/全角符号)—— 按 Unicode code point。
pub fn is_cjk_char(ch: char) -> bool {
    let code = ch as u32;
    (0x2e80..=0x2fff).contains(&code) // 部首、笔画、CJK 符号
        || (0x3000..=0x30ff).contains(&code) // CJK 标点 + 假名
        || (0x3400..=0x4dbf).contains(&code) // 扩展 A
        || (0x4e00..=0x9fff).contains(&code) // 统一表意
        || (0xac00..=0xd7af).contains(&code) // 谚文音节
        || (0xf900..=0xfaff).contains(&code) // 兼容表意
        || (0xff00..=0xffef).contains(&code) // 全角形式
        || (0x20000..=0x2ffff).contains(&code) // 扩展 B 及以后
}

/// 字符类别密度估算:CJK 字符每字 1 token,其他字符每 4 字符 1 token
/// (UTF-8 下 ASCII 字符 = 1 字节,与旧字节密度一致;汉字从字节/4 的
/// 0.75 token/字修正为 1 token/字,贴近主流 BPE 词表的真实密度)。
pub fn estimate_text(text: &str) -> u64 {
    let mut cjk = 0u64;
    let mut other = 0u64;
    for ch in text.chars() {
        if is_cjk_char(ch) {
            cjk = cjk.saturating_add(1);
        } else {
            other = other.saturating_add(1);
        }
    }
    cjk.div_ceil(CJK_CHARS_PER_TOKEN)
        .saturating_add(other.div_ceil(CHARS_PER_TOKEN))
}

/// 系统提示词估算(与消息同一密度;`ROLE_OVERHEAD` 是角色框开销)。
pub fn estimate_system_tokens(text: &str) -> u64 {
    ROLE_OVERHEAD.saturating_add(estimate_text(text))
}

/// 工具 schema JSON 估算(与消息同一密度;`BLOCK_OVERHEAD` 是结构框开销)。
pub fn estimate_tools_tokens(json: &str) -> u64 {
    BLOCK_OVERHEAD.saturating_add(estimate_text(json))
}

/// 单个工具调用的启发式开销(块框 + id + 名字 + 参数)。
///
/// 与 [`estimate_message`] 里逐项累加的口径严格同源——超长参数打桩
/// (`ArgsCleared`)要**原位**替换一个调用的参数,只能靠这份单项账算增量。
fn estimate_call_tokens(id: &str, name: &str, arguments: &str) -> u64 {
    BLOCK_OVERHEAD
        .saturating_add(estimate_text(id))
        .saturating_add(estimate_text(name))
        .saturating_add(estimate_text(arguments))
}

fn estimate_tool_calls(calls: &[ToolCallRef]) -> u64 {
    calls.iter().fold(0u64, |tokens, call| {
        tokens.saturating_add(estimate_call_tokens(&call.id, &call.name, &call.arguments))
    })
}

/// 日志末尾仍未应答的工具调用在模型面上合成的那条中断结果的启发式开销
/// (对齐 `derive_surface` 末尾的 `INTERRUPTED_TOOL_RESULT` 兜底)。
fn synthetic_result_tokens(call_id: &str) -> u64 {
    estimate_message(&ChatMessage::tool_result(call_id, INTERRUPTED_TOOL_RESULT))
}

/// 单条模型可见消息的估算(角色框 + 字符类别密度 + 块开销;与压缩决策
/// `rough_tokens` 同口径,agent-loop 直接复用本函数)。
pub fn estimate_message(message: &ChatMessage) -> u64 {
    let mut tokens = ROLE_OVERHEAD;
    tokens = tokens.saturating_add(estimate_text(&message.content));
    // 图片在任何角色上都要计价。此前这一轮排在 Tool 角色的早退之后,带图
    // 工具结果(截图并进 tool-result,`derive_surface` 的多模态合并路径)
    // 里的图片被整条漏掉——模型面明明带着图,计量却按纯文本算。
    for image in &message.images {
        tokens = tokens
            .saturating_add(BLOCK_OVERHEAD)
            .saturating_add(estimate_text(&image.mime))
            .saturating_add(estimate_text(&image.data));
    }
    // tool-result 只有正文:推理与工具调用字段在它上面没有意义。
    if matches!(message.role, ChatRole::Tool) {
        return tokens;
    }
    if let Some(reasoning) = &message.reasoning_content {
        tokens = tokens
            .saturating_add(BLOCK_OVERHEAD)
            .saturating_add(estimate_text(reasoning));
    }
    for replay in &message.reasoning_replay {
        let mut metadata = replay.payload.clone();
        if let Some(object) = metadata.as_object_mut() {
            object.remove("thinking");
            object.remove("summary");
        }
        tokens = tokens
            .saturating_add(BLOCK_OVERHEAD)
            .saturating_add(estimate_text(&metadata.to_string()));
    }
    tokens = tokens.saturating_add(estimate_tool_calls(&message.tool_calls));
    tokens
}

fn extract_assistant_message(blocks: &[ContentBlock]) -> Option<ChatMessage> {
    denia_core::message::assistant_from_blocks(blocks)
}

/// 输入为 "input + cache_read + cache_write",等价于 provider 的 prompt 计费桶。
fn prompt_tokens(usage: &TokenUsage) -> u64 {
    let cache = usage.cache_read_tokens.unwrap_or(0);
    let cache_write = usage.cache_write_tokens_opt().unwrap_or(0);
    usage
        .input_tokens
        .saturating_add(cache)
        .saturating_add(cache_write)
}

fn add_usage(total: &mut TurnTokenUsage, part: &TurnTokenUsage) {
    total.uncached_input_tokens = total
        .uncached_input_tokens
        .saturating_add(part.uncached_input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(part.output_tokens);
    total.cache_read_tokens = total
        .cache_read_tokens
        .saturating_add(part.cache_read_tokens);
    total.cache_write_tokens = total
        .cache_write_tokens
        .saturating_add(part.cache_write_tokens);
    total.reasoning_tokens = total.reasoning_tokens.saturating_add(part.reasoning_tokens);
}

fn add_sample(acc: &mut TurnTokenUsage, sample: &TokenUsage) {
    acc.uncached_input_tokens = acc
        .uncached_input_tokens
        .saturating_add(sample.input_tokens);
    acc.output_tokens = acc.output_tokens.saturating_add(sample.output_tokens);
    if let Some(cache) = sample.cache_read_tokens {
        acc.cache_read_tokens = acc.cache_read_tokens.saturating_add(cache);
    }
    if let Some(write) = sample.cache_write_tokens_opt() {
        acc.cache_write_tokens = acc.cache_write_tokens.saturating_add(write);
    }
    if let Some(reasoning) = sample.reasoning_tokens {
        acc.reasoning_tokens = acc.reasoning_tokens.saturating_add(reasoning);
    }
}

/// 用 dsh `deriveTurnTokenUsage` 思路 fold 一个 turn 的精确 usage。
///
/// 接受一个 turn 起止区间内的所有 envelope(turn-start 到 turn-end),把每个
/// step 上出现的**所有** usage 样本累加:同一个 step 里流中断后重发过的那次
/// 尝试也是一次真实计费,只留最后一个会少算。结构不成对(turn/step 交叉、
/// 缺 turn-start)仍然整轮拒绝 —— 那是日志形态坏了,不是 provider 没报数。
///
/// 缺 usage 的 step **不再**让整轮作废(旧行为:任何一段缺失即整 Turn 跳过,
/// 于是 provider 没报数的轮次在账上凭空消失、目标预算永远用不完)。它的
/// usage 记 0,保守估算由写入侧的 [`USAGE_ESTIMATED_CODE`] 标注事件承担,
/// 那条事件由 [`ContextMeter`] 折进同一份账本(见 `apply_one_projected`)。
fn derive_turn_token_usage(events: &[SessionEnvelope]) -> Option<TurnTokenUsage> {
    let mut usage = TurnTokenUsage::default();
    let mut open_turn: Option<u32> = None;
    let mut step_attempt: Option<(u32, u32)> = None;
    let mut step_usage = TurnTokenUsage::default();
    let mut invalid = false;
    let mut found_turn = false;

    fn close_step(
        usage: &mut TurnTokenUsage,
        step_usage: &mut TurnTokenUsage,
        step_attempt: &mut Option<(u32, u32)>,
    ) {
        if step_attempt.take().is_some() {
            add_usage(usage, step_usage);
            *step_usage = TurnTokenUsage::default();
        }
    }

    for envelope in events {
        match &envelope.event {
            SessionEvent::TurnStart { turn } => {
                if open_turn.is_some() {
                    invalid = true;
                    break;
                }
                open_turn = Some(*turn);
                found_turn = true;
            }
            SessionEvent::TurnEnd { turn, .. } => {
                if Some(*turn) != open_turn {
                    invalid = true;
                    break;
                }
                close_step(&mut usage, &mut step_usage, &mut step_attempt);
                open_turn = None;
            }
            SessionEvent::StepStart { turn, step } => {
                if Some(*turn) != open_turn || step_attempt.is_some() {
                    invalid = true;
                    break;
                }
                step_attempt = Some((*turn, *step));
            }
            SessionEvent::StepEnd { turn, step } => {
                if Some((*turn, *step)) != step_attempt {
                    invalid = true;
                    break;
                }
                close_step(&mut usage, &mut step_usage, &mut step_attempt);
            }
            SessionEvent::AssistantMessage {
                turn,
                step,
                usage: Some(sample),
                ..
            } => {
                if Some((*turn, *step)) != step_attempt {
                    invalid = true;
                    break;
                }
                add_sample(&mut step_usage, sample);
            }
            _ => {}
        }
    }
    if invalid || !found_turn || open_turn.is_some() {
        return None;
    }
    Some(usage)
}

trait TokenUsageExt {
    fn cache_write_tokens_opt(&self) -> Option<u64>;
}

impl TokenUsageExt for TokenUsage {
    fn cache_write_tokens_opt(&self) -> Option<u64> {
        // denia 当前 TokenUsage 不含 cache_write;扩字段会破坏日志格式,
        // 因此总是 None,以保持字段名稳定。
        let _ = self;
        None
    }
}

/// 模型面与占用估算**必须是同一份投影**。
///
/// `ContextMeter` 的表面积分是 [`crate::estimate_message`] 在事件流上的
/// 增量 fold,而模型面由 `denia_core::session::derive_surface` 派生——两边
/// 都是折叠,折叠规则必须逐条一致,否则面板占用与压缩闸门会对着一个
/// **模型看不到的历史**做决策。这里逐条对齐的口径:
///
/// - 历史投影:被 `denia_session::HistoryProjection` 排除的事件不进表面
///   (`apply_one_projected` 的 `model_visible`);
/// - 基线注入通道(见 `denia_core::session::BASELINE_INJECTION_CHANNELS`)在
///   模型面上只留最新一条,旧节点连计价一起撤下(通道身份与折叠名单都问
///   core,不在这里另写一份判据);
/// - 摘要节点定位用 `keep_from - 1`(不是压缩事件自己的 seq),第二次压缩
///   时“旧摘要是否落在新区间”的判定才能与派生面一致;
/// - `ArgsCleared` 原位替换单个工具调用的参数(高估消除);
/// - 未应答的工具调用在表面末端补一条合成中断结果(低估消除);
/// - 带图用户消息在等 tool-result 期间并进 tool-result(而不是单独占一条);
/// - 带 `replaces` 的工具结果**原位**替换(保持原 seq,与派生面同语义)。
///
/// 不变量:`surface_tokens() == Σ estimate_message(derive_surface(日志前缀))`,
/// 由 `meter_surface_matches_derived_surface_*` 一系测试对账。
///
/// 增量 fold:维护 `ContextBreakdown`(纯估算)+ `TurnTokenUsage`(精确)+ `ContextPressure`(锚点 + 表面增量)。
///
/// 口径完全对齐 dsh `contextPressure` / `contextBreakdown` 投影:
/// - 表面总量(`surface_tokens`)对所有模型可见消息持续累加,不因锚点重置;
/// - 每个 usage 样本(AssistantMessage.usage)都重设锚点,且在**该消息加入
///   表面之前**盖章 —— 样本对应的请求不包含它自己的回复,锚点必须对着
///   请求所见的表面;
/// - 路由容量来自最新 `request/context` 记录;
/// - 没有 usage 样本就没有 `pressureTokens`,不做启发式兜底。
pub struct ContextMeter {
    system_tokens: u64,
    tools_tokens: u64,
    /// 模型可见消息的启发式运行总量(对齐 dsh `surfaceTokens`)。
    surface_tokens: u64,
    /// 每个 surface 节点(事件 seq)的启发式 token 数;替换剪枝时据此扣减
    /// 旧节点(对齐 dsh shadow-price 的净效果;内存版不需要落 shadow 事件)。
    surface_nodes: HashMap<u64, u64>,
    /// 基线注入通道 → 它在表面上的当前节点 seq。同通道新版本入表时靠它
    /// 定位要撤下的旧节点(对齐 `derive_surface` 的按通道折叠)。
    baseline_nodes: HashMap<String, u64>,
    /// 日志末尾仍未应答的工具调用(保持声明顺序)。
    ///
    /// `derive_surface` 会为它们各补一条合成中断结果(provider 要求每个
    /// tool_call 都有 tool_result),所以它们的开销必须同时计入表面;
    /// 应答时(D)撤下。
    unanswered: Vec<UnansweredCall>,
    /// 工具调用 id → 其所在 assistant 表面节点:超长参数打桩(`ArgsCleared`)
    /// 要原位替换**那一个调用**的参数,需要知道它落在哪条消息上。
    call_nodes: HashMap<String, CallNode>,
    /// 等 tool-result 期间注入的带图用户消息里的图片(暂存,并入紧随的
    /// tool-result;对齐 `derive_surface` 的 `pending_tool_images`)。
    pending_tool_images: Vec<ImageData>,
    /// 表面已按哪一份历史投影折过表(见 [`ContextMeter::folded_projection`])。
    folded_projection: Option<ProjectionStamp>,
    /// session 内累计的精确 usage(每个完成的 Turn 累加一次)。
    turn_usage: TurnTokenUsage,
    /// 靠保守估算计入账本的步数(provider 没报 usage;见
    /// [`Self::estimated_steps`])。
    estimated_steps: u64,
    /// 最近一次 provider usage 的 prompt 侧总量(锚点;None = 无样本)。
    pressure_tokens: Option<u64>,
    /// 取锚点时的表面总量;锚点后的表面增量 = surface - sampled。
    sampled_surface_tokens: Option<u64>,
    /// 取锚点时的系统提示 / 工具声明估算。两者也会在会话中途变化(动态
    /// 系统提示、工具集增删),必须与表面增量走同一有向口径,否则它们的
    /// 变化会被错误地记到对话消息行上。
    sampled_system_tokens: Option<u64>,
    sampled_tools_tokens: Option<u64>,
    /// 最新 `request/context` 的路由容量(上下文窗口)。
    context_window: Option<u64>,
}

/// 把固定成本行(system / tools)与动态行(message)对到锚定总量上的校准。
///
/// 余额法:固定成本直接显示估算值(工具集不变时每轮稳定,语义为"工具
/// 声明成本"),`target` 减去固定成本后的**余额**全部记给对话消息行 ——
/// 它是动态最大、占绝对大头、理应吸收所有估算误差的那部分。三数之和
/// 恒等于 `target`(即 `projectedTokens`),且固定行不再随缩放比漂移。
///
/// 防御退化:若 `target` 居然小于固定成本之和(provider 报的总量低于
/// 固定估算,现实几乎不可能),按最大余数法把三数一起缩放到 `target`,
/// 保证不变量在任何输入下成立。
fn calibrate_breakdown(system: u64, tools: u64, message: u64, target: u64) -> ContextBreakdown {
    let fixed = system.saturating_add(tools);
    let total = fixed.saturating_add(message);
    if total == 0 {
        return ContextBreakdown {
            system_tokens: 0,
            tools_tokens: 0,
            message_tokens: target,
        };
    }
    if message > 0 && total > fixed && target >= fixed {
        // 常态:余额全部归对话消息,固定行原样。
        return ContextBreakdown {
            system_tokens: system,
            tools_tokens: tools,
            message_tokens: target - fixed,
        };
    }
    // 全零份额或退化(总量 ≥ 固定行但 target 低于固定成本):按估算占比
    // 缩放,保证三数整数和恰好为 target。
    // token 数远小于 2^53,f64 缩放精度足够;只用于决定分配比例。
    let scaled = [
        system as f64 * target as f64 / total as f64,
        tools as f64 * target as f64 / total as f64,
        message as f64 * target as f64 / total as f64,
    ];
    let mut allocated = [
        scaled[0].floor() as u64,
        scaled[1].floor() as u64,
        scaled[2].floor() as u64,
    ];
    let mut remainder = target - allocated[0] - allocated[1] - allocated[2];
    let mut order = [0usize, 1, 2];
    order.sort_by(|&a, &b| {
        let frac_a = scaled[a] - allocated[a] as f64;
        let frac_b = scaled[b] - allocated[b] as f64;
        frac_b
            .partial_cmp(&frac_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut index = 0usize;
    while remainder > 0 {
        allocated[order[index % 3]] = allocated[order[index % 3]].saturating_add(1);
        remainder -= 1;
        index += 1;
    }
    ContextBreakdown {
        system_tokens: allocated[0],
        tools_tokens: allocated[1],
        message_tokens: allocated[2],
    }
}

/// 本消息的基线通道名(通道身份与折叠名单都问 core,口径唯一)。
fn baseline_channel<'a>(
    text: &'a str,
    injected: bool,
    channel: Option<&'a str>,
) -> Option<&'a str> {
    denia_core::session::injection_channel(text, injected, channel)
        .filter(|name| denia_core::session::is_baseline_channel(name))
}

/// 尚未应答的工具调用:合成中断结果的价码(应答时按此撤下)。
#[derive(Debug, Clone)]
struct UnansweredCall {
    id: String,
    synthetic_tokens: u64,
}

/// 历史投影指纹:`(投影的 source_last_seq, drop_seqs.len())`。
///
/// 投影是**事后**建立的(旧子代理首次继续),建立时 meter 里已经折进了它要
/// 排除的事件。调用方拿指纹判断“这份投影是否已经按到我的表上”,不一致就
/// 整条重建——投影是落盘后不可变的清单,这两个数足以区分“还没建 / 刚建 /
/// 换了一份”。
pub type ProjectionStamp = (u64, usize);

/// 工具调用在表面上的位置与自身开销(参数打桩原位替换用)。
///
/// 调用 id 不在这里再存一份:它就是 `ContextMeter::call_nodes` 的键(热路径
/// 上每个工具调用少一次 `String` 克隆)。
#[derive(Debug, Clone)]
struct CallNode {
    /// 承载该调用的 assistant 表面节点的 seq。
    seq: u64,
    name: String,
    /// 该调用当前的启发式开销(id + name + arguments + 块框)。
    tokens: u64,
}

impl ContextMeter {
    pub fn new() -> Self {
        Self {
            system_tokens: 0,
            tools_tokens: 0,
            surface_tokens: 0,
            surface_nodes: HashMap::new(),
            baseline_nodes: HashMap::new(),
            unanswered: Vec::new(),
            call_nodes: HashMap::new(),
            pending_tool_images: Vec::new(),
            folded_projection: None,
            turn_usage: TurnTokenUsage::default(),
            estimated_steps: 0,
            pressure_tokens: None,
            sampled_surface_tokens: None,
            sampled_system_tokens: None,
            sampled_tools_tokens: None,
            context_window: None,
        }
    }

    /// 单事件增量回放(全部事件视为模型可见)。
    pub fn apply_one(&mut self, envelope: &SessionEnvelope) {
        self.apply_one_projected(envelope, true);
    }

    /// 单事件增量回放(会话历史投影感知版)。
    ///
    /// `model_visible = false` 表示该事件被会话的历史投影排除(旧子代理的
    /// 父运行态与自动注入,见 `denia_session::HistoryProjection`):它不进模型
    /// 面,因此也不能进表面积分——否则“主请求看不见的历史”仍在推高占用面板
    /// 与压缩闸门,与主请求的投影分叉。
    ///
    /// 例外:`request/context`(路由容量)与 `system-prompt` 是元数据而不是
    /// 历史(`derive_surface` 对它们完全不投影),被投影过滤掉时仍要生效。
    /// 否则旧 child 继续运行时会短暂失去窗口容量,压缩闸门判定不出压力。
    ///
    /// 用量记账标注(压缩请求用量、缺 usage 的保守估算)同理:它是账单事实,
    /// 不是模型面内容 —— 既不进表面,也不受投影影响。
    pub fn apply_one_projected(&mut self, envelope: &SessionEnvelope, model_visible: bool) {
        match &envelope.event {
            SessionEvent::RequestContext { context_window, .. } => {
                // 路由容量 last-wins;未通告时移除(dsh 同语义)。
                self.context_window = *context_window;
                return;
            }
            SessionEvent::SystemPrompt { .. } => {
                // 不参与 fold;由 driver 调 `set_system_tokens` 喂 framed 版本。
                return;
            }
            SessionEvent::RetryAttempt { .. } => {
                // 真实重试轨迹(attempt ≥ 1)与其它 code 在此不产生任何计数。
                if let Some(accounted) = AccountedUsage::parse(&envelope.event) {
                    let part = TurnTokenUsage {
                        uncached_input_tokens: accounted.uncached_input_tokens,
                        output_tokens: accounted.output_tokens,
                        cache_read_tokens: accounted.cache_read_tokens,
                        cache_write_tokens: accounted.cache_write_tokens,
                        reasoning_tokens: accounted.reasoning_tokens,
                    };
                    add_usage(&mut self.turn_usage, &part);
                    if accounted.estimated {
                        self.estimated_steps = self.estimated_steps.saturating_add(1);
                    }
                }
                return;
            }
            _ => {}
        }
        if !model_visible {
            return;
        }
        match &envelope.event {
            SessionEvent::UserMessage {
                text,
                images,
                injected,
                channel,
            } => {
                // 等 tool-result 期间注入的带图用户消息**不进表面**:图片暂存,
                // 并入紧随的 tool-result(多数 provider 要求 tool_call 与
                // tool_result 相邻)。条件与去处与 `derive_surface` 逐字一致。
                if !self.unanswered.is_empty() && !images.is_empty() {
                    self.pending_tool_images.extend(images.iter().cloned());
                    return;
                }
                let message = if images.is_empty() {
                    ChatMessage::user(text)
                } else {
                    ChatMessage::user_with_images(text, images.clone())
                };
                let tokens = estimate_message(&message);
                // 基线通道的注入块在模型面上只留最新一条(`derive_surface` 的
                // 同通道折叠):旧节点连带计价一起撤下,否则表面会同时计入
                // 模型已经看不到的旧版本。通道身份与折叠名单都问 core,不在
                // 这里再写一份判据。
                match baseline_channel(text, *injected, channel.as_deref()) {
                    Some(channel) => self.fold_baseline_add(channel, envelope.seq, tokens),
                    None => self.fold_message_add(envelope.seq, tokens),
                }
            }
            SessionEvent::AgentDelivery { text, .. } => {
                let message = ChatMessage::user(text);
                self.fold_message_add(envelope.seq, estimate_message(&message));
            }
            SessionEvent::AssistantMessage { blocks, usage, .. } => {
                // usage 样本先于本消息入表:消息本身不在产生它的请求里。
                if let Some(sample) = usage {
                    self.pressure_tokens = Some(prompt_tokens(sample));
                    self.sampled_surface_tokens = Some(self.surface_tokens);
                    self.sampled_system_tokens = Some(self.system_tokens);
                    self.sampled_tools_tokens = Some(self.tools_tokens);
                }
                let Some(message) = extract_assistant_message(blocks) else {
                    return;
                };
                for call in &message.tool_calls {
                    // 未应答的调用在 surface 末端会合成一条中断结果;先计入,
                    // 应答时再撤下。
                    self.note_unanswered(&call.id);
                    // 记下调用所在节点:参数打桩要原位替换这条消息里的那个调用。
                    // 调用 id 就是这张表的键,不再在值里存一份(热路径的克隆
                    // 只留一次)。
                    self.call_nodes.insert(
                        call.id.clone(),
                        CallNode {
                            seq: envelope.seq,
                            name: call.name.clone(),
                            tokens: estimate_call_tokens(&call.id, &call.name, &call.arguments),
                        },
                    );
                }
                self.fold_message_add(envelope.seq, estimate_message(&message));
            }
            SessionEvent::ToolResult {
                call_id,
                content,
                replaces,
                ..
            } => {
                // 已应答:surface 末端不再为它合成中断结果。
                self.answer_call(call_id);
                let tokens = if self.pending_tool_images.is_empty() {
                    estimate_message(&ChatMessage::tool_result(call_id.clone(), content.clone()))
                } else {
                    // 带图合并:与 `derive_surface` 同样的合并文案与多模态
                    // 形状(图片计价由 `estimate_message` 负责)。
                    let images = std::mem::take(&mut self.pending_tool_images);
                    let mut merged = content.clone();
                    merged.push_str(TOOL_IMAGE_NOTE);
                    let mut message = ChatMessage::tool_result(call_id.clone(), merged);
                    message.images = images;
                    estimate_message(&message)
                };
                if let Some(replaced_seq) = replaces
                    && let Some(old_tokens) = self.surface_nodes.remove(replaced_seq)
                {
                    // 剪枝替换:**原位**扣旧、原位落新——被替换项保留原 seq,
                    // 与 `derive_surface` 的 `item.message = ...` 同语义。
                    // 若按新事件 seq 落节点,第二次替换同一个目标时会找不到
                    // 旧节点,导致扣减漏掉。
                    self.surface_tokens = self.surface_tokens.saturating_sub(old_tokens);
                    self.fold_message_add(*replaced_seq, tokens);
                    return;
                }
                self.fold_message_add(envelope.seq, tokens);
            }
            SessionEvent::ArgsCleared {
                call_id,
                placeholder,
                ..
            } => {
                // 超长参数打桩:原位替换那条 assistant 消息里的调用参数,只改
                // 该调用自身的开销。找不到节点说明该调用已不在模型面上
                // (被压缩折叠),与 `derive_surface` 一样静默跳过。
                let Some(node) = self.call_nodes.get(call_id).cloned() else {
                    return;
                };
                let Some(current) = self.surface_nodes.get(&node.seq).copied() else {
                    return;
                };
                let updated = estimate_call_tokens(call_id, &node.name, placeholder);
                let adjusted = current.saturating_sub(node.tokens).saturating_add(updated);
                self.surface_nodes.insert(node.seq, adjusted);
                if updated >= node.tokens {
                    self.surface_tokens = self.surface_tokens.saturating_add(updated - node.tokens);
                } else {
                    self.surface_tokens = self.surface_tokens.saturating_sub(node.tokens - updated);
                }
                self.call_nodes.insert(
                    call_id.clone(),
                    CallNode {
                        tokens: updated,
                        ..node
                    },
                );
            }
            SessionEvent::CompactionSummary {
                summary,
                replaces_from,
                replaces_to,
                keep_from,
                ..
            } => {
                // LLM 总结压缩:被压缩区间的事件不再占据表面,摘要消息
                // 代替它们计价(对齐 Claude Code compact:日志保留旧事件,
                // 表面只投影新形态)。
                let mut removed = 0u64;
                self.surface_nodes.retain(|seq, tokens| {
                    if *seq >= *replaces_from && *seq <= *replaces_to {
                        removed = removed.saturating_add(*tokens);
                        false
                    } else {
                        true
                    }
                });
                self.surface_tokens = self.surface_tokens.saturating_sub(removed);
                // 基线通道的定位表跟着一起清:指向已离开表面的 seq,下一次
                // 同通道注入会以为"还有旧节点可扣"(实际已被压缩扣过)。
                self.baseline_nodes
                    .retain(|_, seq| self.surface_nodes.contains_key(seq));
                // 摘要节点定位与 `derive_surface` 一致:插在保留窗口**之前**
                // (`keep_from - 1`),而不是压缩事件自身的 seq。这个 seq 决定了
                // 下一次压缩时“旧摘要是否落在新区间”的判定,口径必须一致。
                self.fold_message_add(
                    keep_from.saturating_sub(1),
                    estimate_message(&ChatMessage::user(summary)),
                );
            }
            _ => {}
        }
    }

    /// 记上一条未应答的工具调用:它的合成中断结果立即计入表面
    /// (`derive_surface` 在日志前缀末尾总是补上它。)
    fn note_unanswered(&mut self, call_id: &str) {
        let synthetic_tokens = synthetic_result_tokens(call_id);
        self.surface_tokens = self.surface_tokens.saturating_add(synthetic_tokens);
        self.unanswered.push(UnansweredCall {
            id: call_id.to_string(),
            synthetic_tokens,
        });
    }

    /// 该调用被应答:撤下合成中断结果的计价。
    fn answer_call(&mut self, call_id: &str) {
        let mut answered = 0u64;
        self.unanswered.retain(|call| {
            if call.id == call_id {
                answered = answered.saturating_add(call.synthetic_tokens);
                false
            } else {
                true
            }
        });
        self.surface_tokens = self.surface_tokens.saturating_sub(answered);
    }

    /// 一批事件按顺序增量回放。
    pub fn fold(&mut self, events: &[SessionEnvelope]) {
        for envelope in events {
            self.apply_one(envelope);
        }
    }

    /// 把 Turn 闭包内的事件喂进来,fold 出该 turn 的精确 usage 并并入会话累计。
    /// 必须在 turn 闭合时(`turn-end` 之后)调用;返回的 Option 表示该 turn
    /// 是不是 fold 成功。锚点不在这里设:每个 usage 样本由 `apply_one` 处理。
    pub fn fold_turn(&mut self, events: &[SessionEnvelope]) -> bool {
        if let Some(turn) = derive_turn_token_usage(events) {
            self.turn_usage.uncached_input_tokens = self
                .turn_usage
                .uncached_input_tokens
                .saturating_add(turn.uncached_input_tokens);
            self.turn_usage.output_tokens = self
                .turn_usage
                .output_tokens
                .saturating_add(turn.output_tokens);
            self.turn_usage.cache_read_tokens = self
                .turn_usage
                .cache_read_tokens
                .saturating_add(turn.cache_read_tokens);
            self.turn_usage.cache_write_tokens = self
                .turn_usage
                .cache_write_tokens
                .saturating_add(turn.cache_write_tokens);
            self.turn_usage.reasoning_tokens = self
                .turn_usage
                .reasoning_tokens
                .saturating_add(turn.reasoning_tokens);
            true
        } else {
            false
        }
    }

    fn fold_message_add(&mut self, seq: u64, tokens: u64) {
        self.surface_tokens = self.surface_tokens.saturating_add(tokens);
        self.surface_nodes.insert(seq, tokens);
    }

    /// 基线通道注入入表:同一通道只保留最新一条,旧节点连计价一起撤下。
    ///
    /// 与 `derive_surface` 的按通道折叠同口径(旧节点已被压缩移除时
    /// `remove` 返回 `None`,不会重复扣)。
    fn fold_baseline_add(&mut self, channel: &str, seq: u64, tokens: u64) {
        if let Some(previous) = self.baseline_nodes.insert(channel.to_string(), seq)
            && let Some(previous_tokens) = self.surface_nodes.remove(&previous)
        {
            self.surface_tokens = self.surface_tokens.saturating_sub(previous_tokens);
        }
        self.fold_message_add(seq, tokens);
    }

    /// 未校准的表面启发式总量(system / tools 之外的模型可见消息)。
    ///
    /// 与 `derive_surface` 的派生面**严格同口径**:
    /// `surface_tokens() == Σ estimate_message(derive_surface(日志前缀))`。
    /// 面板显示走 [`Self::breakdown`](带锚点校准),这个原始量给压缩预算
    /// 与口径对账测试用——对不上就是 bug,不是噪声。
    pub fn surface_tokens(&self) -> u64 {
        self.surface_tokens
    }

    /// 表面已按哪一份历史投影折过表(`None` = 折表时无投影)。
    ///
    /// 投影晚于 meter 建立时(旧子代理首次继续),已折进表里的被排除事件
    /// 不会自己消失——调用方(`Session::append`)比对这个指纹,不一致就整条
    /// 重建,否则偏差只会在重载后发作。
    pub fn folded_projection(&self) -> Option<ProjectionStamp> {
        self.folded_projection
    }

    /// 标记表面是按哪一份投影折出来的(整表折完后由折表方设置)。
    pub fn set_folded_projection(&mut self, stamp: Option<ProjectionStamp>) {
        self.folded_projection = stamp;
    }

    /// 读当前快照:校准后的组成拆分(系统提示词 / 工具 / 对话消息)。
    ///
    /// 有 provider 锚点(`pressure_tokens` + 采样表面)时,固定成本行
    /// (系统/工具)原样显示、每轮稳定,真实总量减去固定成本后的**余额**
    /// 归对话消息行 —— 三数整数和恒等于 [`Self::context_pressure`] 的
    /// projected 值,与面板顶部显示一致;无锚点时即原始启发式估算(此时
    /// 目标本就和值相等,行为一致)。
    pub fn breakdown(&self) -> ContextBreakdown {
        let raw = ContextBreakdown {
            system_tokens: self.system_tokens,
            tools_tokens: self.tools_tokens,
            message_tokens: self.surface_tokens,
        };
        let (Some(pressure), Some(sampled)) = (self.pressure_tokens, self.sampled_surface_tokens)
        else {
            return raw;
        };
        let target = pressure.saturating_add_signed(self.projected_drift(sampled));
        calibrate_breakdown(
            self.system_tokens,
            self.tools_tokens,
            self.surface_tokens,
            target,
        )
    }

    /// 读当前精确 usage 累计。
    pub fn turn_usage(&self) -> TurnTokenUsage {
        self.turn_usage.clone()
    }

    /// 账本里靠保守估算凑出来的步数(provider 没报 usage)。
    ///
    /// 非零意味着 `turn_usage` 里有一部分不是 provider 上报的精确值:
    /// 面板/评测报告按 [`EXECUTION_METERING_VERSION`] 与这个计数决定能不能
    /// 把两个总量直接相减。
    pub fn estimated_steps(&self) -> u64 {
        self.estimated_steps
    }

    /// 锚点之后的有向漂移:表面消息 + 系统提示 + 工具声明。
    ///
    /// 必须带符号:微压缩与摘要会把节点从表面移除,清理腾出的空间如果不
    /// 回落到投影上,已经腾出空间的会话仍会判定为超压,进而进入有信息
    /// 损失的摘要压缩。下限 0 由 `saturating_add_signed` 保证。
    fn projected_drift(&self, sampled: u64) -> i64 {
        let mut drift = self.surface_tokens as i64 - sampled as i64;
        if let Some(sampled_system) = self.sampled_system_tokens {
            drift += self.system_tokens as i64 - sampled_system as i64;
        }
        if let Some(sampled_tools) = self.sampled_tools_tokens {
            drift += self.tools_tokens as i64 - sampled_tools as i64;
        }
        drift
    }

    /// 读当前压力投影(锚点 + 锚点后表面增量)。
    pub fn context_pressure(&self) -> ContextPressure {
        let projected = self.pressure_tokens.map(|pressure| {
            let sampled = self.sampled_surface_tokens.unwrap_or(0);
            pressure.saturating_add_signed(self.projected_drift(sampled))
        });
        ContextPressure {
            context_window: self.context_window,
            pressure_tokens: self.pressure_tokens,
            projected_tokens: projected,
        }
    }

    /// 更新系统提示词 token(framed 版本)。
    pub fn set_system_tokens(&mut self, tokens: u64) {
        self.system_tokens = tokens;
    }

    /// 更新工具声明 token。
    pub fn set_tools_tokens(&mut self, tokens: u64) {
        self.tools_tokens = tokens;
    }
}

impl Default for ContextMeter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::SessionEnvelope;

    fn envelope(seq: u64, event: SessionEvent) -> SessionEnvelope {
        SessionEnvelope {
            seq,
            time: 1_700_000_000_000 + seq,
            event,
        }
    }

    #[test]
    fn estimate_simple_text() {
        let m = ChatMessage::user("hello world");
        assert_eq!(estimate_message(&m), ROLE_OVERHEAD + (11_u64.div_ceil(4)));
    }

    #[test]
    fn system_prompt_event_ignored_in_breakdown() {
        let events = vec![
            envelope(
                1,
                SessionEvent::UserMessage {
                    text: "a".into(),
                    injected: false,
                    images: Vec::new(),
                    channel: None,
                },
            ),
            envelope(
                2,
                SessionEvent::SystemPrompt {
                    turn: 1,
                    step: 1,
                    text: "任一".into(),
                },
            ),
        ];
        let mut meter = ContextMeter::new();
        meter.fold(&events);
        assert_eq!(meter.breakdown().system_tokens, 0);
        assert!(meter.breakdown().message_tokens > 0);
    }

    #[test]
    fn derive_turn_token_usage_with_two_attempts() {
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(2, SessionEvent::StepStart { turn: 1, step: 1 }),
            envelope(
                3,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![ContentBlock::Text {
                        text: "tool".into(),
                    }],
                    usage: Some(TokenUsage {
                        input_tokens: 100,
                        output_tokens: 5,
                        cache_read_tokens: Some(20),
                        reasoning_tokens: None,
                    }),
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                    first_token_time: None,
                },
            ),
            envelope(4, SessionEvent::StepEnd { turn: 1, step: 1 }),
            envelope(5, SessionEvent::StepStart { turn: 1, step: 2 }),
            envelope(
                6,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 2,
                    blocks: vec![ContentBlock::Text { text: "ok".into() }],
                    usage: Some(TokenUsage {
                        input_tokens: 80,
                        output_tokens: 3,
                        cache_read_tokens: Some(0),
                        reasoning_tokens: Some(2),
                    }),
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                    first_token_time: None,
                },
            ),
            envelope(7, SessionEvent::StepEnd { turn: 1, step: 2 }),
            envelope(
                8,
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: denia_core::session::TurnEndReason::Completed,
                },
            ),
        ];
        let u = derive_turn_token_usage(&events).expect("turn should fold");
        assert_eq!(u.uncached_input_tokens, 180);
        assert_eq!(u.output_tokens, 8);
        assert_eq!(u.cache_read_tokens, 20);
        assert_eq!(u.reasoning_tokens, 2);
    }

    #[test]
    fn derive_turn_token_usage_counts_a_turn_with_a_usage_less_step() {
        // 缺一个 step 的 usage 不再整轮作废:已知的部分照常入账,缺的那段
        // 记 0(保守估算由写入侧的标注事件承担,见 `estimated_usage_events`)。
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(2, SessionEvent::StepStart { turn: 1, step: 1 }),
            envelope(
                3,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![ContentBlock::Text { text: "x".into() }],
                    usage: None,
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                    first_token_time: None,
                },
            ),
            envelope(4, SessionEvent::StepEnd { turn: 1, step: 1 }),
            envelope(5, SessionEvent::StepStart { turn: 1, step: 2 }),
            envelope(
                6,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 2,
                    blocks: vec![ContentBlock::Text { text: "ok".into() }],
                    usage: Some(TokenUsage {
                        input_tokens: 40,
                        output_tokens: 4,
                        cache_read_tokens: None,
                        reasoning_tokens: None,
                    }),
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                    first_token_time: None,
                },
            ),
            envelope(7, SessionEvent::StepEnd { turn: 1, step: 2 }),
            envelope(
                8,
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: denia_core::session::TurnEndReason::Completed,
                },
            ),
        ];
        let usage = derive_turn_token_usage(&events).expect("turn still folds");
        assert_eq!(usage.uncached_input_tokens, 40);
        assert_eq!(usage.output_tokens, 4);
    }

    #[test]
    fn derive_turn_token_usage_sums_every_sample_of_one_step() {
        // 同一个 step 里流中断后重发的尝试:两次都付了费,不能只留最后一个。
        let sample = |input: u64| SessionEvent::AssistantMessage {
            turn: 1,
            step: 1,
            blocks: vec![ContentBlock::Text { text: "x".into() }],
            usage: Some(TokenUsage {
                input_tokens: input,
                output_tokens: 1,
                cache_read_tokens: None,
                reasoning_tokens: None,
            }),
            interrupted: false,
            source_event_seqs: Vec::new(),
            first_token_time: None,
        };
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(2, SessionEvent::StepStart { turn: 1, step: 1 }),
            envelope(3, sample(100)),
            envelope(4, sample(30)),
            envelope(5, SessionEvent::StepEnd { turn: 1, step: 1 }),
            envelope(
                6,
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: denia_core::session::TurnEndReason::Completed,
                },
            ),
        ];
        let usage = derive_turn_token_usage(&events).expect("turn should fold");
        assert_eq!(usage.uncached_input_tokens, 130);
        assert_eq!(usage.output_tokens, 2);
    }

    #[test]
    fn accounted_usage_events_land_in_the_books_without_touching_the_surface() {
        // 压缩请求的用量:进 turn 账本,不进模型面(它没有模型可见的回复)。
        let mut meter = ContextMeter::new();
        meter.apply_one(&envelope(1, SessionEvent::TurnStart { turn: 1 }));
        meter.apply_one(&envelope(2, SessionEvent::StepStart { turn: 1, step: 1 }));
        let surface_before = meter.surface_tokens();
        let accounted = AccountedUsage {
            uncached_input_tokens: 900,
            output_tokens: 120,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            estimated: false,
        };
        meter.apply_one(&envelope(3, accounted.event(1, 1)));
        assert_eq!(meter.turn_usage().uncached_input_tokens, 900);
        assert_eq!(meter.turn_usage().output_tokens, 120);
        assert_eq!(meter.surface_tokens(), surface_before, "账目标注不进表面");
        assert_eq!(meter.estimated_steps(), 0);
        // 缺 usage 的保守估算:同样入账,但被标注。
        let estimated = AccountedUsage::estimated_only(400, 50);
        meter.apply_one(&envelope(4, estimated.event(1, 1)));
        assert_eq!(meter.turn_usage().uncached_input_tokens, 1_300);
        assert_eq!(meter.estimated_steps(), 1);
        assert_eq!(meter.surface_tokens(), surface_before);
        // 真实重试轨迹(attempt ≥ 1)不是账目标注。
        meter.apply_one(&envelope(
            5,
            SessionEvent::RetryAttempt {
                turn: 1,
                step: 1,
                attempt: 1,
                code: "EMPTY_RESPONSE".into(),
                message: "x".into(),
                delay_ms: 10,
            },
        ));
        assert_eq!(meter.turn_usage().uncached_input_tokens, 1_300);
    }

    #[test]
    fn tool_result_replacement_shrinks_surface_tokens() {
        let mut meter = ContextMeter::new();
        let long = "x".repeat(4_000);
        let short = "pruned";
        let user_tokens = estimate_message(&ChatMessage::user("hi"));
        let long_tokens = estimate_message(&ChatMessage::tool_result("c", &long));
        let short_tokens = estimate_message(&ChatMessage::tool_result("c", short));

        meter.apply_one(&envelope(
            1,
            SessionEvent::UserMessage {
                text: "hi".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            },
        ));
        meter.apply_one(&envelope(
            2,
            SessionEvent::ToolResult {
                turn: 1,
                step: 1,
                call_id: "c".into(),
                content: long,
                is_error: false,
                error: None,
                error_identity: None,
                meta: None,
                replaces: None,
                truncation: None,
            },
        ));
        assert_eq!(meter.breakdown().message_tokens, user_tokens + long_tokens);

        meter.apply_one(&envelope(
            3,
            SessionEvent::ToolResult {
                turn: 1,
                step: 1,
                call_id: "c".into(),
                content: short.into(),
                is_error: false,
                error: None,
                error_identity: None,
                meta: None,
                replaces: Some(2),
                truncation: None,
            },
        ));
        let after = meter.breakdown().message_tokens;
        assert_eq!(after, user_tokens + short_tokens);
        assert!(after < user_tokens + long_tokens);
    }

    #[test]
    fn pressure_anchor_uses_last_usage() {
        let mut meter = ContextMeter::new();
        // 模拟 turn 1 完整回放(锚点由 apply_one 在 usage 样本处设置)。
        let events = vec![
            envelope(1, SessionEvent::TurnStart { turn: 1 }),
            envelope(2, SessionEvent::StepStart { turn: 1, step: 1 }),
            envelope(
                3,
                SessionEvent::AssistantMessage {
                    turn: 1,
                    step: 1,
                    blocks: vec![ContentBlock::Text { text: "ok".into() }],
                    usage: Some(TokenUsage {
                        input_tokens: 100,
                        output_tokens: 5,
                        cache_read_tokens: Some(20),
                        reasoning_tokens: None,
                    }),
                    interrupted: false,
                    source_event_seqs: Vec::new(),
                    first_token_time: None,
                },
            ),
            envelope(4, SessionEvent::StepEnd { turn: 1, step: 1 }),
            envelope(
                5,
                SessionEvent::TurnEnd {
                    turn: 1,
                    reason: denia_core::session::TurnEndReason::Completed,
                },
            ),
        ];
        meter.fold(&events);
        // 锚点 = input + cache_read = 120;锚点在本消息入表前盖章,所以
        // projected = 120 + 本消息启发式(dsh 语义:回答下一次请求规模)。
        // breakdown 三数按锚点校准后,整数和恒等于 projected。
        let p = meter.context_pressure();
        assert_eq!(p.pressure_tokens, Some(120));
        let b = meter.breakdown();
        assert_eq!(p.projected_tokens, Some(125));
        assert_eq!(
            b.system_tokens + b.tools_tokens + b.message_tokens,
            p.projected_tokens.unwrap()
        );
        // 锚点之后多发一条 user 消息:projected 继续增长,锚点不动。
        meter.apply_one(&envelope(
            6,
            SessionEvent::UserMessage {
                text: "再来".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            },
        ));
        let p2 = meter.context_pressure();
        assert_eq!(p2.pressure_tokens, Some(120));
        assert!(p2.projected_tokens.unwrap() > p.projected_tokens.unwrap());
        // 校准后的 breakdown 依然与 projected 严格相等(三数之和)。
        let b2 = meter.breakdown();
        assert_eq!(
            b2.system_tokens + b2.tools_tokens + b2.message_tokens,
            p2.projected_tokens.unwrap()
        );
    }

    #[test]
    fn pressure_projection_falls_after_surface_shrinks() {
        // 回归:微压缩/摘要把旧工具结果从表面移除后,腾出的空间必须反映到
        // 压力投影上。此前漂移用 `saturating_sub` 计算,负增量被截成 0,
        // 已经腾出空间的会话仍会被判定为超压、进而触发有信息损失的摘要。
        let mut meter = ContextMeter::new();
        meter.apply_one(&envelope(
            1,
            SessionEvent::UserMessage {
                text: "开始".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            },
        ));
        meter.apply_one(&envelope(
            2,
            SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks: vec![ContentBlock::Text { text: "ok".into() }],
                usage: Some(TokenUsage {
                    input_tokens: 100_000,
                    output_tokens: 5,
                    cache_read_tokens: Some(0),
                    reasoning_tokens: None,
                }),
                interrupted: false,
                source_event_seqs: Vec::new(),
                first_token_time: None,
            },
        ));
        // 约 10k tokens 的旧工具结果(role + ceil(content/4))。
        meter.apply_one(&envelope(
            3,
            SessionEvent::ToolResult {
                turn: 1,
                step: 1,
                call_id: "call_1".into(),
                content: "x".repeat(40_000),
                is_error: false,
                error: None,
                error_identity: None,
                meta: None,
                truncation: None,
                replaces: None,
            },
        ));
        let peak = meter.context_pressure().projected_tokens.unwrap();
        assert!(peak > 110_000, "锚点后应计入大工具结果: {peak}");
        // 微压缩:同 seq 原位替换为占位符(内容不再进模型)。
        meter.apply_one(&envelope(
            4,
            SessionEvent::ToolResult {
                turn: 1,
                step: 2,
                call_id: "call_1".into(),
                content: "[旧工具结果内容已清理]".into(),
                is_error: false,
                error: None,
                error_identity: None,
                meta: None,
                truncation: None,
                replaces: Some(3),
            },
        ));
        let after = meter.context_pressure().projected_tokens.unwrap();
        assert!(
            after < peak - 9_000,
            "清理腾出的空间必须从投影里回落: peak={peak} after={after}"
        );
        // 校准后的三数与投影仍然严格相等。
        let b = meter.breakdown();
        assert_eq!(b.system_tokens + b.tools_tokens + b.message_tokens, after);
    }

    #[test]
    fn pressure_projection_tracks_system_and_tools_changes() {
        // 回归:系统提示与工具声明在锚点之后的变化同样进入投影口径,
        // 否则它们的变化会被错误地记到对话消息行上。
        let mut meter = ContextMeter::new();
        meter.set_system_tokens(1_000);
        meter.set_tools_tokens(500);
        meter.apply_one(&envelope(
            1,
            SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks: vec![ContentBlock::Text { text: "ok".into() }],
                usage: Some(TokenUsage {
                    input_tokens: 2_000,
                    output_tokens: 5,
                    cache_read_tokens: Some(0),
                    reasoning_tokens: None,
                }),
                interrupted: false,
                source_event_seqs: Vec::new(),
                first_token_time: None,
            },
        ));
        let base = meter.context_pressure().projected_tokens.unwrap();
        meter.set_system_tokens(3_000);
        assert_eq!(
            meter.context_pressure().projected_tokens.unwrap(),
            base + 2_000
        );
        meter.set_tools_tokens(100);
        assert_eq!(
            meter.context_pressure().projected_tokens.unwrap(),
            base + 2_000 - 400
        );
    }

    #[test]
    fn pressure_without_anchor_has_no_fallback() {
        // 无 usage 样本:没有 pressureTokens,不做启发式兜底(dsh 语义,
        // provider 没报数就不显示占用)。
        let mut meter = ContextMeter::new();
        meter.set_system_tokens(100);
        meter.set_tools_tokens(50);
        meter.apply_one(&envelope(
            1,
            SessionEvent::UserMessage {
                text: "hi".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            },
        ));
        let p = meter.context_pressure();
        assert_eq!(p.pressure_tokens, None);
        assert_eq!(p.projected_tokens, None);
        assert_eq!(p.context_window, None);
        // 纯启发式拆分照常可读;message = 表面运行总量(不含 system/tools)。
        let b = meter.breakdown();
        assert_eq!(b.system_tokens, 100);
        assert_eq!(b.tools_tokens, 50);
        // role 4 + text ceil(2/4) 1 = 5。
        assert_eq!(b.message_tokens, 5);
    }

    #[test]
    fn request_context_supplies_window_last_wins() {
        let mut meter = ContextMeter::new();
        meter.apply_one(&envelope(
            1,
            SessionEvent::RequestContext {
                turn: 1,
                step: 1,
                provider: "deepseek".into(),
                model: "deepseek-chat".into(),
                context_window: Some(128_000),
            },
        ));
        assert_eq!(meter.context_pressure().context_window, Some(128_000));
        // 未通告的新记录移除容量(dsh 同语义)。
        meter.apply_one(&envelope(
            2,
            SessionEvent::RequestContext {
                turn: 1,
                step: 2,
                provider: "deepseek".into(),
                model: "deepseek-chat".into(),
                context_window: None,
            },
        ));
        assert_eq!(meter.context_pressure().context_window, None);
    }

    #[test]
    fn projected_tokens_answers_next_request() {
        // 锚点后发消息,projected = 锚点 + 锚点后表面增量;换模型重锚后,
        // 以新锚点为基准。
        let mut meter = ContextMeter::new();
        meter.apply_one(&envelope(
            1,
            SessionEvent::UserMessage {
                text: "hi".into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            },
        ));
        meter.apply_one(&envelope(
            2,
            SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks: vec![ContentBlock::Text { text: "ok".into() }],
                usage: Some(TokenUsage {
                    input_tokens: 1_000,
                    output_tokens: 5,
                    cache_read_tokens: Some(0),
                    reasoning_tokens: None,
                }),
                interrupted: false,
                source_event_seqs: Vec::new(),
                first_token_time: None,
            },
        ));
        let p = meter.context_pressure();
        assert_eq!(p.pressure_tokens, Some(1_000));
        // sampled = 锚点前表面(只有 user "hi"),projected = 1000 + assistant。
        let user_tokens = estimate_message(&ChatMessage::user("hi"));
        let assistant_tokens = estimate_message(&ChatMessage::assistant("ok", None, Vec::new()));
        assert_eq!(p.projected_tokens, Some(1_000 + assistant_tokens));
        // 原始表面 = user + assistant;校准后三数之和 == projected。
        assert_eq!(user_tokens + assistant_tokens, 10);
        let b = meter.breakdown();
        assert_eq!(
            b.system_tokens + b.tools_tokens + b.message_tokens,
            p.projected_tokens.unwrap()
        );
    }

    #[test]
    fn estimate_text_uses_character_classes() {
        // ASCII 仍按 4 字符 1 token,CJK 每字 1 token(全角标点计入 CJK)。
        assert_eq!(estimate_text("hello"), 2);
        assert_eq!(estimate_text("你好"), 2);
        assert_eq!(estimate_text("你好，世界"), 5);
        assert_eq!(estimate_system_tokens("上下文计量"), 9); // 5 字 + 4 角色框
    }

    #[test]
    fn calibrate_breakdown_balances_message_on_top_of_fixed_cost() {
        // 常态:固定行(系统/工具)原样,余额全部归对话消息行。
        assert_eq!(
            calibrate_breakdown(10, 20, 70, 200),
            ContextBreakdown {
                system_tokens: 10,
                tools_tokens: 20,
                message_tokens: 170
            }
        );
        assert_eq!(
            calibrate_breakdown(10, 20, 70, 100),
            ContextBreakdown {
                system_tokens: 10,
                tools_tokens: 20,
                message_tokens: 70
            }
        );
        assert_eq!(
            calibrate_breakdown(10, 20, 70, 90),
            ContextBreakdown {
                system_tokens: 10,
                tools_tokens: 20,
                message_tokens: 60
            }
        );
        // 退化:target 低于固定成本之和(现实几乎不可能),按占比缩放
        // 仍保证整数和恰好 == target。
        assert_eq!(
            calibrate_breakdown(10, 20, 70, 25),
            ContextBreakdown {
                system_tokens: 3,
                tools_tokens: 5,
                message_tokens: 17
            }
        );
        // 全零份额:target 全额给 message 兜底。
        assert_eq!(
            calibrate_breakdown(0, 0, 0, 5),
            ContextBreakdown {
                system_tokens: 0,
                tools_tokens: 0,
                message_tokens: 5
            }
        );
        assert_eq!(
            calibrate_breakdown(1, 2, 3, 0),
            ContextBreakdown {
                system_tokens: 0,
                tools_tokens: 0,
                message_tokens: 0
            }
        );
    }

    // —— 与派生面(`derive_surface`)同口径的对账 ——
    //
    // 占用估算与模型面必须是同一份投影。下面每条测试都拿 `derive_surface`
    // 的派生结果当“标准答案”,分别钉住曾经分叉的四处口径(a~e)与侦察之外
    // 发现的第五处(f)。断言的是口径,不是实现细节。

    /// 派生面的启发式总量:与 `ContextMeter::surface_tokens` 对账的期望值。
    fn derived_surface_tokens(events: &[SessionEnvelope]) -> u64 {
        denia_core::session::derive_surface(events)
            .iter()
            .fold(0u64, |acc, item| {
                acc.saturating_add(estimate_message(&item.message))
            })
    }

    /// 逐事件 fold,每一步都断言 meter 与派生面同口径,返回折完的 meter。
    ///
    /// 逐**前缀**对账很关键:表面积分是增量的,只在末尾对账会漏掉中途的
    /// 多算/少算(它们可能恰好互相抵消)。
    fn fold_matching_surface(events: &[SessionEnvelope]) -> ContextMeter {
        let mut meter = ContextMeter::new();
        for (index, envelope) in events.iter().enumerate() {
            meter.apply_one(envelope);
            assert_eq!(
                meter.surface_tokens(),
                derived_surface_tokens(&events[..=index]),
                "第 {index} 条(seq {})之后表面与派生面分叉",
                envelope.seq
            );
        }
        meter
    }

    fn user(seq: u64, text: &str) -> SessionEnvelope {
        envelope(
            seq,
            SessionEvent::UserMessage {
                text: text.into(),
                injected: false,
                images: Vec::new(),
                channel: None,
            },
        )
    }

    /// 一条注入消息(`channel: None` = 旧日志里靠文本前缀识别的注入块)。
    fn injection(seq: u64, channel: Option<&str>, text: &str) -> SessionEnvelope {
        envelope(
            seq,
            SessionEvent::UserMessage {
                text: text.into(),
                injected: true,
                channel: channel.map(str::to_owned),
                images: Vec::new(),
            },
        )
    }

    fn image(data: &str) -> Vec<ImageData> {
        vec![ImageData {
            mime: "image/png".into(),
            data: data.into(),
            path: None,
        }]
    }

    /// 一条带工具调用的 assistant 消息的 blocks(与测试里的期望值共用一份,
    /// 避免两处构造出不同的消息)。
    fn tool_call_blocks(call_id: &str, name: &str, arguments: &str) -> Vec<ContentBlock> {
        vec![
            ContentBlock::Text {
                text: "动手".into(),
            },
            ContentBlock::ToolCall {
                id: call_id.into(),
                name: name.into(),
                arguments: arguments.into(),
                incomplete: false,
            },
        ]
    }

    fn assistant_with_call(seq: u64, blocks: Vec<ContentBlock>) -> SessionEnvelope {
        envelope(
            seq,
            SessionEvent::AssistantMessage {
                turn: 1,
                step: 1,
                blocks,
                usage: None,
                interrupted: false,
                source_event_seqs: Vec::new(),
                first_token_time: None,
            },
        )
    }

    fn tool_result(
        seq: u64,
        call_id: &str,
        content: &str,
        replaces: Option<u64>,
    ) -> SessionEnvelope {
        envelope(
            seq,
            SessionEvent::ToolResult {
                turn: 1,
                step: 1,
                call_id: call_id.into(),
                content: content.into(),
                is_error: false,
                error: None,
                error_identity: None,
                meta: None,
                replaces,
                truncation: None,
            },
        )
    }

    fn summary(seq: u64, text: &str, from: u64, to: u64, keep_from: u64) -> SessionEnvelope {
        envelope(
            seq,
            SessionEvent::CompactionSummary {
                turn: 1,
                step: 1,
                summary: text.into(),
                replaces_from: from,
                replaces_to: to,
                keep_from,
                pre_tokens: 1,
                post_tokens: 1,
            },
        )
    }

    #[test]
    fn hidden_events_leave_the_surface_but_keep_the_window() {
        // 差异 a(单元级口径):被历史投影排除的事件不进表面计量——它们不在
        // 模型面上,算进去就是拿“模型看不见的历史”去推高占用与压缩闸门。
        // 例外是路由容量:`request/context` 不是历史。
        let mut meter = ContextMeter::new();
        meter.apply_one_projected(&user(1, "注入的运行时上下文"), false);
        assert_eq!(meter.surface_tokens(), 0, "被排除的事件不得进表面");
        meter.apply_one_projected(
            &envelope(
                2,
                SessionEvent::RequestContext {
                    turn: 1,
                    step: 1,
                    provider: "p".into(),
                    model: "m".into(),
                    context_window: Some(64_000),
                },
            ),
            false,
        );
        assert_eq!(
            meter.context_pressure().context_window,
            Some(64_000),
            "上下文窗口是路由容量而不是历史:被投影过滤掉时仍要生效"
        );
        // 同一事件模型可见时照常计价。
        meter.apply_one_projected(&user(3, "注入的运行时上下文"), true);
        assert_eq!(
            meter.surface_tokens(),
            estimate_message(&ChatMessage::user("注入的运行时上下文"))
        );
    }

    #[test]
    fn meter_surface_matches_derived_surface_when_args_cleared() {
        // 差异 c:超长参数打桩(`ArgsCleared`)在模型面上把 tool_call 的
        // arguments 原位换成占位符。meter 若继续按原始参数计价,就是**高估**
        // ——而且高估量恰好是模型永远看不到的那份死重。
        let long_args = format!(
            "{{\"path\":\"a.rs\",\"content\":\"{}\"}}",
            "x".repeat(8_000)
        );
        let placeholder = "[参数已打桩:write_file a.rs 8KB]";
        let mut events = vec![
            user(1, "把这段写进文件"),
            assistant_with_call(2, tool_call_blocks("call_w", "write_file", &long_args)),
            tool_result(3, "call_w", "已写入", None),
        ];
        let before = fold_matching_surface(&events).surface_tokens();
        events.push(envelope(
            4,
            SessionEvent::ArgsCleared {
                turn: 1,
                step: 1,
                call_id: "call_w".into(),
                placeholder: placeholder.into(),
            },
        ));
        let after = fold_matching_surface(&events).surface_tokens();
        assert!(
            after < before,
            "打桩必须让表面变小: before={before} after={after}"
        );
        // 差额恰好是那一份 arguments 的估算差(口径断言,不是实现细节)。
        assert_eq!(
            before - after,
            estimate_text(&long_args) - estimate_text(placeholder)
        );
    }

    #[test]
    fn meter_surface_matches_derived_surface_for_unanswered_tool_call() {
        // 差异 d:日志末尾未应答的工具调用,模型面会补一条合成中断结果
        // (每个 tool_call 都必须有 tool_result)。meter 若不算它就是**低估**,
        // 而低估正好发生在崩溃/中断之后——最需要压一压的时候。
        let events = vec![
            user(1, "看看这个文件"),
            assistant_with_call(
                2,
                tool_call_blocks("call_x", "read_file", "{\"path\":\"a.rs\"}"),
            ),
        ];
        let pending = fold_matching_surface(&events).surface_tokens();
        let synthetic =
            estimate_message(&ChatMessage::tool_result("call_x", INTERRUPTED_TOOL_RESULT));
        assert!(synthetic > 0, "合成中断结果本身有角色框开销");
        // 未应答期间,那条合成结果就占着模型面。
        assert_eq!(
            pending,
            estimate_message(&ChatMessage::user("看看这个文件"))
                + estimate_message(
                    &denia_core::message::assistant_from_blocks(&tool_call_blocks(
                        "call_x",
                        "read_file",
                        "{\"path\":\"a.rs\"}"
                    ))
                    .unwrap()
                )
                + synthetic
        );
        // 应答之后:合成结果换成真实结果,不是两份都算。
        let mut answered = events;
        answered.push(tool_result(3, "call_x", "文件内容", None));
        let after = fold_matching_surface(&answered).surface_tokens();
        let real = estimate_message(&ChatMessage::tool_result("call_x", "文件内容"));
        assert_eq!(
            after as i64 - pending as i64,
            real as i64 - synthetic as i64,
            "应答后合成中断结果必须被真实结果替换(净变化 = 真实结果 - 合成结果)"
        );
    }

    #[test]
    fn meter_surface_matches_derived_surface_for_tool_result_images() {
        // 差异 e:等工具结果期间注入的带图用户消息(截图)在模型面上**并进**
        // 紧随的 tool-result(provider 要求 tool_call 与 tool_result 相邻),
        // 不是单独一条 user;meter 若一律按纯文本算,合并文案与图片全都不计。
        let images = image(&"QUJD".repeat(200));
        let blocks = tool_call_blocks("call_shot", "browser", "{\"action\":\"screenshot\"}");
        let events = vec![
            user(1, "打开页面"),
            assistant_with_call(2, blocks.clone()),
            envelope(
                3,
                SessionEvent::UserMessage {
                    text: "看这张截图".into(),
                    injected: false,
                    images: images.clone(),
                    channel: None,
                },
            ),
            tool_result(4, "call_shot", "已截图", None),
            envelope(
                5,
                SessionEvent::UserMessage {
                    text: "再看这张".into(),
                    injected: false,
                    images: images.clone(),
                    channel: None,
                },
            ),
        ];
        let meter = fold_matching_surface(&events);
        // 表面逐条对出来:等结果期间那条注图消息**不进表面**(它的文字也不进),
        // 图并进紧随的 tool-result;工具跑完之后发的图是正常的带图用户消息。
        let mut merged = ChatMessage::tool_result("call_shot", format!("已截图{TOOL_IMAGE_NOTE}"));
        merged.images = images.clone();
        let expected = estimate_message(&ChatMessage::user("打开页面"))
            + estimate_message(&denia_core::message::assistant_from_blocks(&blocks).unwrap())
            + estimate_message(&merged)
            + estimate_message(&ChatMessage::user_with_images("再看这张", images.clone()));
        assert_eq!(meter.surface_tokens(), expected);
        // 旧口径(一律纯文本、不并图)确实给出另一个数——否则这条测试什么
        // 也没钉住。
        let text_only = estimate_message(&ChatMessage::user("打开页面"))
            + estimate_message(&denia_core::message::assistant_from_blocks(&blocks).unwrap())
            + estimate_message(&ChatMessage::user("看这张截图"))
            + estimate_message(&ChatMessage::tool_result("call_shot", "已截图"))
            + estimate_message(&ChatMessage::user("再看这张"));
        assert_ne!(
            meter.surface_tokens(),
            text_only,
            "图片与合并文案必须进计量"
        );
    }

    #[test]
    fn meter_surface_matches_derived_surface_across_two_compactions() {
        // 差异 b:摘要节点在模型面上定位在 `keep_from - 1`(排在保留窗口
        // 之前),不是压缩事件自己的 seq。第二次压缩的区间若覆盖了这个定位
        // seq,旧摘要必须跟着一起被淘汰——meter 按事件 seq 记账就会留下旧
        // 摘要:与模型面同时看到两份互相矛盾的摘要,还多算一份 token。
        //
        // 布局特意让"第一次压缩的事件 seq"(10)落在第二次区间(3..=8)之外,
        // 而它的定位 seq(3)落在区间之内:这正是两种口径给出不同结果的形状。
        let events = vec![
            user(1, "一"),
            user(2, "二"),
            user(3, "三"),
            user(4, "四"),
            user(5, "五"),
            user(6, "六"),
            user(7, "七"),
            user(8, "八"),
            user(9, "九"),
            summary(10, "第一份摘要", 1, 3, 4),
            summary(11, "第二份摘要", 3, 8, 9),
            user(12, "十"),
        ];
        let meter = fold_matching_surface(&events);
        // 第二次压缩后表面 = 新摘要(定位 8 = keep_from - 1)+ 保留窗口。
        assert_eq!(
            meter.surface_tokens(),
            estimate_message(&ChatMessage::user("第二份摘要"))
                + estimate_message(&ChatMessage::user("九"))
                + estimate_message(&ChatMessage::user("十"))
        );
    }

    #[test]
    fn meter_surface_matches_derived_surface_for_chained_result_replacement() {
        // 差异 f(侦察之外发现):带 `replaces` 的结果在模型面上是**原位**替换
        // (节点保持原 seq)。meter 若把新节点落在新事件 seq 上,第二次替换
        // 同一个目标时就找不到旧节点、扣减漏掉——表面越算越大。
        let events = vec![
            user(1, "跑一下"),
            tool_result(2, "call_r", &"x".repeat(4_000), None),
            tool_result(3, "call_r", "[旧工具结果内容已清理]", Some(2)),
            tool_result(4, "call_r", "[更短的占位]", Some(2)),
        ];
        let meter = fold_matching_surface(&events);
        assert_eq!(
            meter.surface_tokens(),
            estimate_message(&ChatMessage::user("跑一下"))
                + estimate_message(&ChatMessage::tool_result("call_r", "[更短的占位]"))
        );
    }

    #[test]
    fn meter_surface_matches_derived_surface_across_clears_results_and_compaction() {
        // 跨口径回归:一节会话里同时出现参数打桩(c)、链式原位替换(f)、
        // 未应答调用(d)与摘要压缩(b);表面是增量的,每一步都必须与模型面
        // 同口径(`fold_matching_surface` 逐前缀对账)。
        let long_args = format!(
            "{{\"path\":\"a.rs\",\"content\":\"{}\"}}",
            "y".repeat(6_000)
        );
        let events = vec![
            user(1, "起手"),
            assistant_with_call(2, tool_call_blocks("call_a", "write_file", &long_args)),
            tool_result(3, "call_a", "写入完成", None),
            envelope(
                4,
                SessionEvent::ArgsCleared {
                    turn: 1,
                    step: 1,
                    call_id: "call_a".into(),
                    placeholder: "[已打桩]".into(),
                },
            ),
            tool_result(5, "call_a", "写入完成(清理后的占位)", Some(3)),
            tool_result(6, "call_a", "[旧工具结果内容已清理]", Some(5)),
            assistant_with_call(
                7,
                tool_call_blocks("call_b", "bash", "{\"command\":\"ls\"}"),
            ),
            user(8, "先停一下"),
            summary(9, "摘要", 1, 6, 7),
        ];
        let meter = fold_matching_surface(&events);
        // 压缩把 call_a 那条 assistant 与它的结果节点折进摘要;call_b 的合成
        // 中断结果与保留窗口仍在表面(未应答的调用不因压缩而消失)。
        let call_b = denia_core::message::assistant_from_blocks(&tool_call_blocks(
            "call_b",
            "bash",
            "{\"command\":\"ls\"}",
        ))
        .unwrap();
        assert_eq!(
            meter.surface_tokens(),
            estimate_message(&ChatMessage::user("摘要"))
                + estimate_message(&call_b)
                + estimate_message(&ChatMessage::user("先停一下"))
                + estimate_message(&ChatMessage::tool_result("call_b", INTERRUPTED_TOOL_RESULT))
        );
    }

    #[test]
    fn meter_surface_matches_derived_surface_across_folded_injections() {
        // 基线通道的旧版本从模型面退出:meter 若继续累加它们,表面就比模型面
        // 大——而且差值随刷新次数线性增长。`fold_matching_surface` 是**逐
        // 前缀**对账,中途任何一轮多算/少算都会立刻暴露(不会等到末尾恰好
        // 互相抵消)。
        let events = vec![
            injection(1, Some("capability"), "能力 v1"),
            injection(2, Some("workspace-instructions"), "工作区 v1"),
            user(3, "干活"),
            injection(4, Some("capability"), "能力 v2"),
            // 旧日志(无通道字段)的注入块按前缀归到同一通道,同样折叠。
            injection(5, None, "<system-reminder>\n工作区指令:旧版"),
            // 逐条注入通道(图片、通知、纠错)不折叠:每条都是独立事实。
            injection(6, Some("image"), "[harness] 读取图片:a.png"),
            injection(7, Some("image"), "[harness] 读取图片:b.png"),
        ];
        let meter = fold_matching_surface(&events);
        assert_eq!(
            meter.surface_tokens(),
            estimate_message(&ChatMessage::user("干活"))
                + estimate_message(&ChatMessage::user("能力 v2"))
                + estimate_message(&ChatMessage::user("<system-reminder>\n工作区指令:旧版"))
                + estimate_message(&ChatMessage::user("[harness] 读取图片:a.png"))
                + estimate_message(&ChatMessage::user("[harness] 读取图片:b.png"))
        );
    }

    #[test]
    fn folded_injection_after_compaction_does_not_double_subtract() {
        // 压缩把通道的当前节点撤出表面后,同一通道又出现新版本:meter 不能再
        // 扣一次已经撤下的旧节点(否则表面越算越小)。
        // 布局特意让摘要占住被撤下的那个 seq(keep_from - 1 = 2),把"通道
        // 定位表没跟着压缩一起清理"的形状逼出来。
        let events = vec![
            user(1, "起手"),
            injection(2, Some("runtime-context"), "上下文 v1"),
            user(3, "二"),
            summary(4, "摘要", 1, 2, 3),
            injection(5, Some("runtime-context"), "上下文 v2"),
        ];
        let meter = fold_matching_surface(&events);
        assert_eq!(
            meter.surface_tokens(),
            estimate_message(&ChatMessage::user("摘要"))
                + estimate_message(&ChatMessage::user("二"))
                + estimate_message(&ChatMessage::user("上下文 v2"))
        );
    }
}
