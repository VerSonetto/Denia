/**
 * 浏览器可视化侧栏的运行时回归：AI 请求 visualMode 时必须通知宿主打开面板，
 * 且用户手动收起后本轮不再自动展开。
 *
 *   node scripts/check-browser-sidebar.mjs
 */
import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const here = fileURLToPath(new URL('.', import.meta.url))
const srcRoot = join(here, '..', 'src')
const outDir = join(here, '..', 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const entry = join(outDir, 'browser-sidebar-entry.tsx')

const bridge = {
  listeners: new Set(),
  runListeners: new Set(),
  runningIds: {},
  requested: [],
  state: { running: false, tabs: [] },
}
globalThis.__browserSidebarBridge = bridge

writeFileSync(
  entry,
  `
import { useState } from 'react'
import { useBrowserSidebar } from ${JSON.stringify(join(srcRoot, 'hooks', 'useBrowserSidebar.ts').replace(/\\/g, '/'))}
function Harness() {
  const [activeId, setActiveId] = useState('session-a')
  const { open, userClose } = useBrowserSidebar(activeId, (id) => globalThis.__browserSidebarBridge.requested.push(id))
  return <div>
    <span id="open">{open ? 'open' : 'closed'}</span>
    <span id="active">{activeId}</span>
    <button id="switch" onClick={() => setActiveId('session-b')}>switch</button>
    <button id="close" onClick={userClose}>close</button>
  </div>
}
export { Harness }
`,
)

const stubPlugin = {
  name: 'browser-sidebar-stubs',
  setup(build) {
    build.onResolve({ filter: /\.\.\/(browserApi|appStore)$/ }, (args) => ({
      path: args.path.split('/').pop(),
      namespace: 'browser-sidebar-stub',
    }))
    build.onLoad({ filter: /.*/, namespace: 'browser-sidebar-stub' }, (args) => ({
      contents: args.path === 'browserApi'
        ? `const b = globalThis.__browserSidebarBridge
           export async function fetchBrowserState() { return b.state }
           export function subscribeBrowserEvents(cb) { b.listeners.add(cb); return () => b.listeners.delete(cb) }
           export async function browserCommand() { return { ok: true } }
          `
        : `import { useSyncExternalStore } from 'react'
           export function useSessions() { return [{ id: 'session-a' }, { id: 'session-b' }] }
           export function useSessionsLoaded() { return true }
           export function useRunningIds() {
             return useSyncExternalStore(
               (cb) => { globalThis.__browserSidebarBridge.runListeners.add(cb); return () => globalThis.__browserSidebarBridge.runListeners.delete(cb) },
               () => globalThis.__browserSidebarBridge.runningIds,
             )
           }
          `,
      loader: 'js',
    }))
  },
}

const bundle = join(outDir, 'browser-sidebar-bundle.mjs')
await build({
  entryPoints: [entry],
  outfile: bundle,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  jsx: 'automatic',
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  plugins: [stubPlugin],
  logLevel: 'silent',
})

const dom = new JSDOM('<!doctype html><html><body><div id="root"></div></body></html>', {
  url: 'http://localhost/',
  pretendToBeVisual: true,
})
const g = dom.window
for (const key of [
  'window', 'document', 'navigator', 'location', 'history', 'HTMLElement', 'Element', 'Node',
  'Event', 'CustomEvent', 'MouseEvent', 'KeyboardEvent', 'MutationObserver', 'getComputedStyle',
  'requestAnimationFrame', 'cancelAnimationFrame', 'sessionStorage', 'localStorage',
]) {
  Object.defineProperty(globalThis, key, { value: g[key], configurable: true, writable: true })
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true

const React = await import('react')
const { createRoot } = await import('react-dom/client')
const { act } = React
const { Harness } = await import(pathToFileURL(bundle).href)

const root = createRoot(document.getElementById('root'))
await act(async () => root.render(React.createElement(Harness)))
const text = (id) => document.getElementById(id)?.textContent
const click = async (id) => {
  await act(async () => document.getElementById(id).dispatchEvent(new g.MouseEvent('click', { bubbles: true })))
}
const fire = async (event) => {
  bridge.state = { running: true, tabs: [{ tabId: 't1', title: 'AI page', url: 'https://example.com' }] }
  await act(async () => {
    for (const listener of bridge.listeners) listener(event)
  })
}
const wait = async (predicate, label) => {
  const deadline = Date.now() + 2000
  while (Date.now() < deadline) {
    if (predicate()) return
    await act(async () => { await new Promise((resolve) => setTimeout(resolve, 10)) })
  }
  throw new Error(label)
}

if (bridge.listeners.size !== 1) throw new Error('浏览器事件订阅未建立')
if (text('open') !== 'closed') throw new Error('初始应为收起')

await fire({ type: 'visual-mode-requested' })
await wait(() => text('open') === 'open', '可视化请求必须展开侧栏')
if (bridge.requested.at(-1) !== 'session-a') throw new Error('宿主回调必须收到请求会话')

// 用户手动收起：解除绑定，同一轮再次请求也不展开。
await click('close')
await wait(() => text('open') === 'closed', '手动收起后必须关闭')
await fire({ type: 'visual-mode-requested' })
await act(async () => { await new Promise((resolve) => setTimeout(resolve, 20)) })
if (text('open') !== 'closed') throw new Error('手动收起后同一轮不得自动重开')

// 切到另一个会话：新会话的可视化请求重新展开并通知宿主。
await click('switch')
if (text('active') !== 'session-b') throw new Error('切换会话失败')
// 新一轮运行开始：解除上一轮的手动收起抑制。
await act(async () => {
  bridge.runningIds = { 'session-b': true }
  for (const listener of bridge.runListeners) listener()
})
await fire({ type: 'visual-mode-requested' })
await wait(() => text('open') === 'open', '新会话的可视化请求必须展开')
if (bridge.requested.at(-1) !== 'session-b') throw new Error('宿主回调必须收到新会话')

await act(async () => root.unmount())
if (bridge.listeners.size !== 0) throw new Error('卸载必须清理浏览器事件订阅')
console.log('ok browser sidebar: visual request expands, manual close suppresses, session switch rebinds')
