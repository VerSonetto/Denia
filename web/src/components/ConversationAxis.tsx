import { useCallback, useEffect, useId, useMemo, useRef, useState } from 'react'
import type { TranscriptNode } from '../fold'
import { t } from '../i18n'

/**
 * 对话轴(抄 dsh TurnNavigator):聊天区左侧一条紧凑轨道,每条用户消息一个
 * 短横线 tick。默认按 10px 自然间距挤成一簇、在可视带里垂直居中;只有当自然
 * 高度超过可视带时才压缩成等比铺满——少时是一小簇,多时才撑开。整条轨道接管
 * 指针:悬浮显示对应消息预览,点击跳转并居中。
 */

/** 相邻 tick 的自然间距与轨道两端内边距(px)。 */
const MARK_SPACING_PX = 10
const RAIL_INSET_PX = 6

type AxisItem = { anchor: number; text: string }

function nearestItem(marks: HTMLElement[], clientY: number): number {
  if (marks.length === 0) return -1
  let best = -1
  let bestDist = Infinity
  for (let i = 0; i < marks.length; i++) {
    const rect = marks[i].getBoundingClientRect()
    const dist = Math.abs(clientY - (rect.top + rect.height / 2))
    if (dist < bestDist) {
      bestDist = dist
      best = i
    }
  }
  return best
}

export function ConversationAxis({
  nodes,
  scrollRef,
}: {
  nodes: TranscriptNode[]
  scrollRef: React.RefObject<HTMLDivElement | null>
}) {
  const items = useMemo<AxisItem[]>(
    () =>
      nodes
        .filter(
          (n): n is Extract<TranscriptNode, { kind: 'user' }> =>
            n.kind === 'user' && n.anchor !== undefined,
        )
        .map((n) => ({ anchor: n.anchor!, text: n.text })),
    [nodes],
  )

  const [preview, setPreview] = useState(-1)
  const [active, setActive] = useState(-1)
  const marksRef = useRef<HTMLElement[]>([])
  const railRef = useRef<HTMLElement | null>(null)
  const previewId = useId()

  const setMark = useCallback(
    (index: number) => (el: HTMLElement | null) => {
      if (el) marksRef.current[index] = el
    },
    [],
  )

  const jump = useCallback(
    (anchor: number) => {
      const scroller = scrollRef.current
      if (!scroller) return
      const target = scroller.querySelector<HTMLElement>(`[data-user-anchor="${anchor}"]`)
      if (!target) return
      target.scrollIntoView({ behavior: 'smooth', block: 'center' })
    },
    [scrollRef],
  )

  // active = 可视带中线最近的那条用户消息;滚动与内容变化时重算。
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
          best = items.findIndex((it) => String(it.anchor) === node.dataset.userAnchor)
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
  }, [items, scrollRef])

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
          const i = nearestItem(marksRef.current, e.clientY)
          if (i >= 0) jump(items[i].anchor)
        }}
        onPointerMove={(e) => setPreview(nearestItem(marksRef.current, e.clientY))}
        onPointerLeave={() => setPreview(-1)}
      >
        <div className="axis-marks">
          {items.map((item, index) => {
            const state =
              index === active ? 'active' : index === previewIndex ? 'preview' : 'rest'
            return (
              <div
                key={item.anchor}
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
