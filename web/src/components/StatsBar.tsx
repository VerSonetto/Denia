import { memo, useEffect, useMemo, useState } from 'react'
import { t } from '../i18n'
import type { TranscriptNode } from '../fold'
import { cacheHitPercent, deriveStats, formatCompactDuration, formatTokens } from '../stats'

/**
 * 输入框下方的会话状态栏 —— Denia 自有设计:分段胶囊条。
 * 每个统计维度一个迷你胶囊(图标+数值),横向排列;
 * 运行中时长胶囊带呼吸灯实时跳秒。
 * 上下文占用只住在输入框的占用圆环(ContextRing),一处事实一处家。
 * 无数据的胶囊整组消失;全部无数据时不渲染。
 */
export const StatsBar = memo(function StatsBar({
  nodes,
  running,
}: {
  nodes: TranscriptNode[]
  running: boolean
}) {
  const stats = useMemo(() => deriveStats(nodes), [nodes])

  const hasActivity = stats.steps > 0 || stats.toolCalls > 0
  const hasTokens = stats.inputTokens > 0 || stats.outputTokens > 0
  if (!hasActivity && !hasTokens && !running) return null

  return (
    <div className="stats-bar" role="status">
      {running && <LivePill />}
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
        return (
          <Pill
            label={t('statsCache', { percent: cache })}
            title={t('statsCacheHint', { percent: cache, cache: formatTokens(stats.cacheReadTokens), input: formatTokens(stats.inputTokens) })}
            mono
          />
        )
      })()}
    </div>
  )
})

/** 运行中胶囊:呼吸灯 + 实时跳秒,隔离 4Hz tick 不碰外层。 */
function LivePill() {
  const [startedAt] = useState(() => Date.now())
  const [now, setNow] = useState(startedAt)
  useEffect(() => {
    const interval = window.setInterval(() => setNow(Date.now()), 250)
    return () => window.clearInterval(interval)
  }, [])
  const elapsed = ((now - startedAt) / 1000).toFixed(1)
  return (
    <span className="stats-pill live" title={t('statsRunningHint')}>
      <span className="stats-dot" aria-hidden />
      <span className="stats-mono">{elapsed}s</span>
    </span>
  )
}

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
