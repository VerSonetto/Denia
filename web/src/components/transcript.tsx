import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import type { ReactNode } from 'react'
import { localeRevision, t } from '../i18n'
import { groupTranscript, type OverviewRow, type TranscriptNode } from '../fold'
import { MarkdownText } from '../markdown/MarkdownText'
import type { MarkdownLabels } from '../markdown/MarkdownText'
import { useTypewriter } from '../typewriter'
import { toolCallInput, toolCallSummary } from '../toolDisplay'
import type { UserMessageImage } from '../types'
import { UserMessageBubble } from './UserMessageImages'
import { BranchMessageButton, CopyMessageButton } from './CopyMessageButton'
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
  onRewind,
  onFork,
  onLoopContinue,
}: {
  nodes: TranscriptNode[]
  /** 已发送未获确认的用户消息:渲染为尾部"发送中"行。 */
  pendingMessages?: { text: string; images?: UserMessageImage[] }[]
  /** 用户消息点击回退按钮时回调。 */
  onRewind?: (seq: number) => void
  /** 轮次收尾消息点击分支按钮时回调(以该消息 seq 为锚点)。 */
  onFork?: (seq: number) => void
  /** 死循环提示行点「继续」:自动发送继续消息。 */
  onLoopContinue?: () => void
}) {
  if (nodes.length === 0 && pendingMessages.length === 0) {
    return <div className="empty-hint">{t('emptyTranscript')}</div>
  }
  const rows = groupTranscript(nodes)
  // 每个已完成轮次的最后一条助手消息挂复制/分支按钮;运行中/中间 step 不显示。
  // dsh 语义:任意已完成轮次都可分支,不再限制"仅 transcript 尾部"。
  const lastAssistantStep = new Map<number, number>()
  const endedTurns = new Set<number>()
  for (const node of nodes) {
    if (node.kind === 'assistant') lastAssistantStep.set(node.turn, node.step)
    else if (node.kind === 'turn-end') endedTurns.add(node.turn)
  }
  return (
    <>
      {rows.map((row, index) =>
        row.kind === 'node' ? (
          <NodeView
            key={index}
            node={row.node}
            onRewind={onRewind}
            onFork={onFork}
            onLoopContinue={onLoopContinue}
            showActions={
              row.node.kind === 'assistant' &&
              endedTurns.has(row.node.turn) &&
              row.node.step === lastAssistantStep.get(row.node.turn)
            }
          />
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
export function formatDuration(ms: number): string {
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
const NodeView = memo(function NodeView({
  node,
  onRewind,
  onFork,
  onLoopContinue,
  showActions = false,
}: {
  node: TranscriptNode
  onRewind?: (seq: number) => void
  onFork?: (seq: number) => void
  /** 死循环提示行点「继续」:自动发送继续消息。 */
  onLoopContinue?: () => void
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
      return <ContextInjectionRow text={node.text} />
    case 'system-prompt':
      return <SystemPromptRow text={node.text} />
    case 'assistant':
      return <AssistantNode node={node} showActions={showActions} onFork={onFork} />
    case 'turn-start':
      // Boundary marker; only carries the turn's start time.
      return null
    case 'turn-end':
      return <TurnChrome node={node} onLoopContinue={onLoopContinue} />
    case 'tool':
      return <ToolRow node={node} />
    case 'compaction':
      return <CompactionRow node={node} />
  }
})

/**
 * LLM 总结压缩的边界标记(学 Claude Code compact boundary):显示摘要、
 * 被压缩的事件区间与 token 前后对比。摘要内部折叠,点击展开。
 */
function CompactionRow({ node }: { node: Extract<TranscriptNode, { kind: 'compaction' }> }) {
  const [open, setOpen] = useState(false)
  const pre = node.preTokens ?? 0
  const post = node.postTokens ?? 0
  const saved = pre > post ? pre - post : 0
  // Hook 必须常驻组件顶层:放在条件 JSX 内会在展开/收起时改变 hook 数量,
  // 触发 React #310(Rendered more hooks than during the previous render)。
  const labels = useMemo<MarkdownLabels>(
    () => ({
      code: { copyLabel: t('copy'), copiedLabel: t('copied') },
      footnotes: t('footnotes'),
    }),
    [localeRevision()],
  )
  return (
    <div className="compaction-row">
      <button
        type="button"
        className="compaction-header"
        onClick={() => setOpen((v) => !v)}
        title={`replaces ${node.replacesFrom}..${node.replacesTo}, keep ${node.keepFrom}`}
      >
        <span className="badge">{t('contextCompacted')}</span>
        <span className="compaction-meta">
          {pre > 0 && post > 0
            ? t('contextCompactionTokens', { pre, post, saved })
            : t('contextCompactionRange', {
                from: node.replacesFrom,
                to: node.replacesTo,
              })}
        </span>
        <span className="chevron">{open ? '▾' : '▸'}</span>
      </button>
      {open && (
        <div className="compaction-body prose">
          <MarkdownText text={node.summary} streaming={false} labels={labels} />
        </div>
      )}
    </div>
  )
}

function AssistantNode({
  node,
  showActions,
  onFork,
}: {
  node: Extract<TranscriptNode, { kind: 'assistant' }>
  /** 已结束轮次的最后一条助手消息显示图标簇(dsh 语义:每个已完成轮次都可分支)。 */
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
  const hasVisible =
    node.streaming ||
    node.interrupted ||
    node.blocks.some((b) => b.kind !== 'tool-call')
  if (!hasVisible && !showActions) return null
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
          <div key={index} className={`prose${node.streaming ? ' streaming-block' : ''}`}>
            <MarkdownText text={block.text} streaming={node.streaming} labels={labels} />
          </div>
        )
      })}
      {node.interrupted && <span className="badge warn">{t('interrupted')}</span>}
      {showActions && (
        <div className="message-actions always-visible">
          {copyText && <CopyMessageButton text={copyText} />}
          {onFork && node.seq !== undefined && (
            <BranchMessageButton onBranch={() => onFork(node.seq!)} />
          )}
        </div>
      )}
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

/** 注入消息的 UI 投影:对话流上"老老实实显示是什么"——识别通道、给出诚实
 * 标题并剥掉给模型看的 `<system-reminder>` 框架与通道头行;未识别的文本保持
 * 原样。模型可见历史不受影响:框架只在后端日志里,前端只做展示投影。 */
const REMINDER_OPEN = '<system-reminder>\n'
const REMINDER_CLOSE = '</system-reminder>'

function injectionDisplay(text: string): { title: string; body: string } {
  // 子代理/后台任务通知沿用首行原样展示。
  if (
    text.startsWith('[子代理') ||
    text.startsWith('[代理 ') ||
    text.startsWith('[后台任务')
  ) {
    return { title: text.split('\n')[0], body: text }
  }
  if (text.startsWith('[技能正文 ')) {
    const head = text.split('\n')[0] ?? ''
    const name = head.slice('[技能正文 '.length).split('（')[0]?.trim() || '…'
    return {
      title: t('injectionSkillBody', { name }),
      body: text.slice(head.length + 1),
    }
  }
  if (text.startsWith('[denia 能力上下文]')) {
    return {
      title: t('injectionCapability'),
      body: text.split('\n').slice(1).join('\n'),
    }
  }
  if (text.startsWith('Current runtime context.')) {
    return {
      title: t('injectionRuntimeSnapshot'),
      body: text.split('\n').slice(1).join('\n'),
    }
  }
  if (text.startsWith(REMINDER_OPEN)) {
    const inner = text.endsWith(REMINDER_CLOSE)
      ? text.slice(REMINDER_OPEN.length, -REMINDER_CLOSE.length - 1)
      : text.slice(REMINDER_OPEN.length)
    const nl = inner.indexOf('\n')
    const head = nl < 0 ? inner : inner.slice(0, nl)
    const body = nl < 0 ? '' : inner.slice(nl + 1).replace(/^\n+/, '')
    if (head.startsWith('工作区指令:')) {
      return {
        title: t(head.includes('取代') ? 'injectionWorkspaceUpdate' : 'injectionWorkspace'),
        body,
      }
    }
    if (head.startsWith('技能目录:')) {
      // <available_skills> 帧是给模型的;对话流里直接展示技能行列表。
      const lines = body
        .replace('<available_skills>\n', '')
        .replace('\n</available_skills>', '')
      return {
        title: t(head.includes('取代') ? 'injectionSkillCatalogUpdate' : 'injectionSkillCatalog'),
        body: lines,
      }
    }
    return { title: t('contextInjectionTitle'), body: inner }
  }
  return { title: t('contextInjectionTitle'), body: text }
}

function ContextInjectionRow({ text }: { text: string }) {
  const display = injectionDisplay(text)
  return <DisclosureRow title={display.title} icon={<IconTool size={14} />} text={display.body} />
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
  // 内部滚动跟随:思考内容长在 240px 内部滚动框里,外层对话容器不撑高。
  // 流式期间框内吸底(新行出现即滚到最新);在框内手动向上滚即打断,
  // 滚回底部自动恢复跟随 —— 语义与外层对话流的贴底跟随一致。
  const preRef = useRef<HTMLPreElement | null>(null)
  const pinnedRef = useRef(true)
  const handleInnerScroll = useCallback(() => {
    const el = preRef.current
    if (el === null) return
    pinnedRef.current = el.scrollHeight - el.scrollTop - el.clientHeight <= 24
  }, [])
  useEffect(() => {
    if (!streaming || !open) return
    // 重新展开 = 全新视角,默认吸到最新;用户随后仍可在框内滚开打断。
    pinnedRef.current = true
    let raf = 0
    let lastHeight = -1
    const tick = () => {
      const el = preRef.current
      if (el !== null && el.scrollHeight !== lastHeight) {
        lastHeight = el.scrollHeight
        if (pinnedRef.current) el.scrollTop = el.scrollHeight
      }
      raf = requestAnimationFrame(tick)
    }
    raf = requestAnimationFrame(tick)
    return () => cancelAnimationFrame(raf)
  }, [streaming, open])
  // 思考框正文:流式时逐字 reveal(打字机),settle 立即全量。
  const displayText = useTypewriter(text, streaming)
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
            {/* 跟随滚动发生在 240px 内部框自己身上,外层不撑高。 */}
            <pre
              ref={preRef}
              className={streaming ? 'think-streaming' : undefined}
              onScroll={handleInnerScroll}
              style={{ color: 'var(--label-tertiary)' }}
            >
              {displayText}
            </pre>
          </div>
        </div>
      )}
    </div>
  )
}

function toolMeta(name: string): { title: string; icon: ReactNode } {
  switch (name) {
    case 'spawn_agent':
    case 'fork_agent': return { title: t('runtimeAgents'), icon: <IconTool size={14} /> }
    case 'send_message': return { title: t('runtimeMessage'), icon: <IconTool size={14} /> }
    case 'interrupt_agent':
    case 'job_kill': return { title: t('runtimeInterrupt'), icon: <IconTool size={14} /> }
    case 'list_agents':
    case 'wait_agent': return { title: t('runtimeAgents'), icon: <IconTool size={14} /> }
    case 'job_start':
    case 'job_list': return { title: t('runtimeJobs'), icon: <IconTool size={14} /> }
    case 'job_output': return { title: t('runtimeOutput'), icon: <IconTool size={14} /> }
    case 'skill': return { title: t('runtimeSkills'), icon: <IconTool size={14} /> }

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

/** skill load 的 SKILL.md 正文随工具结果 JSON 返回;展开时按 markdown 渲染
 * 而不是裸 JSON。解析失败或非 load 结果(list/resource)返回 null 走原 pre。 */
function skillLoadBody(name: string, content: string, isError: boolean): string | null {
  if (isError || name !== 'skill') return null
  try {
    const value = JSON.parse(content) as { body?: unknown }
    return typeof value.body === 'string' && value.body.trim() ? value.body : null
  } catch {
    return null
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
  // Hook 常驻组件顶层:条件 JSX 内挂 hook 会在展开/收起时改变 hook 数量。
  const labels = useMemo<MarkdownLabels>(
    () => ({
      code: { copyLabel: t('copy'), copiedLabel: t('copied') },
      footnotes: t('footnotes'),
    }),
    [localeRevision()],
  )
  const skillBody = useMemo(
    () => (node.result ? skillLoadBody(node.name, node.result.content, node.result.isError) : null),
    [node.name, node.result],
  )
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
              {skillBody ? (
                <div className="prose tool-skill-body">
                  <MarkdownText text={skillBody} streaming={false} labels={labels} />
                </div>
              ) : (
                <pre className={node.result.isError ? 'err' : ''}>
                  {node.result.content}
                </pre>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  )
}

function TurnChrome({
  node,
  onLoopContinue,
}: {
  node: Extract<TranscriptNode, { kind: 'turn-end' }>
  /** 死循环提示行点「继续」:自动发送继续消息。 */
  onLoopContinue?: () => void
}) {
  const { reason, usage } = node
  const label =
    reason.kind === 'completed'
      ? t('reasonCompleted')
      : reason.kind === 'aborted'
        ? t('reasonAborted')
        : reason.kind === 'max-tokens'
          ? t('reasonMaxTokens')
          : reason.kind === 'interrupted'
            ? t('reasonInterrupted')
            : reason.kind === 'loop-detected'
              ? t('loopDetectedHint')
              : `${t('reasonError')}: [${reason.failure?.code ?? 'unknown'}] ${reason.failure?.message ?? ''}`
  const cls =
    reason.kind === 'completed' ? '' : reason.kind === 'error' ? 'err' : 'warn'
  return (
    <div className="turn-chrome">
      <span
        className={`dot ${reason.kind === 'completed' ? 'ok' : reason.kind === 'error' ? 'err' : 'run'}`}
      />
      {reason.kind === 'loop-detected' ? (
        // 死循环保护提示行:「继续」为下划线可点文字,点击自动发送继续消息。
        <span className="reason loop-hint">
          {t('loopDetectedHint')}
          <button
            type="button"
            className="loop-continue"
            onClick={onLoopContinue}
          >
            {t('loopDetectedContinue')}
          </button>
        </span>
      ) : (
        <span className={`reason ${cls}`} title={label}>
          {label}
        </span>
      )}
      {usage && (
        <span className="usage-pill">
          {t('inputTokens')} {usage.inputTokens} · {t('outputTokens')}{' '}
          {usage.outputTokens}
        </span>
      )}
    </div>
  )
}
