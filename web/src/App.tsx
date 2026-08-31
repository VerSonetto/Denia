import { useCallback, useEffect, useRef, useState } from 'react'
import { t } from './i18n'
import ModelsPage from './pages/ModelsPage'
import SessionsPage from './pages/SessionsPage'
import * as api from './api'
import {
  BrandMark,
  IconChat,
  IconPlus,
  IconSliders,
  IconTrash,
} from './components/icons'
import type { SessionSummary } from './types'

export interface Toast {
  kind: 'ok' | 'err'
  message: string
}

export type Notify = (kind: Toast['kind'], message: string) => void

type Page = 'sessions' | 'models'

export default function App() {
  const [page, setPage] = useState<Page>('sessions')
  const [toast, setToast] = useState<Toast | null>(null)
  const [connLost, setConnLost] = useState(false)
  const [sessions, setSessions] = useState<SessionSummary[]>([])
  const [activeId, setActiveId] = useState<string | null>(null)
  const [sandbox, setSandbox] = useState(true)
  const [cwd, setCwd] = useState('')
  const [runningIds, setRunningIds] = useState<Record<string, boolean>>({})
  const [reloadKey, setReloadKey] = useState(0)
  const timer = useRef<number | undefined>(undefined)

  const notify: Notify = useCallback((kind, message) => {
    setToast({ kind, message })
    window.clearTimeout(timer.current)
    timer.current = window.setTimeout(() => setToast(null), 3500)
  }, [])

  useEffect(() => {
    const source = new EventSource('/api/events')
    source.onopen = () => setConnLost(false)
    source.onerror = () => setConnLost(true)
    return () => source.close()
  }, [])

  useEffect(() => {
    api
      .listSessions()
      .then((data) => setSessions(data.sessions))
      .catch(() => {})
  }, [reloadKey])

  useEffect(() => {
    const unsubscribe = api.subscribeEvents((type) => {
      if (type === 'sessions-updated') setReloadKey((key) => key + 1)
    })
    return unsubscribe
  }, [])

  const create = useCallback(async () => {
    try {
      const { session } = await api.createSession({
        sandbox,
        cwd: sandbox ? undefined : cwd.trim() || undefined,
      })
      setReloadKey((key) => key + 1)
      setActiveId(session.id)
      setPage('sessions')
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }, [sandbox, cwd, notify])

  const remove = async (id: string) => {
    try {
      await api.deleteSession(id)
      if (activeId === id) setActiveId(null)
      notify('ok', t('sessionDeleted'))
      setReloadKey((key) => key + 1)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }

  const onRunningChange = useCallback((id: string, running: boolean) => {
    setRunningIds((previous) => ({ ...previous, [id]: running }))
  }, [])

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
        </nav>
        {page === 'sessions' && (
          <div className="sidebar-section">
            <button className="btn-new-session" onClick={() => void create()}>
              <IconPlus size={14} />
              {t('newSession')}
            </button>
            <div className="workspace-box">
              <label className="check">
                <input
                  type="checkbox"
                  checked={sandbox}
                  onChange={(event) => setSandbox(event.target.checked)}
                />
                {t('sandboxLabel')}
              </label>
              {!sandbox && (
                <input
                  type="text"
                  placeholder={t('cwdPlaceholder')}
                  value={cwd}
                  onChange={(event) => setCwd(event.target.value)}
                />
              )}
            </div>
            <div className="session-list">
              {sessions.length === 0 && (
                <div className="empty-hint">{t('emptySessions')}</div>
              )}
              {sessions.map((session) => (
                <button
                  key={session.id}
                  className={`session-row${activeId === session.id ? ' active' : ''}`}
                  onClick={() => setActiveId(session.id)}
                >
                  <span className="lead">
                    {runningIds[session.id] ? (
                      <span className="dot run" />
                    ) : (
                      <IconChat size={13} />
                    )}
                  </span>
                  <span className="body">
                    <span className="excerpt">
                      {session.excerpt || session.id.slice(0, 8)}
                    </span>
                    <span className="time">
                      {new Date(session.created_at).toLocaleString()}
                    </span>
                  </span>
                  <span
                    className="del"
                    role="button"
                    tabIndex={0}
                    onClick={(event) => {
                      event.stopPropagation()
                      void remove(session.id)
                    }}
                  >
                    <IconTrash size={13} />
                  </span>
                </button>
              ))}
            </div>
          </div>
        )}
      </aside>
      <main className="main">
        {page === 'sessions' ? (
          <SessionsPage
            activeId={activeId}
            notify={notify}
            onRunningChange={onRunningChange}
            onCreate={() => void create()}
          />
        ) : (
          <ModelsPage notify={notify} />
        )}
      </main>
      {connLost && <div className="conn-lost">{t('connectionLost')}</div>}
      {toast && <div className={`toast ${toast.kind}`}>{toast.message}</div>}
    </div>
  )
}
