import { t } from '../../i18n'

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
