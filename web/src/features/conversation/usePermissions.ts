import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import * as api from '../../api';
import type { ApprovalDecisionPayload } from '../../api';
import { notify } from '../../appStore';
import { attach } from '../../sessionStreams';
import { consoleDefaultPermission, latestPendingApproval, latestPermissionMode } from './sessionState';
import type { ApprovalDecision, ApprovalRequest } from '../../components/ApprovalDialog';
import { hasStoredPermission, loadPermission } from '../../components/PermissionSelector';
import { normalizePermissionMode, type PermissionMode, type ModelSelection, type SessionSummary } from '../../types';

/** 权限档位与审批:本地三态、后端写入口、审批弹窗与计划审批的完整簇。 */
export function usePermissions({
  activeId,
  activeSession,
  catalogTick,
  applySelection,
}: {
  activeId: string | null
  activeSession: SessionSummary | null
  catalogTick: number
  applySelection: (next: ModelSelection) => void
}) {
  // 用户显式选过档位 → 用本地记忆;否则跟随设置里的"默认权限模式"
  // (异步拉取,见下方 default-permission 效应)。会话级档位优先,由事件流覆盖。
  const [permission, setPermission] = useState<PermissionMode>(() =>
    hasStoredPermission() ? loadPermission() : 'auto-edit',
  )
  const [permissionBusy, setPermissionBusy] = useState(false)
  const [fullAccessConfirm, setFullAccessConfirm] = useState(false)
  const pendingFullAccessRef = useRef<PermissionMode | null>(null)
  const [approvalReq, setApprovalReq] = useState<ApprovalRequest | null>(null)

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
    [activeId, permission],
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
    [approvalReq, activeId],
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
  }, [activeId])

  // 跟随会话事件流:同步服务端权限模式,并监听后端发起的审批请求。
  // (模型选择恢复与累计统计的另一半监听留在页面组件里,见 SessionsPage。)
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

  // agent preset(预设):名册来自服务端;当前值优先取用户在新会话页的选择,
  // 其次取会话摘要里记的预设(列表接口携带),最后落到部署默认值。
  // 会话一旦产出内容就没有改选入口——选择器只长在新会话页那一行。
  const [agentPresetCatalog, setAgentPresetCatalog] = useState<api.AgentPresetsView | null>(null)
  const [agentPresetChoice, setAgentPresetChoice] = useState<string | null>(null)
  const [agentPresetBusy, setAgentPresetBusy] = useState(false)

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
      // 默认预设写在设置里:设置一变,名册里的"默认"徽标与兜底值同步。
      if (type === 'settings-updated') refreshAgentPresets()
    })
  }, [refreshAgentPresets])

  /** 新会话页显示与提交用的预设 id:本地选择 > 会话摘要 > 部署默认值。 */
  const agentPresetId =
    agentPresetChoice ?? activeSession?.agent_preset ?? agentPresetCatalog?.default ?? null

  /**
   * 切换当前会话的预设(只有还没产出内容的会话可切;服务端拒绝时原样报错)。
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
    [activeId],
  )

  // 换会话时本地选择归零:重新跟随那个会话自己记录的预设。
  useEffect(() => {
    setAgentPresetChoice(null)
  }, [activeId])

  return {
    permission,
    setPermission,
    permissionBusy,
    fullAccessConfirm,
    confirmFullAccess,
    cancelFullAccess,
    approvalReq,
    setApprovalReq,
    applyPermission,
    requestPermissionChange,
    handleApproval,
    planReview,
    handlePlanDecision,
    handlePlanExit,
    agentPresetCatalog,
    agentPresetChoice,
    agentPresetBusy,
    agentPresetId,
    changeAgentPreset,
  }
}
