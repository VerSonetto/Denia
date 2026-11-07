/**
 * 队列消息默认处理方式（console 设置 `queueDefaultAction` 的前端镜像）。
 *
 * 用户按下回车时同步读取，避免为了一个偏好等待一次网络请求；App 启动、
 * `settings-updated` 与设置页保存时刷新这里的缓存。默认 `ask` 保持原有
 * 语义：消息先进入队列，由用户在条目上选择插话或引导。
 */
export type QueueDefaultAction = 'ask' | 'interject' | 'guide'

let current: QueueDefaultAction = 'ask'

/** 宽松解析设置值；未知/缺省值一律回落到「每次询问」。 */
export function normalizeQueueDefaultAction(raw: unknown): QueueDefaultAction {
  return raw === 'interject' || raw === 'guide' ? raw : 'ask'
}

/** 写入缓存（App 拉取设置、设置页保存成功后调用）。 */
export function setQueueDefaultAction(action: QueueDefaultAction): void {
  current = action
}

/** 发送路径同步读取当前值。 */
export function peekQueueDefaultAction(): QueueDefaultAction {
  return current
}
