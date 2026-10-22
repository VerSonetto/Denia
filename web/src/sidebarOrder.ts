/**
 * 侧栏手动排序的纯逻辑:不碰 DOM、不碰存储,便于在 node 检查脚本里直测。
 *
 * 排序是"叠加"而非"接管":手动顺序只约束记录过的 id,之后新出现的
 * 会话/工作区(服务端都是 prepend)保持原顺序排在最前,不会被旧顺序挤到
 * 列表底部。
 */

export type DropPlacement = 'before' | 'after'

/**
 * 把 `order` 里登记的 id 按记录顺序排在后面,未登记的项保持原顺序排在最前。
 * 顺序里的失效 id(已删除)与重复 id 直接忽略。
 */
export function applyManualOrder<T extends { id: string }>(
  items: T[],
  order: readonly string[] | undefined,
): T[] {
  if (!order || order.length === 0) return items
  const byId = new Map(items.map((item) => [item.id, item]))
  const seen = new Set<string>()
  const ordered: T[] = []
  for (const id of order) {
    const item = byId.get(id)
    if (!item || seen.has(id)) continue
    seen.add(id)
    ordered.push(item)
  }
  const fresh = items.filter((item) => !seen.has(item.id))
  return fresh.length === 0 ? ordered : [...fresh, ...ordered]
}

/**
 * 置顶项稳定上浮:置顶项保留它们在传入顺序里的相对位置,其余项保持原序
 * 跟在后面。没有命中项时返回原数组引用,避免无谓重渲染。
 */
export function applyPins<T extends { id: string }>(
  items: T[],
  pinned: readonly string[],
): T[] {
  if (pinned.length === 0) return items
  const set = new Set(pinned)
  const head = items.filter((item) => set.has(item.id))
  if (head.length === 0) return items
  return [...head, ...items.filter((item) => !set.has(item.id))]
}

/** 置顶开关:存在则移除,不存在则追加到末尾。 */
export function toggleId(ids: readonly string[], id: string): string[] {
  return ids.includes(id) ? ids.filter((item) => item !== id) : [...ids, id]
}

/**
 * 把 `from` 插到 `to` 的前/后。找不到任一 id、或结果与传入顺序一致时,
 * 原样返回同一个数组引用,调用方据此判断是否需要落盘。
 */
export function moveId(
  ids: string[],
  from: string,
  to: string,
  placement: DropPlacement,
): string[] {
  if (from === to) return ids
  if (!ids.includes(from) || !ids.includes(to)) return ids
  const next = ids.filter((id) => id !== from)
  const target = next.indexOf(to)
  next.splice(placement === 'after' ? target + 1 : target, 0, from)
  return next.every((id, index) => id === ids[index]) ? ids : next
}
