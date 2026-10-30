/**
 * 打字机显示:把「模型已到达的文本」与「UI 逐帧显示的文本」分离,
 * 逐字 reveal 的质感与模型吐字速度天然并存。
 *
 * ## 自适应跟随(核心)
 * 不用固定字符/秒限速(那会让快模型积压、显示变慢),而是让显示位置
 * 以指数收敛逼近目标:每次提交把显示位置向目标推进 `1 - e^(-λ·dt)` 比例。
 *
 * 收敛按真实经过时间算,但提交频率封顶约 30 次/秒(见 `MIN_COMMIT_MS`):
 * 240Hz 屏上逐帧提交等于每秒 240 次重渲染,换不来肉眼可见的平滑度。
 *
 * 稳态下显示领先/落后量与模型速率成正比:
 * - 快模型(每秒上百字):每次提交自动多 reveal 一些,延迟恒定在 ~100ms,
 *   视觉上是"快速一字字长出来",不积压不跳变;
 * - 慢模型(每秒十几字):每次提交 reveal 半字到一字,打字机质感;
 * - 模型暂停:显示位置收完剩余文本后停帧,追加时从中断处继续。
 *
 * 模型完全收尾(settle,streaming=false)时立即返回全文,不播尾巴。
 *
 * ## 代理对安全
 * 文本按 UTF-16 码元索引推进,截断点落在完整代理对上
 * (emoji/生僻字是 2 个码元,拆开会显示 �),`safeSlice` 保证不折断。
 *
 * ## 重置
 * 目标文本非 append(重试生成/切消息)时放弃旧累积从头播。
 */

import { useEffect, useRef, useState } from 'react'

/** 收敛速率(1/秒):显示位置向目标指数逼近的系数。越大越跟手。 */
const DEFAULT_FOLLOW = 8
/** 帧间隔下限(ms):防止 dt 过小导致无进展。 */
const MIN_FRAME_MS = 8
/**
 * 提交间隔下限(ms):约 30 次/秒。
 *
 * 高频屏(实测 240Hz)上每帧 setState 等于每秒 240 次 React 重渲染 +
 * Markdown 尾部重解析,而人眼对逐字显示的分辨率到不了这个量级。
 * 收敛仍按真实 dt 计算,所以滞后量不变(依旧约 100ms),只是采样更疏。
 */
const MIN_COMMIT_MS = 32
/** 落后超过该值直接跳到底,不再逐字播(极端卡顿恢复场景)。 */
const CATCH_UP_JUMP = 2000

export interface TypewriterOptions {
  /** 收敛速率(1/秒);缺省 8,越大显示越跟手。 */
  follow?: number
}

/**
 * 返回当前应显示的文本片段(逐字 reveal)。
 * @param text - 模型已到达的完整目标文本。
 * @param streaming - 是否仍在流式;false 时立即返回全文。
 * @param options - 跟随速率等显示参数。
 */
export function useTypewriter(
  text: string,
  streaming: boolean,
  options?: TypewriterOptions,
): string {
  const follow = options?.follow ?? DEFAULT_FOLLOW
  // 当前显示了多少"码元"。
  const shownRef = useRef(0)
  // 上一帧的目标文本(判定 append / 重置)。
  const targetRef = useRef<string | null>(null)
  const [shown, setShown] = useState(() => (streaming ? 0 : text.length))

  useEffect(() => {
    if (!streaming) {
      // settle / 非流式:立即补齐,不播尾巴。
      shownRef.current = text.length
      targetRef.current = null
      setShown(text.length)
      return
    }
    const last = targetRef.current
    if (last !== null && text.startsWith(last)) {
      // append-only 继续:显示位置照旧,继续追新增。
    } else {
      // 首帧或目标非 append(重试/切消息):从头播。
      shownRef.current = 0
      setShown(0)
    }
    if (shownRef.current > text.length) shownRef.current = text.length
    targetRef.current = text

    let raf = 0
    let stopped = false
    let lastCommitAt = performance.now()
    const tick = (now: number) => {
      if (stopped) return
      const shownNow = shownRef.current
      if (shownNow >= text.length) {
        // 播完:停帧(模型后续追加到 text 会重新触发 effect)。
        return
      }
      if (now - lastCommitAt < MIN_COMMIT_MS && text.length - shownNow <= CATCH_UP_JUMP) {
        // 未到提交间隔:继续等下一帧,不触发渲染。
        raf = requestAnimationFrame(tick)
        return
      }
      const dt = Math.max(MIN_FRAME_MS, now - lastCommitAt) / 1000
      // 指数收敛:每次提交前进剩余距离的固定比例,速率自适应模型。
      const k = 1 - Math.exp(-follow * dt)
      let next = shownNow + (text.length - shownNow) * k
      if (text.length - shownNow > CATCH_UP_JUMP || next >= text.length - 0.5) {
        // 极端积压直接跳底;剩余不足半字时落定,避免无限逼近。
        next = text.length
      }
      shownRef.current = next
      setShown(Math.floor(next))
      lastCommitAt = now
      if (next < text.length) raf = requestAnimationFrame(tick)
    }
    raf = requestAnimationFrame(tick)
    return () => {
      stopped = true
      cancelAnimationFrame(raf)
    }
  }, [text, streaming, follow])

  if (!streaming) return text
  const target = targetRef.current
  return target !== null && text.startsWith(target) ? safeSlice(text, shown) : ''
}

/**
 * 按"码元数"截断字符串,绝不切开 UTF-16 代理对。
 * 落点在代理对起点(高位代理)时包含整个对;在代理对中部(低位代理)
 * 时回退一个码元,保证 emoji 完整显示。
 */
function safeSlice(text: string, length: number): string {
  const target = Math.floor(length)
  if (target >= text.length) return text
  const code = text.charCodeAt(target)
  if (code >= 0xd800 && code <= 0xdbff && target + 1 < text.length) {
    return text.slice(0, target + 2)
  }
  if (code >= 0xdc00 && code <= 0xdfff) {
    return text.slice(0, target - 1)
  }
  return text.slice(0, target)
}