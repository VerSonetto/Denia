import * as api from './api'
import type { ModelSelection, SessionEnvelope } from './types'

/**
 * 提示词优化:用当前模型重写输入框里的提示词。
 *
 * 上下文取最近 5 轮对话里的用户与助手正文(含 harness 注入的文件/引用
 * 等上下文),工具调用/结果、系统提示词、纯过程事件一律不进优化请求。
 */

const OPTIMIZE_SYSTEM = `你是专业的提示词优化助手。
你的任务是根据用户提供的待优化提示词和最近对话上下文,输出一份优化后的提示词。
要求:
1. 保持原始意图,不改变用户目标。
2. 让描述更具体、完整、可执行,补齐必要的背景、约束与输出格式。
3. 结合最近对话上下文,避免与已讨论过的内容重复或冲突。
4. 直接输出优化后的提示词正文,不要解释、不要前缀、不要 Markdown 代码块、不要引号。`

export interface OptimizePromptOptions {
  sessionId: string
  prompt: string
  selection: ModelSelection
  signal?: AbortSignal
}

interface RoundLine {
  role: 'user' | 'assistant'
  text: string
}

interface RoundContext {
  turn: number
  lines: RoundLine[]
}

/** 从会话事件里抽出最近 5 轮的人话上下文,排除工具调用等过程消息。 */
export function buildRecentContext(events: SessionEnvelope[]): string {
  const rounds: RoundContext[] = []
  let pendingLines: RoundLine[] = []
  let current: RoundContext | null = null
  // turn-start 之后、turn-end 之前出现的 user-message 属于当前轮(运行时注入/
  // 纠错反馈);turn-end 之后到下一个 turn-start 之前的是下一轮用户提问。
  let openTurn = false

  for (const event of events) {
    if (event.type === 'user-message') {
      const line: RoundLine = { role: 'user', text: event.text }
      if (current && openTurn) current.lines.push(line)
      else pendingLines.push(line)
      continue
    }
    if (event.type === 'turn-start') {
      current = { turn: event.turn, lines: pendingLines }
      rounds.push(current)
      pendingLines = []
      openTurn = true
      continue
    }
    if (event.type === 'turn-end') {
      openTurn = false
      continue
    }
    if (event.type === 'assistant-message' && current && current.turn === event.turn) {
      const text = event.blocks
        .filter((block) => block.type === 'text')
        .map((block) => block.text)
        .join('\n')
      if (text.trim().length > 0) {
        current.lines.push({ role: 'assistant', text })
      }
    }
  }

  const recent = rounds.slice(-5)
  const parts: string[] = []
  for (const round of recent) {
    if (round.lines.length === 0) continue
    const lines = round.lines.map((line) => (
      `${line.role === 'user' ? '用户' : '助手'}：${line.text}`
    ))
    parts.push(lines.join('\n'))
  }
  return parts.join('\n\n')
}

/** 去掉模型偶尔残留的代码块/说明前缀,只留提示词正文。 */
export function cleanOptimizedPrompt(text: string): string {
  let out = text.trim()
  const fence = out.match(/^```(?:[a-z0-9]+)?\n?([\s\S]*?)\n?```$/i)
  if (fence) out = fence[1].trim()
  out = out.replace(/^(优化后的提示词|优化结果|优化后的内容)\s*[:：]\s*/i, '').trim()
  return out
}

/** 拉取会话上下文 + 调用当前模型,返回优化后的提示词正文。 */
export async function optimizePromptText(
  options: OptimizePromptOptions,
): Promise<string> {
  const { events } = await api.getSession(options.sessionId, options.signal)
  const context = buildRecentContext(events)
  const contextBlock = context
    ? `最近 5 轮对话上下文(已排除工具调用消息):\n\n${context}\n\n`
    : '当前对话暂时没有历史上下文。\n\n'
  const userContent =
    `${contextBlock}请优化以下提示词,结合上下文使其更准确、完整、可执行:\n\n` +
    options.prompt

  const result = await api.chatCompletion(
    {
      provider: options.selection.provider,
      model: options.selection.model,
      reasoningEffort: options.selection.reasoningEffort,
      system: OPTIMIZE_SYSTEM,
      messages: [{ role: 'user', content: userContent }],
    },
    options.signal,
  )

  const cleaned = cleanOptimizedPrompt(result)
  if (!cleaned) {
    throw new Error('优化结果为空,请重试')
  }
  return cleaned
}
