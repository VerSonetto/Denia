/**
 * 会话行尾相对时间的分桶:同一时刻在任何界面上措辞一致;单位词由
 * i18n 字典渲染,now 桶没有数量词。
 */

export type RelativeTimeUnit = 'now' | 'minutes' | 'hours' | 'days' | 'months' | 'years'

export interface RelativeTime {
  unit: RelativeTimeUnit
  n: number
}

/** epoch ms → 分桶结果;at 晚于 now(时钟偏差)按 0 处理。 */
export function relativeTime(at: number, now: number): RelativeTime {
  const MIN = 60_000
  const HOUR = 3_600_000
  const DAY = 86_400_000
  const diff = Math.max(0, now - at)
  if (diff < MIN) return { unit: 'now', n: 0 }
  if (diff < HOUR) return { unit: 'minutes', n: Math.floor(diff / MIN) }
  if (diff < DAY) return { unit: 'hours', n: Math.floor(diff / HOUR) }
  if (diff < 30 * DAY) return { unit: 'days', n: Math.floor(diff / DAY) }
  if (diff < 365 * DAY) return { unit: 'months', n: Math.floor(diff / (30 * DAY)) }
  return { unit: 'years', n: Math.floor(diff / (365 * DAY)) }
}
