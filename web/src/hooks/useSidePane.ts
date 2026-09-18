/**
 * 右侧面板的编排:标签创建/关闭、终端生命周期、浏览器绑定联动。
 *
 * # 为什么编排单独成 hook
 *
 * `SidePane` 是纯展示组件(收 props 回调),`sidePaneStore` 是纯状态。
 * 两者之间的**决策**——"打开终端要生成什么 id"、"关掉最后一个终端要不要
 * 连带收起面板"、"AI 请求可视化时怎么开浏览器标签"——集中在这里,既不污染
 * 展示层,也不污染纯状态层。
 */

import { useCallback, useEffect, useMemo, useRef } from 'react'
import {
  DRAFT_SCOPE,
  activateFile,
  closeAllTabs,
  closeFile,
  closeTab,
  findTab,
  hasType,
  openFileInTab,
  scopeKey,
  upsertTab,
  type ClosedTab,
  type SidePaneState,
  type SidePaneTab,
  type SidePaneTabType,
} from '../sidePane'
import {
  dropScope,
  forgetClosed as forgetClosedInStore,
  peekSidePane,
  rememberClosed as rememberClosedInStore,
  setSidePaneCollapsed,
  updateSidePane,
  useRecentClosed,
  useSidePaneCollapsed,
  useSidePaneState,
} from '../sidePaneStore'
import { fetchTerminals, type TerminalInfo } from '../terminalApi'
/** 生成一个稳定唯一的面板标签 id。 */
function newId(prefix: string): string {
  return `${prefix}:${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`
}

/**
 * 把各种形态的路径归一成「相对工作区根、以 `/` 分隔」的坐标。
 *
 * 四个触发点给的路径形态不一：对话里的 Markdown 链接多是 `src/main.rs`
 * 这种相对路径，而 edit/write 工具行与轮次产物列表给的是**相对 cwd 的
 * 路径**（可能是 `./src/main.rs`、带反斜杠、甚至偶尔是绝对路径）。
 *
 * 不归一的话，同一次点击从不同入口进来会得到两个不同 key，同一个文件在
 * 标签页里出现两条 —— 这正是“重复点击应复用”要避免的。
 *
 * 返回 null 表示这个路径不属于当前工作区（绝对路径在根之外、或含 `..`
 * 逃逸），调用方应当放弃打开而不是猜一个路径。
 */
export function normalizeWorkspaceRelative(
  raw: string,
  workspacePath: string | null,
): string | null {
  // 反斜杠统一成 `/`，去掉 `./` 前缀与重复斜杠。
  let path = raw.trim().replace(/\\/g, '/').replace(/\/{2,}/g, '/')
  while (path.startsWith('./')) path = path.slice(2)
  if (!path) return null
  const root = workspacePath?.trim().replace(/\\/g, '/').replace(/\/{2,}/g, '/')
  if (root && path.toLowerCase().startsWith(`${root.toLowerCase()}/`)) {
    // 工作区内的绝对路径：裁掉根前缀变成相对路径。
    path = path.slice(root.length + 1)
  } else if (/^[a-zA-Z]:\//.test(path) || path.startsWith('/')) {
    // 其他绝对路径（含盘符）：不属于本工作区，不猜。
    return null
  }
  // `..` 一律拒绝：后端也会拦，这里先拦可以避免把无效路径写进标签页。
  const segments: string[] = []
  for (const segment of path.split('/')) {
    if (segment === '' || segment === '.') continue
    if (segment === '..') return null
    segments.push(segment)
  }
  return segments.length > 0 ? segments.join('/') : null
}

export interface SidePaneController {
  state: SidePaneState
  collapsed: boolean
  recentClosed: ClosedTab[]
  /** 面板归属的 scope 键(终端/审查缓存用)。 */
  scope: string
  setCollapsed(collapsed: boolean): void
  toggle(): void
  update(updater: (current: SidePaneState) => SidePaneState): void
  /** 打开某个类型的面板(已开则只激活;审查/浏览器是单例)。 */
  openPanel(type: SidePaneTabType, options?: { cwd?: string; title?: string }): void
  /**
   * 把工作区内的一个文件打开到「文件读取」标签页（四类点击触发的统一入口）。
   *
   * 标签页不存在则创建并激活；已存在则把目标文件开进去（同一文件复用
   * 已有条目）。返回 false 表示没工作区/路径为空/路径不在本工作区，
   * 调用方可以据此保持默认行为或提示。
   */
  openFile(path: string): boolean
  /** 切换文件读取标签页内的激活条目。 */
  activateFile(path: string): void
  /** 关掉文件读取标签页里的一个条目（最后一条会连带关掉标签页）。 */
  closeFile(path: string): void
  closeTab(id: string): void
  rememberClosed(tabs: SidePaneTab[]): void
  forgetClosed(id: string): void
}

/**
 * 面板编排。
 *
 * @param sessionId 当前会话 id(null = 空白态)
 * @param workspacePath 当前工作区路径(终端 cwd 与审查面板用)
 */
export function useSidePaneController(
  sessionId: string | null,
  workspacePath: string | null,
): SidePaneController {
  const state = useSidePaneState(sessionId)
  const collapsed = useSidePaneCollapsed(sessionId)
  const recentClosed = useRecentClosed()
  const scope = scopeKey(sessionId)

  const setCollapsed = useCallback(
    (next: boolean) => setSidePaneCollapsed(sessionId, next),
    [sessionId],
  )

  const update = useCallback(
    (updater: (current: SidePaneState) => SidePaneState) =>
      updateSidePane(sessionId, updater),
    [sessionId],
  )

  /** 打开面板。单例类型已开时只激活,不新建。 */
  const openPanel = useCallback(
    (type: SidePaneTabType, options: { cwd?: string; title?: string } = {}) => {
      const current = peekSidePane(sessionId)
      // 审查、浏览器、工作区文件是单例:
      // - 审查:同一时刻只可能看一份 git 状态,开两个没有意义;
      // - 浏览器:denia 的 BrowserManager 是**单实例 + 内部 tab 列表**
      //   (见 crates/browser),开多个侧栏浏览器标签会全部显示同一个实例,
      //   反而让人以为"新开了一个浏览器"。
      // - 工作区文件:树是相对工作区根的唯一一份,两个标签必然同内容。
      // 终端相反:每个终端是独立 PTY,必须可多开。
      if (type === 'review' || type === 'browser' || type === 'files') {
        const existing = current.tabs.find((tab) => tab.type === type)
        if (existing) {
          updateSidePane(sessionId, (value) => ({ ...value, activeTabId: existing.id }))
          setSidePaneCollapsed(sessionId, false)
          return
        }
      }
      const tab: SidePaneTab = {
        id: newId(type),
        type,
        openedAt: Date.now(),
        ...(options.title ? { title: options.title } : {}),
        // 终端要有起点目录:显式传入优先,否则落到当前工作区。
        ...(type === 'terminal'
          ? { cwd: options.cwd ?? workspacePath ?? undefined }
          : options.cwd
            ? { cwd: options.cwd }
            : {}),
      }
      updateSidePane(sessionId, (value) => upsertTab(value, tab))
      setSidePaneCollapsed(sessionId, false)
    },
    [sessionId, workspacePath],
  )

  const closeTabById = useCallback(
    (id: string) => updateSidePane(sessionId, (current) => closeTab(current, id)),
    [sessionId],
  )

  /**
   * 打开文件到「文件读取」标签页：四类触发点（对话里的文件引用、
   * edit/write 工具行的路径、轮次产物列表、工作区文件树）共用这一个入口。
   *
   * 路径统一按**相对工作区根**存（与文件树、拖拽引用、@ 提及同一坐标），
   * 这样同一次点击从不同入口进来都能复用同一个条目。
   */
  const openFile = useCallback(
    (path: string) => {
      const raw = path.trim()
      if (!raw) return false
      const relative = normalizeWorkspaceRelative(raw, workspacePath)
      if (relative === null) return false
      updateSidePane(sessionId, (current) =>
        openFileInTab(current, { path: relative }, Date.now()),
      )
      // 展开面板：点了文件却看不到内容是最坏的一种“没反应”。
      setSidePaneCollapsed(sessionId, false)
      return true
    },
    [sessionId, workspacePath],
  )

  const activateFileEntry = useCallback(
    (path: string) => updateSidePane(sessionId, (current) => activateFile(current, path)),
    [sessionId],
  )

  const closeFileEntry = useCallback(
    (path: string) => updateSidePane(sessionId, (current) => closeFile(current, path)),
    [sessionId],
  )

  const rememberClosed = useCallback((tabs: SidePaneTab[]) => {
    rememberClosedInStore(tabs)
  }, [])

  const forgetClosed = useCallback((id: string) => {
    forgetClosedInStore(id)
  }, [])

  const toggle = useCallback(() => {
    setSidePaneCollapsed(sessionId, !collapsed)
  }, [sessionId, collapsed, setSidePaneCollapsed])

  /* ---- 会话被删除:清掉它的桶 ---- */

  useEffect(() => {
    return () => {
      // 组件卸载不等于会话删除,所以这里**不做**清理;
      // 真正的清理由 App 在删除会话时显式调用 `dropScope`。
    }
  }, [])

  return {
    state,
    collapsed,
    recentClosed,
    scope,
    setCollapsed,
    toggle,
    update,
    openPanel,
    openFile,
    activateFile: activateFileEntry,
    closeFile: closeFileEntry,
    closeTab: closeTabById,
    rememberClosed,
    forgetClosed,
  }
}

/**
 * 启动时对账:服务端还活着的终端,如果前端没有对应标签,就补一个。
 *
 * 场景:刷新页面(标签存在 localStorage,但 PTY 是服务端状态)。
 * 两边都可能多:
 * - 前端多 → 那个标签指向的 PTY 已经没了(服务端重启过),要清掉;
 * - 服务端多 → 有 PTY 但没标签(不该发生,但重启前端时可能),补标签。
 *
 * 不做这个对账的话,刷新后会看到标签在但终端一片空白且无法输入
 * (WebSocket 连上去立刻收到 `终端不存在` 并不断重连)。
 */
export function useTerminalReconcile(
  sessionId: string | null,
  enabled: boolean,
  onReconcile: (liveIds: string[]) => void,
): void {
  const onReconcileRef = useRef(onReconcile)
  onReconcileRef.current = onReconcile
  useEffect(() => {
    if (!enabled) return
    let disposed = false
    fetchTerminals()
      .then((snapshot: { terminals: TerminalInfo[] }) => {
        if (disposed) return
        onReconcileRef.current(snapshot.terminals.map((item) => item.id))
      })
      .catch(() => {
        // 拉不到就保持现状:下一次打开面板时还会再拉一次。
      })
    return () => {
      disposed = true
    }
    // sessionId 变化时重新对账(切会话 = 换一批终端)。
  }, [enabled, sessionId])
}

/** 当前面板里有哪些类型(菜单可用性判定用)。 */
export function usePanelAvailability(state: SidePaneState) {
  return useMemo(
    () => ({
      hasReview: hasType(state, 'review'),
      hasTerminal: hasType(state, 'terminal'),
      hasBrowser: hasType(state, 'browser'),
      activeTab: findTab(state, state.activeTabId),
      terminalTabs: state.tabs.filter((tab) => tab.type === 'terminal'),
    }),
    [state],
  )
}

export { DRAFT_SCOPE, closeAllTabs, dropScope }
