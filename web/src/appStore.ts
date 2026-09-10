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
  /**
   * 本页面自己创建的空白会话 id。
   *
   * 空白会话的自动清理作用域**必须**限于此集合:会话列表是全局共享的,
   * 而 `activeId` 只代表本页面的焦点。若按"列表里非活跃即空白"去清扫,
   * 就会把别的标签页 / 别的实例(正式与开发实例共用同一数据目录)刚创建、
   * 正在使用的会话当垃圾删掉 —— 表现为"新建会话没选中工作区 + 发不出消息
   * + 列表里看不到新会话",以及发消息时报 `session not found`。
   */
  localBlankIds: Record<string, true>
  sessionsLoaded: boolean
  /** 运行中的会话集合:服务端 SSE 推送驱动 + 本地乐观兜底。 */
  runningIds: Record<string, boolean>
  /** 正在压缩的会话:会话 id → 发起时刻(epoch ms),刷新可恢复。 */
  compactingIds: Record<string, number>
}

/* ---- 压缩中集合(跨刷新) ----

   压缩是后台任务(202 接单),前端不再 await 结果,所以"正在压缩"这个
   状态必须有自己的持久来源,否则一刷新就没了。持久化到 sessionStorage:
   标签页刷新保留、关闭即清空,不会跨会话长期残留。

   存的是发起时刻(epoch ms),既用于显示已等待秒数,也用于兜底过期:
   万一"结束"事件丢了(断网/实例重启),状态不会永久卡住。 */

const COMPACTING_KEY = 'denia.compacting'
/** 兜底超时:远超任何一次压缩耗时,过期即当作已结束。 */
const COMPACTING_TTL_MS = 10 * 60 * 1000

/**
 * 读取持久化的压缩标记。
 *
 * **常量必须定义在 `state` 初始化之前**:早先它们排在 `state` 之后,
 * 这里的 `COMPACTING_KEY` 落在 TDZ 里,抛出的 ReferenceError 被下面的
 * catch 静默吞成 `{}` —— 表现为"标记写进去了,刷新后却读不回来"
 * (状态没丢、组件丢了)。storage 相关的 catch 只能兜"不可用/损坏",
 * 不能掩盖编程错误,故顺序即正确性。
 */
function readCompacting(): Record<string, number> {
  try {
    const raw = window.sessionStorage.getItem(COMPACTING_KEY)
    if (!raw) return {}
    const parsed: unknown = JSON.parse(raw)
    if (!parsed || typeof parsed !== 'object') return {}
    const now = Date.now()
    const next: Record<string, number> = {}
    for (const [id, at] of Object.entries(parsed as Record<string, unknown>)) {
      if (typeof at !== 'number') continue
      // 丢弃过期项:没有对应结束事件时,状态不能永久挂着。
      if (now - at > COMPACTING_TTL_MS) continue
      next[id] = at
    }
    return next
  } catch {
    /* storage 不可用(隐私模式):退化成纯内存,不影响主流程。 */
    return {}
  }
}

function writeCompacting(map: Record<string, number>) {
  try {
    if (Object.keys(map).length === 0) window.sessionStorage.removeItem(COMPACTING_KEY)
    else window.sessionStorage.setItem(COMPACTING_KEY, JSON.stringify(map))
  } catch {
    /* 写失败只影响刷新恢复,不影响当前页面 —— 内存态仍然正确。 */
  }
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
  localBlankIds: {},
  sessionsLoaded: false,
  runningIds: {},
  compactingIds: readCompacting(),
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

/** SSE 连接快照:整表替换,刷新/重连后还原中断按钮。 */
export function replaceRunningIds(ids: string[]) {
  const next: Record<string, boolean> = {}
  for (const id of ids) next[id] = true
  const before = state.runningIds
  const beforeKeys = Object.keys(before)
  if (beforeKeys.length === ids.length && ids.every((id) => before[id])) return
  setState({ runningIds: next })
}

/* ---- 压缩中集合(跨刷新):常量与读写函数见文件顶部(`state` 之前) ----

   为什么放 sessionStorage 而不是内存:刷新是本次改要修的核心场景 —— 旧
   实现把状态放在组件 useState 里,刷新即丢。sessionStorage 在**标签页刷
   新时保留、关闭即清空**,正好匹配"刷新不丢、重开不该有残留";localStorage
   则会跨会话长期残留,关掉浏览器再打开还会显示"正在压缩"。

   存的是发起时刻(epoch ms),既用于显示已等待秒数,也用于兜底过期:
   万一"结束"事件丢了(断网/实例重启),状态不会永久卡住。 */

/** 标记某会话正在压缩(传发起时刻)。 */
export function markCompacting(id: string, startedAt: number) {
  const next = { ...state.compactingIds, [id]: startedAt }
  writeCompacting(next)
  setState({ compactingIds: next })
}

/** 清除某会话的压缩中标记(成功/无可压缩/失败都走这里)。 */
export function clearCompacting(id: string) {
  if (!(id in state.compactingIds)) return
  const next = { ...state.compactingIds }
  delete next[id]
  writeCompacting(next)
  setState({ compactingIds: next })
}

/** 压缩中集合订阅。 */
export function useCompactingIds(): Record<string, number> {
  return useApp((s) => s.compactingIds)
}

/** 某会话是否正在压缩(返回发起时刻,未压缩为 null)。 */
export function useCompactingFor(id: string | null): number | null {
  return useApp((s) => (id === null ? null : (s.compactingIds[id] ?? null)))
}

/** SSE 连接快照:running 已结束的会话,其压缩标记也一并清掉
 *  (刷新后服务端权威的 running 是 false,说明任务早已收尾)。 */
export function pruneCompactingByRunning(runningIds: Record<string, boolean>) {
  const ids = Object.keys(state.compactingIds)
  if (ids.length === 0) return
  const stale = ids.filter((id) => !runningIds[id])
  if (stale.length === 0) return
  const next = { ...state.compactingIds }
  for (const id of stale) delete next[id]
  writeCompacting(next)
  setState({ compactingIds: next })
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

/**
 * 登记"本页面创建的空白会话":只有落在这个集合里的会话,才允许被本页面
 * 在切走时自动清理。跨页面/跨实例的清理一律不做(见 `localBlankIds` 注释)。
 */
export function markLocalBlank(id: string) {
  if (state.localBlankIds[id]) return
  setState({ localBlankIds: { ...state.localBlankIds, [id]: true } })
}

export function useLocalBlankIds(): Record<string, true> {
  return useApp((s) => s.localBlankIds)
}

/** 全量刷新会话列表 + 工作区列表(SSE 通知/删除/创建后调用)。 */
export async function refreshList(): Promise<void> {
  try {
    const [sess, ws] = await Promise.all([api.listSessions(), api.listWorkspaces()])
    const nextStarted = { ...state.startedIds }
    for (const session of sess.sessions) {
      if (session.excerpt) nextStarted[session.id] = true
    }
    // activeId 指向的会话已不存在(被别的页面/实例删除,或本页面切走后
    // 被清理):必须丢掉悬空焦点,否则输入框仍停在它上面,发消息会报
    // `session not found`。只清焦点、保留 pendingWsId —— 工作区落点还在,
    // 页面回到该工作区的可输入草稿态,而不是掉进 inert 死态。
    const activeStillExists =
      state.activeId === null || sess.sessions.some((s) => s.id === state.activeId)
    setState({
      sessions: sess.sessions,
      workspaces: ws.workspaces,
      startedIds: nextStarted,
      sessionsLoaded: true,
      ...(activeStillExists ? {} : { activeId: null }),
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
  const localBlankIds = { ...state.localBlankIds }
  delete localBlankIds[id]
  setState({
    sessions: state.sessions.filter((s) => s.id !== id),
    workspaces: state.workspaces.map((ws) => ({
      ...ws,
      sessionIds: ws.sessionIds.filter((sid) => sid !== id),
    })),
    localBlankIds,
    // 只丢焦点,保留 pendingWsId:删掉当前会话后仍停在该工作区的草稿态,
    // 输入框可以继续用,不会掉进 inert 死态。
    ...(state.activeId === id ? { activeId: null } : {}),
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
  // activeId 可能悬空(会话被别的页面/实例删掉,而本页面焦点还没跟上)。
  // 直接拿它去 postPrompt 会报 `session not found`,所以先校验它确实还在
  // 列表里;列表尚未加载完时不做校验,避免误判。
  if (state.activeId !== null) {
    const known = !state.sessionsLoaded || state.sessions.some((s) => s.id === state.activeId)
    if (known) return state.activeId
    setState({ activeId: null })
  }
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
  // 本页面为发送首条消息而创建的会话:同样登记,发送失败留下空白时可被清理。
  markLocalBlank(summary.id)
  setActiveId(summary.id, ws.id)
  return summary.id
}

/** 同步读取当前工作区(非 hook,供事件回调)。 */
export function peekActiveWorkspace(): WorkspaceRecord | null {
  return getActiveWorkspace()
}
