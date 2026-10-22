/**
 * 会话显示名覆盖(用户手动重命名)。
 *
 * 只影响控制台里的显示名:侧栏、会话标题、搜索与引用都走
 * [`sessionDisplayTitle`],不写会话日志里的 AI 标题事件。放在模块级缓存是
 * 因为 `sessionDisplayTitle` 在渲染与搜索里被高频调用,不能每次都读 storage。
 */
const ALIAS_KEY = 'denia.session-aliases.v1'

let cache: Record<string, string> | null = null

function load(): Record<string, string> {
  if (cache) return cache
  try {
    const value: unknown = JSON.parse(window.localStorage.getItem(ALIAS_KEY) ?? 'null')
    if (!value || typeof value !== 'object' || Array.isArray(value)) {
      cache = {}
      return cache
    }
    const next: Record<string, string> = {}
    for (const [id, title] of Object.entries(value as Record<string, unknown>)) {
      if (typeof title === 'string' && title.trim()) next[id] = title.trim()
    }
    cache = next
  } catch {
    cache = {}
  }
  return cache
}

/** 该会话的自定义显示名;没有时返回 undefined。 */
export function sessionAlias(id: string): string | undefined {
  return load()[id]
}

/** 设置/清除自定义显示名(空串 = 恢复自动标题)。 */
export function setSessionAlias(id: string, title: string): void {
  const next = { ...load() }
  const trimmed = title.trim()
  if (trimmed) next[id] = trimmed
  else delete next[id]
  cache = next
  try {
    window.localStorage.setItem(ALIAS_KEY, JSON.stringify(next))
  } catch {
    // 存储不可用只影响刷新后的显示名,本次页面内仍然生效。
  }
}
