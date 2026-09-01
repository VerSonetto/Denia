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

  useEffect(() => {
    api
      .pickerCapability()
      .then((cap) => {
        capabilityRef.current = cap.kind === 'browse' ? 'browse' : 'native'
      })
      .catch(() => {})
  }, [])

  useEffect(() => {
    const source = new EventSource('/api/events')
    source.onopen = () => {
      setConnLost(false)
      // 连接建立/恢复即重拉列表:自愈"后端未就绪时首拉失败"的启动竞态
      // 与后端重启后的静默空态。
      setReloadKey((key) => key + 1)
    }
    source.onerror = () => setConnLost(true)
    return () => source.close()
  }, [])

  useEffect(() => {
    Promise.all([api.listSessions(), api.listWorkspaces()])
      .then(([sess, ws]) => {
        setSessions(sess.sessions)
        setWorkspaces(ws.workspaces)
        setSessionsLoaded(true)
      })
      .catch(() => {})
  }, [reloadKey])

  /* ---- A2:URL 会话状态 ---- */

  // 列表加载完成后判定 URL 里的会话:存在则恢复,不存在则放弃(交给 launch
  // 导航);判定结束前自动导航不运行,避免旧闭包覆盖恢复结果。
  useEffect(() => {
    if (!sessionsLoaded) return
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

  useEffect(() => {
    const applyConsole = () => {
      api
        .getSettings()
        .then((data) => {
          const console = data.namespaces.find((ns) => ns.ns === 'console')
          const theme = (console?.value.theme as string) ?? 'system'
          if (theme === 'system') delete document.documentElement.dataset.theme
          else document.documentElement.dataset.theme = theme
          setLocale((console?.value.locale as string) ?? 'zh')
          // 设置页可能改过默认模型:驱动会话页重拉 catalog。
          setCatalogTick((tick) => tick + 1)
        })
        .catch(() => {})
    }
    applyConsole()
    const unsubscribe = api.subscribeEvents((type) => {
      if (type === 'sessions-updated') setReloadKey((key) => key + 1)
      else if (type === 'settings-updated') applyConsole()
    })
    return unsubscribe
  }, [])

  const activeSession = sessions.find((s) => s.id === activeId) ?? null
  const isBlank = (s: SessionSummary) => !s.excerpt
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
      const blank = sessions.find(
        (s) => isBlank(s) && s.cwd === ws.path && s.cwd_alive !== false,
      )
      if (blank) {
        setActiveId(blank.id)
        setPendingWsId(ws.id)
        return
      }
      setActiveId(null)
      setPendingWsId(ws.id)
    },
    [sessions, activeSession],
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

  // 显式"新建会话":总是创建全新会话,不复用空白会话;
  // 复用只属于自动落点/切换工作区(connectWorkspace)。
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
      try {
        const { session } = await api.createSession({ workspaceId: target.id })
        setActiveId(session.id)
        setPendingWsId(target.id)
        setReloadKey((key) => key + 1)
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      }
    },
    [workspaces, activeSession, recentWorkspace, wsOfSession, notify],
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
        setReloadKey((key) => key + 1)
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      }
    },
    [notify],
  )

  // 发送时确保会话存在:复用 active,否则在目标工作区建(dsh blank 复用)。
  const ensureSession = useCallback(async (): Promise<string | null> => {
    if (activeId) return activeId
    const ws = activeWs
    if (!ws) return null
    const { session } = await api.createSession({ workspaceId: ws.id })
    setActiveId(session.id)
    setReloadKey((key) => key + 1)
    return session.id
  }, [activeId, activeWs])

  return (
    <div className="shell">
      <aside className="sidebar">
        <div className="brand-row">
          <BrandMark />
          <div>
            <div className="wordmark">dsh-rs</div>
            <div className="caption">harness console</div>
          </div>
        </div>
        <nav className="nav-stack">
          <button
            className={`nav-row${page === 'sessions' ? ' active' : ''}`}
            onClick={() => setPage('sessions')}
          >
            <IconChat size={15} />
            {t('navSessions')}
          </button>
          <button
            className={`nav-row${page === 'models' ? ' active' : ''}`}
            onClick={() => setPage('models')}
          >
            <IconSliders size={15} />
            {t('navModels')}
          </button>
          <button
            className={`nav-row${page === 'settings' ? ' active' : ''}`}
            onClick={() => setPage('settings')}
          >
            <IconGear size={15} />
            {t('navSettings')}
          </button>
        </nav>
        {page === 'sessions' && (
          <SidebarWorkspaces
            sessions={sessions}
            workspaces={workspaces}
            activeId={activeId}
            runningIds={runningIds}
            onOpenSession={(id, wsId) => {
              setActiveId(id)
              setPendingWsId(wsId ?? null)
            }}
            onNewSession={(wsId) => void startSession(wsId)}
            onAddWorkspace={openDirectoryFlow}
            onDeleteWorkspace={(ws) => void deleteWorkspace(ws)}
          />
        )}
      </aside>
      <main className="main">
        <div className="page-pane" hidden={page !== 'sessions'}>
          <SessionsPage
            activeId={activeId}
            activeSession={activeSession}
            activeWs={activeWs}
            workspaces={workspaces}
            running={activeId ? !!runningIds[activeId] : false}
            attach={attach}
            locked={locked}
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
  onAddWorkspace,
  onDeleteWorkspace,
}: {
  sessions: SessionSummary[]
  workspaces: WorkspaceRecord[]
  activeId: string | null
  runningIds: Record<string, boolean>
  onOpenSession: (id: string, wsId?: string) => void
  onNewSession: (wsId?: string) => void
  onAddWorkspace: () => void
  onDeleteWorkspace: (ws: WorkspaceRecord) => void
}) {
  const [expanded, setExpanded] = useState<Record<string, boolean>>({})
  const [showAll, setShowAll] = useState<Record<string, boolean>>({})
  const COLLAPSED_LIMIT = 5

  const membersOf = (ws: WorkspaceRecord) =>
    ws.sessionIds
      .map((id) => sessions.find((s) => s.id === id))
      .filter((s): s is SessionSummary => !!s && s.cwd === ws.path)

  const ungrouped = sessions.filter(
    (s) => !workspaces.some((w) => w.path === s.cwd),
  )

  return (
    <div className="sidebar-section">
      <div className="section-head-row">
        <span className="section-title">{t('workspacesTitle')}</span>
        <button className="icon-btn" title={t('addWorkspace')} onClick={onAddWorkspace}>
          <IconPlus size={13} />
        </button>
      </div>
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
            {(expanded.ungrouped ?? true) &&
              ungrouped.map((session) => (
                <SessionRow
                  key={session.id}
                  session={session}
                  active={session.id === activeId}
                  running={!!runningIds[session.id]}
                  onOpen={() => onOpenSession(session.id)}
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
}: {
  session: SessionSummary
  active: boolean
  running: boolean
  onOpen: () => void
}) {
  return (
    <button className={`session-row${active ? ' active' : ''}`} onClick={onOpen}>
      <span className="lead">
        {session.cwd_alive === false ? (
          <span className="dot err" title={t('deadCwd')} />
        ) : running ? (
          <span className="dot run" />
        ) : (
          <IconChat size={13} />
        )}
      </span>
      <span className="body">
        <span className="excerpt">
          {session.excerpt ?? t('blankSession')}
        </span>
        {session.excerpt && (
          <span className="time">{new Date(session.created_at).toLocaleString()}</span>
        )}
      </span>
    </button>
  )
}
