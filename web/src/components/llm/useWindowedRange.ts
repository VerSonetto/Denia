/**
 * 自实现固定行高窗口化:scrollTop → 可见行区间(含 overscan),
 * 只在区间变化时 setState,滚动本身不触发父级重渲染以外的开销。
 */

import { useCallback, useEffect, useRef, useState } from 'react'

export function useWindowedRange(rowCount: number, rowHeight: number, overscan = 6) {
  const scrollRef = useRef<HTMLDivElement | null>(null)
  const viewportHeightRef = useRef(0)
  const [range, setRange] = useState({ start: 0, end: 0 })

  const recompute = useCallback(() => {
    const el = scrollRef.current
    if (!el) return
    const start = Math.max(0, Math.floor(el.scrollTop / rowHeight) - overscan)
    const visible = Math.ceil(el.clientHeight / rowHeight) + overscan * 2
    const end = Math.min(rowCount, start + visible)
    setRange((prev) => (prev.start === start && prev.end === end ? prev : { start, end }))
  }, [rowCount, rowHeight, overscan])

  // 视口高度测量(打开面板/拖动窗口时跟随)。
  useEffect(() => {
    const el = scrollRef.current
    if (!el) return
    viewportHeightRef.current = el.clientHeight
    const observer = new ResizeObserver(() => {
      viewportHeightRef.current = el.clientHeight
      recompute()
    })
    observer.observe(el)
    return () => observer.disconnect()
  }, [recompute])

  // 行数变化(过滤/排序)与首帧都要重算,并把越界 scrollTop 收回。
  useEffect(() => {
    const el = scrollRef.current
    if (el) {
      const maxScroll = Math.max(0, rowCount * rowHeight - el.clientHeight)
      if (el.scrollTop > maxScroll) el.scrollTop = maxScroll
    }
    recompute()
  }, [rowCount, rowHeight, recompute])

  const onScroll = useCallback(() => recompute(), [recompute])

  return { scrollRef, range, onScroll }
}
