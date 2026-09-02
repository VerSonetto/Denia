import { useCallback, useEffect, useRef, useState, type KeyboardEvent, type WheelEvent } from 'react'
import * as api from '../api'
import {
  ensureSession,
  getActiveWorkspace,
  markStarted,
  notify,
  setActiveId,
  setRunningStatus,
  useCatalogTick,
  useRunningFor,
  useSessions,
  useWorkspaces,
} from '../appStore'
import { t } from '../i18n'
import { sessionDisplayTitle } from '../sessionDisplay'
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
import { TodoPanel } from '../components/TodoPanel'
import { ConversationAxis } from '../components/ConversationAxis'
import type {
  ModelCatalog,
  ModelSelection,
  SessionSummary,
  TodoItem,
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
// 品牌改名前的旧 key 字面量,故意保留 dsh-rs:只用于读取并搬运老用户的选择。
const LAST_MODEL_KEY_LEGACY = 'dsh-rs.last-model'
/** dsh ChatView FOLLOW_THRESHOLD */
const FOLLOW_THRESHOLD = 24

export default function SessionsPage({
  activeId,
  locked,
  hasStarted,
  onSelectWorkspace,
  onOpenPicker,
  onAddWorkspace,
}: {
  activeId: string | null
  locked: boolean
  hasStarted: boolean
  onSelectWorkspace: (ws: WorkspaceRecord) => void
  onOpenPicker: () => void
  onAddWorkspace: () => void
}) {
  const sessions = useSessions()
  const workspaces = useWorkspaces()
  const catalogTick = useCatalogTick()
  const activeSession: SessionSummary | null = activeId
    ? (sessions.find((s) => s.id === activeId) ?? null)
    : null

  // 运行状态直接订阅引擎(侧栏/状态栏同源)。
  const running = useRunningFor(activeId)

  const [catalog, setCatalog] = useState<ModelCatalog | null>(null)
  const [prompt, setPrompt] = useState('')
  const [selection, setSelection] = useState<ModelSelection | null>(null)
  const [wsMenuOpen, setWsMenuOpen] = useState(false)
  const [sending, setSending] = useState(false)
  const [scrollTick, setScrollTick] = useState(0)
  const [atBottom, setAtBottom] = useState(true)
  // 已发送未获服务端确认的用户消息(乐观行)。
  const [pendingTexts, setPendingTexts] = useState<string[]>([])
  // 当前会话的 transcript 节点,供状态栏统计。
  const [transcriptNodes, setTranscriptNodes] = useState<TranscriptNode[]>([])
  const [todos, setTodos] = useState<TodoItem[]>([])
  const sendingRef = useRef(false)
  const promptRef = useRef<HTMLTextAreaElement | null>(null)
  const scrollRef = useRef<HTMLDivElement | null>(null)
  const atBottomRef = useRef(true)
  const seatRef = useRef<HTMLDivElement | null>(null)

  const activeWs: WorkspaceRecord | null = getActiveWorkspace()

  const syncPromptHeight = () => {
    const el = promptRef.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${el.scrollHeight}px`
  }

  useEffect(() => {
    setPrompt('')
    setSending(false)
    setPendingTexts([])
    sendingRef.current = false
    setTranscriptNodes([])
    setTodos([])
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
  }, [catalogTick])

  const loadLastModel = (current: ModelCatalog): ModelSelection | null => {
    try {
      let raw = window.localStorage.getItem(LAST_MODEL_KEY)
      if (!raw) {
        // 改名 denia → Denia 前的旧 key:搬一次后清掉,用户选择不丢。
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
    }
  }, [running, sending])

  const inert = !activeId && !activeWs
  const hasHistory = Boolean(activeSession?.excerpt)
  const showTranscript = Boolean(activeId && (hasStarted || sending || hasHistory || pendingTexts.length > 0))
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
      scroller.style.setProperty('--conversation-viewport-height', `${scroller.clientHeight}px`)
      followIfPinned()
    })
    observer.observe(seat)
    observer.observe(scroller)
    return () => observer.disconnect()
  }, [followIfPinned, phase])

  /**
   * 乐观行协调:
   * - push:发送 202 后注入乐观行。
   * - settle:流中已出现同文本 user-message(可能先于 push,
   *   因为 202 返回前服务端已落库并推送)→ 从列表移除并计数确认。
   * 确认计数保证竞态下不丢:settle 先到时,push 不再注入。
   */
  const confirmedRef = useRef(new Map<string, number>())

  const settlePending = useCallback((text: string) => {
    confirmedRef.current.set(text, (confirmedRef.current.get(text) ?? 0) + 1)
    setPendingTexts((previous) => {
      const index = previous.indexOf(text)
      if (index < 0) return previous
      const next = [...previous]
      next.splice(index, 1)
      return next
    })
  }, [])

  const pushPending = useCallback((text: string) => {
    const confirmed = confirmedRef.current.get(text) ?? 0
    if (confirmed > 0) {
      // 该条已被服务端流确认:无需乐观行。
      confirmedRef.current.set(text, confirmed - 1)
      return
    }
    setPendingTexts((previous) => [...previous, text])
  }, [])

  /**
   * 发送流程(乐观):
   * 1. 确保会话存在(复用/创建);
   * 2. postPrompt 成功 → 立即注入乐观用户行 + running,界面零等待;
   * 3. 服务端事件(同文本 user-message)到达 → 移除乐观行(由 Server 流确认);
   * 4. 失败 → 移除乐观行、恢复输入框、报错。
   */
  const send = async () => {
    const message = prompt.trim()
    if (!message || sendingRef.current) return
    if (inert) {
      onOpenPicker()
      return
    }
    sendingRef.current = true
    setSending(true)
    try {
      const id = await ensureSession(activeWs)
      if (!id) {
        onOpenPicker()
        return
      }
      markStarted(id)
      await api.postPrompt(id, {
        prompt: message,
        provider: selection?.provider,
        model: selection?.model,
        reasoningEffort: selection?.reasoningEffort,
      })
      // 收到 202:服务端已接单,立即乐观反馈(running 也由服务端 SSE 推送)。
      pushPending(message)
      setRunningStatus(id, true)
      setPrompt('')
      setScrollTick((tick) => tick + 1)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
      // 保留输入框内容;若会话侧已经创建但发送失败,等待用户重试。
    } finally {
      sendingRef.current = false
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
        {phase === 'active' && (
          <ConversationAxis nodes={transcriptNodes} scrollRef={scrollRef} />
        )}
        <div className="conversation-view">
          {showTranscript && activeId ? (
            <SessionView
              id={activeId}
              pendingTexts={pendingTexts}
              onTodosChange={setTodos}
              onPendingSettled={settlePending}
              onNotFound={() => {
                // 会话已被删除(其他窗口/流 404):清空视图。
                setActiveId(null, null)
              }}
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
            {phase === 'active' && <TodoPanel todos={todos} />}
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
