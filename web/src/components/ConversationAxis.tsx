import { useCallback, useEffect, useId, useMemo, useRef, useState } from 'react'
import type { SessionAnchor } from '../api'
import { t } from '../i18n'

/**
 * 对话轴(抄 dsh TurnNavigator):聊天区左侧一条紧凑轨道,每条用户消息一个
 * 短横线 tick。刻度来自后端全量锚点(不受分页窗口限制),即使只加载了尾部
 * 窗口也能显示全部轮次;点击未加载的刻度由视图向上翻页覆盖后再定位。
 * 默认按 10px 自然间距挤成一簇、在可视带里垂直居中;只有当自然高度超过
 * 可视带时才压缩成等比铺满——少时是一小簇,多时才撑开。整条轨道接管
 * 指针:悬浮显示对应消息预览,点击跳转并居中。
 */

/** 相邻 tick 的自然间距与轨道两端内边距(px)。 */
const MARK_SPACING_PX = 10
const RAIL_INSET_PX = 6
/**
 * 刻度几何缓存的存活时长(ms)。轨道是 sticky 的、滚动时刻度不动,所以同一
 * 份几何可以复用;但 rail 高度过渡与窗口 resize 都会挪动刻度,给缓存一个
 * 短存活期即可自愈,不必逐事件作废(指针每移动几像素就重建一次反而更贵)。
 */
const CENTERS_TTL_MS = 200

/**
 * 轨道上离指针最近的刻度。
 *
 * 每个刻度的 `getBoundingClientRect()` 都是一次强制布局读取:直接按指针
 * 位置逐个问过去,鼠标在轨道上移动时每次 pointermove 都要读一遍全部刻度。
 * 调用方把一份"刻度中心线"缓存传进来(见 `centersRef`),这里只做纯算术。
 */
function nearestItem(centers: readonly number[], clientY: number): number {
  let best = -1
  let bestDist = Infinity
  for (let i = 0; i < centers.length; i++) {
    const dist = Math.abs(clientY - centers[i])
    if (dist < bestDist) {
      bestDist = dist
      best = i
    }
  }
  return best
}

export function ConversationAxis({
  anchors,
  scrollRef,
  onJumpMiss,
  onJump,
}: {
  anchors: SessionAnchor[]
  scrollRef: React.RefObject<HTMLDivElement | null>
  /** 点击的刻度尚未加载(锚点在分页窗口之外):交由视图翻页覆盖后定位。 */
  onJumpMiss?: (seq: number) => void
  /** 跳转开始:通知外层解开吸底,免得心跳把平滑滚动拉回底部。 */
  onJump?: () => void
}) {
  const items = anchors

  const [preview, setPreview] = useState(-1)
  const [active, setActive] = useState(-1)
  const marksRef = useRef<HTMLElement[]>([])
  const railRef = useRef<HTMLElement | null>(null)
  const previewId = useId()
  /**
   * 刻度中心线的缓存(相对视口)。轨道是 sticky 的,滚动时刻度不动,
   * 所以同一份几何可以供多次 pointermove 复用 —— 每个刻度的
   * `getBoundingClientRect()` 都是一次强制布局读取,逐次问过去太贵。
   */
  const centersRef = useRef<number[]>([])
  const centersAtRef = useRef(0)

  /** 重建刻度中心线缓存(刻度重挂载或距上次测量超过节拍时)。 */
  const syncCenters = useCallback(() => {
    const now = performance.now()
    if (centersRef.current.length > 0 && now - centersAtRef.current < CENTERS_TTL_MS) {
      return centersRef.current
    }
    const marks = marksRef.current
    const centers: number[] = new Array(marks.length)
    for (let i = 0; i < marks.length; i++) {
      const rect = marks[i]?.getBoundingClientRect()
      centers[i] = rect ? rect.top + rect.height / 2 : Number.NaN
    }
    centersRef.current = centers
    centersAtRef.current = now
    return centers
  }, [])

  /** 按 seq 查锚点下标:原先每个已加载用户消息都做一次线性 findIndex,O(m²)。 */
  const indexBySeq = useMemo(() => {
    const map = new Map<number, number>()
    items.forEach((item, index) => {
      if (!map.has(item.seq)) map.set(item.seq, index)
    })
    return map
  }, [items])

  // 刻度元素的注册回调按 index 缓存:若每次渲染都新建箭头,React 会把
  // 每个刻度的 ref 回调拆掉重挂,清缓存的副作用就会在每次 hover 重渲染时
  // 反复触发,几何缓存形同虚设。
  const markRefs = useRef(new Map<number, (el: HTMLElement | null) => void>())
  const setMark = useCallback((index: number) => {
    let handler = markRefs.current.get(index)
    if (!handler) {
      handler = (el: HTMLElement | null) => {
        if (el) marksRef.current[index] = el
        // 刻度挂载/卸载(数量变化、会话切换):几何缓存立即作废。
        centersRef.current = []
      }
      markRefs.current.set(index, handler)
    }
    return handler
  }, [])

  const jump = useCallback(
    (anchor: number) => {
      const scroller = scrollRef.current
      if (!scroller) return
      const target = scroller.querySelector<HTMLElement>(`[data-user-anchor="${anchor}"]`)
      if (!target) {
        onJumpMiss?.(anchor)
        return
      }
      // 定位期间必须解开吸底:否则内容一到就落底,把平滑滚动拉回去。
      onJump?.()
      target.scrollIntoView({ behavior: 'smooth', block: 'center' })
    },
    [scrollRef, onJumpMiss, onJump],
  )

  // active = 可视带中线最近的那条用户消息;滚动与内容变化时重算。
  // 已加载的行才能量到几何,按 seq 映射回全量锚点序列定位刻度。
  useEffect(() => {
    const scroller = scrollRef.current
    if (!scroller) return
    let frame = 0
    const measure = () => {
      const sRect = scroller.getBoundingClientRect()
      const composer = parseFloat(scroller.style.getPropertyValue('--composer-height')) || 0
      const bandCenter = sRect.top + (sRect.height - composer) / 2
      let best = -1
      let bestDist = Infinity
      for (const node of scroller.querySelectorAll<HTMLElement>('[data-user-anchor]')) {
        const r = node.getBoundingClientRect()
        const dist = Math.abs(bandCenter - (r.top + r.height / 2))
        if (dist < bestDist) {
          bestDist = dist
          // 锚点可能在窗口里重复出现(乐观行/未加载);取首个匹配,与旧行为一致。
          const index = indexBySeq.get(Number(node.dataset.userAnchor))
          if (index !== undefined) best = index
        }
      }
      setActive(best)
    }
    const onScroll = () => {
      cancelAnimationFrame(frame)
      frame = requestAnimationFrame(measure)
    }
    measure()
    scroller.addEventListener('scroll', onScroll, { passive: true })
    return () => {
      cancelAnimationFrame(frame)
      scroller.removeEventListener('scroll', onScroll)
    }
  }, [items, scrollRef, indexBySeq])

  if (items.length === 0) return null

  const count = items.length
  const railStyle = {
    '--rail-natural-height': `${(count - 1) * MARK_SPACING_PX + 2 * RAIL_INSET_PX}px`,
    '--rail-inset': `${RAIL_INSET_PX}px`,
  } as React.CSSProperties

  const positionVars = (index: number) =>
    ({
      '--mark-natural': `${index * MARK_SPACING_PX}px`,
      '--mark-percent': `${count <= 1 ? 50 : (index / (count - 1)) * 100}%`,
    }) as React.CSSProperties

  const previewItem = preview >= 0 ? items[preview] : undefined
  const previewIndex = preview

  return (
    <div className="conversation-axis" aria-label={t('axisLabel')}>
      <nav
        ref={railRef}
        className="axis-rail"
        style={railStyle}
        onClick={(e) => {
          const i = nearestItem(syncCenters(), e.clientY)
          if (i >= 0) jump(items[i].seq)
        }}
        onPointerMove={(e) => setPreview(nearestItem(syncCenters(), e.clientY))}
        onPointerLeave={() => setPreview(-1)}
      >
        <div className="axis-marks">
          {items.map((item, index) => {
            const state =
              index === active ? 'active' : index === previewIndex ? 'preview' : 'rest'
            return (
              <div
                key={item.seq}
                ref={setMark(index)}
                className={`axis-mark axis-mark-${state}`}
                style={positionVars(index)}
              />
            )
          })}
        </div>
        {previewItem !== undefined && (
          <div
            id={previewId}
            role="tooltip"
            className="axis-preview"
            style={positionVars(previewIndex)}
          >
            <div className="axis-preview-text">{previewItem.text}</div>
          </div>
        )}
      </nav>
    </div>
  )
}
