import { memo } from 'react'
import type { TranscriptNode } from '../../fold'
import type { AskAnswer } from '../../types'
import { UserMessageBubble } from '../UserMessageImages'
import { IconSlashCommand } from '../icons'
import { AssistantNode } from './AssistantNode'
import { CompactionRow } from './CompactionRow'
import { CompactingRow } from './CompactingRow'
import { ContextInjectionRow } from './ContextInjectionRow'
import { SystemPromptRow } from './SystemPromptRow'
import { ToolRow } from './ToolRow'
import { TurnChrome } from './TurnChrome'

// 行组件 memo 化:流式帧只有引用变化的行重渲染,历史行全部跳过。
export const NodeView = memo(function NodeView({
  node,
  cwd = null,
  onRewind,
  onFork,
  onLoopContinue,
  onAskAnswer,
  onAskCancel,
  showActions = false,
}: {
  node: TranscriptNode
  /** 会话头 cwd:收尾产物行里相对路径的锚点。 */
  cwd?: string | null
  onRewind?: (seq: number) => void
  onFork?: (seq: number) => void
  /** 死循环提示行点「继续」:自动发送继续消息。 */
  onLoopContinue?: () => void
  onAskAnswer?: (requestId: string, answers: AskAnswer[]) => void
  onAskCancel?: (requestId: string) => void
  /** 已结束轮次的最后一条助手消息:显示复制/分支图标簇。 */
  showActions?: boolean
}) {
  switch (node.kind) {
    case 'user':
      return (
        <div className="user-row" data-user-anchor={node.anchor}>
          <UserMessageBubble
            text={node.text}
            images={node.images}
            seq={node.anchor}
            onRewind={onRewind}
          />
        </div>
      )
    case 'context-injection':
      return (
        <ContextInjectionRow
          text={node.text}
          images={node.images}
          fromParentAgent={node.fromParentAgent}
        />
      )
    case 'command-echo':
      // 本地斜杠命令回显(如 /goal):右对齐"指令输入"行,按事件 seq 排序。
      return (
        <div className="user-row">
          <div className="command-echo">
            <IconSlashCommand name={node.name} size={13} />
            <span>{node.text}</span>
          </div>
        </div>
      )
    case 'system-prompt':
      return <SystemPromptRow text={node.text} />
    case 'assistant':
      return <AssistantNode node={node} showActions={showActions} onFork={onFork} />
    case 'turn-start':
      // Boundary marker; only carries the turn's start time.
      return null
    case 'turn-end':
      return <TurnChrome node={node} cwd={cwd} onLoopContinue={onLoopContinue} />
    case 'tool':
      return <ToolRow node={node} onAskAnswer={onAskAnswer} onAskCancel={onAskCancel} />
    case 'compaction':
      return <CompactionRow node={node} />
    // 压缩中占位:不来自事件流,由发起压缩的前端状态挂载。
    case 'compacting':
      return <CompactingRow startedAt={node.startedAt} />
  }
})
