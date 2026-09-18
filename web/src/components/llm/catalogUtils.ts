/** 模型目录的纯函数工具:平铺、过滤、排序、错误文本脱敏。 */

import type { CatalogFilter, CatalogSortDir, CatalogSortKey, CatalogRow } from './types'
import type { ModelCatalog } from '../../types'

/** 把分组目录平铺为行(保持目录分组顺序,组内顺序原样)。 */
export function flattenCatalog(catalog: ModelCatalog): CatalogRow[] {
  const rows: CatalogRow[] = []
  for (const group of catalog.groups) {
    for (const model of group.models) {
      rows.push({ providerId: group.id, providerName: group.name, model })
    }
  }
  return rows
}

/** 搜索 + 过滤(纯函数,输入已防抖的 query)。 */
export function filterRows(rows: CatalogRow[], filter: CatalogFilter): CatalogRow[] {
  const q = filter.query.trim().toLowerCase()
  return rows.filter((row) => {
    if (filter.providerId !== 'all' && row.providerId !== filter.providerId) return false
    if (filter.visionOnly && !(row.model.inputModalities ?? []).includes('image')) return false
    if (filter.thinkingOnly && row.model.thinkingSupported !== true) return false
    if (!q) return true
    return (
      row.model.id.toLowerCase().includes(q) ||
      row.model.name.toLowerCase().includes(q) ||
      row.providerName.toLowerCase().includes(q) ||
      (row.model.description ?? '').toLowerCase().includes(q)
    )
  })
}

/** 多列排序:主键 + 方向;模型 id 恒为次级键(稳定展示)。 */
export function sortRows(
  rows: CatalogRow[],
  key: CatalogSortKey,
  dir: CatalogSortDir,
): CatalogRow[] {
  const sign = dir === 'asc' ? 1 : -1
  const sorted = [...rows]
  sorted.sort((a, b) => {
    let primary = 0
    if (key === 'provider') {
      primary = a.providerName.localeCompare(b.providerName, 'zh-Hans-CN')
    } else if (key === 'context') {
      const av = a.model.contextWindow ?? 0
      const bv = b.model.contextWindow ?? 0
      primary = av - bv
    } else {
      primary = a.model.id.localeCompare(b.model.id, 'zh-Hans-CN', { numeric: true })
    }
    if (primary !== 0) return primary * sign
    return a.model.id.localeCompare(b.model.id, 'zh-Hans-CN', { numeric: true })
  })
  return sorted
}

/** 展示用脱敏:抹掉形如 sk-xxxx / Bearer 后的长令牌片段。 */
export function sanitizeErrorText(text: string): string {
  return text
    .replace(/sk-[A-Za-z0-9_-]{8,}/g, 'sk-****')
    .replace(/(Bearer\s+)[A-Za-z0-9._-]{8,}/gi, '$1****')
    .replace(/(api[-_]?key["'\s:=]+)[^\s"',;)}]+/gi, '$1****')
}

/** 语义化时长:<1s 毫秒,否则秒(一位小数)。 */
export function formatMs(ms: number): string {
  if (ms < 1000) return `${Math.max(0, Math.round(ms))}ms`
  return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)}s`
}

/** 本地时刻 HH:MM:SS。 */
export function formatClock(at: number): string {
  const date = new Date(at)
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`
}
