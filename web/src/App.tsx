import { lazy, Suspense, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { setLocale, t } from './i18n'
import SessionsPage from './pages/SessionsPage'
import * as api from './api'
import {
  addSessionLocal,
  bumpCatalogTick,
  deleteSessionAction,
  deleteWorkspaceAction,
  findWorkspaceBlank,
  getActiveWorkspace,
  notify,
  refreshList,
  setActiveId,
  setConnLost,
  setPendingWsId,
  setRunningStatus,
  useActiveId,
  useConnLost,
  useRunningIds,
  useSessions,
  useSessionsLoaded,
  useStartedIds,
  useToast,
  useWorkspaces,
} from './appStore'
import {
  BrandMark,
  IconClose,
  IconCollapseAll,
  IconExpandAll,
  IconFolder,
  IconGear,
  IconPlus,
  IconSearch,
  IconTrash,
} from './components/icons'
import { DirPicker } from './components/DirPicker'
import { ConfirmDialog } from './components/ConfirmDialog'

// 设置/浏览器面板按需加载,减少首屏主包体积。
const LazySettingsModal = lazy(() =>
  import('./components/SettingsModal').then((module) => ({ default: module.SettingsModal })),
)
const LazyBrowserPanel = lazy(() => import('./components/BrowserPanel'))
import { subscribeBrowserEvents } from './browserApi'
import { sessionDisplayTitle } from './sessionDisplay'
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
  const sessionsLoaded = useSessionsLoaded()
  const connLost = useConnLost()
  const toast = useToast()

  // running 集合:服务端 SSE 推送驱动(侧栏圆点与状态栏同源)。
  const runningIds = useRunningIds()

  const [settingsOpen, setSettingsOpen] = useState(false)
  const [browserOpen, setBrowserOpen] = useState(false)
  // ZCode 式自动展开:AI 调 browser 产生画面帧/状态变化时右侧视图自动出现。
  // 用户手动收起只挡当轮:新一轮 AI 轮次开始后重新允许自动展开。
  const browserAutoDismissedRef = useRef(false)
  const browserOpenRef = useRef(false)
  useEffect(() => {
    browserOpenRef.current = browserOpen
  }, [browserOpen])
  useEffect(() => {
    const close = subscribeBrowserEvents((event) => {
      if ((event.type === 'frame' || event.type === 'tabs-changed') && !browserAutoDismissedRef.current && !browserOpenRef.current) {
        setBrowserOpen(true)
      }
    })
    return close
  }, [])
  const prevRunningRef = useRef<Record<string, boolean>>({})
  useEffect(() => {
    const freshRun = Object.keys(runningIds).some((id) => !prevRunningRef.current[id])
    prevRunningRef.current = runningIds
    if (freshRun) browserAutoDismissedRef.current = false
  }, [runningIds])
  const [sidebarSearchOpen, setSidebarSearchOpen] = useState(false)
  const [sidebarExpandAllTick, setSidebarExpandAllTick] = useState(0)
  const [sidebarAllExpanded, setSidebarAllExpanded] = useState(false)

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
    const source = new EventSource('/api/events')
    source.onopen = () => {
      setConnLost(false)
      // 仅首次连接与断线重连后重拉列表,避免 onopen 抖动造成无限刷新。
      if (!sseOpenedRef.current) {
        sseOpenedRef.current = true
        void refreshList()
      } else if (sseWasLostRef.current) {
        sseWasLostRef.current = false
        void refreshList()
      }
    }
    source.onerror = () => {
      sseWasLostRef.current = true
      setConnLost(true)
    }
    source.onmessage = (event) => {
      try {
        const parsed = JSON.parse(event.data) as { type?: string; id?: string; running?: boolean }
        if (parsed.type === 'sessions-updated') void refreshList()
        else if (parsed.type === 'settings-updated') applyConsoleSettings()
        else if (parsed.type === 'running-changed' && parsed.id) {
          setRunningStatus(parsed.id, parsed.running === true)
        }
      } catch {
        /* ignore malformed frame */
      }
    }
    return () => source.close()
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

  /* ---- 空白会话不保留:离开即删 + 非活跃清扫 ---- */

  const prevActiveIdRef = useRef<string | null>(null)
  useEffect(() => {
    const prev = prevActiveIdRef.current
    prevActiveIdRef.current = activeId
    if (prev === null || prev === activeId) return
    // 焦点从空白会话切走:它没有任何内容,直接清理,不留账本残影。
    const left = sessions.find((s) => s.id === prev)
    if (left && isBlank(left)) {
      deleteSessionAction(prev).catch(() => {
        // 删除失败(如网络抖动):残留空白无害,非活跃清扫会兜底重试。
      })
    }
  }, [activeId, sessions, isBlank])

  // 兜底清扫:启动/列表刷新后,非活跃的空白会话(历史遗留、浏览器直接
  // 关闭的残留)统一静默清掉;timer 推迟一拍,避免抢在启动自动落点的
  // 空白复用之前动手。
  useEffect(() => {
    if (!sessionsLoaded) return
    const timer = window.setTimeout(() => {
      for (const s of sessions) {
        if (s.id !== activeId && isBlank(s)) {
          deleteSessionAction(s.id).catch(() => {
            // 同上:删除失败静默留待下次机会,不打断用户操作。
          })
        }
      }
    }, 0)
    return () => window.clearTimeout(timer)
  }, [sessions, sessionsLoaded, activeId, isBlank])

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
          cwd: session.cwd,
          sandbox: session.sandbox,
          cwd_alive: true,
        }
        addSessionLocal(summary, target.id)
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
          void deleteWorkspaceAction(ws.id)
            .then(() => notify('ok', t('workspaceDeleted')))
            .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
        },
      })
    },
    [],
  )

  const deleteSession = useCallback((session: SessionSummary) => {
    setConfirmReq({
      title: t('confirmDeleteSession'),
      desc: t('confirmDeleteSessionDesc', { title: sessionDisplayTitle(session) }),
      danger: true,
      onConfirm: () => {
        void deleteSessionAction(session.id)
          .then(() => notify('ok', t('sessionDeleted')))
          .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
      },
    })
  }, [])

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
            }
          })()
            .then(() => notify('ok', t('sessionDeleted')))
            .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
        },
      })
    },
    [runningIds],
  )

  const openSession = useCallback((id: string, wsId?: string) => {
    setActiveId(id, wsId ?? null)
  }, [])

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="sidebar-logo">
          <BrandMark size={22} />
          <span className="brand-text">Denia</span>
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
      </aside>
      <main className="main">
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
            />
          </div>
          {browserOpen && (
            <aside className="browser-sidebar" data-open="true">
              <div className="brw-head">
                <span>{t('navBrowser')}</span>
                <button
                  type="button"
                  className="icon-btn"
                  onClick={() => {
                    browserAutoDismissedRef.current = true
                    setBrowserOpen(false)
                  }}
                >
                  <IconClose size={16} />
                </button>
              </div>
              <div className="brw-body">
                <Suspense fallback={<div className="empty-hint">{t('loading')}</div>}>
                  <LazyBrowserPanel />
                </Suspense>
              </div>
            </aside>
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
 */
function nestByParent(members: SessionSummary[]): { session: SessionSummary; depth: number }[] {
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
      walk(byParent.get(session.id) ?? [], depth + 1)
    }
  }
  walk(roots, 0)
  return out
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
  const [query, setQuery] = useState('')
  const searchInputRef = useRef<HTMLInputElement>(null)
  const lastExpandTickRef = useRef(0)

  // 10k 会话时避免每次 get/sessionIds 都做 O(n) find;一次建 Map,O(1) 取行。
  const sessionsById = useMemo(
    () => new Map(sessions.map((session) => [session.id, session])),
    [sessions],
  )

  const membersOf = useCallback(
    (ws: WorkspaceRecord) =>
      ws.sessionIds
        .map((id) => sessionsById.get(id))
        .filter((s): s is SessionSummary => !!s && s.cwd === ws.path),
    [sessionsById],
  )

  const ungrouped = sessions.filter((s) => !workspaces.some((w) => w.path === s.cwd))

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
  }, [expandAllTick, groupKeys, isGroupOpen, membersOf, workspaces, activeId])

  useEffect(() => {
    if (!searchOpen) {
      setQuery('')
      return
    }
    const id = requestAnimationFrame(() => searchInputRef.current?.focus())
    return () => cancelAnimationFrame(id)
  }, [searchOpen])

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
          const nested = nestByParent(members)
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
                  {nestByParent(filteredUngrouped).map(({ session, depth }) => (
                    <SessionRow
                      key={session.id}
                      session={session}
                      depth={depth}
                      active={session.id === activeId}
                      running={!!runningIds[session.id]}
                      onOpen={() => onOpenSession(session.id)}
                      onDelete={() => onDeleteSession(session)}
                    />
                  ))}
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
  onOpen,
  onDelete,
}: {
  session: SessionSummary
  /** 血缘嵌套深度:0 = 顶层,>0 = 分支子会话(缩进显示)。 */
  depth?: number
  active: boolean
  running: boolean
  onOpen: () => void
  onDelete: () => void
}) {
  // 标题前补"(父)"提示,让用户一眼看出这是分支出来的子会话(已嵌套显示,
  // 但同工作区里有多个分支时文字也帮回忆),只对子会话生效,顶会不画。
  const isBranch = depth > 0
  return (
    <div
      className={`session-row-wrap${active ? ' active' : ''}${isBranch ? ' nested' : ''}`}
      style={isBranch ? { paddingLeft: 10 + depth * 14 } : undefined}
    >
      <button type="button" className={`session-row${active ? ' active' : ''}`} onClick={onOpen}>
        <span className="lead">
          {session.cwd_alive === false ? (
            <span className="dot err" title={t('deadCwd')} />
          ) : running ? (
            <span className="dot run" />
          ) : null}
        </span>
        <span className="excerpt">{sessionDisplayTitle(session)}</span>
        {isBranch && (
          <span className="branch-tag" title={t('branchTagHint')} aria-label={t('branchTagHint')}>
            {t('branchTag')}
          </span>
        )}
      </button>
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
