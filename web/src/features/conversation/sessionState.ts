import * as api from '../../api'
import type { ApprovalRequest } from '../../components/ApprovalDialog'
import type { TrajectoryQuote } from '../../trajectory'
import type { ModelCatalog, ModelSelection, PermissionMode, SessionEnvelope, QueuedMessage, PastedImage, PendingAttachment, UserMessageImage } from '../../types'
import { normalizePermissionMode } from '../../types'
import { resolveSessionReasoningEffort } from '../../modelCatalog'

export function normalizeSelection(catalog: ModelCatalog, selection: ModelSelection): ModelSelection {
  const group = catalog.groups.find((entry) => entry.id === selection.provider)
  const model = group?.models.find((entry) => entry.id === selection.model)
  const efforts = model?.reasoning?.efforts ?? []
  return {
    ...selection,
    reasoningEffort: resolveSessionReasoningEffort(efforts, selection.reasoningEffort),
  }
}

export const LAST_MODEL_KEY = 'denia.last-model'
// 品牌改名前的旧 key 字面量,故意保留 dsh-rs:只用于读取并搬运老用户的选择。
export const LAST_MODEL_KEY_LEGACY = 'dsh-rs.last-model'
/**
 * transcript nodes 落 state 的节流间隔(ms)。统计条等下游只需"跟得上",
 * 不需要 60Hz;真正的流式渲染发生在会话流子树内部(它自持 nodes)。
 */
export const NODES_THROTTLE_MS = 200

/** 上下文占用轮询的响应是否与上一份等价(等价则沿用旧引用,不惊动渲染)。 */
export function sameBreakdown(
  previous: api.ContextBreakdownResponse | null,
  next: api.ContextBreakdownResponse,
): boolean {
  if (previous === null) return false
  const a = previous.breakdown
  const b = next.breakdown
  const pa = previous.pressure
  const pb = next.pressure
  const ua = previous.usage
  const ub = next.usage
  return (
    a.systemTokens === b.systemTokens &&
    a.toolsTokens === b.toolsTokens &&
    a.messageTokens === b.messageTokens &&
    pa.contextWindow === pb.contextWindow &&
    pa.pressureTokens === pb.pressureTokens &&
    pa.projectedTokens === pb.projectedTokens &&
    ua.uncachedInputTokens === ub.uncachedInputTokens &&
    ua.outputTokens === ub.outputTokens &&
    ua.cacheReadTokens === ub.cacheReadTokens &&
    ua.cacheWriteTokens === ub.cacheWriteTokens &&
    ua.reasoningTokens === ub.reasoningTokens
  )
}
/**
 * 初始模型选择(默认模型功能已删,交互改为本地记忆):
 * 1. 上一次使用的模型(localStorage,跨进程持久)——从别的会话新建会话、
 *    或完全重新进入界面时,都默认落在它上面;
 * 2. 该模型已不在目录(网关删除/模型下架)时,回退目录里第一个可用模型;
 * 3. 目录为空返回 null(选择器隐藏,发送前必须先配好模型)。
 */
export function firstAvailableSelection(catalog: ModelCatalog): ModelSelection | null {
  for (const group of catalog.groups) {
    const model = group.models[0]
    if (model) return { provider: group.id, model: model.id }
  }
  return null
}

/**
 * 一条待发送消息的完整载荷。省略时取输入区当前内容(手动发送路径);
 * 队列自动发送/立即发送把当时搬进队列的内容显式传回(那时输入区可能已经
 * 在写新消息,不能取现场状态)。
 */
export interface OutgoingPayload {
  images: PastedImage[]
  attachments: PendingAttachment[]
  quotes: TrajectoryQuote[]
}

/** 把一条队列消息转回发送载荷(队列 → postMessage 的桥)。 */
export function queuedPayload(message: QueuedMessage): OutgoingPayload {
  return {
    images: message.images ?? [],
    attachments: message.attachments ?? [],
    quotes: (message.quotes ?? []) as TrajectoryQuote[],
  }
}

/** 乐观行内容指纹:文本 + 内联图片都参与匹配,避免多条纯图片消息("图片")互相误删。 */
export function pendingMessageKey(message: { text: string; images?: UserMessageImage[] }): string {
  const images = message.images ?? []
  return `${message.text}\u0000${images.map((image) => `${image.mime}\u0000${image.data}`).join('\u0001')}`
}

/** 从事件流反向找最近一次 permission-mode(旧三档值映射到新四档)。 */
export function latestPermissionMode(events: SessionEnvelope[]): PermissionMode | null {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index]
    if (event.type === 'permission-mode') return normalizePermissionMode(event.mode)
  }
  return null
}

/**
 * 设置里的"默认权限模式":无本地显式记忆时的输入框初始档位。
 * 计划模式不出现在设置里,取到也回落自动编辑(与后端校验一致)。
 */
export async function consoleDefaultPermission(): Promise<PermissionMode> {
  try {
    const describe = await api.getSettings()
    const console = describe.namespaces.find((ns) => ns.ns === 'console')
    const raw = console?.value.defaultPermissionMode
    const mode = typeof raw === 'string' ? normalizePermissionMode(raw) : 'auto-edit'
    return mode === 'plan' ? 'auto-edit' : mode
  } catch {
    return 'auto-edit'
  }
}

/** 从事件流反向找最近一次请求头,恢复该会话最后实际使用的模型。 */
export function latestRequestSelection(events: SessionEnvelope[]): ModelSelection | null {
  for (let index = events.length - 1; index >= 0; index -= 1) {
    const event = events[index]
    if (event.type === 'request-header') {
      const { provider, model, reasoningEffort } = event.header.config
      if (!provider || !model) return null
      return { provider, model, reasoningEffort }
    }
    if (event.type === 'request-context') {
      // 老日志可能只有 request-context(路由元数据),无思考强度可恢复。
      if (!event.provider || !event.model) return null
      return { provider: event.provider, model: event.model }
    }
  }
  return null
}

/** 从事件流恢复仍未结算的审批请求(approval-asked 未被对应 decided 关闭)。 */
export function latestPendingApproval(events: SessionEnvelope[]): ApprovalRequest | null {
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

