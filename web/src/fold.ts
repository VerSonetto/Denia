import type { ContentBlock, SessionEnvelope, StreamChunk, TokenUsage, TurnEndReason } from './types'

/** One rendered block inside an assistant message. */
export interface UiBlock {
  kind: 'text' | 'reasoning' | 'tool-call'
  text: string
  id?: string
  name?: string
  args?: string
}

export type TranscriptNode =
  | { kind: 'user'; text: string; injected?: boolean }
  | {
      kind: 'assistant'
      turn: number
      step: number
      blocks: UiBlock[]
      usage?: TokenUsage
      interrupted: boolean
      streaming: boolean
    }
  | {
      kind: 'tool'
      callId: string
      name: string
      args: string
      result?: { content: string; isError: boolean }
    }
  | { kind: 'turn-start'; turn: number; time: number }
  | {
      kind: 'turn-end'
      turn: number
      time: number
      reason: TurnEndReason
      usage?: TokenUsage
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
    case 'block-start':
      return [...blocks, { kind: chunk.block_type, text: '' }]
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
      case 'turn-start':
        closeOpen()
        nodes.push({ kind: 'turn-start', turn: event.turn, time: event.time })
        break
      case 'user-message':
        closeOpen()
        nodes.push({
          kind: 'user',
          text: event.text,
          ...event.injected ? { injected: true } : {},
        })
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
          closeOpen()
        } else {
          closeOpen()
          nodes.push({
            kind: 'assistant',
            turn: event.turn,
            step: event.step,
            blocks,
            usage: event.usage,
            interrupted: event.interrupted ?? false,
            streaming: false,
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
        }
        tools.set(event.call_id, node)
        nodes.push(node)
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
            result,
          })
        }
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

/**
 * Incremental fold: applies one envelope to an existing node list without
 * refolding the prefix. Only the affected tail node is copied, so a live
 * stream costs O(1) per envelope instead of O(n).
 */
export function applyEnvelope(
  nodes: TranscriptNode[],
  event: SessionEnvelope,
): TranscriptNode[] {
  switch (event.type) {
    case 'turn-start':
      return [...nodes, { kind: 'turn-start', turn: event.turn, time: event.time }]
    case 'user-message':
      return [...nodes, { kind: 'user', text: event.text, injected: event.injected }]
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
      }
      return [...nodes, { ...fresh, blocks: applyChunk(fresh.blocks, event.chunk) }]
    }
    case 'assistant-message': {
      const settled: TranscriptNode = {
        kind: 'assistant',
        turn: event.turn,
        step: event.step,
        blocks: event.blocks.map(toUiBlock),
        usage: event.usage,
        interrupted: event.interrupted ?? false,
        streaming: false,
      }
      const last = nodes[nodes.length - 1]
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
    case 'tool-call':
      return [
        ...nodes,
        { kind: 'tool', callId: event.call_id, name: event.name, args: event.arguments },
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
 * Groups transcript nodes for display. A closed turn folds everything before
 * its last tool call into one "worked X · N tool calls" overview row, keeping
 * the last tool call and the answer that follows it visible; genuine user
 * messages stay in place. An open (running) turn is left flat: tool calls
 * render as individual rows so the reader always sees live progress.
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

/** Fold one closed turn's prefix before its last tool call into an overview row. */
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
  const prefix = span.slice(0, lastTool)
  const hidden = prefix.filter((node) => !(node.kind === 'user' && !node.injected))
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
    const foldable = !(node.kind === 'user' && !node.injected)
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
  rows.push(...span.slice(lastTool).map((node) => ({ kind: 'node', node }) as TranscriptRow))
  return rows
}

/** Whether a hidden node would paint anything when expanded. */
function rendersContent(node: TranscriptNode): boolean {
  if (node.kind === 'tool' || node.kind === 'user') return true
  if (node.kind === 'assistant') {
    return (
      node.streaming ||
      node.interrupted ||
      node.blocks.some((block) => block.kind !== 'tool-call')
    )
  }
  return false
}
