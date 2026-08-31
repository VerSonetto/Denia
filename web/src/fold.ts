import type { ContentBlock, SessionEnvelope, TokenUsage, TurnEndReason } from './types'

/** One rendered block inside an assistant message. */
export interface UiBlock {
  kind: 'text' | 'reasoning' | 'tool-call'
  text: string
  id?: string
  name?: string
  args?: string
}

export type TranscriptNode =
  | { kind: 'user'; text: string }
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
  | {
      kind: 'turn-end'
      turn: number
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
  const blockAt = (index: number): UiBlock => {
    const assistant = open!
    while (assistant.blocks.length <= index) {
      assistant.blocks.push({ kind: 'text', text: '' })
    }
    return assistant.blocks[index]
  }

  for (const event of events) {
    switch (event.type) {
      case 'user-message':
        closeOpen()
        nodes.push({ kind: 'user', text: event.text })
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
        const chunk = event.chunk
        switch (chunk.type) {
          case 'block-start':
            open.blocks.push({ kind: chunk.block_type, text: '' })
            break
          case 'text-delta':
          case 'reasoning-delta':
            blockAt(chunk.index).text += chunk.text
            break
          case 'tool-call-delta': {
            const block = blockAt(chunk.index)
            block.args = (block.args ?? '') + chunk.arguments_delta
            if (chunk.id) block.id = chunk.id
            if (chunk.name) block.name = chunk.name
            break
          }
          case 'block-end': {
            const settled = toUiBlock(chunk.block)
            open.blocks[chunk.index] = { ...settled, text: settled.text || blockAt(chunk.index).text }
            break
          }
          case 'usage':
          case 'finish':
            break
        }
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
    case 'user-message':
      return [...nodes, { kind: 'user', text: event.text }]
    case 'assistant-chunk': {
      const last = nodes[nodes.length - 1]
      if (
        last?.kind === 'assistant' &&
        last.streaming &&
        last.turn === event.turn &&
        last.step === event.step
      ) {
        const blocks = last.blocks.slice()
        const chunk = event.chunk
        switch (chunk.type) {
          case 'block-start':
            blocks.push({ kind: chunk.block_type, text: '' })
            break
          case 'text-delta':
          case 'reasoning-delta': {
            const block = blocks[chunk.index] ?? { kind: 'text' as const, text: '' }
            blocks[chunk.index] = { ...block, text: block.text + chunk.text }
            break
          }
          case 'tool-call-delta': {
            const block = blocks[chunk.index] ?? { kind: 'tool-call' as const, text: '' }
            blocks[chunk.index] = {
              ...block,
              args: (block.args ?? '') + chunk.arguments_delta,
              id: chunk.id || block.id,
              name: chunk.name ?? block.name,
            }
            break
          }
          case 'block-end': {
            const settled = toUiBlock(chunk.block)
            const previous = blocks[chunk.index]
            blocks[chunk.index] = {
              ...settled,
              text: settled.text || previous?.text || '',
            }
            break
          }
          default:
            break
        }
        return [...nodes.slice(0, -1), { ...last, blocks }]
      }
      return [
        ...nodes,
        {
          kind: 'assistant',
          turn: event.turn,
          step: event.step,
          blocks: [],
          interrupted: false,
          streaming: true,
        },
      ]
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
