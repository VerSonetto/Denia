/**
 * 「打开审查面板」的共享入口。
 *
 * # 为什么是模块级注册而不是 prop/context
 *
 * 收尾轮次的变更卡片渲染在 transcript 深处(turn-end 行),而审查面板的
 * 控制器住在 App 顶层(SessionsPage → SessionView → Transcript),把它透传
 * 下去要动四层组件的 props,而这条交互只有一个调用点。`fileOpen.ts`
 * (打开文件读取面板)已经为同类问题定过规矩:模块级注册 + hook 两层,
 * 注册方在 App 挂载时写入,组件侧用 hook 读同一份值。
 *
 * # 默认行为
 *
 * 未注册(如 jsdom 挂载测试)时返回 false,调用方据此**不做任何事**,而不是
 * 抛错或假装打开 —— 与 `openWorkspaceFile` 的同一原则。
 */

import { useSyncExternalStore } from 'react'

/** 打开审查面板的处理函数:接手返回 true,无法接手返回 false。 */
export type OpenReviewHandler = () => boolean

let handler: OpenReviewHandler = () => false
const listeners = new Set<() => void>()

function setHandler(next: OpenReviewHandler): void {
  handler = next
  for (const listener of listeners) listener()
}

/**
 * 注册打开审查面板的处理函数。
 *
 * 注销时只在"当前还是自己"的情况下回退(React 严格模式会双调用 effect,
 * 后注册的不能被先卸载的顺手清掉)。
 *
 * @returns 注销函数。
 */
export function registerOpenReview(next: OpenReviewHandler): () => void {
  setHandler(next)
  return () => {
    if (handler === next) setHandler(() => false)
  }
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => void listeners.delete(listener)
}

/** 组件侧入口:订阅注册变化,拿到稳定的调用函数。 */
export function useOpenReview(): OpenReviewHandler {
  return useSyncExternalStore(
    subscribe,
    () => handler,
    () => handler,
  )
}
