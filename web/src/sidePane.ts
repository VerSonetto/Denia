/**
 * 右侧面板的标签模型(纯函数,无 React)。
 *
 * 抄 ZCode 的 `sidePaneState` 形态:
 * ```
 * { tabs: SidePaneTab[], activeTabId: string }
 * ```
 * 但它比 ZCode 简化了一处:ZCode 把**所有工作区/任务**的标签放在同一个
 * 数组里,渲染时按 owner 过滤(见其 `Qu()`),于是每次切换都要遍历全表。
 * denia 这里直接**按 scope 分桶**存,每个 scope 一份独立的 `{tabs,activeTabId}`,
 * 切会话就是换一个桶 —— O(1) 且天然不会串台。
 *
 * 这一层必须是纯函数:标签栏的拖拽/关闭/重排逻辑最容易写错,放在纯函数里
 * 才能用 `check-side-pane.mjs` 直接断言(见 web/scripts)。
 */

/** 面板里能放的东西。与 ZCode 的 tab type 一一对应(取其子集)。 */
export type SidePaneTabType =
  /** 审查:Git 状态与 Diff。单例。 */
  | 'review'
  /** 终端:交互式 PTY。可多开。 */
  | 'terminal'
  /** 浏览器:内嵌实时画面。可多开。 */
  | 'browser'
  /** 工作区文件:文件系统目录树(与 Git 无关)。单例。 */
  | 'files'

export interface SidePaneTab {
  id: string
  type: SidePaneTabType
  /** 创建时刻(epoch ms):标签总览里显示"刚刚/3 分钟前"。 */
  openedAt: number
  /** 终端/浏览器 tab 的标题(终端的 shell 标签、浏览器页面标题)。 */
  title?: string
  /** 终端 tab 的工作目录。 */
  cwd?: string
}

export interface SidePaneState {
  tabs: SidePaneTab[]
  activeTabId: string
}

/** 空状态:没有标签。 */
export const EMPTY_SIDE_PANE: SidePaneState = { tabs: [], activeTabId: '' }

/** 最近关闭标签的保留上限(抄 ZCode 的 `Ffe = 8`)。 */
export const RECENT_CLOSED_LIMIT = 8

export interface ClosedTab {
  tab: SidePaneTab
  closedAt: number
}

/* ---- 查询 ---- */

export function findTab(state: SidePaneState | null, id: string): SidePaneTab | null {
  if (!state) return null
  return state.tabs.find((tab) => tab.id === id) ?? null
}

export function activeTab(state: SidePaneState | null): SidePaneTab | null {
  if (!state) return null
  return findTab(state, state.activeTabId)
}

/** 该类型是否已经开着(审查是单例,靠它避免重复开)。 */
export function hasType(state: SidePaneState | null, type: SidePaneTabType): boolean {
  if (!state) return false
  return state.tabs.some((tab) => tab.type === type)
}

/** 该类型的全部标签(终端/浏览器可能多个)。 */
export function tabsOfType(state: SidePaneState | null, type: SidePaneTabType): SidePaneTab[] {
  if (!state) return []
  return state.tabs.filter((tab) => tab.type === type)
}

/* ---- 变更 ---- */

/**
 * 插入或更新一个标签,并激活它。
 *
 * 同 id 已存在时**原地替换**(保留位置):重开最近关闭的标签时,用户期望
 * 它回到原来的位置,而不是跑到最右边。
 */
export function upsertTab(
  state: SidePaneState | null,
  tab: SidePaneTab,
  options: { activate?: boolean } = {},
): SidePaneState {
  const base = state ?? EMPTY_SIDE_PANE
  const index = base.tabs.findIndex((item) => item.id === tab.id)
  const tabs = index >= 0
    ? base.tabs.map((item) => (item.id === tab.id ? tab : item))
    : [...base.tabs, tab]
  const activate = options.activate !== false
  return {
    tabs,
    activeTabId: activate ? tab.id : base.activeTabId,
  }
}

/** 激活一个标签(不存在则原样返回)。 */
export function activateTab(state: SidePaneState | null, id: string): SidePaneState {
  if (!state) return EMPTY_SIDE_PANE
  if (!state.tabs.some((tab) => tab.id === id)) return state
  if (state.activeTabId === id) return state
  return { ...state, activeTabId: id }
}

/**
 * 关闭一个标签,并选出下一个激活项。
 *
 * 选择顺序(抄 ZCode 的 `od()` 语义):优先激活**右边**那个 ——
 * 关掉中间标签时焦点前移比后退更符合直觉(与浏览器一致);
 * 右边没有才回退到左边;都没有就清空。
 */
export function closeTab(state: SidePaneState | null, id: string): SidePaneState {
  if (!state) return EMPTY_SIDE_PANE
  const index = state.tabs.findIndex((tab) => tab.id === id)
  if (index < 0) return state
  const tabs = state.tabs.filter((tab) => tab.id !== id)
  if (tabs.length === 0) return EMPTY_SIDE_PANE
  if (state.activeTabId !== id) return { tabs, activeTabId: state.activeTabId }
  const next = tabs[Math.min(index, tabs.length - 1)]
  return { tabs, activeTabId: next.id }
}

/** 关闭除 `keepId` 外的全部标签。 */
export function closeOtherTabs(state: SidePaneState | null, keepId: string): SidePaneState {
  if (!state) return EMPTY_SIDE_PANE
  const kept = state.tabs.find((tab) => tab.id === keepId)
  if (!kept) return state
  return { tabs: [kept], activeTabId: kept.id }
}

/** 关闭全部标签。 */
export function closeAllTabs(): SidePaneState {
  return EMPTY_SIDE_PANE
}

/**
 * 拖拽重排:把 `fromId` 移到 `toId` 的位置。
 *
 * 用 splice 两次(取出→插入)而不是相邻交换:一次跨多个位置拖动也正确。
 */
export function reorderTab(state: SidePaneState | null, fromId: string, toId: string): SidePaneState {
  if (!state || fromId === toId) return state ?? EMPTY_SIDE_PANE
  const from = state.tabs.findIndex((tab) => tab.id === fromId)
  const to = state.tabs.findIndex((tab) => tab.id === toId)
  if (from < 0 || to < 0) return state
  const tabs = [...state.tabs]
  const [moved] = tabs.splice(from, 1)
  if (!moved) return state
  tabs.splice(to, 0, moved)
  return { ...state, tabs }
}

/* ---- 最近关闭 ---- */

/** 记录一批关闭的标签(新记录在前,超限截断)。 */
export function pushClosed(
  history: ClosedTab[],
  tabs: SidePaneTab[],
  closedAt: number,
): ClosedTab[] {
  if (tabs.length === 0) return history
  const ids = new Set(tabs.map((tab) => tab.id))
  const fresh = tabs.map((tab) => ({ tab, closedAt }))
  return [...fresh, ...history.filter((item) => !ids.has(item.tab.id))].slice(
    0,
    RECENT_CLOSED_LIMIT,
  )
}

/**
 * 从最近关闭里取出一个用于重开。
 *
 * 浏览器标签**必须换新 id**:它的 id 绑定了内嵌浏览器的实例身份,旧实例
 * 已经销毁,复用 id 会让前端去认领一个不存在的会话(表现为"重开后空白且
 * 无法导航")。ZCode 在 `Te()` 里做了同样的事(`browser:${新随机}`)。
 */
export function reviveClosed(tab: SidePaneTab, newId: string): SidePaneTab {
  if (tab.type === 'browser') {
    return { ...tab, id: newId, title: undefined }
  }
  return { ...tab, id: newId }
}

/* ---- 作用域键 ---- */

/**
 * 面板状态的归属键。
 *
 * 抄 ZCode 的 `(workspaceKey, ownerTaskId)` 双元组,但 denia 的会话已经
 * 隐含了工作区(cwd 不可变),所以直接用 `sessionId`;没有活跃会话(空白态)
 * 时用 `__draft__` 桶 —— 这样"还没发第一条消息"时也能开终端看目录。
 */
export const DRAFT_SCOPE = '__draft__'

export function scopeKey(sessionId: string | null): string {
  return sessionId ?? DRAFT_SCOPE
}
