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
  /**
   * 文件读取:只读查看工作区内文件内容。单例。
   *
   * 没有手动入口 —— 不能从 `+` 菜单新建、不能手输路径。它只能由四类点击
   * 触发（对话里的文件引用 / edit·write 工具行的路径 / 轮次产物列表 /
   * 工作区文件树的文件项），具体见 `hooks/useSidePane` 的 `openFile`。
   */
  | 'file'

/** 文件读取标签页里已打开的一个文件条目。 */
export interface OpenedFile {
  /** 相对工作区根的路径（以 `/` 分隔）：去重键，与文件树/拖拽引用同一坐标。 */
  path: string
  /** 文件短名（条目标签上显示）。 */
  name: string
  openedAt: number
}

export interface SidePaneTab {
  id: string
  type: SidePaneTabType
  /** 创建时刻(epoch ms):标签总览里显示"刚刚/3 分钟前"。 */
  openedAt: number
  /** 终端/浏览器 tab 的标题(终端的 shell 标签、浏览器页面标题)。 */
  title?: string
  /** 终端 tab 的工作目录。 */
  cwd?: string
  /**
   * 文件读取标签页已打开的文件列表（只有 `file` 类型用）。
   *
   * 存在标签上而不是单独一份 store 里：标签被关掉时这些条目跟着消失，
   * 不需要额外的清理路径 —— 而“关闭后再点触发点要能重新创建”正是要求。
   */
  files?: OpenedFile[]
  /** 当前激活的文件路径（`files` 里的某一项）。 */
  activeFile?: string
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

/* ---- 文件读取标签页 ---- */

/** 文件读取标签页的 id。单例：面板里最多只有一个，所以用固定 id。 */
export const FILE_TAB_ID = 'file'

/** 从路径取短名（兼容 `/` 与 `\`）。 */
export function fileBasename(path: string): string {
  const index = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'))
  return index < 0 ? path : path.slice(index + 1)
}

/** 找到文件读取标签页（没有则 null）。 */
export function fileTab(state: SidePaneState | null): SidePaneTab | null {
  if (!state) return null
  return state.tabs.find((tab) => tab.type === 'file') ?? null
}

/**
 * 打开一个文件到文件读取标签页。
 *
 * 四种情况合一：
 * - 标签不存在 → 建一个（连同首个文件条目），并激活；
 * - 标签已存在但文件未开过 → 追加条目并激活它（**不**重建标签，也不
 *   重复添加已开过的文件）；
 * - 文件已打开过 → 只把 `activeFile` 指过去（复用已有条目）；
 * - 无论哪种情况，标签本身都保持“当前激活”（用户点文件就是要看它）。
 *
 * 返回新状态；调用方负责写回 store。
 */
export function openFileInTab(
  state: SidePaneState | null,
  file: { path: string; name?: string },
  openedAt: number,
): SidePaneState {
  const base = state ?? EMPTY_SIDE_PANE
  const path = file.path.trim()
  if (!path) return base
  const name = file.name?.trim() || fileBasename(path)
  const existing = fileTab(base)
  if (existing === null) {
    const tab: SidePaneTab = {
      id: FILE_TAB_ID,
      type: 'file',
      openedAt,
      files: [{ path, name, openedAt }],
      activeFile: path,
    }
    return upsertTab(base, tab)
  }
  const files = existing.files ?? []
  // 已开过：只切激活项，不重复添加（条目顺序保持稳定，不因重复点击而跳动）。
  const nextFiles = files.some((item) => item.path === path)
    ? files
    : [...files, { path, name, openedAt }]
  const tabs = base.tabs.map((tab) =>
    tab.id === existing.id ? { ...tab, files: nextFiles, activeFile: path } : tab,
  )
  return { tabs, activeTabId: existing.id }
}

/** 切换文件读取标签页内的激活条目（不存在则原样返回）。 */
export function activateFile(state: SidePaneState | null, path: string): SidePaneState {
  const existing = fileTab(state)
  if (existing === null) return state ?? EMPTY_SIDE_PANE
  if (existing.activeFile === path) return state ?? EMPTY_SIDE_PANE
  if (!(existing.files ?? []).some((item) => item.path === path)) return state ?? EMPTY_SIDE_PANE
  return {
    tabs: state!.tabs.map((tab) => (tab.id === existing.id ? { ...tab, activeFile: path } : tab)),
    activeTabId: existing.id,
  }
}

/**
 * 从文件读取标签页里关掉一个文件条目。
 *
 * 关掉最后一个条目时**连带关掉标签页**：留一个没有任何条目的空壳既没有
 * 内容可看，也没有入口可以再往里加（该标签页本来就没有手动入口）。
 */
export function closeFile(state: SidePaneState | null, path: string): SidePaneState {
  const existing = fileTab(state)
  if (existing === null) return state ?? EMPTY_SIDE_PANE
  const files = (existing.files ?? []).filter((item) => item.path !== path)
  if (files.length === 0) return closeTab(state, existing.id)
  // 关掉的正是当前激活项：顺位接上邻居（优先右边，与标签栏关闭同一语义）。
  let activeFile = existing.activeFile
  if (activeFile === path) {
    const order = (existing.files ?? []).map((item) => item.path)
    const index = order.indexOf(path)
    const next = order[index + 1] ?? order[index - 1] ?? files[files.length - 1]!.path
    activeFile = files.some((item) => item.path === next) ? next : files[0]!.path
  }
  return {
    tabs: state!.tabs.map((tab) => (tab.id === existing.id ? { ...tab, files, activeFile } : tab)),
    activeTabId: state!.activeTabId,
  }
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
