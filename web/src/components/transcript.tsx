import { memo, useEffect, useState } from 'react'
import type { ReactNode } from 'react'
import { t } from '../i18n'
import { groupTranscript, type OverviewRow, type TranscriptNode } from '../fold'
import {
  IconChevron,
  IconRead,
  IconTerminal,
  IconThink,
  IconTool,
  IconWrite,
} from './icons'

export function Transcript({ nodes }: { nodes: TranscriptNode[] }) {
  if (nodes.length === 0) {
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
        <div className={`msg-user${node.injected ? ' injected' : ''}`}>
          {node.text}
        </div>
      )
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
  const lastTextIndex = (() => {
    for (let i = node.blocks.length - 1; i >= 0; i--) {
      if (node.blocks[i].kind === 'text') return i
    }
    return -1
  })()
  const hasVisible =
    node.streaming ||
    node.interrupted ||
    node.blocks.some((b) => b.kind !== 'tool-call')
  if (!hasVisible) return null
  return (
    <div className="msg-assistant">
      {node.blocks.map((block, index) => {
        if (block.kind === 'reasoning') {
          return <ThinkRow key={index} text={block.text} streaming={node.streaming} />
        }
        if (block.kind === 'tool-call') {
          // Rendered by the correlated tool node below.
          return null
        }
        return (
          <div
            key={index}
            className={`prose${node.streaming && index === lastTextIndex ? ' streaming' : ''}`}
          >
            {block.text}
          </div>
        )
      })}
      {node.interrupted && <span className="badge warn">{t('interrupted')}</span>}
    </div>
  )
}

function ThinkRow({
  text,
  streaming = false,
}: {
  text: string
  /** 流式期间自动展开;输出结束后收起为摘要行。 */
  streaming?: boolean
}) {
  const [open, setOpen] = useState(streaming)
  useEffect(() => {
    if (open && !streaming) setOpen(false)
  }, [streaming])
  const summary = firstLine(text, 80)
  return (
    <div className={`disc-row${open ? ' open' : ''}`}>
      <button className="disc-head" onClick={() => setOpen(!open)}>
        <span className="glyph">
          <IconThink size={14} />
        </span>
        <span className="title">{t('thinkTitle')}</span>
        <span className="sep" />
        <span className="summary" style={{ fontFamily: 'inherit' }}>
          {summary}
        </span>
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
  const summary = running
    ? firstLine(node.args)
    : node.result!.isError
      ? firstLine(node.result!.content)
      : firstLine(node.args) || firstLine(node.result!.content)
  return (
    <div
      className={`disc-row${open ? ' open' : ''}${running ? ' running' : ''}`}
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
        <span className="chev">
          <IconChevron size={12} />
        </span>
      </button>
      {open && (
        <div className="disc-body">
          <div className="code-card">
            <div className="banner">in</div>
            <pre>{node.args}</pre>
          </div>
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
