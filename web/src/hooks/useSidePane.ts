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
  closeAllTabs,
  closeTab,
  findTab,
  hasType,
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
