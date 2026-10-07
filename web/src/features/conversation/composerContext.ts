import { t } from '../../i18n'
import { sessionDisplayTitle } from '../../sessionDisplay'
import type { SessionEnvelope, SessionSummary } from '../../types'
import type { TrajectoryQuote } from '../../trajectory'

export const CONTEXT_QUOTE_LIMIT = 64 * 1024
export const CONTEXT_QUOTES_TOTAL_LIMIT = 256 * 1024
const encoder = new TextEncoder()

export function sameWorkspacePath(a: string | null | undefined, b: string | null | undefined): boolean {
  if (!a || !b) return false
  const normalize = (path: string) => {
    const slashes = path.replace(/\\/g, '/')
    const normalized = slashes.replace(/\/+$/, '')
    return /^[a-z]:\//i.test(slashes) || slashes.startsWith('//') ? normalized.toLowerCase() : normalized
  }
  return normalize(a) === normalize(b)
}

export function utf8Prefix(text: string, limit: number): string {
  let bytes = 0
  let end = 0
  for (const character of text) {
    const size = encoder.encode(character).length
    if (bytes + size > limit) break
    bytes += size
    end += character.length
  }
  return text.slice(0, end)
}

function utf8Suffix(text: string, limit: number): string {
  const characters = Array.from(text)
  let bytes = 0
  let start = characters.length
  while (start > 0) {
    const size = encoder.encode(characters[start - 1]).length
    if (bytes + size > limit) break
    bytes += size
    start -= 1
  }
  return characters.slice(start).join('')
}

/** Only settled dialogue is quoted; system prompts, tool payloads and reasoning stay private. */
export function buildSessionContextQuote(
  session: SessionSummary,
  events: SessionEnvelope[],
  hasMoreBefore: boolean,
): TrajectoryQuote {
  const messages = events.flatMap((event) => {
    if (event.type === 'user-message' && !event.injected && event.text.trim()) {
      return [`### ${t('contextUser')} · seq=${event.seq}\n${event.text}`]
    }
    if (event.type === 'assistant-message') {
      const text = event.blocks.filter(block => block.type === 'text').map(block => block.text).join('\n')
      return text.trim() ? [`### ${t('contextAssistant')} · seq=${event.seq}\n${text}`] : []
    }
    if (event.type === 'compaction-summary' && event.summary.trim()) {
      return [`### ${t('contextSummary')} · seq=${event.seq}\n${event.summary}`]
    }
    return []
  })
  if (!messages.length) throw new Error(t('contextSessionEmpty'))
  const title = utf8Prefix(`${t('contextSessionQuote')}: ${sessionDisplayTitle(session)}`, 200)
  const metadata = `${title}\nSession: ${session.id}\nWorkspace: ${session.cwd ?? ''}\n\n`
  const body = messages.join('\n\n')
  const truncated = hasMoreBefore || encoder.encode(metadata + body).length > CONTEXT_QUOTE_LIMIT
  const notice = truncated ? `${t('contextSessionTruncated')}\n\n` : ''
  const budget = CONTEXT_QUOTE_LIMIT - encoder.encode(metadata + notice).length
  const text = metadata + notice + utf8Suffix(body, Math.max(0, budget))
  return { id: `session:${session.id}`, title, text }
}

export function validateContextQuotes(quotes: TrajectoryQuote[]): void {
  let total = 0
  for (const quote of quotes) {
    const bytes = encoder.encode(quote.text.trim()).length
    if (!bytes || bytes > CONTEXT_QUOTE_LIMIT || encoder.encode(quote.title.trim()).length > 200) {
      throw new Error(t('contextQuoteTooLarge'))
    }
    total += bytes
  }
  if (total > CONTEXT_QUOTES_TOTAL_LIMIT) throw new Error(t('contextQuotesTooLarge'))
}
