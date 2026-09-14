/**
 * 面板标签栏:标签总览 + 标签列表 + 新增菜单。
 *
 * # 抄 ZCode 的四处关键交互
 *
 * 1. **关闭按钮常驻 + 右侧渐隐带**。ZCode 的标签里,关闭按钮**始终渲染**
 *    (不是 hover 才出现),配合一条 8px 的 `transparent → 标签底色` 渐变带,
 *    让长标题从右向左淡出而不是硬截断。这里用同样的结构:按钮常驻 →
 *    标签宽度不随 hover 变化(无布局抖动);渐变带取 `--tab-bg` 变量 →
 *    激活/非激活两态下都无缝。
 *
 * 2. **拖拽用 HTML5 DnD 而不是引 dnd-kit**。ZCode 用 dnd-kit(带
 *    `activationConstraint: distance 4`)。denia 不引 UI 库是硬约束,而
 *    HTML5 拖拽的"移动 4px 才启动"由浏览器自己保证(draggable 元素要
 *    真正开始拖动才触发 dragstart),天然满足同一个需求。
 *
 * 3. **横向溢出时 `+` 按钮搬家 + 三态渐隐 mask**。标签多到放不下时,
 *    滚动区左右两侧按"能否继续滚"加不同方向的 mask,让用户看出还有内容;
 *    `+` 按钮从滚动区内部移到右端固定位(否则它会被滚走)。
 *
 * 4. **标签总览 Popover**。一次列出"打开的"和"最近关闭的",支持模糊搜索,
 *    最近关闭的可以点回来。这是标签多开后的必要兜底 —— 标签栏只能显示
 *    十来个,总览才是全量入口。
 */

import { useEffect, useMemo, useRef, useState } from 'react'
import { t } from '../i18n'
import {
  RECENT_CLOSED_LIMIT,
  type ClosedTab,
  type SidePaneTab,
  type SidePaneTabType,
} from '../sidePane'
import { IconClose, IconGlobe, IconPlus, IconSearch, IconStack, IconTerminal, IconTool } from './icons'
import { PanePopover } from './PanePopover'

/** 单个标签的宽度基准:与 ZCode 的 `flex-[1_1_9.75rem]` 对齐(156px)。 */
const TAB_BASE_WIDTH = 156
/** 标签间距:与 ZCode 的 `gap-2` 一致(8px)。 */
const TAB_GAP = 8
/** `+` 按钮占宽(含与标签的间距)。与 `.pane-add` 的 CSS 定宽保持一致。 */
const ADD_BUTTON_WIDTH = 26 + TAB_GAP

export function tabTypeLabel(type: SidePaneTabType): string {
  if (type === 'review') return t('sidePaneReview')
  if (type === 'terminal') return t('sidePaneTerminal')
  return t('sidePaneBrowser')
}

export function TabIcon({ type, size = 13 }: { type: SidePaneTabType; size?: number }) {
  if (type === 'review') return <IconTool size={size} />
  if (type === 'terminal') return <IconTerminal size={size} />
  return <IconGlobe size={size} />
}

/** 标签显示名:有自定义标题(终端 shell / 浏览器页面标题)就用它。 */
export function tabTitle(tab: SidePaneTab): string {
  const custom = tab.title?.trim()
  if (custom) return custom
  return tabTypeLabel(tab.type)
}

/** 相对时间(标签总览里显示"刚刚 / 3 分钟前")。 */
function relativeTime(at: number): string {
  const delta = Date.now() - at
  if (delta < 60_000) return t('sidePaneTimeJustNow')
  const minutes = Math.floor(delta / 60_000)
  if (minutes < 60) return `${minutes} ${t('sidePaneTimeMinutes')}`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} ${t('sidePaneTimeHours')}`
  return `${Math.floor(hours / 24)} ${t('sidePaneTimeDays')}`
}

/**
 * 模糊搜索评分(抄 ZCode 的 `XAt` 分层加权)。
 *
 * 分层而不是简单的 `includes`:标题前缀命中(120)远强于类型名命中(20),
 * 所以搜 "term" 时"终端"面板会排在只是类型名含 term 的项前面。
 * 全部 token 必须命中(AND 预过滤),否则搜两个词会返回一堆只匹配一个的项。
 */
function score(query: string[], title: string, hint: string): number {
  const lowerTitle = title.toLowerCase()
  const lowerHint = hint.toLowerCase()
  const all = `${lowerTitle} ${lowerHint}`
  if (!query.every((token) => all.includes(token))) return 0
  return query.reduce((total, token) => {
    if (lowerTitle.startsWith(token)) return total + 120
    if (lowerTitle.includes(token)) return total + 70
    if (lowerHint.includes(token)) return total + 40
    return total + 1
  }, 0)
}

export interface SidePaneTabsProps {
  tabs: SidePaneTab[]
  activeTabId: string
  recentClosed: ClosedTab[]
  onActivate(id: string): void
  onClose(id: string): void
  onCloseOthers(id: string): void
  onCloseAll(): void
  onReorder(fromId: string, toId: string): void
  onReopen(id: string): void
  onAdd(type: SidePaneTabType): void
  /** `+` 菜单里可用的类型(由注册表按可用性过滤后传入)。 */
  addable: SidePaneTabType[]
}

export function SidePaneTabs({
  tabs,
  activeTabId,
  recentClosed,
  onActivate,
  onClose,
  onCloseOthers,
  onCloseAll,
  onReorder,
  onReopen,
  onAdd,
  addable,
}: SidePaneTabsProps) {
  const [overviewOpen, setOverviewOpen] = useState(false)
  const [addOpen, setAddOpen] = useState(false)
  const [draggingId, setDraggingId] = useState<string | null>(null)
  const viewportRef = useRef<HTMLDivElement | null>(null)
  /** 浮层锚点:菜单 portal 出去后要靠它算落点(见 `PanePopover`)。 */
  const overviewAnchorRef = useRef<HTMLButtonElement | null>(null)
  const addAnchorRef = useRef<HTMLButtonElement | null>(null)
  /** 溢出方向:决定滚动区两侧的 mask。 */
  const [overflow, setOverflow] = useState({ left: false, right: false })
  /** 是否溢出:`+` 按钮据此搬家。 */
  const [overflowing, setOverflowing] = useState(false)

  /* ---- 溢出检测 ---- */

  useEffect(() => {
    const viewport = viewportRef.current
    if (!viewport) return
    const measure = () => {
      const scrollable = viewport.scrollWidth - viewport.clientWidth
      const left = viewport.scrollLeft > 1
      const right = scrollable > 1 && viewport.scrollLeft < scrollable - 1
      setOverflow((prev) => (prev.left === left && prev.right === right ? prev : { left, right }))
      // 估算:标签按基准宽 + 间距算总需宽,再加 `+` 按钮(定宽)与标签栏内边距,
      // 超过可用宽就认为溢出。
      //
      // `+` 宽度用常量而不是实测:按钮是 CSS 定宽的(26px),实测会让
      // measure → setState → 重渲染 → measure 形成回路,且首帧量到 0
      // 会误判为"不溢出",按钮在两种位置间跳一次。
      const needed =
        tabs.length * TAB_BASE_WIDTH +
        Math.max(0, tabs.length - 1) * TAB_GAP +
        ADD_BUTTON_WIDTH
      setOverflowing(needed > viewport.clientWidth + 1)
    }
    measure()
    viewport.addEventListener('scroll', measure, { passive: true })
    const observer = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(measure)
    observer?.observe(viewport)
    window.addEventListener('resize', measure)
    return () => {
      viewport.removeEventListener('scroll', measure)
      observer?.disconnect()
      window.removeEventListener('resize', measure)
    }
  }, [tabs.length])

  /* ---- 激活标签滚入视野(仅越界时滚,避免每次切换都抖动) ---- */

  useEffect(() => {
    const viewport = viewportRef.current
    if (!viewport || !activeTabId) return
    const handle = requestAnimationFrame(() => {
      const element = viewport.querySelector<HTMLElement>(`[data-tab-id="${CSS.escape(activeTabId)}"]`)
      if (!element) return
      const box = viewport.getBoundingClientRect()
      const target = element.getBoundingClientRect()
      const leftOverflow = target.left - box.left
      const rightOverflow = target.right - box.right
      if (leftOverflow < 0) viewport.scrollBy({ left: leftOverflow, behavior: 'smooth' })
      else if (rightOverflow > 0) viewport.scrollBy({ left: rightOverflow, behavior: 'smooth' })
    })
    return () => cancelAnimationFrame(handle)
  }, [activeTabId, tabs])

  /* ---- 总览的搜索 ---- */

  const [query, setQuery] = useState('')
  const tokens = useMemo(
    () => query.trim().toLowerCase().split(/\s+/).filter(Boolean),
    [query],
  )
  const [tick, setTick] = useState(() => Date.now())
  // 相对时间每分钟刷新一次(只在总览打开时跑)。
  useEffect(() => {
    if (!overviewOpen) return
    setTick(Date.now())
    const timer = window.setInterval(() => setTick(Date.now()), 60_000)
    return () => window.clearInterval(timer)
  }, [overviewOpen])

  const openItems = useMemo(() => {
    const items = tabs.map((tab) => ({
      tab,
      title: tabTitle(tab),
      score: score(tokens, tabTitle(tab), `${tab.type} ${tab.cwd ?? ''}`),
    }))
    return tokens.length > 0
      ? items.filter((item) => item.score > 0).sort((a, b) => b.score - a.score)
      : items
  }, [tabs, tokens])

  const closedItems = useMemo(() => {
    const items = recentClosed.slice(0, RECENT_CLOSED_LIMIT).map((item) => ({
      item,
      title: tabTitle(item.tab),
      score: score(tokens, tabTitle(item.tab), `${item.tab.type} ${item.tab.cwd ?? ''}`),
    }))
    return tokens.length > 0
      ? items.filter((entry) => entry.score > 0).sort((a, b) => b.score - a.score)
      : items
  }, [recentClosed, tokens, tick])

  const noResults = openItems.length === 0 && closedItems.length === 0

  /**
   * `+` 按钮与它的菜单。
   *
   * 菜单走 [`PanePopover`] portal 到 body:标签栏的滚动区有
   * `overflow-x:auto; overflow-y:hidden`,祖先又有 `overflow:hidden`,
   * 内联渲染会被**从下方截断**(实测 bug)。portal 出去后 `z-index` 才真正生效。
   */
  const addButton = (
    <div className="pane-add-wrap">
      <button
        ref={addAnchorRef}
        type="button"
        className="pane-add"
        data-pane-add-trigger=""
        title={t('sidePaneAddTab')}
        aria-label={t('sidePaneAddTab')}
        aria-expanded={addOpen}
        aria-haspopup="menu"
        onClick={() => setAddOpen((open) => !open)}
      >
        <IconPlus size={14} />
      </button>
      <PanePopover
        anchorRef={addAnchorRef}
        open={addOpen}
        onClose={() => setAddOpen(false)}
        align="end"
        className="pane-menu"
        ariaLabel={t('sidePaneAddTab')}
      >
        {addable.map((type) => (
          <button
            key={type}
            type="button"
            role="menuitem"
            className="pane-menu-item"
            data-pane-add-item={type}
            onClick={() => {
              setAddOpen(false)
              onAdd(type)
            }}
          >
            <TabIcon type={type} size={14} />
            <span>{tabTypeLabel(type)}</span>
          </button>
        ))}
        {addable.length === 0 && <p className="pane-menu-empty">{t('sidePaneNoAddable')}</p>}
      </PanePopover>
    </div>
  )

  return (
    <div className="pane-tabs">
      {/* 左:标签总览 */}
      <div className="pane-overview-wrap">
        <button
          ref={overviewAnchorRef}
          type="button"
          className={`pane-overview${overviewOpen ? ' active' : ''}`}
          title={t('sidePaneTabOverview')}
          aria-label={t('sidePaneTabOverview')}
          aria-expanded={overviewOpen}
          aria-haspopup="dialog"
          onClick={() => setOverviewOpen((open) => !open)}
        >
          <IconStack size={14} />
        </button>
        {/* 同样 portal:总览比 `+` 菜单更高,更容易被下方内容裁掉。 */}
        <PanePopover
          anchorRef={overviewAnchorRef}
          open={overviewOpen}
          onClose={() => setOverviewOpen(false)}
          align="start"
          className="pane-overview-pop"
          role="dialog"
          ariaLabel={t('sidePaneTabOverview')}
        >
          <div className="pane-overview-search">
            <IconSearch size={13} />
            <input
              type="search"
              autoFocus
              value={query}
              placeholder={t('sidePaneSearchTabs')}
              onChange={(event) => setQuery(event.target.value)}
            />
          </div>
          <div className="pane-overview-list">
            {noResults && <p className="pane-overview-empty">{t('sidePaneNoTabs')}</p>}
            {openItems.length > 0 && (
              <>
                <p className="pane-overview-group">{t('sidePaneOpenTabs')}</p>
                {openItems.map(({ tab }) => (
                  <div
                    key={tab.id}
                    className={`pane-overview-item${tab.id === activeTabId ? ' active' : ''}`}
                  >
                    <button
                      type="button"
                      className="pane-overview-main"
                      onClick={() => {
                        onActivate(tab.id)
                        setOverviewOpen(false)
                      }}
                    >
                      <TabIcon type={tab.type} size={13} />
                      <span className="pane-overview-title">{tabTitle(tab)}</span>
                      <span className="pane-overview-time">{relativeTime(tab.openedAt)}</span>
                    </button>
                    <button
                      type="button"
                      className="pane-overview-close"
                      aria-label={t('sidePaneCloseTab')}
                      onClick={() => onClose(tab.id)}
                    >
                      <IconClose size={11} />
                    </button>
                  </div>
                ))}
              </>
            )}
            {closedItems.length > 0 && (
              <>
                <p className="pane-overview-group">{t('sidePaneRecentlyClosed')}</p>
                {closedItems.map(({ item }) => (
                  <div key={item.tab.id} className="pane-overview-item">
                    <button
                      type="button"
                      className="pane-overview-main"
                      onClick={() => {
                        onReopen(item.tab.id)
                        setOverviewOpen(false)
                      }}
                    >
                      <TabIcon type={item.tab.type} size={13} />
                      <span className="pane-overview-title">{tabTitle(item.tab)}</span>
                      <span className="pane-overview-time">{relativeTime(item.closedAt)}</span>
                    </button>
                  </div>
                ))}
              </>
            )}
          </div>
        </PanePopover>
      </div>

      {/* 中:标签滚动区 */}
      <div
        ref={viewportRef}
        className={`pane-tab-viewport${overflow.left && overflow.right ? ' mask-both' : overflow.left ? ' mask-left' : overflow.right ? ' mask-right' : ''}`}
        data-pane-tab-viewport=""
      >
        <div className="pane-tab-content" data-pane-tab-content="">
          {tabs.map((tab) => {
            const active = tab.id === activeTabId
            const title = tabTitle(tab)
            return (
              <div
                key={tab.id}
                data-tab-id={tab.id}
                data-active={active ? '' : undefined}
                className={`pane-tab${active ? ' active' : ''}${draggingId === tab.id ? ' dragging' : ''}`}
                title={title}
                draggable
                onDragStart={(event) => {
                  setDraggingId(tab.id)
                  event.dataTransfer.effectAllowed = 'move'
                  // Firefox 要求设置数据才启动拖拽。
                  event.dataTransfer.setData('text/plain', tab.id)
                }}
                onDragEnd={() => setDraggingId(null)}
                onDragOver={(event) => {
                  if (!draggingId || draggingId === tab.id) return
                  event.preventDefault()
                  event.dataTransfer.dropEffect = 'move'
                }}
                onDrop={(event) => {
                  event.preventDefault()
                  const from = draggingId
                  setDraggingId(null)
                  if (from && from !== tab.id) onReorder(from, tab.id)
                }}
                onClick={() => onActivate(tab.id)}
                onAuxClick={(event) => {
                  // 中键关闭(与浏览器一致)。
                  if (event.button === 1) {
                    event.preventDefault()
                    onClose(tab.id)
                  }
                }}
                onContextMenu={(event) => {
                  event.preventDefault()
                  onCloseOthers(tab.id)
                }}
                role="tab"
                aria-selected={active}
                tabIndex={active ? 0 : -1}
                onKeyDown={(event) => {
                  if (event.key === 'Enter' || event.key === ' ') {
                    event.preventDefault()
                    onActivate(tab.id)
                  }
                }}
              >
                <span className="pane-tab-icon" aria-hidden="true">
                  <TabIcon type={tab.type} size={13} />
                </span>
                <span className="pane-tab-label">{title}</span>
                {/* 右侧渐隐带 + 常驻关闭按钮 */}
                <span className="pane-tab-actions">
                  <span className="pane-tab-fade" aria-hidden="true" />
                  <span className="pane-tab-close-bg">
                    <button
                      type="button"
                      className="pane-tab-close"
                      aria-label={t('sidePaneCloseTab')}
                      onPointerDown={(event) => event.stopPropagation()}
                      onClick={(event) => {
                        event.preventDefault()
                        event.stopPropagation()
                        onClose(tab.id)
                      }}
                    >
                      <IconClose size={11} />
                    </button>
                  </span>
                </span>
              </div>
            )
          })}
          {/* 未溢出时 `+` 在滚动区末尾(跟随标签),溢出时移到右端固定位 */}
          {!overflowing && addButton}
        </div>
      </div>

      {/* 右:`+` 固定位(仅溢出时) */}
      {overflowing && (
        <div className="pane-actions">
          <button
            type="button"
            className="pane-close-all"
            title={t('sidePaneCloseAllTabs')}
            aria-label={t('sidePaneCloseAllTabs')}
            onClick={onCloseAll}
          >
            <IconClose size={13} />
          </button>
          {addButton}
        </div>
      )}
    </div>
  )
}
