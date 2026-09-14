/**
 * 右侧面板外壳:标签栏 + 内容区 + 空态引导页 + 拖拽调宽。
 *
 * # 三条核心行为(全部对齐 ZCode)
 *
 * 1. **折叠不卸载**。折叠时内容仍在 DOM 里,只是 `display:none`。终端与
 *    浏览器的状态因此不丢 —— 这是"切回来即所见"的前提。代价是折叠期间
 *    终端仍在跑(那是期望行为:AI 在后台跑构建,用户收起面板不该中断它)。
 *
 * 2. **宽度记忆**。拖拽调宽写 localStorage(全局一份,不按会话分)。
 *    展开时用记忆值,没有则取父容器的 45%。上限 65% —— 再宽就把对话
 *    区挤没了;下限 240px —— 再窄标签栏放不下。
 *
 * 3. **空态引导页**。面板展开但没标签时,显示可打开的面板按钮网格而不是
 *    空白。宽窄两态用 CSS 容器查询切换(窄 → 竖排列表,宽 → 卡片网格),
 *    零 JS 分支。
 */

import { lazy, Suspense, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react'
import { t } from '../i18n'
import {
  DEFAULT_PANE_RATIO,
  MAX_PANE_RATIO,
  MIN_PANE_WIDTH,
  setSidePaneWidth,
  useSidePaneWidth,
} from '../sidePaneStore'
import {
  closeAllTabs,
  closeOtherTabs,
  closeTab,
  reorderTab,
  reviveClosed,
  upsertTab,
  type SidePaneState,
  type SidePaneTab,
  type SidePaneTabType,
} from '../sidePane'
import { availablePanels } from '../panelRegistry'
import { SidePaneTabs, TabIcon, tabTypeLabel } from './SidePaneTabs'
import { IconChevronDown, IconPlus } from './icons'
import './SidePane.css'

export interface SidePaneProps {
  /** 当前会话 id(null = 空白态,draft 桶)。 */
  sessionId: string | null
  /** 面板状态(来自 store)。 */
  state: SidePaneState
  /** 是否折叠。 */
  collapsed: boolean
  /** 工作区路径(审查面板与终端 cwd 用)。 */
  workspacePath: string | null
  /** 内嵌浏览器是否可用。 */
  supportsBrowser: boolean
  /** 最近关闭的标签。 */
  recentClosed: import('../sidePane').ClosedTab[]
  /** 面板状态变更回调(由 App 转发到 store)。 */
  onChange(updater: (current: SidePaneState) => SidePaneState): void
  /** 折叠态变更。 */
  onCollapsedChange(collapsed: boolean): void
  /** 记录一批关闭的标签到历史。 */
  onRememberClosed(tabs: SidePaneTab[]): void
  /** 从历史移除一条(重开后)。 */
  onForgetClosed(id: string): void
  /** 终端标签整组渲染(由 App 注入 TerminalHost,见下方注释)。 */
  renderTerminals?(tabs: SidePaneTab[], activeId: string, paneVisible: boolean): React.ReactNode
  /** 浏览器 tab 的渲染。 */
  renderBrowser?(tab: SidePaneTab, visible: boolean): React.ReactNode
  /** 打开某个类型的面板(创建 tab + 展开)。 */
  onOpenPanel(type: SidePaneTabType): void
}

export function SidePane({
  sessionId,
  state,
  collapsed,
  workspacePath,
  supportsBrowser,
  recentClosed,
  onChange,
  onCollapsedChange,
  onRememberClosed,
  onForgetClosed,
  renderTerminals,
  renderBrowser,
  onOpenPanel,
}: SidePaneProps) {
  const rootRef = useRef<HTMLDivElement | null>(null)
  const storedWidth = useSidePaneWidth()
  const [dragging, setDragging] = useState(false)
  /** 拖动时的实时宽度(px);null 表示用 store 值/CSS 默认。 */
  const [liveWidth, setLiveWidth] = useState<number | null>(null)
  const dragRef = useRef<{ startX: number; startWidth: number } | null>(null)

  /* ---- 宽度解析 ---- */

  const resolveDefault = useCallback(() => {
    const parent = rootRef.current?.parentElement
    const parentWidth = parent?.getBoundingClientRect().width ?? 0
    if (parentWidth <= 0) return MIN_PANE_WIDTH
    return Math.max(MIN_PANE_WIDTH, Math.round(parentWidth * DEFAULT_PANE_RATIO))
  }, [])

  /** 实际生效宽度:拖动中 → 实时值;否则 → 记忆值;都没有 → 父容器 45%。 */
  const [resolvedWidth, setResolvedWidth] = useState(0)
  useLayoutEffect(() => {
    if (collapsed) return
    if (liveWidth !== null) {
      setResolvedWidth(liveWidth)
      return
    }
    setResolvedWidth(storedWidth > 0 ? storedWidth : resolveDefault())
  }, [collapsed, liveWidth, storedWidth, resolveDefault])

  // 父容器尺寸变化(窗口缩放/左栏开合)时重算默认宽度 —— 但只在"没有记忆值"
  // 时跟随,否则用户调过的宽度会被窗口缩放改掉。
  useEffect(() => {
    if (collapsed || storedWidth > 0 || liveWidth !== null) return
    const parent = rootRef.current?.parentElement
    if (!parent || typeof ResizeObserver === 'undefined') return
    const observer = new ResizeObserver(() => setResolvedWidth(resolveDefault()))
    observer.observe(parent)
    return () => observer.disconnect()
  }, [collapsed, storedWidth, liveWidth, resolveDefault])

  /* ---- 拖拽调宽 ---- */

  const beginDrag = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      if (event.button !== 0) return
      event.preventDefault()
      const width = rootRef.current?.getBoundingClientRect().width ?? resolvedWidth
      dragRef.current = { startX: event.clientX, startWidth: width }
      setDragging(true)
      event.currentTarget.setPointerCapture(event.pointerId)
    },
    [resolvedWidth],
  )

  const moveDrag = useCallback((event: React.PointerEvent<HTMLDivElement>) => {
    const drag = dragRef.current
    if (!drag) return
    const parent = rootRef.current?.parentElement
    const parentWidth = parent?.getBoundingClientRect().width ?? 0
    const max = parentWidth > 0 ? Math.round(parentWidth * MAX_PANE_RATIO) : 1200
    // 面板在右侧:鼠标往左移 → 面板变宽,所以是 `startX - clientX`。
    const delta = drag.startX - event.clientX
    const next = Math.max(MIN_PANE_WIDTH, Math.min(max, drag.startWidth + delta))
    setLiveWidth(next)
  }, [])

  const endDrag = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      if (!dragRef.current) return
      dragRef.current = null
      setDragging(false)
      const width = liveWidth
      setLiveWidth(null)
      if (width !== null) setSidePaneWidth(width)
      try {
        event.currentTarget.releasePointerCapture(event.pointerId)
      } catch {
        /* 指针已释放:忽略。 */
      }
    },
    [liveWidth],
  )

  /** 键盘调宽:方向键 ±16px,Shift ±64px(无障碍要求)。 */
  const keyDrag = useCallback(
    (event: React.KeyboardEvent<HTMLDivElement>) => {
      if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return
      event.preventDefault()
      const step = event.shiftKey ? 64 : 16
      const base = liveWidth ?? resolvedWidth
      const parent = rootRef.current?.parentElement
      const parentWidth = parent?.getBoundingClientRect().width ?? 0
      const max = parentWidth > 0 ? Math.round(parentWidth * MAX_PANE_RATIO) : 1200
      // 左方向键 = 变宽(面板在右侧)。
      const delta = event.key === 'ArrowLeft' ? step : -step
      const next = Math.max(MIN_PANE_WIDTH, Math.min(max, base + delta))
      setLiveWidth(next)
      setSidePaneWidth(next)
      // 键盘调整不需要保持 live 态:直接落库,下一帧由 store 驱动。
      window.setTimeout(() => setLiveWidth(null), 0)
    },
    [liveWidth, resolvedWidth],
  )

  /* ---- 标签动作 ---- */

  const handleClose = useCallback(
    (id: string) => {
      const tab = state.tabs.find((item) => item.id === id)
      if (tab) onRememberClosed([tab])
      onChange((current) => closeTab(current, id))
    },
    [state.tabs, onChange, onRememberClosed],
  )

  const handleCloseOthers = useCallback(
    (id: string) => {
      const others = state.tabs.filter((tab) => tab.id !== id)
      if (others.length > 0) onRememberClosed(others)
      onChange((current) => closeOtherTabs(current, id))
    },
    [state.tabs, onChange, onRememberClosed],
  )

  const handleCloseAll = useCallback(() => {
    if (state.tabs.length > 0) onRememberClosed(state.tabs)
    onChange(() => closeAllTabs())
  }, [state.tabs, onChange, onRememberClosed])

  const handleReopen = useCallback(
    (id: string) => {
      const closed = recentClosed.find((item) => item.tab.id === id)
      if (!closed) return
      // 浏览器标签换新 id:旧的内嵌实例已销毁,复用 id 会认领到不存在的会话。
      const nextId =
        closed.tab.type === 'browser'
          ? `browser:${Math.random().toString(36).slice(2, 10)}`
          : closed.tab.id
      const revived = reviveClosed(closed.tab, nextId)
      onChange((current) => upsertTab(current, { ...revived, openedAt: Date.now() }))
      onForgetClosed(id)
    },
    [recentClosed, onChange, onForgetClosed],
  )

  const handleReorder = useCallback(
    (fromId: string, toId: string) => onChange((current) => reorderTab(current, fromId, toId)),
    [onChange],
  )

  /* ---- 可打开的面板 ---- */

  const addable = useMemo(
    () =>
      availablePanels({
        workspacePath,
        isOpen: (type) => state.tabs.some((tab) => tab.type === type),
        supportsBrowser,
      }).map((panel) => panel.type),
    [workspacePath, state.tabs, supportsBrowser],
  )

  const openable = useMemo(
    () =>
      availablePanels({
        workspacePath,
        isOpen: () => false,
        supportsBrowser,
      }),
    [workspacePath, supportsBrowser],
  )

  const activeId = state.tabs.some((tab) => tab.id === state.activeTabId)
    ? state.activeTabId
    : (state.tabs[state.tabs.length - 1]?.id ?? '')

  const isEmpty = state.tabs.length === 0

  /** 终端标签单独拎出来整组渲染;其余按标签逐个渲染。 */
  const terminalTabs = useMemo(
    () => state.tabs.filter((tab) => tab.type === 'terminal'),
    [state.tabs],
  )
  const nonTerminalTabs = useMemo(
    () => state.tabs.filter((tab) => tab.type !== 'terminal'),
    [state.tabs],
  )

  return (
    <aside
      className={`side-pane${collapsed ? ' collapsed' : ''}${dragging ? ' dragging' : ''}`}
      ref={rootRef}
      data-collapsed={collapsed ? 'true' : undefined}
      style={collapsed ? undefined : { width: resolvedWidth > 0 ? `${resolvedWidth}px` : undefined }}
      aria-hidden={collapsed}
    >
      {/* 拖拽把手:面板左侧边缘 */}
      {!collapsed && (
        <div
          className="side-pane-resizer"
          role="separator"
          aria-orientation="vertical"
          aria-label={t('sidePaneResize')}
          aria-valuenow={resolvedWidth}
          aria-valuemin={MIN_PANE_WIDTH}
          tabIndex={0}
          onPointerDown={beginDrag}
          onPointerMove={moveDrag}
          onPointerUp={endDrag}
          onPointerCancel={endDrag}
          onLostPointerCapture={endDrag}
          onKeyDown={keyDrag}
        />
      )}

      <div className="side-pane-inner">
        {isEmpty ? (
          /* 空态引导页:容器查询在宽/窄两态间自动切换布局 */
          <div className="pane-open-shell">
            <div className="pane-open-scroll">
              <div className="pane-open-content">
                <div className="pane-open-head">
                  <h2 className="pane-open-title">{t('sidePaneOpenTab')}</h2>
                  <p className="pane-open-desc">{t('sidePaneOpenTabDesc')}</p>
                </div>
                <div className="pane-open-list">
                  {openable.map((panel) => (
                    <button
                      key={panel.type}
                      type="button"
                      className="pane-open-button"
                      data-pane-open-item={panel.type}
                      onClick={() => onOpenPanel(panel.type)}
                    >
                      <TabIcon type={panel.type} size={15} />
                      <span className="pane-open-label">{tabTypeLabel(panel.type)}</span>
                    </button>
                  ))}
                </div>
              </div>
            </div>
            <button
              type="button"
              className="pane-open-collapse"
              title={t('sidePaneCollapse')}
              aria-label={t('sidePaneCollapse')}
              onClick={() => onCollapsedChange(true)}
            >
              <IconChevronDown size={14} />
            </button>
          </div>
        ) : (
          <>
            <SidePaneTabs
              tabs={state.tabs}
              activeTabId={activeId}
              recentClosed={recentClosed}
              onActivate={(id) => onChange((current) => ({ ...current, activeTabId: id }))}
              onClose={handleClose}
              onCloseOthers={handleCloseOthers}
              onCloseAll={handleCloseAll}
              onReorder={handleReorder}
              onReopen={handleReopen}
              onAdd={onOpenPanel}
              addable={addable}
            />
            <div className="side-pane-body">
              {/* 终端标签**整组渲染**:TerminalHost 自己给每个终端挂槽并按
                  激活态显隐。逐个包在下面的 per-tab 循环里会导致切标签时
                  终端组件被卸载重建(xterm 的 scrollback 与 WebSocket 全丢)。 */}
              {terminalTabs.length > 0 &&
                renderTerminals?.(terminalTabs, activeId, !collapsed)}
              {nonTerminalTabs.map((tab) => {
                const visible = tab.id === activeId
                return (
                  <div
                    key={tab.id}
                    className="side-pane-tab-panel"
                    data-visible={visible ? 'true' : undefined}
                    aria-hidden={visible ? undefined : true}
                  >
                    {tab.type === 'review' &&
                      (workspacePath ? (
                        <ReviewSlot workspacePath={workspacePath} sessionId={sessionId} />
                      ) : (
                        <div className="pane-placeholder">{t('sidePaneNeedsWorkspace')}</div>
                      ))}
                    {tab.type === 'files' &&
                      (workspacePath ? (
                        <FileTreeSlot workspacePath={workspacePath} sessionId={sessionId} />
                      ) : (
                        <div className="pane-placeholder">{t('sidePaneNeedsWorkspace')}</div>
                      ))}
                    {tab.type === 'browser' && renderBrowser?.(tab, visible)}
                  </div>
                )
              })}
            </div>
          </>
        )}
      </div>

      {/* 折叠后的展开把手:细条贴在右边缘 */}
      {collapsed && (
        <button
          type="button"
          className="side-pane-expand"
          title={t('sidePaneExpand')}
          aria-label={t('sidePaneExpand')}
          onClick={() => onCollapsedChange(false)}
        >
          <IconPlus size={13} />
        </button>
      )}
    </aside>
  )
}

/** 审查面板槽:懒加载(它带着 diff 逻辑,不必进首包)。 */
const LazyReviewPanel = lazy(() => import('./ReviewPanel'))

/** 工作区文件面板槽:同样懒加载(树只在真打开该标签时才需要)。 */
const LazyFileTreePanel = lazy(() => import('./FileTreePanel'))

function FileTreeSlot({ workspacePath, sessionId }: { workspacePath: string; sessionId: string | null }) {
  return (
    <Suspense fallback={<div className="pane-placeholder">{t('loading')}</div>}>
      <LazyFileTreePanel workspacePath={workspacePath} sessionId={sessionId} />
    </Suspense>
  )
}

function ReviewSlot({ workspacePath, sessionId }: { workspacePath: string; sessionId: string | null }) {
  return (
    <Suspense fallback={<div className="pane-placeholder">{t('loading')}</div>}>
      <LazyReviewPanel workspacePath={workspacePath} sessionId={sessionId} />
    </Suspense>
  )
}

export default SidePane
