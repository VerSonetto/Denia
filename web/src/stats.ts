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
  /** 累计输入 token(含缓存读)。 */
  inputTokens: number
  /** 累计输出 token。 */
  outputTokens: number
  /** 累计缓存命中 token。 */
  cacheReadTokens: number
  /** 累计推理 token。 */
  reasoningTokens: number
  /** 平均首 token 延迟(step-start → 第一个 chunk),毫秒;无数据为 null。 */
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
          // TTFT: step-start → 第一个 chunk。
          if (node.stepStartTime !== undefined && node.firstChunkTime !== undefined) {
            ttftSum += Math.max(0, node.firstChunkTime - node.stepStartTime)
            ttftCount += 1
          }
          // 吞吐: 第一个 chunk → settle 之间的 output tokens。
          if (node.firstChunkTime !== undefined && node.settleTime !== undefined && node.usage) {
            const ms = Math.max(0, node.settleTime - node.firstChunkTime)
            if (ms > 0) {
              decodeMs += ms
              decodeTokens += node.usage.outputTokens
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

/** 缓存命中率:两位小数百分比;无输入返回 null。 */
export function cacheHitPercent(stats: SessionStats): string | null {
  if (stats.inputTokens <= 0) return null
  const percent = (stats.cacheReadTokens / stats.inputTokens) * 100
  return Math.min(100, percent).toFixed(2)
}
