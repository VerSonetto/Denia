import { memo, useEffect, useMemo, useRef, useState } from 'react'
import { t } from '../i18n'
import type { TranscriptNode } from '../fold'
import { cacheHitPercent, deriveStats, formatCompactDuration, formatTokens } from '../stats'

/**
 * 输入框下方的会话状态栏 —— Denia 自有设计:分段胶囊条。
 * 每个统计维度一个迷你胶囊(图标+数值),横向排列;
 * 运行中状态由消息流末尾的「工作中」指示行承担,这里只放已结算统计。
 * 上下文占用只住在输入框的占用圆环(ContextRing),一处事实一处家。
 * 无数据的胶囊整组消失;全部无数据时不渲染。
 */
export const StatsBar = memo(function StatsBar({ nodes }: { nodes: TranscriptNode[] }) {
  const stats = useMemo(() => deriveStats(nodes), [nodes])

  const hasActivity = stats.steps > 0 || stats.toolCalls > 0
  const hasTokens = stats.inputTokens > 0 || stats.outputTokens > 0
  if (!hasActivity && !hasTokens) return null

  return (
    <div className="stats-bar" role="status">
      {hasActivity && (
        <Pill
          label={t('statsTurns', { turns: stats.turns, steps: stats.steps })}
          title={t('statsTurnsHint', { turns: stats.turns, steps: stats.steps, tools: stats.toolCalls })}
        />
      )}
      {stats.turnMs > 0 && (
        <Pill
          label={formatCompactDuration(stats.turnMs)}
          title={t('statsDurationHint', { duration: formatCompactDuration(stats.turnMs) })}
          mono
        />
      )}
      {stats.avgTtftMs !== null && (
        <Pill
          label={t('statsTtft', { duration: formatCompactDuration(stats.avgTtftMs) })}
          title={t('statsTtftHint', { duration: formatCompactDuration(stats.avgTtftMs) })}
          mono
        />
      )}
      {stats.tokensPerSecond !== null && (
        <Pill
          label={t('statsSpeed', { speed: stats.tokensPerSecond.toFixed(1) })}
          title={t('statsSpeedHint', { speed: stats.tokensPerSecond.toFixed(1) })}
          mono
        />
      )}
      {hasTokens && (
        <TokenPill
          input={stats.inputTokens}
          output={stats.outputTokens}
          cacheRead={stats.cacheReadTokens}
        />
      )}
      {(() => {
        const cache = cacheHitPercent(stats)
        if (cache === null) return null
        return <CachePill percent={cache} cacheRead={stats.cacheReadTokens} input={stats.inputTokens} />
      })()}
    </div>
  )
})

/** 通用胶囊。 */
function Pill({ label, title, mono }: { label: string; title: string; mono?: boolean }) {
  return (
    <span className="stats-pill" title={title}>
      <span className={mono ? 'stats-mono' : undefined}>{label}</span>
    </span>
  )
}

/** token 胶囊:输入/输出 + 缓存命中部分。 */
function TokenPill({
  input,
  output,
  cacheRead,
}: {
  input: number
  output: number
  cacheRead: number
}) {
  const label = `${formatTokens(input)}↑ ${formatTokens(output)}↓`
  const cachePart = cacheRead > 0 ? t('statsCachePart', { cache: formatTokens(cacheRead) }) : ''
  const hint = t('statsTokensHint', {
    input: formatTokens(input),
    output: formatTokens(output),
    cache: cachePart,
  })
  return (
    <span className="stats-pill" title={hint}>
      <span className="stats-mono">{label}</span>
    </span>
  )
}

/** 缓存胶囊:点击弹出命中详情卡片(取代原 hover title)。 */
function CachePill({ percent, cacheRead, input }: { percent: string; cacheRead: number; input: number }) {
  const [open, setOpen] = useState(false)
  const rootRef = useRef<HTMLSpanElement | null>(null)

  useEffect(() => {
    if (!open) return
    const onPointerDown = (event: PointerEvent) => {
      if (rootRef.current && !rootRef.current.contains(event.target as Node)) setOpen(false)
    }
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false)
    }
    document.addEventListener('pointerdown', onPointerDown)
    document.addEventListener('keydown', onKeyDown)
    return () => {
      document.removeEventListener('pointerdown', onPointerDown)
      document.removeEventListener('keydown', onKeyDown)
    }
  }, [open])

  const uncached = Math.max(0, input - cacheRead)
  return (
    <span className="cache-anchor" ref={rootRef}>
      <button
        type="button"
        className="stats-pill stats-pill-button"
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        <span className="stats-mono">{t('statsCache', { percent })}</span>
      </button>
      {open && (
        <div className="cache-card" role="dialog" aria-label={t('statsCacheHitRate')}>
          <div className="cache-card-head">
            <span>{t('statsCacheHitRate')}</span>
            <span className="cache-card-percent stats-mono">{percent}%</span>
          </div>
          <dl className="cache-card-rows">
            <div>
              <dt>{t('cacheReadTokens')}</dt>
              <dd className="stats-mono">{formatTokens(cacheRead)}</dd>
            </div>
            <div>
              <dt>{t('statsCacheMiss')}</dt>
              <dd className="stats-mono">{formatTokens(uncached)}</dd>
            </div>
            <div>
              <dt>{t('inputTokens')}</dt>
              <dd className="stats-mono">{formatTokens(input)}</dd>
            </div>
          </dl>
        </div>
      )}
    </span>
  )
}
