import { useCallback, useEffect, useRef, useState, type MutableRefObject } from 'react'

/**
 * 吸底跟随(对话流与思考框共用)。
 *
 * 核心原则:**归属(stick)只由「读者的滚动」决定,内容增长只负责"跟随"**。
 * 两者互不当证据 —— 这是与旧实现的关键差别。
 *
 * 旧实现把"离底距离"直接当归属判据,而且要等 500ms 采样才读一次几何。流式内容
 * 在那段窗口里已经长了几百 px,读者拖到底的瞬间其实已经贴底,却因为判定时又长
 * 了新内容,`floor - scrollTop` 远超阈值,被判成"已离开底部";恢复路径同样要等
 * 下一次采样,于是越滚越丢,只能反复把滚动条拉到底。本实现把判定搬回滚动发生的
 * 那一刻,并分四层:
 *
 * 1. 程序滚动自己记账:落底写入时同步记下落点,scroll 回执与账本一致即认定是
 *    我们的滚动,不改归属 —— 内容收缩被钳底、同帧多次写入都不会被误读;
 * 2. 读者向下移动:走到**上一拍的底部**(他拖到底那一刻能看到的底)或接近当前
 *    底部,即恢复跟随。前者正是旧实现失守的地方:拿"现在的底"去比"他的位置",
 *    中间隔着那段时间里新长出来的内容,必然判失败;
 * 3. 位置本就在底部(含内容收缩被钳底):恢复跟随,除非程序性跳转正在飞;
 * 4. 读者向上移动:只有当期内容收缩解释不了的那部分位移才记账,累积越过阈值
 *    才脱跟 —— 滚动锚定、图片回流这类抖动不会把跟随打掉。
 *
 * 滚轮上翻另走快路:wheel 事件里直接累积,越过阈值立即脱跟,不等几何判定。
 * 内容暴涨时几何会滞后,而我们的落底写入可能反手把读者"抢"回底部,wheel 是唯一
 * 能抢在它前面拿到意图的信号。落在内层滚动区(思考框/代码卡/输入器)上的滚轮
 * 不算,那些区自己消化;滚动条拖动没有 wheel 信号,靠第 2/4 层兜住。
 *
 * 内容增长由心跳驱动:active 期(流式/压缩/思考吐字中)每帧脏检查,只在**内容真的
 * 长高**时落底 —— 高度没变就不碰位置,不和正在拖滚动条的读者抢。active 结束后
 * 再多观察 wakeMs:打字机逐帧 reveal 只改文本高度、不产生节点事件,settle 那一下
 * 还会补一大段文本,都要靠心跳兜住。稳定期心跳自动停,不空转;脱跟期只刷新几何
 * 基准,让读者回到底部时的判定有新鲜参照。
 */

/** 离底容差(px):读者落在此范围内即恢复跟随。 */
const DEFAULT_SNAP = 32
/** "贴底"判定容差(px):到底时 scrollTop 精确等于上限,留一点舍入余量。 */
const BOTTOM_EPS = 2
/** 累积上翻意图超过该值才判定读者确实在往上翻(px)。 */
const MIN_INTENT = 16
/** 心跳观察窗(ms):active 期每帧续期,active 结束后自然收尾这一段。 */
const DEFAULT_WAKE_MS = 600
/** 程序性跳转的静默窗口(ms):期间不自动吸附,读者一动即交还控制权。 */
const JUMP_GRACE_MS = 1500
/** 读者拖拽期间的避让窗口(ms):此间心跳不落底,免得和读者拉锯。 */
const YIELD_MS = 150
/** 判定"位置/高度是否变化"的容差(px),滤掉子像素抖动。 */
const EPSILON = 0.5

export interface StickToBottomOptions {
  /** 内容高频增长期(流式/压缩/思考吐字中):心跳全程运行。 */
  active?: boolean
  /** 离底容差(px),缺省 32;窄的内层滚动框可调小。 */
  snap?: number
  /** 心跳观察窗(ms),缺省 600。 */
  wakeMs?: number
  /** 累积上翻意图阈值(px),缺省 16;内层滚动框可调小。 */
  minIntent?: number
  /**
   * 容器挂载时是否直接对齐到底部。
   * true(缺省):对话流首屏 / 切会话 —— 总是显示最新内容;
   * false:流式结束后才手动展开的内层区(思考框),让读者从头读。
   */
  alignOnAttach?: boolean
}

export interface StickToBottomHandle {
  /** 当前是否吸底(回底按钮、展开态等 UI 依据)。 */
  stick: boolean
  /** 挂到滚动容器上:`ref={handle.setNode}`。 */
  setNode: (el: HTMLElement | null) => void
  /** 回底并恢复吸底:发送消息 / 点回底按钮。 */
  snapToBottom: () => void
  /** 脱跟:程序性跳转(轮次轴定位、翻页)前调用,免得心跳把跳转拉回底部。 */
  release: () => void
  /** 复位为吸底:切会话时配合清空 scrollTop,不写 DOM。 */
  reset: () => void
  /** 内容可能增长时调用(节点更新 / 容器尺寸变化 / 回合启动)。 */
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

export function useStickToBottom<T extends HTMLElement>(
  ref: MutableRefObject<T | null>,
  options: StickToBottomOptions = {},
): StickToBottomHandle {
  const {
    active = false,
    snap = DEFAULT_SNAP,
    wakeMs = DEFAULT_WAKE_MS,
    minIntent = MIN_INTENT,
    alignOnAttach = true,
  } = options
  const [stick, setStickState] = useState(true)

  const stickRef = useRef(true)
  /** 我们的程序滚动最后写入的落点;-1 表示当前没有待认领的滚动。 */
  const ledgerRef = useRef(-1)
  /** 上一拍几何:读者位移与内容变化量的比较基准。 */
  const lastTopRef = useRef(0)
  const lastHeightRef = useRef(0)
  /** 上一拍的底部上限;-1 表示基线尚未建立(挂载后首个事件只记录不判定)。 */
  const lastFloorRef = useRef(-1)
  /** 我们最后一次落底写入时的底部上限:心跳据此判断"有没有新内容"。 */
  const writtenFloorRef = useRef(0)
  /** 累积的上翻意图(px)。 */
  const upIntentRef = useRef(0)
  /** 程序性跳转在飞:静默期内不自动吸附。 */
  const jumpRef = useRef(false)
  const jumpTimerRef = useRef(0)
  /** 避让截止时刻:读者正在拖,心跳暂时别落底。 */
  const yieldUntilRef = useRef(0)
  /** 心跳观察窗截止时刻(performance.now())。 */
  const wakeUntilRef = useRef(0)
  const rafRef = useRef(0)

  // 选项进 ref:回调标识保持稳定,不因每次渲染重挂 ref/监听。
  const activeRef = useRef(active)
  activeRef.current = active
  const snapRef = useRef(snap)
  snapRef.current = snap
  const wakeRef = useRef(wakeMs)
  wakeRef.current = wakeMs
  const intentRef = useRef(minIntent)
  intentRef.current = minIntent
  const alignRef = useRef(alignOnAttach)
  alignRef.current = alignOnAttach

  const setStick = useCallback((next: boolean) => {
    if (stickRef.current === next) return
    stickRef.current = next
    setStickState(next)
  }, [])

  /** 恢复跟随(读者回到底部 / 内容不足一屏 / 程序性回底)。 */
  const attach = useCallback(() => {
    upIntentRef.current = 0
    jumpRef.current = false
    if (jumpTimerRef.current !== 0) {
      window.clearTimeout(jumpTimerRef.current)
      jumpTimerRef.current = 0
    }
    setStick(true)
  }, [setStick])

  /** 脱跟。此后只有读者明确向下回到贴底位置、或程序性回底才恢复。 */
  const detach = useCallback(() => {
    wakeUntilRef.current = 0
    ledgerRef.current = -1
    // 基线作废:紧跟其后的 scroll 回执不得再被当成"还在底部"。
    lastFloorRef.current = -1
    upIntentRef.current = 0
    setStick(false)
  }, [setStick])

  /** 刷新几何基线(只读,不写 scrollTop);脱跟期也用它保持基准新鲜。 */
  const sync = useCallback(() => {
    const el = ref.current
    if (el === null) return
    lastTopRef.current = el.scrollTop
    lastHeightRef.current = el.scrollHeight
    lastFloorRef.current = Math.max(0, el.scrollHeight - el.clientHeight)
  }, [ref])

  /** 落底并记账(读回被钳制后的真实落点,账本恒等于 DOM)。 */
  const write = useCallback(() => {
    const el = ref.current
    if (el === null) return
    const floor = Math.max(0, el.scrollHeight - el.clientHeight)
    if (Math.abs(floor - el.scrollTop) > EPSILON) el.scrollTop = floor
    ledgerRef.current = el.scrollTop
    lastTopRef.current = el.scrollTop
    lastHeightRef.current = el.scrollHeight
    lastFloorRef.current = floor
    writtenFloorRef.current = floor
  }, [ref])

  // 心跳:吸底时补齐新长出来的内容;脱跟时只刷基线,绝不碰读者的位置。
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
    // active 期每帧续期,于是 active 结束后还能自然多观察一段(settle 补文)。
    if (activeRef.current) wakeUntilRef.current = now + wakeRef.current
    if (now >= wakeUntilRef.current) return
    if (stickRef.current) {
      const floor = Math.max(0, el.scrollHeight - el.clientHeight)
      if (floor > writtenFloorRef.current + EPSILON) {
        // 有新内容才落底;读者正在拖就先让路,免得两边拉锯。
        if (now >= yieldUntilRef.current) write()
      } else if (floor < writtenFloorRef.current - EPSILON) {
        // 内容收缩(卡片收起/图片回流):基线跟着新高度走,否则之后要长过
        // 旧高度才会重新落底。
        writtenFloorRef.current = floor
        sync()
      }
    } else {
      sync()
    }
    kick()
  }

  /** 唤醒心跳并延长观察窗(不改变归属)。 */
  const wake = useCallback(() => {
    wakeUntilRef.current = performance.now() + wakeRef.current
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
    write()
    wake()
  }, [attach, write, wake])

  const reset = useCallback(() => {
    wakeUntilRef.current = 0
    ledgerRef.current = -1
    upIntentRef.current = 0
    writtenFloorRef.current = 0
    yieldUntilRef.current = 0
    jumpRef.current = false
    if (jumpTimerRef.current !== 0) {
      window.clearTimeout(jumpTimerRef.current)
      jumpTimerRef.current = 0
    }
    setStick(true)
    // 几何基线作废:调用方紧接着会把 scrollTop 清零,那不是读者上翻。
    lastTopRef.current = 0
    lastHeightRef.current = 0
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
    write()
    wake()
  }, [ref, sync, write, wake])

  const handleScroll = useCallback(() => {
    const el = ref.current
    if (el === null) return
    const height = el.scrollHeight
    const floor = Math.max(0, height - el.clientHeight)
    const top = el.scrollTop
    const prevTop = lastTopRef.current
    const prevHeight = lastHeightRef.current
    const prevFloor = lastFloorRef.current
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
    const movedDown = top > prevTop + EPSILON
    const movedUp = top < prevTop - EPSILON
    if (movedDown) {
      upIntentRef.current = 0
      // 走到上一拍的底部 = 读者已经拖到底(那是他当时能看到的底);内容此后
      // 又长了多少都不该推翻这个事实 —— 旧实现正是在这里失守。接近当前底部
      // 同样恢复跟随。
      if (top >= prevFloor - BOTTOM_EPS || floor - top <= snapRef.current) {
        attach()
        wake()
      }
      return
    }
    if (!movedUp) {
      // 位置就在底部(读者精确落底、内容收缩被钳底):恢复跟随。
      if (!jumpRef.current && floor - top <= snapRef.current) {
        attach()
        wake()
      }
      return
    }
    // 向上:程序性跳转在飞时不算读者意图(平滑滚动早期位移很小)。
    if (jumpRef.current) return
    // 内容收缩会把 scrollTop 钳到新底(位置"上移"但不是读者在翻):扣掉记账。
    const shrink = Math.max(0, prevHeight - height)
    const unexplained = prevTop - top - shrink
    if (unexplained <= 0) {
      upIntentRef.current = 0
      return
    }
    upIntentRef.current += unexplained
    if (upIntentRef.current > intentRef.current) {
      detach()
      return
    }
    // 还没到阈值:读者可能正在拖,先让路,别用落底把这段位移抹掉。
    yieldUntilRef.current = performance.now() + YIELD_MS
  }, [ref, attach, detach, wake])

  /** 挂容器:监听装在这里,条件渲染的滚动框(思考框)自动跟随挂载。 */
  const detachRef = useRef<(() => void) | null>(null)
  /**
   * 上一次挂过的元素。React 会因 ref 回调换标识而 detach/attach 同一个 DOM 节点,
   * 那不是"新容器" —— 若按新容器对齐到底部,就会把读者的位置抢走。
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
      sync()
      const onWheel = (event: WheelEvent) => {
        if (insideNestedScroller(el, event.target)) return
        const delta = wheelPixels(event, el.clientHeight)
        if (delta >= 0) {
          // 向下滚:抵消累积的上翻意图,否则"上上下下"会把跟随抖掉。
          upIntentRef.current = 0
          return
        }
        // 已经在顶端:没有可上翻的内容,别把这当成脱跟。
        if (el.scrollTop <= 0) return
        jumpRef.current = false
        // 不等几何判定:我们的落底写入可能反手把读者"抢"回底部。
        upIntentRef.current += -delta
        if (upIntentRef.current > intentRef.current) detach()
        else yieldUntilRef.current = performance.now() + YIELD_MS
      }
      el.addEventListener('scroll', handleScroll, { passive: true })
      el.addEventListener('wheel', onWheel, { passive: true })
      detachRef.current = () => {
        el.removeEventListener('scroll', handleScroll)
        el.removeEventListener('wheel', onWheel)
      }
      // 真正换人的容器(首屏 / 折叠后重新展开的思考框 / 切会话)才对底:
      // 同一节点被重挂一次就抢读者的位置,是长会话里最像"跟随又丢了"的行为。
      if (fresh && alignRef.current) {
        attach()
        write()
        wake()
      }
    },
    [ref, sync, handleScroll, attach, detach, write, wake],
  )

  return { stick, setNode, snapToBottom, release, reset, follow }
}
