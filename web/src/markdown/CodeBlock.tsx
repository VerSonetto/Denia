import { Fragment, useCallback, useMemo, useRef, useState, useSyncExternalStore } from 'react'
import type { ReactNode } from 'react'
import {
  getHighlightModule, highlightRevision, subscribeHighlightLoaded,
} from './highlightLoader'
import type { HighlightSpan, HighlightTheme, StreamingHighlightSession } from './highlight'
import { withinHighlightBudget } from './highlightBudget'

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
  /** Optional Shiki theme override for panels that follow the app theme. */
  theme?: HighlightTheme
  /** Render each settled source line separately, for editor-like gutters. */
  lineNumbers?: boolean
}

/**
 * The `pre` attributes shiki's HTML arm emits, mirrored so the streaming arm's
 * tree is interchangeable with the settled arm. The class name must stay in sync
 * with the theme name in `highlight.ts` (shiki stamps it onto the `<pre>` it
 * generates, and the CSS in `markdown.css` keys off it).
 */
const SHIKI_PRE_STYLE = { backgroundColor: 'var(--shiki-background)', color: 'var(--shiki-foreground)' } as const
const SHIKI_PRE_PROPS = {
  className: 'shiki vitesse-dark',
  style: SHIKI_PRE_STYLE,
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

export function CodeBlock({ code, lang, streaming, copyLabel, copiedLabel, theme = 'vitesse-dark', lineNumbers = false }: CodeBlockProps) {
  const trimmed = code.endsWith('\n') ? code.slice(0, -1) : code
  // Re-render when a lazy grammar finishes loading, so a fence that showed plain
  // text while its language's grammar imported picks up highlighting.
  const highlightable = useMemo(() => withinHighlightBudget(trimmed), [trimmed])
  const loaded = useSyncExternalStore(subscribeHighlightLoaded, highlightRevision, highlightRevision)
  const html = useMemo(
    () => (streaming === true || lineNumbers || !highlightable ? undefined : getHighlightModule(lang)?.highlightToHtml(trimmed, lang, theme)),
    [streaming, trimmed, lang, loaded, highlightable, theme, lineNumbers],
  )
  const numberedLines = useMemo(
    () => (lineNumbers && streaming !== true && highlightable
      ? getHighlightModule(lang)?.highlightToLines(trimmed, lang, theme)
      : undefined),
    [lineNumbers, streaming, highlightable, lang, trimmed, theme, loaded],
  )
  // Streaming state lives in refs mutated inside the memo: the session's caches
  // carry across chunks only because the owner keys this instance stably.
  const sessionRef = useRef<StreamingHighlightSession | null>(null)
  const lineCacheRef = useRef<{ lines: readonly HighlightSpan[][]; elements: ReactNode[] } | null>(null)
  const streamedBody = useMemo(() => {
    if (streaming !== true || !highlightable) {
      sessionRef.current = null
      lineCacheRef.current = null
      return undefined
    }
    const highlighter = getHighlightModule(lang)
    if (!highlighter) {
      sessionRef.current = null
      lineCacheRef.current = null
      return undefined
    }
    sessionRef.current ??= new highlighter.StreamingHighlightSession()
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
  }, [streaming, trimmed, lang, loaded, highlightable])
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
    : numberedLines !== undefined
      ? (
        <pre className={`shiki ${theme}`} style={SHIKI_PRE_STYLE} tabIndex={0}>
          <code>{numberedLines.map((line, index) => (
            <Fragment key={index}>
              {index > 0 && '\n'}
              <span className="line">
                {line.map((span, spanIndex) => <span key={spanIndex} style={span.style}>{span.text}</span>)}
              </span>
            </Fragment>
          ))}</code>
        </pre>
      )
    : html === undefined
      ? (
        <pre className="md-code-plain"><code>{trimmed}</code></pre>
      )
      : (
        <div className={`md-code-shiki md-code-shiki-${theme}`} dangerouslySetInnerHTML={{ __html: html }} />
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
