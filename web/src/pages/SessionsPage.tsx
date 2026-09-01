import { useEffect, useRef, useState } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import type { Notify } from '../App'
import type { StreamListener } from '../hooks/useSessionStreams'
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

const LAST_MODEL_KEY = 'dsh-rs.last-model'
/** 发送成功后,锁到 running 确认(或超时兜底)才允许下一条。 */
const TURN_LOCK_TIMEOUT = 5000

export default function SessionsPage({
  activeId,
  activeSession,
  activeWs,
  workspaces,
  running,
  attach,
  locked,
  catalogTick,
  notify,
  onStarted,
  onSelectWorkspace,
  onOpenPicker,
  onAddWorkspace,
  onEnsureSession,
}: {
  activeId: string | null
  activeSession: SessionSummary | null
  activeWs: WorkspaceRecord | null
  workspaces: WorkspaceRecord[]
  running: boolean
  attach: (id: string, listener: StreamListener) => () => void
  locked: boolean
  catalogTick: number
  notify: Notify
  onStarted: (id: string) => void
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
  const [scrollTick, setScrollTick] = useState(0)
  // 本会话是否已发出首条消息:发出后不再回欢迎页,不等 excerpt 刷新。
  const [started, setStarted] = useState(false)
  const lockTimer = useRef<number | undefined>(undefined)

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
  }, [catalogTick, notify])

  /** 用户上次选择的模型;校验在目录中存在,否则回落。 */
  const loadLastModel = (current: ModelCatalog): ModelSelection | null => {
    try {
      const raw = window.localStorage.getItem(LAST_MODEL_KEY)
      if (!raw) return null
      const parsed = JSON.parse(raw) as ModelSelection
      const group = current.groups.find((g) => g.id === parsed.provider)
      if (!group?.models.some((m) => m.id === parsed.model)) return null
      return parsed
    } catch {
      return null
    }
  }

  // 初始选择:用户上次的选择优先,否则跟随默认。
  useEffect(() => {
    if (catalog && !selection) setSelection(loadLastModel(catalog) ?? catalog.default)
  }, [catalog, selection])

  // 目录重拉后校验当前选择仍有效(默认模型改动/提供方被删)。
  useEffect(() => {
    if (!catalog || !selection) return
    const valid = catalog.groups.some(
      (g) => g.id === selection.provider && g.models.some((m) => m.id === selection.model),
    )
    if (!valid) setSelection(loadLastModel(catalog) ?? catalog.default)
  }, [catalog, selection])

  // 手动变更选择时持久化,刷新/新会话恢复。
  const applySelection = (next: ModelSelection) => {
    setSelection(next)
    try {
      window.localStorage.setItem(LAST_MODEL_KEY, JSON.stringify(next))
    } catch {
      /* storage unavailable */
    }
  }

  // A4:运行确认后解锁发送;超时兜底防挂死。
  useEffect(() => {
    if (running && sending) {
      setSending(false)
      window.clearTimeout(lockTimer.current)
    }
  }, [running, sending])

  useEffect(() => () => window.clearTimeout(lockTimer.current), [])

  // 无会话且无目标工作区 ⇒ 输入栏 inert,点击弹工作区选择(dsh 硬前置)。
  const inert = !activeId && !activeWs
  // 新会话页(dsh blank 态):空白会话且还没发过消息时显示居中欢迎布局;
  // running 不参与判断——总线会自己汇报 running,免得闪回。
  const showWelcome = !started && (!activeSession || !activeSession.excerpt)

  const send = async () => {
    const message = prompt.trim()
    if (!message || sending || running) return
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
      onStarted(id)
      // 发送是明确的看新内容意图:强制吸附回底部。
      setScrollTick((tick) => tick + 1)
      // 锁到下一条的 running 确认(running 到达即解锁,超时兜底)。
      window.clearTimeout(lockTimer.current)
      lockTimer.current = window.setTimeout(() => setSending(false), TURN_LOCK_TIMEOUT)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
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

  const switchModel = (provider: string, modelId: string) => {
    const nextGroup = catalog?.groups.find((g) => g.id === provider)
    const nextModel = nextGroup?.models.find((m) => m.id === modelId)
    const nextEfforts = nextModel?.reasoning?.efforts ?? []
    // 目标模型支持当前 effort 则保留;不支持才回退默认,不再静默丢失。
    const effort =
      selection?.reasoningEffort && nextEfforts.some((e) => e.id === selection.reasoningEffort)
        ? selection.reasoningEffort
        : nextModel?.reasoning?.defaultEffort
    applySelection({ provider, model: modelId, reasoningEffort: effort })
  }

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
              {workspaces.map((ws) => {
                const notCurrent = ws.id !== activeWs?.id
                const blocked = locked && notCurrent
                return (
                  <button
                    key={ws.id}
                    className={`ws-menu-item${ws.id === activeWs?.id ? ' active' : ''}`}
                    title={blocked ? t('workspaceLockedTitle') : ws.path}
                    disabled={blocked}
                    onClick={() => {
                      setWsMenuOpen(false)
                      onSelectWorkspace(ws)
                    }}
                  >
                    <IconFolder size={13} />
                    <span className="name">{ws.title}</span>
                  </button>
                )
              })}
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
            if (!running && !sending) void send()
          }
        }}
      />
      <div className="composer-toolbar">
        {catalog && selection && (
          <>
            <select
              className="chip-select"
              value={selKey}
              disabled={running}
              title={
                running
                  ? t('modelLockedWhileRunning')
                  : `${t('sessionModelHint')} · ${t('modelLabel')}`
              }
              onChange={(event) => {
                const [provider, modelId] = event.target.value.split('::')
                switchModel(provider, modelId)
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
                disabled={running}
                title={running ? t('modelLockedWhileRunning') : t('reasoningLabel')}
                onChange={(event) => {
                  const value = event.target.value
                  applySelection({ ...selection, reasoningEffort: value || undefined })
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
            <SessionView
              id={activeId}
              running={running}
              scrollTick={scrollTick}
              attach={attach}
            />
          )}
          <div className="composer-zone">{composerCard}</div>
        </>
      )}
    </>
  )
}