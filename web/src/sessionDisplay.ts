import { t } from './i18n'
import type { SessionSummary } from './types'

/** Sidebar/header label: AI-generated title, then first-prompt excerpt, then blank. */
export function sessionDisplayTitle(session: SessionSummary): string {
  const title = session.title?.trim()
  if (title) return title
  const excerpt = session.excerpt?.trim()
  if (excerpt) return excerpt
  return t('blankSession')
}
