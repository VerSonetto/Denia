import type { SessionEnvelope } from './types'

export const SESSION_PAGE_LIMIT = 200
export const LIVE_EVENT_LIMIT = 600

export function appendSessionEvents(events: SessionEnvelope[], batch: SessionEnvelope[]): void {
  const settled = new Map<string, number>()
  const closedTurns = new Map<number, number>()
  for (const envelope of batch) {
    events.push(envelope)
    if (envelope.type === 'assistant-message') settled.set(`${envelope.turn}:${envelope.step}`, envelope.seq)
    if (envelope.type === 'turn-end') closedTurns.set(envelope.turn, envelope.seq)
  }
  if (closedTurns.size === 0 && settled.size === 0) return
  let writeIndex = 0
  for (const envelope of events) {
    if (envelope.type === 'assistant-chunk' &&
      (envelope.seq < (closedTurns.get(envelope.turn) ?? 0) ||
        envelope.seq < (settled.get(`${envelope.turn}:${envelope.step}`) ?? 0))) continue
    events[writeIndex++] = envelope
  }
  events.length = writeIndex
}

export function liveWindowStart(events: SessionEnvelope[]): number {
  if (events.length <= LIVE_EVENT_LIMIT) return 0
  const boundary = events.length - SESSION_PAGE_LIMIT
  for (let index = boundary; index > 0; index--) {
    const envelope = events[index]
    if (envelope.type === 'user-message' && !envelope.injected) return index
  }
  return 0
}
