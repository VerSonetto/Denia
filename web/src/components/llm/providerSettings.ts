/** settings 命名空间读取的小工具(llm 面板各子页共用)。 */

import type { NamespaceView, SettingsDescribe } from '../../types'

export const OPENAI_NS = 'llm-openai'

export function namespaceOf(
  settings: SettingsDescribe,
  ns: string = OPENAI_NS,
): NamespaceView | undefined {
  return settings.namespaces.find((view) => view.ns === ns)
}

/** 宽容解析:非 object(或数组)一律按空记录处理。 */
export function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {}
}
