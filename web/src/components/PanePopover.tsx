/**
 * 面板内的浮层(新增菜单 / 标签总览):portal 到 body 后按锚点定位。
 *
 * # 为什么必须 portal
 *
 * 标签栏的滚动区是 `overflow-x:auto; overflow-y:hidden`(横向滚动、纵向不滚),
 * 祖先 `.side-pane-inner` 又是 `overflow:hidden`。下拉菜单如果**内联**渲染在
 * 触发按钮旁边,就会被这两层 overflow 裁掉 —— 而且是**从下方内容处被截断**,
 * 不是被挡住:`overflow` 裁切发生在合成之前,`z-index` 再高也没用。
 *
 * 这正是实测踩到的 bug。ZCode 用 Radix 的 Popover/DropdownMenu,它们默认
 * portal 到 body,所以没有这个问题;denia 不引 UI 库,就自己实现这一步。
 *
 * # 定位方式:先量后摆
 *
 * 首帧用 `visibility:hidden` 布局拿到真实尺寸,再算最终 left/top。否则
 * 元素会先从 (0,0) 闪一下再跳到锚点旁(TurnUsageCard 用的是同一套做法)。
 */

import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'

/** 浮层与锚点之间的间距(px)。 */
const ANCHOR_GAP = 4
/** 浮层与视口边缘的最小留白(px)。 */
const VIEWPORT_MARGIN = 8

/** 测量阶段的占位样式:已布局但不可见,避免闪烁。 */
const MEASURE_STYLE = {
  position: 'fixed' as const,
  left: 0,
  top: 0,
  visibility: 'hidden' as const,
}

export interface PanePopoverProps {
  /** 触发按钮;用于取锚点矩形,并作为"外部点击"判定的内部节点。 */
  anchorRef: React.RefObject<HTMLElement | null>
  open: boolean
  onClose(): void
  /** 相对锚点水平对齐:`start` 贴左缘,`end` 贴右缘(默认 `end`)。 */
  align?: 'start' | 'end'
  className?: string
  role?: string
  ariaLabel?: string
  children: React.ReactNode
}

export function PanePopover({
  anchorRef,
  open,
  onClose,
  align = 'end',
  className,
  role = 'menu',
  ariaLabel,
  children,
}: PanePopoverProps) {
  const panelRef = useRef<HTMLDivElement | null>(null)
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)

  /** 量浮层尺寸 + 锚点位置,算出最终落点并钳进视口。 */
  const place = useCallback(() => {
    const anchor = anchorRef.current
    const panel = panelRef.current
    // 拿不到锚点说明调用方漏了 `ref={anchorRef}` —— 这是编程错误,不是运行时状况。
    // **不猜位置**:先前这里兜底放到视口左上角,结果菜单"逃逸"到界面最左侧,
    // 看起来像布局 bug,反而掩盖了真正的原因(锚点 ref 没挂上)。
    // 现在的处理:保持未定位(不渲染可见浮层)+ 显式告警,让问题一眼可见。
    if (!anchor) {
      console.warn('[PanePopover] 锚点未挂载,浮层不渲染;检查调用方是否漏了 ref')
      setPos(null)
      return
    }
    if (!panel) return
    const a = anchor.getBoundingClientRect()
    const p = panel.getBoundingClientRect()
    const vw = window.innerWidth
    const vh = window.innerHeight

    // 水平:贴锚点左缘或右缘,再钳进视口。
    const rawLeft = align === 'end' ? a.right - p.width : a.left
    const left = Math.max(
      VIEWPORT_MARGIN,
      Math.min(rawLeft, vw - p.width - VIEWPORT_MARGIN),
    )

    // 垂直:优先朝下(菜单在标签栏下方展开,符合直觉);
    // 下方放不下才翻到锚点上方,同样钳进视口。
    const below = a.bottom + ANCHOR_GAP
    const above = a.top - ANCHOR_GAP - p.height
    const top =
      below + p.height <= vh - VIEWPORT_MARGIN
        ? below
        : above >= VIEWPORT_MARGIN
          ? above
          : Math.max(VIEWPORT_MARGIN, Math.min(below, vh - p.height - VIEWPORT_MARGIN))

    setPos({ left, top })
  }, [anchorRef, align])

  // 先量后摆:打开时先以隐藏态布局,拿到尺寸再定位。
  useLayoutEffect(() => {
    if (!open) {
      setPos(null)
      return
    }
    place()
  }, [open, place])

  // 打开期间:外点 / Escape 关闭。浮层已 portal,不算"外部"。
  useEffect(() => {
    if (!open) return
    const onPointerDown = (event: PointerEvent) => {
      const node = event.target
      if (!(node instanceof Node)) return
      if (anchorRef.current?.contains(node) === true) return
      if (panelRef.current?.contains(node) === true) return
      onClose()
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.stopPropagation()
        onClose()
      }
    }
    document.addEventListener('pointerdown', onPointerDown)
    document.addEventListener('keydown', onKeyDown)
    return () => {
      document.removeEventListener('pointerdown', onPointerDown)
      document.removeEventListener('keydown', onKeyDown)
    }
  }, [open, onClose, anchorRef])

  // 锚点是 fixed 定位,但页面滚动/窗口缩放会改变锚点位置:跟着重算,
  // 不让浮层飘在旧位置。
  useEffect(() => {
    if (!open) return
    let frame = 0
    const follow = () => {
      cancelAnimationFrame(frame)
      frame = requestAnimationFrame(place)
    }
    window.addEventListener('scroll', follow, { capture: true, passive: true })
    window.addEventListener('resize', follow)
    return () => {
      cancelAnimationFrame(frame)
      window.removeEventListener('scroll', follow, { capture: true })
      window.removeEventListener('resize', follow)
    }
  }, [open, place])

  if (!open) return null

  return createPortal(
    <div
      ref={panelRef}
      className={className}
      role={role}
      aria-label={ariaLabel}
      // 定位是否已完成。显式暴露成 data 属性(而不是让调用方去嗅探内联样式):
      // 首帧是 `visibility:hidden` 的测量态,React 会把样式序列化成
      // `visibility:hidden`(无空格)、`left:0px` —— 靠字符串匹配极易写错,
      // 而"菜单停在测量态"正是"锚点 ref 漏挂"的表现,必须能被稳定断言。
      data-positioned={pos === null ? 'false' : 'true'}
      style={pos === null ? MEASURE_STYLE : { left: pos.left, top: pos.top }}
    >
      {children}
    </div>,
    document.body,
  )
}

export default PanePopover
