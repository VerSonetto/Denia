import { useCallback, useEffect, useRef, useState, type MutableRefObject } from 'react'
import {
  LINE_HEIGHT_PX,
  MAX_FRAME_MS,
  SPRING_REST_PX,
  easeReserve,
  reserveTargetFor,
  revealScaleFor,
  springStep,
} from './physics'
import { SPEED_TTL_MS, createPressure, type StreamPressure } from './pressure'

/**
 * 跟随引擎:把「内容长高的台阶」变成「视口匀速逼近的滑轨」。
 *
 * ## 两个状态机
 * 内容呈现(揭示引擎)与视口运动(本引擎)各管一段:内容已到达多少由流决定,
 * 视口跟到哪由弹簧决定。读者看到的平滑是两者叠加的结果 —— 揭示负责字句匀速
 * 涌现,本引擎负责视口不被折行与整段插入踢动。
 *
 * ## 为什么是弹簧而不是"写到底"
 * `scrollTop = scrollHeight` 是位移直通:一行正文折行会让内容底在一次布局里
 * 跳 24~28px,工具卡展开、图片回流、结算补文更是几百 px,读者的视线就得追。
 * 改成二阶阻尼弹簧(k=130, c=24, 每帧 4 亚步进)后,每帧只推进剩余滞后的一
 * 部分:折行的那一跳被摊到 4~6 帧、单帧不到 8px;几百 px 的插入同样是滑进去的。
 *
 * 速度钳在非负:越过目标(内容收缩)直接停,不做回弹 —— 来回弹是廉价感里最
 * 刺眼的一种。
 *
 * ## 预测预留
 * 视口稳态停在离底 `reserve` px 处(由揭示速率决定,24~56px,180ms 时间常数
 * 开合)。折行需要的那一整行因此落进预留里,而不是从视野底下顶出来。生产端
 * 停下后预留收回,视口回到真正的底部。
 *
 * 这段预留能真的"藏"住最新内容而不在底部留白,靠的是对话区自己的布局:输入区
 * (`.composer-seat`,sticky 贴底)在滚动内容里占着上百 px 的高度,它下面就是
 * 内容底。视口停在离底 reserve px 时,被推迟的内容正好落在输入区背后那条地带
 * 里 —— 读者看到的是最后一行慢慢升上来,而不是一块空白。
 *
 * ## 闭环背压
 * 滞后占容量(预留 + 一行)的比例超过 10% 起,揭示引擎按 1 → 0.55 线性减速:
 * 跟随跟不上就让内容慢下来,而不是让读者看着自己越掉越远。这是两个状态机
 * 之间唯一的耦合,通道见 `pressure.ts`。
 *
 * ## 归属(stick)只由读者的滚动决定
 * 程序写入自己记账(账本 = 最后写下的 scrollTop),回执与账本一致即认定是
 * 我们的滚动;读者上翻越过阈值立即脱跟,此后内容再长也不抢他的位置;读者
 * 回到底部(或点回底按钮)才恢复。滚轮上翻另走快路:不等几何判定,因为我们的
 * 下一次写入可能反手把读者抢回底部。
 */

/** 心跳观察窗(ms):active 期每帧续期,active 结束后自然收尾这一段。 */
const WAKE_MS = 600
/** 程序性跳转的静默窗口(ms):期间不自动吸附,读者一动即交还控制权。 */
const JUMP_GRACE_MS = 1500
/** 读者拖拽期间的避让窗口(ms):此间心跳不推进,免得和读者拉锯。 */
const YIELD_MS = 150
/** 贴底判定容差(px)。 */
const BOTTOM_EPS = 2
/** 读者上翻越过这么多 px 才算"要离开",一两 px 的抖动不算。 */
const UNPIN_PX = 8
/** 离底容差(px):读者落在此范围内即恢复跟随。 */
const SLACK_PX = 25
/** 恢复跟随要求的位置容差(px):真正回到底部。 */
const REPIN_PX = 1
/** 判定"位置/高度是否变化"的容差(px),滤掉子像素抖动。 */
const EPSILON = 0.5

export interface FollowOptions {
  /** 生产期(流式/压缩/思考吐字中):心跳全程运行,预留全开。 */
  active?: boolean
  /**
   * 容器挂载时是否直接对齐到底部。
   * true(缺省):对话流首屏 / 切会话 —— 总是显示最新内容;
   * false:流式结束后才手动展开的内层区(思考框),让读者从头读。
   */
  alignOnAttach?: boolean
}

export interface FollowHandle {
  /** 当前是否吸底(回底按钮、展开态等 UI 依据)。 */
  stick: boolean
  /** 本容器的压力通道:揭示引擎读 scale、写 speedCps。 */
  pressure: StreamPressure
  /** 挂到滚动容器上:`ref={handle.setPort}`。 */
  setPort: (el: HTMLElement | null) => void
  /** 回底并恢复吸底:发送消息 / 点回底按钮。 */
  snapToBottom: () => void
  /** 脱跟:程序性跳转(轮次轴定位、翻页)前调用,免得心跳把跳转拉回底部。 */
  release: () => void
  /** 复位为吸底:切会话时配合清空 scrollTop,不写 DOM。 */
  reset: () => void
  /** 内容可能增长时调用(节点更新 / 容器尺寸变化 / 回合启动):唤醒阻尼循环。 */
  follow: () => void
}

/** 滚轮是否落在容器内的另一个可滚动区(思考框/代码卡/输入器):那些位移不归外层管。 */
function insideNestedScroller(root: HTMLElement, target: EventTarget | null): boolean {
  let node: Node | null = target instanceof Node ? target : null
  while (node !== null && node !== root) {
    if (node instanceof HTMLElement && node.scrollHeight - node.clientHeight > 1) return true
    node = node.parentNode
  }
  return false
}

/** 滚轮位移折算成像素(行/页模式要换算,否则阈值判据会失真)。 */
function wheelPixels(event: WheelEvent, viewport: number): number {
  if (event.deltaMode === 1) return event.deltaY * 16
  if (event.deltaMode === 2) return event.deltaY * viewport
  return event.deltaY
}

export function useFollow<T extends HTMLElement>(
  ref: MutableRefObject<T | null>,
  options: FollowOptions = {},
): FollowHandle {
  const { active = false, alignOnAttach = true } = options
  const [stick, setStickState] = useState(true)
  const stickRef = useRef(true)

  /** 我们最后一次写入的 scrollTop;-1 表示当前没有待认领的滚动。 */
  const ledgerRef = useRef(-1)
  /** 上一拍几何:读者位移的参照。 */
  const lastTopRef = useRef(0)
  const lastHeightRef = useRef(0)
  /** 上一拍追踪的目标位置(底 - 预留):读者上翻的判定基准。 */
  const lastTargetRef = useRef(0)
  const lastFloorRef = useRef(-1)
  /** 弹簧状态:速度跨帧保留,于是连续增长能"接上劲"而不是每帧重新起步。 */
  const velocityRef = useRef(0)
  /** 当前预留(px):按揭示速率开合。 */
  const reserveRef = useRef(0)
  /** 程序性跳转在飞:静默期内不自动吸附。 */
  const jumpRef = useRef(false)
  const jumpTimerRef = useRef(0)
  /** 避让截止时刻:读者正在拖,心跳暂时别推进。 */
  const yieldUntilRef = useRef(0)
  /** 心跳观察窗截止时刻。 */
  const wakeUntilRef = useRef(0)
  const lastTickRef = useRef(0)
  const rafRef = useRef(0)
  const pressureRef = useRef<StreamPressure>(createPressure())

  // 选项进 ref:回调标识保持稳定,不因每次渲染重挂 ref/监听。
  const activeRef = useRef(active)
  activeRef.current = active
  const alignRef = useRef(alignOnAttach)
  alignRef.current = alignOnAttach

  const setStick = useCallback((next: boolean) => {
    if (stickRef.current === next) return
    stickRef.current = next
    setStickState(next)
  }, [])

  /** 刷新几何基线(只读,不写 scrollTop);脱跟期也用它保持基准新鲜。 */
  const sync = useCallback(() => {
    const el = ref.current
    if (el === null) return
    const height = el.scrollHeight
    const floor = Math.max(0, height - el.clientHeight)
    lastTopRef.current = el.scrollTop
    lastHeightRef.current = height
    lastFloorRef.current = floor
    lastTargetRef.current = Math.max(0, floor - reserveRef.current)
  }, [ref])

  /** 一步到位对齐到底(首屏、切会话、回底按钮):不播动画。 */
  const write = useCallback(() => {
    const el = ref.current
    if (el === null) return
    const floor = Math.max(0, el.scrollHeight - el.clientHeight)
    if (Math.abs(floor - el.scrollTop) > EPSILON) el.scrollTop = floor
    ledgerRef.current = el.scrollTop
    velocityRef.current = 0
    lastTopRef.current = el.scrollTop
    lastHeightRef.current = el.scrollHeight
    lastFloorRef.current = floor
    lastTargetRef.current = Math.max(0, floor - reserveRef.current)
  }, [ref])

  /** 恢复跟随(读者回到底部 / 内容不足一屏 / 程序性回底)。 */
  const attach = useCallback(() => {
    velocityRef.current = 0
    jumpRef.current = false
    if (jumpTimerRef.current !== 0) {
      window.clearTimeout(jumpTimerRef.current)
      jumpTimerRef.current = 0
    }
    setStick(true)
  }, [setStick])

  /** 脱跟。此后只有读者明确向下回到底部、或程序性回底才恢复。 */
  const detach = useCallback(() => {
    wakeUntilRef.current = 0
    ledgerRef.current = -1
    velocityRef.current = 0
    // 基线作废:紧跟其后的 scroll 回执不得再被当成"还在底部"。
    lastFloorRef.current = -1
    // 读者自己看东西时不再压揭示速度。
    pressureRef.current.scale = 1
    setStick(false)
  }, [setStick])

  // 心跳:吸底时逐帧推进(弹簧),脱跟时只刷基线,绝不碰读者的位置。
  const tickRef = useRef<() => void>(() => {})
  const kick = useCallback(() => {
    if (rafRef.current !== 0) return
    rafRef.current = requestAnimationFrame(() => tickRef.current())
  }, [])

  tickRef.current = () => {
    rafRef.current = 0
    const el = ref.current
    if (el === null) return
    const now = performance.now()
    // active 期每帧续期,于是 active 结束后还能自然多观察一段(结算补文)。
    if (activeRef.current) wakeUntilRef.current = now + WAKE_MS
    const dtMs = Math.min(MAX_FRAME_MS, Math.max(1, now - lastTickRef.current))
    lastTickRef.current = now

    const height = el.scrollHeight
    const client = el.clientHeight
    const floor = Math.max(0, height - client)
    const pressure = pressureRef.current
    // 揭示速率:超过 TTL 没上报就当空闲,预留收回。
    const speed = now - pressure.speedAt <= SPEED_TTL_MS ? pressure.speedCps : 0
    reserveRef.current = easeReserve(reserveRef.current, reserveTargetFor(speed), dtMs)
    const target = Math.max(0, floor - reserveRef.current)
    lastTargetRef.current = target
    lastFloorRef.current = floor
    lastHeightRef.current = height

    if (!stickRef.current) {
      lastTopRef.current = el.scrollTop
      pressure.scale = 1
      if (now < wakeUntilRef.current) kick()
      return
    }

    const top = el.scrollTop
    if (floor <= BOTTOM_EPS) {
      // 内容不足一屏:没有"跟随"可言。
      lastTopRef.current = top
      pressure.scale = 1
      if (now < wakeUntilRef.current) kick()
      return
    }

    if (now >= yieldUntilRef.current) {
      const step = springStep(dtMs, target - top, velocityRef.current)
      velocityRef.current = step.velocityPxPerSec
      if (step.advancePx > SPRING_REST_PX) {
        // 一步到位的前提是"不越界":弹簧推进量永远小于滞后,写回也不需要钳制,
        // 但内容同帧收缩时仍以当前底为上限。
        el.scrollTop = Math.min(top + step.advancePx, floor)
        ledgerRef.current = el.scrollTop
      }
    }

    const painted = el.scrollTop
    lastTopRef.current = painted
    // 背压看的是"离真正的底还有多远":预留是主动让出来的,不该被当成滞后。
    pressure.scale = revealScaleFor(Math.max(0, floor - painted), reserveRef.current + LINE_HEIGHT_PX)

    const settled = Math.abs(target - painted) <= SPRING_REST_PX &&
      reserveRef.current <= SPRING_REST_PX
    // 还没收敛就继续跑:追赶本身就是在干活,不能因为观察窗到期而停在半路。
    if (!settled) wakeUntilRef.current = now + WAKE_MS
    if (!settled || activeRef.current || now < wakeUntilRef.current) kick()
  }

  /** 唤醒心跳并延长观察窗(不改变归属)。 */
  const wake = useCallback(() => {
    wakeUntilRef.current = performance.now() + WAKE_MS
    kick()
  }, [kick])

  useEffect(() => {
    if (active) wake()
  }, [active, wake])

  useEffect(
    () => () => {
      if (rafRef.current !== 0) cancelAnimationFrame(rafRef.current)
      if (jumpTimerRef.current !== 0) window.clearTimeout(jumpTimerRef.current)
    },
    [],
  )

  const snapToBottom = useCallback(() => {
    attach()
    reserveRef.current = 0
    write()
    wake()
    sync()
  }, [attach, write, wake, sync])

  const reset = useCallback(() => {
    wakeUntilRef.current = 0
    ledgerRef.current = -1
    velocityRef.current = 0
    reserveRef.current = 0
    yieldUntilRef.current = 0
    jumpRef.current = false
    if (jumpTimerRef.current !== 0) {
      window.clearTimeout(jumpTimerRef.current)
      jumpTimerRef.current = 0
    }
    pressureRef.current.scale = 1
    setStick(true)
    // 几何基线作废:调用方紧接着会把 scrollTop 清零,那不是读者上翻。
    lastTopRef.current = 0
    lastHeightRef.current = 0
    lastTargetRef.current = 0
    lastFloorRef.current = -1
  }, [setStick])

  /** 程序性跳转:解跟并静默一段时间,免得心跳把跳转拉回底部。 */
  const release = useCallback(() => {
    detach()
    jumpRef.current = true
    if (jumpTimerRef.current !== 0) window.clearTimeout(jumpTimerRef.current)
    jumpTimerRef.current = window.setTimeout(() => {
      jumpTimerRef.current = 0
      jumpRef.current = false
    }, JUMP_GRACE_MS)
  }, [detach])

  const follow = useCallback(() => {
    if (ref.current === null) return
    if (!stickRef.current) {
      // 脱跟期内容仍在长:保持基线新鲜,读者回到底部时才判定得准。
      sync()
      return
    }
    // 只唤醒弹簧循环,不在这里落底:内容每长一点就跳一次正是要消灭的行为。
    wake()
  }, [ref, sync, wake])

  const handleScroll = useCallback(() => {
    const el = ref.current
    if (el === null) return
    const height = el.scrollHeight
    const floor = Math.max(0, height - el.clientHeight)
    const top = el.scrollTop
    const prevTop = lastTopRef.current
    const prevHeight = lastHeightRef.current
    const prevFloor = lastFloorRef.current
    const prevTarget = lastTargetRef.current
    const ledger = ledgerRef.current
    lastTopRef.current = top
    lastHeightRef.current = height
    lastFloorRef.current = floor
    ledgerRef.current = -1

    // 程序滚动的回执:落点就是我们最后写入的位置,不算读者输入。
    if (ledger >= 0 && Math.abs(top - ledger) <= 1) return
    // 内容不足一屏:没有"跟随"可言,一律视为贴底。
    if (floor <= BOTTOM_EPS) {
      attach()
      return
    }
    if (!stickRef.current) {
      // 脱跟期:读者回到底部才恢复跟随。位置到得了真正的底,或者落进了容差。
      if (top >= floor - REPIN_PX || floor - top <= SLACK_PX) {
        attach()
        wake()
      }
      return
    }
    if (top < prevTop - EPSILON) {
      // 读者上翻:相对"我们正在追踪的目标"(底 - 预留)而不是"当前的底" ——
      // 预留本来就停在那上面,拿底去比会把稳态判成读者的意图。
      if (jumpRef.current) return
      // 内容收缩(卡片收起 / 图片回流)会把 scrollTop 钳到新底:那一段上移
      // 不是读者在翻,扣掉之后剩下的才算他的意图。
      const shrink = Math.max(0, prevHeight - height)
      const unexplained = prevTop - top - shrink
      if (unexplained <= EPSILON) {
        // 完全由收缩解释:基线已是新底,归属不变。
        return
      }
      if (top < prevTarget - UNPIN_PX || (prevFloor >= 0 && top < prevFloor - reserveRef.current - UNPIN_PX)) {
        detach()
        return
      }
      // 还没到阈值:读者可能正在拖,先让路,别用锤底把这段位移抹掉。
      yieldUntilRef.current = performance.now() + YIELD_MS
    }
  }, [ref, attach, detach, wake])

  /** 挂容器:监听装在这里,条件渲染的滚动框(思考框)自动跟随挂载。 */
  const detachRef = useRef<(() => void) | null>(null)
  /**
   * 上一次挂过的元素。React 会因 ref 回调换标识而 detach/attach 同一个 DOM
   * 节点,那不是"新容器" —— 若按新容器对齐到底部,就会把读者的位置抢走。
   */
  const nodeRef = useRef<HTMLElement | null>(null)
  const setNode = useCallback(
    (el: HTMLElement | null) => {
      if (detachRef.current !== null) {
        detachRef.current()
        detachRef.current = null
      }
      ref.current = el as T | null
      if (el === null) return
      const fresh = nodeRef.current !== el
      nodeRef.current = el
      // 引擎自己管位置:浏览器的滚动锚定会在同一帧里回写 scrollTop,两边打架。
      el.style.overflowAnchor = 'none'
      sync()
      const onWheel = (event: WheelEvent) => {
        if (insideNestedScroller(el, event.target)) return
        const delta = wheelPixels(event, el.clientHeight)
        if (delta >= 0) return
        // 已经在顶端:没有可上翻的内容,别把这当成脱跟。
        if (el.scrollTop <= 0) return
        jumpRef.current = false
        // 不等几何判定:我们的下一次写入可能反手把读者抢回底部。
        if (el.scrollTop < lastTargetRef.current - UNPIN_PX) detach()
      }
      el.addEventListener('scroll', handleScroll, { passive: true })
      el.addEventListener('wheel', onWheel, { passive: true })
      detachRef.current = () => {
        el.removeEventListener('scroll', handleScroll)
        el.removeEventListener('wheel', onWheel)
      }
      // 真正换人的容器(首屏 / 折叠后重新展开的思考框 / 切会话)才对齐到底:
      // 同一节点被重挂一次就抢读者的位置,是长会话里最像"跟随又丢了"的行为。
      if (fresh && alignRef.current) {
        attach()
        write()
        wake()
      }
    },
    [ref, sync, handleScroll, attach, detach, write, wake],
  )

  return {
    stick,
    pressure: pressureRef.current,
    setPort: setNode,
    snapToBottom,
    release,
    reset,
    follow,
  }
}
