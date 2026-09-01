import { useCallback, useEffect, useRef, useState } from 'react'
import { setLocale, t } from './i18n'
import ModelsPage from './pages/ModelsPage'
import SessionsPage from './pages/SessionsPage'
import SettingsPage from './pages/SettingsPage'
import * as api from './api'
import { useSessionStreams } from './hooks/useSessionStreams'
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
import { sessionDisplayTitle } from './sessionDisplay'
import type { SessionSummary, WorkspaceRecord } from './types'

export interface Toast {
  kind: 'ok' | 'err'
  message: string
}

export type Notify = (kind: Toast['kind'], message: string) => void

type Page = 'sessions' | 'models' | 'settings'

export default function App() {
  const [page, setPage] = useState<Page>('sessions')
  const [toast, setToast] = useState<Toast | null>(null)
  const [connLost, setConnLost] = useState(false)

  const [sessions, setSessions] = useState<SessionSummary[]>([])
  const [workspaces, setWorkspaces] = useState<WorkspaceRecord[]>([])
  const [activeId, setActiveId] = useState<string | null>(null)
  // 首条消息前可切换的目标工作区(dsh pendingWorkspace)。
  const [pendingWsId, setPendingWsId] = useState<string | null>(null)
  const [reloadKey, setReloadKey] = useState(0)
  // 会话流总线:running 状态与事件流生命周期统一在 App 层。
  const { attach, runningIds } = useSessionStreams()
  // 本会话内已发出首条消息的会话(excerpt 异步刷新前的锁定依据)。
  const [startedIds, setStartedIds] = useState<Record<string, true>>({})
  // 设置页改动默认模型后驱动会话页重拉 catalog。
  const [catalogTick, setCatalogTick] = useState(0)
  // A2:URL 会话状态。挂载时读 hash;列表加载完成后判定恢复或放弃,
  // 判定结束前启动导航不运行(避免覆盖恢复结果)。
  const [pendingHashId] = useState<string | null>(() => {
    const match = window.location.hash.match(/^#s=([^#]+)$/)
    return match ? decodeURIComponent(match[1]) : null
  })
  const [sessionsLoaded, setSessionsLoaded] = useState(false)
  const [hashSettled, setHashSettled] = useState(false)
  const hashRestoredRef = useRef(false)
  const firstRenderRef = useRef(true)

  // 目录流:capability 决定 native 还是 browse。
  const [pickerOpen, setPickerOpen] = useState(false)
  const [, setPicking] = useState(false)
  const capabilityRef = useRef<'native' | 'browse'>('native')

  const timer = useRef<number | undefined>(undefined)
  const notify: Notify = useCallback((kind, message) => {
    setToast({ kind, message })
    window.clearTimeout(timer.current)
    timer.current = window.setTimeout(() => setToast(null), 3500)
  }, [])

  const markStarted = useCallback((id: string) => {
    setStartedIds((previous) => (previous[id] ? previous : { ...previous, [id]: true }))
  }, [])

  const applyConsoleSettings = useCallback(() => {
    api
      .getSettings()
      .then((data) => {
        const console = data.namespaces.find((ns) => ns.ns === 'console')
        const theme = (console?.value.theme as string) ?? 'system'
        if (theme === 'system') delete document.documentElement.dataset.theme
        else document.documentElement.dataset.theme = theme
        setLocale((console?.value.locale as string) ?? 'zh')
        setCatalogTick((tick) => tick + 1)
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

  const sseOpenedRef = useRef(false)
  const sseWasLostRef = useRef(false)

  useEffect(() => {
    const source = new EventSource('/api/events')
    source.onopen = () => {
      setConnLost(false)
      // 仅首次连接与断线重连后重拉列表,避免 onopen 抖动造成无限刷新。
      if (!sseOpenedRef.current) {
        sseOpenedRef.current = true
        setReloadKey((key) => key + 1)
      } else if (sseWasLostRef.current) {
        sseWasLostRef.current = false
        setReloadKey((key) => key + 1)
      }
    }
    source.onerror = () => {
      sseWasLostRef.current = true
      setConnLost(true)
    }
    source.onmessage = (event) => {
      try {
        const parsed = JSON.parse(event.data) as { type?: string }
        if (parsed.type === 'sessions-updated') setReloadKey((key) => key + 1)
        else if (parsed.type === 'settings-updated') applyConsoleSettings()
      } catch {
        /* ignore malformed frame */
      }
    }
    return () => source.close()
  }, [applyConsoleSettings])

  useEffect(() => {
    let cancelled = false
    Promise.all([api.listSessions(), api.listWorkspaces()])
      .then(([sess, ws]) => {
        if (cancelled) return
        setSessions(sess.sessions)
        setWorkspaces(ws.workspaces)
        setStartedIds((previous) => {
          const next = { ...previous }
          for (const session of sess.sessions) {
            if (session.excerpt) next[session.id] = true
          }
          return next
        })
        setSessionsLoaded(true)
      })
      .catch((error) => {
        if (cancelled) return
        setSessionsLoaded(true)
        notify('err', error instanceof Error ? error.message : String(error))
      })
    return () => {
      cancelled = true
    }
  }, [reloadKey, notify])

  /* ---- A2:URL 会话状态 ---- */

  // 列表首次加载完成后判定 URL 里的会话:存在则恢复,不存在则放弃(交给 launch
  // 导航)。只运行一次;后续 sessions-updated 重拉列表不得覆盖用户当前导航。
  useEffect(() => {
    if (!sessionsLoaded || hashRestoredRef.current) return
    hashRestoredRef.current = true
    if (pendingHashId !== null && sessions.some((s) => s.id === pendingHashId)) {
      setActiveId(pendingHashId)
    }
    setHashSettled(true)
  }, [sessions, sessionsLoaded, pendingHashId])

  // activeId 变化时同步 hash(replaceState 不触发 hashchange,无回环)。
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

  // 前进/后退恢复;目标会话不存在则不响应。
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

  const sessionHasStarted = useCallback(
    (id: string) => {
      const session = sessions.find((s) => s.id === id)
      return !!startedIds[id] || !!session?.excerpt
    },
    [sessions, startedIds],
  )

  const activeSession = sessions.find((s) => s.id === activeId) ?? null
  const isBlank = (s: SessionSummary) =>
    !s.excerpt && !startedIds[s.id]
  const findWorkspaceBlank = useCallback(
    (cwd: string): SessionSummary | undefined =>
      sessions.find(
        (s) => s.cwd === cwd && s.cwd_alive !== false && isBlank(s),
      ),
    [sessions, startedIds],
  )
  const focusSession = useCallback((sessionId: string, wsId: string) => {
    setActiveId(sessionId)
    setPendingWsId(wsId)
    setPage('sessions')
  }, [])
  // 首条消息后(或已有历史)会话与工作区绑定,不能换。
  const locked =
    activeSession !== null &&
    (!!startedIds[activeSession.id] || !isBlank(activeSession))
  const wsOfSession = (s: SessionSummary) =>
    workspaces.find((w) => w.path === s.cwd) ?? null
  const activeWs = activeSession
    ? wsOfSession(activeSession)
    : (workspaces.find((w) => w.id === pendingWsId) ?? null)

  /* ---- dsh navigation.ts 的抄写 ---- */

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

  // 复用该工作区的 blank 会话;没有则只落目标工作区(首条消息时才创建,
  // 切换工作区与启动导航都不静默建会话实体)。
  const connectWorkspace = useCallback(
    async (ws: WorkspaceRecord) => {
      if (activeSession && activeSession.cwd === ws.path) {
        setPendingWsId(ws.id)
        return
      }
      const blank = findWorkspaceBlank(ws.path)
      if (blank) {
        focusSession(blank.id, ws.id)
        return
      }
      setActiveId(null)
      setPendingWsId(ws.id)
    },
    [sessions, activeSession, findWorkspaceBlank, focusSession],
  )

  // 启动自动导航:有工作区且无当前会话 → 落最近工作区(dsh watchNavigation)。
  // hashSettled 前不运行:URL 恢复与自动导航共用同一会话槽,避免旧闭包覆盖。
  const bootedRef = useRef(false)
  useEffect(() => {
    if (bootedRef.current || activeId) return
    if (!hashSettled) return
    if (workspaces.length === 0) return
    bootedRef.current = true
    const recent = recentWorkspace()
    if (recent) void connectWorkspace(recent)
  }, [workspaces, activeId, hashSettled, recentWorkspace, connectWorkspace])

  // 显式"新建会话":目标工作区已有空白会话则复用,否则创建。
  const startSession = useCallback(
    async (targetWsId?: string) => {
      const target =
        workspaces.find((w) => w.id === targetWsId) ??
        (activeSession ? wsOfSession(activeSession) : null) ??
        recentWorkspace()
      if (!target) {
        setActiveId(null)
        setPendingWsId(null)
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
        setSessions((previous) => [summary, ...previous.filter((s) => s.id !== summary.id)])
        setWorkspaces((previous) =>
          previous.map((ws) =>
            ws.id === target.id
              ? { ...ws, sessionIds: [summary.id, ...ws.sessionIds.filter((id) => id !== summary.id)] }
              : ws,
          ),
        )
        setActiveId(summary.id)
        setPendingWsId(target.id)
        setPage('sessions')
        setReloadKey((key) => key + 1)
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      }
    },
    [workspaces, activeSession, recentWorkspace, wsOfSession, notify, findWorkspaceBlank, focusSession],
  )

  // 目录流入口:capability 分流 native/browse;失败进"无法打开文件夹"。
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
            setWorkspaces((prev) => [workspace, ...prev.filter((w) => w.id !== workspace.id)])
            await connectWorkspace(workspace)
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
  }, [connectWorkspace, notify])

  const adoptFromBrowse = useCallback(
    async (path: string) => {
      const { workspace } = await api.createWorkspace(path)
      setWorkspaces((prev) => [workspace, ...prev.filter((w) => w.id !== workspace.id)])
      await connectWorkspace(workspace)
      notify('ok', t('workspaceAdded'))
    },
    [connectWorkspace, notify],
  )

  const deleteWorkspace = useCallback(
    async (ws: WorkspaceRecord) => {
      if (!window.confirm(t('deleteWorkspaceDesc', { name: ws.title }))) return
      try {
        await api.deleteWorkspace(ws.id)
        if (activeId && sessions.some((s) => s.id === activeId && s.cwd === ws.path)) {
          setActiveId(null)
          window.location.hash = ''
        }
        setReloadKey((key) => key + 1)
        notify('ok', t('workspaceDeleted'))
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      }
    },
    [activeId, notify, sessions],
  )

  const removeSessionFromState = useCallback((sessionId: string) => {
    setSessions((previous) => previous.filter((session) => session.id !== sessionId))
    setWorkspaces((previous) =>
      previous.map((workspace) => ({
        ...workspace,
        sessionIds: workspace.sessionIds.filter((id) => id !== sessionId),
      })),
    )
    if (activeId === sessionId) {
      setActiveId(null)
      window.location.hash = ''
    }
  }, [activeId])

  const deleteSession = useCallback(
    async (session: SessionSummary) => {
      if (!window.confirm(t('deleteSessionDesc', { title: sessionDisplayTitle(session) }))) {
        return
      }
      try {
        await api.deleteSession(session.id)
        removeSessionFromState(session.id)
        notify('ok', t('sessionDeleted'))
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      }
    },
    [notify, removeSessionFromState],
  )

  const deleteUngrouped = useCallback(
    async (items: SessionSummary[]) => {
      if (items.length === 0) return
      if (!window.confirm(t('deleteUngroupedDesc', { n: items.length }))) return
      try {
        for (const session of items) {
          if (runningIds[session.id]) {
            throw new Error(t('sessionRunningDelete'))
          }
          await api.deleteSession(session.id)
          removeSessionFromState(session.id)
        }
        notify('ok', t('sessionDeleted'))
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      }
    },
    [notify, removeSessionFromState, runningIds],
  )

  // 发送时确保会话存在:复用 active 或该工作区的空白会话,否则创建。
  const ensureSession = useCallback(async (): Promise<string | null> => {
    if (activeId) return activeId
    const ws = activeWs
    if (!ws) return null
    const blank = findWorkspaceBlank(ws.path)
    if (blank) {
      setActiveId(blank.id)
      return blank.id
    }
    const { session } = await api.createSession({ workspaceId: ws.id })
    const summary: SessionSummary = {
      id: session.id,
      created_at: session.created_at,
      excerpt: null,
      cwd: session.cwd,
      sandbox: session.sandbox,
      cwd_alive: true,
    }
    setSessions((previous) => [summary, ...previous.filter((s) => s.id !== summary.id)])
    setWorkspaces((previous) =>
      previous.map((item) =>
        item.id === ws.id
          ? { ...item, sessionIds: [summary.id, ...item.sessionIds.filter((id) => id !== summary.id)] }
          : item,
      ),
    )
    setActiveId(summary.id)
    setReloadKey((key) => key + 1)
    return summary.id
  }, [activeId, activeWs, findWorkspaceBlank])

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="sidebar-logo">
          <BrandMark size={22} />
          <span className="brand-text">
            dsh<em>-rs</em>
          </span>
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
            onOpenSession={(id, wsId) => {
              setPage('sessions')
              setActiveId(id)
              setPendingWsId(wsId ?? null)
            }}
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
            onClick={() => setPage('sessions')}
          >
            <IconChat size={16} />
            <span>{t('navSessions')}</span>
          </button>
          <button
            type="button"
            className={`sidebar-nav${page === 'models' ? ' active' : ''}`}
            onClick={() => setPage('models')}
          >
            <IconSliders size={16} />
            <span>{t('navModels')}</span>
          </button>
          <button
            type="button"
            className={`sidebar-nav${page === 'settings' ? ' active' : ''}`}
            onClick={() => setPage('settings')}
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
            activeSession={activeSession}
            activeWs={activeWs}
            workspaces={workspaces}
            running={activeId ? !!runningIds[activeId] : false}
            attach={attach}
            locked={locked}
            hasStarted={activeId ? sessionHasStarted(activeId) : false}
            catalogTick={catalogTick}
            notify={notify}
            onStarted={markStarted}
            onSelectWorkspace={(ws) => {
              // 首条消息前可切换;已发消息则锁死(dsh 交互)。
              if (locked) {
                notify('err', t('lockedWorkspace'))
                return
              }
              void connectWorkspace(ws)
            }}
            onOpenPicker={() => {
              if (workspaces.length === 0) openDirectoryFlow()
              else setPickerOpen(true)
            }}
            onAddWorkspace={openDirectoryFlow}
            onEnsureSession={ensureSession}
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
