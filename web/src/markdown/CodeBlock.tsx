import { Fragment, useCallback, useMemo, useRef, useState, useSyncExternalStore } from 'react'
import type { ReactNode } from 'react'
import {
  StreamingHighlightSession, grammarLoadCount, highlightToHtml, subscribeGrammarLoaded,
} from './highlight'
import type { HighlightSpan } from './highlight'

export interface CodeBlockProps {
  /** The source text, rendered verbatim (trailing newline trimmed for display). */
  code: string
  /** Grammar hint (markdown fence info string or a fixed caller id); unknown = plain. */
  lang?: string | undefined
  /**
   * The code is still growing (a streaming markdown fence): highlight through
   * a per-instance {@link StreamingHighlightSession}, which re-tokenizes only
   * appended text and keeps completed lines' elements (and DOM) untouched.
   */
  streaming?: boolean | undefined
  /** Copy-button idle label; the owner passes localized copy. */
  copyLabel: string
  /** Copy-button label during the post-copy confirmation window. */
  copiedLabel: string
}

/**
 * The `pre` attributes shiki's HTML arm emits for the css-variables theme,
 * mirrored so the streaming arm's tree is interchangeable with the settled arm.
 */
const SHIKI_PRE_PROPS = {
  className: 'shiki css-variables',
  style: { backgroundColor: 'var(--shiki-background)', color: 'var(--shiki-foreground)' },
  tabIndex: 0,
} as const

async function writeClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    return false
  }
}

export function CodeBlock({ code, lang, streaming, copyLabel, copiedLabel }: CodeBlockProps) {
  const trimmed = code.endsWith('\n') ? code.slice(0, -1) : code
  // Re-render when a lazy grammar finishes loading, so a fence that showed plain
  // text while its language's grammar imported picks up highlighting.
  const loaded = useSyncExternalStore(subscribeGrammarLoaded, grammarLoadCount, grammarLoadCount)
  const html = useMemo(
    () => (streaming === true ? undefined : highlightToHtml(trimmed, lang)),
    [streaming, trimmed, lang, loaded],
  )
  // Streaming state lives in refs mutated inside the memo: the session's caches
  // carry across chunks only because the owner keys this instance stably.
  const sessionRef = useRef<StreamingHighlightSession | null>(null)
  const lineCacheRef = useRef<{ lines: readonly HighlightSpan[][]; elements: ReactNode[] } | null>(null)
  const streamedBody = useMemo(() => {
    if (streaming !== true) {
      sessionRef.current = null
      lineCacheRef.current = null
      return undefined
    }
    sessionRef.current ??= new StreamingHighlightSession()
    const lines = sessionRef.current.update(trimmed, lang)
    if (lines === undefined) {
      lineCacheRef.current = null
      return undefined
    }
    // A retained line keeps its span-array identity across chunks, so its
    // cached element is reused and React leaves that line's DOM untouched.
    const previous = lineCacheRef.current
    const elements = lines.map((line, index) => previous !== null && previous.lines[index] === line
      ? previous.elements[index]
      : (
        <Fragment key={index}>
          {index > 0 && '\n'}
          <span className="line">
            {line.map((span, spanIndex) => <span key={spanIndex} style={span.style}>{span.text}</span>)}
          </span>
        </Fragment>
      ))
    lineCacheRef.current = { lines, elements }
    return <pre {...SHIKI_PRE_PROPS}><code>{elements}</code></pre>
  }, [streaming, trimmed, lang, loaded])
  const rootRef = useRef<HTMLDivElement>(null)
  const [copied, setCopied] = useState(false)

  const onCopy = useCallback(() => {
    if (copied) return
    const text = rootRef.current?.querySelector('pre')?.textContent ?? trimmed
    void writeClipboard(text).then((ok) => {
      if (!ok) return
      setCopied(true)
      window.setTimeout(() => { setCopied(false) }, 1000)
    })
  }, [copied, trimmed])

  // shiki's HTML output is a static span tree it generated from `code` (no
  // user HTML passes through), the sanctioned innerHTML consumption path per
  // shiki's own docs.
  const body = streamedBody !== undefined
    ? streamedBody
    : html === undefined
      ? (
        <pre className="md-code-plain"><code>{trimmed}</code></pre>
      )
      : (
        <div dangerouslySetInnerHTML={{ __html: html }} />
      )

  return (
    <div ref={rootRef} className="md-code-block">
      <div className="md-code-banner-wrap">
        <div className="md-code-banner">
          <div className="md-code-infostring">{lang ?? ''}</div>
          <div className="md-code-action">
            <button type="button" className="md-code-copy" onClick={onCopy}>
              {copied ? copiedLabel : copyLabel}
            </button>
          </div>
        </div>
      </div>
      {body}
    </div>
  )
}