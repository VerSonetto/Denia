import { useCallback, useEffect, useRef, useState } from 'react'

// 侧栏收起偏好与过渡时长(与 styles.css 的 shell 列宽过渡同步)。
const SIDEBAR_COLLAPSED_KEY = 'denia.sidebar-collapsed'
const SIDEBAR_TRANSITION_MS = 260

/**
 * 侧栏收起(折叠控制栏)。
 * setSidebarSearchOpen 是 App 的搜索框状态:滑动落位后再弹搜索框。
 */
export function useSidebarCollapse(setSidebarSearchOpen: (open: boolean) => void) {
  // 收起态是 56px 图标控制栏而不是零宽:常驻开关/新建/添加/搜索/设置,
  // 与展开态各行顺序一一对应。偏好进 localStorage(纯 UI 态,不进 console 配置)。
  const [sidebarCollapsed, setSidebarCollapsed] = useState(() => {
    try {
      return window.localStorage.getItem(SIDEBAR_COLLAPSED_KEY) === '1'
    } catch {
      return false
    }
  })
  useEffect(() => {
    try {
      window.localStorage.setItem(SIDEBAR_COLLAPSED_KEY, sidebarCollapsed ? '1' : '0')
    } catch {
      /* 隐私模式等存储不可用:仅失去持久化,不影响交互 */
    }
  }, [sidebarCollapsed])

  // 交叉过渡编排:收起时宽内容原地淡出(列宽滑动由 CSS 裁切,内容冻结宽度
  // 不重排),落位后卸载宽态、控制栏淡入;展开时控制栏让位、宽内容淡入。
  // 首次渲染即收起时不播放入场动画;reduced-motion 直接切换。
  const [wideMounted, setWideMounted] = useState(!sidebarCollapsed)
  const [railMounted, setRailMounted] = useState(sidebarCollapsed)
  const [wideFadeOut, setWideFadeOut] = useState(false)
  const [wideEnter, setWideEnter] = useState(false)
  const [railEnter, setRailEnter] = useState(false)
  const prevCollapsedRef = useRef(sidebarCollapsed)
  useEffect(() => {
    const toggled = prevCollapsedRef.current !== sidebarCollapsed
    prevCollapsedRef.current = sidebarCollapsed
    const reduced =
      typeof window.matchMedia === 'function' &&
      window.matchMedia('(prefers-reduced-motion: reduce)').matches
    if (!toggled || reduced) {
      setWideMounted(!sidebarCollapsed)
      setRailMounted(sidebarCollapsed)
      setWideFadeOut(false)
      setWideEnter(false)
      setRailEnter(false)
      return
    }
    if (sidebarCollapsed) {
      setWideFadeOut(true)
      setWideEnter(false)
      const timer = window.setTimeout(() => {
        setWideMounted(false)
        setRailMounted(true)
        setRailEnter(true)
      }, SIDEBAR_TRANSITION_MS)
      return () => window.clearTimeout(timer)
    }
    setRailMounted(false)
    setRailEnter(false)
    setWideFadeOut(false)
    setWideMounted(true)
    setWideEnter(true)
  }, [sidebarCollapsed])

  // 滑动落位后再弹搜索框并聚焦,过渡中聚焦会被移动的输入框甩开。
  const expandSidebarWithSearch = useCallback(() => {
    setSidebarCollapsed(false)
    window.setTimeout(() => setSidebarSearchOpen(true), SIDEBAR_TRANSITION_MS + 20)
  }, [setSidebarSearchOpen])

  return {
    sidebarCollapsed,
    setSidebarCollapsed,
    wideMounted,
    railMounted,
    wideFadeOut,
    wideEnter,
    railEnter,
    expandSidebarWithSearch,
  }
}
