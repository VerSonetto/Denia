const CHILDREN_KEY = 'denia.sidebar-collapsed-children.v1'
const ORDER_KEY = 'denia.sidebar-order.v1'
const FILTER_KEY = 'denia.sidebar-filter.v1'
const PINS_KEY = 'denia.sidebar-pins.v1'
const IN_PROGRESS_KEY = 'denia.sidebar-in-progress.v1'

/** 侧栏手动排序账本:工作区顺序、未分组会话顺序、每个工作区的会话顺序。 */
export interface SidebarOrder {
  workspaces: string[]
  ungrouped: string[]
  sessions: Record<string, string[]>
}

export type SidebarFilter = 'all' | 'running'

/** 置顶账本:工作区与会话各一份 id 列表,顺序不影响显示(显示按当前手动顺序)。 */
export interface SidebarPins {
  workspaces: string[]
  sessions: string[]
}

export function readCollapsedChildren(): Record<string, boolean> {
  try {
    const value: unknown = JSON.parse(window.localStorage.getItem(CHILDREN_KEY) ?? '{}')
    if (!value || typeof value !== 'object' || Array.isArray(value)) return {}
    return Object.fromEntries(Object.entries(value).filter(([, collapsed]) => collapsed === true))
  } catch {
    return {}
  }
}

export function writeCollapsedChildren(value: Record<string, boolean>): void {
  try {
    window.localStorage.setItem(CHILDREN_KEY, JSON.stringify(
      Object.fromEntries(Object.entries(value).filter(([, collapsed]) => collapsed === true)),
    ))
  } catch {
    // 禁用存储或配额不足不影响本次页面内的折叠操作。
  }
}

function readIdList(value: unknown): string[] {
  if (!Array.isArray(value)) return []
  return value.filter((id): id is string => typeof id === 'string' && id.length > 0)
}

/** 读取手动排序账本;损坏/类型不符的字段按空处理,不影响默认服务端顺序。 */
export function readSidebarOrder(): SidebarOrder {
  try {
    const value: unknown = JSON.parse(window.localStorage.getItem(ORDER_KEY) ?? 'null')
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
      return { workspaces: [], ungrouped: [], sessions: {} }
    }
    const doc = value as Record<string, unknown>
    const sessions: Record<string, string[]> = {}
    if (doc.sessions && typeof doc.sessions === 'object' && !Array.isArray(doc.sessions)) {
      for (const [key, list] of Object.entries(doc.sessions as Record<string, unknown>)) {
        const ids = readIdList(list)
        if (ids.length > 0) sessions[key] = ids
      }
    }
    return {
      workspaces: readIdList(doc.workspaces),
      ungrouped: readIdList(doc.ungrouped),
      sessions,
    }
  } catch {
    return { workspaces: [], ungrouped: [], sessions: {} }
  }
}

export function writeSidebarOrder(value: SidebarOrder): void {
  try {
    window.localStorage.setItem(ORDER_KEY, JSON.stringify({
      workspaces: value.workspaces,
      ungrouped: value.ungrouped,
      sessions: Object.fromEntries(
        Object.entries(value.sessions).filter(([, ids]) => ids.length > 0),
      ),
    }))
  } catch {
    // 写入失败只影响刷新后的顺序,本次页面内的拖动结果仍然生效。
  }
}

/** 筛选仅两种状态:全部任务 / 进行中任务;其余值一律回落到全部。 */
export function readSidebarFilter(): SidebarFilter {
  try {
    return window.localStorage.getItem(FILTER_KEY) === 'running' ? 'running' : 'all'
  } catch {
    return 'all'
  }
}

export function writeSidebarFilter(value: SidebarFilter): void {
  try {
    if (value === 'all') window.localStorage.removeItem(FILTER_KEY)
    else window.localStorage.setItem(FILTER_KEY, value)
  } catch {
    // 存储不可用只影响刷新后的筛选状态。
  }
}

/** 读取置顶账本;损坏/类型不符的字段按空处理。 */
export function readSidebarPins(): SidebarPins {
  try {
    const value: unknown = JSON.parse(window.localStorage.getItem(PINS_KEY) ?? 'null')
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
      return { workspaces: [], sessions: [] }
    }
    const doc = value as Record<string, unknown>
    return {
      workspaces: readIdList(doc.workspaces),
      sessions: readIdList(doc.sessions),
    }
  } catch {
    return { workspaces: [], sessions: [] }
  }
}

export function writeSidebarPins(value: SidebarPins): void {
  try {
    window.localStorage.setItem(PINS_KEY, JSON.stringify({
      workspaces: value.workspaces,
      sessions: value.sessions,
    }))
  } catch {
    // 写入失败只影响刷新后的置顶状态。
  }
}

/**
 * 手动标记为「进行中」的会话账本。
 *
 * 与 `runningIds`(服务端权威的正在跑)是**两个事实**:
 * - running 是模型此刻是否在产出,轮次结束就消失;
 * - 标记是用户显式的工作清单,只有「移除标记」或会话被删除才会消失。
 *
 * 「进行中任务」筛选展示两者的并集:标记项跨轮次、跨重启保留,running 项
 * 仍按原行为即时进出。账本只存 id,列表渲染时按会话表过滤 —— 会话删掉后
 * 下次读取自然不再出现,不留下永远点不到的孤儿。
 */
export function readInProgressMarks(): string[] {
  try {
    const value: unknown = JSON.parse(window.localStorage.getItem(IN_PROGRESS_KEY) ?? '[]')
    // 写入已去重;读取也兜住手工编辑/旧版本留下的重复 id。
    return [...new Set(readIdList(value))]
  } catch {
    return []
  }
}

export function writeInProgressMarks(ids: readonly string[]): void {
  try {
    const unique = [...new Set(ids.filter((id) => id.length > 0))]
    if (unique.length === 0) window.localStorage.removeItem(IN_PROGRESS_KEY)
    else window.localStorage.setItem(IN_PROGRESS_KEY, JSON.stringify(unique))
  } catch {
    // 写入失败只影响刷新后的标记,本次页面内的勾选仍然生效。
  }
}

export function restoredSidebarSession(activeId: string | null): string | null {
  if (activeId) return activeId
  try {
    const match = window.location.hash.match(/^#s=([^#]+)$/)
    return match ? decodeURIComponent(match[1]) : null
  } catch {
    return null
  }
}
