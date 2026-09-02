import { memo, useEffect, useMemo, useState } from 'react'
import type { ReactNode } from 'react'
import { localeRevision, t } from '../i18n'
import { groupTranscript, type OverviewRow, type TranscriptNode } from '../fold'
import { MarkdownText } from '../markdown/MarkdownText'
import type { MarkdownLabels } from '../markdown/MarkdownText'
import { toolCallInput, toolCallSummary } from '../toolDisplay'
import type { UserMessageImage } from '../types'
import { UserMessageBubble } from './UserMessageImages'
import { CopyMessageButton } from './CopyMessageButton'
import {
  IconChevron,
  IconEdit,
  IconGlob,
  IconPrompt,
  IconRead,
  IconSearch,
  IconTerminal,
  IconThink,
  IconTool,
  IconWrite,
} from './icons'

export function Transcript({
  nodes,
  pendingMessages = [],
}: {
  nodes: TranscriptNode[]
  /** 已发送未获确认的用户消息:渲染为尾部"发送中"行。 */
  pendingMessages?: { text: string; images?: UserMessageImage[] }[]
}) {
  if (nodes.length === 0 && pendingMessages.length === 0) {
    return <div className="empty-hint">{t('emptyTranscript')}</div>
  }
  const rows = groupTranscript(nodes)
  return (
    <>
      {rows.map((row, index) =>
        row.kind === 'node' ? (
          <NodeView key={index} node={row.node} />
        ) : (
          <TurnOverview key={index} row={row} />
        ),
      )}
      {pendingMessages.map((message, index) => (
        <div className="user-row" key={`pending-${index}`} data-user-anchor="pending">
          <UserMessageBubble text={message.text} images={message.images} pending />
        </div>
      ))}
    </>
  )
}

/** Compact duration: 45秒 below a minute, 2分30秒 beyond, 1小时02分30秒 past an hour. */
function formatDuration(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000))
  const hours = Math.floor(total / 3600)
  const minutes = Math.floor((total % 3600) / 60)
  const seconds = total % 60
  const pad = (value: number) => String(value).padStart(2, '0')
  if (hours > 0) return t('durationHours', { h: hours, m: pad(minutes), s: pad(seconds) })
  if (minutes > 0) return t('durationMinutes', { m: minutes, s: pad(seconds) })
  return t('durationSeconds', { s: seconds })
}

function TurnOverview({ row }: { row: OverviewRow }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="process-group">
      <button className="process-head" onClick={() => setOpen((o) => !o)}>
        <span className={`chev${open ? ' open' : ''}`}>
          <IconChevron size={11} />
        </span>
        <span className="title">
          {t('processOverview', {
            duration: formatDuration(row.durationMs),
            n: row.toolCount,
          })}
        </span>
      </button>
      {open && (
        <div className="process-body">
          {row.hidden.map((node, index) => (
            <NodeView key={index} node={node} />
          ))}
        </div>
      )}
    </div>
  )
}

// 行组件 memo 化:流式帧只有引用变化的行重渲染,历史行全部跳过。
const NodeView = memo(function NodeView({ node }: { node: TranscriptNode }) {
  switch (node.kind) {
    case 'user':
      return (
        <div className="user-row" data-user-anchor={node.anchor}>
          <UserMessageBubble text={node.text} images={node.images} />
        </div>
      )
    case 'context-injection':
      return <ContextInjectionRow text={node.text} />
    case 'system-prompt':
      return <SystemPromptRow text={node.text} />
    case 'assistant':
      return <AssistantNode node={node} />
    case 'turn-start':
      // Boundary marker; only carries the turn's start time.
      return null
    case 'turn-end':
      return <TurnChrome node={node} />
    case 'tool':
      return <ToolRow node={node} />
  }
})

function AssistantNode({
  node,
}: {
  node: Extract<TranscriptNode, { kind: 'assistant' }>
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
  const hasVisible =
    node.streaming ||
    node.interrupted ||
    node.blocks.some((b) => b.kind !== 'tool-call')
  if (!hasVisible) return null
  const copyText = node.blocks
    .filter((block) => block.kind === 'text')
    .map((block) => (block.kind === 'text' ? block.text : ''))
    .join('\n\n')
  return (
    <div className="msg-assistant msg-copy-anchor">
      {node.blocks.map((block, index) => {
        if (block.kind === 'reasoning') {
          // 思考是否仍在输出:节点在流式,且该块是最后一块(正文块出现即视为
          // 思考完毕,立刻自动折叠)。
          const thinking = node.streaming && index === node.blocks.length - 1
          return <ThinkRow key={index} text={block.text} streaming={thinking} />
        }
        if (block.kind === 'tool-call') {
          // Rendered by the correlated tool node below.
          return null
        }
        return (
          <div key={index} className="prose">
            <MarkdownText text={block.text} streaming={node.streaming} labels={labels} />
          </div>
        )
      })}
      {node.interrupted && <span className="badge warn">{t('interrupted')}</span>}
      {copyText && <CopyMessageButton text={copyText} />}
    </div>
  )
}

function DisclosureRow({
  title,
  icon,
  text,
}: {
  title: string
  icon: ReactNode
  text: string
}) {
  const [open, setOpen] = useState(false)
  const summary = firstLine(text, 72)
  return (
    <div className={`disc-row inject-row${open ? ' open' : ''}`}>
      <button className="disc-head" onClick={() => setOpen(!open)}>
        <span className="glyph">{icon}</span>
        <span className="title">{title}</span>
        {!open && (
          <>
            <span className="sep" />
            <span className="summary inject-summary">{summary}</span>
          </>
        )}
      </button>
      {open && (
        <div className="disc-body">
          <div className="code-card inject-card">
            <pre>{text}</pre>
          </div>
        </div>
      )}
    </div>
  )
}

function SystemPromptRow({ text }: { text: string }) {
  return <DisclosureRow title={t('systemPromptTitle')} icon={<IconPrompt size={14} />} text={text} />
}

function ContextInjectionRow({ text }: { text: string }) {
  return (
    <DisclosureRow
      title={t('contextInjectionTitle')}
      icon={<IconTool size={14} />}
      text={text}
    />
  )
}

function ThinkRow({
  text,
  streaming = false,
}: {
  text: string
  /** 思考内容正在流式输出:强制展开;输出完毕自动折叠(手动点击随时可开合)。 */
  streaming?: boolean
}) {
  const [open, setOpen] = useState(streaming)
  // 流式中强制展开,输出完毕自动折叠 —— 跟随 streaming 状态,不依赖用户手势。
  useEffect(() => {
    setOpen(streaming)
  }, [streaming])
  const summary = firstLine(text, 80)
  return (
    <div className={`disc-row${open ? ' open' : ''}`}>
      <button
        className="disc-head"
        onClick={() => setOpen(!open)}
      >
        <span className="glyph">
          <IconThink size={14} />
        </span>
        <span className="title">{t('thinkTitle')}</span>
        {!open && (
          <>
            <span className="sep" />
            <span className="summary" style={{ fontFamily: 'inherit' }}>
              {summary}
            </span>
          </>
        )}
        <span className="chev">
          <IconChevron size={12} />
        </span>
      </button>
      {open && (
        <div className="disc-body">
          <div className="code-card">
            <pre style={{ color: 'var(--label-tertiary)' }}>{text}</pre>
          </div>
        </div>
      )}
    </div>
  )
}

function toolMeta(name: string): { title: string; icon: ReactNode } {
  switch (name) {
    case 'bash':
      return { title: 'Bash', icon: <IconTerminal size={14} /> }
    case 'read_file':
      return { title: 'Read', icon: <IconRead size={14} /> }
    case 'write_file':
      return { title: 'Write', icon: <IconWrite size={14} /> }
    case 'grep':
      return { title: 'Grep', icon: <IconSearch size={14} /> }
    case 'glob':
      return { title: 'Glob', icon: <IconGlob size={14} /> }
    case 'edit':
      return { title: 'Edit', icon: <IconEdit size={14} /> }
    default:
      return { title: name, icon: <IconTool size={14} /> }
  }
}

/** 首个非空行;indexOf 切片,不 split 整个大文本(流式每帧调用)。 */
function firstLine(text: string, max = 90): string {
  let rest = text
  for (;;) {
    const nl = rest.indexOf('\n')
    const line = (nl < 0 ? rest : rest.slice(0, nl)).trim()
    if (line.length > 0 || nl < 0) {
      return line.length > max ? `${line.slice(0, max)}…` : line
    }
    rest = rest.slice(nl + 1)
  }
}

function ToolRow({ node }: { node: Extract<TranscriptNode, { kind: 'tool' }> }) {
  const [open, setOpen] = useState(false)
  const running = !node.result
  const meta = toolMeta(node.name)
  const argSummary = toolCallSummary(node.name, node.args)
  const summary = running
    ? argSummary
    : node.result!.isError
      ? firstLine(node.result!.content)
      : argSummary || firstLine(node.result!.content)
  const inputBody = toolCallInput(node.name, node.args)
  return (
    <div
      className={`disc-row tool-row${open ? ' open' : ''}${running ? ' running' : ''}`}
    >
      <button className="disc-head" onClick={() => setOpen(!open)}>
        <span className="glyph">
          {running ? (
            <span className="dot run" />
          ) : node.result!.isError ? (
            <span className="dot err" />
          ) : (
            meta.icon
          )}
        </span>
        <span className="title">{meta.title}</span>
        <span className="sep" />
        <span
          className="summary"
          style={node.result?.isError ? { color: 'var(--error)' } : undefined}
        >
          {summary}
        </span>
      </button>
      {open && (
        <div className="disc-body">
          {inputBody && (
            <div className="code-card">
              <div className="banner">in</div>
              <pre>{inputBody}</pre>
            </div>
          )}
          {node.result && (
            <div className="code-card">
              <div className="banner">
                out
                {node.result.isError ? (
                  <span style={{ color: 'var(--error)' }}>error</span>
                ) : (
                  <span style={{ color: 'var(--success)' }}>ok</span>
                )}
              </div>
              <pre className={node.result.isError ? 'err' : ''}>
                {node.result.content}
              </pre>
            </div>
          )}
        </div>
      )}
    </div>
  )
}

function TurnChrome({
  node,
}: {
  node: Extract<TranscriptNode, { kind: 'turn-end' }>
}) {
  const { reason, usage } = node
  const label =
    reason.kind === 'completed'
      ? t('reasonCompleted')
      : reason.kind === 'aborted'
        ? t('reasonAborted')
        : reason.kind === 'max-tokens'
          ? t('reasonMaxTokens')
          : `${t('reasonError')}: [${reason.failure.code}] ${reason.failure.message}`
  const cls =
    reason.kind === 'completed' ? '' : reason.kind === 'error' ? 'err' : 'warn'
  return (
    <div className="turn-chrome">
      <span
        className={`dot ${reason.kind === 'completed' ? 'ok' : reason.kind === 'error' ? 'err' : 'run'}`}
      />
      <span className={cls}>{label}</span>
      {usage && (
        <span className="usage-pill">
          {t('inputTokens')} {usage.inputTokens} · {t('outputTokens')}{' '}
          {usage.outputTokens}
        </span>
      )}
    </div>
  )
}
