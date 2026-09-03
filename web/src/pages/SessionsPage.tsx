import { useCallback, useEffect, useRef, useState, type ClipboardEvent, type KeyboardEvent, type WheelEvent } from 'react'
import * as api from '../api'
import { optimizePromptText } from '../promptOptimizer'
import {
  addSessionLocal,
  ensureSession,
  getActiveWorkspace,
  markStarted,
  notify,
  refreshList,
  setActiveId,
  setRunningStatus,
  useCatalogTick,
  useRunningFor,
  useSessions,
  useWorkspaces,
} from '../appStore'
import { t } from '../i18n'
import { attach } from '../sessionStreams'
import { sessionDisplayTitle } from '../sessionDisplay'
import type { TranscriptNode } from '../fold'
import type { TrajectoryQuote } from '../trajectory'
import { SessionView } from '../components/SessionView'
import { StatsBar } from '../components/StatsBar'
import { ComposerModelMenu } from '../components/ComposerModelMenu'
import { PermissionSelector, loadPermission, type PermissionLevel } from '../components/PermissionSelector'
import { ApprovalDialog, type ApprovalDecision, type ApprovalRequest } from '../components/ApprovalDialog'
import { ConfirmDialog } from '../components/ConfirmDialog'
import { ContextRing } from '../components/ContextRing'
import {
  BrandMark,
  IconBranch,
  IconChevron,
  IconClose,
  IconDownload,
  IconFolder,
  IconImage,
  IconPlus,
  IconPaperclip,
  IconRewind,
  IconSend,
  IconSparkles,
  IconSpinner,
  IconStop,
} from '../components/icons'
import { resolveSessionReasoningEffort } from '../modelCatalog'
import { TodoPanel } from '../components/TodoPanel'
import { ConversationAxis } from '../components/ConversationAxis'
import {
  downloadFile,
  serializeSession,
  type ExportFormat,
} from '../lib/exportSession'
import type {
  CatalogModel,
  ModelCatalog,
  ModelSelection,
  PermissionMode,
  SessionEnvelope,
  SessionSummary,
  TodoItem,
  UserMessageImage,
  WorkspaceRecord,
} from '../types'
import { isGenericImageName } from '../userImages'

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

/** 乐观行内容指纹:文本 + 内联图片都参与匹配,避免多条纯图片消息("图片")互相误删。 */
function pendingMessageKey(message: { text: string; images?: UserMessageImage[] }): string {
  const images = message.images ?? []
  return `${message.text}\u0000${images.map((image) => `${image.mime}\u0000${image.data}`).join('\u0001')}`
}

/** dsh 权限模式 → 前端三档枚举。 */
function permissionLevelFromMode(mode: PermissionMode): PermissionLevel {
  return mode === 'danger-full-access' ? 'full' : mode
}

/** 前端三档枚举 → dsh 权限模式。 */
function permissionModeForLevel(level: PermissionLevel): PermissionMode {
  return level === 'full' ? 'danger-full-access' : level
}

/** 从事件流反向找最近一次 permission-mode。 */
function latestPermissionMode(events: SessionEnvelope[]): PermissionLevel | null {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index]
    if (event.type === 'permission-mode') return permissionLevelFromMode(event.mode)
  }
  return null
}

/** 从事件流恢复仍未结算的审批请求(approval-asked 未被对应 decided 关闭)。 */
function latestPendingApproval(events: SessionEnvelope[]): ApprovalRequest | null {
  let pending: ApprovalRequest | null = null
  for (const event of events) {
    if (event.type === 'approval-asked') {
      pending = {
        requestId: event.request_id,
        toolName: event.tool,
        argsPreview: event.args_preview,
        reason: event.reason,
      }
    } else if (event.type === 'approval-decided' && pending?.requestId === event.request_id) {
      pending = null
    }
  }
  return pending
}

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
  const [optimizing, setOptimizing] = useState(false)
  const [optimizedPrompt, setOptimizedPrompt] = useState<string | null>(null)
  const originalPromptRef = useRef('')
  const optimizeAbortRef = useRef<AbortController | null>(null)
  const optimizingRef = useRef(false)
  const [scrollTick, setScrollTick] = useState(0)
  const [atBottom, setAtBottom] = useState(true)
  // 已发送未获服务端确认的用户消息(乐观行)。
  const [pendingMessages, setPendingMessages] = useState<
    { text: string; images?: UserMessageImage[] }[]
  >([])
  // 回退确认框状态;changes 为空时只做简单确认。
  const [rewindReq, setRewindReq] = useState<{
    seq: number
    text: string
    images?: UserMessageImage[]
    changes: api.FileDiffEntry[]
    removedMessages: number
  } | null>(null)
  const [rewindBusy, setRewindBusy] = useState(false)
  // 回退成功后递增,强制重挂载 SessionView 重新拉快照。
  const [transcriptReloadTick, setTranscriptReloadTick] = useState(0)
  // 当前会话的 transcript 节点,供状态栏统计。
  const [transcriptNodes, setTranscriptNodes] = useState<TranscriptNode[]>([])
  const [todos, setTodos] = useState<TodoItem[]>([])
  // 会话内容视图(dsh conversation.view 环):对话 / 轨迹。组件按
  // activeId 重挂(key),视图状态随会话切换自然复位。
  const [view, setView] = useState<'chat' | 'trajectory'>('chat')
  // 导出菜单状态:exportOpen 控制下拉显隐,exportBusy 阻及双击。
  const [exportOpen, setExportOpen] = useState(false)
  const [exportBusy, setExportBusy] = useState(false)
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
    optimizeAbortRef.current?.abort()
    optimizeAbortRef.current = null
    optimizingRef.current = false
    setOptimizing(false)
    setOptimizedPrompt(null)
    originalPromptRef.current = ''
    setPrompt('')
    setSending(false)
    setPendingMessages([])
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
  const showTranscript = Boolean(
    activeId && (hasStarted || sending || hasHistory || pendingMessages.length > 0),
  )
  const phase = showTranscript ? 'active' : 'hero'

  /* ---- 粘贴图片 / 附件上传 / 权限与审批 ---- */

  const [pastedImages, setPastedImages] = useState<{
    name: string
    mime: string
    data: string
    bytes: number
    preview: string
  }[]>([])
  const [attachments, setAttachments] = useState<{ name: string; mime: string; file: File }[]>([])
  // 轨迹引用(芯片挂在输入框上方,随消息以 injected 上下文注入)。
  const [trajQuotes, setTrajQuotes] = useState<TrajectoryQuote[]>([])
  const promptEmpty = !prompt.trim() && pastedImages.length === 0
  const primaryStops = running && promptEmpty
  const [permission, setPermission] = useState<PermissionLevel>(() => loadPermission())
  const [permissionBusy, setPermissionBusy] = useState(false)
  const [fullAccessConfirm, setFullAccessConfirm] = useState(false)
  const pendingFullAccessRef = useRef<PermissionLevel | null>(null)
  const [approvalReq, setApprovalReq] = useState<ApprovalRequest | null>(null)
  const fileInputRef = useRef<HTMLInputElement | null>(null)

  /** 切换当前会话权限(活动会话直接写后端;无活动会话先记本地,首次发送时带上)。 */
  const applyPermission = useCallback(
    (level: PermissionLevel) => {
      if (activeId && level === permission) return
      setPermission(level)
      try {
        window.localStorage.setItem('denia.permission', level)
        if (level === 'full') {
          window.localStorage.setItem('denia.permission.migrated', '1')
        }
      } catch {
        /* storage unavailable */
      }
      if (!activeId) return
      setPermissionBusy(true)
      api
        .setSessionPermission(activeId, permissionModeForLevel(level))
        .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
        .finally(() => setPermissionBusy(false))
    },
    [activeId, permission, notify],
  )

  /** 选择权限:完整权限走 dsh 同款风险确认,其余直接写。 */
  const requestPermissionChange = useCallback(
    (level: PermissionLevel) => {
      if (level === 'full' && permission !== 'full') {
        pendingFullAccessRef.current = level
        setFullAccessConfirm(true)
        return
      }
      void applyPermission(level)
    },
    [permission, applyPermission],
  )

  const confirmFullAccess = useCallback(() => {
    const level = pendingFullAccessRef.current ?? 'full'
    pendingFullAccessRef.current = null
    setFullAccessConfirm(false)
    void applyPermission(level)
  }, [applyPermission])

  const cancelFullAccess = useCallback(() => {
    pendingFullAccessRef.current = null
    setFullAccessConfirm(false)
  }, [])

  /** 把用户在审批弹窗里的选择 POST 回后端,driver 随即继续/拒绝该调用。 */
  const handleApproval = useCallback(
    (decision: ApprovalDecision) => {
      if (!approvalReq || !activeId) return
      const request = approvalReq
      setApprovalReq(null)
      api
        .answerApproval(activeId, request.requestId, decision)
        .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
    },
    [approvalReq, activeId, notify],
  )

  // 跟随会话事件流:同步服务端权限模式,并监听后端发起的审批请求。
  useEffect(() => {
    if (!activeId) {
      setApprovalReq(null)
      return
    }
    const unsubscribe = attach(activeId, {
      onSnapshot: (_header, events) => {
        const mode = latestPermissionMode(events)
        if (mode) setPermission(mode)
        setApprovalReq(latestPendingApproval(events))
      },
      onEnvelope: (event) => {
        if (event.type === 'permission-mode') {
          setPermission(permissionLevelFromMode(event.mode))
        } else if (event.type === 'approval-asked') {
          setApprovalReq({
            requestId: event.request_id,
            toolName: event.tool,
            argsPreview: event.args_preview,
            reason: event.reason,
          })
        } else if (event.type === 'approval-decided') {
          setApprovalReq((current) =>
            current?.requestId === event.request_id ? null : current,
          )
        }
      },
    })
    return () => {
      unsubscribe()
      setApprovalReq(null)
    }
  }, [activeId])

  const visionModelInfo = useCallback((): CatalogModel | null => {
    if (!catalog || !selection) return null
    const group = catalog.groups.find((g) => g.id === selection.provider)
    return group?.models.find((m) => m.id === selection.model) ?? null
  }, [catalog, selection])

  const modelSupportsVision = useCallback((): boolean => {
    const model = visionModelInfo()
    return !!model?.inputModalities?.includes('image')
  }, [visionModelInfo])

  /** 检查模型能否识图;不能时提示并返回 false(拒绝粘贴/发送)。 */
  const ensureVision = useCallback((): boolean => {
    if (modelSupportsVision()) return true
    notify('err', `${t('imageNotSupportedTitle')}:${t('imageNotSupportedDesc')}`)
    return false
  }, [modelSupportsVision, notify])

  const readFileAsDataUrl = useCallback((file: File): Promise<string> => {
    return new Promise((resolve, reject) => {
      const reader = new FileReader()
      reader.onload = () => resolve(String(reader.result))
      reader.onerror = () => reject(reader.error)
      reader.readAsDataURL(file)
    })
  }, [])

  const handlePaste = useCallback(
    async (event: ClipboardEvent<HTMLTextAreaElement>) => {
      if (optimizing) return
      const items = event.clipboardData?.items
      if (!items) return
      const imageItems = Array.from(items).filter(
        (item) => item.kind === 'file' && item.type.startsWith('image/'),
      )
      if (imageItems.length === 0) return
      // 粘贴图片:模型必须标记为可识图,否则拒绝。
      if (!ensureVision()) {
        event.preventDefault()
        return
      }
      event.preventDefault()
      for (const item of imageItems) {
        const file = item.getAsFile()
        if (!file) continue
        try {
          const preview = await readFileAsDataUrl(file)
          const comma = preview.indexOf(',')
          setPastedImages((previous) => [
            ...previous,
            {
              name: file.name || `pasted-${Date.now()}.png`,
              mime: file.type || 'image/png',
              data: comma >= 0 ? preview.slice(comma + 1) : preview,
              bytes: file.size,
              preview,
            },
          ])
        } catch {
          /* unreadable clipboard image: ignored */
        }
      }
    },
    [ensureVision, readFileAsDataUrl, optimizing],
  )

  const onPickFiles = useCallback((event: React.ChangeEvent<HTMLInputElement>) => {
    const files = event.target.files
    if (!files) return
    for (const file of Array.from(files)) {
      setAttachments((previous) => [
        ...previous,
        { name: file.name, mime: file.type || 'application/octet-stream', file },
      ])
    }
    event.target.value = ''
  }, [])

  const fileToBase64 = useCallback((file: File): Promise<string> => {
    return new Promise((resolve, reject) => {
      const reader = new FileReader()
      reader.onload = () => {
        const result = String(reader.result)
        const comma = result.indexOf(',')
        resolve(comma >= 0 ? result.slice(comma + 1) : result)
      }
      reader.onerror = () => reject(reader.error)
      reader.readAsDataURL(file)
    })
  }, [])

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

  const settlePending = useCallback((message: { text: string; images?: UserMessageImage[] }) => {
    const key = pendingMessageKey(message)
    confirmedRef.current.set(key, (confirmedRef.current.get(key) ?? 0) + 1)
    setPendingMessages((previous) => {
      const index = previous.findIndex((candidate) => pendingMessageKey(candidate) === key)
      if (index < 0) return previous
      const next = [...previous]
      next.splice(index, 1)
      return next
    })
  }, [])

  /* ---- 回退:预览文件变化 → 确认 → 物理截断 + 恢复文件 ---- */

  const handleRewind = useCallback(async (seq: number) => {
    if (!activeId || rewindBusy) return
    const node = transcriptNodes.find(
      (n): n is Extract<TranscriptNode, { kind: 'user' }> =>
        n.kind === 'user' && n.anchor === seq,
    )
    try {
      const [checkpointsRes, diffRes] = await Promise.all([
        api.getCheckpoints(activeId),
        api.getCheckpointDiff(activeId, seq),
      ])
      const index = checkpointsRes.checkpoints.findIndex((c) => c.seq === seq)
      if (index < 0) {
        notify('err', t('rewindNotFound'))
        return
      }
      setRewindReq({
        seq,
        text: node?.text ?? checkpointsRes.checkpoints[index]?.text ?? '',
        images: node?.images,
        changes: diffRes.changes,
        removedMessages: checkpointsRes.checkpoints.length - index,
      })
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }, [activeId, transcriptNodes, rewindBusy])

  /* ---- 分支(抄 dsh forkAt):从收尾消息分叉出全新会话并立即切换 ---- */

  const forkBusyRef = useRef(false)

  const handleFork = useCallback(
    async (seq: number) => {
      if (!activeId || forkBusyRef.current) return
      forkBusyRef.current = true
      try {
        const { session } = await api.forkSession(activeId, seq)
        // 子会话挂同一工作区(dsh:fork 后立即 open 子会话)。
        addSessionLocal({ ...session, cwd_alive: true }, activeWs?.id)
        setActiveId(session.id, activeWs?.id ?? null)
        void refreshList()
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      } finally {
        forkBusyRef.current = false
      }
    },
    [activeId, activeWs],
  )

  /* ---- 轨迹引用:芯片挂到输入框上方,随消息注入(与粘贴图片一致) ---- */

  const handleQuote = useCallback((quote: TrajectoryQuote) => {
    setTrajQuotes((previous) => [...previous, quote])
    // 引用动作来自轨迹视图:切回对话视图,在输入框上方补写问题后发送。
    setView('chat')
    notify('ok', t('trajQuoted'))
  }, [])

  /* ---- 导出会话:从后端拉全量 events 后回放文件 ---- */

  const handleExport = useCallback(
    async (format: ExportFormat) => {
      if (!activeId || exportBusy) return
      setExportOpen(false)
      setExportBusy(true)
      try {
        const snapshot = await api.getSession(activeId)
        if (snapshot.events.length === 0) {
          notify('err', t('exportEmpty'))
          return
        }
        const title = activeSession ? sessionDisplayTitle(activeSession) : t('blankSession')
        const payload = serializeSession(format, {
          header: snapshot.header,
          events: snapshot.events,
          title,
        })
        downloadFile(payload.content, payload.filename, payload.mimeType)
        notify('ok', t('exportDownloaded', { name: payload.filename }))
      } catch (error) {
        notify('err', t('exportFailed'))
      } finally {
        setExportBusy(false)
      }
    },
    [activeId, activeSession, exportBusy, notify],
  )

  /* ---- 提示词优化:当前模型 + 最近 5 轮上下文,支持一键撤销 ---- */

  const handleOptimize = async () => {
    if (!activeId || !selection || optimizing || optimizingRef.current || inert || running || sending) return
    const original = prompt
    if (!original.trim()) return
    const sessionId = activeId
    const controller = new AbortController()
    optimizeAbortRef.current?.abort()
    optimizeAbortRef.current = controller
    optimizingRef.current = true
    originalPromptRef.current = original
    setOptimizedPrompt(null)
    setOptimizing(true)
    try {
      const optimized = await optimizePromptText({
        sessionId,
        prompt: original,
        selection,
        signal: controller.signal,
      })
      if (activeId !== sessionId || optimizeAbortRef.current !== controller) return
      setPrompt(optimized)
      setOptimizedPrompt(optimized)
      syncPromptHeight()
      notify('ok', t('optimizeDone'))
    } catch (error) {
      if (controller.signal.aborted || optimizeAbortRef.current !== controller) return
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      if (optimizeAbortRef.current === controller) {
        optimizeAbortRef.current = null
        optimizingRef.current = false
        setOptimizing(false)
      }
    }
  }

  const handleUndoOptimize = () => {
    if (!optimizedPrompt) return
    setPrompt(originalPromptRef.current)
    setOptimizedPrompt(null)
    syncPromptHeight()
  }

  const confirmRewind = useCallback(async () => {
    if (!activeId || !rewindReq || rewindBusy) return
    setRewindBusy(true)
    try {
      const result = await api.rewindSession(activeId, rewindReq.seq)
      // 恢复目标消息文本/图片到输入框,方便修改后重发。
      setPrompt(result.toMessage ?? rewindReq.text)
      setOptimizedPrompt(null)
      originalPromptRef.current = ''
      if (rewindReq.images && rewindReq.images.length > 0) {
        setPastedImages(
          rewindReq.images.map((image) => ({
            name: 'pasted-image.png',
            mime: image.mime,
            data: image.data,
            bytes: Math.ceil((image.data.length * 3) / 4),
            preview: `data:${image.mime};base64,${image.data}`,
          })),
        )
      } else {
        setPastedImages([])
      }
      setAttachments([])
      setPendingMessages([])
      setTranscriptNodes([])
      // 强制重挂载 SessionView,重新拉截断后的快照。
      setTranscriptReloadTick((tick) => tick + 1)
      notify('ok', t('rewindDone'))
      setRewindReq(null)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      setRewindBusy(false)
    }
  }, [activeId, rewindReq, rewindBusy])

  /* ---- 上下文窗口占用:全部由服务端 token-meter fold(对齐 dsh contextPressure 投影) ---- */

  const [context, setContext] = useState<api.ContextBreakdownResponse | null>(null)

  const refreshContextBreakdown = () => {
    if (!activeId) return
    api.contextBreakdown(activeId).then(setContext).catch(() => {
      /* 服务端取不到不报错,面板仍显示旧值 */
    })
  }

  useEffect(() => {
    refreshContextBreakdown()
    // 会话流式追加后按节拍重拉(refetch-on-interval,与 dsh 后推语义一致),
    // 比轮事件列表 O(新增) 更轻(schedule 短,值得)。
    const id = setInterval(refreshContextBreakdown, 1500)
    return () => clearInterval(id)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [activeId])

  const showAttachRow =
    pastedImages.length > 0 || attachments.length > 0 || trajQuotes.length > 0

  const pushPending = useCallback((text: string, images?: UserMessageImage[]) => {
    const message = { text, images }
    const key = pendingMessageKey(message)
    const confirmed = confirmedRef.current.get(key) ?? 0
    if (confirmed > 0) {
      // 该条已被服务端流确认:无需乐观行。
      confirmedRef.current.set(key, confirmed - 1)
      return
    }
    setPendingMessages((previous) => [...previous, message])
  }, [])

  /**
   * 发送流程(乐观):
   * 1. 确保会话存在(复用/创建);
   * 2. postPrompt 成功 → 立即注入乐观用户行 + running,界面零等待;
   * 3. 服务端事件(同文本 user-message)到达 → 移除乐观行(由 Server 流确认);
   * 4. 失败 → 移除乐观行、恢复输入框、报错。
   */
  const send = async () => {
    const trimmed = prompt.trim()
    const message =
      trimmed || (pastedImages.length > 0 ? t('pastedImageLabel') : '')
    if (optimizing || !message || sendingRef.current) return
    if (inert) {
      onOpenPicker()
      return
    }
    // 粘贴图片:模型必须标记为可识图,否则拒绝整条发送。
    if (pastedImages.length > 0 && !ensureVision()) return
    sendingRef.current = true
    setSending(true)
    try {
      // 从欢迎页发首条消息时,活动会话尚未创建;把当前选择的权限带到新会话。
      const wasHero = !activeId
      const id = await ensureSession(activeWs)
      if (!id) {
        onOpenPicker()
        return
      }
      markStarted(id)
      if (wasHero && permission !== 'workspace-write') {
        await api.setSessionPermission(id, permissionModeForLevel(permission))
      }
      // 附件上传(不限格式):先持久化,再随消息注入路径。
      const uploadedPaths: string[] = []
      for (const attachment of attachments) {
        try {
          const data = await fileToBase64(attachment.file)
          const result = await api.uploadAttachment({
            sessionId: id,
            name: attachment.name,
            mime: attachment.mime,
            data,
          })
          uploadedPaths.push(result.path)
        } catch (error) {
          throw new Error(`${t('uploadFailed')}:${attachment.name}(${error instanceof Error ? error.message : String(error)})`)
        }
      }
      await api.postPrompt(id, {
        prompt: message,
        provider: selection?.provider,
        model: selection?.model,
        reasoningEffort: selection?.reasoningEffort,
        images: pastedImages.map(({ name, mime, data }) => ({ name, mime, data })),
        files: uploadedPaths,
        quoted: trajQuotes.map(({ title, text }) => ({ title, text })),
      })
      const sentImages: UserMessageImage[] = pastedImages.map(({ mime, data }) => ({ mime, data }))
      // 收到 202:服务端已接单,立即乐观反馈(running 也由服务端 SSE 推送)。
      pushPending(message, sentImages.length > 0 ? sentImages : undefined)
      setRunningStatus(id, true)
      setPrompt('')
      setOptimizedPrompt(null)
      originalPromptRef.current = ''
      setPastedImages([])
      setAttachments([])
      setTrajQuotes([])
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
    if (optimizing) return
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
      className={`prompt-panel${inert ? ' pick-target' : ''}${optimizing ? ' optimizing' : ''}`}
      onClick={inert ? onOpenPicker : undefined}
    >
      <ApprovalDialog request={approvalReq} onDecide={handleApproval} />
      {showAttachRow && (
        <div className="composer-attachments">
          {trajQuotes.map((quote) => (
            <span className="att-chip traj-quote-chip" key={quote.id}>
              <IconBranch size={12} />
              <span className="att-name" title={quote.title}>
                {quote.title}
              </span>
              <button
                type="button"
                className="att-remove"
                title={t('removeAttachment')}
                onClick={(event) => {
                  event.stopPropagation()
                  setTrajQuotes((previous) => previous.filter((q) => q.id !== quote.id))
                }}
              >
                <IconClose size={10} />
              </button>
            </span>
          ))}
          {pastedImages.map((image, index) => (
            <div className="composer-image-thumb" key={`img-${index}`}>
              <img src={image.preview} alt="" />
              {!isGenericImageName(image.name) && (
                <span className="composer-image-label" title={image.name}>
                  {image.name}
                </span>
              )}
              <button
                type="button"
                className="composer-image-remove"
                title={t('removeAttachment')}
                onClick={(event) => {
                  event.stopPropagation()
                  setPastedImages((previous) => previous.filter((_, i) => i !== index))
                }}
              >
                <IconClose size={10} />
              </button>
            </div>
          ))}
          {attachments.map((attachment, index) => (
            <span className="att-chip" key={`att-${index}`}>
              <IconImage size={12} />
              <span className="att-name">{attachment.name}</span>
              <button
                type="button"
                className="att-remove"
                title={t('removeAttachment')}
                onClick={(event) => {
                  event.stopPropagation()
                  setAttachments((previous) => previous.filter((_, i) => i !== index))
                }}
              >
                <IconClose size={10} />
              </button>
            </span>
          ))}
        </div>
      )}
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
          readOnly={inert || optimizing}
          aria-readonly={optimizing}
          onChange={(event) => {
            setPrompt(event.target.value)
            syncPromptHeight()
          }}
          onPaste={(event) => void handlePaste(event)}
          onFocus={() => {
            if (inert) onOpenPicker()
          }}
          onKeyDown={onPromptKeyDown}
          onWheel={onPromptWheel}
        />
      </div>
      <div className="prompt-bar">
        <div className="composer-modes">
          <PermissionSelector
            value={permission}
            onChange={requestPermissionChange}
            disabled={inert || permissionBusy}
          />
          {catalog && selection && (
            <ComposerModelMenu
              catalog={catalog}
              selection={selection}
              disabled={inert || optimizing}
              onChange={applySelection}
            />
          )}
        </div>
        <div className="composer-right">
          <input
            ref={fileInputRef}
            type="file"
            multiple
            hidden
            onChange={onPickFiles}
          />
          <button
            type="button"
            className="icon-btn composer-upload-btn"
            title={t('uploadFile')}
            disabled={inert || optimizing}
            onClick={() => fileInputRef.current?.click()}
          >
            <IconPaperclip size={16} />
          </button>
          <button
            type="button"
            className={`icon-btn composer-opt-btn${optimizedPrompt ? ' undo' : ''}${optimizing ? ' busy' : ''}`}
            title={t('optimizePromptHint')}
            aria-label={optimizing ? t('optimizing') : optimizedPrompt ? t('optimizeUndo') : t('optimizePrompt')}
            disabled={
              inert ||
              optimizing ||
              running ||
              sending ||
              !activeId ||
              !selection ||
              (!optimizedPrompt && !prompt.trim())
            }
            onClick={() => {
              if (optimizedPrompt) handleUndoOptimize()
              else void handleOptimize()
            }}
          >
            {optimizing ? (
              <IconSpinner size={16} />
            ) : optimizedPrompt ? (
              <IconRewind size={16} />
            ) : (
              <IconSparkles size={16} />
            )}
          </button>
          <ContextRing pressure={context?.pressure} breakdown={context?.breakdown} />
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
              disabled={promptEmpty || sending || optimizing || inert}
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
          <div className="view-switch" role="tablist" aria-label={t('viewTrajectory')}>
            <button
              type="button"
              role="tab"
              aria-selected={view === 'chat'}
              className={`view-switch-tab${view === 'chat' ? ' active' : ''}`}
              onClick={() => setView('chat')}
            >
              {t('viewChat')}
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={view === 'trajectory'}
              className={`view-switch-tab${view === 'trajectory' ? ' active' : ''}`}
              onClick={() => setView('trajectory')}
            >
              {t('viewTrajectory')}
            </button>
          </div>
          <div className={`session-export${exportOpen ? ' open' : ''}`}>
            <button
              type="button"
              className="icon-btn session-export-btn"
              title={t('exportSessionHint')}
              aria-label={t('exportSession')}
              aria-haspopup="menu"
              aria-expanded={exportOpen}
              disabled={!activeId || exportBusy}
              onClick={() => setExportOpen((open) => !open)}
            >
              <IconDownload size={15} />
            </button>
            {exportOpen && (
              <>
                <div className="menu-backdrop" onClick={() => setExportOpen(false)} />
                <div className="session-export-menu" role="menu">
                  <button
                    type="button"
                    role="menuitem"
                    className="session-export-item"
                    onClick={() => void handleExport('markdown')}
                  >
                    <span className="session-export-item-title">{t('exportAsMarkdown')}</span>
                    <span className="session-export-item-hint">.md</span>
                  </button>
                  <button
                    type="button"
                    role="menuitem"
                    className="session-export-item"
                    onClick={() => void handleExport('json')}
                  >
                    <span className="session-export-item-title">{t('exportAsJson')}</span>
                    <span className="session-export-item-hint">.json</span>
                  </button>
                </div>
              </>
            )}
          </div>
        </header>
      )}
      <div
        className="conversation-scroll"
        ref={scrollRef}
        onScroll={handleScroll}
        data-conversation-scroll=""
      >
        {phase === 'active' && view === 'chat' && (
          <ConversationAxis nodes={transcriptNodes} scrollRef={scrollRef} />
        )}
        <div className={view === 'trajectory' ? 'conversation-view full-bleed' : 'conversation-view'}>
          {showTranscript && activeId ? (
            <SessionView
              key={`${activeId}-${transcriptReloadTick}`}
              id={activeId}
              view={view}
              pendingMessages={pendingMessages}
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
              onRewind={(seq) => void handleRewind(seq)}
              onFork={(seq) => void handleFork(seq)}
              onQuote={handleQuote}
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
        {(phase === 'hero' || view === 'chat') && (
          <div className="composer-seat" ref={seatRef} data-composer-seat="">
            <div className={`composer-stack${phase === 'hero' ? ' composer-hero' : ''}`}>
              {phase === 'hero' && workspaceRow}
              {phase === 'active' && <TodoPanel todos={todos} />}
              {composerCard}
              {phase === 'active' && (
                <StatsBar nodes={transcriptNodes} running={running} />
              )}
            </div>
          </div>
        )}
      </div>
      {phase === 'active' && view === 'chat' && !atBottom && (
        <button
          type="button"
          className="jump-bottom"
          onClick={snapToBottom}
          title={t('jumpToBottom')}
        >
          <IconChevron size={14} />
        </button>
      )}
      {fullAccessConfirm && (
        <ConfirmDialog
          open
          title={t('permissionFullConfirmTitle')}
          desc={t('permissionFullConfirmDesc')}
          confirmLabel={t('permissionFullConfirmEnable')}
          onConfirm={confirmFullAccess}
          onCancel={cancelFullAccess}
        />
      )}
      {rewindReq && (
        <ConfirmDialog
          open
          title={t('rewindConfirmTitle')}
          desc={
            rewindReq.changes.length > 0
              ? t('rewindConfirmDesc', {
                  removed: rewindReq.removedMessages,
                  files: rewindReq.changes.length,
                })
              : t('rewindConfirmSimple', { removed: rewindReq.removedMessages })
          }
          danger
          confirmLabel={t('rewind')}
          onConfirm={() => void confirmRewind()}
          onCancel={() => setRewindReq(null)}
        >
          {rewindReq.changes.length > 0 && (
            <div className="rewind-file-list">
              {rewindReq.changes.map((change) => (
                <div key={change.path} className="rewind-file-row">
                  <span className={`rewind-file-action ${change.action}`}>
                    {change.action === 'restore' ? t('rewindRestore') : t('rewindDelete')}
                  </span>
                  <code>{change.path}</code>
                </div>
              ))}
            </div>
          )}
        </ConfirmDialog>
      )}
    </div>
  )
}
