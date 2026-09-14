/**
 * 右侧面板的 store:按 scope 分桶的状态 + 折叠态 + 最近关闭 + 尺寸记忆。
 *
 * # 为什么独立于 appStore
 *
 * 面板状态的生命周期与全局会话状态**不同**:
 * - 全局状态(会话列表/工作区)由服务端权威,前端只是缓存;
 * - 面板状态是**纯前端偏好**,要落 localStorage 跨刷新保留,且按会话分桶。
 *
 * 混在一起会让 appStore 的每次 `setState` 都带上面板字段,面板的频繁变更
 * (拖拽、开合)又反过来触发会话订阅者重渲染。分开两个 store,订阅面互不干扰。
 *
 * # 持久化策略
 *
 * - **标签集合**:localStorage(跨刷新、跨重启保留),按 scope 分桶;
 * - **折叠态**:localStorage,按 scope 分桶(抄 ZCode 的
 *   `sidePaneCollapsedByOwner`);
 * - **展开宽度**:localStorage,全局一份(不按会话分 —— 用户对面板宽度的
 *   预期是"我调过一次就一直这样",按会话分反而要在每个会话重调)。
 *
 * 分桶表本身也做上限:ZCode 用 LRU 50(`dd` Map)。这里同样限 50 个 scope,
 * 防止长期使用后 localStorage 无限增长(每个会话都开过终端的话)。
 */

import { useSyncExternalStore } from 'react'
import {
  EMPTY_SIDE_PANE,
  type ClosedTab,
  type SidePaneState,
  type SidePaneTab,
  type SidePaneTabType,
  scopeKey,
} from './sidePane'

const SCOPES_KEY = 'denia.sidePane.scopes'
const COLLAPSED_KEY = 'denia.sidePane.collapsed'
const WIDTH_KEY = 'denia.sidePane.width'
const RECENT_KEY = 'denia.sidePane.recentClosed'

/** 分桶上限(抄 ZCode 的 50)。 */
const SCOPE_LIMIT = 50

/** 展开宽度默认值:父容器的 45%(抄 ZCode 的 `kjt = 0.45`)。 */
export const DEFAULT_PANE_RATIO = 0.45
/** 面板最小宽度:低于此值标签栏放不下,不如折叠。 */
export const MIN_PANE_WIDTH = 240
/** 面板最大宽度占比:再宽就把对话区挤没了。 */
export const MAX_PANE_RATIO = 0.65

interface SidePaneSnapshot {
  /** scope 键 → 面板状态。 */
  scopes: Record<string, SidePaneState>
  /** scope 键 → 是否折叠。 */
  collapsed: Record<string, boolean>
  /** 展开宽度(px);0 表示"还没量过,按比例算"。 */
  width: number
  /** 最近关闭的标签(全局一份,不按 scope 分 —— 用户视角就是"我刚关掉的")。 */
  recentClosed: ClosedTab[]
}

/* ---- 读写 localStorage ----
 *
 * 每个 catch 只兜"storage 不可用/内容损坏",不掩盖编程错误:
 * 解析失败时回退到空对象,而不是让整个控制台白屏。
 */

function readJson<T>(key: string, fallback: T): T {
  try {
    const raw = window.localStorage.getItem(key)
    if (!raw) return fallback
    const parsed: unknown = JSON.parse(raw)
    if (!parsed || typeof parsed !== 'object') return fallback
    return parsed as T
  } catch {
    return fallback
  }
}

function writeJson(key: string, value: unknown): void {
  try {
    window.localStorage.setItem(key, JSON.stringify(value))
  } catch {
    /* 写失败只影响刷新恢复,不影响当前页面。 */
  }
}

/** 载入时校验分桶内容,丢掉结构不对的桶(旧版本残留/手改坏了)。 */
function readScopes(): Record<string, SidePaneState> {
  const raw = readJson<Record<string, unknown>>(SCOPES_KEY, {})
  const out: Record<string, SidePaneState> = {}
  for (const [key, value] of Object.entries(raw)) {
    if (!value || typeof value !== 'object') continue
    const candidate = value as Partial<SidePaneState>
    if (!Array.isArray(candidate.tabs)) continue
    const tabs = candidate.tabs.filter(isValidTab)
    if (tabs.length === 0) continue
    const activeTabId =
      typeof candidate.activeTabId === 'string' &&
      tabs.some((tab) => tab.id === candidate.activeTabId)
        ? candidate.activeTabId
        : tabs[tabs.length - 1]!.id
    out[key] = { tabs, activeTabId }
  }
  return out
}

function isValidTab(value: unknown): value is SidePaneTab {
  if (!value || typeof value !== 'object') return false
  const tab = value as Partial<SidePaneTab>
  if (typeof tab.id !== 'string' || tab.id.length === 0) return false
  if (!isTabType(tab.type)) return false
  if (typeof tab.openedAt !== 'number') return false
  return true
}

function isTabType(value: unknown): value is SidePaneTabType {
  return value === 'review' || value === 'terminal' || value === 'browser'
}

function readCollapsed(): Record<string, boolean> {
  const raw = readJson<Record<string, unknown>>(COLLAPSED_KEY, {})
  const out: Record<string, boolean> = {}
  for (const [key, value] of Object.entries(raw)) {
    if (typeof value === 'boolean') out[key] = value
  }
  return out
}

function readWidth(): number {
  try {
    const raw = window.localStorage.getItem(WIDTH_KEY)
    if (!raw) return 0
    const value = Number(raw)
    return Number.isFinite(value) && value > 0 ? value : 0
  } catch {
    return 0
  }
}

function readRecent(): ClosedTab[] {
  const raw = readJson<unknown[]>(RECENT_KEY, [])
  if (!Array.isArray(raw)) return []
  const out: ClosedTab[] = []
  for (const value of raw) {
    if (!value || typeof value !== 'object') continue
    const item = value as Partial<ClosedTab>
    if (!isValidTab(item.tab)) continue
    if (typeof item.closedAt !== 'number') continue
    out.push({ tab: item.tab, closedAt: item.closedAt })
  }
  return out
}

/* ---- store ---- */

let state: SidePaneSnapshot = {
  scopes: readScopes(),
  collapsed: readCollapsed(),
  width: readWidth(),
  recentClosed: readRecent(),
}

const listeners = new Set<() => void>()

function setState(patch: Partial<SidePaneSnapshot>) {
  state = { ...state, ...patch }
  for (const listener of listeners) listener()
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

function useSidePaneStore<T>(pick: (snapshot: SidePaneSnapshot) => T): T {
  return useSyncExternalStore(subscribe, () => pick(state))
}

/* ---- 分桶写入(带 LRU 上限) ---- */

function putScope(scopes: Record<string, SidePaneState>, key: string, next: SidePaneState) {
  // 重新插入以刷新"最近使用"顺序:JS 对象保留字符串键的插入顺序,
  // 所以删掉再设回去等价于把该键移到队尾。
  const copy: Record<string, SidePaneState> = { ...scopes }
  delete copy[key]
  if (next.tabs.length > 0) copy[key] = next
  const keys = Object.keys(copy)
  if (keys.length > SCOPE_LIMIT) {
    const overflow = keys.length - SCOPE_LIMIT
    for (let i = 0; i < overflow; i += 1) delete copy[keys[i]!]
  }
  return copy
}

/* ---- 订阅 hooks ---- */

/** 某个 scope 的面板状态(没有则空态)。 */
export function useSidePaneState(sessionId: string | null): SidePaneState {
  const key = scopeKey(sessionId)
  return useSidePaneStore((snapshot) => snapshot.scopes[key] ?? EMPTY_SIDE_PANE)
}

/** 某个 scope 是否折叠。 */
export function useSidePaneCollapsed(sessionId: string | null): boolean {
  const key = scopeKey(sessionId)
  return useSidePaneStore((snapshot) => snapshot.collapsed[key] ?? true)
}

export function useSidePaneWidth(): number {
  return useSidePaneStore((snapshot) => snapshot.width)
}

export function useRecentClosed(): ClosedTab[] {
  return useSidePaneStore((snapshot) => snapshot.recentClosed)
}

/** 非 hook 读取(事件回调里用)。 */
export function peekSidePane(sessionId: string | null): SidePaneState {
  return state.scopes[scopeKey(sessionId)] ?? EMPTY_SIDE_PANE
}

export function peekCollapsed(sessionId: string | null): boolean {
  return state.collapsed[scopeKey(sessionId)] ?? true
}

/* ---- 动作 ---- */

/** 写入某个 scope 的面板状态(传函数以基于当前值计算)。 */
export function updateSidePane(
  sessionId: string | null,
  updater: (current: SidePaneState) => SidePaneState,
): void {
  const key = scopeKey(sessionId)
  const current = state.scopes[key] ?? EMPTY_SIDE_PANE
  const next = updater(current)
  if (next === current) return
  const scopes = putScope(state.scopes, key, next)
  setState({ scopes })
  writeJson(SCOPES_KEY, scopes)
}

/** 设置某个 scope 的折叠态。 */
export function setSidePaneCollapsed(sessionId: string | null, collapsed: boolean): void {
  const key = scopeKey(sessionId)
  if ((state.collapsed[key] ?? true) === collapsed) return
  const collapsedMap = { ...state.collapsed, [key]: collapsed }
  setState({ collapsed: collapsedMap })
  writeJson(COLLAPSED_KEY, collapsedMap)
}

/** 记录展开宽度(px)。 */
export function setSidePaneWidth(width: number): void {
  const rounded = Math.round(width)
  if (!Number.isFinite(rounded) || rounded <= 0) return
  if (state.width === rounded) return
  setState({ width: rounded })
  try {
    window.localStorage.setItem(WIDTH_KEY, String(rounded))
  } catch {
    /* 同上:只影响刷新恢复。 */
  }
}

/** 记录一批关闭的标签到历史。 */
export function rememberClosed(tabs: SidePaneTab[], closedAt = Date.now()): void {
  if (tabs.length === 0) return
  const ids = new Set(tabs.map((tab) => tab.id))
  const fresh = tabs.map((tab) => ({ tab, closedAt }))
  const recentClosed = [
    ...fresh,
    ...state.recentClosed.filter((item) => !ids.has(item.tab.id)),
  ].slice(0, 8)
  setState({ recentClosed })
  writeJson(RECENT_KEY, recentClosed)
}

/** 从历史里移除一条(重开后调用)。 */
export function forgetClosed(id: string): void {
  if (!state.recentClosed.some((item) => item.tab.id === id)) return
  const recentClosed = state.recentClosed.filter((item) => item.tab.id !== id)
  setState({ recentClosed })
  writeJson(RECENT_KEY, recentClosed)
}

/**
 * 会话被删除时清掉它的桶与折叠态。
 *
 * 不做的话 localStorage 会留下永远不会再被读到的孤儿桶(占用上限名额,
 * 把真正在用的 scope 挤出去)。
 */
export function dropScope(sessionId: string): void {
  const key = scopeKey(sessionId)
  if (!(key in state.scopes) && !(key in state.collapsed)) return
  const scopes = { ...state.scopes }
  const collapsed = { ...state.collapsed }
  delete scopes[key]
  delete collapsed[key]
  setState({ scopes, collapsed })
  writeJson(SCOPES_KEY, scopes)
  writeJson(COLLAPSED_KEY, collapsed)
}
