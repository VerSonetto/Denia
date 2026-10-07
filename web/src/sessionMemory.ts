import type { SessionEnvelope } from './types'

export const SESSION_PAGE_LIMIT = 200
export const LIVE_EVENT_LIMIT = 600

/**
 * 高频流式帧:只服务实时观感,不占分页位次、不触发历史复制、结算后从内存
 * 回收。
 *
 * 与后端 `is_transient_event`(`crates/session/src/lib.rs`)是同一份名单,
 * 两边必须同步:后端漏登记会让增量帧混进分页窗口,前端漏登记会把它们当成
 * 持久事件计数——表现为会话翻页总数在命令运行期间自己往上飘。
 */
export function isTransientEvent(envelope: SessionEnvelope): boolean {
  return envelope.type === 'assistant-chunk' || envelope.type === 'tool-output-chunk'
}

export function appendSessionEvents(events: SessionEnvelope[], batch: SessionEnvelope[]): void {
  const settled = new Map<string, number>()
  const closedTurns = new Map<number, number>()
  for (const envelope of batch) {
    events.push(envelope)
    if (envelope.type === 'assistant-message') settled.set(`${envelope.turn}:${envelope.step}`, envelope.seq)
    if (envelope.type === 'turn-end') closedTurns.set(envelope.turn, envelope.seq)
  }
  // 清扫判据只看**本批**:每帧都全量过一遍缓冲会把实时流的每帧成本从
  // O(1) 拉到 O(n),长会话越跑越卡。工具结果到达是唯一需要额外触发清扫的
  // 事件(它让对应的实时输出增量作废)。
  const answeredSomething = batch.some((envelope) => envelope.type === 'tool-result')
  if (closedTurns.size === 0 && settled.size === 0 && !answeredSomething) return
  // 清扫对象取自整份缓冲(这一遍本来就是全量):增量的有效窗口是
  // tool-call → tool-result,此刻缓冲里凡是已有结果的调用,其实时增量
  // 都不再有意义 —— 正文以结果为准。
  const answered = new Set<string>()
  for (const envelope of events) {
    if (envelope.type === 'tool-result') answered.add(envelope.call_id)
  }
  let writeIndex = 0
  for (const envelope of events) {
    if (envelope.type === 'assistant-chunk' &&
      (envelope.seq < (closedTurns.get(envelope.turn) ?? 0) ||
        envelope.seq < (settled.get(`${envelope.turn}:${envelope.step}`) ?? 0))) continue
    if (envelope.type === 'tool-output-chunk' && answered.has(envelope.call_id)) continue
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
