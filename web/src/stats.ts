import type { TranscriptNode } from './fold'
import type { SessionEnvelope, SessionTotals, TokenUsage } from './types'

/**
 * 会话级统计:从 transcript 节点折叠出的展示总量。
 * 抄 dsh StatsLine 的 deriveStats —— 字段名对齐,方便后续接 projection。
 */
export interface SessionStats {
  /** 已关闭的轮次数(有 turn-end 的 turn)。 */
  turns: number
  /** assistant 步数(已 settle 的 assistant-message)。 */
  steps: number
  /** 工具调用总次数。 */
  toolCalls: number
  /** 各轮墙钟时长之和(turn-start → turn-end),毫秒。 */
  turnMs: number
  /** 累计未缓存输入 token(与缓存读互斥,见 core `TokenUsage`)。 */
  inputTokens: number
  /** 累计输出 token。 */
  outputTokens: number
  /** 累计缓存命中 token。 */
  cacheReadTokens: number
  /** 累计推理 token。 */
  reasoningTokens: number
  /** 平均首 token 延迟(step-start → 首个 token 帧),毫秒;无数据为 null。 */
  avgTtftMs: number | null
  /** 解码吞吐:output tokens / decode 墙钟秒;无数据为 null。 */
  tokensPerSecond: number | null
}

/** 从 transcript 节点折叠会话统计;空会话返回零值。 */
export function deriveStats(nodes: TranscriptNode[]): SessionStats {
  const turns = new Set<number>()
  let steps = 0
  let toolCalls = 0
  let turnMs = 0
  let inputTokens = 0
  let outputTokens = 0
  let cacheReadTokens = 0
  let reasoningTokens = 0

  // turn-start 时间表,配对 turn-end 求墙钟。
  const turnStart = new Map<number, number>()

  // TTFT / 吞吐累计。
  let ttftSum = 0
  let ttftCount = 0
  let decodeMs = 0
  let decodeTokens = 0

  const addUsage = (usage: TokenUsage | undefined) => {
    if (!usage) return
    inputTokens += usage.inputTokens
    outputTokens += usage.outputTokens
    cacheReadTokens += usage.cacheReadTokens ?? 0
    reasoningTokens += usage.reasoningTokens ?? 0
  }

  for (const node of nodes) {
    switch (node.kind) {
      case 'turn-start':
        turnStart.set(node.turn, node.time)
        break
      case 'turn-end': {
        turns.add(node.turn)
        const start = turnStart.get(node.turn)
        if (start !== undefined) turnMs += Math.max(0, node.time - start)
        break
      }
      case 'assistant':
        if (!node.streaming) {
          steps += 1
          addUsage(node.usage)
          // TTFT: step-start → 首个 token 帧(后端落盘的 first_token_time
          // 为权威;实时流由 chunk 推断补齐,口径一致)。
          if (node.stepStartTime !== undefined && node.firstTokenTime !== undefined) {
            ttftSum += Math.max(0, node.firstTokenTime - node.stepStartTime)
            ttftCount += 1
          }
          // 吞吐: 首 token → settle 之间的产出。usage.outputTokens 是本步
          // 全部输出;当推理内容没有流式回传(流里没有非空 reasoning 块)时,
          // usage 里报的 reasoning tokens 耗在首 token 之前的思考期,不在
          // 解码窗口内,要从分子剔除,否则速度被低估。
          if (node.firstTokenTime !== undefined && node.settleTime !== undefined && node.usage) {
            const ms = Math.max(0, node.settleTime - node.firstTokenTime)
            if (ms > 0) {
              const streamedReasoning = node.blocks.some(
                (block) => block.kind === 'reasoning' && /\S/.test(block.text),
              )
              const reasoning = streamedReasoning ? 0 : (node.usage.reasoningTokens ?? 0)
              const decoded = node.usage.outputTokens - reasoning
              if (decoded > 0) {
                decodeMs += ms
                decodeTokens += decoded
              }
            }
          }
        }
        break
      case 'tool':
        toolCalls += 1
        break
      default:
        break
    }
  }

  const avgTtftMs = ttftCount > 0 ? ttftSum / ttftCount : null
  const tokensPerSecond = decodeMs > 0 ? decodeTokens / (decodeMs / 1000) : null

  return { turns: turns.size, steps, toolCalls, turnMs, inputTokens, outputTokens, cacheReadTokens, reasoningTokens, avgTtftMs, tokensPerSecond }
}

/**
 * 全会话累计统计的客户端侧:服务端基准 + 实时增量。
 *
 * 上表 `deriveStats` 折的是**当前加载窗口**里的节点;窗口被首屏分页(尾部
 * N 条)、重连重快照、长会话实时裁剪(>600 条丢旧轮次)截断后,累计值就会
 * 缩水 —— 用户看到"耗时/轮次/token 忽大忽小"。所以累计字段改由服务端对
 * 整份日志投影(`SessionTotals`,见 Rust `denia_session::SessionTotals`),
 * 这里只维护"快照之后新到达的事件"那一小段增量;规则与 `deriveStats`
 * 及服务端投影逐项一致,三处口径必须同步改。
 *
 * 进行中的轮次不计入任何一侧(它没有 turn-end),由界面按当前时刻加实时值。
 */
export function emptyTotals(): SessionTotals {
  return {
    turnMs: 0,
    turns: 0,
    steps: 0,
    toolCalls: 0,
    inputTokens: 0,
    outputTokens: 0,
    cacheReadTokens: 0,
    reasoningTokens: 0,
  }
}

/** 分页响应里的累计统计:旧后端/缺字段按零值补齐,不编造。 */
export function totalsFromPage(totals: Partial<SessionTotals> | undefined): SessionTotals {
  return { ...emptyTotals(), ...totals }
}

/**
 * 增量累加器。`openTurns` 是未闭合轮次的起点表:每次快照用页面事件重建
 * (窗口里已闭合的轮次在服务端基准里,不进表);配不上起点的 turn-end
 * 直接跳过 —— 下一次快照的基准会把它补上,不会长期丢失。
 */
export interface TotalsAccumulator {
  totals: SessionTotals
  openTurns: Map<number, number>
}

/** 用快照基准重置累加器(首次挂载与每次重连重快照都会走)。 */
export function resetTotals(base: SessionTotals, events: SessionEnvelope[]): TotalsAccumulator {
  const openTurns = new Map<number, number>()
  for (const envelope of events) {
    if (envelope.type === 'turn-start') openTurns.set(envelope.turn, envelope.time)
    else if (envelope.type === 'turn-end') openTurns.delete(envelope.turn)
  }
  return { totals: { ...base }, openTurns }
}

/**
 * 折叠一条实时事件;返回是否改动了累计值(调用方据此决定要不要惊动 React)。
 */
export function applyTotalsEvent(state: TotalsAccumulator, envelope: SessionEnvelope): boolean {
  switch (envelope.type) {
    case 'turn-start':
      state.openTurns.set(envelope.turn, envelope.time)
      return false
    case 'turn-end': {
      const started = state.openTurns.get(envelope.turn)
      state.openTurns.delete(envelope.turn)
      if (started === undefined) return false
      state.totals.turns += 1
      state.totals.turnMs += Math.max(0, envelope.time - started)
      return true
    }
    case 'assistant-message':
      state.totals.steps += 1
      if (envelope.usage) {
        state.totals.inputTokens += envelope.usage.inputTokens
        state.totals.outputTokens += envelope.usage.outputTokens
        state.totals.cacheReadTokens += envelope.usage.cacheReadTokens ?? 0
        state.totals.reasoningTokens += envelope.usage.reasoningTokens ?? 0
      }
      return true
    case 'tool-call':
      state.totals.toolCalls += 1
      return true
    default:
      return false
  }
}

/** 紧凑时长:45.2s 不足一分钟,2m42s 之后。 */
export function formatCompactDuration(ms: number): string {
  const s = ms / 1000
  if (s < 60) return `${Math.round(s * 10) / 10}s`
  const whole = Math.round(s)
  return `${Math.floor(whole / 60)}m${whole % 60}s`
}

/** token 数紧凑显示:1234 → 1.2k,45678 → 45.7k,1234567 → 1.2m。 */
export function formatTokens(count: number): string {
  if (count < 1_000) return String(count)
  if (count < 1_000_000) return `${(count / 1_000).toFixed(1)}k`
  return `${(count / 1_000_000).toFixed(1)}m`
}

/**
 * 缓存命中率:缓存读 /(未缓存输入 + 缓存读)。
 *
 * 分母是**计费输入总量**。`TokenUsage` 的契约是 `cache_read_tokens` 与
 * `input_tokens` 互斥(`input_tokens` 只含未缓存部分),所以两者相加才是
 * 这次请求真正的 prompt 总量 —— 命中率 = 命中 / 总 prompt。
 *
 * 注意:这个公式只有在 `input_tokens` 确实是"未缓存"时才成立。曾经
 * OpenAI 系协议的映射漏了减法(`prompt_tokens` 是含缓存的总量),导致
 * `input_tokens` 变成总 prompt,于是这里的加法把分母算成了近两倍,
 * 命中率显示成真值的一半。口径的归一在 `crates/llm/src/wire.rs` 的
 * `map_usage`;那边有回归用例 `both_wire_flavors_normalize_to_uncached_input`。
 * 无任何计费输入返回 null。
 */
export function cacheHitPercent(inputTokens: number, cacheReadTokens: number): string | null {
  const billed = inputTokens + cacheReadTokens
  if (billed <= 0) return null
  const percent = (cacheReadTokens / billed) * 100
  return Math.min(100, percent).toFixed(2)
}

/** 计费输入总量 = 未缓存输入 + 缓存读(= 本次请求的 prompt 总量)。 */
export function billedInputTokens(inputTokens: number, cacheReadTokens: number): number {
  return inputTokens + cacheReadTokens
}
