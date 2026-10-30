import { useMemo } from 'react'
import { localeRevision, t } from '../../i18n'
import { MarkdownText } from '../../markdown/MarkdownText'
import type { MarkdownLabels } from '../../markdown/MarkdownText'
import { assistantHasContent, type TranscriptNode } from '../../fold'
import { BranchMessageButton, CopyMessageButton } from '../CopyMessageButton'
import { ThinkRow } from './ThinkRow'

export function AssistantNode({
  node,
  showActions,
  onFork,
}: {
  node: Extract<TranscriptNode, { kind: 'assistant' }>
  /** 已结束轮次的最后一条助手消息显示图标簇(每个已完成轮次都可分支)。 */
  showActions: boolean
  onFork?: (seq: number) => void
}) {
  // markdown chrome 文案:locale 变化时重建(引用变化让流式渲染缓存失效);
  // locale 稳定时保持同一引用,流式缓存在 chunk 间存活。
  const labels = useMemo<MarkdownLabels>(
    () => ({
      code: { copyLabel: t('copy'), copiedLabel: t('copied') },
      footnotes: t('footnotes'),
    }),
    [localeRevision()],
  )
  if (!assistantHasContent(node) && !showActions) return null
  const copyText = node.blocks
    .filter((block) => block.kind === 'text')
    .map((block) => (block.kind === 'text' ? block.text : ''))
    .join('\n\n')
  return (
    <div className="msg-assistant msg-copy-anchor">
      {node.blocks.map((block, index) => {
        if (block.kind === 'tool-call' || block.text.trim().length === 0) return null
        if (block.kind === 'reasoning') {
          // 思考是否仍在输出:节点在流式,且该块是最后一块(正文块出现即视为
          // 思考完毕,立刻自动折叠)。
          const thinking = node.streaming && index === node.blocks.length - 1
          return <ThinkRow key={index} text={block.text} streaming={thinking} />
        }
        return (
          <div key={index} className={`prose${node.streaming ? ' streaming-block' : ''}`}>
            <MarkdownText text={block.text} streaming={node.streaming} labels={labels} />
          </div>
        )
      })}
      {node.interrupted && <span className="badge warn">{t('interrupted')}</span>}
      {showActions && (
        <div className="message-actions always-visible">
          {copyText.trim() && <CopyMessageButton text={copyText} />}
          {onFork && node.seq !== undefined && (
            <BranchMessageButton onBranch={() => onFork(node.seq!)} />
          )}
        </div>
      )}
    </div>
  )
}
