import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { SessionAnchor } from '../api'
import type { TranscriptNode } from '../fold'
import { t } from '../i18n'

/**
 * 对话轴(对齐 Codex AppThreadUserMessageNavigationRail):聊天区左侧一条
 * 浮轨,每条用户消息一个短横线。少于 4 条不出现。刻度来自后端全量锚点,
 * 不受分页窗口限制;点击未加载的刻度由视图向上翻页覆盖后再定位。
 *
 * 长度不是「悬停单独加长一条」,而是以指针所在刻度为中心向两侧衰减:
 * ±1 / ±2 / ±3 分别落到 0.7 / 0.4 / 0.2。按下后可以不松手拖动,轨道即时
 * 跳到对应消息;单击才平滑滚动,并给目标气泡做一次高亮。
 */

/** 少于这么多条用户消息时整条轨道不渲染。 */
const MIN_ITEMS = 4
/** 预览延迟:指针停住才拉卡片,划过时不闪。 */
const PREVIEW_DELAY_MS = 150
/** 拖动/翻页后的定位校正窗口。 */
const REVEAL_WAIT_MS = 1500
const CORRECT_DELAY_MS = 350
/** Alt+方向键认为「已经对齐」的容差。 */
const ALIGN_TOLERANCE_PX = 24
/** 跳转高亮:文字色 14% 混透明,保持到 35% 再落到 5%。 */
const HIGHLIGHT_MS = 700
const FLASH_PEAK = 'color-mix(in srgb, var(--label-primary) 14%, transparent)'
const FLASH_REST = 'color-mix(in srgb, var(--label-primary) 5%, transparent)'
const EASE = 'cubic-bezier(0.23, 1, 0.32, 1)'
/** 预览卡里产物最多展示几个,多出来的记成 +N。 */
const OUTPUT_LIMIT = 2

interface TurnPreview {
  response: string
  outputs: string[]
}

function basename(path: string): string {
  const cut = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'))
  return cut >= 0 ? path.slice(cut + 1) : path
}

/**
 * 锚点之后、下一条用户消息之前的助手回复与落盘文件。节点只覆盖已加载
 * 窗口,窗口外的刻度没有预览,卡片退回只显示用户原文。
 */
function previewsFor(anchors: readonly SessionAnchor[], nodes: readonly TranscriptNode[]): TurnPreview[] {
  const out: TurnPreview[] = anchors.map(() => ({ response: '', outputs: [] }))
  if (anchors.length === 0 || nodes.length === 0) return out
  const indexBySeq = new Map<number, number>()
  anchors.forEach((anchor, index) => {
    if (!indexBySeq.has(anchor.seq)) indexBySeq.set(anchor.seq, index)
  })
  let current = -1
  const seen = new Set<string>()
  for (const node of nodes) {
    if (node.kind === 'user') {
      current = node.anchor === undefined ? -1 : indexBySeq.get(node.anchor) ?? -1
      seen.clear()
      continue
    }
    if (current < 0) continue
    const slot = out[current]
    if (!slot) continue
    if (node.kind === 'assistant') {
      const text = node.blocks
        .filter((block) => block.kind === 'text')
        .map((block) => block.text)
        .join('\n')
        .trim()
      if (text) slot.response = text
    } else if (node.kind === 'turn-end' && node.produced) {
      for (const file of node.produced) {
        if (seen.has(file.path)) continue
        seen.add(file.path)
        slot.outputs.push(file.path)
      }
    }
  }
  return out
}


/** 预览卡贴在目标按钮右侧、垂直居中,并夹在视口里。 */
function previewStyle(button: HTMLElement | null): React.CSSProperties {
  if (!button) return { visibility: 'hidden' }
  const rect = button.getBoundingClientRect()
  const width = Math.min(320, window.innerWidth - 16)
  const height = 84
  const top = Math.max(8, Math.min(rect.top + rect.height / 2 - height / 2, window.innerHeight - height - 8))
  const left = Math.min(rect.right, window.innerWidth - width - 8)
  return { top, left, width }
}

export function ConversationAxis({
  anchors,
  nodes,
  scrollRef,
  onJumpMiss,
  onJump,
}: {
  anchors: SessionAnchor[]
  /** 已加载的对话节点:给预览卡补助手回复与本轮产物。 */
  nodes: TranscriptNode[]
  scrollRef: React.RefObject<HTMLDivElement | null>
  /** 点击的刻度尚未加载(锚点在分页窗口之外):交由视图翻页覆盖后定位。 */
  onJumpMiss?: (seq: number, behavior: ScrollBehavior) => void
  /** 跳转开始:通知外层解开吸底,免得心跳把滚动拉回底部。 */
  onJump?: () => void
}) {
  const listRef = useRef<HTMLDivElement | null>(null)
  const buttonsRef = useRef<HTMLButtonElement[]>([])
  const previewId = useId()
  const [hover, setHover] = useState(-1)
  const [open, setOpen] = useState(false)
  const [scrub, setScrub] = useState(-1)
  const [active, setActive] = useState(-1)
  const [reducedMotion, setReducedMotion] = useState(false)
  const [previewBox, setPreviewBox] = useState<React.CSSProperties | null>(null)
  /** 按下后拖过别的刻度:松手时吞掉随后的 click,避免再跳一次。 */
  const draggedRef = useRef(false)
  const scrubRef = useRef(-1)
  const pointerRef = useRef<number | null>(null)
  const previewTimer = useRef(0)

  const items = anchors
  const previews = useMemo(() => previewsFor(items, nodes), [items, nodes])

  const clearPreviewTimer = useCallback(() => {
    if (previewTimer.current) window.clearTimeout(previewTimer.current)
    previewTimer.current = 0
  }, [])

  useEffect(() => {
    const media = window.matchMedia('(prefers-reduced-motion: reduce)')
    const apply = () => setReducedMotion(media.matches)
    apply()
    media.addEventListener('change', apply)
    return () => media.removeEventListener('change', apply)
  }, [])

  useEffect(() => () => clearPreviewTimer(), [clearPreviewTimer])

  const indexBySeq = useMemo(() => {
    const map = new Map<number, number>()
    items.forEach((item, index) => {
      if (!map.has(item.seq)) map.set(item.seq, index)
    })
    return map
  }, [items])

  /** 让当前刻度留在轨道自己的滚动口里(刻度多到超出视口时)。 */
  const revealMark = useCallback((index: number) => {
    const list = listRef.current
    const button = buttonsRef.current[index]
    if (!list || !button) return
    const top = button.offsetTop
    const bottom = top + button.offsetHeight
    if (top < list.scrollTop) list.scrollTop = top
    else if (bottom > list.scrollTop + list.clientHeight) {
      list.scrollTop = bottom - list.clientHeight + 1
    }
  }, [])

  const flash = useCallback(
    (seq: number) => {
      const scroller = scrollRef.current
      const row = scroller?.querySelector<HTMLElement>(`[data-user-anchor="${seq}"]`)
      const bubble = row?.querySelector<HTMLElement>('.msg-user')
      if (!bubble) return
      bubble.animate(
        [
          { backgroundColor: FLASH_PEAK },
          { backgroundColor: FLASH_PEAK, offset: 0.35 },
          { backgroundColor: FLASH_REST },
        ],
        { duration: reducedMotion ? 0 : HIGHLIGHT_MS, easing: EASE },
      )
    },
    [reducedMotion, scrollRef],
  )

  const locate = useCallback(
    (seq: number, behavior: ScrollBehavior) => {
      const scroller = scrollRef.current
      const row = scroller?.querySelector<HTMLElement>(`[data-user-anchor="${seq}"]`)
      if (!row) {
        onJumpMiss?.(seq, behavior)
        return false
      }
      onJump?.()
      row.scrollIntoView({ behavior, block: 'start' })
      flash(seq)
      return true
    },
    [flash, onJump, onJumpMiss, scrollRef],
  )

  // 翻页把目标行补进来之后补一次定位:当时行还不在 DOM 里。
  useEffect(() => {
    const pending = scrubRef.current
    if (pending < 0) return
    const seq = items[pending]?.seq
    if (seq === undefined) return
    const scroller = scrollRef.current
    if (!scroller?.querySelector(`[data-user-anchor="${seq}"]`)) return
    const frame = requestAnimationFrame(() => {
      locate(seq, 'instant')
    })
    return () => cancelAnimationFrame(frame)
  }, [items, locate, nodes, scrollRef])

  // 当前刻度 = 视口顶缘之下第一条用户消息(Codex 的 IntersectionObserver
  // rootMargin `-16px`)。量不到的历史刻度保持上一次的结果。
  useEffect(() => {
    const scroller = scrollRef.current
    if (!scroller || items.length === 0) return
    let frame = 0
    const measure = () => {
      const edge = scroller.getBoundingClientRect().top + 16
      let first = -1
      for (const node of scroller.querySelectorAll<HTMLElement>('[data-user-anchor]')) {
        const seq = Number(node.dataset.userAnchor)
        if (!Number.isFinite(seq)) continue
        const index = indexBySeq.get(seq)
        if (index === undefined) continue
        if (node.getBoundingClientRect().bottom > edge) {
          first = index
          break
        }
      }
      if (first >= 0) setActive(first)
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
  }, [indexBySeq, items.length, scrollRef])

  // 视口里的当前刻度始终留在轨道滚动口内。
  useEffect(() => {
    if (active >= 0 && scrub < 0) revealMark(active)
  }, [active, revealMark, scrub])

  const focusIndex = scrub >= 0 ? scrub : hover
  const shownIndex = open ? (focusIndex >= 0 ? focusIndex : active) : -1

  const placePreview = useCallback((index: number) => {
    const button = buttonsRef.current[index]
    setPreviewBox(button ? previewStyle(button) : null)
  }, [])

  useLayoutEffect(() => {
    if (shownIndex < 0) {
      setPreviewBox(null)
      return
    }
    placePreview(shownIndex)
    const onMove = () => placePreview(shownIndex)
    window.addEventListener('resize', onMove)
    window.addEventListener('scroll', onMove, true)
    return () => {
      window.removeEventListener('resize', onMove)
      window.removeEventListener('scroll', onMove, true)
    }
  }, [placePreview, shownIndex])

  const armPreview = useCallback(
    (index: number) => {
      clearPreviewTimer()
      previewTimer.current = window.setTimeout(() => {
        previewTimer.current = 0
        setHover(index)
        setOpen(true)
      }, PREVIEW_DELAY_MS)
    },
    [clearPreviewTimer],
  )

  const pointerIndex = useCallback((event: React.PointerEvent<HTMLElement>) => {
    const list = listRef.current
    if (!list) return -1
    const rect = list.getBoundingClientRect()
    const y = Math.max(rect.top, Math.min(event.clientY, rect.bottom - 1))
    const hit = document.elementFromPoint(rect.left + rect.width / 2, y)
    const button = hit?.closest<HTMLElement>('[data-axis-index]')
    if (!button || !list.contains(button)) return -1
    const index = Number(button.dataset.axisIndex)
    return Number.isInteger(index) ? index : -1
  }, [])

  const releasePointer = useCallback(
    (event: React.PointerEvent<HTMLElement>) => {
      if (pointerRef.current !== event.pointerId) return
      const target = event.currentTarget
      if (target.hasPointerCapture(event.pointerId)) target.releasePointerCapture(event.pointerId)
      pointerRef.current = null
      setScrub(-1)
      scrubRef.current = -1
      if (!open) setHover(-1)
      window.setTimeout(() => {
        draggedRef.current = false
      }, 0)
    },
    [open],
  )

  // Alt+↑/↓ 在用户消息之间跳;目标不在窗口内时交回翻页。
  useEffect(() => {
    if (items.length < MIN_ITEMS) return
    const onKey = (event: KeyboardEvent) => {
      if (
        event.defaultPrevented ||
        !event.altKey ||
        event.shiftKey ||
        event.ctrlKey ||
        event.metaKey ||
        (event.key !== 'ArrowUp' && event.key !== 'ArrowDown')
      ) {
        return
      }
      const scroller = scrollRef.current
      if (!scroller) return
      const target = event.target
      if (target instanceof Element && !scroller.contains(target) && target !== document.body) return
      const direction = event.key === 'ArrowUp' ? -1 : 1
      const edge = scroller.getBoundingClientRect().top
      const rows = [...scroller.querySelectorAll<HTMLElement>('[data-user-anchor]')]
      let next = -1
      if (direction > 0) {
        const passed = rows.findIndex((row) => row.getBoundingClientRect().top > edge + ALIGN_TOLERANCE_PX)
        const current = passed === -1 ? rows.length - 1 : passed - 1
        const seq = Number(rows[current]?.dataset.userAnchor)
        const index = indexBySeq.get(seq) ?? -1
        next = index + 1
      } else {
        for (let i = rows.length - 1; i >= 0; i--) {
          const top = rows[i].getBoundingClientRect().top
          if (Math.abs(top - edge) <= ALIGN_TOLERANCE_PX) {
            next = (indexBySeq.get(Number(rows[i].dataset.userAnchor)) ?? 1) - 1
            break
          }
          if (top < edge) {
            next = indexBySeq.get(Number(rows[i].dataset.userAnchor)) ?? -1
            break
          }
        }
        if (next < 0 && rows.length > 0) next = indexBySeq.get(Number(rows[0].dataset.userAnchor)) ?? -1
      }
      if (next < 0 || next >= items.length) return
      event.preventDefault()
      const seq = items[next].seq
      locate(seq, 'smooth')
      window.setTimeout(() => locate(seq, 'smooth'), CORRECT_DELAY_MS)
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [indexBySeq, items, locate, scrollRef])

  if (items.length < MIN_ITEMS) return null

  const previewIndex = shownIndex
  const previewItem = previewIndex >= 0 ? items[previewIndex] : undefined
  const preview = previewIndex >= 0 ? previews[previewIndex] : undefined
  const label = previewItem?.text.trim() ?? ''
  const response = preview?.response.trim() ?? ''
  const outputs = preview?.outputs ?? []
  const shownOutputs = outputs.slice(0, OUTPUT_LIMIT)
  const extraOutputs = outputs.length - shownOutputs.length

  return (
    <div className="conversation-axis">
      <nav className="axis-rail" aria-label={t('axisLabel')}>
        <div
          ref={listRef}
          className="axis-list"
          data-axis-list=""
          data-scrubbing={scrub >= 0 ? '' : undefined}
          onPointerDown={(event) => {
            if (event.button !== 0) return
            const index = pointerIndex(event)
            if (index < 0) return
            clearPreviewTimer()
            draggedRef.current = false
            pointerRef.current = event.pointerId
            scrubRef.current = index
            event.currentTarget.setPointerCapture(event.pointerId)
            setScrub(index)
            setHover(index)
            setOpen(true)
            locate(items[index].seq, 'instant')
            revealMark(index)
          }}
          onPointerMove={(event) => {
            if (pointerRef.current !== null) {
              if (pointerRef.current !== event.pointerId) return
              if (event.buttons % 2 === 0) {
                releasePointer(event)
                return
              }
              const index = pointerIndex(event)
              if (index < 0 || index === scrubRef.current) return
              draggedRef.current = true
              scrubRef.current = index
              setScrub(index)
              setHover(index)
              locate(items[index].seq, 'instant')
              revealMark(index)
              return
            }
            const index = pointerIndex(event)
            if (index < 0 || index === hover) return
            setHover(index)
            if (open) return
            armPreview(index)
          }}
          onPointerUp={releasePointer}
          onPointerCancel={releasePointer}
          onLostPointerCapture={releasePointer}
          onPointerEnter={() => {
            if (pointerRef.current !== null) return
            if (hover >= 0) armPreview(hover)
          }}
          onPointerLeave={() => {
            if (pointerRef.current !== null) return
            clearPreviewTimer()
            setHover(-1)
            setOpen(false)
          }}
        >
          {items.map((item, index) => {
            return (
              <button
                key={item.seq}
                ref={(element) => {
                  if (element) buttonsRef.current[index] = element
                }}
                type="button"
                className={`axis-mark${index === active ? ' axis-mark-current' : ''}${scrub === index ? ' is-scrub' : ''}`}
                data-axis-index={index}
                aria-current={index === active ? 'true' : undefined}
                aria-describedby={open && previewIndex === index ? previewId : undefined}
                aria-label={t('axisJumpLabel', { n: index + 1 })}
                onClick={() => {
                  if (draggedRef.current) {
                    draggedRef.current = false
                    return
                  }
                  setHover(index)
                  setOpen(true)
                  const seq = item.seq
                  locate(seq, reducedMotion ? 'instant' : 'smooth')
                  window.setTimeout(() => {
                    const scroller = scrollRef.current
                    const row = scroller?.querySelector(`[data-user-anchor="${seq}"]`)
                    if (!row) locate(seq, 'instant')
                  }, REVEAL_WAIT_MS)
                }}
                onFocus={() => {
                  clearPreviewTimer()
                  setHover(index)
                  setOpen(true)
                }}
                onBlur={(event) => {
                  const next = event.relatedTarget
                  if (next instanceof Node && listRef.current?.contains(next)) return
                  setHover(-1)
                  setOpen(false)
                }}
              >
                <span className="axis-mark-line" />
              </button>
            )
          })}
        </div>
        {open && previewItem && previewIndex >= 0 && previewBox && createPortal(
          <div
            id={previewId}
            role="tooltip"
            className="axis-preview"
            style={previewBox}
          >
            <div className="axis-preview-title">
              {label || t('axisEmpty')}
            </div>
            {response && <div className="axis-preview-response">{response}</div>}
            {shownOutputs.length > 0 && (
              <div className="axis-preview-outputs">
                {shownOutputs.map((path) => (
                  <span className="axis-preview-output" key={path} title={path}>
                    {basename(path)}
                  </span>
                ))}
                {extraOutputs > 0 && (
                  <span className="axis-preview-more">+{extraOutputs}</span>
                )}
              </div>
            )}
          </div>,
          document.body,
        )}
      </nav>
    </div>
  )
}
