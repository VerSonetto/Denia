/**
 * Trajectory 布局投影(抄 dsh ui-trajectory 的 `deriveTrajectoryLayout`,
 * 裁剪掉本仓没有的 Code Mode subtool / compaction / Request 节点):
 *
 * - 事件流折叠为三类表面记录:User / Message / Tool;reasoning 块跳过
 *   (无块级时钟,dsh 决策:与其显示 — 不如省略该行)。
 * - 每对 `tool-call` + `tool-result` 按 callId 折成一行 Tool;
 *   result 未到 → 运行中,Time 为 —。
 * - User 行自身耗时 +0s;Message = assistant-message 时间 − 上一表面时间
 *   (跳过的 context 节点仍推进游标);Tool = result 时间 − call 时间。
 * - Message 行携带 assistant-message 的 usage(无 text 块时仍产生一条
 *   空回退行挂 usage);TTFT / 生成耗时来自 chunk 级时钟。
 * - 轮次分组:组头耗时 = 组内最早 → 最晚绝对时间(墙钟跨度,Tool 贡献
 *   起点 + 自身耗时);组描述含工具直方图(`1.5s bash×6`)。
 * - 无线上 turn 的 user-message(坏日志防御)归入"轮次之间"组。
 *
 * 纯函数:冷历史与流式增量走同一条 fold,输出逐字一致。
 */

import type { SessionEnvelope, TokenUsage } from './types'

export type TrajectoryKind = 'user' | 'message' | 'tool'

export interface TrajectoryRecord {
  /** 稳定行 key(fold 定稿时分配);概览条与台账行 hover 联动用。 */
  key: string
  kind: TrajectoryKind
  /** 所属轮次;null = 无线上 turn(轮次之间)。 */
  turn: number | null
  step?: number
  /** 产生该记录的事件 seq。 */
  seq: number
  /** 开始时间(epoch ms)。 */
  time: number
  /** 自身耗时;运行中缺省(dsh:Time —,不伪造)。 */
  durationMs?: number
  /* ---- user ---- */
  text?: string
  injected?: boolean
  imageCount?: number
  /* ---- message ---- */
  /** text 块正文(一个 text 块一行 Message)。 */
  content?: string
  usage?: TokenUsage
  interrupted?: boolean
  /** 首 token 延迟(step-start → 首个 assistant-chunk)。 */
  ttftMs?: number
  /** 生成耗时(首个 chunk → settle)。 */
  decodeMs?: number
  /* ---- tool ---- */
  callId?: string
  toolName?: string
  args?: string
  result?: { content: string; isError: boolean }
}

export interface TrajectoryToolStat {
  name: string
  count: number
  totalMs: number
}

export interface TrajectoryGroup {
  /** 轮次号;null = 轮次之间。 */
  turn: number | null
  /** 轮次未闭合(进行中)。 */
  open: boolean
  /** 结束原因(轮次闭合时);组头据此显示完成/中止/超长/出错/中断徽标。 */
  endReason?: 'completed' | 'aborted' | 'max-tokens' | 'error' | 'interrupted'
  startTime: number
  endTime: number
  /** 墙钟跨度(endTime − startTime)。 */
  wallMs: number
  records: TrajectoryRecord[]
  toolStats: TrajectoryToolStat[]
}

export interface TrajectoryLayout {
  groups: TrajectoryGroup[]
  /** 全部记录,按时间顺序(跨组扁平,供 Overview 条使用)。 */
  records: TrajectoryRecord[]
  startTime: number
  endTime: number
}

export function deriveTrajectory(events: SessionEnvelope[]): TrajectoryLayout {
  const groups: TrajectoryGroup[] = []
  const records: TrajectoryRecord[] = []
  // 可变扫描上下文(对象承载:TS 闭包内赋值不参与流程收窄)。
  const ctx = {
    turn: null as number | null,
    group: null as TrajectoryGroup | null,
    // Message 耗时游标:上一表面时间(user/tool-call/message 自身,跳过的
    // context 节点推进但不产出记录)。
    lastSurface: null as number | null,
  }
  // chunk 级时钟:step-start / 首 chunk 时间,按 turn:step。
  const stepStarts = new Map<string, number>()
  const firstChunks = new Map<string, number>()
  // callId → tool 记录(配对 tool-result)。
  const toolByCall = new Map<string, TrajectoryRecord>()
  // 先于 turn-start 落库的用户消息(本仓驱动器布局):按 dsh 规则
  // 归入下一个 turn 组;到日志结束仍无 turn → "轮次之间"。
  const pendingUsers: TrajectoryRecord[] = []

  const startGroup = (turn: number | null, time: number, open: boolean): TrajectoryGroup => {
    const group: TrajectoryGroup = {
      turn,
      open,
      startTime: time,
      endTime: time,
      wallMs: 0,
      records: [],
      toolStats: [],
    }
    groups.push(group)
    ctx.group = group
    return group
  }
  const endGroup = () => {
    if (ctx.group) {
      finishGroupSpan(ctx.group)
      ctx.group = null
    }
  }
  const push = (group: TrajectoryGroup | null, record: TrajectoryRecord) => {
    records.push(record)
    if (group) group.records.push(record)
  }
  /**
   * 闭合组的统计(dsh 组描述语义):
   * - 墙钟跨度 = 组内最早 → 最晚绝对时间;只有 Tool 贡献"起点 + 自身耗时"
   *   终点,Message 的 durationMs 是"距上一表面"的间隔,不得叠加进时间轴。
   * - 工具直方图此处统一重算(tool-result 的耗时是事后回填,增量聚合
   *   会停留在 0)。
   */
  const finishGroupSpan = (group: TrajectoryGroup) => {
    let start = group.startTime
    let end = group.startTime
    const stats = new Map<string, TrajectoryToolStat>()
    for (const record of group.records) {
      start = Math.min(start, record.time)
      const finish =
        record.kind === 'tool' && record.durationMs !== undefined
          ? record.time + record.durationMs
          : record.time
      end = Math.max(end, finish)
      if (record.kind === 'tool' && record.toolName) {
        const stat = stats.get(record.toolName) ?? {
          name: record.toolName,
          count: 0,
          totalMs: 0,
        }
        stat.count += 1
        stat.totalMs += record.durationMs ?? 0
        stats.set(record.toolName, stat)
      }
    }
    group.startTime = start
    group.endTime = end
    group.wallMs = Math.max(0, end - start)
    group.toolStats = [...stats.values()]
  }

  for (const event of events) {
    switch (event.type) {
      case 'turn-start': {
        endGroup()
        ctx.turn = event.turn
        const group = startGroup(event.turn, event.time, true)
        // 先于本轮落库的用户消息归入本组(dsh 规则;已在扁平表,不重复 push)。
        for (const pending of pendingUsers) group.records.push(pending)
        pendingUsers.length = 0
        break
      }
      case 'turn-end': {
        if (ctx.group) {
          ctx.group.open = false
          ctx.group.endReason = event.reason.kind
        }
        endGroup()
        ctx.turn = null
        break
      }
      case 'step-start': {
        stepStarts.set(`${event.turn}:${event.step}`, event.time)
        break
      }
      case 'user-message': {
        const record: TrajectoryRecord = {
          kind: 'user',
          key: '',
          turn: ctx.turn,
          seq: event.seq,
          time: event.time,
          durationMs: 0,
          text: event.text,
          injected: event.injected,
          imageCount: event.images?.length,
        }
        if (ctx.turn === null) {
          // 先于任何 turn:等下一个 turn-start 归组(dsh 规则)。
          pendingUsers.push(record)
          records.push(record)
        } else {
          push(ctx.group, record)
        }
        ctx.lastSurface = event.time
        break
      }
      case 'system-prompt': {
        // context 节点不产出记录,但推进 Message 耗时游标(dsh 语义)。
        ctx.lastSurface = event.time
        break
      }
      case 'assistant-chunk': {
        const key = `${event.turn}:${event.step}`
        if (!firstChunks.has(key)) firstChunks.set(key, event.time)
        break
      }
      case 'assistant-message': {
        const group = ctx.group
        const stepStart = stepStarts.get(`${event.turn}:${event.step}`)
        const firstChunk = firstChunks.get(`${event.turn}:${event.step}`)
        const ttftMs =
          stepStart !== undefined && firstChunk !== undefined
            ? Math.max(0, firstChunk - stepStart)
            : undefined
        const decodeMs = firstChunk !== undefined ? Math.max(0, event.time - firstChunk) : undefined
        const durationMs =
          ctx.lastSurface !== null ? Math.max(0, event.time - ctx.lastSurface) : undefined
        const textBlocks = event.blocks
          .filter((block) => block.type === 'text')
          .map((block) => (block.type === 'text' ? block.text : ''))
        // 每个 text 块一行 Message;usage 只挂第一行;无 text 块时空回退行。
        if (textBlocks.length === 0) {
          push(group, {
            kind: 'message',
          key: '',
            turn: ctx.turn,
            step: event.step,
            seq: event.seq,
            time: event.time,
            durationMs,
            usage: event.usage,
            interrupted: event.interrupted,
            ttftMs,
            decodeMs,
          })
        } else {
          textBlocks.forEach((content, index) => {
            push(group, {
              kind: 'message',
          key: '',
              turn: ctx.turn,
              step: event.step,
              seq: event.seq,
              time: event.time,
              durationMs: index === 0 ? durationMs : undefined,
              content,
              usage: index === 0 ? event.usage : undefined,
              interrupted: index === 0 ? event.interrupted : undefined,
              ttftMs: index === 0 ? ttftMs : undefined,
              decodeMs: index === 0 ? decodeMs : undefined,
            })
          })
        }
        ctx.lastSurface = event.time
        break
      }
      case 'tool-call': {
        const group = ctx.group
        const record: TrajectoryRecord = {
          kind: 'tool',
          key: '',
          turn: ctx.turn,
          step: event.step,
          seq: event.seq,
          time: event.time,
          callId: event.call_id,
          toolName: event.name,
          args: event.arguments,
        }
        push(group, record)
        toolByCall.set(event.call_id, record)
        ctx.lastSurface = event.time
        break
      }
      case 'tool-result': {
        const record = toolByCall.get(event.call_id)
        const result = { content: event.content, isError: event.is_error }
        if (record) {
          record.durationMs = Math.max(0, event.time - record.time)
          record.result = result
        } else {
          // 孤儿结果:独立一行,自身耗时不可知。
          push(ctx.group, {
            kind: 'tool',
          key: '',
            turn: ctx.turn,
            step: event.step,
            seq: event.seq,
            time: event.time,
            callId: event.call_id,
            result,
          })
        }
        break
      }
      default:
        break
    }
  }
  endGroup()

  // 到日志结束仍无 turn 的用户消息 → "轮次之间"组。
  if (pendingUsers.length > 0) {
    const group = startGroup(null, pendingUsers[0].time, false)
    for (const pending of pendingUsers) group.records.push(pending)
    pendingUsers.length = 0
    endGroup()
  }

  const start = events[0]?.time ?? 0
  const end = records.reduce(
    (latest, record) =>
      Math.max(latest, record.durationMs !== undefined ? record.time + record.durationMs : record.time),
    start,
  )
  // 稳定行 key:fold 定稿后统一分配(概览条与台账行联动共用)。
  records.forEach((record, index) => {
    record.key = `r${index}`
  })
  return { groups, records, startTime: start, endTime: end }
}

/** 自身耗时列:+Ns / +N.1s,≥60s 为 +N分SSs;运行中为 —。 */
export function formatSelfDuration(ms?: number): string {
  if (ms === undefined) return '—'
  if (ms < 60_000) {
    const seconds = ms / 1000
    return `+${seconds < 10 ? seconds.toFixed(1) : Math.round(seconds)}s`
  }
  const minutes = Math.floor(ms / 60_000)
  const seconds = Math.round((ms % 60_000) / 1000)
  return `+${minutes}m${String(seconds).padStart(2, '0')}s`
}

/* ---- 轨迹引用(发给 AI 的上下文,插入方式与粘贴图片一致) ---- */

export interface TrajectoryQuote {
  /** composer 芯片标识与移除键。 */
  id: string
  /** 芯片上的短标题(轨迹 · bash · 12:03 / 轨迹区间 · 14 条)。 */
  title: string
  /** 发给模型的完整引用正文。 */
  text: string
}

/** 引用正文的单条上限;超出截断并注明。 */
const QUOTE_TEXT_LIMIT = 60_000

function quoteClock(time: number): string {
  return new Date(time).toLocaleTimeString('zh-CN', { hour12: false })
}

function quotePretty(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2)
  } catch {
    return raw
  }
}

function quoteFence(content: string): string {
  const trimmed = content.length > 4000 ? `${content.slice(0, 4000)}\n…(已截断)` : content
  const fences = trimmed.match(/^`{3,}/gm)
  const fence = fences ? '`'.repeat(Math.max(3, ...fences.map((f) => f.length)) + 1) : '```'
  return `${fence}\n${trimmed}\n${fence}`
}

function kindLabelZh(record: TrajectoryRecord): string {
  if (record.kind === 'user') return '用户消息'
  if (record.kind === 'message') return '助手消息'
  return `工具 ${record.toolName ?? '?'}`
}

function recordTurnLabel(record: TrajectoryRecord): string {
  if (record.turn === null) return '轮次之间'
  const step = record.step !== undefined ? ` · 步骤 ${record.step}` : ''
  return `第 ${record.turn} 轮${step}`
}

/** 单条轨迹记录 → 引用正文块。 */
function quoteRecordBlock(record: TrajectoryRecord): string {
  const head = `### ${kindLabelZh(record)}(${recordTurnLabel(record)}) · ${quoteClock(record.time)}`
  const lines: string[] = [head]
  if (record.kind === 'tool') {
    lines.push(`- 耗时:${formatSelfDuration(record.durationMs)}`)
    if (record.args) lines.push(`- 参数:\n${quoteFence(quotePretty(record.args))}`)
    if (record.result) {
      lines.push(
        `- 结果(${record.result.isError ? '失败' : '成功'}):\n${quoteFence(record.result.content)}`,
      )
    } else {
      lines.push('- 结果:未捕获(进行中)')
    }
  } else if (record.kind === 'message') {
    const timing: string[] = []
    if (record.ttftMs !== undefined) timing.push(`首 token ${formatSelfDuration(record.ttftMs)}`)
    if (record.decodeMs !== undefined) timing.push(`生成 ${formatSelfDuration(record.decodeMs)}`)
    lines.push(`- 耗时:${formatSelfDuration(record.durationMs)}${timing.length ? `(${timing.join(' · ')})` : ''}`)
    if (record.usage) {
      lines.push(`- Token:输入 ${record.usage.inputTokens} · 输出 ${record.usage.outputTokens}`)
    }
    lines.push(`- 内容:\n${quoteFence(record.content ?? '(无文本内容)')}`)
  } else {
    lines.push(`- 内容:\n${quoteFence(record.text ?? '')}`)
  }
  return lines.join('\n')
}

/** 单条轨迹记录 → 引用(芯片 + 正文)。 */
export function quoteTrajectoryRecord(record: TrajectoryRecord): TrajectoryQuote {
  const text = recordRecordText(record)
  return {
    id: `traj-q-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    title: `轨迹 · ${record.toolName ?? (record.kind === 'message' ? '消息' : '用户消息')} · ${quoteClock(record.time)}`,
    text,
  }
}

function recordRecordText(record: TrajectoryRecord): string {
  const body = `## 引用轨迹记录\n\n${quoteRecordBlock(record)}`
  return truncateQuote(body)
}

/** 一段区间(起点入区间的记录)→ 引用。 */
export function quoteTrajectoryInterval(
  records: TrajectoryRecord[],
  start: number,
  end: number,
): TrajectoryQuote | null {
  if (records.length === 0) return null
  const body = [
    `## 引用轨迹区间 ${quoteClock(start)} → ${quoteClock(end)}(共 ${records.length} 条记录)`,
    '',
    ...records.map(quoteRecordBlock),
  ].join('\n\n')
  return {
    id: `traj-q-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    title: `轨迹区间 · ${records.length} 条`,
    text: truncateQuote(body),
  }
}

function truncateQuote(text: string): string {
  if (text.length <= QUOTE_TEXT_LIMIT) return text
  return `${text.slice(0, QUOTE_TEXT_LIMIT)}\n\n…(引用过长,已截断)`
}
