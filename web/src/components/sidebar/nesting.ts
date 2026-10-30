import { t } from '../../i18n'
import { relativeTime } from '../../relativeTime'
import type { DropPlacement } from '../../sidebarOrder'
import type { SessionSummary } from '../../types'

export const UNGROUPED_KEY = 'ungrouped'
export const COLLAPSED_LIMIT = 5
export const WORKSPACE_DRAG_MIME = 'application/x-denia-workspace'

/** 侧栏拖拽中的源:会话(限定在工作区内)或工作区。 */
export type SidebarDrag =
  | { kind: 'session'; id: string; groupKey: string }
  | { kind: 'workspace'; id: string }

/** 拖拽落点与插入方向;仅在当前拖拽类型匹配时渲染指示线。 */
export type SidebarDropTarget =
  | { kind: 'session'; id: string; groupKey: string; placement: DropPlacement }
  | { kind: 'workspace'; id: string; placement: DropPlacement }

/** 指针在行内上半/下半:上半插到目标前,下半插到目标后。 */
export function placementFromPointer(event: { currentTarget: HTMLElement; clientY: number }): DropPlacement {
  const rect = event.currentTarget.getBoundingClientRect()
  return event.clientY - rect.top < rect.height / 2 ? 'before' : 'after'
}

/**
 * 血缘嵌套排序:子会话紧跟其父会话
 * 之后(先序遍历);父不在本组(跨组/已删)的子会话留在顶层。
 * `hideSubtreeOf` 里的会话只渲染自身,其整棵后代子树不进列表(折叠)。
 */
export function nestByParent(
  members: SessionSummary[],
  hideSubtreeOf?: ReadonlySet<string>,
): { session: SessionSummary; depth: number }[] {
  const ids = new Set(members.map((s) => s.id))
  const byParent = new Map<string, SessionSummary[]>()
  const roots: SessionSummary[] = []
  for (const session of members) {
    const parent = session.parent_session
    if (parent && ids.has(parent)) {
      const list = byParent.get(parent)
      if (list) list.push(session)
      else byParent.set(parent, [session])
    } else {
      roots.push(session)
    }
  }
  const out: { session: SessionSummary; depth: number }[] = []
  const walk = (list: SessionSummary[], depth: number) => {
    for (const session of list) {
      out.push({ session, depth })
      if (hideSubtreeOf?.has(session.id)) continue
      walk(byParent.get(session.id) ?? [], depth + 1)
    }
  }
  walk(roots, 0)
  return out
}

const TIME_UNIT_KEYS = {
  minutes: 'timeMinutes',
  hours: 'timeHours',
  days: 'timeDays',
  months: 'timeMonths',
  years: 'timeYears',
} as const

/** 会话行尾相对时间:now 桶不带"前"。 */
export function relativeTimeLabel(at: number, now: number): string {
  const { unit, n } = relativeTime(at, now)
  if (unit === 'now') return t('timeNow')
  return t('timeAgo', { t: t(TIME_UNIT_KEYS[unit], { n }) })
}

/** 一组会话的血缘计量:每个会话的后代总数与"有运行中后代"的祖先集合。 */
export function lineageStats(
  members: SessionSummary[],
  runningIds: Record<string, boolean>,
): { descendantCount: Map<string, number>; runningAncestors: Set<string> } {
  const ids = new Set(members.map((s) => s.id))
  const parentOf = new Map<string, string>()
  for (const session of members) {
    if (session.parent_session && ids.has(session.parent_session)) {
      parentOf.set(session.id, session.parent_session)
    }
  }
  const descendantCount = new Map<string, number>()
  for (const session of members) {
    const seen = new Set<string>()
    let cursor = parentOf.get(session.id)
    while (cursor && !seen.has(cursor)) {
      seen.add(cursor)
      descendantCount.set(cursor, (descendantCount.get(cursor) ?? 0) + 1)
      cursor = parentOf.get(cursor)
    }
  }
  const runningAncestors = new Set<string>()
  for (const session of members) {
    if (!runningIds[session.id]) continue
    const seen = new Set<string>()
    let cursor = parentOf.get(session.id)
    while (cursor && !seen.has(cursor)) {
      seen.add(cursor)
      runningAncestors.add(cursor)
      cursor = parentOf.get(cursor)
    }
  }
  return { descendantCount, runningAncestors }
}
