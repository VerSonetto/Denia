import type {
  AskQuestion,
  AskResolution,
  ContentBlock,
  SessionEnvelope,
  StreamChunk,
  TokenUsage,
  TurnEndReason,
  UserMessageImage,
} from './types'
import type { TodoSnapshotItem } from './toolDisplay'

/** `ask` 工具的问答载荷(挂在对应工具行上)。 */
export interface AskCardData {
  requestId: string
  questions: AskQuestion[]
  timeoutMs: number
  /** 请求发起时刻(epoch ms),倒计时用。 */
  startedAt: number
  /** 已结算时的结果;缺省 = 仍在等待。 */
  resolution?: AskResolution
}

/** One rendered block inside an assistant message. */
export interface UiBlock {
  kind: 'text' | 'reasoning' | 'tool-call'
  text: string
  id?: string
  name?: string
  args?: string
}

export type TranscriptNode =
  | { kind: 'user'; text: string; anchor?: number; images?: UserMessageImage[] }
  | { kind: 'context-injection'; text: string; seq?: number }
  | { kind: 'system-prompt'; text: string }
  | {
      kind: 'assistant'
      turn: number
      step: number
      /** assistant-message 事件的 seq;流式未 settle 时缺省。分支锚点。 */
      seq?: number
      blocks: UiBlock[]
      usage?: TokenUsage
      interrupted: boolean
      streaming: boolean
      /** step-start 事件的 epoch ms;无 step-start 时为 undefined。 */
      stepStartTime?: number
      /** 第一个 assistant-chunk 的 epoch ms;TTFT = firstChunkTime - stepStartTime。 */
      firstChunkTime?: number
      /** assistant-message settle 的 epoch ms;decodeMs = settleTime - firstChunkTime。 */
      settleTime?: number
    }
  | {
      kind: 'tool'
      callId: string
      name: string
      args: string
      /** tool-call 事件的 seq;孤儿结果行用 result 事件的 seq。 */
      seq?: number
      result?: { content: string; isError: boolean }
      /** `ask` 工具的提问载荷:挂在对应工具行上渲染问答卡片。 */
      ask?: AskCardData
      /**
       * todo_write 的前一次清单快照(上一次 todo-write 事件的 todos)。
       * 卡片据此标出"本次哪些条目推进了";首张清单为 undefined。
       */
      prevTodos?: TodoSnapshotItem[]
    }
  | { kind: 'turn-start'; turn: number; time: number }
  | {
      kind: 'turn-end'
      turn: number
      time: number
      reason: TurnEndReason
      usage?: TokenUsage
    }
  | {
      kind: 'compaction'
      turn: number
      step: number
      summary: string
      replacesFrom: number
      replacesTo: number
      keepFrom: number
      preTokens?: number
      postTokens?: number
      seq: number
    }
  | {
      /** 本地斜杠命令回显(如 /goal):右对齐命令气泡,按事件 seq 排序。 */
      kind: 'command-echo'
      name: string
      text: string
      seq: number
    }

function toUiBlock(block: ContentBlock): UiBlock {
  switch (block.type) {
    case 'text':
      return { kind: 'text', text: block.text }
    case 'reasoning':
      return { kind: 'reasoning', text: block.text }
    case 'tool-call':
      return {
        kind: 'tool-call',
        text: '',
        id: block.id,
        name: block.name,
        args: block.arguments,
      }
  }
}

/**
 * Applies one stream chunk to an immutable block list. The delta decides the
 * block kind when it arrives before its `block-start` (reasoning deltas are
 * never rendered as plain text); an existing loose `text` block is upgraded,
 * never downgraded. `block-end` replaces the block with the authority.
 */
function applyChunk(blocks: UiBlock[], chunk: StreamChunk): UiBlock[] {
  switch (chunk.type) {
    case 'block-start': {
      // 正常流:delta 携带的 index 与已打开块数一致。异常流(网络错误重试,
      // 模型从头重新生成)会重发同 index 的 block-start,而后续 delta 仍按
      // 原 index 路由 —— 原样追加会造出一个永远收不到内容的空块(并且旧块
      // 还会串进重试后的 delta)。把 index 规范化为"当前块数"并跳过对已
      // 存在块的重复开启,两条路径(冷历史/实时流)才与 settle 后的权威块
      // (重试尝试整体重建、无重复块)保持一致。
      const existing = blocks[chunk.index]
      if (
        existing !== undefined &&
        (chunk.index !== blocks.length || existing.kind === chunk.block_type)
      ) {
        return blocks
      }
      return [...blocks, { kind: chunk.block_type, text: '' }]
    }
    case 'text-delta':
    case 'reasoning-delta': {
      const kind = chunk.type === 'reasoning-delta' ? 'reasoning' as const : 'text' as const
      const block = blocks[chunk.index] ?? { kind, text: '' }
      const next = [...blocks]
      next[chunk.index] = {
        ...block,
        kind: block.kind === 'text' ? kind : block.kind,
        text: block.text + chunk.text,
      }
      return next
    }
    case 'tool-call-delta': {
      const block = blocks[chunk.index] ?? { kind: 'tool-call' as const, text: '' }
      const next = [...blocks]
      next[chunk.index] = {
        ...block,
        args: (block.args ?? '') + chunk.arguments_delta,
        id: chunk.id || block.id,
        name: chunk.name ?? block.name,
      }
      return next
    }
    case 'block-end': {
      const settled = toUiBlock(chunk.block)
      const next = [...blocks]
      next[chunk.index] = { ...settled, text: settled.text || blocks[chunk.index]?.text || '' }
      return next
    }
    default:
      return blocks
  }
}

/**
 * The deterministic fold: identical output for cold history and live
 * streaming. Chunk deltas accumulate into an in-progress assistant node;
 * the settled `assistant-message` replaces it with authoritative blocks.
 */
export function foldEvents(events: SessionEnvelope[]): TranscriptNode[] {
  const nodes: TranscriptNode[] = []
  let open: Extract<TranscriptNode, { kind: 'assistant' }> | null = null
  const tools = new Map<string, Extract<TranscriptNode, { kind: 'tool' }>>()
  let turnUsage: TokenUsage | null = null
  // step-start 时间表:turn:step → epoch ms,供 assistant 节点算 TTFT。
  const stepStarts = new Map<string, number>()
  // 当前清单快照:供下一次 todo_write 卡片比对"这次推进了哪条"。
  let lastTodos: TodoSnapshotItem[] | undefined

  const addUsage = (usage?: TokenUsage) => {
    if (!usage) return
    turnUsage = {
      inputTokens: (turnUsage?.inputTokens ?? 0) + usage.inputTokens,
      outputTokens: (turnUsage?.outputTokens ?? 0) + usage.outputTokens,
      cacheReadTokens:
        (turnUsage?.cacheReadTokens ?? 0) + (usage.cacheReadTokens ?? 0) || undefined,
      reasoningTokens:
        (turnUsage?.reasoningTokens ?? 0) + (usage.reasoningTokens ?? 0) || undefined,
    }
  }

  const closeOpen = () => {
    if (open) {
      open.streaming = false
      open = null
    }
  }

  for (const event of events) {
    switch (event.type) {
      case 'agent-delivery':
        closeOpen()
        nodes.push({ kind: 'context-injection', text: event.text, seq: event.seq })
        break
      case 'command-run':
        closeOpen()
        nodes.push({ kind: 'command-echo', name: event.name, text: event.text, seq: event.seq })
        break
      case 'turn-start':
        closeOpen()
        nodes.push({ kind: 'turn-start', turn: event.turn, time: event.time })
        break
      case 'user-message':
        closeOpen()
        if (event.injected) {
          nodes.push({ kind: 'context-injection', text: event.text, seq: event.seq })
        } else {
          nodes.push({
            kind: 'user',
            text: event.text,
            anchor: event.seq,
            images: event.images?.length ? event.images : undefined,
          })
        }
        break
      case 'system-prompt':
        closeOpen()
        nodes.push({ kind: 'system-prompt', text: event.text })
        break
      case 'step-start':
        stepStarts.set(`${event.turn}:${event.step}`, event.time)
        break
      case 'step-end':
        break
      case 'assistant-chunk': {
        if (!open || open.turn !== event.turn || open.step !== event.step) {
          closeOpen()
          open = {
            kind: 'assistant',
            turn: event.turn,
            step: event.step,
            blocks: [],
            interrupted: false,
            streaming: true,
            stepStartTime: stepStarts.get(`${event.turn}:${event.step}`),
            firstChunkTime: event.time,
          }
          nodes.push(open)
        }
        open.blocks = applyChunk(open.blocks, event.chunk)
        break
      }
      case 'assistant-message': {
        const blocks = event.blocks.map(toUiBlock)
        if (open && open.turn === event.turn && open.step === event.step) {
          open.blocks = blocks
          open.usage = event.usage
          open.interrupted = event.interrupted ?? false
          open.settleTime = event.time
          open.seq = event.seq
          closeOpen()
        } else {
          closeOpen()
          nodes.push({
            kind: 'assistant',
            turn: event.turn,
            step: event.step,
            seq: event.seq,
            blocks,
            usage: event.usage,
            interrupted: event.interrupted ?? false,
            streaming: false,
            stepStartTime: stepStarts.get(`${event.turn}:${event.step}`),
            settleTime: event.time,
          })
        }
        addUsage(event.usage)
        break
      }
      case 'tool-call': {
        closeOpen()
        const node: Extract<TranscriptNode, { kind: 'tool' }> = {
          kind: 'tool',
          callId: event.call_id,
          name: event.name,
          args: event.arguments,
          seq: event.seq,
        }
        // todo_write:带上本次调用之前的清单快照(工具执行时才 emit
        // todo-write 事件,所以这里的 lastTodos 就是"上一次的")。
        if (event.name === 'todo_write') node.prevTodos = lastTodos
        tools.set(event.call_id, node)
        nodes.push(node)
        break
      }
      case 'todo-write':
        // 清单快照(last-write-wins):只更新状态,不产生对话流节点 ——
        // 清单由对应的 todo_write 工具行渲染,避免同一份数据出现两处。
        lastTodos = event.todos
        break
      case 'ask-requested': {
        const node = tools.get(event.call_id)
        const ask: AskCardData = {
          requestId: event.request_id,
          questions: event.questions,
          timeoutMs: event.timeout_ms,
          startedAt: event.time,
        }
        if (node) {
          node.ask = ask
        } else {
          closeOpen()
          nodes.push({
            kind: 'tool',
            callId: event.call_id,
            name: 'ask',
            args: '',
            seq: event.seq,
            ask,
          })
        }
        break
      }
      case 'ask-resolved': {
        for (const node of nodes) {
          if (node.kind === 'tool' && node.ask?.requestId === event.request_id) {
            node.ask = { ...node.ask, resolution: event.resolution }
            break
          }
        }
        break
      }
      case 'tool-result': {
        const node = tools.get(event.call_id)
        const result = { content: event.content, isError: event.is_error }
        if (node) {
          node.result = result
        } else {
          nodes.push({
            kind: 'tool',
            callId: event.call_id,
            name: '?',
            args: '',
            seq: event.seq,
            result,
          })
        }
        break
      }
      case 'compaction-summary': {
        closeOpen()
        nodes.push({
          kind: 'compaction',
          turn: event.turn,
          step: event.step,
          summary: event.summary,
          replacesFrom: event.replaces_from,
          replacesTo: event.replaces_to,
          keepFrom: event.keep_from,
          preTokens: event.pre_tokens,
          postTokens: event.post_tokens,
          seq: event.seq,
        })
        break
      }
      case 'turn-end':
        closeOpen()
        nodes.push({
          kind: 'turn-end',
          turn: event.turn,
          time: event.time,
          reason: event.reason,
          usage: turnUsage ?? undefined,
        })
        turnUsage = null
        break
      default:
        break
    }
  }
  closeOpen()
  return nodes
}

/** A turn is live when some `turn-start` lacks its `turn-end`. */
export function hasOpenTurn(events: SessionEnvelope[]): boolean {
  let open = false
  for (const event of events) {
    if (event.type === 'turn-start') open = true
    else if (event.type === 'turn-end') open = false
  }
  return open
}

/** 当前未闭合 turn 的起点(epoch ms);无 open turn 返回 undefined。 */
export function openTurnStartedAt(nodes: TranscriptNode[]): number | undefined {
  let started: number | undefined
  for (const node of nodes) {
    if (node.kind === 'turn-start') started = node.time
    else if (node.kind === 'turn-end') started = undefined
  }
  return started
}

/** 增量 fold 的 step-start 时间暂存:turn:step → epoch ms。 */
const incrementalStepStarts = new Map<string, number>()

/** 增量 fold 的清单快照暂存:供下一次 todo_write 卡片比对变化。 */
let incrementalLastTodos: TodoSnapshotItem[] | undefined

/**
 * Incremental fold: applies one envelope to an existing node list without
 * refolding the prefix. Only the affected tail node is copied, so a live
 * stream costs O(1) per envelope instead of O(n).
 */
export function applyEnvelope(
  nodes: TranscriptNode[],
  event: SessionEnvelope,
): TranscriptNode[] {
  return applyEnvelopes(nodes, [event])
}

/**
 * 批量应用一帧内的多条事件(流式 rAF 合并路径)。
 *
 * 单条逐次 `applyEnvelope` 对连续 assistant-chunk 是 O(m·n):每条都要
 * `[...nodes.slice(0, -1), { ...last, ... }]` 复制整个节点数组。流式高频
 * 帧(一帧可达数十条 chunk)在长会话上会放大成每帧 O(m·n) 的数组复制。
 *
 * 这里把落在同一流式 assistant 节点上的连续 chunk 累积进一个可变
 * `blocks` 缓冲,中途完全不动节点数组;遇到非 chunk 事件(settle/
 * turn-end/工具调用等)或帧末,才把缓冲一次 flush 成新数组。连续 chunk
 * 的数组复制从每帧 m 次降到 1 次,语义与 `applyEnvelope` 逐条应用完全一致。
 */
export function applyEnvelopes(
  nodes: TranscriptNode[],
  events: SessionEnvelope[],
): TranscriptNode[] {
  if (events.length === 0) return nodes
  let current = nodes
  // 正在累积的流式尾节点:存在时 current 尾部就是它(引用未变,
  // 只有 accumulate 真实发生时才在 flush 时重建数组)。
  interface Accum {
    node: Extract<TranscriptNode, { kind: 'assistant' }>
    blocks: UiBlock[]
  }
  let acc: Accum | null = null
  const flush = (): Extract<TranscriptNode, { kind: 'assistant' }> | null => {
    if (acc === null) return null
    const settled = { ...acc.node, blocks: acc.blocks }
    current = current.slice(0, -1)
    current.push(settled)
    acc = null
    return settled
  }
  for (const event of events) {
    if (event.type === 'assistant-chunk') {
      const last = current[current.length - 1]
      if (
        acc !== null &&
        last?.kind === 'assistant' &&
        last.streaming &&
        last.turn === event.turn &&
        last.step === event.step
      ) {
        acc.blocks = applyChunk(acc.blocks, event.chunk)
        continue
      }
      // 先落掉上一个累积节点(节点迁移),再开新累积。
      if (acc !== null) flush()
      const tail = current[current.length - 1]
      if (
        tail?.kind === 'assistant' &&
        tail.streaming &&
        tail.turn === event.turn &&
        tail.step === event.step
      ) {
        const blocks = applyChunk(tail.blocks, event.chunk)
        acc = { node: { ...tail }, blocks }
      } else {
        const fresh: Extract<TranscriptNode, { kind: 'assistant' }> = {
          kind: 'assistant',
          turn: event.turn,
          step: event.step,
          blocks: [],
          interrupted: false,
          streaming: true,
          stepStartTime: incrementalStepStarts.get(`${event.turn}:${event.step}`),
          firstChunkTime: event.time,
        }
        acc = { node: fresh, blocks: applyChunk(fresh.blocks, event.chunk) }
        current = [...current, fresh]
      }
      continue
    }
    if (acc !== null) flush()
    current = applyEnvelopeStep(current, event)
  }
  if (acc !== null) flush()
  return current
}

/** 单条事件应用(applyEnvelope 的主体,供批量路径逐条复用)。 */
function applyEnvelopeStep(
  nodes: TranscriptNode[],
  event: SessionEnvelope,
): TranscriptNode[] {
  switch (event.type) {
    case 'agent-delivery':
      return [...nodes, { kind: 'context-injection', text: event.text, seq: event.seq }]
    case 'command-run':
      return [
        ...nodes,
        { kind: 'command-echo', name: event.name, text: event.text, seq: event.seq },
      ]
    case 'turn-start':
      return [...nodes, { kind: 'turn-start', turn: event.turn, time: event.time }]
    case 'user-message':
      if (event.injected) {
        return [...nodes, { kind: 'context-injection', text: event.text, seq: event.seq }]
      }
      return [
        ...nodes,
        {
          kind: 'user',
          text: event.text,
          anchor: event.seq,
          images: event.images?.length ? event.images : undefined,
        },
      ]
    case 'system-prompt':
      return [...nodes, { kind: 'system-prompt', text: event.text }]
    case 'step-start':
      incrementalStepStarts.set(`${event.turn}:${event.step}`, event.time)
      return nodes
    case 'step-end':
      return nodes
    case 'assistant-chunk': {
      const last = nodes[nodes.length - 1]
      if (
        last?.kind === 'assistant' &&
        last.streaming &&
        last.turn === event.turn &&
        last.step === event.step
      ) {
        return [...nodes.slice(0, -1), { ...last, blocks: applyChunk(last.blocks, event.chunk) }]
      }
      const fresh: Extract<TranscriptNode, { kind: 'assistant' }> = {
        kind: 'assistant',
        turn: event.turn,
        step: event.step,
        blocks: [],
        interrupted: false,
        streaming: true,
        stepStartTime: incrementalStepStarts.get(`${event.turn}:${event.step}`),
        firstChunkTime: event.time,
      }
      return [...nodes, { ...fresh, blocks: applyChunk(fresh.blocks, event.chunk) }]
    }
    case 'assistant-message': {
      const last = nodes[nodes.length - 1]
      // settle 时保留流式阶段积累的时间戳。
      const prior = last?.kind === 'assistant' && last.turn === event.turn && last.step === event.step
        ? last
        : undefined
      const settled: TranscriptNode = {
        kind: 'assistant',
        turn: event.turn,
        step: event.step,
        seq: event.seq,
        blocks: event.blocks.map(toUiBlock),
        usage: event.usage,
        interrupted: event.interrupted ?? false,
        streaming: false,
        stepStartTime: prior?.stepStartTime ?? incrementalStepStarts.get(`${event.turn}:${event.step}`),
        firstChunkTime: prior?.firstChunkTime,
        settleTime: event.time,
      }
      if (
        last?.kind === 'assistant' &&
        last.streaming &&
        last.turn === event.turn &&
        last.step === event.step
      ) {
        return [...nodes.slice(0, -1), settled]
      }
      return [...nodes, settled]
    }
    case 'tool-call': {
      const base: Extract<TranscriptNode, { kind: 'tool' }> = {
        kind: 'tool',
        callId: event.call_id,
        name: event.name,
        args: event.arguments,
        seq: event.seq,
      }
      // 与冷路径一致:todo_write 带上本次调用之前的清单快照。
      if (event.name === 'todo_write') base.prevTodos = incrementalLastTodos
      return [...nodes, base]
    }
    case 'todo-write':
      // 清单快照(last-write-wins):不产生节点,清单由工具行渲染。
      incrementalLastTodos = event.todos
      return nodes
    // ask 的提问/结算挂到对应工具行(卡片与工具行同体,不产生游离节点)。
    case 'ask-requested': {
      const patch = (node: Extract<TranscriptNode, { kind: 'tool' }>) => ({
        ...node,
        ask: {
          requestId: event.request_id,
          questions: event.questions,
          timeoutMs: event.timeout_ms,
          startedAt: event.time,
          resolution: undefined,
        },
      })
      for (let i = nodes.length - 1; i >= 0; i--) {
        const node = nodes[i]
        if (node.kind === 'tool' && node.callId === event.call_id) {
          const copy = nodes.slice()
          copy[i] = patch(node)
          return copy
        }
      }
      // 找不到对应工具行(日志截断等):以孤儿工具行承载卡片。
      return [
        ...nodes,
        {
          kind: 'tool',
          callId: event.call_id,
          name: 'ask',
          args: '',
          seq: event.seq,
          ask: {
            requestId: event.request_id,
            questions: event.questions,
            timeoutMs: event.timeout_ms,
            startedAt: event.time,
          },
        },
      ]
    }
    case 'ask-resolved': {
      for (let i = nodes.length - 1; i >= 0; i--) {
        const node = nodes[i]
        if (node.kind === 'tool' && node.ask?.requestId === event.request_id) {
          const copy = nodes.slice()
          copy[i] = { ...node, ask: { ...node.ask, resolution: event.resolution } }
          return copy
        }
      }
      return nodes
    }
    case 'compaction-summary':
      return [
        ...nodes,
        {
          kind: 'compaction',
          turn: event.turn,
          step: event.step,
          summary: event.summary,
          replacesFrom: event.replaces_from,
          replacesTo: event.replaces_to,
          keepFrom: event.keep_from,
          preTokens: event.pre_tokens,
          postTokens: event.post_tokens,
          seq: event.seq,
        },
      ]
    case 'tool-result': {
      for (let i = nodes.length - 1; i >= 0; i--) {
        const node = nodes[i]
        if (node.kind === 'tool' && node.callId === event.call_id) {
          const copy = nodes.slice()
          copy[i] = {
            ...node,
            result: { content: event.content, isError: event.is_error },
          }
          return copy
        }
      }
      return [
        ...nodes,
        {
          kind: 'tool',
          callId: event.call_id,
          name: '?',
          args: '',
          seq: event.seq,
          result: { content: event.content, isError: event.is_error },
        },
      ]
    }
    case 'turn-end': {
      let input = 0
      let output = 0
      for (let i = nodes.length - 1; i >= 0; i--) {
        const node = nodes[i]
        if (node.kind === 'turn-end') break
        if (node.kind === 'assistant' && node.usage) {
          input += node.usage.inputTokens
          output += node.usage.outputTokens
        }
      }
      return [
        ...nodes,
        {
          kind: 'turn-end',
          turn: event.turn,
          time: event.time,
          reason: event.reason,
          usage:
            input || output
              ? { inputTokens: input, outputTokens: output }
              : undefined,
        },
      ]
    }
    default:
      return nodes
  }
}

/* ---- presentation grouping ---- */

/** One closed turn's folded prefix: work duration, tool count, hidden rows. */
export interface OverviewRow {
  kind: 'overview'
  durationMs: number
  toolCount: number
  hidden: TranscriptNode[]
}

export type TranscriptRow =
  | { kind: 'node'; node: TranscriptNode }
  | OverviewRow

/**
 * Groups transcript nodes for display. A closed turn folds everything through
 * its last tool call (inclusive) into one "worked X · N tool calls" overview
 * row, keeping only the answer that follows it visible; genuine user messages
 * stay in place. An open (running) turn is left flat: tool calls render as
 * individual rows so the reader always sees live progress.
 */
export function groupTranscript(nodes: TranscriptNode[]): TranscriptRow[] {
  const rows: TranscriptRow[] = []
  let index = 0
  while (index < nodes.length) {
    const marker = nodes[index]
    if (marker.kind !== 'turn-start') {
      rows.push({ kind: 'node', node: marker })
      index += 1
      continue
    }
    let end = -1
    for (let j = index + 1; j < nodes.length; j++) {
      if (nodes[j].kind === 'turn-end') {
        end = j
        break
      }
    }
    if (end < 0) {
      for (const node of nodes.slice(index + 1)) {
        if (node.kind !== 'turn-start') rows.push({ kind: 'node', node })
      }
      break
    }
    rows.push(
      ...closedTurnRows(
        nodes.slice(index + 1, end),
        marker,
        nodes[end] as Extract<TranscriptNode, { kind: 'turn-end' }>,
      ),
    )
    rows.push({ kind: 'node', node: nodes[end] })
    index = end + 1
  }
  return rows
}

/** Fold one closed turn's prefix *through* its last tool call into an overview row. */
function closedTurnRows(
  span: TranscriptNode[],
  marker: Extract<TranscriptNode, { kind: 'turn-start' }>,
  endNode: Extract<TranscriptNode, { kind: 'turn-end' }>,
): TranscriptRow[] {
  let lastTool = -1
  for (let i = span.length - 1; i >= 0; i--) {
    if (span[i].kind === 'tool') {
      lastTool = i
      break
    }
  }
  if (lastTool < 0) return span.map((node) => ({ kind: 'node', node }) as TranscriptRow)
  // 折叠窗口含最后一条工具调用(用户规格:最后一条工具调用"以上"全部折叠),
  // 可见部分只剩其后的最终回答。
  const foldEnd = lastTool + 1
  const prefix = span.slice(0, foldEnd)
  const hidden = prefix.filter((node) => node.kind !== 'user')
  // The overview is inserted where the first folded row would sit; when there
  // is nothing expandable, no overview is shown at all.
  const overview: TranscriptRow | null = hidden.some(rendersContent)
    ? {
      kind: 'overview',
      durationMs: Math.max(0, endNode.time - marker.time),
      toolCount: span.reduce((count, node) => count + (node.kind === 'tool' ? 1 : 0), 0),
      hidden,
    }
    : null
  const rows: TranscriptRow[] = []
  let placed = false
  for (const node of prefix) {
    const foldable = node.kind !== 'user'
    if (!foldable || overview === null) {
      rows.push({ kind: 'node', node })
      continue
    }
    if (!placed) {
      rows.push(overview)
      placed = true
    }
  }
  if (overview !== null && !placed) rows.push(overview)
  rows.push(...span.slice(foldEnd).map((node) => ({ kind: 'node', node }) as TranscriptRow))
  return rows
}

/** Whether a hidden node would paint anything when expanded. */
function rendersContent(node: TranscriptNode): boolean {
  if (
    node.kind === 'tool' ||
    node.kind === 'user' ||
    node.kind === 'context-injection' ||
    node.kind === 'system-prompt'
  ) {
    return true
  }
  if (node.kind === 'assistant') {
    return (
      node.streaming ||
      node.interrupted ||
      node.blocks.some((block) => block.kind !== 'tool-call')
    )
  }
  return false
}
