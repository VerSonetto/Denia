/**
 * 揭示引擎:把「模型已到达的文本」与「UI 已显示的文本」拆成两个状态机的一半。
 *
 * ## 字符积分(核心)
 * 显示位置不跟"剩余距离的比例"走,而是跟**速度**走:
 *
 *   v = 90 + 积压^1.25 × 0.85  (封顶 600 码元/秒)
 *
 * 积压小的时候 v ≈ 90,是一段稳定的打字机节奏(不会越放越慢);积压大时
 * 超线性追赶(1000 字积压约 4700 码元/秒的势能,但被上限钳住),一次网络
 * 分块送来几百字,画面仍是一行行长出来,而不是整段砸在屏幕上。每次提交的
 * 小数零头记进 `debt` 带到下一帧 —— 分数级积分不丢字。
 *
 * 积压超过 `LIVE_LAG_CHAR_CEILING` 后额外按 0.15s 窗口补速,保证显示不会
 * 长时间落后于流;极端积压(切回标签页、断流重连)直接跳到全文。
 *
 * ## 闭环背压
 * 每帧从压力通道读 `scale`(0.55~1)乘进速度:跟随引擎的滞后占满容量时
 * 揭示自动慢下来,腾出空间让视口追上。两个状态机因此互相咬合,而不是各自
 * 按自己的节奏跑。
 *
 * ## 提交频率
 * 收敛按真实时间算,但提交间隔封顶约 30 次/秒(见 `MIN_COMMIT_MS`):
 * 240Hz 屏上逐帧提交等于每秒 240 次 React 重渲染 + Markdown 尾部重解析,
 * 而人眼对逐字显示的分辨率到不了这个量级。
 *
 * ## 尾部收束
 * 生产端结束(settle)时通常还剩几十个字(揭示本来就允许略落后)。这里不
 * 直接跳到全文,而是按"剩余量 / 100~280ms"的速度匀速排空,`done` 之后才
 * 交给定稿渲染 —— 结算那一帧不会整段砸下来,数学公式与标题修复也顺延一个
 * 呼吸的时间。
 *
 * ## 代理对安全
 * 文本按 UTF-16 码元索引推进,截断点落在完整代理对上(emoji/生僻字是 2 个
 * 码元,拆开会显示 �),`safeSlice` 保证不折断。
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import { MAX_FRAME_MS, drainCpsFor, queueRevealStep } from './streaming/physics'
import { useStreamPressure, type StreamPressure } from './streaming/pressure'

/**
 * 提交间隔下限(ms):约 30 次/秒。
 *
 * 高频屏(实测 240Hz)上每帧 setState 等于每秒 240 次 React 重渲染 +
 * Markdown 尾部重解析,换不来肉眼可见的平滑度。速度仍按真实 dt 计算,
 * 所以揭示速率不受影响,只是采样更疏。
 */
const MIN_COMMIT_MS = 32
/** 单次积分的时长下限(ms):防止同一毫秒内两次提交算出零进展。 */
const MIN_FRAME_MS = 8
/** 积压超过该值直接跳到底(极端积压:切回标签页、断流重连后的补发)。 */
const CATCH_UP_JUMP = 2000
/** 收束兜底(ms):帧循环被隐藏标签页等拖住时,到时也必须定稿。 */
const DRAIN_DEADLINE_MS = 400

export interface TypewriterOptions {
  /**
   * 压力通道;缺省取对话流的 context。
   *
   * 自带内层滚动框的组件(思考框)传自己的那一份 —— 那里的跟随发生在内层,
   * 背压也该由内层的滞后决定。
   */
  pressure?: StreamPressure | null
}

export interface Reveal {
  /** 当前应显示的文本前缀。 */
  text: string
  /** 生产端已结束且残余已排空:可以切到定稿渲染。 */
  done: boolean
}

/**
 * 返回当前应显示的文本与"是否已定稿"。
 * @param text - 模型已到达的完整目标文本。
 * @param streaming - 生产端是否仍在输出。
 * @param options - 压力通道等显示参数。
 */
export function useTypewriter(
  text: string,
  streaming: boolean,
  options?: TypewriterOptions,
): Reveal {
  const contextPressure = useStreamPressure()
  const pressure = options?.pressure ?? contextPressure
  const pressureRef = useRef<StreamPressure | null>(pressure)
  pressureRef.current = pressure

  // 已显示的码元数(UTF-16 单元)。ref 供帧循环推进,state 只负责触发渲染。
  const shownRef = useRef(streaming ? 0 : text.length)
  // 分数级积分的进位:几次提交凑不满一个字也不会丢。
  const debtRef = useRef(0)
  // 上一帧的目标文本(判定追加 / 重置)。
  const targetRef = useRef<string | null>(null)
  // 这段文本是否以"生产中"的身份进来过:决定 settle 时排空还是直接全量。
  const liveRef = useRef(false)
  const [shown, setShown] = useState(() => (streaming ? 0 : text.length))
  const rafRef = useRef(0)
  const timerRef = useRef(0)

  const stop = useCallback(() => {
    if (rafRef.current !== 0) {
      cancelAnimationFrame(rafRef.current)
      rafRef.current = 0
    }
    if (timerRef.current !== 0) {
      window.clearTimeout(timerRef.current)
      timerRef.current = 0
    }
  }, [])

  useEffect(() => {
    stop()
    const length = text.length
    // 目标非追加(重试生成 / 换了一段正文):放弃旧位置从头播。
    const previous = targetRef.current
    if (previous !== null && !text.startsWith(previous)) {
      shownRef.current = 0
      debtRef.current = 0
      setShown(0)
    }
    targetRef.current = text

    const finish = () => {
      stop()
      shownRef.current = length
      setShown(length)
    }

    if (!streaming) {
      const wasLive = liveRef.current
      liveRef.current = false
      if (!wasLive || shownRef.current >= length) {
        // 非流式首屏(历史回放 / 已定稿):立即全量,不播尾巴。
        finish()
        return
      }
      // 尾部收束:残余按固定速度排空,结算那一帧不再整段砸下来。
      let lastCommitAt = performance.now()
      const drainTick = (now: number) => {
        rafRef.current = 0
        const backlog = length - shownRef.current
        if (backlog <= 0) {
          finish()
          return
        }
        const since = now - lastCommitAt
        if (since < MIN_COMMIT_MS) {
          rafRef.current = requestAnimationFrame(drainTick)
          return
        }
        const dtMs = Math.max(MIN_FRAME_MS, Math.min(MAX_FRAME_MS, since))
        const chars = Math.max(1, Math.round((drainCpsFor(backlog) * dtMs) / 1000))
        shownRef.current = Math.min(length, shownRef.current + chars)
        lastCommitAt = now
        setShown(Math.floor(shownRef.current))
        if (shownRef.current >= length) {
          finish()
          return
        }
        rafRef.current = requestAnimationFrame(drainTick)
      }
      // 隐藏标签页里 rAF 不跑:兜底定时器保证这段文本总会定稿。
      timerRef.current = window.setTimeout(finish, DRAIN_DEADLINE_MS)
      rafRef.current = requestAnimationFrame(drainTick)
      return () => stop()
    }

    liveRef.current = true
    if (shownRef.current >= length) {
      // 已追平:停帧,等下一次 text 变化重新拉起。
      return
    }
    let lastCommitAt = performance.now()
    const tick = (now: number) => {
      rafRef.current = 0
      const backlog = length - shownRef.current
      if (backlog <= 0) return
      const since = now - lastCommitAt
      if (since < MIN_COMMIT_MS && backlog <= CATCH_UP_JUMP) {
        rafRef.current = requestAnimationFrame(tick)
        return
      }
      if (backlog > CATCH_UP_JUMP) {
        shownRef.current = length
        debtRef.current = 0
        setShown(length)
        return
      }
      const channel = pressureRef.current
      const dtMs = Math.max(MIN_FRAME_MS, Math.min(MAX_FRAME_MS, since))
      const step = queueRevealStep(backlog, dtMs, debtRef.current, channel?.scale ?? 1)
      shownRef.current = Math.min(length, shownRef.current + step.chars)
      debtRef.current = step.debt
      // 上报揭示速率:跟随引擎据此开合预测预留。
      if (channel !== null) {
        channel.speedCps = step.speedCps
        channel.speedAt = now
      }
      lastCommitAt = now
      setShown(Math.floor(shownRef.current))
      if (shownRef.current < length) rafRef.current = requestAnimationFrame(tick)
    }
    rafRef.current = requestAnimationFrame(tick)
    return () => stop()
  }, [text, streaming, stop])

  useEffect(() => stop, [stop])

  if (!streaming && shown >= text.length) return { text, done: true }
  const target = targetRef.current
  // 目标与本次渲染的文本对不上(追加还没进 effect):先空一帧,不显示错位文本。
  const base = target !== null && text.startsWith(target) ? text : ''
  return { text: safeSlice(base, shown), done: false }
}

/**
 * 按"码元数"截断字符串,绝不切开 UTF-16 代理对。
 * 落点在代理对起点(高位代理)时包含整个对;在代理对中部(低位代理)时
 * 回退一个码元,保证 emoji 完整显示。
 */
function safeSlice(text: string, length: number): string {
  const target = Math.floor(length)
  if (target <= 0) return ''
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
