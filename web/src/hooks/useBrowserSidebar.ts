import { useCallback, useEffect, useRef, useState } from 'react'
import { fetchBrowserState, subscribeBrowserEvents } from '../browserApi'
import { useRunningIds, useSessions, useSessionsLoaded } from '../appStore'

/* ---- 侧栏绑定持久化(跨刷新) ----

   存"侧栏绑定的会话 id":刷新后活跃会话由 URL hash 还原,两边对上侧栏
   才回来。存会话 id 而不是裸开合布尔:布尔恢复不了归属,刷新后不管落在
   哪个会话都会亮出侧栏 —— 那正是会话绑定要消灭的行为。 */

const BROWSER_BINDING_KEY = 'denia.browserSidebar'

function readBinding(): string | null {
  try {
    const raw = window.sessionStorage.getItem(BROWSER_BINDING_KEY)
    // 旧格式的裸标记 '1' 没有归属语义,不识别。
    return raw && raw !== '1' ? raw : null
  } catch {
    /* storage 不可用(隐私模式):退化成纯内存。 */
    return null
  }
}

function writeBinding(id: string | null) {
  try {
    if (id) window.sessionStorage.setItem(BROWSER_BINDING_KEY, id)
    else window.sessionStorage.removeItem(BROWSER_BINDING_KEY)
  } catch {
    /* 写失败只影响刷新恢复,不影响当前页面。 */
  }
}

/** 健康轮询间隔:兜住被广播通道丢掉的控制事件(见 verify)。 */
const HEALTH_POLL_MS = 2500
/** 不健康复核延迟:给"关掉旧 tab 马上开新 tab"留时间窗。 */
const UNHEALTHY_RECHECK_MS = 1500

/**
 * 浏览器侧栏开合编排:会话绑定 + 收尾自动销毁 + 刷新恢复。
 *
 * 侧栏属于"发起可视化请求时用户正在看的会话":AI 带 visualMode 的命令
 * 到达时绑定当前活跃会话并展开;切换会话只是隐藏,绑定期内切回原会话且
 * 浏览器仍有内容则恢复显示;用户手动收起、实例退出、标签清空才解除绑定
 * 销毁侧栏。绑定写 sessionStorage,刷新后随 hash 还原的活跃会话对上即
 * 原样恢复。
 *
 * 浏览器 SSE 负责快(收尾事件一到就响应),健康轮询负责稳:screencast
 * 帧和控制事件共用一条广播通道,高帧率下接收端 Lagged 会静默丢事件,
 * 轮询兜底保证"AI 关光标签 / 进程退出"迟早把侧栏收掉。
 */
export function useBrowserSidebar(activeId: string | null): {
  /** 当前是否应渲染侧栏(绑定会话 === 活跃会话)。 */
  open: boolean
  /** 用户点击面板收起:解除绑定,本轮内 AI 重复请求不再展开。 */
  userClose: () => void
} {
  const sessions = useSessions()
  const sessionsLoaded = useSessionsLoaded()
  const runningIds = useRunningIds()

  const [binding, setBindingState] = useState<string | null>(() => readBinding())
  const setBinding = useCallback((id: string | null) => {
    writeBinding(id)
    setBindingState(id)
  }, [])

  // 事件回调里取"此刻用户在看哪个会话",不进依赖。
  const activeIdRef = useRef(activeId)
  activeIdRef.current = activeId
  const bindingRef = useRef(binding)
  bindingRef.current = binding

  /* ---- 用户手动收起:本轮 AI 不再自动展开,新一轮开始后解除 ---- */

  const dismissedRef = useRef(false)
  const prevRunningRef = useRef<Record<string, boolean>>({})
  useEffect(() => {
    const freshRun = Object.keys(runningIds).some((id) => !prevRunningRef.current[id])
    prevRunningRef.current = runningIds
    if (freshRun) dismissedRef.current = false
  }, [runningIds])

  /* ---- 健康核验:连续两次不健康才销毁 ---- */

  const strikeRef = useRef(0)
  const recheckTimerRef = useRef<number | undefined>(undefined)

  const verify = useCallback(async () => {
    if (!bindingRef.current) return
    const snapshot = await fetchBrowserState().catch(() => null)
    if (!snapshot) return
    if (snapshot.running && snapshot.tabs.length > 0) {
      strikeRef.current = 0
      return
    }
    if (strikeRef.current === 0) {
      // 第一次不健康:先复核一次,瞬时清空(关旧开新)不算收尾。
      strikeRef.current = 1
      window.clearTimeout(recheckTimerRef.current)
      recheckTimerRef.current = window.setTimeout(() => void verify(), UNHEALTHY_RECHECK_MS)
      return
    }
    // 实例退出,或标签清空且复核仍空:侧栏没有可看内容,销毁。
    strikeRef.current = 0
    setBinding(null)
  }, [setBinding])

  // 绑定期内健康轮询:事件被丢也有这条兜底路径。
  useEffect(() => {
    if (!binding) return
    strikeRef.current = 0
    const timer = window.setInterval(() => void verify(), HEALTH_POLL_MS)
    return () => window.clearInterval(timer)
  }, [binding, verify])

  /* ---- 切回绑定会话:立即核验一次,不给死浏览器复活成空壳的机会 ---- */

  const open = binding !== null && binding === activeId
  useEffect(() => {
    if (open) void verify()
  }, [open, verify])

  /* ---- 绑定的会话被删除:绑定悬空,解除 ---- */

  useEffect(() => {
    if (!sessionsLoaded || !binding) return
    if (!sessions.some((session) => session.id === binding)) setBinding(null)
  }, [sessions, sessionsLoaded, binding, setBinding])

  /* ---- 浏览器事件订阅 ---- */

  useEffect(() => {
    const close = subscribeBrowserEvents((event) => {
      if (event.type === 'visual-mode-requested') {
        // 用户手动收起即"不要看":本轮内 AI 重复请求也不再展开。
        if (dismissedRef.current) return
        if (activeIdRef.current === null) return
        setBinding(activeIdRef.current)
      } else if (event.type === 'exited') {
        // 进程退出是确定信号,立即销毁;轮询只兜事件丢失的情况。
        if (bindingRef.current) setBinding(null)
      } else if (event.type === 'tabs-changed') {
        void verify()
      }
    })
    return () => {
      window.clearTimeout(recheckTimerRef.current)
      close()
    }
  }, [verify, setBinding])

  const userClose = useCallback(() => {
    dismissedRef.current = true
    setBinding(null)
  }, [setBinding])

  return { open, userClose }
}
