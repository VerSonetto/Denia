/** Shared model-catalog entry shape for settings editors. */

import type { WireProtocol } from './types'

/** 可选网关协议(UI 顺序 = most-used first),标签即 dsh 的三协议叫法。 */
export const WIRE_PROTOCOLS: readonly {
  id: WireProtocol
  label: string
  /** i18n 键:解释该协议适配的网关形态。 */
  hintKey: 'protocolCompletionsHint' | 'protocolResponsesHint' | 'protocolMessagesHint'
}[] = [
  {
    id: 'openai-completions',
    label: 'chat/completions',
    hintKey: 'protocolCompletionsHint',
  },
  {
    id: 'openai-responses',
    label: 'responses',
    hintKey: 'protocolResponsesHint',
  },
  {
    id: 'anthropic-messages',
    label: 'messages',
    hintKey: 'protocolMessagesHint',
  },
] as const

/** 协议展示标签;未知值按 chat/completions 兜底显示。 */
export function protocolLabel(protocol: WireProtocol | undefined): string {
  return WIRE_PROTOCOLS.find((entry) => entry.id === (protocol ?? 'openai-completions'))?.label ?? 'chat/completions'
}

export const REASONING_EFFORT_OFF = 'off' as const
export const REASONING_INTENSITY_IDS = ['low', 'medium', 'high', 'xhigh', 'max'] as const
export const REASONING_EFFORT_IDS = [REASONING_EFFORT_OFF, ...REASONING_INTENSITY_IDS] as const
export type ReasoningEffortId = (typeof REASONING_EFFORT_IDS)[number]
export type ReasoningIntensityId = (typeof REASONING_INTENSITY_IDS)[number]

export function isReasoningEffortId(value: string): value is ReasoningEffortId {
  return (REASONING_EFFORT_IDS as readonly string[]).includes(value)
}

const REASONING_EFFORT_RANK: Record<ReasoningEffortId, number> = {
  off: 0,
  low: 1,
  medium: 2,
  high: 3,
  xhigh: 4,
  max: 5,
}

/** 开启思考后的默认档位:仅「关」(思考但不选强度)。 */
export function defaultReasoningEfforts(): ReasoningEffortId[] {
  return [REASONING_EFFORT_OFF]
}

/** 从可用档位中选出最高强度;无强度档位时回退到「关」。 */
export function highestReasoningEffort(
  effortIds: readonly string[],
): ReasoningEffortId | undefined {
  const known = effortIds.filter((id): id is ReasoningEffortId => isReasoningEffortId(id))
  if (known.length === 0) return undefined
  return known.reduce((best, id) =>
    REASONING_EFFORT_RANK[id] > REASONING_EFFORT_RANK[best] ? id : best,
  )
}

/** 会话/选择器用的思考强度:保留当前值,否则默认最高档位。 */
export function resolveSessionReasoningEffort(
  efforts: { id: string }[],
  preferred?: string,
): string | undefined {
  const ids = efforts.map((effort) => effort.id)
  if (preferred && ids.includes(preferred)) return preferred
  return highestReasoningEffort(ids)
}

export interface CatalogModelEntry {
  id: string
  name?: string
  description?: string
  contextWindow?: number
  inputModalities?: string[]
  thinkingSupported?: boolean
  reasoningEfforts?: ReasoningEffortId[]
}

export function hasVision(entry: CatalogModelEntry): boolean {
  return (entry.inputModalities ?? ['text']).includes('image')
}

export function setVision(entry: CatalogModelEntry, enabled: boolean): CatalogModelEntry {
  const base = (entry.inputModalities ?? ['text']).filter((modality) => modality !== 'image')
  return {
    ...entry,
    inputModalities: enabled ? [...base, 'image'] : base.length > 0 ? base : ['text'],
  }
}

export function entryFromDiscovered(id: string, name?: string): CatalogModelEntry {
  return {
    id,
    name: name && name !== id ? name : undefined,
    inputModalities: ['text'],
    thinkingSupported: false,
  }
}

export function entryFromWire(raw: Record<string, unknown>): CatalogModelEntry {
  const reasoningEfforts = Array.isArray(raw.reasoningEfforts)
    ? raw.reasoningEfforts.filter(
        (value): value is ReasoningEffortId =>
          typeof value === 'string' && isReasoningEffortId(value),
      )
    : undefined
  return {
    id: String(raw.id ?? ''),
    name: typeof raw.name === 'string' ? raw.name : undefined,
    description: typeof raw.description === 'string' ? raw.description : undefined,
    contextWindow: typeof raw.contextWindow === 'number' ? raw.contextWindow : undefined,
    inputModalities: Array.isArray(raw.inputModalities)
      ? raw.inputModalities.filter((value): value is string => typeof value === 'string')
      : undefined,
    thinkingSupported:
      typeof raw.thinkingSupported === 'boolean' ? raw.thinkingSupported : undefined,
    reasoningEfforts: reasoningEfforts && reasoningEfforts.length > 0 ? reasoningEfforts : undefined,
  }
}

export function entryToWire(entry: CatalogModelEntry): Record<string, unknown> {
  const wire: Record<string, unknown> = { id: entry.id.trim() }
  if (entry.name?.trim()) wire.name = entry.name.trim()
  if (entry.description?.trim()) wire.description = entry.description.trim()
  if (entry.contextWindow && entry.contextWindow > 0) wire.contextWindow = entry.contextWindow
  if (entry.inputModalities && entry.inputModalities.length > 0) {
    wire.inputModalities = entry.inputModalities
  }
  if (entry.thinkingSupported !== undefined) wire.thinkingSupported = entry.thinkingSupported
  if (entry.thinkingSupported && entry.reasoningEfforts && entry.reasoningEfforts.length > 0) {
    wire.reasoningEfforts = entry.reasoningEfforts
  }
  return wire
}

export function formatContextWindow(value: number | undefined): string {
  if (!value) return '—'
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(value % 1_000_000 === 0 ? 0 : 1)}M`
  if (value >= 1_000) return `${Math.round(value / 1_000)}K`
  return String(value)
}
