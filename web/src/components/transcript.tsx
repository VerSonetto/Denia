import { useMemo } from 'react'
import { t } from '../i18n'
import {
  assistantHasContent,
  assistantHasActions,
  groupTranscript,
  openTurnStartedAt,
  transcriptNodeKey,
  withCompacting,
  type TranscriptNode,
} from '../fold'
import type { AskAnswer, UserMessageImage } from '../types'
import { UserMessageBubble } from './UserMessageImages'
import { ViewportRow } from './ViewportRow'
import { NodeView } from './transcript/NodeView'
import { TurnOverview } from './transcript/TurnOverview'
import { WorkIndicator } from './transcript/WorkIndicator'

export function Transcript({
  nodes,
  cwd = null,
  pendingMessages = [],
  compactingAt,
  onRewind,
  onFork,
  onLoopContinue,
  onAskAnswer,
  onAskCancel,
}: {
  nodes: TranscriptNode[]
  /** 会话头 cwd:收尾产物行里相对路径的锚点。 */
  cwd?: string | null
  /** 已发送未获确认的用户消息:渲染为尾部"发送中"行。 */
  pendingMessages?: { text: string; images?: UserMessageImage[] }[]
  /**
   * 压缩进行中:传发起时刻则在尾部挂占位行,传 null/undefined 不挂。
   * 压缩结束(成功或失败)由父级置回 null,摘要作为 compaction 事件到达。
   */
  compactingAt?: number | null
  /** 用户消息点击回退按钮时回调。 */
  onRewind?: (seq: number) => void
  /** 轮次收尾消息点击分支按钮时回调(以该消息 seq 为锚点)。 */
  onFork?: (seq: number) => void
  /** 死循环提示行点「继续」:自动发送继续消息。 */
  onLoopContinue?: () => void
  /** 回答一组模型提问(`ask` 工具)。 */
  onAskAnswer?: (requestId: string, answers: AskAnswer[]) => void
  /** 取消一组模型提问。 */
  onAskCancel?: (requestId: string) => void
}) {
  // 压缩中占位行不进 nodes state:它是纯本地的瞬时状态,不该污染事件流
  // fold 的结果(否则重拉快照/切视图时会出现幽灵行)。
  const viewNodes = useMemo(
    () => (compactingAt ? withCompacting(nodes, compactingAt) : nodes),
    [nodes, compactingAt],
  )
  // 全部 hook 必须在提前 return 之前:回退把日志截断为空后走空态分支,
  // 若 hook 挂在 return 之下,同一个实例在"空态 → 有乐观行"切换时
  // hook 数量变化,React 抛 #310(Rendered more hooks than during the
  // previous render),异常冒泡到根 →整页白屏。
  const workStartedAt = useMemo(() => openTurnStartedAt(nodes), [nodes])
  // 每个已完成轮次的最后一条助手消息挂复制/分支按钮;运行中/中间 step 不显示。
  // 任意已完成轮次都可分支,不再限制"仅 transcript 尾部"。
  const turnIndex = useMemo(() => {
    const lastStep = new Map<number, number>()
    const ended = new Set<number>()
    for (const node of nodes) {
      if (node.kind === 'assistant') lastStep.set(node.turn, node.step)
      else if (node.kind === 'turn-end') ended.add(node.turn)
    }
    return { lastStep, ended }
  }, [nodes])
  const rows = useMemo(() => groupTranscript(viewNodes).flatMap((row) => {
    const showActions = row.kind === 'node' && row.node.kind === 'assistant' &&
      turnIndex.ended.has(row.node.turn) &&
      row.node.step === turnIndex.lastStep.get(row.node.turn) &&
      assistantHasActions(row.node, onFork)
    if (row.kind === 'node' && (row.node.kind === 'turn-start' ||
      (row.node.kind === 'assistant' && !assistantHasContent(row.node) && !showActions))) return []
    return [{ row, showActions }]
  }), [viewNodes, turnIndex, onFork])
  if (nodes.length === 0 && pendingMessages.length === 0 && !compactingAt) {
    return <div className="empty-hint">{t('emptyTranscript')}</div>
  }
  return (
    <>
      {rows.map(({ row, showActions }, index) => {
        const key = row.kind === 'node' ? transcriptNodeKey(row.node) :
          `overview:${transcriptNodeKey(row.hidden[0])}`
        const content = row.kind === 'node' ? (
          <NodeView
            key={key}
            node={row.node}
            cwd={cwd}
            onRewind={onRewind}
            onFork={onFork}
            onLoopContinue={onLoopContinue}
            onAskAnswer={onAskAnswer}
            onAskCancel={onAskCancel}
            showActions={showActions}
          />
        ) : (
          <TurnOverview key={key} row={row} />
        )
        // 父代理来信与用户消息一样常驻(不虚拟化),否则长会话向上翻页时
        // 主代理的任务正文会跟着概览一起消失。
        const inPlace = row.kind === 'node' &&
          (row.node.kind === 'user' ||
            (row.node.kind === 'context-injection' && row.node.fromParentAgent === true))
        return inPlace ? content : (
          <ViewportRow key={key} eager={index >= rows.length - 12}>{content}</ViewportRow>
        )
      })}
      {pendingMessages.map((message, index) => (
        <div className="user-row" key={`pending-${index}`} data-user-anchor="pending">
          <UserMessageBubble text={message.text} images={message.images} pending />
        </div>
      ))}
      {workStartedAt !== undefined && <WorkIndicator startedAt={workStartedAt} />}
    </>
  )
}
