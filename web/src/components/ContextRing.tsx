import { useEffect, useRef, useState } from 'react'
import * as api from '../api'
import type { ContextBreakdown, ContextPressure } from '../api'
import { markCompacting, useCompactingFor } from '../appStore'
import { t } from '../i18n'

/** 从压力投影解析有界展示占用;分子或容量缺任一项即返回 null(不渲染)。
 * 对齐 dsh `contextOccupancy`:分子取 `projectedTokens ?? pressureTokens`。 */
export interface ContextOccupancy {
  percent: number
  usedTokens: number
  contextWindow: number
}

export function contextOccupancy(
  pressure: ContextPressure | undefined,
): ContextOccupancy | null {
  const usedTokens = pressure?.projectedTokens ?? pressure?.pressureTokens
  if (usedTokens === undefined || pressure?.contextWindow === undefined) return null
  return {
    percent: Math.min(100, Math.round((usedTokens / pressure.contextWindow) * 100)),
    usedTokens,
    contextWindow: pressure.contextWindow,
  }
}

/** 标记占用句子的拆分槽:面板标题保留语序,每个 locale 自有词序
 * (`45% of context used` / `上下文已用 45%`)。 */
const READING_SLOT = '\u0000'

/** 图例行,按分段条顺序;颜色类同时承担色块与分段着色。 */
const ROWS = [
  { key: 'systemTokens', label: 'contextSystem', color: 'cm-color-system' },
  { key: 'toolsTokens', label: 'contextTools', color: 'cm-color-tools' },
  { key: 'messageTokens', label: 'contextMessages', color: 'cm-color-messages' },
] as const

/** 紧凑 token 数:不足 1K 原样,其后 K / M;<100 保留一位小数。 */
function formatTokens(value: number): string {
  const scaled = (candidate: number): string =>
    candidate >= 100 ? String(Math.round(candidate)) : String(Math.round(candidate * 10) / 10)
  if (value < 1_000) return String(value)
  if (value < 1_000_000) return t('numberThousand', { value: scaled(value / 1_000) })
  return t('numberMillion', { value: scaled(value / 1_000_000) })
}

/**
 * 输入框发送键旁的上下文占用圆环(dsh `ContextMeter` 同款交互):
 * 14px 圆环显示 provider 口径占用百分比,点击弹出启发式拆分面板
 * (系统提示词 / 工具 / 对话消息)。provider 未报数或无路由容量时不渲染。
 */
export function ContextRing({
  pressure,
  breakdown,
  sessionId,
  running,
}: {
  /** 服务端 `contextPressure` 投影(锚点 + 表面增量 + 路由容量)。 */
  pressure?: ContextPressure
  /** 服务端启发式拆分(系统提示词 / 工具 / 消息)。 */
  breakdown?: ContextBreakdown
  /** 会话 id:存在时才可手动压缩。 */
  sessionId?: string
  /** 会话是否在运行中:运行中禁用压缩按钮(服务端同样拒绝)。 */
  running?: boolean
}) {
  const [open, setOpen] = useState(false)
  // 压缩中状态来自全局 store:与对话流的"正在压缩"行是同一份,两个入口
  // (面板按钮 / slash 命令)与刷新恢复共用,不会各说各话。
  const compressingAt = useCompactingFor(sessionId ?? null)
  const compressing = compressingAt !== null
  const [note, setNote] = useState<string | null>(null)
  const rootRef = useRef<HTMLSpanElement | null>(null)
  const context = contextOccupancy(pressure)
  const available = context !== null

  // 反馈提示 8 秒后自动消退,不占用面板常驻空间。
  useEffect(() => {
    if (note === null) return
    const timer = window.setTimeout(() => setNote(null), 8_000)
    return () => window.clearTimeout(timer)
  }, [note])

  const handleCompact = async (): Promise<void> => {
    if (!sessionId || compressing || running) return
    setNote(null)
    try {
      // 202 接单即返回:压缩在后台跑,不阻塞这里;收尾由全局"压缩中"标记
      // 驱动(ContextRing 与对话流共用同一份状态,刷新也不丢)。
      await api.compactSession(sessionId)
      markCompacting(sessionId, Date.now())
    } catch (error) {
      setNote(error instanceof api.ApiError ? error.message : t('contextCompactFailed'))
    }
  }

  // 模型切换可能暂时移除容量而本组件仍挂载:面板随之关闭,不留陈旧 UI。
  useEffect(() => {
    if (!available && open) setOpen(false)
  }, [available, open])

  // 打开期间挂一个文档级监听:外点 / Escape 关闭。
  useEffect(() => {
    if (!open || !available) return
    const onPointerDown = (event: PointerEvent): void => {
      if (event.target instanceof Node && rootRef.current?.contains(event.target) === true) return
      setOpen(false)
    }
    const onKeyDown = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') setOpen(false)
    }
    document.addEventListener('pointerdown', onPointerDown)
    document.addEventListener('keydown', onKeyDown)
    return () => {
      document.removeEventListener('pointerdown', onPointerDown)
      document.removeEventListener('keydown', onKeyDown)
    }
  }, [available, open])

  if (context === null) return null
  const percent = context.percent
  const reading = `${percent}%`
  const [headBefore = '', headAfter = ''] = t('contextAria', { percent: READING_SLOT })
    .split(READING_SLOT)
    .map((part) => part.trim())

  // 明细由服务端按 provider 锚点校准:三数之和 == 顶部的 projected 值。
  // 只有手上数据真的对上了才宣称"已校准",避免旧数据源呈现误导性说明。
  const calibrated =
    breakdown !== undefined &&
    pressure?.projectedTokens !== undefined &&
    breakdown.systemTokens + breakdown.toolsTokens + breakdown.messageTokens ===
      pressure.projectedTokens

  // 分段条总长保持 provider 精确百分比;启发式拆分只决定彩色部分的配比。
  // 零宽段直接丢弃:.segment 的 min-width 会让 0% 占用也画出满条。
  const breakdownTotal =
    breakdown === undefined
      ? 0
      : breakdown.systemTokens + breakdown.toolsTokens + breakdown.messageTokens
  const parts =
    breakdown === undefined || breakdownTotal === 0
      ? [{ key: 'total', color: undefined, width: percent }]
      : ROWS.map((row) => ({
          key: row.key,
          color: row.color,
          width: (percent * breakdown[row.key]) / breakdownTotal,
        }))
  const segments = parts.filter((part) => part.width > 0)

  // 圆环几何:14px viewBox,2px 描边。
  const radius = 5.5
  const circumference = 2 * Math.PI * radius

  return (
    <span className="cm-root" ref={rootRef}>
      <button
        type="button"
        className="cm-trigger"
        title={t('contextAria', { percent: reading })}
        aria-label={t('contextAria', { percent: reading })}
        aria-haspopup="dialog"
        aria-expanded={open}
        onClick={() => {
          setOpen(!open)
        }}
      >
        <svg viewBox="0 0 14 14" width="14" height="14" aria-hidden>
          <circle className="cm-track" cx="7" cy="7" r={radius} />
          <circle
            className="cm-fill"
            cx="7"
            cy="7"
            r={radius}
            strokeDasharray={`${(circumference * percent) / 100} ${circumference}`}
            transform="rotate(-90 7 7)"
          />
        </svg>
      </button>
      {open && (
        <div className="cm-panel" role="dialog" aria-label={t('contextUsed')}>
          <div className="cm-header">
            {/* 空侧经 `.cm-headline:empty` 塌陷,不需要先导(或后随)文字
                的语序不占标题间隙。 */}
            <span className="cm-headline">{headBefore}</span>
            <span className="cm-percent">{reading}</span>
            <span className="cm-headline">{headAfter}</span>
            <span className="cm-figures">
              {`~${formatTokens(context.usedTokens)} / ${formatTokens(context.contextWindow)}`}
            </span>
          </div>
          <div className="cm-bar">
            {segments.map((segment) => (
              <div
                key={segment.key}
                className={segment.color === undefined ? 'cm-segment' : `cm-segment ${segment.color}`}
                style={{ width: `${segment.width}%` }}
              />
            ))}
          </div>
          {breakdown !== undefined && (
            <>
              <dl className="cm-rows">
                {ROWS.map((row) => (
                  <div key={row.key} className="cm-row">
                    <dt>
                      <span className={`cm-swatch ${row.color}`} aria-hidden />
                      {t(row.label)}
                    </dt>
                    <dd>{`~${formatTokens(breakdown[row.key])}`}</dd>
                  </div>
                ))}
              </dl>
              <p className="cm-note">
                {calibrated ? t('contextNoteAnchored') : t('contextNoteEstimated')}
              </p>
              <button
                type="button"
                className="cm-compact"
                disabled={!sessionId || compressing || running}
                title={running ? t('contextCompactBusy') : undefined}
                onClick={() => void handleCompact()}
              >
                {compressing ? t('contextCompactRunning') : t('contextCompactNow')}
              </button>
              {note !== null && <p className="cm-note cm-note-result">{note}</p>}
            </>
          )}
        </div>
      )}
    </span>
  )
}
