import { useSyncExternalStore } from 'react'
import * as api from './api'
import type { SessionSummary, WorkspaceRecord } from './types'
import { dropSession } from './sessionStreams'

/**
 * 全局应用 store(自写,不引状态库):唯一数据源。
 *
 * - 所有会话/工作区/导航状态集中在这里,组件只通过 hook 订阅自己
 *   关心的字段,引用稳定 → 不相关组件零重渲染。
 * - 派生值(activeSession / activeWs / locked)由组件用 hook 组合,
 *   不在 store 里放重复副本。
 */

export interface Toast {
  kind: 'ok' | 'err'
  message: string
}

interface AppSnapshot {
  sessions: SessionSummary[]
  workspaces: WorkspaceRecord[]
  activeId: string | null
  pendingWsId: string | null
  toast: Toast | null
  connLost: boolean
  catalogTick: number
  startedIds: Record<string, true>
  sessionsLoaded: boolean
  /** 运行中的会话集合:服务端 SSE 推送驱动 + 本地乐观兜底。 */
  runningIds: Record<string, boolean>
}

let state: AppSnapshot = {
  sessions: [],
  workspaces: [],
  activeId: null,
  pendingWsId: null,
  toast: null,
  connLost: false,
  catalogTick: 0,
  startedIds: {},
  sessionsLoaded: false,
  runningIds: {},
}

const listeners = new Set<() => void>()

function setState(patch: Partial<AppSnapshot>) {
  state = { ...state, ...patch }
  for (const listener of listeners) listener()
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

function useApp<T>(pick: (s: AppSnapshot) => T): T {
  return useSyncExternalStore(subscribe, () => pick(state))
}

/* ---- 字段级 hooks(引用稳定,隔离重渲染) ---- */

export const useSessions = () => useApp((s) => s.sessions)
export const useWorkspaces = () => useApp((s) => s.workspaces)
export const useActiveId = () => useApp((s) => s.activeId)
export const usePendingWsId = () => useApp((s) => s.pendingWsId)
export const useToast = () => useApp((s) => s.toast)
export const useConnLost = () => useApp((s) => s.connLost)
export const useCatalogTick = () => useApp((s) => s.catalogTick)
export const useStartedIds = () => useApp((s) => s.startedIds)
export const useSessionsLoaded = () => useApp((s) => s.sessionsLoaded)

export function getActiveId() {
  return state.activeId
}

/** running 集合订阅(侧栏/状态栏)。 */
export function subscribeRunningStatus(listener: (ids: Record<string, boolean>) => void): () => void {
  return subscribe(() => {
    listener(state.runningIds)
  })
}

/** 服务端推送/本地乐观的 running 更新。 */
export function setRunningStatus(id: string, running: boolean) {
  const before = state.runningIds
  if (!!before[id] === running) return
  const next = { ...before }
  if (running) next[id] = true
  else delete next[id]
  setState({ runningIds: next })
}

export function useRunningIds(): Record<string, boolean> {
  return useApp((s) => s.runningIds)
}

export function useRunningFor(id: string | null): boolean {
  return useApp((s) => (id === null ? false : !!s.runningIds[id]))
}

/* ---- 动作 ---- */

let toastTimer: number | undefined

export function notify(kind: Toast['kind'], message: string) {
  setState({ toast: { kind, message } })
  window.clearTimeout(toastTimer)
  toastTimer = window.setTimeout(() => setState({ toast: null }), 3500)
}

export function setActiveId(id: string | null, pendingWsId?: string | null) {
  setState({ activeId: id, ...(pendingWsId !== undefined ? { pendingWsId } : {}) })
}

export function setPendingWsId(id: string | null) {
  setState({ pendingWsId: id })
}

export function setConnLost(lost: boolean) {
  if (state.connLost === lost) return
  setState({ connLost: lost })
}

export function bumpCatalogTick() {
  setState({ catalogTick: state.catalogTick + 1 })
}

export function markStarted(id: string) {
  const startedIds = state.startedIds
  if (startedIds[id]) return
  setState({ startedIds: { ...startedIds, [id]: true } })
}

/** 全量刷新会话列表 + 工作区列表(SSE 通知/删除/创建后调用)。 */
export async function refreshList(): Promise<void> {
  try {
    const [sess, ws] = await Promise.all([api.listSessions(), api.listWorkspaces()])
    const nextStarted = { ...state.startedIds }
    for (const session of sess.sessions) {
      if (session.excerpt) nextStarted[session.id] = true
    }
    setState({
      sessions: sess.sessions,
      workspaces: ws.workspaces,
      startedIds: nextStarted,
      sessionsLoaded: true,
    })
  } catch (error) {
    setState({ sessionsLoaded: true })
    notify('err', error instanceof Error ? error.message : String(error))
  }
}

/** 本地插入/更新一条会话摘要(创建成功时,避免整表重拉)。 */
export function addSessionLocal(summary: SessionSummary, workspaceId?: string) {
  const sessions = [
    summary,
    ...state.sessions.filter((s) => s.id !== summary.id),
  ]
  const workspaces = workspaceId
    ? state.workspaces.map((ws) =>
        ws.id === workspaceId
          ? {
              ...ws,
              sessionIds: [summary.id, ...ws.sessionIds.filter((id) => id !== summary.id)],
            }
          : ws,
      )
    : state.workspaces
  setState({ sessions, workspaces })
}

/** 会话删除:成功后本地移除 + 工作区账本去引用 + 停流。 */
export async function deleteSessionAction(id: string): Promise<void> {
  await api.deleteSession(id)
  dropSession(id)
  setState({
    sessions: state.sessions.filter((s) => s.id !== id),
    workspaces: state.workspaces.map((ws) => ({
      ...ws,
      sessionIds: ws.sessionIds.filter((sid) => sid !== id),
    })),
    ...(state.activeId === id ? { activeId: null, pendingWsId: null } : {}),
  })
}

/** 工作区删除:成功后级联刷新(后端已级联删会话)。 */
export async function deleteWorkspaceAction(id: string): Promise<void> {
  await api.deleteWorkspace(id)
  await refreshList()
  if (state.activeId && !state.sessions.some((s) => s.id === state.activeId)) {
    setState({ activeId: null, pendingWsId: null })
  }
}

/* ---- 派生查询(非 hook,供动作/组件一次性计算) ---- */

function isBlank(s: SessionSummary): boolean {
  return !s.excerpt && !state.startedIds[s.id]
}

/** 该工作区路径下可复用的空白会话。 */
export function findWorkspaceBlank(cwd: string): SessionSummary | undefined {
  return state.sessions.find(
    (s) => s.cwd === cwd && s.cwd_alive !== false && isBlank(s),
  )
}

/** 当前活跃会话摘要(activeId 在列表里时)。 */
export function getActiveSession(): SessionSummary | null {
  if (!state.activeId) return null
  return state.sessions.find((s) => s.id === state.activeId) ?? null
}

/** 当前生效的工作区:活跃会话的 cwd 匹配项,否则 pendingWsId。 */
export function getActiveWorkspace(): WorkspaceRecord | null {
  const active = getActiveSession()
  if (active) {
    return state.workspaces.find((w) => w.path === active.cwd) ?? null
  }
  return state.workspaces.find((w) => w.id === state.pendingWsId) ?? null
}

/**
 * 发送前确保会话存在:复用 activeId → 复用该工作区空白会话 → 创建。
 * 返回会话 id;无工作区时返回 null。
 */
export async function ensureSession(ws: WorkspaceRecord | null): Promise<string | null> {
  if (state.activeId) return state.activeId
  if (!ws) return null
  const blank = findWorkspaceBlank(ws.path)
  if (blank) {
    setActiveId(blank.id, ws.id)
    return blank.id
  }
  const { session } = await api.createSession({ workspaceId: ws.id })
  const summary: SessionSummary = {
    id: session.id,
    created_at: session.created_at,
    excerpt: null,
    cwd: session.cwd,
    sandbox: session.sandbox,
    cwd_alive: true,
  }
  addSessionLocal(summary, ws.id)
  setActiveId(summary.id, ws.id)
  return summary.id
}

/** 同步读取当前工作区(非 hook,供事件回调)。 */
export function peekActiveWorkspace(): WorkspaceRecord | null {
  return getActiveWorkspace()
}
