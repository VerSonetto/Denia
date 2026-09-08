import { useEffect, useState } from 'react'
import {
  browserCommand,
  reconBreakpoints,
  reconClear,
  reconConsole,
  reconEval,
  reconPaused,
  reconRemoveBreakpoint,
  reconResume,
  reconSearch,
  reconSetBreakpoint,
  reconStep,
  subscribeBrowserEvents,
} from '../browserApi'
import type {
  BrowserEventFrame,
  BrowserOutcome,
  ReconBreakpoint,
  ReconConsoleEntry,
  ReconMatch,
  ReconPaused,
} from '../types'
import { t } from '../i18n'

/**
 * JS 逆向工作台:断点暂停调试 / 源码检索 / 请求抓包 / 控制台消息。
 * 挂在浏览器面板底部;依赖 recon REST 端点(浏览器须已运行)。
 */
export default function ReconPanel({ tabId }: { tabId: string | null }) {
  const [open, setOpen] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const [paused, setPaused] = useState<ReconPaused | null>(null)
  const [frameIndex, setFrameIndex] = useState(0)
  const [evalExpr, setEvalExpr] = useState('')
  const [evalResult, setEvalResult] = useState<string | null>(null)
  const [bannerFor, setBannerFor] = useState<string | null>(null)

  const [searchQuery, setSearchQuery] = useState('')
  const [searchFilter, setSearchFilter] = useState('')
  const [isRegex, setIsRegex] = useState(false)
  const [caseSensitive, setCaseSensitive] = useState(false)
  const [searching, setSearching] = useState(false)
  const [matches, setMatches] = useState<ReconMatch[]>([])
  const [skippedLarge, setSkippedLarge] = useState(0)

  const [breakpoints, setBreakpoints] = useState<ReconBreakpoint[]>([])

  const [requests, setRequests] = useState<Array<Record<string, unknown>>>([])
  const [reqFilter, setReqFilter] = useState('')
  const [expanded, setExpanded] = useState<string | null>(null)
  const [bodyOf, setBodyOf] = useState<{ requestId: string; body: unknown; kind: string } | null>(null)

  const [consoleMsgs, setConsoleMsgs] = useState<ReconConsoleEntry[]>([])

  // 每次事件刷新轮询数据(轻量;暂停态被事件驱动)。
  const refreshBreakpoints = (id: string) => {
    void reconBreakpoints(id).then((r) => setBreakpoints(r.breakpoints)).catch(() => {})
  }
  const refreshConsole = (id: string) => {
    void reconConsole(id, 100).then((r) => setConsoleMsgs(r.messages)).catch(() => {})
  }
  const refreshRequests = async (_id: string) => {
    try {
      // 浏览器没跑时后端原地返回空列表(不拉起实例):这里照常收敛为空态。
      const outcome = (await browserCommand({ method: 'networkList', max: 100 })) as BrowserOutcome
      setRequests(((outcome.value as { requests?: unknown[] })?.requests ?? []) as Array<Record<string, unknown>>)
    } catch {
      /* 传输错误等,忽略 */
    }
  }

  const doRefresh = (id: string) => {
    refreshBreakpoints(id)
    refreshConsole(id)
    refreshRequests(id)
  }

  useEffect(() => {
    if (!tabId || !open) return
    doRefresh(tabId)
    // 控制台跟随:打开时 2s 轮询(高频日志事件不入 SSE,避免刷屏)。
    const timer = window.setInterval(() => {
      if (!tabId) return
      void reconConsole(tabId, 100).then((r) => setConsoleMsgs(r.messages)).catch(() => {})
    }, 2000)
    return () => window.clearInterval(timer)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tabId, open])

  // 事件驱动:暂停/恢复横幅 + 断点变化刷新。
  useEffect(() => {
    let disposed = false
    const close = subscribeBrowserEvents((event: BrowserEventFrame) => {
      if (disposed) return
      if (event.type === 'debugger-paused' && event.tabId === tabId) {
        void reconPaused(tabId).then((r) => {
          if (!disposed) {
            setPaused(r.paused)
            setFrameIndex(0)
            setEvalResult(null)
          }
        }).catch(() => {})
        setBannerFor(t(`reconPausedBanner`))
        void refreshBreakpoints(tabId)
      } else if (event.type === 'debugger-resumed' && event.tabId === tabId) {
        setPaused(null)
        setBannerFor(null)
      } else if (event.type === 'tabs-changed' || event.type === 'navigated') {
        if (tabId) void refreshRequests(tabId)
      }
    })
    return () => {
      disposed = true
      close()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tabId])

  if (!tabId) return null

  const guard = (action: () => Promise<unknown>): Promise<unknown> =>
    action().catch((err: unknown) => setError(String(err instanceof Error ? err.message : err)))

  const runSearch = () => {
    const query = searchQuery.trim()
    if (!query || searching) return
    setSearching(true)
    setError(null)
    void guard(async () => {
      const r = await reconSearch(tabId, query, {
        isRegex,
        caseSensitive,
        urlFilter: searchFilter.trim() || undefined,
        maxResults: 40,
      })
      setMatches(r.matches)
      setSkippedLarge(r.skippedLarge)
    }).finally(() => setSearching(false))
  }

  const setBp = (match: ReconMatch) => {
    void guard(async () => {
      await reconSetBreakpoint(tabId, match.lineContent.trim(), {
        urlFilter: match.url || undefined,
        occurrence: 1,
      })
      await refreshBreakpoints(tabId)
    })
  }

  const removeBp = (id: string) => {
    void guard(async () => {
      await reconRemoveBreakpoint(tabId, id)
      setBreakpoints((list) => list.filter((bp) => bp.breakpointId !== id))
    })
  }

  const doEval = () => {
    const expr = evalExpr.trim()
    if (!expr) return
    setEvalResult(null)
    void guard(async () => {
      const r = await reconEval(tabId, expr, frameIndex)
      setEvalResult(typeof r.value === 'string' ? r.value : JSON.stringify(r.value, null, 2))
    })
  }

  const doStep = (direction: 'over' | 'into' | 'out') => {
    setEvalResult(null)
    void guard(() => reconStep(tabId, direction))
  }

  const doResume = () => {
    setEvalResult(null)
    void guard(() => reconResume(tabId))
  }

  const expandRequest = async (requestId: string) => {
    if (expanded === requestId) {
      setExpanded(null)
      setBodyOf(null)
      return
    }
    setExpanded(requestId)
    setBodyOf(null)
  }

  const fetchBody = async (requestId: string, kind: 'response' | 'request') => {
    try {
      const outcome = (await browserCommand({ method: 'networkGetBody', requestId, bodyKind: kind })) as BrowserOutcome
      if (outcome.ok && outcome.value) {
        setBodyOf({ requestId, body: outcome.value, kind })
      } else {
        setBodyOf({ requestId, body: { error: outcome.error?.message ?? 'failed' }, kind })
      }
    } catch (err) {
      setBodyOf({ requestId, body: { error: String(err) }, kind })
    }
  }

  const clearAll = () => {
    void guard(async () => {
      await reconClear(tabId)
      setRequests([])
      setMatches([])
      setConsoleMsgs([])
    })
  }

  const formatHeaders = (headers: unknown): string => {
    if (!headers || typeof headers !== 'object') return '—'
    return Object.entries(headers as Record<string, unknown>)
      .map(([key, value]) => `${key}: ${value}`)
      .join('\n')
  }

  const initiatorText = (entry: Record<string, unknown>) => {
    const initiator = entry.initiator as { type?: string; url?: string; functionName?: string; stack?: unknown[] } | undefined
    if (!initiator) return t('reconNoInitiator')
    const frames = (initiator.stack ?? []).map((frame) => {
      const f = frame as { functionName?: string; url?: string; lineNumber?: number }
      return `  ${f.functionName || '(匿名)'} @ ${f.url || '(未知)'}:${(f.lineNumber ?? 0) + 1}`
    })
    return `${initiator.type ?? ''}${frames.length ? `\n${frames.join('\n')}` : ''}`.trim() || t('reconNoInitiator')
  }

  const statusClass = (status: unknown) => {
    if (status == null) return ''
    const code = Number(status)
    if (code >= 400) return 'recon-bad'
    if (code >= 300) return 'recon-redirect'
    return ''
  }

  return (
    <div className="recon-panel">
      <div className="recon-header">
        <button type="button" className="recon-toggle" onClick={() => setOpen((on) => !on)} title={t('reconTip')}>
          {open ? '▾' : '▸'} {t('reconToggle')}
        </button>
        {bannerFor && <span className="recon-banner">{bannerFor}</span>}
        <button type="button" className="recon-clear" onClick={() => void clearAll()} title={t('reconClear')}>
          {t('reconClear')}
        </button>
      </div>
      {error && <div className="recon-error">{error}</div>}
      {!open ? null : (
        <div className="recon-body">
          {/* ---- 断点暂停控制 ---- */}
          {paused && (
            <div className="recon-section recon-paused">
              <div className="recon-section-title">
                {t('reconPaused')} · {t('reconPausedReason')}: {paused.reason}
              </div>
              <div className="recon-frames">
                {paused.frames.slice(0, 12).map((frame, index) => (
                  <button
                    key={frame.callFrameId}
                    type="button"
                    className={`recon-frame${index === frameIndex ? ' active' : ''}`}
                    onClick={() => setFrameIndex(index)}
                    title={frame.scopeNames.join(', ')}
                  >
                    {t('reconFrame', { fn: frame.functionName, url: frame.url || '(内联)', line: (frame.lineNumber ?? 0) + 1 })}
                  </button>
                ))}
              </div>
              <div className="recon-eval-row">
                <input
                  value={evalExpr}
                  onChange={(event) => setEvalExpr(event.target.value)}
                  placeholder={t('reconEvalHint')}
                  onKeyDown={(event) => event.key === 'Enter' && doEval()}
                />
                <button type="button" onClick={() => void doEval()}>{t('reconEvalBtn')}</button>
                <button type="button" onClick={() => void doStep('over')}>{t('reconStepOver')}</button>
                <button type="button" onClick={() => void doStep('into')}>{t('reconStepInto')}</button>
                <button type="button" onClick={() => void doStep('out')}>{t('reconStepOut')}</button>
                <button type="button" className="recon-primary" onClick={() => void doResume()}>{t('reconResume')}</button>
              </div>
              {evalResult != null && <pre className="recon-eval-result">{evalResult}</pre>}
              <div className="recon-muted">{t('reconTabPaused')}</div>
            </div>
          )}

          {/* ---- 源码检索 + 断点 ---- */}
          <div className="recon-section">
            <div className="recon-section-title">{t('reconSearch')}</div>
            <div className="recon-search-row">
              <input
                value={searchQuery}
                onChange={(event) => setSearchQuery(event.target.value)}
                placeholder={t('reconSearchHint')}
                onKeyDown={(event) => event.key === 'Enter' && runSearch()}
              />
              <input
                value={searchFilter}
                onChange={(event) => setSearchFilter(event.target.value)}
                placeholder={t('reconFilter')}
                className="recon-filter-input"
              />
              <label className="recon-check">
                <input type="checkbox" checked={isRegex} onChange={(event) => setIsRegex(event.target.checked)} />
                {t('reconRegex')}
              </label>
              <label className="recon-check">
                <input type="checkbox" checked={caseSensitive} onChange={(event) => setCaseSensitive(event.target.checked)} />
                {t('reconCaseSensitive')}
              </label>
              <button type="button" className="recon-primary" onClick={() => void runSearch()} disabled={searching}>
                {searching ? t('reconSearching') : t('reconSearchBtn')}
              </button>
            </div>
            {matches.length > 0 && (
              <div className="recon-muted">
                {t('reconResultCount', { count: matches.length })}
                {skippedLarge > 0 ? ` ${t('reconSkippedLarge', { count: skippedLarge })}` : ''}
              </div>
            )}
            {searching ? null : matches.length === 0 && searchQuery.trim() ? (
              <div className="recon-muted">{t('reconNoResult')}</div>
            ) : null}
            <div className="recon-matches">
              {matches.slice(0, 30).map((match, index) => (
                <div key={`${match.scriptId}:${match.lineNumber}:${index}`} className="recon-match">
                  <span className="recon-match-meta">
                    {(match.lineNumber ?? 0) + 1} · {match.url || '(内联)'}
                  </span>
                  <code className="recon-match-code">{match.lineContent.slice(0, 140)}</code>
                  <button type="button" className="recon-mini" title={t('reconSetBreakpoint')} onClick={() => void setBp(match)}>
                    ● {t('reconSetBreakpoint')}
                  </button>
                </div>
              ))}
            </div>
            <div className="recon-section-title recon-bp-title">{t('reconBreakpoints')}</div>
            {breakpoints.length === 0 ? (
              <div className="recon-muted">{t('reconNoBreakpoints')}</div>
            ) : (
              <div className="recon-bp-list">
                {breakpoints.map((bp) => (
                  <div key={bp.breakpointId} className="recon-bp">
                    <span title={bp.condition ?? ''}>
                      {(bp.lineNumber ?? 0) + 1} · {bp.url}
                      {bp.condition ? ` if ${bp.condition}` : ''}
                    </span>
                    <button type="button" className="recon-mini" onClick={() => void removeBp(bp.breakpointId)}>
                      × {t('reconRemoveBreakpoint')}
                    </button>
                  </div>
                ))}
              </div>
            )}
          </div>

          {/* ---- 请求抓包 ---- */}
          <div className="recon-section">
            <div className="recon-section-title">{t('reconRequests')}</div>
            <div className="recon-search-row">
              <input
                value={reqFilter}
                onChange={(event) => setReqFilter(event.target.value)}
                placeholder={t('reconFilter')}
                className="recon-filter-input"
              />
              <button type="button" onClick={() => void refreshRequests(tabId)}>{t('reconRefresh')}</button>
            </div>
            {requests.length === 0 ? (
              <div className="recon-muted">{t('reconRequestsEmpty')}</div>
            ) : (
              <div className="recon-req-list">
                {requests
                  .filter((entry) => reqFilter === '' || String(entry.url ?? '').includes(reqFilter))
                  .slice(-80)
                  .reverse()
                  .map((entry) => (
                    <div key={String(entry.requestId)} className="recon-req">
                      <button
                        type="button"
                        className="recon-req-row"
                        onClick={() => void expandRequest(String(entry.requestId))}
                        title={String(entry.url ?? '')}
                      >
                        <span className={`recon-method ${String(entry.method ?? '').toLowerCase()}`}>{String(entry.method ?? '?')}</span>
                        <span className={`recon-status ${statusClass(entry.status)}`}>{String(entry.status ?? '')}</span>
                        <span className="recon-url">{String(entry.url ?? '')}</span>
                        {entry.failed ? <span className="recon-bad">✗ {String(entry.failed)}</span> : null}
                      </button>
                      {expanded === entry.requestId && (
                        <div className="recon-req-detail">
                          <div className="recon-muted">{t('reconInitiator')}:</div>
                          <pre>{initiatorText(entry)}</pre>
                          {Array.isArray(entry.postData) ? null : entry.postData ? (
                            <>
                              <div className="recon-muted">{t('reconPostData')}:</div>
                              <pre>{String(entry.postData).slice(0, 800)}</pre>
                            </>
                          ) : null}
                          {entry.extraRequestHeaders ? (
                            <>
                              <div className="recon-muted">{t('reconCookieHeaders')}:</div>
                              <pre>{formatHeaders(entry.extraRequestHeaders).slice(0, 1200)}</pre>
                            </>
                          ) : null}
                          {entry.extraResponseHeaders ? (
                            <>
                              <div className="recon-muted">{t('reconResponseHeaders')}:</div>
                              <pre>{formatHeaders(entry.extraResponseHeaders).slice(0, 1200)}</pre>
                            </>
                          ) : null}
                          {Array.isArray(entry.wsFrames) && entry.wsFrames.length > 0 && (
                            <>
                              <div className="recon-muted">WebSocket {entry.wsFrames.length} 帧:</div>
                              <pre>
                                {(entry.wsFrames as Array<{ dir: string; payload: string }>)
                                  .slice(-8)
                                  .map((frame) => `[${frame.dir}] ${frame.payload.slice(0, 200)}`)
                                  .join('\n')}
                              </pre>
                            </>
                          )}
                          <div className="recon-req-actions">
                            <button type="button" onClick={() => void fetchBody(String(entry.requestId), 'response')}>
                              {t('reconBody')}
                            </button>
                            <button type="button" onClick={() => void fetchBody(String(entry.requestId), 'request')}>
                              {t('reconPostData')}
                            </button>
                          </div>
                          {bodyOf && bodyOf.requestId === entry.requestId && (
                            <pre className="recon-body">
                              {typeof bodyOf.body === 'string'
                                ? bodyOf.body.slice(0, 4000)
                                : JSON.stringify(bodyOf.body, null, 2).slice(0, 4000)}
                            </pre>
                          )}
                        </div>
                      )}
                    </div>
                  ))}
              </div>
            )}
          </div>

          {/* ---- 控制台 ---- */}
          <div className="recon-section">
            <div className="recon-section-title">{t('reconConsole')}</div>
            {consoleMsgs.length === 0 ? (
              <div className="recon-muted">{t('reconConsoleEmpty')}</div>
            ) : (
              <div className="recon-console">
                {consoleMsgs.slice(-60).map((message) => (
                  <div key={message.seq} className={`recon-console-line recon-level-${message.level}`}>
                    <span className="recon-console-level">{message.level}</span>
                    <span className="recon-console-text" title={message.url ? `${message.url}:${message.line + 1}` : ''}>
                      {message.text.slice(0, 300)}
                    </span>
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  )
}