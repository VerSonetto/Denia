/**
 * 按段落批量揭示正在流入的 markdown 正文。
 *
 * 流式正文只在达到一个段落/行边界时前进，并把未完成的代码围栏与表格
 * 留到下一批；生产端结束后立即 flush，历史正文则从第一帧直接全量显示。
 */

import { useEffect, useRef, useState } from 'react'

export const DEFAULT_PARAGRAPH_CHARS = 500
export const DEFAULT_POLL_MS = 200

export interface ParagraphRevealOptions {
  /** 流式状态下每批至少等待的目标字符数。 */
  minChars?: number
  /** 流式轮询间隔（毫秒）。 */
  pollMs?: number
}

export interface ParagraphReveal {
  /** 当前应显示的全文前缀。 */
  text: string
  /** 正文已不再生产，且显示位置已追平全文。 */
  done: boolean
}

/** 返回是否为 markdown 围栏行。 */
function isFenceLine(line: string): boolean {
  return /^\s*```/.test(line)
}

/** 返回是否为 markdown 表格行。 */
function isTableLine(line: string): boolean {
  return /^\s*\|/.test(line)
}

function walkLines(text: string, visit: (start: number, end: number, line: string) => void): void {
  let start = 0
  while (start <= text.length) {
    const newline = text.indexOf('\n', start)
    const end = newline === -1 ? text.length : newline
    visit(start, end, text.slice(start, end))
    if (newline === -1) break
    start = newline + 1
  }
}

/**
 * 把候选揭示位置推回到安全的 markdown 状态。
 *
 * 一个仍在增长的围栏或表格可能让尾部解析结果随下一块文本整体改写；在
 * 它们结束前不揭示其中的内容，避免 UI 在半个代码块/表格里断开。
 */
export function extendToSafeMarkdown(text: string, position: number, flush = false): number {
  if (flush || position >= text.length) return text.length

  let fenceFrom = -1
  let tableFrom = -1
  let inFence = false
  let inTable = false

  walkLines(text, (start, _end, line) => {
    if (isFenceLine(line)) {
      if (!inFence) {
        inFence = true
        fenceFrom = start
      } else {
        inFence = false
        fenceFrom = -1
      }
      inTable = false
      tableFrom = -1
      return
    }

    if (!inFence && isTableLine(line)) {
      if (!inTable) {
        inTable = true
        tableFrom = start
      }
    } else if (inTable && line.trim() === '') {
      inTable = false
      tableFrom = -1
    } else if (inTable && !isTableLine(line)) {
      inTable = false
      tableFrom = -1
    }
  })

  if (inFence) return fenceFrom >= 0 ? fenceFrom : position
  if (inTable) return tableFrom >= 0 ? tableFrom : position
  return position
}

/**
 * 计算下一批流式正文的目标位置。
 *
 * 优先选择字符阈值之后的空行，否则退到其后的普通换行；没有边界时保持
 * 原位置。`flush` 用于生产端结束，直接排空全部文本。
 */
export function paragraphTarget(
  text: string,
  shown: number,
  flush: boolean,
  minChars = DEFAULT_PARAGRAPH_CHARS,
): number {
  if (typeof text !== 'string') return 0
  if (flush) return text.length

  const safeShown = Math.max(0, Math.floor(shown))
  const need = safeShown + Math.max(0, Math.floor(minChars))
  if (text.length < need) return safeShown

  let position = -1
  const paragraphBreak = text.indexOf('\n\n', need)
  if (paragraphBreak !== -1) position = paragraphBreak + 2
  else {
    const newline = text.indexOf('\n', need)
    if (newline !== -1) position = newline + 1
  }
  if (position === -1) return safeShown

  const safe = extendToSafeMarkdown(text, position, false)
  return safe > safeShown ? safe : safeShown
}

/**
 * 将 UTF-16 截断点调整到完整代理对，避免 emoji/生僻字出现半个替代字符。
 */
function safeSlice(text: string, length: number): string {
  const target = Math.floor(length)
  if (target <= 0) return ''
  if (target >= text.length) return text
  const code = text.charCodeAt(target)
  if (code >= 0xd800 && code <= 0xdbff && target + 1 < text.length) return text.slice(0, target + 2)
  if (code >= 0xdc00 && code <= 0xdfff) return text.slice(0, target - 1)
  return text.slice(0, target)
}

/**
 * 按段落批量揭示 markdown 正文，替代逐字打字机。
 *
 * `text` 追加时保留已显示位置；目标不是旧文本前缀时从头开始。非流式
 * 文本不经过动画，因此历史回放和流结束都能立即显示完整正文。
 */
export function useParagraphReveal(
  text: string,
  streaming: boolean,
  options?: ParagraphRevealOptions,
): ParagraphReveal {
  const minChars = options?.minChars ?? DEFAULT_PARAGRAPH_CHARS
  const pollMs = options?.pollMs ?? DEFAULT_POLL_MS
  const targetRef = useRef<string | null>(null)
  const textRef = useRef(text)
  const streamingRef = useRef(streaming)
  const shownRef = useRef(streaming ? 0 : text.length)
  const [shown, setShown] = useState(shownRef.current)

  textRef.current = text
  streamingRef.current = streaming

  useEffect(() => {
    const previous = targetRef.current
    const isAppend = previous === null || text.startsWith(previous)
    if (!isAppend) {
      shownRef.current = 0
      setShown(0)
    }
    targetRef.current = text

    if (!streaming) {
      shownRef.current = text.length
      setShown(text.length)
    }
  }, [text, streaming])

  useEffect(() => {
    if (!streaming) return undefined

    const tick = () => {
      if (!streamingRef.current) return
      const current = textRef.current
      const next = paragraphTarget(current, shownRef.current, false, minChars)
      if (next <= shownRef.current) return
      shownRef.current = Math.min(current.length, next)
      setShown(shownRef.current)
    }

    const timer = setInterval(tick, Math.max(1, pollMs))
    return () => clearInterval(timer)
  }, [streaming, minChars, pollMs])

  const target = targetRef.current
  // 目标更新到 effect 前，非追加正文不能暂时沿用旧文本的位置。
  const coherent = target === null || text.startsWith(target)
  const visible = coherent ? safeSlice(text, Math.min(shown, text.length)) : ''
  const done = !streaming && coherent && shown >= text.length
  return { text: visible, done }
}
