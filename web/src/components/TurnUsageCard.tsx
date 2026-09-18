import { useCallback, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import { t } from '../i18n'
import type { TokenUsage } from '../types'
import { billedInputTokens, cacheHitPercent, formatTokens } from '../stats'

/**
 * 轮次用量明细卡片(抄 dsh TurnUsagePanel:行内触发 + 点开明细)。
 *
 * denia 原先把 `输入12.3k ·输出4.5k` 做成一个死 tooltip —— 有精确值却看不全,
 * 缓存读与推理更是完全没有出口。这里把它变成可点开的明细。
 *
 * **必须走 portal 到 body**(与 dsh 同款)。会话行 `.turn-chrome` 带着
 * `content-visibility: auto`(长会话性能优化),它带来 paint containment:
 * 绝对定位在锚点内的浮层会被整块裁掉 —— 卡片渲染了却一个像素都画不出来,
 * 只在行内留下一圈被裁的边框残影。portal 到 body 后浮层不再有裁剪祖先。
 *
 * 定位:fixed + 视口钳制。优先朝上开(收尾行在对话流末尾,朝下会撞输入框),
 * 上方放不下才翻到下方;水平右对齐锚点右缘,再钳进视口。
 */
export function TurnUsageCard({ usage }: { usage: TokenUsage }) {
  const [open, setOpen] = useState(false)
  const rootRef = useRef<HTMLSpanElement | null>(null)
  const panelRef = useRef<HTMLDivElement | null>(null)
  /** 计算出的落点;null = 尚未量到(先以隐藏态布局一次再定位)。 */
  const [pos, setPos] = useState<{ left: number; top: number } | null>(null)

  const place = useCallback(() => {
    const anchor = rootRef.current
    const panel = panelRef.current
    if (!anchor || !panel) return
    const a = anchor.getBoundingClientRect()
    const p = panel.getBoundingClientRect()
    const vw = window.innerWidth
    const vh = window.innerHeight
    // 右对齐锚点右缘(状态行靠左,锚点右缘即最右落点),再钳进视口。
    const left = Math.max(GAP_MARGIN, Math.min(a.right - p.width, vw - p.width - GAP_MARGIN))
    // 优先朝上;上方空间不足才翻到下方,并同样钳进视口。
    const above = a.top - PANEL_GAP - p.height
    const top = above >= GAP_MARGIN
      ? above
      : Math.min(a.bottom + PANEL_GAP, Math.max(GAP_MARGIN, vh - p.height - GAP_MARGIN))
    setPos({ left, top })
  }, [])

  // 先量后定位:首帧以 visibility:hidden 布局,拿到真实尺寸再摆位,
  // 避免卡片从 (0,0) 闪一下。
  useLayoutEffect(() => {
    if (!open) {
      setPos(null)
      return
    }
    place()
  }, [open, place])

  // 打开期间:外点 / Escape 关闭。面板已 portal 出去,不算"外部"。
  useEffect(() => {
    if (!open) return
    const onPointerDown = (event: PointerEvent) => {
      const node = event.target
      if (!(node instanceof Node)) return
      if (rootRef.current?.contains(node) === true) return
      if (panelRef.current?.contains(node) === true) return
      setOpen(false)
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

  // 浮层是 fixed 的,锚点却会随对话流滚动:跟着重算,不让卡片飘在旧位置。
  useEffect(() => {
    if (!open) return
    let frame = 0
    const follow = () => {
      cancelAnimationFrame(frame)
      frame = requestAnimationFrame(place)
    }
    window.addEventListener('scroll', follow, { capture: true, passive: true })
    window.addEventListener('resize', follow)
    return () => {
      cancelAnimationFrame(frame)
      window.removeEventListener('scroll', follow, { capture: true })
      window.removeEventListener('resize', follow)
    }
  }, [open, place])

  const cacheRead = usage.cacheReadTokens ?? 0
  const reasoning = usage.reasoningTokens ?? 0
  // 提供方"没报缓存"与"报了 0"是两回事:前者整组不出现(不拿 0.00% 冒充
  // 实测零命中),后者才如实显示 0.00%。
  const hasCacheData = usage.cacheReadTokens !== undefined
  const billed = billedInputTokens(usage.inputTokens, cacheRead)
  const cachePercent = hasCacheData ? cacheHitPercent(usage.inputTokens, cacheRead) : null
  const total = billed + usage.outputTokens

  return (
    <span className="turn-usage-anchor" ref={rootRef}>
      <button
        type="button"
        className="turn-usage"
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => setOpen((value) => !value)}
      >
        {t('turnUsageInputUncached')} {formatTokens(usage.inputTokens)} ·{' '}
        {t('outputTokens')} {formatTokens(usage.outputTokens)}
      </button>
      {open && createPortal(
        <div
          ref={panelRef}
          className="turn-usage-card"
          role="dialog"
          aria-label={t('turnUsageTitle')}
          style={pos === null ? MEASURE_STYLE : { left: pos.left, top: pos.top }}
        >
          <div className="turn-usage-head">
            <span>{t('turnUsageTitle')}</span>
            <span className="turn-usage-total stats-mono">{formatTokens(total)}</span>
          </div>
          <dl className="turn-usage-rows">
            {/* 输入侧三桶:未缓存输入与缓存读互斥,相加才是计费输入。 */}
            <div>
              <dt>{t('turnUsageInputUncached')}</dt>
              <dd className="stats-mono">{formatTokens(usage.inputTokens)}</dd>
            </div>
            {hasCacheData && (
              <div>
                <dt>{t('cacheReadTokens')}</dt>
                <dd className="stats-mono">{formatTokens(cacheRead)}</dd>
              </div>
            )}
            {cachePercent !== null && (
              <div>
                <dt>{t('statsCacheHitRate')}</dt>
                <dd className="stats-mono">{cachePercent}%</dd>
              </div>
            )}
            <div>
              <dt>{t('outputTokens')}</dt>
              <dd className="stats-mono">
                {formatTokens(usage.outputTokens)}
                {reasoning > 0 && (
                  <span className="turn-usage-note">
                    {t('turnUsageReasoning', { tokens: formatTokens(reasoning) })}
                  </span>
                )}
              </dd>
            </div>
          </dl>
        </div>,
        document.body,
      )}
    </span>
  )
}

/** 卡片与锚点的间距(px)。 */
const PANEL_GAP = 8
/** 视口边缘保留的余量(px)。 */
const GAP_MARGIN = 12
/** 未定位时的量测态:隐藏但仍参与布局,好让 place() 读到真实尺寸。 */
const MEASURE_STYLE = { left: 0, top: 0, visibility: 'hidden' } as const
