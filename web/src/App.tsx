import { useCallback, useEffect, useRef, useState } from 'react'
import { setLocale, t } from './i18n'
import ModelsPage from './pages/ModelsPage'
import SessionsPage from './pages/SessionsPage'
import SettingsPage from './pages/SettingsPage'
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
  setPage,
  setPendingWsId,
  setRunningStatus,
  useActiveId,
  useConnLost,
  usePage,
  useRunningIds,
  useSessions,
  useSessionsLoaded,
  useStartedIds,
  useToast,
  useWorkspaces,
} from './appStore'
import {
  BrandMark,
  IconChat,
  IconChevron,
  IconFolder,
  IconGear,
  IconPlus,
  IconSliders,
  IconTrash,
} from './components/icons'
import { DirPicker } from './components/DirPicker'
import { ConfirmDialog } from './components/ConfirmDialog'
import { sessionDisplayTitle } from './sessionDisplay'
import type { SessionSummary, WorkspaceRecord } from './types'

/**
 * 应用外壳:布局 + 全局事件订阅(SSE 推送)+ URL 导航 + 启动引导。
 * 业务状态与动作全部在 `appStore`,本组件只做编排,不持有会话数据。
 */
export type Notify = (kind: 'ok' | 'err', message: string) => void

type Page = 'sessions' | 'models' | 'settings'

export default function App() {
  const page = usePage()
  const sessions = useSessions()
  const workspaces = useWorkspaces()
  const activeId = useActiveId()
  const startedIds = useStartedIds()
  const sessionsLoaded = useSessionsLoaded()
  const connLost = useConnLost()
  const toast = useToast()

  // running 集合:服务端 SSE 推送驱动(侧栏圆点与状态栏同源)。
  const runningIds = useRunningIds()

  const setPageSafe = useCallback((page: Page) => setPage(page), [])

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
    setPage('sessions')
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
      const blank = findWorkspaceBlank(target.path)
      if (blank) {
        focusSession(blank.id, target.id)
        return
      }
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
        setPage('sessions')
        void refreshList()
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      }
    },
    [workspaces, activeSession, recentWorkspace, focusSession],
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
    setPage('sessions')
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
            <button
              type="button"
              className="icon-btn"
              title={t('addWorkspace')}
              onClick={openDirectoryFlow}
            >
              <IconPlus size={14} />
            </button>
          </div>
          <SidebarWorkspaces
            sessions={sessions}
            workspaces={workspaces}
            activeId={activeId}
            runningIds={runningIds}
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
            className={`sidebar-nav${page === 'sessions' ? ' active' : ''}`}
            onClick={() => setPageSafe('sessions')}
          >
            <IconChat size={16} />
            <span>{t('navSessions')}</span>
          </button>
          <button
            type="button"
            className={`sidebar-nav${page === 'models' ? ' active' : ''}`}
            onClick={() => setPageSafe('models')}
          >
            <IconSliders size={16} />
            <span>{t('navModels')}</span>
          </button>
          <button
            type="button"
            className={`sidebar-nav${page === 'settings' ? ' active' : ''}`}
            onClick={() => setPageSafe('settings')}
          >
            <IconGear size={16} />
            <span>{t('navSettings')}</span>
          </button>
        </nav>
      </aside>
      <main className="main">
        <div className="page-pane" hidden={page !== 'sessions'}>
          <SessionsPage
            key={activeId ?? 'draft'}
            activeId={activeId}
            locked={locked}
            hasStarted={activeId ? !!startedIds[activeId] : false}
            onSelectWorkspace={(ws) => {
              // 首条消息前可切换;已发消息则锁死(dsh 交互)。
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
        <div className="page-pane" hidden={page !== 'models'}>
          <ModelsPage notify={notify} />
        </div>
        <div className="page-pane" hidden={page !== 'settings'}>
          <SettingsPage notify={notify} />
        </div>
      </main>
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

function SidebarWorkspaces({
  sessions,
  workspaces,
  activeId,
  runningIds,
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
  onOpenSession: (id: string, wsId?: string) => void
  onNewSession: (wsId?: string) => void
  onDeleteWorkspace: (ws: WorkspaceRecord) => void
  onDeleteSession: (session: SessionSummary) => void
  onDeleteUngrouped: (sessions: SessionSummary[]) => void
}) {
  const [expanded, setExpanded] = useState<Record<string, boolean>>({})
  const [showAll, setShowAll] = useState<Record<string, boolean>>({})
  const COLLAPSED_LIMIT = 5

  useEffect(() => {
    if (!activeId) return
    const ws = workspaces.find((item) => item.sessionIds.includes(activeId))
    if (!ws) return
    setExpanded((previous) => (previous[ws.id] ? previous : { ...previous, [ws.id]: true }))
  }, [activeId, workspaces])

  const membersOf = (ws: WorkspaceRecord) =>
    ws.sessionIds
      .map((id) => sessions.find((s) => s.id === id))
      .filter((s): s is SessionSummary => !!s && s.cwd === ws.path)

  const ungrouped = sessions.filter(
    (s) => !workspaces.some((w) => w.path === s.cwd),
  )

  return (
    <div className="sidebar-section">
      <div className="session-list">
        {workspaces.length === 0 && ungrouped.length === 0 && (
          <div className="empty-hint">{t('emptySessions')}</div>
        )}
        {workspaces.map((ws) => {
          const members = membersOf(ws)
          const open = expanded[ws.id] ?? members.some((s) => s.id === activeId)
          const visible = showAll[ws.id] ? members : members.slice(0, COLLAPSED_LIMIT)
          return (
            <div className="ws-group" key={ws.id}>
              <div className="ws-group-head-row">
                <button
                  className={`ws-group-head${members.some((s) => s.id === activeId) ? ' active' : ''}`}
                  title={ws.path}
                  onClick={() =>
                    setExpanded((prev) => ({ ...prev, [ws.id]: !open }))
                  }
                >
                  <span className={`chev${open ? ' open' : ''}`}>
                    <IconChevron size={11} />
                  </span>
                  <IconFolder size={13} />
                  <span className="name">{ws.title}</span>
                  <span className="count">{members.length}</span>
                </button>
                <span className="row-actions">
                  <button
                    className="icon-btn"
                    title={t('newSessionIn', { name: ws.title })}
                    onClick={() => onNewSession(ws.id)}
                  >
                    <IconPlus size={12} />
                  </button>
                  <button
                    className="icon-btn"
                    title={t('deleteWorkspace')}
                    onClick={() => onDeleteWorkspace(ws)}
                  >
                    <IconTrash size={12} />
                  </button>
                </span>
              </div>
              {open && (
                <>
                  {visible.map((session) => (
                    <SessionRow
                      key={session.id}
                      session={session}
                      active={session.id === activeId}
                      running={!!runningIds[session.id]}
                      onOpen={() => onOpenSession(session.id, ws.id)}
                      onDelete={() => onDeleteSession(session)}
                    />
                  ))}
                  {members.length > COLLAPSED_LIMIT && (
                    <button
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
                </>
              )}
            </div>
          )
        })}
        {ungrouped.length > 0 && (
          <div className="ws-group">
            <div className="ws-group-head-row">
              <button
                className={`ws-group-head${ungrouped.some((s) => s.id === activeId) ? ' active' : ''}`}
                onClick={() =>
                  setExpanded((prev) => ({ ...prev, ungrouped: !(expanded.ungrouped ?? true) }))
                }
              >
                <span className={`chev${expanded.ungrouped ?? true ? ' open' : ''}`}>
                  <IconChevron size={11} />
                </span>
                <IconFolder size={13} />
                <span className="name">{t('ungrouped')}</span>
                <span className="count">{ungrouped.length}</span>
              </button>
              <span className="ungrouped-actions">
                <button
                  type="button"
                  className="ungrouped-clear"
                  title={t('deleteUngroupedDesc', { n: ungrouped.length })}
                  onClick={() => onDeleteUngrouped(ungrouped)}
                >
                  {t('deleteUngrouped')}
                </button>
              </span>
            </div>
            {(expanded.ungrouped ?? true) &&
              ungrouped.map((session) => (
                <SessionRow
                  key={session.id}
                  session={session}
                  active={session.id === activeId}
                  running={!!runningIds[session.id]}
                  onOpen={() => onOpenSession(session.id)}
                  onDelete={() => onDeleteSession(session)}
                />
              ))}
          </div>
        )}
      </div>
    </div>
  )
}

function SessionRow({
  session,
  active,
  running,
  onOpen,
  onDelete,
}: {
  session: SessionSummary
  active: boolean
  running: boolean
  onOpen: () => void
  onDelete: () => void
}) {
  return (
    <div className={`session-row-wrap${active ? ' active' : ''}`}>
      <button type="button" className={`session-row${active ? ' active' : ''}`} onClick={onOpen}>
        <span className="lead">
          {session.cwd_alive === false ? (
            <span className="dot err" title={t('deadCwd')} />
          ) : running ? (
            <span className="dot run" />
          ) : null}
        </span>
        <span className="excerpt">{sessionDisplayTitle(session)}</span>
      </button>
      <span className="row-actions">
        <button
          type="button"
          className="icon-btn"
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
