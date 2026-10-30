import { lazy, Suspense, useCallback, useEffect, useRef, useState } from 'react'
import type { CSSProperties } from 'react'
import { setLocale, t } from './i18n'
import { subscribeServerEvents, subscribeServerStatus } from './serverEvents'
import SessionsPage from './pages/SessionsPage'
import { DeniaWordmark } from './components/DeniaWordmark'
import { UpdateBadge } from './components/updates/UpdateBadge'
import { ensureStartupUpdateCheck, useUpdateDialogOpen } from './updateStore'
import { loadComposer } from './composerMemory'
import { extensionTabFromHash, type ExtensionTab } from './extensionsRoute'
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
import { startQueueScheduler } from './features/conversation/queueScheduler'
import { useSidebarCollapse } from './hooks/useSidebarCollapse'
import { useSplitView } from './hooks/useSplitView'
import { registerOpenFile } from './fileOpen'
import { registerOpenReview, type OpenReviewOptions } from './reviewOpen'
import { setTurnChanges } from './turnChangesStore'
import { scopeKey, upsertTab } from './sidePane'
import { SidePane } from './components/SidePane'
import { TerminalHost } from './components/TerminalHost'
import { closeTerminal } from './terminalApi'
import { dropScope, peekSidePane, setSidePaneCollapsed, updateSidePane } from './sidePaneStore'
import { useModelSelection } from './modelSelectionStore'
import type { SidePaneTab } from './sidePane'
import {
  SPLIT_MAX_RATIO,
  SPLIT_MIN_RATIO,
  openSessionInView,
  type PaneSlot,
  type SplitSide,
} from './splitView'
import {
  IconClose,
  IconCollapseAll,
  IconExpandAll,
  IconFolderPlus,
  IconGear,
  IconMenu,
  IconPanelClose,
  IconPanelOpen,
  IconNewSession,
  IconExtensions,
  IconSessionSearch,
} from './components/icons'
import { DirPicker } from './components/DirPicker'
import { ConfirmDialog } from './components/ConfirmDialog'
import { RemoteGate, shouldShowGate } from './components/RemoteGate'
import { RemoteBanner } from './components/RemoteBanner'
import { SidebarWorkspaces } from './components/sidebar/SidebarWorkspaces'
import { DesktopTitleBar } from './components/DesktopTitleBar'

// 设置/浏览器面板按需加载,减少首屏主包体积。
const LazySettingsModal = lazy(() =>
  import('./components/SettingsModal').then((module) => ({ default: module.SettingsModal })),
)
const LazyBrowserPanel = lazy(() => import('./components/BrowserPanel'))
const LazyExtensionsPage = lazy(() => import('./pages/ExtensionsPage'))
const LazyVersionPage = lazy(() => import('./pages/VersionPage'))
const LazyUpdateDialog = lazy(() => import('./components/updates/UpdateDialog').then((module) => ({ default: module.UpdateDialog })))
import type { SessionSummary, WorkspaceRecord } from './types'

/**
 * 应用外壳:布局 + 全局事件订阅(SSE 推送)+ URL 导航 + 启动引导。
 * 业务状态与动作全部在 `appStore`,本组件只做编排,不持有会话数据。
 */
export type Notify = (kind: 'ok' | 'err', message: string) => void

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
  const [versionOpen, setVersionOpen] = useState(() => window.location.hash === '#version')
  const [extensionTab, setExtensionTab] = useState<ExtensionTab | null>(() => extensionTabFromHash(window.location.hash))
  const updateDialogOpen = useUpdateDialogOpen()
  const openVersion = useCallback(() => {
    setSettingsOpen(false)
    setExtensionTab(null)
    setVersionOpen(true)
    window.location.hash = 'version'
  }, [])
  const closeVersion = useCallback(() => setVersionOpen(false), [])
  const openExtensions = useCallback((tab: ExtensionTab = 'skills') => {
    setVersionOpen(false)
    setExtensionTab(tab)
    window.location.hash = `extensions/${tab}`
  }, [])
  const leaveExtensions = useCallback(() => {
    setVersionOpen(false)
    setExtensionTab(null)
  }, [])
  // 浏览器侧栏编排(会话绑定 / 收尾自动销毁 / 刷新恢复)全部在 hook 内。
  // 面板标签现在统一由 SidePane 承载:可视化请求到达时由下面的回调真正
  // 打开/激活浏览器标签并展开侧栏(此前只记录绑定,所以看不到画面)。
  const visualRequestRef = useRef<(sessionId: string) => void>(() => {})
  const { userClose: dismissBrowserSidebar } = useBrowserSidebar(
    activeId,
    (sessionId) => visualRequestRef.current(sessionId),
  )

  /* ---- 右侧面板(多标签工作区) ---- */

  // 面板归属:当前会话的工作区路径。会话头 cwd 不可变,所以直接取。
  const activeSessionForPane = activeId
    ? (sessions.find((s) => s.id === activeId) ?? null)
    : null
  // 没有活跃会话(空白态)时退回"待发工作区":终端要有起点目录,
  // 而用户在空白页也已经选好了工作区。
  const paneWorkspacePath = activeSessionForPane?.cwd ?? getActiveWorkspace()?.path ?? null
  const pane = useSidePaneController(activeId, paneWorkspacePath)

  /**
   * 可视化请求 → 面板动作。
   *
   * - 请求来自当前会话：直接打开/激活浏览器标签并展开面板——这就是
   *   "AI 调用浏览器工具时自动看到画面"的落点；
   * - 请求来自后台会话：只给那个会话的 scope 预开浏览器标签并展开，
   *   不抢当前会话的视图；用户切回去时画面已经在那里。
   * 同一会话重复请求走 upsert/激活路径，不会叠加多个浏览器标签。
   */
  visualRequestRef.current = (sessionId: string) => {
    if (sessionId === activeId) {
      pane.openPanel('browser')
      return
    }
    updateSidePane(sessionId, (current) => {
      const existing = current.tabs.find((tab) => tab.type === 'browser')
      if (existing) {
        return current.activeTabId === existing.id
          ? current
          : { ...current, activeTabId: existing.id }
      }
      return upsertTab(current, {
        id: `browser:${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`,
        type: 'browser',
        openedAt: Date.now(),
      })
    })
    setSidePaneCollapsed(sessionId, false)
  }

  /** 用户手动收起浏览器：解除 AI 自动展开绑定，并真正收起面板。 */
  const closeBrowserSidebar = useCallback(() => {
    dismissBrowserSidebar()
    pane.setCollapsed(true)
  }, [dismissBrowserSidebar, pane])
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

  /**
   * 把 `pane.openFile` 注册给模块级入口：Markdown 链接渲染是纯函数
   * （见 `fileOpen.ts` 注释），拿不到 hook，只能走这条注册通道。
   *
   * 依赖里带上 `pane.openFile`（它已按 sessionId/workspacePath 记忆），
   * 切会话时会重新注册，指向新的工作区。
   */
  useEffect(() => registerOpenFile(pane.openFile), [pane.openFile])

  /**
   * 收尾变更卡片的「查看变更」:打开审查面板,并把本轮改动交给它。
   *
   * 走 `pane.openPanel('review')` 而不是新建标签 —— 审查是单例面板,里面
   * 已经有工作区的全部改动与逐文件 diff,本轮文件是它们的子集,再开一个
   * 面板只会让人对不上"哪份是权威"。
   *
   * 本轮文件另存到 `turnChangesStore`(内存态,不进 localStorage):审查
   * 面板据此多出一个**不依赖 git** 的来源。理由是审查面板的其余来源全
   * 走 `git status`/`git diff`,工作区不是仓库时它们只有空态 —— 那时
   * 本轮这张卡就是唯一还讲得清"改了什么"的地方,却会因为点开的是空面板
   * 而白做。点文件行进来时额外传 focusPath,面板定位到该文件。
   */
  const openReviewPanel = useCallback(
    (options: OpenReviewOptions = {}) => {
      if (options.files !== undefined && options.files.length > 0) {
        setTurnChanges(scopeKey(activeId), {
          turn: options.turn ?? 0,
          files: options.files,
          at: Date.now(),
        })
      }
      pane.openPanel('review')
      return true
    },
    [pane.openPanel, activeId],
  )
  useEffect(() => registerOpenReview(openReviewPanel), [openReviewPanel])

  /** 终端进程退出:最后一个终端退出时连带收起面板。 */
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

  /* ---- 侧栏收起(折叠控制栏) ---- */

  const {
    sidebarCollapsed,
    setSidebarCollapsed,
    wideMounted,
    railMounted,
    wideFadeOut,
    wideEnter,
    railEnter,
    expandSidebarWithSearch,
  } = useSidebarCollapse(setSidebarSearchOpen)

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

  useEffect(() => {
    if (!gateActive) ensureStartupUpdateCheck()
  }, [gateActive])

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

  // 队列自动发送按会话维度常驻调度:切走会话后原会话本轮结束仍会发送。
  // 放在 App 层而不是会话页:页面卸载不影响调度器，且全局只有一套 running
  // 订阅，不会为后台会话额外建 SSE 连接。
  useEffect(() => startQueueScheduler(), [])

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
    const target = versionOpen ? '#version' : extensionTab ? `#extensions/${extensionTab}` : activeId === null ? '' : `#s=${encodeURIComponent(activeId)}`
    if (window.location.hash !== target) {
      window.history.replaceState(null, '', target || window.location.pathname)
    }
  }, [activeId, extensionTab, versionOpen])

  useEffect(() => {
    const onHashChange = () => {
      const version = window.location.hash === '#version'
      setVersionOpen(version)
      if (version) {
        setExtensionTab(null)
        return
      }
      const tab = extensionTabFromHash(window.location.hash)
      setExtensionTab(tab)
      if (tab) return
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

  // 首条消息后(或已有历史)会话与工作区绑定,不能换。分屏副窗格按它
  // 自己的会话判定(不能借用主会话的状态)。
  const lockedFor = useCallback(
    (id: string) => {
      const session = sessions.find((s) => s.id === id)
      return session !== null && session !== undefined && (!!startedIds[session.id] || !isBlank(session))
    },
    [sessions, startedIds, isBlank],
  )
  const locked = activeSession !== null && lockedFor(activeSession.id)

  const focusSession = useCallback((sessionId: string, wsId?: string) => {
    setActiveId(sessionId, wsId ?? null)
  }, [])

  /* ---- 启动自动导航 ---- */

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
      void loadComposer(`session:${prev}`).then((draft) => {
        if (draft.text || draft.images.length || draft.attachments.length || draft.quotes.length || draft.queue.length) return
        return deleteSessionAction(prev)
      }).catch(() => {
        // 草稿不可读或删除失败时保留会话，避免误删未发送内容。
      })
    }
  }, [activeId, sessions, localBlankIds, isBlank])

  /* ---- 显式"新建会话" ---- */

  const startSession = useCallback(
    async (targetWsId?: string) => {
      leaveExtensions()
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
        // 分屏保持:新会话落在主窗格,注意力跟过去。
        setSplit((prev) => (prev ? { ...prev, focus: 'primary' } : prev))
        void refreshList()
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      }
    },
    [workspaces, activeSession, recentWorkspace, leaveExtensions],
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
    [sessions, cleanupSessionPane],
  )

  // 删除确认在侧栏行内完成(删除图标 → 红色“确认删除”),这里只执行删除。
  const deleteSession = useCallback((session: SessionSummary) => {
    void deleteSessionAction(session.id)
      .then(() => {
        // 会话没了,它的面板桶也就永远不会再被读到:清掉,
        // 免得长期使用后 localStorage 里堆满孤儿桶、把在用的 scope
        // 挤出上限。同时把该会话的终端进程一并回收。
        cleanupSessionPane(session.id)
        notify('ok', t('sessionDeleted'))
      })
      .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
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

  /* ---- 分屏:拖动侧栏会话到会话窗口 ---- */

  const {
    split,
    setSplit,
    splitRatio,
    dragHint,
    splitViewRef,
    splitActive,
    applySplitView,
    onPaneDragOver,
    onPaneDragLeave,
    onPaneDrop,
    beginSplitDrag,
    moveSplitDrag,
    endSplitDrag,
    keySplitDrag,
    focusPane,
    navigateSecondary,
  } = useSplitView({ activeId, sessions, sessionsLoaded, workspaces, isMobile, extensionTab })

  const openSession = useCallback(
    (id: string, wsId?: string) => {
      setVersionOpen(false)
      setExtensionTab(null)
      // 手机上选中会话后自动收起抽屉 —— 否则用户点完还得再点一次遮罩
      // 才能看到刚打开的对话。
      setDrawerOpen(false)
      applySplitView(openSessionInView({ primaryId: activeId, split }, id), wsId ?? null)
    },
    [activeId, split, applySplitView],
  )

  /* ---- 会话窗格渲染 ---- */

  /** 主窗格的工作区切换(发消息前可切;已发消息后锁定)。 */
  const selectWorkspaceForPane = (ws: WorkspaceRecord) => {
    if (locked) {
      notify('err', t('lockedWorkspace'))
      return
    }
    connectWorkspace(ws)
  }

  const openWorkspacePicker = () => {
    if (workspaces.length === 0) openDirectoryFlow()
    else setPickerOpen(true)
  }

  const toggleSidePane = () => {
    // 从折叠态点开时,如果还没有任何标签,顺手开一个审查面板 ——
    // 否则用户点了按钮只看到引导页,还要再点一次才有内容。
    if (pane.collapsed && pane.state.tabs.length === 0) {
      pane.openPanel('review')
      return
    }
    pane.toggle()
  }

  /** 单窗格(未分屏)的会话页。 */
  const renderSingleSession = () => (
    <SessionsPage
      key={activeId ?? 'draft'}
      activeId={activeId}
      locked={locked}
      hasStarted={activeId ? !!startedIds[activeId] : false}
      onSelectWorkspace={selectWorkspaceForPane}
      onOpenPicker={openWorkspacePicker}
      onAddWorkspace={openDirectoryFlow}
      sidePaneOpen={!pane.collapsed && pane.state.tabs.length > 0}
      onToggleSidePane={toggleSidePane}
    />
  )

  /** 分屏窗格:副窗格不渲染工作区入口,会话切换约束在本窗格内。 */
  const renderSplitPane = (slot: PaneSlot) => {
    const id = slot === 'primary' ? activeId : split?.secondaryId ?? null
    if (id === null) return null
    const secondarySide: SplitSide = split?.side ?? 'right'
    const side: SplitSide =
      slot === 'secondary' ? secondarySide : secondarySide === 'left' ? 'right' : 'left'
    return (
      <div
        key={slot}
        className={`split-pane ${side}`}
        data-focus={split?.focus === slot ? 'true' : 'false'}
        onPointerDownCapture={() => focusPane(slot)}
      >
        <SessionsPage
          key={id}
          activeId={id}
          locked={lockedFor(id)}
          hasStarted={!!startedIds[id]}
          onSelectWorkspace={selectWorkspaceForPane}
          onOpenPicker={openWorkspacePicker}
          onAddWorkspace={openDirectoryFlow}
          sidePaneOpen={false}
          secondary={slot === 'secondary'}
          navigate={slot === 'secondary' ? navigateSecondary : undefined}
        />
      </div>
    )
  }

  // 扫码落地页:URL 带票据时整页交给远程门,先兑换会话再进控制台。
  if (gateActive) {
    return <RemoteGate />
  }

  return (
    <div className="app-frame">
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
            {/* 品牌区保留窗口拖动能力；折叠按钮由 drag.js 排除。 */}
            <div className="sidebar-logo" data-tauri-drag-region="deep">
              <DeniaWordmark />
              <UpdateBadge />
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
            <nav className="sidebar-primary" aria-label={t('newSession')}>
            <button
              type="button"
              className="sidebar-new-btn"
              onClick={() => void startSession()}
            >
              <IconNewSession size={16} />
              <span>{t('newSession')}</span>
            </button>
            <button type="button" className={`sidebar-new-btn sidebar-extensions-btn${extensionTab ? ' active' : ''}`}
              aria-current={extensionTab ? 'page' : undefined} onClick={() => openExtensions()}>
              <IconExtensions size={16} /><span>{t('extensionsTitle')}</span>
            </button>
            </nav>
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
                    <IconSessionSearch size={14} />
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
                    <IconFolderPlus size={14} />
                  </button>
                </div>
              </div>
              <SidebarWorkspaces
                sessions={sessions}
                workspaces={workspaces}
                activeId={extensionTab || versionOpen ? null : activeId}
                secondaryId={extensionTab || versionOpen ? null : split?.secondaryId ?? null}
                runningIds={runningIds}
                sessionsLoaded={sessionsLoaded}
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
            {/* 收起态顶带:与展开态的品牌行、主列的窗口条同高同底线,
                三段连成一条完整的顶带;它同样可拖窗口。 */}
            <div className="sidebar-rail-top" data-tauri-drag-region="deep">
              <button
                type="button"
                className="rail-btn"
                title={t('sidebarExpand')}
                aria-label={t('sidebarExpand')}
                onClick={() => setSidebarCollapsed(false)}
              >
                <IconPanelOpen size={18} />
              </button>
            </div>
            <button
              type="button"
              className="rail-btn"
              title={t('newSession')}
              aria-label={t('newSession')}
              onClick={() => void startSession()}
            >
              <IconNewSession size={16} />
            </button>
            <button type="button" className={`rail-btn${extensionTab ? ' accent' : ''}`}
              aria-label={t('extensionsTitle')} title={t('extensionsTitle')}
              aria-current={extensionTab ? 'page' : undefined} onClick={() => { openExtensions(); setDrawerOpen(false) }}>
              <IconExtensions size={18} />
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
              <IconSessionSearch size={16} />
            </button>
            <UpdateBadge collapsed />
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
        {/* 桌面壳的窗口条:只占**主列**宽度,不横穿侧栏。这样侧栏能从窗口
            最顶端一直贯到底、品牌 logo 留在原地,而窗口按钮与主列左边界对齐。
            浏览器形态下组件自己返回 null。 */}
        <DesktopTitleBar />
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
        <div className="main-row" data-split={splitActive || undefined}>
          <div
            className="page-pane"
            onDragOver={onPaneDragOver}
            onDragLeave={onPaneDragLeave}
            onDrop={onPaneDrop}
          >
            {versionOpen ? (
              <Suspense fallback={<div className="extensions-loading" role="status">{t('loading')}</div>}>
                <LazyVersionPage onBack={closeVersion} />
              </Suspense>
            ) : extensionTab ? (
              <Suspense fallback={<div className="extensions-loading" role="status">{t('loading')}</div>}>
                <LazyExtensionsPage tab={extensionTab} cwd={paneWorkspacePath ?? undefined} onTabChange={openExtensions} notify={notify} />
              </Suspense>
            ) : splitActive && split ? (
              <div
                className="split-view"
                ref={splitViewRef}
                style={{ '--split-left': `${splitRatio * 100}%` } as CSSProperties}
              >
                {renderSplitPane(split.side === 'left' ? 'secondary' : 'primary')}
                <div className="split-divider">
                  <div
                    className="split-divider-grip"
                    role="separator"
                    aria-orientation="vertical"
                    aria-label={t('splitResize')}
                    aria-valuenow={Math.round(splitRatio * 100)}
                    aria-valuemin={Math.round(SPLIT_MIN_RATIO * 100)}
                    aria-valuemax={Math.round(SPLIT_MAX_RATIO * 100)}
                    tabIndex={0}
                    onPointerDown={beginSplitDrag}
                    onPointerMove={moveSplitDrag}
                    onPointerUp={endSplitDrag}
                    onPointerCancel={endSplitDrag}
                    onKeyDown={keySplitDrag}
                  />
                  <button
                    type="button"
                    className="split-divider-exit"
                    title={t('splitExit')}
                    aria-label={t('splitExit')}
                    data-testid="split-exit"
                    onClick={() => setSplit(null)}
                  >
                    <IconClose size={12} />
                  </button>
                </div>
                {renderSplitPane(split.side === 'left' ? 'primary' : 'secondary')}
              </div>
            ) : (
              renderSingleSession()
            )}
            {/* 拖动侧栏会话经过会话区:半透明遮罩标出松开后的落点。 */}
            {dragHint && <div className={`split-drop-hint ${dragHint}`} aria-hidden="true" />}
          </div>
          {/* 侧边面板在手机上不渲染:390px 宽放不下「对话 + 面板」两列,
              面板会把自己挤成一条缝,还会把主列的输入区顶出屏幕。
              手机用户要终端/文件,用抽屉里的入口或直接开新会话更实际。 */}
          {!isMobile && !extensionTab && !versionOpen && (
          <SidePane
            sessionId={activeId}
            state={pane.state}
            collapsed={pane.collapsed || splitActive}
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
            onOpenFile={(path) => void pane.openFile(path)}
            onActivateFile={(path) => pane.activateFile(path)}
            onCloseFile={(path) => pane.closeFile(path)}
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
            onOpenVersion={openVersion}
            onClose={() => {
              setSettingsOpen(false)
              applyConsoleSettings()
            }}
          />
        </Suspense>
      )}
      {updateDialogOpen && (
        <Suspense fallback={null}>
          <LazyUpdateDialog onOpenVersion={openVersion} />
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
    </div>
  )
}

