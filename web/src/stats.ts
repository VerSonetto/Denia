import type { TranscriptNode } from './fold'
import type { TokenUsage } from './types'

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
