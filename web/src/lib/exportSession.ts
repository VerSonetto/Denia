/**
 * 会话导出:把 events 序列化为 Markdown / JSON,触发浏览器下载。
 *
 * 设计目标:Markdown 和 JSON 等详 —— 每条事件都不能丢,审查场景里
 * 任何一条 `permission-mode` 或 `tool-result` 都可能是事故定性的关键。
 * Markdown 用人类可读形式铺开;JSON 直接 `stringify`,供机器消费 / 工具
 * 进一步处理(grep / jq / 重新生成 fixture)。
 */

import type { ContentBlock, SessionEnvelope, SessionHeader, StreamChunk, TokenUsage, UserMessageImage } from '../types'

export type ExportFormat = 'markdown' | 'json'

interface ExportInput {
  header: SessionHeader
  events: SessionEnvelope[]
  title: string
}

interface Serialized {
  content: string
  filename: string
  mimeType: string
}

const TEXT_HEAD_MAX = 96

function safeText(value: string | undefined | null): string {
  return value ?? ''
}

function formatTime(ms: number | undefined): string {
  if (!ms) return ''
  const date = new Date(ms)
  const pad = (n: number) => n.toString().padStart(2, '0')
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
  )
}

function formatTimeMs(ms: number | undefined): string {
  if (!ms) return ''
  const date = new Date(ms)
  const millis = date.getMilliseconds().toString().padStart(3, '0')
  return `${formatTime(ms)}.${millis}`
}

function trimHead(value: string, max = TEXT_HEAD_MAX): string {
  const oneLine = value.replace(/\s+/g, ' ').trim()
  return oneLine.length > max ? `${oneLine.slice(0, max)}…` : oneLine
}

function usageParts(usage: TokenUsage | undefined): string[] {
  if (!usage) return []
  const parts: string[] = []
  if (usage.inputTokens) parts.push(`输入 ${usage.inputTokens}`)
  if (usage.outputTokens) parts.push(`输出 ${usage.outputTokens}`)
  if (usage.cacheReadTokens) parts.push(`缓存读 ${usage.cacheReadTokens}`)
  if (usage.reasoningTokens) parts.push(`推理 ${usage.reasoningTokens}`)
  return parts
}

function blockSummary(block: ContentBlock): string {
  switch (block.type) {
    case 'text':
      return `text(${block.text.length} 字)`
    case 'reasoning':
      return `reasoning(${block.text.length} 字)`
    case 'tool-call':
      return `tool-call(${block.name}#${block.id})`
  }
}

function describeChunk(chunk: StreamChunk): string {
  switch (chunk.type) {
    case 'block-start':
      return `block-start(${chunk.block_type})`
    case 'text-delta':
      return `text-delta(${chunk.text.length} 字)`
    case 'reasoning-delta':
      return `reasoning-delta(${chunk.text.length} 字)`
    case 'tool-call-delta':
      return `tool-call-delta(${chunk.id}, name=${chunk.name ?? '-'})`
    case 'block-end':
      return `block-end(${blockSummary(chunk.block)})`
    case 'usage':
      return `usage(${usageParts(chunk.usage).join(' · ') || '空'})`
    case 'finish':
      return `finish(${chunk.reason.kind})`
  }
}

function tryFormatJson(value: string): string {
  if (!value) return '`(空)`'
  try {
    return '```json\n' + JSON.stringify(JSON.parse(value), null, 2) + '\n```'
  } catch {
    return '```\n' + value + '\n```'
  }
}

function imageBullets(images: UserMessageImage[]): string {
  if (images.length === 0) return ''
  return `\n- 附图 ${images.length} 张(均为 base64 内联,导出不含图像字节)\n`
}

function fenceSafe(value: string): string {
  // 三引号围栏避免内含 ``` 时破坏 markdown。
  return value.replace(/```/g, 'ʼʼʼ')
}

function headLine(envelope: SessionEnvelope): string {
  const t = envelope.type
  switch (t) {
    case 'turn-start':
      return `turn-start · turn=${envelope.turn}`
    case 'turn-end':
      return `turn-end · turn=${envelope.turn}`
    case 'step-start':
      return `step-start · turn=${envelope.turn} step=${envelope.step}`
    case 'step-end':
      return `step-end · turn=${envelope.turn} step=${envelope.step}`
    case 'user-message':
      return `user-message · ${trimHead(envelope.text)}`
    case 'system-prompt':
      return `system-prompt · turn=${envelope.turn} step=${envelope.step}`
    case 'assistant-chunk':
      return `assistant-chunk · turn=${envelope.turn} step=${envelope.step} · ${describeChunk(envelope.chunk)}`
    case 'assistant-message':
      return `assistant-message · turn=${envelope.turn} step=${envelope.step} · ${envelope.blocks.length} blocks`
    case 'tool-call':
      return `tool-call · ${envelope.name} · call_id=${envelope.call_id}`
    case 'tool-result':
      return `tool-result · call_id=${envelope.call_id}${envelope.is_error ? ' · FAILED' : ''}`
    case 'todo-write':
      return `todo-write · ${envelope.todos.length} 项`
    case 'permission-mode':
      return `permission-mode · ${envelope.mode}`
    case 'approval-policy':
      return `approval-policy · ${envelope.policy}`
    case 'approval-asked':
      return `approval-asked · tool=${envelope.tool}`
    case 'approval-decided':
      return `approval-decided · ${envelope.outcome}`
    case 'compaction-summary':
      return `compaction-summary · turn=${envelope.turn} step=${envelope.step} · replaces ${envelope.replaces_from}..${envelope.replaces_to}`
  }
}

function eventBody(envelope: SessionEnvelope): string {
  const t = envelope.type
  const lines: string[] = []
  switch (t) {
    case 'turn-start':
    case 'turn-end':
    case 'step-start':
    case 'step-end':
      // 边界事件无额外 payload
      return ''
    case 'user-message': {
      const injectedTag = envelope.injected ? ' · 注入' : ''
      const imgTag = envelope.images && envelope.images.length > 0 ? imageBullets(envelope.images) : ''
      lines.push(`- 角色: user${injectedTag}`)
      lines.push(`- 文本(${envelope.text.length} 字):`)
      lines.push('')
      lines.push(safeText(envelope.text) || '`(空)`')
      lines.push(imgTag)
      return lines.join('\n')
    }
    case 'system-prompt':
      lines.push(`- 角色: system`)
      lines.push(`- 文本(${envelope.text.length} 字):`)
      lines.push('')
      lines.push('<details><summary>展开系统提示词</summary>')
      lines.push('')
      lines.push('```')
      lines.push(fenceSafe(envelope.text))
      lines.push('```')
      lines.push('')
      lines.push('</details>')
      return lines.join('\n')
    case 'assistant-chunk':
      lines.push(`- chunk: ${describeChunk(envelope.chunk)}`)
      // text / reasoning / tool-call 携带具体内容,铺出来便于审查
      if (envelope.chunk.type === 'text-delta') {
        lines.push('')
        lines.push('> ' + envelope.chunk.text)
      } else if (envelope.chunk.type === 'reasoning-delta') {
        lines.push('')
        lines.push('> [reasoning] ' + envelope.chunk.text)
      } else if (envelope.chunk.type === 'tool-call-delta' && envelope.chunk.arguments_delta) {
        lines.push('')
        lines.push('- arguments_delta:')
        lines.push('')
        lines.push('```')
        lines.push(fenceSafe(envelope.chunk.arguments_delta))
        lines.push('```')
      } else if (envelope.chunk.type === 'block-end' && envelope.chunk.block.type === 'text') {
        lines.push('')
        lines.push('- block 文本(完整):')
        lines.push('')
        lines.push(safeText(envelope.chunk.block.text) || '`(空)`')
      } else if (envelope.chunk.type === 'block-end' && envelope.chunk.block.type === 'reasoning') {
        lines.push('')
        lines.push('- 完整思考过程:')
        lines.push('')
        lines.push(safeText(envelope.chunk.block.text) || '`(空)`')
      } else if (envelope.chunk.type === 'usage') {
        lines.push('')
        lines.push(`- ${usageParts(envelope.chunk.usage).join(' · ') || '空 usage'}`)
      } else if (envelope.chunk.type === 'finish') {
        const reason = envelope.chunk.reason
        lines.push('')
        if (reason.kind === 'aborted' || reason.kind === 'error') {
          const failure = reason.failure
          lines.push(`- finish: ${reason.kind} (${failure.code}: ${failure.message})`)
        } else {
          lines.push(`- finish: ${reason.kind}`)
        }
      }
      return lines.join('\n')
    case 'assistant-message': {
      lines.push(`- blocks(${envelope.blocks.length}):`)
      for (const [idx, block] of envelope.blocks.entries()) {
        lines.push(`  - [${idx}] ${blockSummary(block)}`)
        if (block.type === 'text') {
          lines.push('')
          lines.push('    > ' + block.text.split('\n').join('\n    > '))
        } else if (block.type === 'reasoning') {
          lines.push('')
          lines.push('    > [reasoning] ' + block.text.split('\n').join('\n    > [reasoning] '))
        } else if (block.type === 'tool-call') {
          lines.push(`    - id: \`${block.id}\``)
          lines.push(`    - name: \`${block.name}\``)
          lines.push(`    - arguments:`)
          lines.push('')
          lines.push('    ```json')
          lines.push('    ' + tryFormatJson(block.arguments).split('\n').join('\n    '))
          lines.push('    ```')
        }
      }
      const usage = usageParts(envelope.usage)
      if (usage.length > 0) lines.push(`- usage: ${usage.join(' · ')}`)
      if (envelope.interrupted) lines.push(`- interrupted: true`)
      return lines.join('\n')
    }
    case 'tool-call': {
      lines.push(`- call_id: \`${envelope.call_id}\``)
      lines.push(`- name: \`${envelope.name}\``)
      lines.push(`- arguments:`)
      lines.push('')
      lines.push(tryFormatJson(envelope.arguments))
      return lines.join('\n')
    }
    case 'tool-result': {
      lines.push(`- call_id: \`${envelope.call_id}\``)
      lines.push(`- is_error: ${envelope.is_error}`)
      if (envelope.error) {
        lines.push(`- error: \`${envelope.error}\``)
      }
      lines.push(`- content(${envelope.content.length} 字):`)
      lines.push('')
      lines.push('```')
      lines.push(fenceSafe(envelope.content))
      lines.push('```')
      return lines.join('\n')
    }
    case 'todo-write': {
      const counts = { pending: 0, in_progress: 0, completed: 0 }
      for (const todo of envelope.todos) counts[todo.status] += 1
      lines.push(`- pending: ${counts.pending}`)
      lines.push(`- in_progress: ${counts.in_progress}`)
      lines.push(`- completed: ${counts.completed}`)
      lines.push('')
      lines.push('| 状态 | 内容 |')
      lines.push('|---|---|')
      for (const todo of envelope.todos) {
        lines.push(`| ${todo.status} | ${safeText(todo.content).replace(/\|/g, '\\|').replace(/\n/g, ' ')} |`)
      }
      return lines.join('\n')
    }
    case 'permission-mode':
      lines.push(`- mode: \`${envelope.mode}\``)
      return lines.join('\n')
    case 'approval-policy':
      lines.push(`- policy: \`${envelope.policy}\``)
      return lines.join('\n')
    case 'approval-asked': {
      lines.push(`- request_id: \`${envelope.request_id}\``)
      lines.push(`- call_id: \`${envelope.call_id}\``)
      lines.push(`- tool: \`${envelope.tool}\``)
      lines.push(`- args_preview(${envelope.args_preview.length} 字):`)
      lines.push('')
      lines.push('```')
      lines.push(fenceSafe(envelope.args_preview))
      lines.push('```')
      if (envelope.reason) {
        lines.push('')
        lines.push(`- reason: \`${envelope.reason}\``)
      }
      return lines.join('\n')
    }
    case 'approval-decided':
      lines.push(`- request_id: \`${envelope.request_id}\``)
      lines.push(`- outcome: \`${envelope.outcome}\``)
      return lines.join('\n')
    case 'compaction-summary': {
      lines.push(`- replaces_from: ${envelope.replaces_from}`)
      lines.push(`- replaces_to: ${envelope.replaces_to}`)
      lines.push(`- keep_from: ${envelope.keep_from}`)
      if (envelope.pre_tokens !== undefined) lines.push(`- pre_tokens: ${envelope.pre_tokens}`)
      if (envelope.post_tokens !== undefined) lines.push(`- post_tokens: ${envelope.post_tokens}`)
      lines.push(`- summary(${envelope.summary.length} 字):`)
      lines.push('')
      lines.push('```')
      lines.push(fenceSafe(envelope.summary))
      lines.push('```')
      return lines.join('\n')
    }
  }
}

function toMarkdown(input: ExportInput): string {
  const { header, events, title } = input
  const lines: string[] = []
  lines.push(`# ${title}`)
  lines.push('')
  // 头部
  lines.push('## 元数据')
  lines.push('')
  lines.push(`- 会话 ID: \`${header.id}\``)
  lines.push(`- 创建时间: ${formatTime(header.created_at)}`)
  lines.push(`- 工作目录: \`${header.cwd}\``)
  lines.push(`- 沙箱: ${header.sandbox ? '是' : '否'}`)
  if (header.parent_session) lines.push(`- 分支自: \`${header.parent_session}\``)
  lines.push(`- 共 ${events.length} 条事件`)
  const turnCount = events.filter((e) => e.type === 'turn-end').length
  lines.push(`- 已关闭 turn: ${turnCount}`)
  lines.push('')

  // 事件总览表(便于扫读)
  lines.push('## 事件总览')
  lines.push('')
  lines.push('| seq | 时间 | 类型 | 摘要 |')
  lines.push('|---:|---|---|---|')
  for (const envelope of events) {
    lines.push(
      `| ${envelope.seq} | ${formatTimeMs(envelope.time)} | \`${envelope.type}\` | ${trimHead(headLine(envelope), 80).replace(/\|/g, '\\|')} |`,
    )
  }
  lines.push('')
  lines.push('---')
  lines.push('')

  // 事件流详情
  lines.push('## 事件流')
  lines.push('')
  if (events.length === 0) {
    lines.push('_（此会话尚无事件）_')
    return lines.join('\n')
  }
  for (const envelope of events) {
    lines.push(`### seq=${envelope.seq} · ${formatTimeMs(envelope.time)} · ${headLine(envelope)}`)
    lines.push('')
    const body = eventBody(envelope)
    if (body) {
      lines.push(body)
      lines.push('')
    }
  }
  return lines.join('\n')
}

function sanitizeFilename(name: string): string {
  return name.replace(/[<>:"/\\|?*\x00-\x1f]/g, '_').replace(/[ .]+$/, '').slice(0, 80) || 'session'
}

export function serializeSession(format: ExportFormat, input: ExportInput): Serialized {
  if (input.events.length === 0 && !input.header) {
    throw new Error('empty')
  }
  const base = sanitizeFilename(input.title || `session-${input.header.id}`)
  if (format === 'json') {
    return {
      content: JSON.stringify({ header: input.header, events: input.events }, null, 2),
      filename: `${base}.json`,
      mimeType: 'application/json',
    }
  }
  return {
    content: toMarkdown(input),
    filename: `${base}.md`,
    mimeType: 'text/markdown;charset=utf-8',
  }
}

/** 触发浏览器下载;SSR / 非浏览器环境下安全退化。 */
export function downloadFile(content: string, filename: string, mimeType: string): void {
  if (typeof document === 'undefined' || typeof URL === 'undefined') return
  const blob = new Blob([content], { type: mimeType })
  const url = URL.createObjectURL(blob)
  const anchor = document.createElement('a')
  anchor.href = url
  anchor.download = filename
  anchor.rel = 'noopener'
  document.body.appendChild(anchor)
  anchor.click()
  document.body.removeChild(anchor)
  setTimeout(() => URL.revokeObjectURL(url), 0)
}
