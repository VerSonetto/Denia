import { t } from './i18n'
import type { SessionSummary } from './types'

/** Sidebar/header label: first-prompt excerpt, then blank. */
export function sessionDisplayTitle(session: SessionSummary): string {
  const excerpt = session.excerpt?.trim()
  if (excerpt) return excerpt
  return t('blankSession')
}
