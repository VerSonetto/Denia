import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('..', import.meta.url))
const read = (path) => readFileSync(new URL(path, import.meta.url), 'utf8')
const web = (path) => readFileSync(`${root}src/${path}`, 'utf8')

/* ---- 任务二：插话 / 引导 ---- */

const page = web('pages/SessionsPage.tsx')
assert.ok(page.includes('guideQueued'), '队列条目提供「引导」发送')
assert.ok(page.includes('interjectQueued'), '队列条目提供「插话」发送')
assert.ok(page.includes('api.steerSession'), '引导走运行中注入接口')
assert.ok(page.includes('onGuide={(message) => void guideQueued(message)}'), '队列面板接引导回调')
assert.ok(page.includes('onInterject={(message) => void interjectQueued(message)}'), '队列面板接插话回调')
assert.ok(!page.includes('sendChoiceOpen'), '输入区不再弹出插话/引导选择窗')
assert.ok(page.includes("enqueueMessage(trimmed)"), '运行中发送仍先入队')

const panel = web('components/QueuedMessagePanel.tsx')
assert.ok(panel.includes('queue-send-picker'), '立即发送改为条目上的左右选择器')
assert.ok(panel.includes('queue-interject') && panel.includes('queue-guide'), '选择器并排提供插话与引导')
assert.ok(panel.includes('queueGuideDesc'), '选择器解释引导的时机差异')
assert.ok(!panel.includes('PanePopover'), '发送方式不得使用悬浮窗口')

const hook = web('features/conversation/useMessageQueue.ts')
assert.ok(hook.includes('registerQueueHandler'), '页面向调度器注册发送器')
assert.ok(hook.includes('source: \'interject\'') || hook.includes("source: 'interject'"), '插话在轨迹中带来源')
assert.ok(hook.includes('flushQueueHead(activeId)'), '当前页也触发统一调度，作为跨会话调度的兜底')

const scheduler = web('features/conversation/queueScheduler.ts')
assert.ok(scheduler.includes('subscribeRunningStatus'), '调度器订阅全局 running 集合')
assert.ok(scheduler.includes('for (const id of Object.keys(previous))'), '每个会话独立判断 running → idle')
assert.ok(scheduler.includes('steering-delivered'), '引导送达后把对应队列项移出')
assert.ok(scheduler.includes('loadComposer'), '切走会话后从持久化快照继续发送')
assert.ok(scheduler.includes('postPrompt'), '非当前页面会话直接走 prompt API')

const app = web('App.tsx')
assert.ok(app.includes('startQueueScheduler()'), 'App 层常驻启动队列调度器')

const api = web('api.ts')
assert.ok(api.includes('/steer'), '前端暴露运行中引导接口')
assert.ok(api.includes("source?: 'user' | 'queue' | 'interject' | 'guide'"), '发送轨迹带来源')

/* ---- 任务三：服务端步骤边界注入 + 跨会话调度 ---- */

const serverSteer = read('../../crates/server/src/agent_runtime/steering.rs')
assert.ok(serverSteer.includes('drain_steering_inner'), '服务端提供步骤边界取走引导')
assert.ok(serverSteer.includes('SteeringDelivered'), '投递后广播送达事件供前端出队')
assert.ok(serverSteer.includes('session/not-running'), '空闲会话拒绝引导，前端回落普通队列')

const loop = read('../../crates/agent-loop/src/turn.rs')
assert.ok(loop.includes('drain_steering'), '驱动器在每个 step 边界取走引导')
assert.ok(loop.includes('steering:{}'), '引导落成带稳定 id 的真实用户消息')

const route = read('../../crates/server/src/api/sessions.rs')
assert.ok(route.includes('/steer'), '会话路由暴露引导接口')

console.log('ok queue guide/interject split, per-session scheduler and steer delivery')
