import { useCallback, useEffect, useRef, useState, type KeyboardEvent, type WheelEvent } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import { sessionDisplayTitle } from '../sessionDisplay'
import type { Notify } from '../App'
import type { StreamListener } from '../hooks/useSessionStreams'
import type { TranscriptNode } from '../fold'
import { SessionView } from '../components/SessionView'
import { StatsBar } from '../components/StatsBar'
import { ComposerModelMenu } from '../components/ComposerModelMenu'
import {
  BrandMark,
  IconChevron,
  IconFolder,
  IconPlus,
  IconSend,
  IconStop,
} from '../components/icons'
import { resolveSessionReasoningEffort } from '../modelCatalog'
import type {
  ModelCatalog,
  ModelSelection,
  SessionSummary,
  WorkspaceRecord,
} from '../types'

function normalizeSelection(catalog: ModelCatalog, selection: ModelSelection): ModelSelection {
  const group = catalog.groups.find((entry) => entry.id === selection.provider)
  const model = group?.models.find((entry) => entry.id === selection.model)
  const efforts = model?.reasoning?.efforts ?? []
  return {
    ...selection,
    reasoningEffort: resolveSessionReasoningEffort(efforts, selection.reasoningEffort),
  }
}

const LAST_MODEL_KEY = 'denia.last-model'
const LAST_MODEL_KEY_LEGACY = 'dsh-rs.last-model'
const TURN_LOCK_TIMEOUT = 5000
/** dsh ChatView FOLLOW_THRESHOLD */
const FOLLOW_THRESHOLD = 24

export default function SessionsPage({
  activeId,
  activeSession,
  activeWs,
  workspaces,
  running,
  attach,
  locked,
  hasStarted,
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
  hasStarted: boolean
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
  const [atBottom, setAtBottom] = useState(true)
  // 当前会话的 transcript 节点,供状态栏统计。
  const [transcriptNodes, setTranscriptNodes] = useState<TranscriptNode[]>([])
  const lockTimer = useRef<number | undefined>(undefined)
  const promptRef = useRef<HTMLTextAreaElement | null>(null)
  const scrollRef = useRef<HTMLDivElement | null>(null)
  const atBottomRef = useRef(true)
  const seatRef = useRef<HTMLDivElement | null>(null)

  const syncPromptHeight = () => {
    const el = promptRef.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${el.scrollHeight}px`
  }

  useEffect(() => {
    setPrompt('')
    setSending(false)
    setTranscriptNodes([])
    window.clearTimeout(lockTimer.current)
    atBottomRef.current = true
    setAtBottom(true)
    const el = scrollRef.current
    if (el !== null) el.scrollTop = 0
  }, [activeId])

  useEffect(() => {
    let cancelled = false
    api
      .getCatalog()
      .then((next) => {
        if (!cancelled) setCatalog(next)
      })
      .catch((error) => {
        if (!cancelled) {
          notify('err', error instanceof Error ? error.message : String(error))
        }
      })
    return () => {
      cancelled = true
    }
  }, [catalogTick, notify])

  const loadLastModel = (current: ModelCatalog): ModelSelection | null => {
    try {
      let raw = window.localStorage.getItem(LAST_MODEL_KEY)
      if (!raw) {
        // 改名 dsh-rs → Denia 前的旧 key:搬一次后清掉,用户选择不丢。
        raw = window.localStorage.getItem(LAST_MODEL_KEY_LEGACY)
        if (raw) {
          window.localStorage.setItem(LAST_MODEL_KEY, raw)
          window.localStorage.removeItem(LAST_MODEL_KEY_LEGACY)
        }
      }
      if (!raw) return null
      const parsed = JSON.parse(raw) as ModelSelection
      const group = current.groups.find((g) => g.id === parsed.provider)
      if (!group?.models.some((m) => m.id === parsed.model)) return null
      return normalizeSelection(current, parsed)
    } catch {
      return null
    }
  }

  useEffect(() => {
    if (!catalog || selection) return
    setSelection(normalizeSelection(catalog, loadLastModel(catalog) ?? catalog.default))
  }, [catalog, selection])

  useEffect(() => {
    if (!catalog || !selection) return
    const valid = catalog.groups.some(
      (g) => g.id === selection.provider && g.models.some((m) => m.id === selection.model),
    )
    if (!valid) setSelection(normalizeSelection(catalog, loadLastModel(catalog) ?? catalog.default))
  }, [catalog, selection])

  const applySelection = (next: ModelSelection) => {
    if (!catalog) return
    const normalized = normalizeSelection(catalog, next)
    setSelection(normalized)
    try {
      window.localStorage.setItem(LAST_MODEL_KEY, JSON.stringify(normalized))
    } catch {
      /* storage unavailable */
    }
  }

  useEffect(() => {
    if (running && sending) {
      setSending(false)
      window.clearTimeout(lockTimer.current)
    }
  }, [running, sending])

  useEffect(() => () => window.clearTimeout(lockTimer.current), [])

  const inert = !activeId && !activeWs
  const hasHistory = Boolean(activeSession?.excerpt)
  const showTranscript = Boolean(activeId && (hasStarted || sending || hasHistory))
  const phase = showTranscript ? 'active' : 'hero'
  const promptEmpty = !prompt.trim()
  const primaryStops = running && promptEmpty

  // 当前选中模型的上下文窗口,供状态栏显示占用环。
  const activeContextWindow = (() => {
    if (!catalog || !selection) return undefined
    const group = catalog.groups.find((g) => g.id === selection.provider)
    const model = group?.models.find((m) => m.id === selection.model)
    return model?.contextWindow ?? group?.models[0]?.contextWindow
  })()

  const snapToBottom = useCallback(() => {
    const el = scrollRef.current
    if (el === null) return
    el.scrollTop = el.scrollHeight
    atBottomRef.current = true
    setAtBottom(true)
  }, [])

  const followIfPinned = useCallback(() => {
    if (!atBottomRef.current) return
    snapToBottom()
  }, [snapToBottom])

  const handleScroll = useCallback(() => {
    const el = scrollRef.current
    if (el === null) return
    const bottom = el.scrollHeight - el.scrollTop - el.clientHeight <= FOLLOW_THRESHOLD + 1
    if (bottom !== atBottomRef.current) {
      atBottomRef.current = bottom
      setAtBottom(bottom)
    }
  }, [])

  useEffect(() => {
    followIfPinned()
  }, [running, followIfPinned])

  useEffect(() => {
    if (scrollTick === 0) return
    snapToBottom()
  }, [scrollTick, snapToBottom])

  useEffect(() => {
    syncPromptHeight()
  }, [prompt, phase])

  // dsh: composer seat 高度变化时,贴底读者保持视野。
  useEffect(() => {
    const seat = seatRef.current
    const scroller = scrollRef.current
    if (seat === null || scroller === null) return
    const observer = new ResizeObserver(() => {
      scroller.style.setProperty('--composer-height', `${seat.offsetHeight}px`)
      followIfPinned()
    })
    observer.observe(seat)
    observer.observe(scroller)
    return () => observer.disconnect()
  }, [followIfPinned])

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
        setSending(false)
        onOpenPicker()
        return
      }
      onStarted(id)
      await api.postPrompt(id, {
        prompt: message,
        provider: selection?.provider,
        model: selection?.model,
        reasoningEffort: selection?.reasoningEffort,
      })
      setPrompt('')
      setScrollTick((tick) => tick + 1)
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

  const onPromptKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key !== 'Enter' || event.shiftKey) return
    event.preventDefault()
    if (primaryStops) {
      void stop()
      return
    }
    if (!sending) void send()
  }

  const onPromptWheel = (event: WheelEvent<HTMLTextAreaElement>) => {
    const el = scrollRef.current
    const field = promptRef.current
    if (el === null || field === null) return
    const atTop = field.scrollTop <= 0
    const atEnd = field.scrollTop + field.clientHeight >= field.scrollHeight - 1
    const up = event.deltaY < 0
    const down = event.deltaY > 0
    if ((up && atTop) || (down && atEnd)) {
      el.scrollTop += event.deltaY
      event.preventDefault()
    }
  }

  const workspaceRow = (
    <div className="composer-workspace hero-ws-row">
      <button
        type="button"
        className={`ws-chip${wsMenuOpen ? ' open' : ''}`}
        aria-expanded={wsMenuOpen}
        onClick={() => {
          if (workspaces.length === 0) onOpenPicker()
          else setWsMenuOpen((open) => !open)
        }}
      >
        <IconFolder size={14} />
        <span className={activeWs ? 'label' : 'label dim'}>
          {activeWs?.title ?? t('heroChooseWorkspace')}
        </span>
        <IconChevron size={11} />
      </button>
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
                  type="button"
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
              type="button"
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
  )

  const composerCard = (
    <div
      className={`prompt-panel${inert ? ' pick-target' : ''}`}
      onClick={inert ? onOpenPicker : undefined}
    >
      <div className="prompt-scroll">
        <textarea
          ref={promptRef}
          rows={1}
          placeholder={
            inert
              ? t('placeholderWorkspace')
              : phase === 'hero'
                ? t('placeholderHero')
                : t('chatPlaceholder')
          }
          value={prompt}
          aria-label={inert ? t('heroChooseWorkspace') : t('chatPlaceholder')}
          readOnly={inert}
          onChange={(event) => {
            setPrompt(event.target.value)
            syncPromptHeight()
          }}
          onFocus={() => {
            if (inert) onOpenPicker()
          }}
          onKeyDown={onPromptKeyDown}
          onWheel={onPromptWheel}
        />
      </div>
      <div className="prompt-bar">
        <div className="composer-modes">
          {catalog && selection && (
            <ComposerModelMenu
              catalog={catalog}
              selection={selection}
              disabled={inert}
              onChange={applySelection}
            />
          )}
        </div>
        <div>
          {primaryStops ? (
            <button
              type="button"
              className="btn-send stop"
              onClick={() => void stop()}
              title={t('stop')}
            >
              <IconStop size={14} />
            </button>
          ) : (
            <button
              type="button"
              className="btn-send"
              disabled={promptEmpty || sending || inert}
              onClick={() => void send()}
              title={t('run')}
            >
              <IconSend size={16} />
            </button>
          )}
        </div>
      </div>
    </div>
  )

  return (
    <div className="session-layout" data-phase={phase}>
      {phase === 'active' && (
        <header className="session-header">
          <h1 className="session-title">
            {activeSession ? sessionDisplayTitle(activeSession) : t('blankSession')}
          </h1>
          {activeSession?.cwd_alive === false && (
            <span className="badge err" title={t('deadCwdHint')}>
              {t('deadCwd')}
            </span>
          )}
        </header>
      )}
      <div
        className="conversation-scroll"
        ref={scrollRef}
        onScroll={handleScroll}
        data-conversation-scroll=""
      >
        <div className="conversation-view">
          {showTranscript && activeId ? (
            <SessionView
              id={activeId}
              running={running}
              attach={attach}
              onNodesChange={(nodes) => {
                setTranscriptNodes(nodes)
                followIfPinned()
              }}
            />
          ) : (
            <div className="session-hero">
              <div className="hero-headline">
                <span className="hero-mark-hitbox">
                  <BrandMark size={30} />
                </span>
                <span className="hero-headline-text">{t('heroTitle')}</span>
              </div>
            </div>
          )}
        </div>
        <div className="composer-seat" ref={seatRef} data-composer-seat="">
          <div className={`composer-stack${phase === 'hero' ? ' composer-hero' : ''}`}>
            {phase === 'hero' && workspaceRow}
            {composerCard}
            {phase === 'active' && (
              <StatsBar
                nodes={transcriptNodes}
                running={running}
                contextWindow={activeContextWindow}
              />
            )}
          </div>
        </div>
      </div>
      {phase === 'active' && !atBottom && (
        <button
          type="button"
          className="jump-bottom"
          onClick={snapToBottom}
          title={t('jumpToBottom')}
        >
          <IconChevron size={14} />
        </button>
      )}
    </div>
  )
}
