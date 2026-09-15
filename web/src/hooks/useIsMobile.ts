import { useEffect, useState } from 'react'

/**
 * 窄屏(手机)判定。
 *
 * 用 `matchMedia` 而不是读 `window.innerWidth` 一次:手机横竖屏切换、分屏、
 * 桌面端拖拽窗口都会改变宽度,布局必须跟着变 —— 只在挂载时读一次的话,
 * 用户把手机横过来就会看到桌面布局挤在窄屏里。
 *
 * 断点与 CSS 的 `@media (max-width: 820px)` 保持一致:JS 与 CSS 用同一个
 * 阈值,避免出现"JS 认为是手机、CSS 认为是桌面"的错位状态。
 */
const MOBILE_QUERY = '(max-width: 820px)'

export function useIsMobile(): boolean {
  const [mobile, setMobile] = useState(() => matchesMobile())

  useEffect(() => {
    if (typeof window.matchMedia !== 'function') return
    const query = window.matchMedia(MOBILE_QUERY)
    const update = () => setMobile(query.matches)
    // 先同步一次:挂载与订阅之间窗口可能已经变过尺寸。
    update()
    query.addEventListener('change', update)
    return () => query.removeEventListener('change', update)
  }, [])

  return mobile
}

function matchesMobile(): boolean {
  if (typeof window.matchMedia !== 'function') return false
  return window.matchMedia(MOBILE_QUERY).matches
}
