import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { CSSProperties } from 'react'
import { setLocale, t } from './i18n'
import { relativeTime } from './relativeTime'
import { subscribeServerEvents, subscribeServerStatus } from './serverEvents'
import SessionsPage from './pages/SessionsPage'
import * as api from './api'
import {
  addSessionLocal,
  bumpCatalogTick,
  deleteSessionAction,
  deleteWorkspaceAction,
  findWorkspaceBlank,
  getActiveWorkspace,
  markLocalBlank,
  notify,
  refreshList,
  setActiveId,
  setConnLost,
  setPendingWsId,
  replaceRunningIds,
  setRunningStatus,
  clearCompacting,
  pruneCompactingByRunning,
  useActiveId,
  useConnLost,
  useLocalBlankIds,
  useRunningIds,
  useSessions,
  useSessionsLoaded,
  useStartedIds,
  useToast,
  useWorkspaces,
} from './appStore'
import { useBrowserSidebar } from './hooks/useBrowserSidebar'
import { useIsMobile } from './hooks/useIsMobile'
import { useSidePaneController, useTerminalReconcile } from './hooks/useSidePane'
import { SidePane } from './components/SidePane'
import { TerminalHost } from './components/TerminalHost'
import { closeTerminal } from './terminalApi'
import { dropScope, peekSidePane } from './sidePaneStore'
import { useModelSelection } from './modelSelectionStore'
import type { SidePaneTab } from './sidePane'
import {
  IconCaretRight,
  IconClose,
  IconCollapseAll,
  IconExpandAll,
  IconFolder,
  IconFolderPlus,
  IconGear,
  IconMenu,
  IconPanelClose,
  IconPanelOpen,
  IconPlus,
  IconSearch,
  IconTrash,
} from './components/icons'
import { DirPicker } from './components/DirPicker'
import { ConfirmDialog } from './components/ConfirmDialog'
import { RemoteGate, shouldShowGate } from './components/RemoteGate'
import { RemoteBanner } from './components/RemoteBanner'

// 设置/浏览器面板按需加载,减少首屏主包体积。
const LazySettingsModal = lazy(() =>
  import('./components/SettingsModal').then((module) => ({ default: module.SettingsModal })),
)
const LazyBrowserPanel = lazy(() => import('./components/BrowserPanel'))
import { sessionDisplayTitle } from './sessionDisplay'
import type { SessionSummary, WorkspaceRecord } from './types'

/**
 * 应用外壳:布局 + 全局事件订阅(SSE 推送)+ URL 导航 + 启动引导。
 * 业务状态与动作全部在 `appStore`,本组件只做编排,不持有会话数据。
 */
export type Notify = (kind: 'ok' | 'err', message: string) => void

// 侧栏收起偏好与过渡时长(与 styles.css 的 shell 列宽过渡同步)。
const SIDEBAR_COLLAPSED_KEY = 'denia.sidebar-collapsed'
const SIDEBAR_TRANSITION_MS = 260

export default function App() {
  const sessions = useSessions()
  const workspaces = useWorkspaces()
  const activeId = useActiveId()
  const startedIds = useStartedIds()
  const localBlankIds = useLocalBlankIds()
  const sessionsLoaded = useSessionsLoaded()
  const connLost = useConnLost()
  const toast = useToast()

  // running 集合:服务端 SSE 推送驱动(侧栏圆点与状态栏同源)。
  const runningIds = useRunningIds()

  const [settingsOpen, setSettingsOpen] = useState(false)
  // 浏览器侧栏编排(会话绑定 / 收尾自动销毁 / 刷新恢复)全部在 hook 内。
  // 面板标签现在统一由 SidePane 承载,所以这里只用它的 `userClose`
  // 把"用户手动收起浏览器"这个意图转达给 hook(停止 AI 自动重开)。
  const { userClose: closeBrowserSidebar } = useBrowserSidebar(activeId)

  /* ---- 右侧面板(ZCode 式多标签工作区) ---- */

  // 面板归属:当前会话的工作区路径。会话头 cwd 不可变,所以直接取。
  const activeSessionForPane = activeId
    ? (sessions.find((s) => s.id === activeId) ?? null)
    : null
  // 没有活跃会话(空白态)时退回"待发工作区":终端要有起点目录,
  // 而用户在空白页也已经选好了工作区。
  const paneWorkspacePath = activeSessionForPane?.cwd ?? getActiveWorkspace()?.path ?? null
  const pane = useSidePaneController(activeId, paneWorkspacePath)
  // 对话区当前选择的模型:右侧「审查」面板的 AI 生成提交信息复用它
  // (同一个用户不该为同一件事选两次模型)。
  const modelSelection = useModelSelection()

  // 启动/切会话时对账终端:清掉指向已消失 PTY 的僵尸标签。
  useTerminalReconcile(activeId, true, (liveIds) => {
    const live = new Set(liveIds)
    pane.update((current) => {
      const stale = current.tabs.filter(
        (tab) => tab.type === 'terminal' && !live.has(tab.id),
      )
      if (stale.length === 0) return current
      const staleIds = new Set(stale.map((tab) => tab.id))
      const tabs = current.tabs.filter((tab) => !staleIds.has(tab.id))
      if (tabs.length === 0) return { tabs: [], activeTabId: '' }
      return {
        tabs,
        activeTabId: tabs.some((tab) => tab.id === current.activeTabId)
          ? current.activeTabId
          : (tabs[tabs.length - 1]?.id ?? ''),
      }
    })
  })

  /** 新建一个终端标签。 */
  const openTerminalTab = useCallback(() => {
    pane.openPanel('terminal', { cwd: paneWorkspacePath ?? undefined })
  }, [pane, paneWorkspacePath])

  /** 终端进程退出:抄 ZCode 的 `lMt` —— 最后一个终端退出时连带收起面板。 */
  const handleTerminalExit = useCallback(
    (tabId: string, _exitCode: number) => {
      // 不自动关标签:用户可能还要看退出前的输出。只在最后一个终端退出
      // 且面板里没有别的标签时收起面板,避免留一个空壳。
      const current = pane.state
      const remaining = current.tabs.filter((tab) => tab.id !== tabId)
      if (remaining.length === 0) {
        // 没有别的标签:收起面板(标签保留,用户可以再展开看到退出画面)。
        pane.setCollapsed(true)
      }
    },
    [pane],
  )

  /** shell 解析出来后回填标签标题。 */
  const handleTerminalTitle = useCallback(
    (tabId: string, title: string) => {
      pane.update((current) => {
        const tab = current.tabs.find((item) => item.id === tabId)
        if (!tab || tab.title === title) return current
        return {
          ...current,
          tabs: current.tabs.map((item) =>
            item.id === tabId ? { ...item, title } : item,
          ),
        }
      })
    },
    [pane],
  )

  /** 关闭一个终端标签:同时关掉服务端 PTY(否则进程泄漏)。 */
  const handleCloseTerminalTab = useCallback((tabs: SidePaneTab[]) => {
    for (const tab of tabs) {
      if (tab.type !== 'terminal') continue
      void closeTerminal(tab.id).catch(() => {
        // 服务端已经没有这个终端(重启过/已被回收):不是错误。
      })
    }
  }, [])

  /**
   * 会话被删除后的面板收尾。
   *
   * 两件事都必须做,否则都会泄漏:
   * - **终端进程**:标签没了但 PTY 还在跑,进程与内存都不回收;
   * - **localStorage 桶**:该 scope 永远不会再被读到,却仍占着分桶上限名额
   *   (`SCOPE_LIMIT`),长期使用会把真正在用的 scope 挤出去。
   */
  const cleanupSessionPane = useCallback((sessionId: string) => {
    const tabs = peekSidePane(sessionId).tabs
    handleCloseTerminalTab(tabs)
    dropScope(sessionId)
  }, [handleCloseTerminalTab])
  const [sidebarSearchOpen, setSidebarSearchOpen] = useState(false)
  const [sidebarExpandAllTick, setSidebarExpandAllTick] = useState(0)
  const [sidebarAllExpanded, setSidebarAllExpanded] = useState(false)

  /* ---- 侧栏收起(dsh 折叠控制栏) ---- */

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
  // 首次渲染即收起时不播放入场动画(dsh 语义);reduced-motion 直接切换。
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

  // 滑动落位后再弹搜索框并聚焦,过渡中聚焦会被移动的输入框甩开(dsh 同款时序)。
  const expandSidebarWithSearch = useCallback(() => {
    setSidebarCollapsed(false)
    window.setTimeout(() => setSidebarSearchOpen(true), SIDEBAR_TRANSITION_MS + 20)
  }, [])

  /* ---- 手机端侧栏抽屉 ---- */

  // 窄屏下侧栏不是"56px 控制栏",而是**覆盖式抽屉**:默认完全收起,让出
  // 整屏给对话;由顶栏汉堡按钮唤出,点遮罩或选中会话后自动关闭。
  // 这与桌面端的折叠语义不同(桌面端收起后仍留一条控制栏),所以单独用一个
  // 状态,不去复用 sidebarCollapsed —— 否则手机用户的选择会被写进桌面偏好。
  const isMobile = useIsMobile()
  const [drawerOpen, setDrawerOpen] = useState(false)
  // 切到桌面尺寸时抽屉状态必须清掉:否则从手机横屏切到平板宽度,抽屉会
  // 以一个"没有遮罩、也不该存在"的浮层留在屏幕上。
  useEffect(() => {
    if (!isMobile) setDrawerOpen(false)
  }, [isMobile])

  // Ctrl+B / Cmd+B 切换侧栏(VSCode 惯例)。
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (
        (event.ctrlKey || event.metaKey) &&
        !event.altKey &&
        !event.shiftKey &&
        event.key.toLowerCase() === 'b'
      ) {
        event.preventDefault()
        setSidebarCollapsed((v) => !v)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  // URL 会话状态:挂载时读 hash;列表加载完成后判定恢复或放弃。
  const [pendingHashId] = useState<string | null>(() => {
    const match = window.location.hash.match(/^#s=([^#]+)$/)
    return match ? decodeURIComponent(match[1]) : null
  })
  const [hashSettled, setHashSettled] = useState(false)
  const hashRestoredRef = useRef(false)
  const firstRenderRef = useRef(true)

  // 目录流:capability 决定 native 还是 browse。
  const [pickerOpen, setPickerOpen] = useState(false)
  const [, setPicking] = useState(false)
  const capabilityRef = useRef<'native' | 'browse'>('native')
  // 扫码落地页判定必须**只求值一次**并锁存。
  //
  // 不能在渲染时现算:`RemoteGate` 兑换成功后会 `history.replaceState` 抹掉
  // URL 里的票据(票据是一次性凭据,不该留在地址栏),那会触发 App 重渲染,
  // 现算就会把 gate 判成 false —— 结果是 PIN 输入框还没显示就被卸载,页面
  // 直接切到控制台,而会话 cookie 尚未下发,所有接口 401,用户看到的是
  // "与服务器的连接断开"。锁存后 gate 生命周期由 RemoteGate 自己掌握。
  const [gateActive] = useState(() => shouldShowGate())
  // 本页是不是"主机自己的控制台"。远程来客(手机)在横幅上不该看到关闭
  // 隧道这类主机侧动作;判定走 capability 的同一份来源(服务端按请求
  // 来源给),不靠前端猜。
  const [localClient, setLocalClient] = useState(true)

  const applyConsoleSettings = useCallback(() => {
    api
      .getSettings()
      .then((data) => {
        const console = data.namespaces.find((ns) => ns.ns === 'console')
        const theme = (console?.value.theme as string) ?? 'system'
        if (theme === 'system') delete document.documentElement.dataset.theme
        else document.documentElement.dataset.theme = theme
        setLocale((console?.value.locale as string) ?? 'zh')
        bumpCatalogTick()
      })
      .catch(() => {})
  }, [])

  useEffect(() => {
    api
      .pickerCapability()
      .then((cap) => {
        capabilityRef.current = cap.kind === 'browse' ? 'browse' : 'native'
        // 远程连接状态下 browse 也用于远程来客;真正的来源判定读
        // 远程状态的 `local` 字段(见 RemoteBanner 的订阅)。
        setLocalClient(cap.kind !== 'browse' || !cap.remote)
      })
      .catch(() => {})
  }, [])

  useEffect(() => {
    applyConsoleSettings()
  }, [applyConsoleSettings])

  /* ---- 全局 SSE 推送:列表/设置失效通知 ---- */

  const sseOpenedRef = useRef(false)
  const sseWasLostRef = useRef(false)

  useEffect(() => {
    // 全应用共用一条 `/api/events`(见 serverEvents.ts):每条 EventSource
    // 都独占一个同源连接名额,浏览器上限 6 条,多开会让后续 fetch 永久排队。
    const unsubscribe = subscribeServerEvents((type, payload) => {
      const parsed = payload as {
        type?: string
        id?: string
        running?: boolean
        ids?: string[]
      }
      if (type === 'sessions-updated') void refreshList()
      else if (type === 'settings-updated') applyConsoleSettings()
      else if (type === 'running-snapshot' && Array.isArray(parsed.ids)) {
        const ids = parsed.ids.filter((id) => typeof id === 'string')
        replaceRunningIds(ids)
        // 刷新恢复的关键一步:sessionStorage 里的"正在压缩"标记要拿服务端
        // 权威的 running 校对 —— 会话已不在运行集中,说明任务早已收尾
        // (摘要事件会在快照重放里出现),标记必须清掉,否则会一直转圈。
        pruneCompactingByRunning(Object.fromEntries(ids.map((id) => [id, true])))
      } else if (type === 'running-changed' && parsed.id) {
        setRunningStatus(parsed.id, parsed.running === true)
        // 运行位转 false:无论压缩成功与否都收尾(失败另有广播,这里兜底)。
        if (parsed.running !== true) clearCompacting(parsed.id)
      } else if (type === 'compaction-failed' && parsed.id) {
        // 压缩后台任务的失败/无物可压出口:清掉"正在压缩"标记。
        // 成功不走这里 —— 成功有 append-only 的 compaction-summary 事件。
        clearCompacting(parsed.id)
      }
    })
    const unsubscribeStatus = subscribeServerStatus((up) => {
      if (up) {
        setConnLost(false)
        // 仅首次连接与断线重连后重拉列表,避免 onopen 抖动造成无限刷新。
        if (!sseOpenedRef.current) {
          sseOpenedRef.current = true
          void refreshList()
        } else if (sseWasLostRef.current) {
          sseWasLostRef.current = false
          void refreshList()
        }
      } else {
        sseWasLostRef.current = true
        setConnLost(true)
      }
    })
    return () => {
      unsubscribe()
      unsubscribeStatus()
    }
  }, [applyConsoleSettings])

  // 首屏加载:一次全量列表(SSE onopen 也会触发,幂等)。
  useEffect(() => {
    void refreshList()
  }, [])

  /* ---- URL 会话状态 ---- */

  useEffect(() => {
    if (!sessionsLoaded || hashRestoredRef.current) return
    hashRestoredRef.current = true
    if (pendingHashId !== null && sessions.some((s) => s.id === pendingHashId)) {
      setActiveId(pendingHashId)
    }
    setHashSettled(true)
  }, [sessions, sessionsLoaded, pendingHashId])

  useEffect(() => {
    if (firstRenderRef.current) {
      firstRenderRef.current = false
      return
    }
    const target = activeId === null ? '' : `#s=${encodeURIComponent(activeId)}`
    if (window.location.hash !== target) {
      window.history.replaceState(null, '', target || window.location.pathname)
    }
  }, [activeId])

  useEffect(() => {
    const onHashChange = () => {
      const match = window.location.hash.match(/^#s=([^#]+)$/)
      const id = match ? decodeURIComponent(match[1]) : null
      if (id === null) {
        setActiveId(null)
      } else if (sessions.some((s) => s.id === id)) {
        setActiveId(id)
      }
    }
    window.addEventListener('hashchange', onHashChange)
    return () => window.removeEventListener('hashchange', onHashChange)
  }, [sessions])

  /* ---- 会话/工作区派生 ---- */

  const activeSession = activeId
    ? (sessions.find((s) => s.id === activeId) ?? null)
    : null

  const isBlank = useCallback(
    (s: SessionSummary) => !s.excerpt && !startedIds[s.id],
    [startedIds],
  )

  // 首条消息后(或已有历史)会话与工作区绑定,不能换。
  const locked =
    activeSession !== null &&
    (!!startedIds[activeSession.id] || !isBlank(activeSession))

  const focusSession = useCallback((sessionId: string, wsId?: string) => {
    setActiveId(sessionId, wsId ?? null)
  }, [])

  /* ---- 启动自动导航(dsh watchNavigation) ---- */

  const recentWorkspace = useCallback((): WorkspaceRecord | null => {
    let best: WorkspaceRecord | null = null
    let bestTime = -1
    for (const ws of workspaces) {
      const memberTime = Math.max(
        ws.createdAt,
        ...sessions
          .filter((s) => s.cwd === ws.path)
          .map((s) => s.created_at),
      )
      if (memberTime > bestTime) {
        bestTime = memberTime
        best = ws
      }
    }
    return best
  }, [workspaces, sessions])

  const connectWorkspace = useCallback(
    (ws: WorkspaceRecord) => {
      if (activeSession && activeSession.cwd === ws.path) {
        setPendingWsId(ws.id)
        return
      }
      const blank = findWorkspaceBlank(ws.path)
      if (blank) {
        focusSession(blank.id, ws.id)
        return
      }
      setActiveId(null, ws.id)
    },
    [activeSession, focusSession],
  )

  const bootedRef = useRef(false)
  useEffect(() => {
    if (bootedRef.current || activeId) return
    if (!hashSettled) return
    if (workspaces.length === 0) return
    bootedRef.current = true
    const recent = recentWorkspace()
    if (recent) connectWorkspace(recent)
  }, [workspaces, activeId, hashSettled, recentWorkspace, connectWorkspace])

  /* ---- 空白会话不保留:仅清理本页面自己创建的空白会话 ---- */

  // 清理作用域必须是 `localBlankIds`(本页面创建的会话),绝不能按"全局列表
  // 里非活跃即空白"去扫:会话列表是跨页面/跨实例共享的(正式与开发实例还
  // 共用同一份数据目录),那样会把别的标签页刚创建、正在使用的会话当垃圾删掉,
  // 表现为"新建会话没选中工作区 + 发不出消息 + 列表里看不到新会话",以及
  // 发消息时报 `session not found`。浏览器直接关闭留下的残留空白,由后端
  // 启动时一次性清扫(见 SessionStore::sweep_stale_blanks)。
  const prevActiveIdRef = useRef<string | null>(null)
  useEffect(() => {
    const prev = prevActiveIdRef.current
    prevActiveIdRef.current = activeId
    if (prev === null || prev === activeId) return
    if (!localBlankIds[prev]) return
    // 焦点从本页面创建的空白会话切走:它没有任何内容,直接清理,不留账本残影。
    const left = sessions.find((s) => s.id === prev)
    if (left && isBlank(left)) {
      deleteSessionAction(prev).catch(() => {
        // 删除失败(如网络抖动):残留空白无害,下次切走或后端启动清扫会兜底。
      })
    }
  }, [activeId, sessions, localBlankIds, isBlank])

  /* ---- 显式"新建会话" ---- */

  const startSession = useCallback(
    async (targetWsId?: string) => {
      const activeWs = getActiveWorkspace()
      const target =
        workspaces.find((w) => w.id === targetWsId) ??
        (activeSession
          ? workspaces.find((w) => w.path === activeSession.cwd) ?? null
          : activeWs) ??
        recentWorkspace()
      if (!target) {
        setActiveId(null, null)
        return
      }
      // 侧栏"新建会话"总是创建全新会话,不复用空白(空白由"离开即删"
      // 与"非活跃清扫"自动清理,不会堆积)。
      try {
        const { session } = await api.createSession({ workspaceId: target.id })
        const summary: SessionSummary = {
          id: session.id,
          created_at: session.created_at,
          excerpt: null,
          title: null,
          cwd: session.cwd,
          sandbox: session.sandbox,
          cwd_alive: true,
        }
        addSessionLocal(summary, target.id)
        // 登记为本页面创建的空白会话:只有它允许被本页面的"切走即删"清理。
        markLocalBlank(summary.id)
        setActiveId(summary.id, target.id)
        void refreshList()
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      }
    },
    [workspaces, activeSession, recentWorkspace],
  )

  /* ---- 目录流入口 ---- */

  const openDirectoryFlow = useCallback(() => {
    if (capabilityRef.current === 'native') {
      setPicking(true)
      api
        .pickDirectory()
        .then(async ({ path }) => {
          setPicking(false)
          if (!path) return
          try {
            const { workspace } = await api.createWorkspace(path)
            void refreshList()
            connectWorkspace(workspace)
            notify('ok', t('workspaceAdded'))
          } catch (error) {
            notify('err', `${t('folderError')}:${error instanceof Error ? error.message : error}`)
          }
        })
        .catch((error) => {
          setPicking(false)
          notify('err', `${t('folderError')}:${error instanceof Error ? error.message : error}`)
        })
    } else {
      setPickerOpen(true)
    }
  }, [connectWorkspace])

  const adoptFromBrowse = useCallback(
    async (path: string) => {
      const { workspace } = await api.createWorkspace(path)
      void refreshList()
      connectWorkspace(workspace)
      notify('ok', t('workspaceAdded'))
    },
    [connectWorkspace],
  )

  /* ---- 删除(自定义确认框,替代 window.confirm) ---- */

  const [confirmReq, setConfirmReq] = useState<{
    title: string
    desc: string
    danger?: boolean
    onConfirm: () => void
  } | null>(null)

  const deleteWorkspace = useCallback(
    (ws: WorkspaceRecord) => {
      setConfirmReq({
        title: t('confirmDeleteWorkspace'),
        desc: t('confirmDeleteWorkspaceDesc', { name: ws.title }),
        danger: true,
        onConfirm: () => {
          // 工作区删除会级联删掉它的全部会话,所以它们的面板桶与终端
          // 进程也要一起收掉 —— 只删服务端数据会留下跑着的 PTY。
          const doomed = sessions.filter((s) => s.cwd === ws.path).map((s) => s.id)
          void deleteWorkspaceAction(ws.id)
            .then(() => {
              for (const id of doomed) cleanupSessionPane(id)
              notify('ok', t('workspaceDeleted'))
            })
            .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
        },
      })
    },
    [sessions, cleanupSessionPane],
  )

  const deleteSession = useCallback((session: SessionSummary) => {
    setConfirmReq({
      title: t('confirmDeleteSession'),
      desc: t('confirmDeleteSessionDesc', { title: sessionDisplayTitle(session) }),
      danger: true,
      onConfirm: () => {
        void deleteSessionAction(session.id)
          .then(() => {
            // 会话没了,它的面板桶也就永远不会再被读到:清掉,
            // 免得长期使用后 localStorage 里堆满孤儿桶、把在用的 scope
            // 挤出上限。同时把该会话的终端进程一并回收。
            cleanupSessionPane(session.id)
            notify('ok', t('sessionDeleted'))
          })
          .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
      },
    })
  }, [cleanupSessionPane])

  const deleteUngrouped = useCallback(
    (items: SessionSummary[]) => {
      if (items.length === 0) return
      setConfirmReq({
        title: t('confirmDeleteUngrouped'),
        desc: t('confirmDeleteUngroupedDesc', { n: items.length }),
        danger: true,
        onConfirm: () => {
          void (async () => {
            for (const session of items) {
              if (runningIds[session.id]) {
                throw new Error(t('sessionRunningDelete'))
              }
              await deleteSessionAction(session.id)
              cleanupSessionPane(session.id)
            }
          })()
            .then(() => notify('ok', t('sessionDeleted')))
            .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
        },
      })
    },
    [runningIds, cleanupSessionPane],
  )

  const openSession = useCallback(
    (id: string, wsId?: string) => {
      setActiveId(id, wsId ?? null)
      // 手机上选中会话后自动收起抽屉 —— 否则用户点完还得再点一次遮罩
      // 才能看到刚打开的对话。
      setDrawerOpen(false)
    },
    [],
  )

  // 扫码落地页:URL 带票据时整页交给远程门,先兑换会话再进控制台。
  if (gateActive) {
    return <RemoteGate />
  }

  return (
    <div
      className="shell"
      data-collapsed={sidebarCollapsed || undefined}
      data-mobile={isMobile || undefined}
      data-drawer={isMobile && drawerOpen ? 'open' : undefined}
    >
      {isMobile && drawerOpen && (
        <div
          className="drawer-scrim"
          role="presentation"
          onClick={() => setDrawerOpen(false)}
        />
      )}
      {/* 抽屉里的任何点击都收起抽屉:手机上点"设置""新建会话"之后,如果
          抽屉还盖在上面,用户会以为操作没生效(实测就是这样)。会话列表
          项自身也会关闭,这里兜住其余入口,不用逐个改 onClick。 */}
      <aside
        className="sidebar"
        aria-hidden={isMobile && !drawerOpen ? true : undefined}
        onClick={
          isMobile
            ? (event) => {
                // 折叠/展开等纯图标操作不关闭抽屉,避免点了箭头抽屉就消失。
                const target = event.target as HTMLElement
                if (target.closest('.sidebar-toggle, .rail-btn')) return
                setDrawerOpen(false)
              }
            : undefined
        }
      >
        {wideMounted && (
          <div
            className={`sidebar-wide${wideFadeOut ? ' fade-out' : ''}${wideEnter ? ' wide-in' : ''}`}
          >
            <div className="sidebar-logo">
              <span className="brand-text">Denia</span>
              <button
                type="button"
                className="icon-btn sidebar-toggle"
                title={t('sidebarCollapse')}
                aria-label={t('sidebarCollapse')}
                onClick={() => setSidebarCollapsed(true)}
              >
                <IconPanelClose size={16} />
              </button>
            </div>
            <button
              type="button"
              className="sidebar-new-btn"
              onClick={() => void startSession()}
            >
              <IconPlus size={14} />
              {t('newSession')}
            </button>
            <div className="sidebar-region">
              <div className="sidebar-region-head">
                <span className="label">{t('workspacesTitle')}</span>
                <div className="sidebar-region-actions">
                  <button
                    type="button"
                    className={`icon-btn${sidebarSearchOpen ? ' active' : ''}`}
                    title={t('searchSessions')}
                    aria-label={t('searchSessions')}
                    aria-pressed={sidebarSearchOpen}
                    onClick={() => setSidebarSearchOpen((open) => !open)}
                  >
                    <IconSearch size={14} />
                  </button>
                  <button
                    type="button"
                    className="icon-btn"
                    title={sidebarAllExpanded ? t('collapseAllWorkspaces') : t('expandAllWorkspaces')}
                    aria-label={sidebarAllExpanded ? t('collapseAllWorkspaces') : t('expandAllWorkspaces')}
                    onClick={() => setSidebarExpandAllTick((tick) => tick + 1)}
                  >
                    {sidebarAllExpanded ? <IconCollapseAll size={14} /> : <IconExpandAll size={14} />}
                  </button>
                  <button
                    type="button"
                    className="icon-btn"
                    title={t('addWorkspace')}
                    aria-label={t('addWorkspace')}
                    onClick={openDirectoryFlow}
                  >
                    <IconPlus size={14} />
                  </button>
                </div>
              </div>
              <SidebarWorkspaces
                sessions={sessions}
                workspaces={workspaces}
                activeId={activeId}
                runningIds={runningIds}
                searchOpen={sidebarSearchOpen}
                onSearchOpenChange={setSidebarSearchOpen}
                expandAllTick={sidebarExpandAllTick}
                onAllExpandedChange={setSidebarAllExpanded}
                onOpenSession={openSession}
                onNewSession={(wsId) => void startSession(wsId)}
                onDeleteWorkspace={(ws) => void deleteWorkspace(ws)}
                onDeleteSession={(session) => void deleteSession(session)}
                onDeleteUngrouped={(items) => void deleteUngrouped(items)}
              />
            </div>
            <nav className="sidebar-foot" aria-label={t('navSettings')}>
              <button
                type="button"
                className="sidebar-nav"
                onClick={() => setSettingsOpen(true)}
              >
                <IconGear size={16} />
                <span>{t('navSettings')}</span>
              </button>
            </nav>
          </div>
        )}
        {railMounted && (
          <div className={`sidebar-rail${railEnter ? ' enter' : ''}`}>
            <button
              type="button"
              className="rail-btn"
              title={t('sidebarExpand')}
              aria-label={t('sidebarExpand')}
              onClick={() => setSidebarCollapsed(false)}
            >
              <IconPanelOpen size={18} />
            </button>
            <button
              type="button"
              className="rail-btn accent"
              title={t('newSession')}
              aria-label={t('newSession')}
              onClick={() => void startSession()}
            >
              <IconPlus size={16} />
            </button>
            <button
              type="button"
              className="rail-btn"
              title={t('addWorkspace')}
              aria-label={t('addWorkspace')}
              onClick={openDirectoryFlow}
            >
              <IconFolderPlus size={16} />
            </button>
            <button
              type="button"
              className="rail-btn"
              title={t('searchSessions')}
              aria-label={t('searchSessions')}
              onClick={expandSidebarWithSearch}
            >
              <IconSearch size={16} />
            </button>
            <div className="rail-spacer" />
            <div className="rail-foot">
              <button
                type="button"
                className="rail-btn"
                title={t('navSettings')}
                aria-label={t('navSettings')}
                onClick={() => setSettingsOpen(true)}
              >
                <IconGear size={16} />
              </button>
            </div>
          </div>
        )}
      </aside>
      <main className="main">
        {/* 手机端顶栏:抽屉开关 + 品牌。桌面端由 CSS 隐藏 —— 桌面有常驻
            侧栏,不需要这个入口。 */}
        {isMobile && (
          <div className="mobile-bar">
            <button
              type="button"
              className="mobile-bar-btn"
              aria-label={t('sidebarExpand')}
              aria-expanded={drawerOpen}
              onClick={() => setDrawerOpen((open) => !open)}
            >
              <IconMenu size={18} />
            </button>
            <span className="mobile-bar-title">Denia</span>
          </div>
        )}
        <RemoteBanner local={localClient} />
        <div className="main-row">
          <div className="page-pane">
            <SessionsPage
              key={activeId ?? 'draft'}
              activeId={activeId}
              locked={locked}
              hasStarted={activeId ? !!startedIds[activeId] : false}
              onSelectWorkspace={(ws) => {
                // 发消息前可切;已发消息后锁定(dsh 语义)。
                if (locked) {
                  notify('err', t('lockedWorkspace'))
                  return
                }
                connectWorkspace(ws)
              }}
              onOpenPicker={() => {
                if (workspaces.length === 0) openDirectoryFlow()
                else setPickerOpen(true)
              }}
              onAddWorkspace={openDirectoryFlow}
              sidePaneOpen={!pane.collapsed && pane.state.tabs.length > 0}
              onToggleSidePane={() => {
                // 从折叠态点开时,如果还没有任何标签,顺手开一个审查面板 ——
                // 否则用户点了按钮只看到引导页,还要再点一次才有内容。
                if (pane.collapsed && pane.state.tabs.length === 0) {
                  pane.openPanel('review')
                  return
                }
                pane.toggle()
              }}
            />
          </div>
          {/* 侧边面板在手机上不渲染:390px 宽放不下「对话 + 面板」两列,
              面板会把自己挤成一条缝,还会把主列的输入区顶出屏幕。
              手机用户要终端/文件,用抽屉里的入口或直接开新会话更实际。 */}
          {!isMobile && (
          <SidePane
            sessionId={activeId}
            state={pane.state}
            collapsed={pane.collapsed}
            workspacePath={paneWorkspacePath}
            supportsBrowser={true}
            modelSelection={modelSelection}
            recentClosed={pane.recentClosed}
            onChange={(updater) => pane.update(updater)}
            onCollapsedChange={(next) => pane.setCollapsed(next)}
            onRememberClosed={(tabs) => {
              // 终端标签关闭时连带关掉服务端 PTY:标签没了但进程还在跑
              // 就是泄漏。TerminalPanel 卸载时也会关一次(幂等),
              // 这里显式做是为了在"标签被移除但组件还没卸载"的窗口里
              // 立刻释放进程。
              handleCloseTerminalTab(tabs)
              pane.rememberClosed(tabs)
            }}
            onForgetClosed={(id) => pane.forgetClosed(id)}
            onOpenPanel={(type) => {
              if (type === 'terminal') openTerminalTab()
              else pane.openPanel(type)
            }}
            renderTerminals={(tabs: SidePaneTab[], activeId2: string, paneVisible: boolean) => (
              <TerminalHost
                tabs={tabs}
                activeId={activeId2}
                cwd={paneWorkspacePath ?? ''}
                paneVisible={paneVisible}
                onTitle={handleTerminalTitle}
                onExit={handleTerminalExit}
                onNewTerminal={openTerminalTab}
              />
            )}
            renderBrowser={(_tab, visible) => (
              <Suspense fallback={<div className="empty-hint">{t('loading')}</div>}>
                <LazyBrowserPanel visible={visible} onClose={closeBrowserSidebar} />
              </Suspense>
            )}
          />
          )}
        </div>
      </main>
      {settingsOpen && (
        <Suspense fallback={<div className="empty-hint">{t('loading')}</div>}>
          <LazySettingsModal
            notify={notify}
            onClose={() => {
              setSettingsOpen(false)
              applyConsoleSettings()
            }}
          />
        </Suspense>
      )}
      {pickerOpen && (
        <DirPicker
          onPick={adoptFromBrowse}
          onClose={() => setPickerOpen(false)}
          onError={(message) => notify('err', `${t('folderError')}:${message}`)}
        />
      )}
      {connLost && <div className="conn-lost">{t('connectionLost')}</div>}
      {toast && <div className={`toast ${toast.kind}`}>{toast.message}</div>}
      <ConfirmDialog
        open={confirmReq !== null}
        title={confirmReq?.title ?? ''}
        desc={confirmReq?.desc ?? ''}
        danger={confirmReq?.danger}
        onConfirm={() => {
          confirmReq?.onConfirm()
          setConfirmReq(null)
        }}
        onCancel={() => setConfirmReq(null)}
      />
    </div>
  )
}

/* ---- 侧栏工作区树(抄 dsh WorkspaceBrowser 交互形态) ---- */

const UNGROUPED_KEY = 'ungrouped'
const COLLAPSED_LIMIT = 5

/**
 * 血缘嵌套排序(抄 dsh parentSessionId 列表嵌套):子会话紧跟其父会话
 * 之后(先序遍历);父不在本组(跨组/已删)的子会话留在顶层。
 * `hideSubtreeOf` 里的会话只渲染自身,其整棵后代子树不进列表(折叠)。
 */
function nestByParent(
  members: SessionSummary[],
  hideSubtreeOf?: ReadonlySet<string>,
): { session: SessionSummary; depth: number }[] {
  const ids = new Set(members.map((s) => s.id))
  const byParent = new Map<string, SessionSummary[]>()
  const roots: SessionSummary[] = []
  for (const session of members) {
    const parent = session.parent_session
    if (parent && ids.has(parent)) {
      const list = byParent.get(parent)
      if (list) list.push(session)
      else byParent.set(parent, [session])
    } else {
      roots.push(session)
    }
  }
  const out: { session: SessionSummary; depth: number }[] = []
  const walk = (list: SessionSummary[], depth: number) => {
    for (const session of list) {
      out.push({ session, depth })
      if (hideSubtreeOf?.has(session.id)) continue
      walk(byParent.get(session.id) ?? [], depth + 1)
    }
  }
  walk(roots, 0)
  return out
}

const TIME_UNIT_KEYS = {
  minutes: 'timeMinutes',
  hours: 'timeHours',
  days: 'timeDays',
  months: 'timeMonths',
  years: 'timeYears',
} as const

/** 会话行尾相对时间:now 桶不带"前"。 */
function relativeTimeLabel(at: number, now: number): string {
  const { unit, n } = relativeTime(at, now)
  if (unit === 'now') return t('timeNow')
  return t('timeAgo', { t: t(TIME_UNIT_KEYS[unit], { n }) })
}

/** 一组会话的血缘计量:每个会话的后代总数与"有运行中后代"的祖先集合。 */
function lineageStats(
  members: SessionSummary[],
  runningIds: Record<string, boolean>,
): { descendantCount: Map<string, number>; runningAncestors: Set<string> } {
  const ids = new Set(members.map((s) => s.id))
  const parentOf = new Map<string, string>()
  for (const session of members) {
    if (session.parent_session && ids.has(session.parent_session)) {
      parentOf.set(session.id, session.parent_session)
    }
  }
  const descendantCount = new Map<string, number>()
  for (const session of members) {
    const seen = new Set<string>()
    let cursor = parentOf.get(session.id)
    while (cursor && !seen.has(cursor)) {
      seen.add(cursor)
      descendantCount.set(cursor, (descendantCount.get(cursor) ?? 0) + 1)
      cursor = parentOf.get(cursor)
    }
  }
  const runningAncestors = new Set<string>()
  for (const session of members) {
    if (!runningIds[session.id]) continue
    const seen = new Set<string>()
    let cursor = parentOf.get(session.id)
    while (cursor && !seen.has(cursor)) {
      seen.add(cursor)
      runningAncestors.add(cursor)
      cursor = parentOf.get(cursor)
    }
  }
  return { descendantCount, runningAncestors }
}

function SidebarWorkspaces({
  sessions,
  workspaces,
  activeId,
  runningIds,
  searchOpen,
  onSearchOpenChange,
  expandAllTick,
  onAllExpandedChange,
  onOpenSession,
  onNewSession,
  onDeleteWorkspace,
  onDeleteSession,
  onDeleteUngrouped,
}: {
  sessions: SessionSummary[]
  workspaces: WorkspaceRecord[]
  activeId: string | null
  runningIds: Record<string, boolean>
  searchOpen: boolean
  onSearchOpenChange: (open: boolean) => void
  expandAllTick: number
  onAllExpandedChange: (allExpanded: boolean) => void
  onOpenSession: (id: string, wsId?: string) => void
  onNewSession: (wsId?: string) => void
  onDeleteWorkspace: (ws: WorkspaceRecord) => void
  onDeleteSession: (session: SessionSummary) => void
  onDeleteUngrouped: (sessions: SessionSummary[]) => void
}) {
  const [expanded, setExpanded] = useState<Record<string, boolean>>({})
  const [showAll, setShowAll] = useState<Record<string, boolean>>({})
  // 血缘折叠:sessionId → 是否收起其后代子树(默认展开)。
  const [collapsedChildren, setCollapsedChildren] = useState<Record<string, boolean>>({})
  // 行尾相对时间的时钟;60s 一跳足够分钟级分桶。
  const [now, setNow] = useState(() => Date.now())
  const [query, setQuery] = useState('')
  const searchInputRef = useRef<HTMLInputElement>(null)
  const lastExpandTickRef = useRef(0)

  // 10k 会话时避免每次 get/sessionIds 都做 O(n) find;一次建 Map,O(1) 取行。
  const sessionsById = useMemo(
    () => new Map(sessions.map((session) => [session.id, session])),
    [sessions],
  )

  // 工作区路径集合:ungrouped 判定与"是否已有同路径工作区"都改走 Set,
  // 把每次渲染的 O(会话数 × 工作区数) 比较降为 O(会话数)。
  const workspacePaths = useMemo(
    () => new Set(workspaces.map((workspace) => workspace.path)),
    [workspaces],
  )

  const membersOf = useCallback(
    (ws: WorkspaceRecord) =>
      ws.sessionIds
        .map((id) => sessionsById.get(id))
        .filter((s): s is SessionSummary => !!s && s.cwd === ws.path),
    [sessionsById],
  )

  const ungrouped = useMemo(
    () => sessions.filter((s) => s.cwd === undefined || !workspacePaths.has(s.cwd)),
    [sessions, workspacePaths],
  )

  const groupKeys = useCallback(() => {
    const keys = workspaces.map((ws) => ws.id)
    if (ungrouped.length > 0) keys.push(UNGROUPED_KEY)
    return keys
  }, [workspaces, ungrouped.length])

  const isGroupOpen = useCallback(
    (key: string, fallback: boolean) => expanded[key] ?? fallback,
    [expanded],
  )

  useEffect(() => {
    if (!activeId) return
    const ws = workspaces.find((item) => item.sessionIds.includes(activeId))
    if (ws) {
      setExpanded((previous) => (previous[ws.id] ? previous : { ...previous, [ws.id]: true }))
      return
    }
    const activeSession = sessions.find((s) => s.id === activeId)
    if (activeSession && !workspaces.some((w) => w.path === activeSession.cwd)) {
      setExpanded((previous) =>
        previous[UNGROUPED_KEY] === false ? { ...previous, [UNGROUPED_KEY]: true } : previous,
      )
    }
  }, [activeId, workspaces, sessions])

  useEffect(() => {
    const keys = groupKeys()
    if (keys.length === 0) {
      onAllExpandedChange(false)
      return
    }
    const allOpen = keys.every((key) => {
      if (key === UNGROUPED_KEY) return isGroupOpen(key, true)
      const members = membersOf(workspaces.find((w) => w.id === key)!)
      return isGroupOpen(key, members.some((s) => s.id === activeId))
    })
    onAllExpandedChange(allOpen)
  }, [expanded, groupKeys, isGroupOpen, membersOf, workspaces, activeId, onAllExpandedChange])

  useEffect(() => {
    if (expandAllTick === 0 || expandAllTick === lastExpandTickRef.current) return
    lastExpandTickRef.current = expandAllTick
    const keys = groupKeys()
    const nextExpanded = keys.every((key) => {
      if (key === UNGROUPED_KEY) return isGroupOpen(key, true)
      const members = membersOf(workspaces.find((w) => w.id === key)!)
      return isGroupOpen(key, members.some((s) => s.id === activeId))
    })
    const wantExpand = !nextExpanded
    const next: Record<string, boolean> = {}
    for (const key of keys) next[key] = wantExpand
    setExpanded((prev) => ({ ...prev, ...next }))
    // 全部展开时连带展开血缘折叠;收起只收组,不动子树。
    if (wantExpand) setCollapsedChildren({})
  }, [expandAllTick, groupKeys, isGroupOpen, membersOf, workspaces, activeId])

  useEffect(() => {
    if (!searchOpen) {
      setQuery('')
      return
    }
    const id = requestAnimationFrame(() => searchInputRef.current?.focus())
    return () => cancelAnimationFrame(id)
  }, [searchOpen])

  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 60_000)
    return () => window.clearInterval(id)
  }, [])

  // 活动会话的折叠祖先自动展开,保证侧栏始终可见当前会话。
  useEffect(() => {
    if (!activeId) return
    setCollapsedChildren((prev) => {
      let changed = false
      const next = { ...prev }
      const seen = new Set<string>()
      let cursor = sessionsById.get(activeId)?.parent_session
      while (cursor && !seen.has(cursor)) {
        seen.add(cursor)
        if (next[cursor]) {
          next[cursor] = false
          changed = true
        }
        cursor = sessionsById.get(cursor)?.parent_session
      }
      return changed ? next : prev
    })
  }, [activeId, sessionsById])

  const normalizedQuery = query.trim().toLowerCase()
  const matchesQuery = useCallback(
    (session: SessionSummary) => {
      if (!normalizedQuery) return true
      return sessionDisplayTitle(session).toLowerCase().includes(normalizedQuery)
    },
    [normalizedQuery],
  )

  const filteredGroups = workspaces
    .map((ws) => {
      const members = membersOf(ws).filter(matchesQuery)
      return { ws, members }
    })
    .filter((group) => (normalizedQuery ? group.members.length > 0 : true))

  const filteredUngrouped = ungrouped.filter(matchesQuery)
  const searching = normalizedQuery.length > 0
  const empty =
    filteredGroups.length === 0 &&
    filteredUngrouped.length === 0 &&
    (searching || (workspaces.length === 0 && ungrouped.length === 0))

  return (
    <div className="sidebar-section">
      {searchOpen && (
        <div className="sidebar-search">
          <IconSearch size={13} />
          <input
            ref={searchInputRef}
            type="search"
            className="sidebar-search-input"
            value={query}
            placeholder={t('searchSessionsPlaceholder')}
            aria-label={t('searchSessions')}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key !== 'Escape') return
              setQuery('')
              onSearchOpenChange(false)
            }}
          />
          {query && (
            <button
              type="button"
              className="icon-btn sidebar-search-clear"
              title={t('searchSessionsClear')}
              aria-label={t('searchSessionsClear')}
              onClick={() => setQuery('')}
            >
              <IconClose size={12} />
            </button>
          )}
        </div>
      )}
      <div className="session-list">
        {empty && (
          <div className="empty-hint">
            {searching ? t('searchSessionsEmpty') : t('emptySessions')}
          </div>
        )}
        {filteredGroups.map(({ ws, members }) => {
          const open = searching || isGroupOpen(ws.id, members.some((s) => s.id === activeId))
          const { descendantCount, runningAncestors } = lineageStats(members, runningIds)
          const collapsedSet = searching
            ? undefined
            : new Set(
                Object.keys(collapsedChildren).filter(
                  (id) => collapsedChildren[id] && descendantCount.has(id),
                ),
              )
          const nested = nestByParent(members, collapsedSet)
          const visible =
            searching || showAll[ws.id] ? nested : nested.slice(0, COLLAPSED_LIMIT)
          return (
            <div className={`ws-group${open ? ' open' : ''}`} key={ws.id}>
              <div className="ws-group-head-row">
                <button
                  type="button"
                  className={`ws-group-head${members.some((s) => s.id === activeId) ? ' active' : ''}`}
                  title={ws.path}
                  aria-expanded={open}
                  onClick={() => {
                    const fallback = members.some((s) => s.id === activeId)
                    setExpanded((prev) => ({
                      ...prev,
                      [ws.id]: !(prev[ws.id] ?? fallback),
                    }))
                  }}
                >
                  <span className="ws-folder">
                    <IconFolder size={14} />
                  </span>
                  <span className="name">{ws.title}</span>
                  <span className="count">{members.length}</span>
                </button>
                <span className="row-actions">
                  <button
                    type="button"
                    className="row-action-btn"
                    title={t('newSessionIn', { name: ws.title })}
                    onClick={() => onNewSession(ws.id)}
                  >
                    <IconPlus size={12} />
                  </button>
                  <button
                    type="button"
                    className="row-action-btn danger"
                    title={t('deleteWorkspace')}
                    onClick={() => onDeleteWorkspace(ws)}
                  >
                    <IconTrash size={12} />
                  </button>
                </span>
              </div>
              <div className={`ws-group-body${open ? ' open' : ''}`}>
                <div className="ws-group-body-inner">
                  {visible.map(({ session, depth }) => (
                    <SessionRow
                      key={session.id}
                      session={session}
                      depth={depth}
                      active={session.id === activeId}
                      running={!!runningIds[session.id]}
                      now={now}
                      childCount={descendantCount.get(session.id) ?? 0}
                      collapsed={collapsedChildren[session.id] === true}
                      childRunning={runningAncestors.has(session.id)}
                      onToggleCollapse={() =>
                        setCollapsedChildren((prev) => ({
                          ...prev,
                          [session.id]: !prev[session.id],
                        }))
                      }
                      onOpen={() => onOpenSession(session.id, ws.id)}
                      onDelete={() => onDeleteSession(session)}
                    />
                  ))}
                  {!searching && members.length > COLLAPSED_LIMIT && (
                    <button
                      type="button"
                      className="expand-rest"
                      onClick={() =>
                        setShowAll((prev) => ({ ...prev, [ws.id]: !showAll[ws.id] }))
                      }
                    >
                      {showAll[ws.id]
                        ? t('collapse')
                        : t('expandRest', { n: members.length - COLLAPSED_LIMIT })}
                    </button>
                  )}
                </div>
              </div>
            </div>
          )
        })}
        {filteredUngrouped.length > 0 && (
          <div
            className={`ws-group${searching || isGroupOpen(UNGROUPED_KEY, true) ? ' open' : ''}`}
          >
            <div className="ws-group-head-row">
              <button
                type="button"
                className={`ws-group-head${filteredUngrouped.some((s) => s.id === activeId) ? ' active' : ''}`}
                aria-expanded={searching || isGroupOpen(UNGROUPED_KEY, true)}
                onClick={() =>
                  setExpanded((prev) => ({
                    ...prev,
                    [UNGROUPED_KEY]: !(prev[UNGROUPED_KEY] ?? true),
                  }))
                }
              >
                <span className="ws-folder muted">
                  <IconFolder size={14} />
                </span>
                <span className="name">{t('ungrouped')}</span>
                <span className="count">{filteredUngrouped.length}</span>
              </button>
              <span className="ungrouped-actions">
                <button
                  type="button"
                  className="ungrouped-clear"
                  title={t('deleteUngroupedDesc', { n: filteredUngrouped.length })}
                  onClick={() => onDeleteUngrouped(filteredUngrouped)}
                >
                  {t('deleteUngrouped')}
                </button>
              </span>
            </div>
              <div
                className={`ws-group-body${searching || isGroupOpen(UNGROUPED_KEY, true) ? ' open' : ''}`}
              >
                <div className="ws-group-body-inner">
                  {(() => {
                    const { descendantCount, runningAncestors } = lineageStats(
                      filteredUngrouped,
                      runningIds,
                    )
                    const collapsedSet = searching
                      ? undefined
                      : new Set(
                          Object.keys(collapsedChildren).filter(
                            (id) => collapsedChildren[id] && descendantCount.has(id),
                          ),
                        )
                    return nestByParent(filteredUngrouped, collapsedSet).map(
                      ({ session, depth }) => (
                        <SessionRow
                          key={session.id}
                          session={session}
                          depth={depth}
                          active={session.id === activeId}
                          running={!!runningIds[session.id]}
                          now={now}
                          childCount={descendantCount.get(session.id) ?? 0}
                          collapsed={collapsedChildren[session.id] === true}
                          childRunning={runningAncestors.has(session.id)}
                          onToggleCollapse={() =>
                            setCollapsedChildren((prev) => ({
                              ...prev,
                              [session.id]: !prev[session.id],
                            }))
                          }
                          onOpen={() => onOpenSession(session.id)}
                          onDelete={() => onDeleteSession(session)}
                        />
                      ),
                    )
                  })()}
                </div>
              </div>
          </div>
        )}
      </div>
    </div>
  )
}

function SessionRow({
  session,
  depth = 0,
  active,
  running,
  now,
  childCount = 0,
  collapsed = false,
  childRunning = false,
  onOpen,
  onDelete,
  onToggleCollapse,
}: {
  session: SessionSummary
  /** 血缘嵌套深度:0 = 顶层,>0 = 分支子会话(缩进显示)。 */
  depth?: number
  active: boolean
  running: boolean
  /** 行尾相对时间的当前时刻(侧栏级 60s 时钟)。 */
  now: number
  /** 后代总数:>0 时渲染折叠钮,折叠时显示 +n 徽标。 */
  childCount?: number
  collapsed?: boolean
  /** 有后代正在运行(折叠时把运行圆点顶到本行)。 */
  childRunning?: boolean
  onOpen: () => void
  onDelete: () => void
  onToggleCollapse: () => void
}) {
  // 标题前补"(父)"提示,让用户一眼看出这是分支出来的子会话(已嵌套显示,
  // 但同工作区里有多个分支时文字也帮回忆),只对子会话生效,顶会不画。
  const isSubagent = Boolean(session.subagent)
  const isBranch = depth > 0 && !isSubagent
  const isChild = depth > 0
  const showCaret = childCount > 0
  const dotRunning = running || (collapsed && childRunning)
  return (
    <div
      className={`session-row-wrap${active ? ' active' : ''}${isChild ? ' nested' : ''}`}
      style={isChild ? ({ '--depth': depth } as CSSProperties) : undefined}
    >
      <span className="session-caret-slot">
        {showCaret && (
          <button
            type="button"
            className={`session-caret${collapsed ? '' : ' open'}`}
            title={collapsed ? t('expandChildren', { n: childCount }) : t('collapseChildren')}
            aria-expanded={!collapsed}
            onClick={onToggleCollapse}
          >
            <IconCaretRight size={10} />
          </button>
        )}
      </span>
      <button type="button" className="session-row" onClick={onOpen}>
        <span className="lead">
          {session.cwd_alive === false ? (
            <span className="dot err" title={t('deadCwd')} />
          ) : dotRunning ? (
            <span
              className="dot run"
              title={collapsed && !running ? t('childRunningHint') : undefined}
            />
          ) : null}
        </span>
        <span className="excerpt">{sessionDisplayTitle(session)}</span>
        {isSubagent && (
          <span className="subagent-tag" title={t('subagentTagHint')} aria-label={t('subagentTagHint')}>
            {t('subagentTag')}
          </span>
        )}
        {isBranch && (
          <span className="branch-tag" title={t('branchTagHint')} aria-label={t('branchTagHint')}>
            {t('branchTag')}
          </span>
        )}
        {showCaret && collapsed && (
          <span className="child-count" title={t('childSessions', { n: childCount })}>
            +{childCount}
          </span>
        )}
      </button>
      {session.created_at > 0 && <span className="session-time">{relativeTimeLabel(session.created_at, now)}</span>}
      <span className="row-actions">
        <button
          type="button"
          className="row-action-btn danger"
          title={t('deleteSession')}
          disabled={running}
          onClick={(event) => {
            event.stopPropagation()
            onDelete()
          }}
        >
          <IconTrash size={12} />
        </button>
      </span>
    </div>
  )
}
