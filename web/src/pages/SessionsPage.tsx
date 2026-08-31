import { useEffect, useState } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import type { Notify } from '../App'
import { SessionView } from '../components/SessionView'
import {
  BrandMark,
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
  // 本会话是否已发出首条消息:发出后不再回欢迎页,不等 excerpt 刷新。
  const [started, setStarted] = useState(false)

  useEffect(() => {
    setStarted(false)
  }, [activeId])

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
  // 新会话页(dsh blank 态):空白会话且还没发过消息时显示居中欢迎布局;
  // running 不参与判断——SessionView 挂载会自己汇报 running,免得闪回。
  const showWelcome = !started && (!activeSession || !activeSession.excerpt)

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
      // 视图立即切到 transcript,不依赖 excerpt/running 的异步刷新。
      setStarted(true)
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

  // 输入卡两种布局共用:欢迎页居中、会话页底部停靠。
  const composerCard = (
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
  )

  return (
    <>
      {showWelcome ? (
        <div className="welcome">
          <BrandMark size={44} />
          <h1>{t('heroTitle')}</h1>
          <p>{activeWs ? t('heroSub') : t('placeholderWorkspace')}</p>
          <div className="welcome-composer">{composerCard}</div>
          {!activeWs && (
            <button className="welcome-cta" onClick={onOpenPicker}>
              <IconFolder size={13} />
              {t('heroChooseWorkspace')}
            </button>
          )}
        </div>
      ) : (
        <>
          {activeId && (
            <SessionView id={activeId} notify={notify} onRunningChange={onRunningChange} />
          )}
          <div className="composer-zone">{composerCard}</div>
        </>
      )}
    </>
  )
}
