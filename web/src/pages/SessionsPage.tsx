import { RuntimePanel } from '../components/RuntimePanel'
import { useCallback, useEffect, useMemo, useRef, useState, type ClipboardEvent, type KeyboardEvent, type WheelEvent } from 'react'
import * as api from '../api'
import type { MentionCandidate } from '../api'
import type { ApprovalDecisionPayload } from '../api'
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
import { attach, ensureFollowing } from '../sessionStreams'
import { sessionDisplayTitle } from '../sessionDisplay'
import type { TranscriptNode } from '../fold'
import type { TrajectoryQuote } from '../trajectory'
import { SessionView } from '../components/SessionView'
import { StatsBar } from '../components/StatsBar'
import { ComposerModelMenu } from '../components/ComposerModelMenu'
import { PermissionSelector, loadPermission } from '../components/PermissionSelector'
import { PlanReviewPanel } from '../components/PlanReviewPanel'
import { ApprovalDialog, type ApprovalDecision, type ApprovalRequest } from '../components/ApprovalDialog'
import { ConfirmDialog } from '../components/ConfirmDialog'
import { ContextRing } from '../components/ContextRing'
import {
  IconBranch,
  IconChevron,
  IconClose,
  IconDownload,
  IconFile,
  IconFolder,
  IconImage,
  IconPlus,
  IconPaperclip,
  IconSend,
  IconSparkles,
  IconUndo,
  IconSpinner,
  IconStop,
} from '../components/icons'
import { resolveSessionReasoningEffort } from '../modelCatalog'
import { activeAtToken, applyMentionInsertion, formatFileMention } from './mention'
import { TodoPanel } from '../components/TodoPanel'
import { QueuedMessagePanel } from '../components/QueuedMessagePanel'
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
  QueuedMessage,
  UserMessageImage,
  WorkspaceRecord,
} from '../types'
import { normalizePermissionMode } from '../types'
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
/**
 * 初始模型选择(默认模型功能已删,交互改为本地记忆):
 * 1. 上一次使用的模型(localStorage,跨进程持久)——从别的会话新建会话、
 *    或完全重新进入界面时,都默认落在它上面;
 * 2. 该模型已不在目录(网关删除/模型下架)时,回退目录里第一个可用模型;
 * 3. 目录为空返回 null(选择器隐藏,发送前必须先配好模型)。
 */
function firstAvailableSelection(catalog: ModelCatalog): ModelSelection | null {
  for (const group of catalog.groups) {
    const model = group.models[0]
    if (model) return { provider: group.id, model: model.id }
  }
  return null
}

/** dsh ChatView FOLLOW_THRESHOLD */
const FOLLOW_THRESHOLD = 24

/** 乐观行内容指纹:文本 + 内联图片都参与匹配,避免多条纯图片消息("图片")互相误删。 */
function pendingMessageKey(message: { text: string; images?: UserMessageImage[] }): string {
  const images = message.images ?? []
  return `${message.text}\u0000${images.map((image) => `${image.mime}\u0000${image.data}`).join('\u0001')}`
}

/** 从事件流反向找最近一次 permission-mode(旧三档值映射到新四档)。 */
function latestPermissionMode(events: SessionEnvelope[]): PermissionMode | null {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index]
    if (event.type === 'permission-mode') return normalizePermissionMode(event.mode)
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
  // 最新 running 的 ref 镜像:异步函数(轮询等 running 变 false)读它,
  // 避免闭包捕获渲染时的旧值。
  const runningRef = useRef(running)
  runningRef.current = running

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
  // 贴底跟随状态:吸底时流式内容追加自动滚动;手动滚动打断;滚回底部恢复。
  const [stick, setStick] = useState(true)
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
  // 全会话用户消息锚点(轮次轴刻度,来自分页响应,不受窗口限制)。
  const [axisAnchors, setAxisAnchors] = useState<api.SessionAnchor[]>([])
  // 轮次轴跳转请求:点击未加载刻度时递增 nonce 触发视图翻页定位。
  const [axisJump, setAxisJump] = useState<{ seq: number; nonce: number } | null>(null)
  const [todos, setTodos] = useState<TodoItem[]>([])
  // 消息队列:AI 运行中输入的新消息,等本轮结束后自动发送。
  const [queuedMessages, setQueuedMessages] = useState<QueuedMessage[]>([])
  const queueSeqRef = useRef(0)
  // 会话内容视图(dsh conversation.view 环):对话 / 轨迹。组件按
  // activeId 重挂(key),视图状态随会话切换自然复位。
  const [view, setView] = useState<'chat' | 'trajectory'>('chat')
  // 导出菜单状态:exportOpen 控制下拉显隐,exportBusy 阻及双击。
  const [exportOpen, setExportOpen] = useState(false)
  const [exportBusy, setExportBusy] = useState(false)
  const sendingRef = useRef(false)
  const promptRef = useRef<HTMLTextAreaElement | null>(null)
  const scrollRef = useRef<HTMLDivElement | null>(null)
  const stickRef = useRef(true)
  const seatRef = useRef<HTMLDivElement | null>(null)

  const activeWs: WorkspaceRecord | null = getActiveWorkspace()

  const syncPromptHeight = () => {
    const el = promptRef.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${el.scrollHeight}px`
  }

  /* ---- 输入框 @ 文件/文件夹提及(对照 dsh file-reference:候选只含路径,内容留在 read 工具) ---- */

  const [mentionOpen, setMentionOpen] = useState(false)
  const [mentionItems, setMentionItems] = useState<MentionCandidate[]>([])
  const [mentionLoading, setMentionLoading] = useState(false)
  /** 当前查询串(空态提示区分"根目录为空"与"无匹配")。 */
  const [mentionQuery, setMentionQuery] = useState('')
  const [mentionActive, setMentionActive] = useState(0)
  const mentionMenuRef = useRef<HTMLDivElement | null>(null)
  const mentionDebounceRef = useRef<number | null>(null)
  const mentionAbortRef = useRef<AbortController | null>(null)
  // IME 组词阶段不触发检测/选择旁路(对照模型菜单的 isComposing 旁路)。
  const composingRef = useRef(false)
  // 候选范围 = 已存活会话的 cwd;无会话或 cwd 失效时不触发。
  const mentionCwd = activeSession?.cwd_alive === false ? null : (activeSession?.cwd ?? null)
  // 下钻时显示当前位置(query 含 / 时取目录前缀),对照 dsh 的面包屑头部。
  const mentionDir = mentionQuery.includes('/')
    ? mentionQuery.slice(0, mentionQuery.lastIndexOf('/') + 1)
    : ''

  const closeMention = useCallback(() => {
    if (mentionDebounceRef.current !== null) {
      window.clearTimeout(mentionDebounceRef.current)
      mentionDebounceRef.current = null
    }
    mentionAbortRef.current?.abort()
    mentionAbortRef.current = null
    setMentionOpen(false)
    setMentionItems([])
    setMentionLoading(false)
  }, [])

  /** 检测光标处 `@` token 并按 150ms 防抖拉候选;token 消失/无 cwd 时关闭。 */
  const refreshMentions = useCallback(() => {
    const el = promptRef.current
    const cwd = mentionCwd
    if (!el || !cwd) {
      closeMention()
      return
    }
    const caret = Math.min(el.selectionStart ?? el.value.length, el.value.length)
    const token = activeAtToken(el.value, caret)
    if (!token) {
      closeMention()
      return
    }
    setMentionQuery(token.query)
    setMentionOpen(true)
    setMentionLoading(true)
    if (mentionDebounceRef.current !== null) window.clearTimeout(mentionDebounceRef.current)
    mentionDebounceRef.current = window.setTimeout(() => {
      mentionDebounceRef.current = null
      mentionAbortRef.current?.abort()
      const controller = new AbortController()
      mentionAbortRef.current = controller
      api
        .searchMentions(cwd, token.query, controller.signal)
        .then(({ items }) => {
          if (controller.signal.aborted) return
          setMentionItems(items)
          setMentionActive(0)
          setMentionLoading(false)
        })
        .catch(() => {
          // 后端 400(目录已死等):按无结果收起候选,不打断输入。
          if (!controller.signal.aborted) {
            setMentionItems([])
            setMentionLoading(false)
          }
        })
    }, 150)
  }, [mentionCwd, closeMention])

  /** 选中候选:用格式化文本替换当前 token,补空格,光标移到其后。 */
  const pickMention = useCallback(
    (candidate: MentionCandidate) => {
      const el = promptRef.current
      if (!el) return
      const caret = Math.min(el.selectionStart ?? el.value.length, el.value.length)
      const token = activeAtToken(el.value, caret)
      if (!token) return
      const mention = formatFileMention(candidate, token.quoted)
      if (mention === undefined) return
      const insertion = applyMentionInsertion(el.value, caret, token, mention)
      setPrompt(insertion.text)
      setOptimizedPrompt(null)
      originalPromptRef.current = ''
      closeMention()
      syncPromptHeight()
      requestAnimationFrame(() => {
        el.focus()
        el.selectionStart = insertion.caret
        el.selectionEnd = insertion.caret
      })
    },
    [closeMention],
  )

  // 键盘高亮条目跟随滚动(与模型菜单同款 data-kb 标记)。
  useEffect(() => {
    if (!mentionOpen) return
    mentionMenuRef.current?.querySelector('[data-kb="true"]')?.scrollIntoView({ block: 'nearest' })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [mentionOpen, mentionActive, mentionItems])

  // cwd 失效/会话切换时确保菜单关闭。
  useEffect(() => {
    if (!mentionCwd) closeMention()
  }, [mentionCwd, closeMention])

  // 卸载兜底:防抖与在途请求随组件销毁取消。
  useEffect(() => {
    return () => {
      if (mentionDebounceRef.current !== null) window.clearTimeout(mentionDebounceRef.current)
      mentionAbortRef.current?.abort()
    }
  }, [])

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
    closeMention()
    setTranscriptNodes([])
    setAxisAnchors([])
    setAxisJump(null)
    setTodos([])
    setQueuedMessages([])
    queueSeqRef.current = 0
    stickRef.current = true
    setStick(true)
    if (smoothRafRef.current !== null) {
      cancelAnimationFrame(smoothRafRef.current)
      smoothRafRef.current = null
      programmaticRef.current = Math.max(0, programmaticRef.current - 1)
    }
    const el = scrollRef.current
    if (el !== null) el.scrollTop = 0
  }, [activeId, closeMention])

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
    const initial = loadLastModel(catalog) ?? firstAvailableSelection(catalog)
    if (initial) setSelection(normalizeSelection(catalog, initial))
  }, [catalog, selection])

  useEffect(() => {
    if (!catalog || !selection) return
    const valid = catalog.groups.some(
      (g) => g.id === selection.provider && g.models.some((m) => m.id === selection.model),
    )
    if (!valid) {
      const next = loadLastModel(catalog) ?? firstAvailableSelection(catalog)
      setSelection(next ? normalizeSelection(catalog, next) : null)
    }
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
  const [permission, setPermission] = useState<PermissionMode>(() => loadPermission())
  const [permissionBusy, setPermissionBusy] = useState(false)
  const [fullAccessConfirm, setFullAccessConfirm] = useState(false)
  const pendingFullAccessRef = useRef<PermissionMode | null>(null)
  const [approvalReq, setApprovalReq] = useState<ApprovalRequest | null>(null)
  const fileInputRef = useRef<HTMLInputElement | null>(null)

  /** 切换当前会话权限(活动会话直接写后端;无活动会话先记本地,首次发送时带上)。 */
  const applyPermission = useCallback(
    (level: PermissionMode) => {
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
        .setSessionPermission(activeId, level)
        .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
        .finally(() => setPermissionBusy(false))
    },
    [activeId, permission, notify],
  )

  /** 选择权限:完全访问走风险确认,其余直接写。 */
  const requestPermissionChange = useCallback(
    (level: PermissionMode) => {
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

  /* ---- 计划审批(exit_plan):替换整个输入区的审批面板 ---- */

  /** 挂起审批是 exit_plan 时,从 args_preview 解析计划正文与标题。 */
  const planReview = useMemo(() => {
    if (!approvalReq || approvalReq.toolName !== 'exit_plan') return null
    let planText = ''
    let title: string | undefined
    try {
      const parsed = JSON.parse(approvalReq.argsPreview ?? '{}') as {
        plan?: unknown
        title?: unknown
      }
      if (typeof parsed.plan === 'string') planText = parsed.plan
      if (typeof parsed.title === 'string') title = parsed.title
    } catch {
      /* 解析失败:面板会显示解析失败提示 */
    }
    return { requestId: approvalReq.requestId, planText, title }
  }, [approvalReq])

  /** 提交计划决策;批准时若换了模型,同步全局模型选择。
   *  普通函数(非 useCallback):依赖 applySelection/catalog 每渲染重建,
   *  面板本身随 approvalReq 变化重渲染,无需稳定引用。 */
  const handlePlanDecision = async (payload: ApprovalDecisionPayload) => {
    if (!planReview || !activeId) return
    if (payload.decision === 'allow-once' && payload.selection) {
      applySelection(payload.selection)
    }
    await api.answerApproval(activeId, planReview.requestId, payload)
  }

  /** 退出计划:切回自动编辑并中断轮次(挂起审批随取消结算为 cancelled)。 */
  const handlePlanExit = useCallback(() => {
    if (!activeId) return
    api
      .setSessionPermission(activeId, 'auto-edit')
      .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
    api
      .cancelSession(activeId)
      .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
  }, [activeId, notify])

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
          setPermission(normalizePermissionMode(event.mode))
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

  /* ---- 对话流贴底跟随 ----
   * 吸底时内容追加自动滚动;用户手动滚动打断;滚回底部恢复。
   * 打断判定不靠"scroll 事件里 dist 是否超阈"这一单一信号——内容增长、
   * 浏览器滚动锚定、折叠引起的布局回缩都会产生方向不明的 scroll 事件,
   * 误把跟随打断(表现:发送后界面停住不再跟随,手动滚到底才恢复)。
   * 因此:程序滚动一律打标,其触发的 scroll 事件不参与判定;
   * 用户意图单独识别(wheel 向上 = 明确离开);滚动条/触摸滚动按几何判定。 */
  const programmaticRef = useRef(0)
  const followQueuedRef = useRef(false)
  // 平滑跟随的 rAF 句柄:内容持续增长时逐帧追尾,不瞬间跳变。
  const smoothRafRef = useRef<number | null>(null)

  /** 瞬间滚到底(用户显式操作/切会话);不打平滑动画。 */
  const scrollBottomNow = useCallback(() => {
    const el = scrollRef.current
    if (el === null) return
    if (smoothRafRef.current !== null) {
      cancelAnimationFrame(smoothRafRef.current)
      smoothRafRef.current = null
      // 动画被取消:把它启动时打的 programmatic 标记还回去,
      // 否则后续用户滚动会被误屏蔽。
      programmaticRef.current = Math.max(0, programmaticRef.current - 1)
    }
    programmaticRef.current += 1
    el.scrollTop = el.scrollHeight
    // scroll 事件在本帧 rendering steps 派发,下一帧解除标记
    requestAnimationFrame(() => {
      programmaticRef.current -= 1
    })
  }, [])

  /**
   * 平滑追尾:吸底时内容增长,用指数缓动逐帧逼近底部而不是瞬跳,
   * 视觉上「吐字跟着走」的连贯感;内容每帧继续增长,目标值每帧重取,
   * 距离恒小所以永不抖动。动画期间持续打 programmatic 标记,scroll
   * 事件不参与打断判定;用户上滚(wheel)或手动拖离后 stickRef 变 false,
   * 循环自然停止。
   */
  const scrollBottomSmooth = useCallback(() => {
    const el = scrollRef.current
    if (el === null || !stickRef.current) return
    if (smoothRafRef.current !== null) return
    const step = () => {
      if (el === null || !stickRef.current) {
        smoothRafRef.current = null
        programmaticRef.current -= 1
        return
      }
      const target = el.scrollHeight
      const current = el.scrollTop
      const delta = target - current
      if (Math.abs(delta) < 0.6) {
        el.scrollTop = target
        smoothRafRef.current = null
        programmaticRef.current -= 1
        return
      }
      // 追尾缓动:差距大(整段追加)时快速逼近,差距小(逐字吐)时平滑跟随,
      // 避免"永远差一点"或"跳变"两个极端。
      const ease = delta > 480 ? 0.6 : delta > 160 ? 0.45 : 0.32
      el.scrollTop = current + delta * ease
      smoothRafRef.current = requestAnimationFrame(step)
    }
    programmaticRef.current += 1
    smoothRafRef.current = requestAnimationFrame(step)
  }, [])

  const snapToBottom = useCallback(() => {
    stickRef.current = true
    setStick(true)
    scrollBottomNow()
  }, [scrollBottomNow])

  /** 吸底时的内容跟随:rAF 合并,一帧最多推进一次平滑追尾。 */
  const scheduleFollow = useCallback(() => {
    if (!stickRef.current || followQueuedRef.current) return
    followQueuedRef.current = true
    requestAnimationFrame(() => {
      followQueuedRef.current = false
      if (!stickRef.current) return
      scrollBottomSmooth()
    })
  }, [scrollBottomSmooth])

  const handleScroll = useCallback(() => {
    if (programmaticRef.current > 0) return
    const el = scrollRef.current
    if (el === null) return
    const bottom = el.scrollHeight - el.scrollTop - el.clientHeight <= FOLLOW_THRESHOLD + 1
    if (bottom !== stickRef.current) {
      stickRef.current = bottom
      setStick(bottom)
    }
  }, [])

  const handleWheel = useCallback((event: WheelEvent<HTMLDivElement>) => {
    if (event.deltaY < 0 && stickRef.current) {
      // 用户向上滚:立即打断,内容继续追加也不再跟随
      stickRef.current = false
      setStick(false)
    }
  }, [])

  // 卸载兜底:平滑跟随动画随组件销毁停止。
  useEffect(() => {
    return () => {
      if (smoothRafRef.current !== null) {
        cancelAnimationFrame(smoothRafRef.current)
        smoothRafRef.current = null
        programmaticRef.current = Math.max(0, programmaticRef.current - 1)
      }
    }
  }, [])

  useEffect(() => {
    scheduleFollow()
  }, [running, scheduleFollow])

  // 运行中的会话才需要 SSE follow 长连接;普通浏览走分页快照,
  // 避免打开长会话就把完整历史加载进服务端 live cache。
  useEffect(() => {
    if (activeId && running) ensureFollowing(activeId)
  }, [activeId, running])

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
      scheduleFollow()
    })
    observer.observe(seat)
    observer.observe(scroller)
    return () => observer.disconnect()
  }, [scheduleFollow, phase])

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
  /**
   * 消息入队(AI 运行中):清空输入框与附件,消息进入队列,本轮结束后自动发送。
   */
  const enqueueMessage = (text: string) => {
    queueSeqRef.current += 1
    setQueuedMessages((previous) => [
      ...previous,
      { id: `queue-${queueSeqRef.current}`, text },
    ])
    setPrompt('')
    setOptimizedPrompt(null)
    originalPromptRef.current = ''
    setPastedImages([])
    setAttachments([])
    setTrajQuotes([])
  }

  /**
   * 核心发送:把指定消息发往当前会话(复用乐观行/附件/引用的完整流程)。
   * 供输入框 send 与队列自动发送共用。
   */
  const postMessage = async (text: string, options?: { clearInput?: boolean }): Promise<boolean> => {
    if (sendingRef.current) return false
    // 粘贴图片:模型必须标记为可识图,否则拒绝整条发送。
    if (pastedImages.length > 0 && !ensureVision()) return false
    // 发送 = 用户要看回复:立即恢复吸底,不等 202——服务端可能先于
    // postPrompt 响应就开始推流,此刻不吸底,后续内容全会落在视口之外。
    snapToBottom()
    sendingRef.current = true
    setSending(true)
    let ok = false
    try {
      // 从欢迎页发首条消息时,活动会话尚未创建;把当前选择的权限带到新会话。
      const wasHero = !activeId
      const id = await ensureSession(activeWs)
      if (!id) {
        onOpenPicker()
        return false
      }
      markStarted(id)
      if (wasHero && permission !== 'auto-edit') {
        await api.setSessionPermission(id, permission)
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
        prompt: text,
        provider: selection?.provider,
        model: selection?.model,
        reasoningEffort: selection?.reasoningEffort,
        images: pastedImages.map(({ name, mime, data }) => ({ name, mime, data })),
        files: uploadedPaths,
        quoted: trajQuotes.map(({ title, text }) => ({ title, text })),
      })
      // 发送成功后立即打开 follow,收流式增量;不要等 running SSE 才建连。
      ensureFollowing(id)
      const sentImages: UserMessageImage[] = pastedImages.map(({ mime, data }) => ({ mime, data }))
      // 收到 202:服务端已接单,立即乐观反馈(running 也由服务端 SSE 推送)。
      pushPending(text, sentImages.length > 0 ? sentImages : undefined)
      setRunningStatus(id, true)
      if (options?.clearInput !== false) {
        // 仅"输入框发送"清空输入区;队列自动发送不清空(用户可能正在打字)。
        setPrompt('')
        setOptimizedPrompt(null)
        originalPromptRef.current = ''
        setPastedImages([])
        setAttachments([])
        setTrajQuotes([])
      }
      ok = true
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
      // 保留输入框内容;若会话侧已经创建但发送失败,等待用户重试。
    } finally {
      sendingRef.current = false
      setSending(false)
    }
    return ok
  }

  const send = async () => {
    const trimmed = prompt.trim()
    const message =
      trimmed || (pastedImages.length > 0 ? t('pastedImageLabel') : '')
    if (optimizing || !message || sendingRef.current) return
    if (inert) {
      onOpenPicker()
      return
    }
    // AI 正在运行:不打断,把消息加入队列,本轮结束后自动发送。
    if (running) {
      if (pastedImages.length > 0 || attachments.length > 0 || trajQuotes.length > 0) {
        // 队列只承载纯文本;带图片/附件/轨迹引用时请等本轮结束后直接发送。
        notify('err', t('queueNoAttachments'))
        return
      }
      enqueueMessage(message)
      notify('ok', t('queueSentHint'))
      return
    }
    // 不运行时:附件随消息一起发。
    await postMessage(message)
    // postMessage 成功后已清空输入框/附件;发送失败时保留,等待重试。
  }

  /**
   * 轮次结束自动发送队首队列消息:
   * running 由 true → false 时,若队列非空,取出队首发送(不打断,因为已结束)。
   * 用 ref 保存最新 postMessage 与队列,避免 effect 闭包过期。
   */
  const postMessageRef = useRef(postMessage)
  postMessageRef.current = postMessage
  const queuedMessagesRef = useRef(queuedMessages)
  queuedMessagesRef.current = queuedMessages
  const prevRunningRef = useRef(running)
  // 正在"立即发送"(sendNow)时抑制自动发送:sendNow 已移除目标消息,
  // 若此时 running 变 false,effect 不能再取队首发一次(会重复发送)。
  const suppressAutoFlushRef = useRef(false)
  useEffect(() => {
    const prev = prevRunningRef.current
    prevRunningRef.current = running
    if (prev && !running && !suppressAutoFlushRef.current) {
      const next = queuedMessagesRef.current[0]
      if (next) {
        setQueuedMessages((previous) => previous.slice(1))
        void postMessageRef.current(next.text, { clearInput: false }).then((ok) => {
          if (!ok) {
            // 发送失败:放回队首,等用户处理(不丢消息)。
            setQueuedMessages((previous) => [{ ...next }, ...previous])
          }
        })
      }
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [running])

  const stop = async () => {
    if (!activeId) return
    try {
      await api.cancelSession(activeId)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }

  /* ---- 队列操作:立即发送(打断)/ 编辑(回填输入框)/ 删除 ---- */

  /** 立即发送:先打断当前 AI,再直接发送这条队列消息。 */
  const sendNow = async (message: QueuedMessage) => {
    if (!activeId) return
    suppressAutoFlushRef.current = true
    try {
      await api.cancelSession(activeId)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
    // cancel 是异步的,等 driver 退出 running 再发送(running guard 会拒绝并发)。
    const deadline = Date.now() + 5000
    while (runningRef.current && Date.now() < deadline) {
      await new Promise((resolve) => setTimeout(resolve, 150))
    }
    if (runningRef.current) {
      // 没能打断:消息保留在队列,提示用户。
      suppressAutoFlushRef.current = false
      notify('err', t('queueSendFailed'))
      return
    }
    // 打断成功:移出队列并发送(纯文本)。
    setQueuedMessages((previous) => previous.filter((item) => item.id !== message.id))
    try {
      const ok = await postMessageRef.current(message.text, { clearInput: false })
      if (!ok) {
        // 发送失败:放回队列(保持原位置),不丢消息。
        setQueuedMessages((previous) => [message, ...previous])
      }
    } finally {
      suppressAutoFlushRef.current = false
    }
  }

  /** 编辑:把消息回填到输入框并移出队列。 */
  const editQueued = (message: QueuedMessage) => {
    setPrompt(message.text)
    setOptimizedPrompt(null)
    originalPromptRef.current = ''
    syncPromptHeight()
    setQueuedMessages((previous) => previous.filter((item) => item.id !== message.id))
    promptRef.current?.focus()
  }

  /** 删除队列消息。 */
  const deleteQueued = (message: QueuedMessage) => {
    setQueuedMessages((previous) => previous.filter((item) => item.id !== message.id))
  }

  const onPromptKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (mentionOpen) {
      // IME 组词阶段的按键交给输入法,不做选择/发送旁路。
      if (composingRef.current) return
      const count = mentionItems.length
      if (event.key === 'ArrowDown' && count > 0) {
        event.preventDefault()
        setMentionActive((index) => Math.min(count - 1, index + 1))
        return
      }
      if (event.key === 'ArrowUp' && count > 0) {
        event.preventDefault()
        setMentionActive((index) => Math.max(0, index - 1))
        return
      }
      // 弹层打开时 Enter/Tab 优先选择候选,不发送。
      if ((event.key === 'Tab' || event.key === 'Enter') && count > 0) {
        event.preventDefault()
        const target = mentionItems[Math.min(mentionActive, count - 1)]
        if (target) pickMention(target)
        return
      }
      if (event.key === 'Escape') {
        event.preventDefault()
        closeMention()
        return
      }
    }
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
            if (!composingRef.current) refreshMentions()
          }}
          onSelect={() => {
            // 光标移动不产生 onChange,单独检测 @ token。
            if (!composingRef.current) refreshMentions()
          }}
          onCompositionStart={() => {
            composingRef.current = true
          }}
          onCompositionEnd={() => {
            composingRef.current = false
          }}
          onPaste={(event) => void handlePaste(event)}
          onFocus={() => {
            if (inert) onOpenPicker()
          }}
          onKeyDown={onPromptKeyDown}
          onWheel={onPromptWheel}
        />
      </div>
      {mentionOpen && (
        <>
          <div className="menu-backdrop" onClick={closeMention} />
          <div
            className="mention-menu"
            role="listbox"
            aria-label={t('mentionAria')}
            ref={mentionMenuRef}
          >
            {mentionDir && <div className="mention-menu-heading">{mentionDir}</div>}
            {mentionLoading ? (
              <div className="mention-menu-empty">
                <IconSpinner size={12} />
                {t('mentionLoading')}
              </div>
            ) : mentionItems.length === 0 ? (
              <div className="mention-menu-empty">
                {mentionQuery ? t('mentionEmpty') : t('mentionEmptyQuery')}
              </div>
            ) : (
              mentionItems.map((item, index) => {
                const active = index === mentionActive
                const directory = item.kind === 'directory'
                const slash = item.path.lastIndexOf('/')
                const name = slash >= 0 ? item.path.slice(slash + 1) : item.path
                const parent = slash >= 0 ? item.path.slice(0, slash + 1) : ''
                return (
                  <button
                    key={`${item.kind}:${item.path}`}
                    type="button"
                    role="option"
                    aria-selected={active}
                    data-kb={active || undefined}
                    className={`mention-menu-item${active ? ' kb' : ''}`}
                    onMouseEnter={() => setMentionActive(index)}
                    onClick={() => pickMention(item)}
                  >
                    <span className={`mention-menu-icon${directory ? ' dir' : ''}`}>
                      {directory ? <IconFolder size={13} /> : <IconFile size={13} />}
                    </span>
                    <span className="mention-menu-text">
                      <span className="mention-menu-name">
                        {name}
                        {directory ? '/' : ''}
                      </span>
                      {parent && <span className="mention-menu-parent">{parent}</span>}
                    </span>
                  </button>
                )
              })
            )}
          </div>
        </>
      )}
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
            title={optimizedPrompt ? t('optimizeUndo') : t('optimizePromptHint')}
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
              <IconUndo size={16} />
            ) : (
              <IconSparkles size={16} />
            )}
          </button>
          <ContextRing
            pressure={context?.pressure}
            breakdown={context?.breakdown}
            sessionId={activeId ?? undefined}
            running={running}
            onCompacted={refreshContextBreakdown}
          />
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
          {activeId && <RuntimePanel key={activeId} id={activeId} />}
        </header>
      )}
      <div
        className="conversation-scroll"
        ref={scrollRef}
        onScroll={handleScroll}
        onWheel={handleWheel}
        data-conversation-scroll=""
      >
        {phase === 'active' && view === 'chat' && (
          <ConversationAxis
            anchors={axisAnchors}
            scrollRef={scrollRef}
            onJumpMiss={(seq) => setAxisJump((prev) => ({ seq, nonce: (prev?.nonce ?? 0) + 1 }))}
          />
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
                scheduleFollow()
              }}
              onAnchorsChange={setAxisAnchors}
              jumpRequest={axisJump}
              onJumpSettled={() => setAxisJump(null)}
              onRewind={(seq) => void handleRewind(seq)}
              onFork={(seq) => void handleFork(seq)}
              onQuote={handleQuote}
              onLoopContinue={() => {
                // 死循环保护提示行的「继续」:程序化发送继续消息。
                void postMessage('继续', { clearInput: false })
              }}
              onAskAnswer={(requestId, answers) => {
                if (!activeId) return
                api
                  .answerAsk(activeId, requestId, answers)
                  .catch((error) =>
                    notify('err', error instanceof Error ? error.message : String(error)),
                  )
              }}
              onAskCancel={(requestId) => {
                if (!activeId) return
                api
                  .cancelAsk(activeId, requestId)
                  .catch((error) =>
                    notify('err', error instanceof Error ? error.message : String(error)),
                  )
              }}
            />
          ) : (
            <div className="session-hero">
              <div className="hero-headline">
                <span className="hero-headline-text">{t('heroTitle')}</span>
              </div>
            </div>
          )}
        </div>
        {(phase === 'hero' || view === 'chat') && (
          <div className="composer-seat" ref={seatRef} data-composer-seat="">
            <div className={`composer-stack${phase === 'hero' ? ' composer-hero' : ''}`}>
              {phase === 'hero' && workspaceRow}
              {phase === 'active' && (
                <QueuedMessagePanel
                  messages={queuedMessages}
                  onSendNow={(message) => void sendNow(message)}
                  onEdit={editQueued}
                  onDelete={deleteQueued}
                />
              )}
              {phase === 'active' && <TodoPanel todos={todos} />}
              {planReview ? (
                <PlanReviewPanel
                  title={planReview.title}
                  planText={planReview.planText}
                  catalog={catalog}
                  selection={selection}
                  onDecision={handlePlanDecision}
                  onExit={handlePlanExit}
                />
              ) : activeSession?.subagent ? <div className="runtime-child-composer"><span>{t('runtimeChildReadonly')}</span><button type="button" className="runtime-child-back-button" onClick={() => activeSession.parent_session && setActiveId(activeSession.parent_session, null)}>{t('runtimeBackParent')}</button></div> : composerCard}
              {phase === 'active' && (
                <StatsBar nodes={transcriptNodes} running={running} />
              )}
            </div>
          </div>
        )}
      </div>
      {phase === 'active' && view === 'chat' && !stick && (
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
