/**
 * 供应商(路由)改动的纯函数:把一条改动套进 `llm-openai.providers` 全表。
 *
 * 改名必须是一次原子迁移:旧键删除、新键写入在同一个 PUT 里完成,否则中途
 * 崩溃会留下两份网关,或者新旧都不存在。这里只算内存中的下一张表;并发、
 * 冲突重试与失败回滚由 `useProviderAutosave` 负责。
 */

export interface RouteChange {
  /** 改动后的路由 id(providers 的键)。 */
  route: string
  /** 改名时的旧路由 id;普通写入/删除省略。 */
  previousRoute?: string
  /** null = 删除该路由。 */
  profile: Record<string, unknown> | null
}

/** 返回新表,不修改入参。 */
export function applyRouteChange(
  base: Record<string, unknown>,
  change: RouteChange,
): Record<string, unknown> {
  const next: Record<string, unknown> = { ...base }
  const previous = change.previousRoute
  if (previous && previous !== change.route) delete next[previous]
  if (change.profile === null) delete next[change.route]
  else next[change.route] = change.profile
  return next
}

/** 这条改动是否是一次真正的改名。 */
export function isRename(change: RouteChange): boolean {
  return change.previousRoute !== undefined && change.previousRoute !== change.route
}
