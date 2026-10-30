import assert from 'node:assert/strict'
import { build } from 'esbuild'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const web = fileURLToPath(new URL('..', import.meta.url))
const dir = mkdtempSync(join(tmpdir(), 'denia-queue-scheduler-'))
const outfile = join(dir, 'scheduler.mjs')

const stubs = {
  appStore: `const b = globalThis.__queueBridge
    export function subscribeRunningStatus(cb) { b.runningListeners.add(cb); return () => b.runningListeners.delete(cb) }
    export function notify(kind, message) { b.notifications.push([kind, message]) }
  `,
  serverEvents: `const b = globalThis.__queueBridge
    export function subscribeServerEvents(cb) { b.serverListeners.add(cb); return () => b.serverListeners.delete(cb) }
  `,
  composerMemory: `const b = globalThis.__queueBridge
    export async function loadComposer(scope) { return structuredClone(b.memory.get(scope) ?? { text: '', images: [], attachments: [], quotes: [], queue: [] }) }
    export async function saveComposer(scope, snapshot) { b.memory.set(scope, structuredClone(snapshot)) }
  `,
  api: `const b = globalThis.__queueBridge
    export async function postPrompt(id, body) {
      b.posts.push({ id, body })
      if (b.postFailures > 0) { b.postFailures -= 1; throw new Error('MOCK_SEND_FAILURE') }
      return { ok: true }
    }
  `,
  modelSelectionStore: `const b = globalThis.__queueBridge
    export function peekModelSelection() { return b.selection }
  `,
  i18n: `export function t(key) { return key }`,
  types: `export function isPendingAttachment(value) { return !!value }`,
}

const result = await build({
  entryPoints: [join(web, 'src/features/conversation/queueScheduler.ts')],
  outfile,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  logLevel: 'silent',
  plugins: [{
    name: 'queue-stubs',
    setup(build) {
      build.onResolve({ filter: /\.\.\/\.\.\/(api|composerMemory|appStore|serverEvents|modelSelectionStore|i18n|types)$/ }, (args) => ({
        path: args.path.split('/').pop(),
        namespace: 'queue-stub',
      }))
      build.onLoad({ filter: /.*/, namespace: 'queue-stub' }, (args) => ({
        contents: stubs[args.path],
        loader: 'js',
      }))
    },
  }],
  metafile: true,
})
const inputs = Object.keys(result.metafile.inputs)
for (const forbidden of ['src/api.ts', 'src/appStore.ts', 'src/composerMemory.ts', 'src/serverEvents.ts']) {
  assert.ok(!inputs.some((path) => path.endsWith(forbidden)), `real ${forbidden} must not be bundled`)
}

const bridge = {
  runningListeners: new Set(),
  serverListeners: new Set(),
  notifications: [],
  posts: [],
  postFailures: 0,
  memory: new Map(),
  selection: { provider: 'mock-provider', model: 'mock-model', reasoningEffort: 'high' },
}
globalThis.__queueBridge = bridge
globalThis.window = {
  setTimeout,
  dispatchEvent() {},
}
Object.defineProperty(globalThis, 'navigator', { value: {}, configurable: true })

const mod = await import(pathToFileURL(outfile).href)
const scope = (id) => `session:${id}`
const queueOf = (id) => bridge.memory.get(scope(id))?.queue ?? []

function setQueue(id, queue) {
  bridge.memory.set(scope(id), { text: '', images: [], attachments: [], quotes: [], queue })
}
function transition(running) {
  for (const listener of bridge.runningListeners) listener(running)
}
async function waitFor(predicate, label) {
  const deadline = Date.now() + 3000
  while (Date.now() < deadline) {
    if (predicate()) return
    await new Promise((resolve) => setTimeout(resolve, 10))
  }
  assert.fail(label)
}

const stop = mod.startQueueScheduler()
assert.equal(bridge.runningListeners.size, 1, '调度器必须订阅全局 running 集合')
assert.equal(bridge.serverListeners.size, 1, '调度器必须订阅引导送达事件')

// A 在运行，B 是当前窗口：调度器只认会话，不看当前选中。
setQueue('A', [{ id: 'm1', text: '跨会话发送', mode: 'guide', state: 'queued' }])
transition({ A: true, B: true })
transition({ B: true })
await waitFor(() => bridge.posts.length === 1, 'A 本轮结束必须自动发送，即使当前在 B')
assert.equal(bridge.posts[0].id, 'A')
assert.equal(bridge.posts[0].body.prompt, '跨会话发送')
assert.equal(bridge.posts[0].body.provider, 'mock-provider')
assert.equal(bridge.posts[0].body.model, 'mock-model')
assert.equal(bridge.posts[0].body.source, 'queue')
assert.deepEqual(queueOf('A'), [], '发送成功后必须出队')

// 队列顺序：一次只发队首，后续等下一条 running 边沿。
setQueue('B', [
  { id: 'm2', text: 'first', mode: 'guide', state: 'queued' },
  { id: 'm3', text: 'second', mode: 'guide', state: 'queued' },
])
transition({ B: true })
transition({})
await waitFor(() => bridge.posts.length === 2, 'B 本轮结束必须发送队首')
assert.equal(bridge.posts[1].body.prompt, 'first')
assert.deepEqual(queueOf('B').map((item) => item.id), ['m3'], '只允许出队队首，顺序不得乱')

// 引导送达：即使轮次还没结束，也要把对应项移出持久化队列。
setQueue('A', [{ id: 'm4', text: 'steered', mode: 'guide', state: 'steering' }])
for (const listener of bridge.serverListeners) {
  listener('steering-delivered', { type: 'steering-delivered', id: 'A', message_id: 'm4' })
}
await waitFor(() => queueOf('A').length === 0, '引导送达必须出队')

// 发送失败：保留队列并提示，下一次轮次边沿再重试。
setQueue('A', [{ id: 'm5', text: 'retry me', mode: 'guide', state: 'queued' }])
bridge.postFailures = 1
transition({ A: true })
transition({})
await waitFor(() => bridge.notifications.length === 1, '发送失败必须提示')
assert.deepEqual(queueOf('A').map((item) => item.id), ['m5'], '失败后消息必须留在队列')
transition({ A: true })
transition({})
await waitFor(() => bridge.posts.length === 4, '下一次边沿必须重试并成功')
assert.deepEqual(queueOf('A'), [])

// 并发去重：同一会话同时两次 flush 只能发一条。
setQueue('A', [{ id: 'm6', text: 'once', mode: 'guide', state: 'queued' }])
await Promise.all([mod.flushQueueHead('A'), mod.flushQueueHead('A')])
assert.equal(bridge.posts.filter((post) => post.body.prompt === 'once').length, 1, '并发 flush 必须去重')

stop()
assert.equal(bridge.runningListeners.size, 0)
assert.equal(bridge.serverListeners.size, 0)
rmSync(dir, { recursive: true, force: true })
console.log('ok queue scheduler: per-session flush, ordering, steering delivery, failure retention and in-flight dedupe')
