import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { t } from '../i18n'
import {
  deriveTrajectory,
  formatSelfDuration,
  quoteTrajectoryInterval,
  quoteTrajectoryRecord,
  type TrajectoryGroup,
  type TrajectoryQuote,
  type TrajectoryRecord,
} from '../trajectory'
import type { SessionEnvelope, TokenUsage } from '../types'
import { toolCallSummary } from '../toolDisplay'
import { CopyMessageButton } from './CopyMessageButton'
import { IconChevron, IconSearch } from './icons'
import { formatDuration } from './transcript'

/**
 * Trajectory 视图(抄 dsh ui-trajectory 交互形态,可读性增强):
 *
 * dsh 原版只给裸数字列与相对耗时;本版在其上增加——
 * - 行内耗时微条:时间分布一眼可见,无需脑补 "+Ns" 的相对大小;
 * - 绝对时刻列:与概览条/inspector 对读;
 * - 概览条 ↔ 台账行双向 hover 联动,Assistant span 分 TTFT/生成两段,
 *   轮次边界画分隔刻度;
 * - 轮次组头:结束原因徽标(完成/中止/超长/出错)+ 步数/工具数/token 小计;
 * - token k 化、零值弱化、斑马纹、错误行染色、工具名 mono 芯片。
 */
export function TrajectoryView({
  events,
  onQuote,
}: {
  events: SessionEnvelope[]
  /** 引用单条/区间到 composer(父级切回对话视图并挂芯片)。 */
  onQuote?: (quote: TrajectoryQuote) => void
}) {
  const layout = useMemo(() => deriveTrajectory(events), [events])
  const durationMax = useMemo(
    () => layout.records.reduce((max, r) => Math.max(max, r.durationMs ?? 0), 0),
    [layout],
  )
  const [query, setQuery] = useState('')
  const [collapsedTurns, setCollapsedTurns] = useState<Set<number>>(new Set())
  const [interval, setInterval] = useState<{ start: number; end: number } | null>(null)
  const [selected, setSelected] = useState<TrajectoryRecord | null>(null)
  // hover 联动:行 ↔ 概览条 span 共用同一个稳定 key(record.key)。
  const [hover, setHover] = useState<string | null>(null)

  // 跳转:概览条单击/滑选后滚动台账到目标行(目标轮次自动展开、过滤自动清空)。
  const rowElsRef = useRef(new Map<string, HTMLElement>())
  const [pendingJump, setPendingJump] = useState<string | null>(null)
  const registerRow = useCallback((key: string, el: HTMLElement | null) => {
    if (el) rowElsRef.current.set(key, el)
    else rowElsRef.current.delete(key)
  }, [])

  const orientTo = useCallback(
    (record: TrajectoryRecord) => {
      const group = layout.groups.find((g) => g.records.some((r) => r.key === record.key))
      if (group && group.turn !== null) {
        setCollapsedTurns((previous) => {
          if (!previous.has(group.turn!)) return previous
          const next = new Set(previous)
          next.delete(group.turn!)
          return next
        })
      }
      setPendingJump(record.key)
    },
    [layout],
  )

  const jumpTo = useCallback(
    (record: TrajectoryRecord) => {
      setSelected(record)
      setInterval(null)
      setQuery('')
      orientTo(record)
    },
    [orientTo],
  )

  const normalizedQuery = query.trim().toLowerCase()
  const matches = useCallback(
    (record: TrajectoryRecord) => {
      // 聚焦语义:起点落在区间内的记录(用户预期"只显示滑选的区间";
      // 相交语义会让横跨大半场的长工具永远可见,等于没筛)。
      if (interval) {
        if (record.time < interval.start || record.time > interval.end) return false
      }
      if (!normalizedQuery) return true
      const haystack =
        record.text ??
        record.content ??
        `${record.toolName ?? ''} ${record.args ?? ''} ${record.result?.content ?? ''}`
      return haystack.toLowerCase().includes(normalizedQuery)
    },
    [interval, normalizedQuery],
  )

  const visibleGroups = useMemo(
    () =>
      layout.groups
        .map((group) => ({ group, records: group.records.filter(matches) }))
        .filter(({ records }) => records.length > 0),
    [layout, matches],
  )

  // 过滤清空/轮次展开后的提交帧里目标行才存在;visibleGroups 变化触发重试。
  useEffect(() => {
    if (pendingJump === null) return
    const el = rowElsRef.current.get(pendingJump)
    if (el) {
      el.scrollIntoView({ block: 'center', behavior: 'smooth' })
      setPendingJump(null)
    }
  }, [pendingJump, visibleGroups])

  const focusedCount = interval
    ? layout.records.filter((r) => r.time >= interval.start && r.time <= interval.end).length
    : 0

  // 可见记录的扁平清单:面板 ↑/↓ 键导航沿它移动。
  const visibleRecords = useMemo(
    () => visibleGroups.flatMap(({ records }) => records),
    [visibleGroups],
  )
  const visibleRecordsRef = useRef<TrajectoryRecord[]>([])
  visibleRecordsRef.current = visibleRecords

  // ↑/↓ 在可见记录间移动选中(输入框聚焦时让路);移动后走 pendingJump 滚动到行。
  useEffect(() => {
    if (!selected) return
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null
      if (
        target &&
        (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable)
      ) {
        return
      }
      if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return
      const list = visibleRecordsRef.current
      const index = list.findIndex((r) => r.key === selected.key)
      if (index < 0) return
      const next = event.key === 'ArrowDown' ? list[index + 1] : list[index - 1]
      if (!next) return
      event.preventDefault()
      setSelected(next)
      setPendingJump(next.key)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [selected])

  const toggleTurn = useCallback((turn: number) => {
    setCollapsedTurns((previous) => {
      const next = new Set(previous)
      if (next.has(turn)) next.delete(turn)
      else next.add(turn)
      return next
    })
  }, [])

  if (events.length === 0) {
    return <div className="empty-hint">{t('trajectoryEmpty')}</div>
  }

  return (
    <div className="traj-pane">
      <div className="traj-toolbar" role="toolbar" aria-label={t('trajectoryToolbarAria')}>
        <span className="traj-toolbar-title">{t('viewTrajectory')}</span>
        <button
          type="button"
          className="icon-btn"
          title={t('trajectoryExpandTurns')}
          aria-label={t('trajectoryExpandTurns')}
          onClick={() => setCollapsedTurns(new Set())}
        >
          <span className="chev open">
            <IconChevron size={11} />
          </span>
        </button>
        <button
          type="button"
          className="icon-btn"
          title={t('trajectoryCollapseTurns')}
          aria-label={t('trajectoryCollapseTurns')}
          onClick={() =>
            setCollapsedTurns(new Set(layout.groups.filter((g) => g.turn !== null).map((g) => g.turn!)))
          }
        >
          <span className="chev">
            <IconChevron size={11} />
          </span>
        </button>
        <div className="traj-legend" aria-hidden>
          <span>
            <i data-kind="user" />
            {t('trajectoryKindUser')}
          </span>
          <span>
            <i data-kind="message" />
            {t('trajectoryKindMessage')}
          </span>
          <span>
            <i data-kind="tool" />
            {t('trajectoryKindTool')}
          </span>
        </div>
        <div className="traj-search">
          <IconSearch size={12} />
          <input
            type="search"
            value={query}
            placeholder={t('trajectorySearchPlaceholder')}
            aria-label={t('trajectorySearch')}
            onChange={(event) => setQuery(event.target.value)}
          />
        </div>
      </div>
      <div className="traj-body">
        <div className="traj-main">
          <TimelineBar
            layout={layout}
            interval={interval}
            hover={hover}
            onHover={setHover}
            onInterval={setInterval}
            onJump={jumpTo}
            onOrient={orientTo}
          />
          {interval && (
            <div className="traj-focus-row">
              {onQuote && (
                <button
                  type="button"
                  className="traj-quote-btn"
                  onClick={() => {
                    const records = layout.records.filter(
                      (r) => r.time >= interval.start && r.time <= interval.end,
                    )
                    const quote = quoteTrajectoryInterval(records, interval.start, interval.end)
                    if (quote) onQuote(quote)
                  }}
                >
                  {t('trajQuoteInterval')}
                </button>
              )}
              <button
                type="button"
                className="traj-focus-clear"
                title={t('close')}
                aria-label={t('close')}
                onClick={() => setInterval(null)}
              >
                {formatClock(interval.start)} → {formatClock(interval.end)}
                <span className="traj-focus-count">{t('trajectoryFocusCount', { n: focusedCount })}</span>
                <strong>×</strong>
              </button>
            </div>
          )}
          <div className="traj-ledger">
            {visibleGroups.length === 0 && <div className="empty-hint">{t('trajectoryEmpty')}</div>}
            {visibleGroups.map(({ group, records }) => (
              <TrajectoryTurnBlock
                key={group.turn === null ? 'between' : `turn-${group.turn}`}
                group={group}
                records={records}
                collapsed={group.turn !== null && collapsedTurns.has(group.turn)}
                onToggle={group.turn !== null ? () => toggleTurn(group.turn!) : undefined}
                durationMax={durationMax}
                selectedKey={selected?.key}
                hoverKey={hover}
                onHover={setHover}
                onSelect={setSelected}
                registerRow={registerRow}
              />
            ))}
          </div>
        </div>
        {selected && (
          <RecordInspector
            record={selected}
            onClose={() => setSelected(null)}
            onQuote={onQuote}
          />
        )}
      </div>
    </div>
  )
}

/* ---- 时间概览条(dsh TrajectoryTimeline + 联动/刻度) ---- */

function TimelineBar({
  layout,
  interval,
  hover,
  onHover,
  onInterval,
  onJump,
  onOrient,
}: {
  layout: ReturnType<typeof deriveTrajectory>
  interval: { start: number; end: number } | null
  hover: string | null
  onHover: (key: string | null) => void
  onInterval: (value: { start: number; end: number } | null) => void
  /** 单击轴:跳转到该时刻的记录(选中 + 滚动到行)。 */
  onJump: (record: TrajectoryRecord) => void
  /** 滑选提交:台账定位到区间起点(不强制选中)。 */
  onOrient: (record: TrajectoryRecord) => void
}) {
  const ref = useRef<HTMLDivElement>(null)
  // 视窗(null = 全域);命名避开全局 window。
  const [viewport, setViewport] = useState<{ start: number; end: number } | null>(null)
  const dragRef = useRef<
    | { kind: 'select'; startX: number }
    | { kind: 'pan'; startX: number; winStart: number; winEnd: number }
    | null
  >(null)

  const span = layout.endTime - layout.startTime || 1
  const win = viewport ?? { start: layout.startTime, end: layout.endTime }
  const winSpan = Math.max(1, win.end - win.start)
  const pct = (time: number) => ((time - win.start) / winSpan) * 100

  const timeAt = useCallback(
    (clientX: number) => {
      const rect = ref.current?.getBoundingClientRect()
      if (!rect || rect.width === 0) return win.start
      const ratio = Math.min(1, Math.max(0, (clientX - rect.left) / rect.width))
      return win.start + ratio * winSpan
    },
    [win.start, winSpan],
  )

  // 卸载兜底:拖拽中离开视口也要释放监听。
  useEffect(() => {
    const onUp = () => {
      dragRef.current = null
    }
    window.addEventListener('pointerup', onUp)
    return () => window.removeEventListener('pointerup', onUp)
  }, [])

  const spanOf = (record: TrajectoryRecord) => {
    const left = pct(record.time)
    if (record.durationMs === undefined) {
      return { left, width: 0, running: true }
    }
    const right = pct(Math.min(win.end, record.time + record.durationMs))
    return { left, width: Math.max(0, right - left), running: false }
  }

  /** 该时刻(或其之前最近)的记录;记录按时间有序。 */
  const recordAt = (time: number): TrajectoryRecord | null => {
    let best: TrajectoryRecord | null = null
    for (const record of layout.records) {
      if (record.time > time) break
      best = record
    }
    return best ?? layout.records[0] ?? null
  }

  return (
    <div className="traj-overview" title={t('trajectoryOverviewHint')}>
      <div
        className="traj-overview-track"
        ref={ref}
        onPointerDown={(event) => {
          if (event.button === 2) {
            // 右键按下:已在缩放态则平移,否则等右键抬起清除。
            if (viewport) {
              dragRef.current = {
                kind: 'pan',
                startX: event.clientX,
                winStart: win.start,
                winEnd: win.end,
              }
            }
            return
          }
          dragRef.current = { kind: 'select', startX: event.clientX }
        }}
        onPointerMove={(event) => {
          const drag = dragRef.current
          if (drag?.kind === 'select') {
            const anchor = timeAt(drag.startX)
            const current = timeAt(event.clientX)
            onInterval({
              start: Math.min(anchor, current),
              end: Math.max(anchor, current),
            })
          } else if (drag?.kind === 'pan') {
            const rect = ref.current?.getBoundingClientRect()
            const width = rect?.width ?? 1
            const shift = ((event.clientX - drag.startX) / width) * (drag.winEnd - drag.winStart)
            setViewport({ start: drag.winStart - shift, end: drag.winEnd - shift })
          }
        }}
        onPointerUp={(event) => {
          const drag = dragRef.current
          dragRef.current = null
          if (event.button === 2) {
            // 右键单击(未拖动)清除聚焦并复位视窗(dsh 语义)。
            if (Math.abs(event.clientX - (drag?.kind === 'pan' ? drag.startX : event.clientX)) < 3) {
              setViewport(null)
              onInterval(null)
            }
            return
          }
          if (drag?.kind === 'select') {
            const anchor = timeAt(drag.startX)
            const current = timeAt(event.clientX)
            if (Math.abs(anchor - current) < span / 500) {
              // 单击:跳转到该时刻的记录(选中 + 台账滚动到行)。
              const target = recordAt(current)
              if (target) onJump(target)
            } else {
              // 滑选:聚焦区间(台账只显示起点落在区间内的记录)并定位到区间起点。
              const start = Math.min(anchor, current)
              const end = Math.max(anchor, current)
              onInterval({ start, end })
              const first = layout.records.find((record) => record.time >= start)
              if (first) onOrient(first)
            }
          }
        }}
        onContextMenu={(event) => event.preventDefault()}
        onWheel={(event) => {
          event.preventDefault()
          const anchor = timeAt(event.clientX)
          const factor = event.deltaY < 0 ? 0.8 : 1.25
          const nextSpan = Math.min(span * 2, Math.max(span / 200, winSpan * factor))
          const ratio = (anchor - win.start) / winSpan
          const start = anchor - ratio * nextSpan
          setViewport({ start, end: start + nextSpan })
        }}
      >
        {/* 轮次边界刻度:概览条上直接可读轮次分组 */}
        {layout.groups
          .filter((group) => group.turn !== null)
          .map((group) => {
            const left = pct(group.startTime)
            if (left < 0 || left > 100) return null
            return <i key={`tick-${group.turn}`} className="traj-tick" style={{ left: `${left}%` }} />
          })}
        {layout.records.map((record) => {
          const geometry = spanOf(record)
          if (geometry.left > 100 || geometry.left + geometry.width < 0) return null
          const ttftFrac =
            record.kind === 'message' &&
            record.ttftMs !== undefined &&
            record.durationMs !== undefined &&
            record.durationMs > 0
              ? Math.min(1, record.ttftMs / record.durationMs)
              : null
          return (
            <span
              key={record.key}
              className={`traj-span ${record.kind}${record.result?.isError ? ' err' : ''}${geometry.running ? ' running' : ''}${hover === record.key ? ' hot' : ''}`}
              style={{ left: `${geometry.left}%`, width: `${Math.max(0.4, geometry.width)}%` }}
              title={`${kindLabel(record.kind)} · ${formatClock(record.time)} · ${formatSelfDuration(record.durationMs)}`}
              onMouseEnter={() => onHover(record.key)}
              onMouseLeave={() => onHover(null)}
            >
              {/* Assistant span 分段:TTFT(等首 token)/ 生成(解码) */}
              {ttftFrac !== null && ttftFrac > 0.02 && (
                <i className="ttft" style={{ width: `${ttftFrac * 100}%` }} />
              )}
            </span>
          )
        })}
        {interval && (
          <span
            className="traj-span-focus"
            style={{
              left: `${pct(interval.start)}%`,
              width: `${Math.max(0, pct(interval.end) - pct(interval.start))}%`,
            }}
          />
        )}
      </div>
      <div className="traj-overview-axis">
        <span>{formatClock(win.start)}</span>
        <span>{formatClock(win.start + winSpan / 2)}</span>
        <span>{formatClock(win.end)}</span>
      </div>
    </div>
  )
}

/* ---- 轮次块:组头(含列标签)+ 台账行 ---- */

function TrajectoryTurnBlock({
  group,
  records,
  collapsed,
  onToggle,
  durationMax,
  selectedKey,
  hoverKey,
  onHover,
  onSelect,
  registerRow,
}: {
  group: TrajectoryGroup
  records: TrajectoryRecord[]
  collapsed: boolean
  onToggle?: () => void
  /** 全会话最大单条耗时;行内微条的比例基准。 */
  durationMax: number
  selectedKey?: string
  hoverKey: string | null
  onHover: (key: string | null) => void
  onSelect: (record: TrajectoryRecord) => void
  registerRow: (key: string, el: HTMLElement | null) => void
}) {
  const title =
    group.turn === null
      ? t('trajectoryBetweenTurns')
      : t('trajectoryTurnLabel', { turn: group.turn })
  // 组头小计:步数 / 工具次数 / token 总量。
  const steps = new Set(records.map((r) => r.step).filter((s) => s !== undefined)).size
  const tools = group.toolStats.reduce((sum, stat) => sum + stat.count, 0)
  const tokens = records.reduce(
    (sum, r) => sum + (r.usage ? r.usage.inputTokens + r.usage.outputTokens : 0),
    0,
  )
  return (
    <section className={`traj-turn${collapsed ? ' collapsed' : ''}${group.turn === null ? ' between' : ''}`}>
      <header className="traj-turn-head" onClick={onToggle}>
        <span className="traj-head-main">
          {onToggle && (
            <span className={`chev${collapsed ? '' : ' open'}`}>
              <IconChevron size={11} />
            </span>
          )}
          <span className="title">{title}</span>
          {group.open ? (
            <span className="badge run">{t('trajectoryTurnOpen')}</span>
          ) : (
            group.endReason && group.endReason !== 'completed' && (
              <span className={`badge ${group.endReason === 'error' ? 'err' : 'warn'}`}>
                {endReasonLabel(group.endReason)}
              </span>
            )
          )}
          <span className="wall">{formatDuration(group.wallMs)}</span>
          <span className="traj-chips">
            {steps > 0 && <span className="traj-chip">{t('trajectorySteps', { n: steps })}</span>}
            {tools > 0 && (
              <span className="traj-chip">{t('trajectoryToolsCount', { n: tools })}</span>
            )}
            {tokens > 0 && (
              <span className="traj-chip">{t('trajectoryTokensShort', { n: fmtTokens(tokens) })}</span>
            )}
          </span>
        </span>
        <span className="traj-head-cols">
          <span>{t('trajectoryColumnInput')}</span>
          <span>{t('trajectoryColumnOutput')}</span>
          <span>{t('trajectoryColumnThink')}</span>
        </span>
        <span className="traj-head-col">{t('trajectoryColClock')}</span>
        <span className="traj-head-col">{t('trajectoryColDuration')}</span>
      </header>
      {!collapsed && (
        <div className="traj-rows">
          {records.map((record) => (
            <TrajectoryRow
              key={record.key}
              record={record}
              durationMax={durationMax}
              selected={record.key === selectedKey}
              hot={record.key === hoverKey}
              onHover={onHover}
              onSelect={() => onSelect(record)}
              registerRow={registerRow}
            />
          ))}
        </div>
      )}
    </section>
  )
}

function endReasonLabel(reason: NonNullable<TrajectoryGroup['endReason']>): string {
  switch (reason) {
    case 'aborted':
      return t('reasonAborted')
    case 'max-tokens':
      return t('reasonMaxTokens')
    case 'error':
      return t('reasonError')
    case 'loop-detected':
      return t('loopDetectedHint')
    case 'interrupted':
      return t('reasonInterrupted')
    default:
      return t('reasonCompleted')
  }
}

// memo 包装:只有 selected/hot/内容变化的行重渲染(hover 联动不拖累整表)。
const TrajectoryRow = memo(TrajectoryRowImpl)

function TrajectoryRowImpl({
  record,
  durationMax,
  selected,
  hot,
  onHover,
  onSelect,
  registerRow,
}: {
  record: TrajectoryRecord
  durationMax: number
  selected: boolean
  hot: boolean
  onHover: (key: string | null) => void
  onSelect: () => void
  registerRow: (key: string, el: HTMLElement | null) => void
}) {
  const running = record.kind === 'tool' && record.result === undefined && record.durationMs === undefined
  const isError = record.result?.isError === true
  const summary =
    record.kind === 'tool'
      ? (record.result?.content ?? '')
      : (record.text ?? record.content ?? '')
  const barPct =
    record.durationMs !== undefined && record.durationMs > 0 && durationMax > 0
      ? Math.max(4, (record.durationMs / durationMax) * 100)
      : 0
  return (
    <button
      type="button"
      ref={(el) => registerRow(record.key, el)}
      className={`traj-row ${record.kind}${selected ? ' selected' : ''}${hot ? ' hot' : ''}${running ? ' running' : ''}${isError ? ' err' : ''}`}
      onClick={onSelect}
      onMouseEnter={() => onHover(record.key)}
      onMouseLeave={() => onHover(null)}
    >
      <span className={`traj-badge ${record.kind}`}>{kindLabel(record.kind)}</span>
      <span className="traj-summary" title={summary || undefined}>
        {record.kind === 'tool' ? (
          <>
            <code className="traj-tool-chip">{record.toolName ?? '?'}</code>
            {toolCallSummary(record.toolName ?? '', record.args ?? '') && (
              <span className="traj-summary-text">
                {toolCallSummary(record.toolName ?? '', record.args ?? '')}
              </span>
            )}
            {record.result?.content && !running && (
              <span className="traj-summary-text dim">
                {firstLine(record.result.content, 60)}
              </span>
            )}
          </>
        ) : (
          summary || (record.imageCount ? t('trajectoryRecordImageOnly') : '—')
        )}
        {record.interrupted && <em className="traj-interrupted">{t('interrupted')}</em>}
        {running && <em className="traj-running-dot" />}
      </span>
      <span className="traj-tokens">
        {record.usage ? (
          <>
            <span>{fmtTokens(record.usage.inputTokens)}</span>
            <span>{fmtTokens(record.usage.outputTokens)}</span>
            <span className={record.usage.reasoningTokens ? undefined : 'dim'}>
              {record.usage.reasoningTokens ? fmtTokens(record.usage.reasoningTokens) : '—'}
            </span>
          </>
        ) : (
          <>
            <span />
            <span />
            <span />
          </>
        )}
      </span>
      <span className="traj-clock">{formatClock(record.time)}</span>
      <span className="traj-dur">
        {barPct > 0 && (
          <i
            className="traj-dur-bar"
            style={{ width: `${barPct}%` }}
            data-kind={record.kind}
            data-err={isError || undefined}
          />
        )}
        <b>{formatSelfDuration(record.durationMs)}</b>
      </span>
    </button>
  )
}

function kindLabel(kind: TrajectoryRecord['kind']): string {
  if (kind === 'user') return t('trajectoryKindUser')
  if (kind === 'message') return t('trajectoryKindMessage')
  return t('trajectoryKindTool')
}

/* ---- inspector(右侧常驻面板:统计条 + 分区正文 + 复制) ---- */

type InspectorTab = 'summary' | 'payload' | 'result' | 'timing' | 'usage'

function RecordInspector({
  record,
  onClose,
  onQuote,
}: {
  record: TrajectoryRecord
  onClose: () => void
  onQuote?: (quote: TrajectoryQuote) => void
}) {
  const tabs = inspectorTabs(record)
  const [tab, setTab] = useState<InspectorTab>(tabs[0])
  useEffect(() => {
    setTab(inspectorTabs(record)[0])
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [record])
  // Esc 关闭面板(dsh 本地 inspector 同款快捷键)。
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  const isError = record.result?.isError === true
  const running = record.kind === 'tool' && record.result === undefined && record.durationMs === undefined
  const bodyText =
    record.kind === 'tool'
      ? (record.result?.content ?? '')
      : (record.text ?? record.content ?? '')
  const argsRaw = record.args ? prettyJson(record.args) : ''
  const throughput =
    record.decodeMs !== undefined && record.usage && record.usage.outputTokens > 0
      ? (record.usage.outputTokens / (record.decodeMs / 1000)).toFixed(1)
      : null

  return (
    <aside className="traj-inspector" aria-label={t('trajectoryInspectorAria')}>
      <div className="traj-inspector-head">
        <div className="traj-inspector-title">
          <span className={`traj-badge ${record.kind}`}>{kindLabel(record.kind)}</span>
          <span className="traj-inspector-summary">
            {record.kind === 'tool'
              ? record.toolName
              : firstLine(bodyText, 60) || '—'}
          </span>
          {onQuote && (
            <button
              type="button"
              className="traj-quote-btn"
              onClick={() => onQuote(quoteTrajectoryRecord(record))}
            >
              {t('trajQuoteRecord')}
            </button>
          )}
          <button type="button" className="icon-btn" onClick={onClose} aria-label={t('close')}>
            ×
          </button>
        </div>
        <div className="traj-inspector-meta">
          {record.turn === null
            ? t('trajectoryBetweenTurns')
            : t('trajectoryTurnLabel', { turn: record.turn })}
          {' · '}
          {t('trajectoryEventSeq', { seq: record.seq })}
          {record.step !== undefined && ` · ${t('trajectoryGroupStep', { step: record.step })}`}
        </div>
      </div>
      <div className="traj-stats">
        <div className="traj-stat">
          <span className="label">{t('trajectoryTimingStarted')}</span>
          <b>{formatClock(record.time)}</b>
        </div>
        <div className="traj-stat">
          <span className="label">{t('trajectoryTimingDuration')}</span>
          <b>
            {record.durationMs === undefined
              ? t('trajectoryTimingNotRecorded')
              : formatSelfDuration(record.durationMs)}
          </b>
        </div>
        {record.kind === 'message' && record.ttftMs !== undefined && (
          <div className="traj-stat">
            <span className="label">{t('trajectoryTimingTtft')}</span>
            <b>{formatSelfDuration(record.ttftMs)}</b>
          </div>
        )}
        {record.kind === 'message' && record.decodeMs !== undefined && (
          <div className="traj-stat">
            <span className="label">{t('trajectoryTimingGeneration')}</span>
            <b>
              {formatSelfDuration(record.decodeMs)}
              {throughput && (
                <span className="traj-stat-sub">
                  {' '}
                  {t('trajectoryUnitTokensPerSecond', { value: throughput })}
                </span>
              )}
            </b>
          </div>
        )}
        {record.kind === 'tool' && (
          <div className="traj-stat">
            <span className="label">{t('trajectoryTabResult')}</span>
            <b className={running ? 'run' : isError ? 'err' : 'ok'}>
              {running
                ? t('trajectoryStatusRunning')
                : isError
                  ? t('trajectoryStatusFailed')
                  : t('trajectoryToolOk')}
            </b>
          </div>
        )}
      </div>
      <div className="traj-tabs" role="tablist">
        {tabs.map((entry) => (
          <button
            key={entry}
            type="button"
            role="tab"
            aria-selected={tab === entry}
            className={`traj-tab${tab === entry ? ' active' : ''}`}
            onClick={() => setTab(entry)}
          >
            {tabLabel(entry)}
          </button>
        ))}
      </div>
      <div className="traj-inspector-body">
        {tab === 'summary' && (
          <div className="traj-card">
            <div className="traj-card-banner">
              <span>{t('trajectoryTabSummary')}</span>
              {bodyText && <CopyMessageButton text={bodyText} />}
            </div>
            <pre className="traj-prose">{bodyText || t('trajectoryRecordNoOutput')}</pre>
          </div>
        )}
        {tab === 'payload' && (
          <div className="traj-card">
            <div className="traj-card-banner">
              <span>{t('trajectoryTabPayload')}</span>
              {argsRaw && <CopyMessageButton text={argsRaw} />}
            </div>
            <pre className="traj-code">{argsRaw || t('trajectoryRecordNoPayload')}</pre>
          </div>
        )}
        {tab === 'result' && (
          <div className="traj-card">
            <div className="traj-card-banner">
              <span>{t('trajectoryTabResult')}</span>
              {record.result && <CopyMessageButton text={record.result.content} />}
            </div>
            <pre className={`traj-code${isError ? ' err' : ''}`}>
              {record.result ? record.result.content : t('trajectoryRecordNoResult')}
            </pre>
          </div>
        )}
        {tab === 'timing' && <TimingTable record={record} />}
        {tab === 'usage' && <UsageTable usage={record.usage} />}
      </div>
    </aside>
  )
}

function inspectorTabs(record: TrajectoryRecord): InspectorTab[] {
  if (record.kind === 'tool') return ['payload', 'result', 'timing']
  if (record.kind === 'user') return ['summary', 'timing']
  return record.usage ? ['summary', 'timing', 'usage'] : ['summary', 'timing']
}

function tabLabel(tab: InspectorTab): string {
  switch (tab) {
    case 'summary':
      return t('trajectoryTabSummary')
    case 'payload':
      return t('trajectoryTabPayload')
    case 'result':
      return t('trajectoryTabResult')
    case 'timing':
      return t('trajectoryTabTiming')
    case 'usage':
      return t('trajectoryTabUsage')
  }
}

function TimingTable({ record }: { record: TrajectoryRecord }) {
  return (
    <dl className="traj-kv">
      <dt>{t('trajectoryTimingStarted')}</dt>
      <dd>{formatClock(record.time)}</dd>
      <dt>{t('trajectoryTimingDuration')}</dt>
      <dd>
        {record.durationMs === undefined
          ? t('trajectoryTimingNotRecorded')
          : formatSelfDuration(record.durationMs)}
      </dd>
      {record.ttftMs !== undefined && (
        <>
          <dt>{t('trajectoryTimingTtft')}</dt>
          <dd>{formatSelfDuration(record.ttftMs)}</dd>
        </>
      )}
      {record.decodeMs !== undefined && (
        <>
          <dt>{t('trajectoryTimingGeneration')}</dt>
          <dd>{formatSelfDuration(record.decodeMs)}</dd>
        </>
      )}
      {record.decodeMs !== undefined && record.usage && record.usage.outputTokens > 0 && (
        <>
          <dt>{t('trajectoryTimingThroughput')}</dt>
          <dd>
            {t('trajectoryUnitTokensPerSecond', {
              value: (record.usage.outputTokens / (record.decodeMs / 1000)).toFixed(1),
            })}
          </dd>
        </>
      )}
    </dl>
  )
}

function UsageTable({ usage }: { usage?: TokenUsage }) {
  if (!usage) {
    return <div className="empty-hint">{t('trajectoryUsageNotReported')}</div>
  }
  return (
    <dl className="traj-kv">
      <dt>{t('inputTokens')}</dt>
      <dd>{usage.inputTokens}</dd>
      {(usage.cacheReadTokens ?? 0) > 0 && (
        <>
          <dt>{t('cacheReadTokens')}</dt>
          <dd>{usage.cacheReadTokens}</dd>
        </>
      )}
      <dt>{t('outputTokens')}</dt>
      <dd>{usage.outputTokens}</dd>
      {(usage.reasoningTokens ?? 0) > 0 && (
        <>
          <dt>{t('reasoningTokens')}</dt>
          <dd>{usage.reasoningTokens}</dd>
        </>
      )}
    </dl>
  )
}

/* ---- 小工具 ---- */

function firstLine(text: string, max: number): string {
  let rest = text
  for (;;) {
    const nl = rest.indexOf('\n')
    const line = (nl < 0 ? rest : rest.slice(0, nl)).trim()
    if (line.length > 0 || nl < 0) {
      return line.length > max ? `${line.slice(0, max)}…` : line
    }
    rest = rest.slice(nl + 1)
  }
}

function formatClock(time: number): string {
  return new Date(time).toLocaleTimeString(undefined, { hour12: false })
}

/** token 列紧凑格式:1.2k / 34k,千以下原样。 */
function fmtTokens(n: number): string {
  if (n >= 10_000) return `${Math.round(n / 1000)}k`
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`
  return String(n)
}

function prettyJson(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2)
  } catch {
    return raw
  }
}
