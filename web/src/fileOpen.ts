/**
 * 文件打开的共享入口。
 *
 * # 为什么是模块级注册 + hook 两层
 *
 * 四个触发点里有一个在**纯函数**里:`render.tsx` 的 Markdown 链接渲染是无
 * hook 的纯函数(它的产物被流式渲染缓存成 React 元素,不能用 context 或
 * `useRef`)。所以这里给出一条模块级的注册通道 —— App 把处理函数注册进来,
 * 渲染器直接调用 `openWorkspaceFile()`。
 *
 * 组件侧(工具行、产物行、文件树)走 `useFileOpen()` hook,读同一份注册值。
 * 两条路径指向同一个实现,不存在"两条逻辑漂移"的风险。
 *
 * # 默认行为
 *
 * 未注册(如 jsdom 挂载测试、或某处单独渲染 transcript)时返回 false,
 * 调用方据此**保持浏览器默认行为**而不是静默吞掉点击 —— 与"指向工作区外
 * 的链接保持默认"是同一条原则。
 */

import { useSyncExternalStore } from 'react'

/** 打开文件的处理函数:接手返回 true,无法接手返回 false。 */
export type OpenFileHandler = (path: string) => boolean

let handler: OpenFileHandler = () => false
const listeners = new Set<() => void>()

function setHandler(next: OpenFileHandler): void {
  handler = next
  for (const listener of listeners) listener()
}

/**
 * 注册打开文件的处理函数。
 *
 * 注销时只在"当前还是自己"的情况下回退:App 重挂载时(React 严格模式会
 * 双调用 effect),后注册的不能被先卸载的顺手清掉。
 *
 * @returns 注销函数。
 */
export function registerOpenFile(next: OpenFileHandler): () => void {
  setHandler(next)
  return () => {
    if (handler === next) setHandler(() => false)
  }
}

/**
 * 渲染器用的直接调用入口(纯函数场景)。
 *
 * 每次调用都读**当前**注册值,而不是绑定时捕获的那份 —— 切会话后 App 会
 * 重新注册,渲染器不需要跟着重建。
 */
export function openWorkspaceFile(path: string): boolean {
  return handler(path)
}

function subscribe(listener: () => void): () => void {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

/** 组件侧入口:订阅注册变化,拿到稳定的调用函数。 */
export function useFileOpen(): OpenFileHandler {
  return useSyncExternalStore(
    subscribe,
    () => handler,
    () => handler,
  )
}
