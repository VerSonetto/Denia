import { useEffect, useRef, useState } from 'react'
import { t } from '../i18n'

/** Intrinsic bytes → 估算 token(ASCII 4 char/token,CJK 3 字节/字,取折中)。 */
function bytesToTokens(bytes: number): number {
  return Math.max(1, Math.round(bytes / 3))
}

/** 一段上下文的展示行:名称、占比条形、token 数与字节数。 */
export interface ContextPart {
  key: string
  label: string
  bytes: number
  /** 条形颜色(用于面板)。 */
  color: string
}

const PART_COLORS = ['#6187d8', '#7aa86f', '#d8a35f', '#9d7bd8', '#c66f6f']

/**
 * 上下文窗口使用情况:圆环(总占用比例)+ 点击弹出的悬浮面板
 * (各组成部分占比与精确字节/token 数)。
 */
export function ContextRing({
  contextWindow,
  parts,
}: {
  /** 当前模型的上下文窗口(token);为空时只显示估算值。 */
  contextWindow?: number
  parts: ContextPart[]
}) {
  const [open, setOpen] = useState(false)
  const panelRef = useRef<HTMLDivElement | null>(null)
  const wrapRef = useRef<HTMLDivElement | null>(null)

  const totalBytes = parts.reduce((sum, part) => sum + part.bytes, 0)
  // token 估算:bytes/3;窗口是 token,用于算使用率(近似)。
  const estimatedTokens = bytesToTokens(totalBytes)
  const ratio = contextWindow && contextWindow > 0
    ? Math.min(1, estimatedTokens / contextWindow)
    : null
  const freeTokens = contextWindow && contextWindow > 0
    ? Math.max(0, contextWindow - estimatedTokens)
    : null

  const radius = 6
  const circumference = 2 * Math.PI * radius

  // 点击外部关闭。
  useEffect(() => {
    if (!open) return
    const onPointer = (event: PointerEvent) => {
      if (!wrapRef.current?.contains(event.target as Node)) setOpen(false)
    }
    window.addEventListener('pointerdown', onPointer)
    return () => window.removeEventListener('pointerdown', onPointer)
  }, [open])

  return (
    <div className="context-ring-wrap" ref={wrapRef}>
      <button
        type="button"
        className={`context-ring-btn${open ? ' open' : ''}`}
        title={t('contextRingLabel')}
        aria-expanded={open}
        onClick={() => setOpen(!open)}
      >
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden>
          <circle
            cx="8"
            cy="8"
            r={radius}
            fill="none"
            stroke="currentColor"
            strokeWidth="1.8"
            opacity="0.15"
          />
          {ratio !== null && (
            <circle
              cx="8"
              cy="8"
              r={radius}
              fill="none"
              stroke="currentColor"
              strokeWidth="1.8"
              strokeLinecap="round"
              strokeDasharray={`${circumference * ratio} ${circumference}`}
              transform="rotate(-90 8 8)"
            />
          )}
        </svg>
      </button>
      {open && (
        <div className="context-panel" ref={panelRef} role="dialog" aria-label={t('contextPanelTitle')}>
          <div className="context-panel-head">
            <span className="context-panel-title">{t('contextPanelTitle')}</span>
            <span className="context-panel-total">
              {contextWindow && contextWindow > 0
                ? `${(ratio! * 100).toFixed(1)}%`
                : t('contextTokens', { n: estimatedTokens })}
            </span>
          </div>
          <div className="context-panel-rows">
            {parts.map((part, index) => {
              const partRatio = totalBytes > 0 ? part.bytes / totalBytes : 0
              return (
                <div className="context-row" key={part.key}>
                  <span
                    className="context-dot"
                    style={{ background: part.color ?? PART_COLORS[index % PART_COLORS.length] }}
                  />
                  <span className="context-name">{part.label}</span>
                  <span className="context-bar">
                    <span
                      className="context-bar-fill"
                      style={{
                        width: `${(partRatio * 100).toFixed(1)}%`,
                        background: part.color ?? PART_COLORS[index % PART_COLORS.length],
                      }}
                    />
                  </span>
                  <span className="context-num">
                    {t('contextTokens', { n: bytesToTokens(part.bytes) })}
                  </span>
                </div>
              )
            })}
            {freeTokens !== null && (
              <div className="context-row free">
                <span className="context-dot" style={{ background: 'var(--label-caption)' }} />
                <span className="context-name">{t('contextFree')}</span>
                <span className="context-bar" />
                <span className="context-num">{t('contextTokens', { n: freeTokens })}</span>
              </div>
            )}
          </div>
          <div className="context-panel-hint">{t('contextEstimate')}</div>
        </div>
      )}
    </div>
  )
}
