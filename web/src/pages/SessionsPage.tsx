import { useEffect, useState } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import type { Notify } from '../App'
import { SessionView } from '../components/SessionView'
import {
  IconChevron,
  IconFolder,
  IconPlus,
  IconSend,
  IconStop,
} from '../components/icons'
import type {
  ModelCatalog,
  ModelSelection,
  SessionSummary,
  WorkspaceRecord,
} from '../types'

export default function SessionsPage({
  activeId,
  activeSession,
  activeWs,
  workspaces,
  runningIds,
  notify,
  onRunningChange,
  onSelectWorkspace,
  onOpenPicker,
  onAddWorkspace,
  onEnsureSession,
}: {
  activeId: string | null
  activeSession: SessionSummary | null
  activeWs: WorkspaceRecord | null
  workspaces: WorkspaceRecord[]
  runningIds: Record<string, boolean>
  notify: Notify
  onRunningChange: (id: string, running: boolean) => void
  onSelectWorkspace: (ws: WorkspaceRecord) => void
  onOpenPicker: () => void
  onAddWorkspace: () => void
  onEnsureSession: () => Promise<string | null>
}) {
  const [catalog, setCatalog] = useState<ModelCatalog | null>(null)
  const [prompt, setPrompt] = useState('')
  const [selection, setSelection] = useState<ModelSelection | null>(null)
  const [wsMenuOpen, setWsMenuOpen] = useState(false)
  const [sending, setSending] = useState(false)

  useEffect(() => {
    api
      .getCatalog()
      .then(setCatalog)
      .catch((error) =>
        notify('err', error instanceof Error ? error.message : String(error)),
      )
  }, [notify])

  useEffect(() => {
    if (catalog && !selection) setSelection(catalog.default)
  }, [catalog, selection])

  const running = activeId ? !!runningIds[activeId] : false
  // 无会话且无目标工作区 ⇒ 输入栏 inert,点击弹工作区选择(dsh 硬前置)。
  const inert = !activeId && !activeWs

  const send = async () => {
    const message = prompt.trim()
    if (!message || sending) return
    if (inert) {
      onOpenPicker()
      return
    }
    setSending(true)
    try {
      const id = await onEnsureSession()
      if (!id) {
        onOpenPicker()
        return
      }
      await api.postPrompt(id, {
        prompt: message,
        provider: selection?.provider,
        model: selection?.model,
        reasoningEffort: selection?.reasoningEffort,
      })
      setPrompt('')
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      setSending(false)
    }
  }

  const stop = async () => {
    if (!activeId) return
    try {
      await api.cancelSession(activeId)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }

  const group = catalog?.groups.find((g) => g.id === selection?.provider)
  const model = group?.models.find((m) => m.id === selection?.model)
  const efforts = model?.reasoning?.efforts ?? []
  const selKey = selection ? `${selection.provider}::${selection.model}` : ''

  return (
    <>
      {activeId ? (
        <SessionView id={activeId} notify={notify} onRunningChange={onRunningChange} />
      ) : (
        <div className="hero">
          <h1>{t('heroTitle')}</h1>
          <p>{activeWs ? t('heroSub') : t('placeholderWorkspace')}</p>
        </div>
      )}
      <div className="composer-zone">
        <div className="composer">
          <div className="ws-strip">
            <button
              className="ws-chip"
              onClick={() => {
                if (workspaces.length === 0) onOpenPicker()
                else setWsMenuOpen((open) => !open)
              }}
            >
              <IconFolder size={13} />
              <span className={activeWs ? '' : 'dim'}>
                {activeWs?.title ?? t('heroChooseWorkspace')}
              </span>
              <IconChevron size={10} />
            </button>
            {activeSession && activeSession.cwd_alive === false && (
              <span className="badge err" title={t('deadCwdHint')}>
                {t('deadCwd')}
              </span>
            )}
            {wsMenuOpen && (
              <>
                <div className="menu-backdrop" onClick={() => setWsMenuOpen(false)} />
                <div className="ws-menu">
                  {workspaces.map((ws) => (
                    <button
                      key={ws.id}
                      className={`ws-menu-item${ws.id === activeWs?.id ? ' active' : ''}`}
                      title={ws.path}
                      onClick={() => {
                        setWsMenuOpen(false)
                        onSelectWorkspace(ws)
                      }}
                    >
                      <IconFolder size={13} />
                      <span className="name">{ws.title}</span>
                    </button>
                  ))}
                  <button
                    className="ws-menu-item create"
                    onClick={() => {
                      setWsMenuOpen(false)
                      onAddWorkspace()
                    }}
                  >
                    <IconPlus size={13} />
                    <span className="name">{t('addWorkspaceMenu')}</span>
                  </button>
                </div>
              </>
            )}
          </div>
          <textarea
            placeholder={inert ? t('placeholderWorkspace') : t('chatPlaceholder')}
            value={prompt}
            onChange={(event) => setPrompt(event.target.value)}
            onFocus={() => {
              if (inert) onOpenPicker()
            }}
            onKeyDown={(event) => {
              if (event.key === 'Enter' && !event.shiftKey) {
                event.preventDefault()
                if (!running) void send()
              }
            }}
          />
          <div className="composer-toolbar">
            {catalog && selection && (
              <>
                <select
                  className="chip-select"
                  value={selKey}
                  onChange={(event) => {
                    const [provider, modelId] = event.target.value.split('::')
                    const nextGroup = catalog.groups.find((g) => g.id === provider)
                    const nextModel = nextGroup?.models.find((m) => m.id === modelId)
                    setSelection({
                      provider,
                      model: modelId,
                      reasoningEffort: nextModel?.reasoning?.defaultEffort,
                    })
                  }}
                >
                  {catalog.groups.map((g) => (
                    <optgroup key={g.id} label={g.name}>
                      {g.models.map((m) => (
                        <option key={m.id} value={`${g.id}::${m.id}`}>
                          {m.name}
                        </option>
                      ))}
                    </optgroup>
                  ))}
                </select>
                {efforts.length > 0 && (
                  <select
                    className="chip-select"
                    value={selection.reasoningEffort ?? ''}
                    onChange={(event) => {
                      const value = event.target.value
                      setSelection({ ...selection, reasoningEffort: value || undefined })
                    }}
                  >
                    <option value="">{t('providerDefault')}</option>
                    {efforts.map((effort) => (
                      <option key={effort.id} value={effort.id}>
                        {effort.name}
                      </option>
                    ))}
                  </select>
                )}
              </>
            )}
            <span className="spacer" />
            {running ? (
              <button className="btn-send stop" onClick={() => void stop()} title={t('stop')}>
                <IconStop size={14} />
              </button>
            ) : (
              <button
                className="btn-send"
                disabled={!prompt.trim() || sending}
                onClick={() => void send()}
                title={t('run')}
              >
                <IconSend size={16} />
              </button>
            )}
          </div>
        </div>
      </div>
    </>
  )
}
