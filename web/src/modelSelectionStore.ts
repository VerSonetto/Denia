/**
 * 当前模型选择的共享视图。
 *
 * # 为什么需要它
 *
 * 模型选择的事实源在 `SessionsPage`(输入框右侧的模型菜单),但右侧面板的
 * 「AI 生成提交信息」也要用同一个模型 —— 它是同一个用户、同一个对话上下文
 * 里的模型,不该让用户再选一次(需求明确要求"不新造模型配置入口")。
 *
 * 两者在组件树上是**兄弟**:`App` 渲染 `SessionsPage` 与 `SidePane`,而
 * `selection` 的 state 在 `SessionsPage` 内部。要么把它提升到 App(要把
 * 整条读写链路搬上去,改动面大且与本次需求无关),要么给它一个共享视图。
 * 这里选后者:一个模块级的极小 store,写入方一处、读取方一处。
 *
 * 与 `sidePaneStore` 同一套写法(`useSyncExternalStore` + 模块级值),
 * 不引状态库 —— 这是项目的硬约束。
 */

import { useSyncExternalStore } from 'react'
import type { ModelSelection } from './types'

let current: ModelSelection | null = null
const listeners = new Set<() => void>()

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

/**
 * 发布当前模型选择。
 *
 * 相等时不通知:调用方(SessionsPage)是在 effect 里同步的,而 effect 会在
 * 每次 selection 变化时跑;不加这道判等会让订阅者(审查面板)跟着无谓重渲染,
 * 而重渲染会打断正在进行的 diff 展开动画。
 */
export function setModelSelection(next: ModelSelection | null): void {
  const same =
    current?.provider === next?.provider &&
    current?.model === next?.model &&
    current?.reasoningEffort === next?.reasoningEffort
  if (same) return
  current = next
  for (const listener of listeners) listener()
}

/** 读取当前模型选择(没有则 null:AI 生成入口据此禁用并给出提示)。 */
export function useModelSelection(): ModelSelection | null {
  return useSyncExternalStore(subscribe, () => current)
}

/** 非 hook 读取(事件回调里用)。 */
export function peekModelSelection(): ModelSelection | null {
  return current
}
