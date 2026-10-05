import { useSessionContext } from '../features/conversation/useSessionContext'
import { useComposerMentions } from '../features/conversation/useComposerMentions';
import { useComposerSlash } from '../features/conversation/useComposerSlash';
import { normalizeSelection, LAST_MODEL_KEY, LAST_MODEL_KEY_LEGACY, NODES_THROTTLE_MS, firstAvailableSelection, queuedPayload, pendingMessageKey, latestPermissionMode, consoleDefaultPermission, latestRequestSelection, latestPendingApproval, type OutgoingPayload } from '../features/conversation/sessionState';
import { RuntimePanel } from '../components/RuntimePanel';
import { useCallback, useEffect, useMemo, useRef, useState, Fragment, type ClipboardEvent, type KeyboardEvent, type WheelEvent } from 'react';
import * as api from '../api';

import type { ApprovalDecisionPayload } from '../api';
import { optimizePromptText } from '../promptOptimizer';
import { addSessionLocal, ensureSession, getActiveWorkspace, markStarted, notify, refreshList, markCompacting, setActiveId, setRunningStatus, useCatalogTick, useCompactingFor, useRunningFor, useSessions, useWorkspaces } from '../appStore';
import { t } from '../i18n';
import { attach, ensureFollowing, invalidateSession } from '../sessionStreams';
import { sessionDisplayTitle } from '../sessionDisplay';
import type { TranscriptNode } from '../fold';
import type { TrajectoryQuote } from '../trajectory';
import { SessionView } from '../components/SessionView';
import { AgentPresetSelector } from '../components/AgentPresetSelector';
import { SessionPresetLabel } from '../components/SessionPresetLabel';
import { OpenInApp } from '../components/OpenInApp';
import { ErrorBoundary } from '../components/ErrorBoundary';
import { StatsBar } from '../components/StatsBar';
import { ComposerModelMenu } from '../components/ComposerModelMenu';
import { ComposerEffortControl } from '../components/ComposerEffortControl';
import { PermissionSelector, loadPermission, hasStoredPermission } from '../components/PermissionSelector';
import { PlanReviewPanel } from '../components/PlanReviewPanel';
import { ApprovalDialog, type ApprovalDecision, type ApprovalRequest } from '../components/ApprovalDialog';
import { ConfirmDialog } from '../components/ConfirmDialog';
import { ContextRing } from '../components/ContextRing';
import { IconBranch, IconArrowDown, IconChevron, IconClose, IconDownload, IconFile, IconFolder, IconImage, IconPanelOpen, IconPlus, IconPaperclip, IconSend, IconSparkles, IconUndo, IconSpinner, IconStop, IconSlashCommand } from '../components/icons';
import { resolveSessionReasoningEffort } from '../modelCatalog';
import { useStickToBottom } from '../hooks/useStickToBottom';

import { collectSkillTokens, parseLeadingCommand } from './slash';
import { serializeEditor } from './editor';
import { REFERENCE_MIME } from '../fileTree';
import { formatTokens } from '../stats';
import { TodoPanel } from '../components/TodoPanel';
import { GoalBar } from '../components/GoalBar';
import { QueuedMessagePanel } from '../components/QueuedMessagePanel';
import { ConversationAxis } from '../components/ConversationAxis';
import { downloadFile, serializeSession, type ExportFormat } from '../lib/exportSession';
import type { AskAnswer, CatalogModel, ModelCatalog, ModelSelection, PermissionMode, SessionSummary, TodoItem, QueuedMessage, UserMessageImage, WorkspaceRecord } from '../types';
import { normalizePermissionMode } from '../types';
import { isGenericImageName } from '../userImages';
import { setModelSelection as setSharedModelSelection } from '../modelSelectionStore';

export default function SessionsPage({
  activeId,
  locked,
  hasStarted,
  onSelectWorkspace,
  onOpenPicker,
  onAddWorkspace,
  sidePaneOpen = false,
  onToggleSidePane,
}: {
  activeId: string | null
  locked: boolean
  hasStarted: boolean
  onSelectWorkspace: (ws: WorkspaceRecord) => void
  onOpenPicker: () => void
  onAddWorkspace: () => void
  /** 右侧面板是否展开(切换按钮的激活态)。 */
  sidePaneOpen?: boolean
  /** 切换右侧面板。 */
  onToggleSidePane?: () => void
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
  const catalogRef = useRef<ModelCatalog | null>(null)
  const [prompt, setPrompt] = useState('')
  const [selection, setSelection] = useState<ModelSelection | null>(null)
  const [wsMenuOpen, setWsMenuOpen] = useState(false)
  const [sending, setSending] = useState(false)
  const [optimizing, setOptimizing] = useState(false)
  const [optimizedPrompt, setOptimizedPrompt] = useState<string | null>(null)
  const originalPromptRef = useRef('')
  const optimizeAbortRef = useRef<AbortController | null>(null)
  const optimizingRef = useRef(false)
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
  /**
   * 压缩中:来自全局 store(sessionStorage 持久化),刷新后仍可恢复。
   * 早先放在组件 useState 里,一刷新就丢 —— 而后端任务其实还在跑。
   */
  const compactingAt = useCompactingFor(activeId)
  // 当前会话的 transcript 节点,供状态栏统计。
  //
  // 节流写入:流式期间 nodes 每帧都是新数组,若直接 setState,这个 2500 行的
  // 页面(连同 composer/侧栏/统计条/对话轴)会跟着每帧重渲染一次 —— 帧内的
  // React 工作量从"会话流子树"放大成"整页"。这里只在节拍上落一次 state
  // (StatsBar 这类只吃统计的下游不需要 60Hz),其余时刻由 nodesRef 承载
  // 最新值给异步逻辑(回退找节点等)读。
  const [transcriptNodes, setTranscriptNodes] = useState<TranscriptNode[]>([])
  const nodesRef = useRef<TranscriptNode[]>([])
  const nodesThrottleRef = useRef<number | null>(null)
  const applyNodes = useCallback((nodes: TranscriptNode[], flush = false) => {
    nodesRef.current = nodes
    if (flush) {
      if (nodesThrottleRef.current !== null) {
        window.clearTimeout(nodesThrottleRef.current)
        nodesThrottleRef.current = null
      }
      setTranscriptNodes(nodes)
      return
    }
    if (nodesThrottleRef.current !== null) return
    nodesThrottleRef.current = window.setTimeout(() => {
      nodesThrottleRef.current = null
      setTranscriptNodes(nodesRef.current)
    }, NODES_THROTTLE_MS)
  }, [])
  useEffect(
    () => () => {
      if (nodesThrottleRef.current !== null) window.clearTimeout(nodesThrottleRef.current)
    },
    [],
  )
  // 全会话用户消息锚点(轮次轴刻度,来自分页响应,不受窗口限制)。
  const [axisAnchors, setAxisAnchors] = useState<api.SessionAnchor[]>([])
  // 轮次轴跳转请求:点击未加载刻度时递增 nonce 触发视图翻页定位。
  const [axisJump, setAxisJump] = useState<{ seq: number; nonce: number; behavior?: ScrollBehavior } | null>(null)
  const [todos, setTodos] = useState<TodoItem[]>([])
  // 当前会话的目标视图(goal 模式);服务端权威,goal/turn-end 事件触发重拉。
  const [goalView, setGoalView] = useState<api.GoalView | null>(null)
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
  // contentEditable 输入器(原 textarea):DOM 是草稿的事实源,prompt 状态
  // 只作镜像(input 事件同步),外部写路径统一走 applyDraft 重建 DOM。
  const promptRef = useRef<HTMLDivElement | null>(null)
  const scrollRef = useRef<HTMLDivElement | null>(null)
  // 对话内容列:增长(工具卡展开/图片加载/markdown 回流)不一定伴随 nodes 变化,
  // ResizeObserver 观察它才能兜住所有内容增高路径(同 dsh columnRef)。
  const viewRef = useRef<HTMLDivElement | null>(null)
  const seatRef = useRef<HTMLDivElement | null>(null)
  // 布局根:回底按钮等多处需要读/写挂在共同祖先上的尺寸变量。
  const layoutRef = useRef<HTMLDivElement | null>(null)

  /* ---- 对话流贴底跟随 ---- *
   * 判定与驱动都在 useStickToBottom 里(见该 hook 顶部注释),这里只负责把
   * 「内容变了」与「程序性跳转」两个信号接进去。 */
  const follower = useStickToBottom(scrollRef, { active: running || compactingAt !== null })
  const stick = follower.stick
  const snapToBottom = follower.snapToBottom
  const releaseFollow = follower.release
  const resetFollow = follower.reset
  const follow = follower.follow

  const activeWs: WorkspaceRecord | null = getActiveWorkspace()

  const syncPromptHeight = useCallback(() => {
    const el = promptRef.current
    if (!el) return
    el.style.height = 'auto'
    el.style.height = `${el.scrollHeight}px`
  }, [])

  const inert = !activeId && !activeWs
  const composingRef = useRef(false)
  const onDraftChange = useCallback((text: string) => {
    setPrompt(text)
    setOptimizedPrompt(null)
    originalPromptRef.current = ''
  }, [])
  const mentionCwd = activeSession?.cwd_alive === false ? null : (activeSession?.cwd ?? null)
  const { mentionOpen, mentionItems, mentionLoading, mentionQuery, mentionActive, setMentionActive,
    mentionMenuRef, mentionDir, closeMention, refreshMentions, pickMention } = useComposerMentions({
      mentionCwd, promptRef, onDraftChange, syncPromptHeight,
    })
  const { slashOpen, slashItems, slashActive, setSlashActive, slashMenuRef, skillEntries,
    applyDraft, closeSlash, refreshMenus, pickSlash, onEditorDrop, runCompactRef } = useComposerSlash({
      activeId, inert, prompt, promptRef, setPrompt, setOptimizedPrompt, originalPromptRef,
      syncPromptHeight, closeMention, refreshMentions, composingRef,
    })

  /* ---- 从侧栏「工作区文件」树拖入的引用 ---- */

  /** 拖拽悬停:只有本应用树里拖来的引用才接受,其余(外部文本/文件)交回浏览器。 */
  const onEditorDragOver = useCallback((event: React.DragEvent<HTMLDivElement>) => {
    if (!event.dataTransfer.types.includes(REFERENCE_MIME)) return
    event.preventDefault()
    event.dataTransfer.dropEffect = 'copy'
  }, [])

  /* ---- 会话目标(goal 模式):服务端权威视图 + 事件触发的重拉 ---- */

  const refreshGoal = useCallback(
    (id: string) => {
      api
        .getGoal(id)
        .then(setGoalView)
        .catch(() => setGoalView(null))
    },
    [],
  )

  /** 裸 /goal 的查看结果(多行 toast)。 */
  const goalSummaryText = (view: api.GoalView): string => {
    const goal = view.goal
    if (!goal) return t('goalNone')
    const budget = goal.tokenBudget !== undefined
      ? `${formatTokens(view.tokensUsed)}/${formatTokens(goal.tokenBudget)}`
      : formatTokens(view.tokensUsed)
    const statusLabel =
      goal.status === 'active'
        ? t('goalPhaseActive')
        : goal.status === 'paused'
          ? t('goalPhasePaused')
          : goal.status === 'blocked'
            ? t('goalPhaseBlocked')
            : goal.status === 'budget-limited'
              ? t('goalPhaseBudgetLimited')
              : t('goalPhaseComplete')
    return t('goalSummary', {
      objective: goal.objective,
      status: statusLabel,
      budget,
      rounds: String(goal.roundsStarted),
      maxRounds: String(view.maxRounds),
    })
  }

  // 会话切换:拉取当前目标;SessionView 的 onGoalTouch(goal 事件/轮次
  // 闭合/快照)也走这里,用量与状态始终以服务端为准。
  useEffect(() => {
    if (!activeId) {
      setGoalView(null)
      return
    }
    refreshGoal(activeId)
  }, [activeId, refreshGoal])

  useEffect(() => {
    optimizeAbortRef.current?.abort()
    optimizeAbortRef.current = null
    optimizingRef.current = false
    setOptimizing(false)
    setOptimizedPrompt(null)
    originalPromptRef.current = ''
    applyDraft('')
    setSending(false)
    setPendingMessages([])
    sendingRef.current = false
    closeMention()
    nodesRef.current = []
    setTranscriptNodes([])
    setAxisAnchors([])
    setAxisJump(null)
    setTodos([])
    setQueuedMessages([])
    queueSeqRef.current = 0
    // 新会话从顶部开始且重新吸底:复位归属,不写 DOM(scrollTop 归零)。
    resetFollow()
    const el = scrollRef.current
    if (el !== null) el.scrollTop = 0
  }, [activeId, closeMention, resetFollow])

  useEffect(() => {
    let cancelled = false
    api
      .getCatalog()
      .then((next) => {
        if (!cancelled) {
          catalogRef.current = next
          setCatalog(next)
        }
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

  // 把当前模型选择同步给右侧面板的「AI 生成提交信息」(见 modelSelectionStore):
  // 它复用同一个模型,不让用户再选一次。
  useEffect(() => {
    setSharedModelSelection(selection)
  }, [selection])

  useEffect(() => {
    if (running && sending) {
      setSending(false)
    }
  }, [running, sending])

  const hasHistory = Boolean(activeSession?.excerpt)
  // 当前模型的思考强度档位:模型没提供档位时没有可调空间,控件不渲染。
  const effortLevels = useMemo(() => {
    if (!catalog || !selection) return []
    const group = catalog.groups.find((entry) => entry.id === selection.provider)
    const model = group?.models.find((entry) => entry.id === selection.model)
    return model?.reasoning?.efforts ?? []
  }, [catalog, selection])
  const activeEffort = selection
    ? resolveSessionReasoningEffort(effortLevels, selection.reasoningEffort)
    : undefined
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
  const promptEmpty =
    !prompt.trim() &&
    pastedImages.length === 0 &&
    attachments.length === 0 &&
    trajQuotes.length === 0
  const primaryStops = running && promptEmpty
  // 用户显式选过档位 → 用本地记忆;否则跟随设置里的"默认权限模式"
  // (异步拉取,见下方 default-permission 效应)。会话级档位优先,由事件流覆盖。
  const [permission, setPermission] = useState<PermissionMode>(() =>
    hasStoredPermission() ? loadPermission() : 'auto-edit',
  )
  const [permissionBusy, setPermissionBusy] = useState(false)
  const [fullAccessConfirm, setFullAccessConfirm] = useState(false)
  // agent preset(组装):名册来自服务端;当前值优先取用户在新会话页的选择,
  // 其次取会话摘要里记的组装(列表接口携带),最后落到部署默认值。
  // 会话一旦产出内容就没有改选入口——选择器只长在新会话页那一行。
  const [agentPresetCatalog, setAgentPresetCatalog] = useState<api.AgentPresetsView | null>(null)
  const [agentPresetChoice, setAgentPresetChoice] = useState<string | null>(null)
  const [agentPresetBusy, setAgentPresetBusy] = useState(false)
  const pendingFullAccessRef = useRef<PermissionMode | null>(null)
  const [approvalReq, setApprovalReq] = useState<ApprovalRequest | null>(null)
  const fileInputRef = useRef<HTMLInputElement | null>(null)

  // 设置里的默认档位只在"当前没有已加载会话 + 用户没显式选过"时生效
  // (会话级档位由 attach 的 onSnapshot/onEnvelope 覆盖,优先级更高)。
  // 保存设置后经 SSE `settings-updated`(App 侧 bumpCatalogTick)重拉,
  // 改动立刻反映到新草稿的输入框档位上。
  const noSessionRef = useRef(activeId === null)
  noSessionRef.current = activeId === null
  const loadDefaultPermission = useCallback(() => {
    if (hasStoredPermission()) return
    void consoleDefaultPermission().then((mode) => {
      // 异步落地期间若已切入/创建了会话,会话日志的档位说了算,不覆盖。
      if (!noSessionRef.current) return
      setPermission(mode)
    })
  }, [])
  useEffect(() => {
    loadDefaultPermission()
  }, [loadDefaultPermission, catalogTick])

  // preset 名册:挂载拉一次;服务端广播(复制/删除/用户改了 preset.yml)后重拉。
  const refreshAgentPresets = useCallback(() => {
    void api
      .listAgentPresets()
      .then(setAgentPresetCatalog)
      .catch(() => {
        /* 名册拉取失败不打断会话:选择器隐藏,发送与运行不受影响 */
      })
  }, [])

  useEffect(() => {
    refreshAgentPresets()
    return api.subscribeEvents((type) => {
      if (type === 'agent-presets-updated') refreshAgentPresets()
      // 默认组装写在设置里:设置一变,名册里的"默认"徽标与兜底值同步。
      if (type === 'settings-updated') refreshAgentPresets()
    })
  }, [refreshAgentPresets])

  /** 新会话页显示与提交用的组装 id:本地选择 > 会话摘要 > 部署默认值。 */
  const agentPresetId =
    agentPresetChoice ?? activeSession?.agent_preset ?? agentPresetCatalog?.default ?? null

  /**
   * 切换当前会话的组装(只有还没产出内容的会话可切;服务端拒绝时原样报错)。
   * 与权限同一套路数:会话已存在就直接写进它的日志(刷新后仍在),还没建会话
   * 就先记本地,首条消息发出时随会话创建一起应用。
   */
  const changeAgentPreset = useCallback(
    (id: string) => {
      setAgentPresetChoice(id)
      if (!activeId) return
      setAgentPresetBusy(true)
      void api
        .setSessionAgentPreset(activeId, id)
        .catch((error) => {
          setAgentPresetChoice(null)
          notify('err', error instanceof Error ? error.message : String(error))
        })
        .finally(() => setAgentPresetBusy(false))
    },
    [activeId, notify],
  )

  // 换会话时本地选择归零:重新跟随那个会话自己记录的组装。
  useEffect(() => {
    setAgentPresetChoice(null)
  }, [activeId])

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
  // 模型选择恢复:切换会话时,把编辑器上的模型切到该会话日志里最后一次
  // 实际使用的模型(request-header 快照),而不是沿用上一会话/localStorage
  // 的记忆;只在切换后的第一次快照恢复,之后的重放快照不覆盖手动选择。
  const restoredSelectionRef = useRef<string | null>(null)
  useEffect(() => {
    if (!activeId) {
      setApprovalReq(null)
      return
    }
    restoredSelectionRef.current = null
    const unsubscribe = attach(activeId, {
      onSnapshot: (_header, events) => {
        const mode = latestPermissionMode(events)
        if (mode) setPermission(mode)
        setApprovalReq(latestPendingApproval(events))
        if (restoredSelectionRef.current === null) {
          const logged = latestRequestSelection(events)
          const current = catalogRef.current
          // 目录未就绪时不消耗恢复机会,等后续自愈快照重试。
          if (current) {
            restoredSelectionRef.current = activeId
            if (
              logged &&
              current.groups.some(
                (g) => g.id === logged.provider && g.models.some((m) => m.id === logged.model),
              )
            ) {
              setSelection(normalizeSelection(current, logged))
            }
          }
        }
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
    async (event: ClipboardEvent<HTMLElement>) => {
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

  /** 编辑器粘贴:图片走识图流程,其余一律降级纯文本(保持节点结构纪律)。 */
  const onEditorPaste = (event: ClipboardEvent<HTMLDivElement>) => {
    const items = event.clipboardData?.items
    if (
      items &&
      Array.from(items).some((item) => item.kind === 'file' && item.type.startsWith('image/'))
    ) {
      void handlePaste(event)
      return
    }
    event.preventDefault()
    const text = event.clipboardData?.getData('text/plain') ?? ''
    if (text) document.execCommand('insertText', false, text)
  }

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

  /* ---- 对话流贴底跟随 ---- *
   * 判断与驱动都在 useStickToBottom 里(见该 hook 顶部注释),这里只负责
   * 把「内容变了」与「程序性跳转」两个信号接进去。 */

  // 回合启动推一把:首块内容到达前先落到底,后续由 onNodesChange 接管。
  useEffect(() => {
    follow()
  }, [running, follow])

  // 运行中的会话才需要 SSE follow 长连接;普通浏览走分页快照,
  // 避免打开长会话就把完整历史加载进服务端 live cache。
  useEffect(() => {
    if (activeId && running) ensureFollowing(activeId)
  }, [activeId, running])

  useEffect(() => {
    syncPromptHeight()
  }, [prompt, phase])

  /* ---- 输入框自动聚焦 ----
   * 进入欢迎态/切换会话后,只要输入框可编辑就自动获得焦点,用户不必先点一下
   * 才能打字。三类情况不抢:输入框不可编辑(inert/优化中)、有弹窗在等用户
   * 决策(审批/计划评审/回退与全权确认)、用户当前焦点在别的输入控件上
   * (搜索框、浏览器面板的地址栏等)。视图切回对话也算一次"进入",一并聚焦。 */
  useEffect(() => {
    if (inert || optimizing) return
    if (approvalReq || rewindReq || fullAccessConfirm || planReview) return
    const el = promptRef.current
    if (el === null) return
    const active = document.activeElement
    if (active === el) return
    // 已有别的可输入控件握着焦点(搜索框/地址栏/弹窗输入):不抢。
    if (
      active instanceof HTMLElement &&
      (active.isContentEditable ||
        active.tagName === 'INPUT' ||
        active.tagName === 'TEXTAREA' ||
        active.tagName === 'SELECT')
    ) {
      return
    }
    el.focus({ preventScroll: true })
  }, [
    activeId,
    phase,
    view,
    inert,
    optimizing,
    approvalReq,
    rewindReq,
    fullAccessConfirm,
    planReview,
  ])

  // dsh: composer seat / 内容列 / 滚动口尺寸变化时,贴底读者保持视野。
  // 内容列观察兜住不伴随 nodes 变化的增高(展开工具卡、图片加载等)。
  //
  // 依赖必须带上 view:轨迹视图下输入区整体卸载,切回对话时拿到的是**另一个**
  // DOM 节点。只依赖 phase 时 observer 会一直盯着已卸载的旧节点,而它对被
  // 移除的元素仍会补发一次回调 —— 那次读到 offsetHeight 为 0,把
  // --composer-height 写成 0px;新节点又从未被 observe,高度再也量不回来。
  // 回底按钮靠这个变量定位,0px 会让它退到容器底部、压在状态栏上。
  useEffect(() => {
    const scroller = scrollRef.current
    const view = viewRef.current
    const layout = layoutRef.current
    if (scroller === null || view === null || layout === null) return
    const measure = () => {
      // 输入区高度要写到布局根上:回底按钮是滚动容器的兄弟节点,只有挂在
      // 共同祖先(session-layout)才继承得到 —— 挂在滚动容器上时按钮拿不到
      // 真实高度,只能用兜底值定位,于是被 sticky 输入区盖住,既看不见也
      // 点不到。滚动容器那份保留:对话轴与滚动口内部还按它算。
      const seat = seatRef.current
      // 输入区不在(轨迹视图)或节点已脱离文档时不写:对已卸载的节点读
      // offsetHeight 得 0,会把按钮按到底部。保留上一次的真实高度即可 ——
      // 这两个消费者此刻本来也不渲染。
      if (seat !== null && seat.isConnected) {
        const height = `${seat.offsetHeight}px`
        layout.style.setProperty('--composer-height', height)
        scroller.style.setProperty('--composer-height', height)
      }
      scroller.style.setProperty('--conversation-viewport-height', `${scroller.clientHeight}px`)
      follow()
    }
    const observer = new ResizeObserver(() => measure())
    const seat = seatRef.current
    if (seat !== null) observer.observe(seat)
    observer.observe(scroller)
    observer.observe(view)
    // 立即量一次:observe() 的首次回调要等下一帧,而切回对话视图时变量
    // 可能还留着上一轮的 0px,按钮会在那一帧里错位。
    measure()
    return () => observer.disconnect()
  }, [phase, view])

  /**
   * 乐观行协调:
   * - push:发送 202 后注入乐观行。
   * - settle:流中已出现同文本 user-message(可能先于 push,
   *   因为 202 返回前服务端已落库并推送)→ 从列表移除并记录确认。
   * 确认记录带短 TTL(settle 先到时 push 不再注入):快照重放会把
   * 历史消息也 settle 一遍,永久计数会让之后"同文本重发"(回退后
   * 常见)的乐观行被误吞——TTL 过期即失效,只有紧邻 202 的竞态窗口
   * (2 秒)内才生效,这正是它要保护的范围。
   */
  const CONFIRM_TTL_MS = 2000
  const confirmedRef = useRef(new Map<string, number>())

  const settlePending = useCallback((message: { text: string; images?: UserMessageImage[] }) => {
    const key = pendingMessageKey(message)
    const now = Date.now()
    // 惰性清理:快照重放会写入大量历史 key,过期项在读路径顺手删。
    if (confirmedRef.current.size > 64) {
      for (const [k, expiresAt] of confirmedRef.current) {
        if (expiresAt <= now) confirmedRef.current.delete(k)
      }
    }
    confirmedRef.current.set(key, now + CONFIRM_TTL_MS)
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
    const node = nodesRef.current.find(
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

  /* ---- 传给 SessionView 的回调:引用必须稳定 ----
   * 这些回调一路透传到 transcript 的 NodeView(memo 化)。若在 JSX 里写内联
   * 箭头,每次页面渲染都是新引用,浅比较必然失败 —— 所有历史行都会跟着
   * 重渲染,memo 形同虚设。 */

  const handleNodesChange = useCallback(
    (nodes: TranscriptNode[]) => {
      applyNodes(nodes)
      follow()
    },
    [applyNodes, follow],
  )

  const handleJumpSettled = useCallback(() => setAxisJump(null), [])

  const handleRewindClick = useCallback(
    (seq: number) => {
      void handleRewind(seq)
    },
    [handleRewind],
  )

  const handleForkClick = useCallback(
    (seq: number) => {
      void handleFork(seq)
    },
    [handleFork],
  )

  const handleLoopContinue = useCallback(() => {
    // 死循环保护提示行的「继续」:程序化发送继续消息。
    void postMessageRef.current('继续', { clearInput: false })
  }, [])

  const handleAskAnswer = useCallback(
    (requestId: string, answers: AskAnswer[]) => {
      if (!activeId) return
      api
        .answerAsk(activeId, requestId, answers)
        .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
    },
    [activeId, notify],
  )

  const handleAskCancel = useCallback(
    (requestId: string) => {
      if (!activeId) return
      api
        .cancelAsk(activeId, requestId)
        .catch((error) => notify('err', error instanceof Error ? error.message : String(error)))
    },
    [activeId, notify],
  )

  const handleGoalTouch = useCallback(() => {
    if (activeId) refreshGoal(activeId)
  }, [activeId, refreshGoal])

  const handleNotFound = useCallback(() => {
    // 会话已被删除(其他窗口/流 404):清空视图。
    setActiveId(null, null)
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
        console.error('[export] 会话导出失败', error)
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
      applyDraft(optimized)
      setOptimizedPrompt(optimized)
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
    applyDraft(originalPromptRef.current)
  }

  const confirmRewind = useCallback(async () => {
    if (!activeId || !rewindReq || rewindBusy) return
    setRewindBusy(true)
    // 回退期间抑制队列自动发送:上面的打断会让 running 转 false,若不抑制,
    // 队首消息会在日志即将被截断的瞬间被发出去。
    suppressAutoFlushRef.current = true
    try {
      // 运行中也能回退:先替用户打断当前轮次并等它真正退出。不再要求
      // 先手动暂停 —— 但打断必须发生在用户确认之后(取消确认框时 AI
      // 照旧跑完,不因一次误点丢掉正在产出的回答)。
      //
      // cancel 幂等(没有运行中的轮次时服务端直接返回),所以不先判前端
      // running:另一个标签页发起的轮次也能被这一步收住。
      try {
        await api.cancelSession(activeId)
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      }
      // cancel 是异步的:等 driver 退出 running 再回退(running 期间
      // 后端拒绝回退,且截断日志会与正在写入的流互相踩)。
      const deadline = Date.now() + 5000
      while (runningRef.current && Date.now() < deadline) {
        await new Promise((resolve) => setTimeout(resolve, 150))
      }
      if (runningRef.current) {
        notify('err', t('rewindStopFailed'))
        return
      }
      const result = await api.rewindSession(activeId, rewindReq.seq)
      // 后端物理截断后事件 seq 重新编号,本页流引擎的 cursor 与 follow
      // 连接已失真:显式重连对齐(重拉快照校正 cursor、按需重开 follow),
      // 不依赖 SessionView 重挂载这类间接效应——否则快照竞态失败或另一
      // 标签页回退时,新事件帧会整段落在旧 cursor 之下被静默吞掉。
      invalidateSession(activeId)
      // 恢复目标消息文本/图片到输入框,方便修改后重发。
      applyDraft(result.toMessage ?? rewindReq.text)
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
      // 时间线已截断:乐观行、确认记录与排队消息一并作废。
      confirmedRef.current.clear()
      setPendingMessages([])
      setQueuedMessages([])
      setTranscriptNodes([])
      // 强制重挂载 SessionView,重新拉截断后的快照。
      setTranscriptReloadTick((tick) => tick + 1)
      notify('ok', t('rewindDone'))
      setRewindReq(null)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      suppressAutoFlushRef.current = false
      setRewindBusy(false)
    }
  }, [activeId, rewindReq, rewindBusy])

  const { context } = useSessionContext(activeId)

  const showAttachRow =
    pastedImages.length > 0 || attachments.length > 0 || trajQuotes.length > 0

  const pushPending = useCallback((text: string, images?: UserMessageImage[]) => {
    const message = { text, images }
    const key = pendingMessageKey(message)
    const expiresAt = confirmedRef.current.get(key)
    if (expiresAt !== undefined) {
      // 确认记录一次性消耗:202 返回前服务端已落库推送(同文本已出现在
      // 流中)→ 不再注入乐观行。TTL 之外(历史快照的 settle)不算数。
      confirmedRef.current.delete(key)
      if (Date.now() < expiresAt) return
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
   * 消息入队(AI 运行中):输入区的全部内容(文本 + 图片 + 附件 + 轨迹引用)
   * 整体搬进队列,输入区随之下空,本轮结束后自动发送。
   */
  const enqueueMessage = (text: string) => {
    queueSeqRef.current += 1
    setQueuedMessages((previous) => [
      ...previous,
      {
        id: `queue-${queueSeqRef.current}`,
        text,
        images: pastedImages.length > 0 ? pastedImages : undefined,
        attachments: attachments.length > 0 ? attachments : undefined,
        quotes: trajQuotes.length > 0 ? trajQuotes : undefined,
      },
    ])
    applyDraft('')
    setPastedImages([])
    setAttachments([])
    setTrajQuotes([])
  }

  /** 清空输入区(文本/优化缓存/图片/附件/轨迹引用),手动发送与命令消费共用。 */
  const clearComposerState = () => {
    applyDraft('')
    setPastedImages([])
    setAttachments([])
    setTrajQuotes([])
  }

  /**
   * 手动压缩上下文:对话流尾部挂"正在压缩"占位行,结束(成功/无可压缩/
   * 失败)一律撤掉 —— 成功时真正的摘要会作为 compaction 事件到达并就地
   * 出现在原位,占位行不抢它的位置。
   *
   * 后端是一次 await、没有中间进度事件,所以这里只表达"进行中";
   * 不画假进度条 —— 不知道百分比就不该装作知道。
   */
  const compactingRef = useRef(false)
  /**
   * 发起压缩:后端 202 接单后立即返回,不 await 结果。
   *
   * 压缩现在与发轮次同构(spawn 到后台),刷新页面不再打断它;这里只负责
   * 打上"正在压缩"标记,收尾由三处驱动:成功 → compaction-summary 事件
   * 到达;无可压缩/失败 → `compaction-failed` 广播;页面刷新 → 标记从
   * sessionStorage 恢复,再由 running 快照判定是否已结束。
   */
  const runCompact = async (id: string): Promise<void> => {
    if (compactingRef.current) return
    compactingRef.current = true
    try {
      await api.compactSession(id)
      markCompacting(id, Date.now())
    } catch (error) {
      notify('err', error instanceof Error ? error.message : t('contextCompactFailed'))
    } finally {
      compactingRef.current = false
    }
  }
  runCompactRef.current = runCompact

  /**
   * 核心发送:把指定消息发往当前会话(复用乐观行/附件/引用的完整流程)。
   * 供输入框 send 与队列自动发送共用。
   */
  const postMessage = async (
    text: string,
    options?: { clearInput?: boolean; payload?: OutgoingPayload },
  ): Promise<boolean> => {
    if (sendingRef.current) return false
    // 载荷:显式传入优先(队列消息),否则取输入区当前内容。
    const outgoingImages = options?.payload?.images ?? pastedImages
    const outgoingAttachments = options?.payload?.attachments ?? attachments
    const outgoingQuotes = options?.payload?.quotes ?? trajQuotes
    // 正文兜底:后端拒绝空 prompt,而纯图片/纯附件/纯引用的消息本身合法。
    // 只在确实无正文时造一句占位文案,用户写过的字一律原样保留。
    const outgoingText =
      text.trim() ||
      (outgoingImages.length > 0
        ? t('pastedImageLabel')
        : outgoingAttachments.length > 0
          ? outgoingAttachments[0].name
          : outgoingQuotes.length > 0
            ? outgoingQuotes[0].title
            : text)
    // 粘贴图片:模型必须标记为可识图,否则拒绝整条发送。
    if (outgoingImages.length > 0 && !ensureVision()) return false
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
      // 欢迎页选过组装而那时还没有会话:现在补上,与权限同一处应用。
      if (wasHero && agentPresetChoice) {
        await api.setSessionAgentPreset(id, agentPresetChoice)
      }
      // 斜杠命令裁定(单一发送咽喉:手动发送/队列自动发送/立即发送共用);
      // 命令优先于技能(对齐 dsh matchEnter 的行认领)。
      const command = parseLeadingCommand(text)
      // 命令裁定看原文(占位文案不该被当成命令);实际发往模型的正文用
      // outgoingText —— 无正文时它已经兜底成图片/附件/引用的可读摘要。
      let body = outgoingText
      if (command?.kind === 'plan') {
        if (permission !== 'plan') {
          await api.setSessionPermission(id, 'plan')
          setPermission('plan')
          try {
            window.localStorage.setItem('denia.permission', 'plan')
          } catch {
            /* storage unavailable */
          }
          notify('ok', t('slashPlanOn'))
        } else {
          notify('ok', t('slashPlanAlready'))
        }
        const rest = (command.rest ?? '').trim()
        if (!rest) {
          // 裸 /plan:只切模式,不发消息;手动发送时清空输入区(队列刷新不清)。
          if (options?.clearInput !== false) clearComposerState()
          return true
        }
        body = rest
      } else if (command?.kind === 'compact') {
        if ((command.rest ?? '').trim()) throw new Error(t('slashCompactNoArgs'))
        await runCompact(id)
        if (options?.clearInput !== false) clearComposerState()
        return true // 裸 /compact:直接压缩,不发消息
      } else if (command?.kind === 'goal') {
        // /goal [<objective>|edit <objective>|pause|resume|clear](对照 dsh /goal 命令语法)
        // 命令原文随请求落 command-run 事件:回显按真实 seq 排进对话流。
        const rest = (command.rest ?? '').trim()
        const sub = rest.split(/\s+/u, 1)[0] ?? ''
        try {
          if (!rest) {
            // 裸 /goal:查看当前目标(本地只读,无副作用不落事件)。
            const view = await api.getGoal(id)
            notify('ok', view.goal ? goalSummaryText(view) : t('goalNone'))
          } else if (sub === 'pause' || sub === 'resume' || sub === 'clear') {
            if (rest !== sub) throw new Error(t('slashGoalTrailing'))
            await api.goalAction(id, { action: sub }, selection ?? undefined, text)
            notify('ok', sub === 'pause' ? t('goalPaused') : sub === 'resume' ? t('goalResumed') : t('goalCleared'))
            refreshGoal(id)
          } else if (sub === 'edit') {
            const objective = rest.slice('edit'.length).trim()
            if (!objective) throw new Error(t('slashGoalNeedObjective'))
            await api.goalAction(id, { action: 'edit', objective }, selection ?? undefined, text)
            notify('ok', t('goalEdited'))
            refreshGoal(id)
          } else {
            // 其余文本整体作为新目标(set;complete 目标直接覆盖重开)。
            await api.goalAction(id, { action: 'set', objective: rest }, selection ?? undefined, text)
            notify('ok', t('goalCreated'))
            refreshGoal(id)
          }
        } catch (error) {
          notify('err', error instanceof Error ? error.message : String(error))
        }
        if (options?.clearInput !== false) clearComposerState()
        return true // /goal 系列都是本地命令,不发消息
      }
      // 技能直调:首行裸 /name 由后端 skill_gesture 注入(collectSkillTokens
      // 已剔除该场景防双重注入);其余命中 user-invocable 词典的 token 收进
      // skills 数组,由后端以「用户显式调用技能」注入。
      const skillNames = collectSkillTokens(
        body,
        skillEntries.map((skill) => skill.name),
      )
      // 附件上传(不限格式):先持久化,再随消息注入路径。
      const uploadedPaths: string[] = []
      for (const attachment of outgoingAttachments) {
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
        prompt: body,
        provider: selection?.provider,
        model: selection?.model,
        reasoningEffort: selection?.reasoningEffort,
        images: outgoingImages.map(({ name, mime, data }) => ({ name, mime, data })),
        files: uploadedPaths,
        quoted: outgoingQuotes.map(({ title, text }) => ({ title, text })),
        skills: skillNames.length > 0 ? skillNames : undefined,
      })
      // 发送成功后立即打开 follow,收流式增量;不要等 running SSE 才建连。
      ensureFollowing(id)
      const sentImages: UserMessageImage[] = outgoingImages.map(({ mime, data }) => ({ mime, data }))
      // 收到 202:服务端已接单,立即乐观反馈(running 也由服务端 SSE 推送)。
      pushPending(body, sentImages.length > 0 ? sentImages : undefined)
      setRunningStatus(id, true)
      if (options?.clearInput !== false) {
        // 仅"输入框发送"清空输入区;队列自动发送不清空(用户可能正在打字)。
        clearComposerState()
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
    // 可发判定:正文、图片、附件、轨迹引用任一非空即可发(空正文由
    // postMessage 在发往后端前兜底成一句话,队列里保留用户真实输入)。
    const hasPayload =
      pastedImages.length > 0 || attachments.length > 0 || trajQuotes.length > 0
    if (optimizing || (!trimmed && !hasPayload) || sendingRef.current) return
    if (inert) {
      onOpenPicker()
      return
    }
    // AI 正在运行:不打断,把消息加入队列(含图片/附件/引用),本轮结束后自动发送。
    if (running) {
      enqueueMessage(trimmed)
      notify('ok', t('queueSentHint'))
      return
    }
    // 不运行时:附件随消息一起发。
    await postMessage(trimmed)
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
        void postMessageRef
          .current(next.text, { clearInput: false, payload: queuedPayload(next) })
          .then((ok) => {
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
    // 打断成功:移出队列并发送(携带入队时的图片/附件/引用)。
    setQueuedMessages((previous) => previous.filter((item) => item.id !== message.id))
    try {
      const ok = await postMessageRef.current(message.text, {
        clearInput: false,
        payload: queuedPayload(message),
      })
      if (!ok) {
        // 发送失败:放回队列(保持原位置),不丢消息。
        setQueuedMessages((previous) => [message, ...previous])
      }
    } finally {
      suppressAutoFlushRef.current = false
    }
  }

  /** 编辑:把消息(含图片/附件/引用)回填到输入框并移出队列(卡片随词典重建)。 */
  const editQueued = (message: QueuedMessage) => {
    applyDraft(message.text)
    // 回填是「整体覆盖」:队列消息就是当时输入区的全量快照,直接恢复。
    setPastedImages(message.images ?? [])
    setAttachments(message.attachments ?? [])
    setTrajQuotes((message.quotes ?? []) as TrajectoryQuote[])
    setQueuedMessages((previous) => previous.filter((item) => item.id !== message.id))
    promptRef.current?.focus()
  }

  /** 删除队列消息。 */
  const deleteQueued = (message: QueuedMessage) => {
    setQueuedMessages((previous) => previous.filter((item) => item.id !== message.id))
  }

  const onPromptKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
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
    if (slashOpen) {
      // IME 组词阶段的按键交给输入法,不做选择/发送旁路。
      if (composingRef.current) return
      const count = slashItems.length
      if (event.key === 'ArrowDown' && count > 0) {
        event.preventDefault()
        setSlashActive((index) => Math.min(count - 1, index + 1))
        return
      }
      if (event.key === 'ArrowUp' && count > 0) {
        event.preventDefault()
        setSlashActive((index) => Math.max(0, index - 1))
        return
      }
      // 弹层打开时 Enter/Tab 优先选择候选,不发送。
      if ((event.key === 'Tab' || event.key === 'Enter') && count > 0) {
        event.preventDefault()
        const target = slashItems[Math.min(slashActive, count - 1)]
        if (target) pickSlash(target)
        return
      }
      if (event.key === 'Escape') {
        event.preventDefault()
        closeSlash()
        return
      }
    }
    // 卡片原子性(contenteditable=false)由浏览器原生保证:光标进不去、
    // Backspace/Delete/选择删除均整卡处理,无需任何拦截。
    if (composingRef.current || event.nativeEvent.isComposing) return
    if (event.key !== 'Enter' || event.shiftKey) return
    event.preventDefault()
    if (optimizing) return
    if (primaryStops) {
      void stop()
      return
    }
    if (!sending) void send()
  }

  const onPromptWheel = (event: WheelEvent<HTMLDivElement>) => {
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
      {/* 模式选择关闭时新会话固定用部署默认组装,选择器整个不渲染。 */}
      {agentPresetCatalog && agentPresetCatalog.modeSelection && agentPresetId && (
        <AgentPresetSelector
          value={agentPresetId}
          presets={agentPresetCatalog.presets}
          defaultId={agentPresetCatalog.default}
          disabled={inert || agentPresetBusy}
          onChange={changeAgentPreset}
        />
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
        <div
          ref={promptRef}
          className="prompt-editor"
          contentEditable={!inert && !optimizing}
          suppressContentEditableWarning
          role="textbox"
          aria-multiline="true"
          aria-label={inert ? t('heroChooseWorkspace') : t('chatPlaceholder')}
          aria-readonly={inert || optimizing}
          data-placeholder={
            inert
              ? t('placeholderWorkspace')
              : phase === 'hero'
                ? t('placeholderHero')
                : t('chatPlaceholder')
          }
          spellCheck={false}
          onInput={() => {
            // DOM 是草稿事实源:同步 prompt 镜像 + 触发检测(IME 组词期间
            // 只同步状态,不刷新弹层)。Chrome 删空后常残留 <br>,清掉让
            // :empty 占位符回归。
            const el = promptRef.current
            if (!el) return
            const text = serializeEditor(el)
            if (!composingRef.current && !text && el.childNodes.length > 0) el.replaceChildren()
            setPrompt(text)
            syncPromptHeight()
            if (!composingRef.current) refreshMenus()
          }}
          onCompositionStart={() => {
            composingRef.current = true
          }}
          onCompositionEnd={() => {
            composingRef.current = false
            const el = promptRef.current
            if (el) setPrompt(serializeEditor(el))
            refreshMenus()
          }}
          onPaste={onEditorPaste}
          onDragOver={onEditorDragOver}
          onDrop={onEditorDrop}
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
                    onMouseDown={(mouseEvent) => {
                      // 阻止菜单夺焦:编辑器必须保住光标/选区,execCommand 才能落点。
                      mouseEvent.preventDefault()
                    }}
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
      {slashOpen && (
        <>
          <div className="menu-backdrop" onClick={closeSlash} />
          <div
            className="mention-menu"
            role="listbox"
            aria-label={t('slashAria')}
            ref={slashMenuRef}
          >
            {slashItems.length === 0 ? (
              <div className="mention-menu-empty">{t('slashEmpty')}</div>
            ) : (
              slashItems.map((item, index) => {
                const active = index === slashActive
                const header =
                  index === 0 || slashItems[index - 1].kind !== item.kind ? (
                    <div className="slash-menu-group" role="presentation">
                      {item.kind === 'command' ? t('slashGroupCommands') : t('slashGroupSkills')}
                    </div>
                  ) : null
                return (
                  <Fragment key={`${item.kind}:${item.name}`}>
                    {header}
                    <button
                      type="button"
                      role="option"
                      aria-selected={active}
                      data-kb={active || undefined}
                      className={`mention-menu-item${active ? ' kb' : ''}`}
                      onMouseDown={(mouseEvent) => {
                        // 阻止菜单夺焦:编辑器必须保住光标/选区,execCommand 才能落点。
                        mouseEvent.preventDefault()
                      }}
                      onMouseEnter={() => setSlashActive(index)}
                      onClick={() => pickSlash(item)}
                    >
                      <span className="mention-menu-icon">
                        {item.kind === 'command' ? (
                          <IconSlashCommand name={item.name} size={13} />
                        ) : (
                          /* 技能:统一四角星,不按名字区分。 */
                          <IconSparkles size={13} />
                        )}
                      </span>
                      <span className="mention-menu-text">
                        <span className="mention-menu-name-row">
                          <span className="mention-menu-name">/{item.name}</span>
                          {item.userOnly && (
                            <span className="slash-badge">{t('slashBadgeUserOnly')}</span>
                          )}
                        </span>
                        <span className="slash-menu-desc">{item.description}</span>
                      </span>
                    </button>
                  </Fragment>
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
          {selection && (
            <div className="composer-model-group">
              {catalog && selection && (
                <ComposerModelMenu
                  catalog={catalog}
                  selection={selection}
                  disabled={inert || optimizing}
                  onChange={applySelection}
                />
              )}
              {selection && activeEffort && effortLevels.length > 0 && (
                <ComposerEffortControl
                  key={`${activeId}:${selection.provider}:${selection.model}`}
                  efforts={effortLevels}
                  value={activeEffort}
                  disabled={inert || optimizing}
                  onChange={(effort) => applySelection({ ...selection, reasoningEffort: effort })}
                />
              )}
            </div>
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
            aria-label={t('uploadFile')}
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
          />
          {primaryStops ? (
            <button
              type="button"
              className="btn-send stop"
              onClick={() => void stop()}
              title={t('stop')}
              aria-label={t('stop')}
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
              aria-label={t('run')}
            >
              <IconSend size={16} />
            </button>
          )}
        </div>
      </div>
    </div>
  )

  return (
    <div className="session-layout" data-phase={phase} ref={layoutRef}>
      {phase === 'active' && (
        <header className="session-header">
          {/* 标题块:标题与组装标签同处一块,标签紧贴标题文字。
              这样头部动作区一个元素都不新增,标签也不会飘到最右。 */}
          <div className="session-title-block">
            <h1 className="session-title">
              {activeSession ? sessionDisplayTitle(activeSession) : t('blankSession')}
            </h1>
            {activeSession && agentPresetId && (
              <SessionPresetLabel
                presetId={agentPresetId}
                presets={agentPresetCatalog?.presets ?? []}
              />
            )}
          </div>
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
          <OpenInApp
            cwd={activeSession?.cwd_alive === false ? null : activeSession?.cwd ?? null}
          />
          {/* 右侧面板开关:与 ZCode 的工作区头部按钮同位置(最右、紧邻导出)。 */}
          {onToggleSidePane && (
            <button
              type="button"
              className={`icon-btn session-pane-btn${sidePaneOpen ? ' active' : ''}`}
              title={t('sidePaneToggle')}
              aria-label={sidePaneOpen ? t('sidePaneCollapse') : t('sidePaneExpand')}
              aria-pressed={sidePaneOpen}
              data-testid="side-pane-toggle"
              onClick={onToggleSidePane}
            >
              <IconPanelOpen size={15} />
            </button>
          )}
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
        ref={follower.setNode}
        data-conversation-scroll=""
      >
        {phase === 'active' && view === 'chat' && (
          <ConversationAxis
            anchors={axisAnchors}
            nodes={transcriptNodes}
            scrollRef={scrollRef}
            onJump={releaseFollow}
            onJumpMiss={(seq, behavior) => {
              // 锚点在分页窗口外:先脱跟,翻页补齐后 SessionView 再定位,
              // 否则每来一页内容都会被心跳拉回底部。
              releaseFollow()
              setAxisJump((prev) => ({
                seq,
                nonce: (prev?.nonce ?? 0) + 1,
                behavior,
              }))
            }}
          />
        )}
        <div
          className={view === 'trajectory' ? 'conversation-view full-bleed' : 'conversation-view'}
          ref={viewRef}
        >
          {showTranscript && activeId ? (
            // 局部边界:transcript 子树崩了不该带走侧栏/输入框(只降级这一块)。
            // key 与会话/重挂 tick 对齐,回退重挂时边界一并重置。
            <ErrorBoundary key={`${activeId}-${transcriptReloadTick}`}>
              <SessionView
                key={`${activeId}-${transcriptReloadTick}`}
                id={activeId}
                view={view}
                pendingMessages={pendingMessages}
                compacting={compactingAt}
                onTodosChange={setTodos}
                onGoalTouch={handleGoalTouch}
                onPendingSettled={settlePending}
                onNotFound={handleNotFound}
                onNodesChange={handleNodesChange}
                onAnchorsChange={setAxisAnchors}
                jumpRequest={axisJump}
                onJumpSettled={handleJumpSettled}
                onRewind={handleRewindClick}
                onFork={handleForkClick}
                onQuote={handleQuote}
                onLoopContinue={handleLoopContinue}
                onAskAnswer={handleAskAnswer}
                onAskCancel={handleAskCancel}
              />
            </ErrorBoundary>
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
              {phase === 'active' && activeId && !activeSession?.subagent && (
                <GoalBar
                  sessionId={activeId}
                  view={goalView}
                  selection={selection ?? undefined}
                  onChanged={() => refreshGoal(activeId)}
                />
              )}
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
              {phase === 'active' && <StatsBar nodes={transcriptNodes} />}
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
          aria-label={t('jumpToBottom')}
        >
          <IconArrowDown size={16} />
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
            (rewindReq.changes.length > 0
              ? t('rewindConfirmDesc', {
                  removed: rewindReq.removedMessages,
                  files: rewindReq.changes.length,
                })
              : t('rewindConfirmSimple', { removed: rewindReq.removedMessages })) +
            // 运行中回退会先替用户打断本轮:这件事必须在确认前说清楚。
            (running ? ` ${t('rewindConfirmInterrupt')}` : '')
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
