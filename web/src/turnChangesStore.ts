/**
 * 「本轮改动」的内存态:审查面板的一个**不依赖 git** 的来源。
 *
 * # 为什么单独一个 store,而不是挂在 review tab 上
 *
 * 侧面板 tab 会整体持久化到 localStorage(见 sidePaneStore:标签集合跨刷新
 * 保留)。本轮改动的 diff 帧动辄几十 KB,塞进 tab 等于把会话日志的一大截
 * 复制进用户偏好存储 —— 存不必要、还会在下次启动时加载一堆过期内容。
 * 所以它只在内存里,刷新即失(卡片本身会重新从 transcript 算出,下次点
 * 又会写进来)。
 *
 * # 与 git 来源的关系
 *
 * 两者是**并存的来源**,不是替代关系:
 * - 「未暂存/已暂存/分支」来自 git,覆盖工作区的全量改动;
 * - 「本轮」来自对话,只覆盖最近一次点开的那个 turn,且**不要求工作区是
 *   git 仓库** —— 没有 git 时它是唯一能看到 diff 的地方。
 *
 * 数据来自 fold 层的 TurnProducedFile(工具参数算出的行级 diff),与 git
 * 无关,所以非 git 目录同样有内容。
 */

import { useSyncExternalStore } from 'react'
import type { TurnProducedFile } from './fold'

/** 一次「查看本轮」的快照。 */
export interface TurnChangesSnapshot {
  /** 轮次号(仅用于面板标题里说"第几轮",不参与定位)。 */
  turn: number
  files: readonly TurnProducedFile[]
  /** 记录时刻:刷新按钮按"陈旧"提示用。 */
  at: number
}

/** scope 键 → 最近一次查看的本轮改动。 */
let byScope: Record<string, TurnChangesSnapshot | null> = {}
const listeners = new Set<() => void>()

function emit() {
  for (const listener of listeners) listener()
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => void listeners.delete(listener)
}

/** 写入某个 scope 的本轮改动(点卡片时调用)。 */
export function setTurnChanges(scope: string, snapshot: TurnChangesSnapshot | null): void {
  if (snapshot === null) {
    if (!(scope in byScope)) return
    const next = { ...byScope }
    delete next[scope]
    byScope = next
    emit()
    return
  }
  byScope = { ...byScope, [scope]: snapshot }
  emit()
}

/** 某个 scope 的本轮改动(没有则 null)。 */
export function peekTurnChanges(scope: string): TurnChangesSnapshot | null {
  return byScope[scope] ?? null
}

/** hook 形式:面板组件订阅用。 */
export function useTurnChanges(scope: string): TurnChangesSnapshot | null {
  return useSyncExternalStore(
    subscribe,
    () => byScope[scope] ?? null,
    () => null,
  )
}
