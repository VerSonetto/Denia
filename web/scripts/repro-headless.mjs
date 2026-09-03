// 无头复现:直接渲染 App 到 jsdom,加载目标会话,抓渲染异常。
import { readFileSync } from 'node:fs'

process.env.NODE_ENV = 'development'

// --- polyfills before react-dom loads ---
const { JSDOM } = await import('jsdom')
const dom = new JSDOM('<!doctype html><html><body><div id="root"></div></body></html>', {
  url: 'http://localhost:5173/#s=c41e7b39-871a-4f04-94d6-29d304821887',
  pretendToBeVisual: true,
})
const g = dom.window
for (const k of ['window','document','navigator','location','history','HTMLElement','Element','Node','Event','CustomEvent','MouseEvent','KeyboardEvent','PointerEvent','EventTarget','MutationObserver','ResizeObserver','IntersectionObserver','getComputedStyle','requestAnimationFrame','cancelAnimationFrame','matchMedia','localStorage','sessionStorage','DOMParser','Blob','URL','AbortController','EventSource','FileReader','HTMLIFrameElement','SVGSVGElement','CSS']) {
  try { globalThis[k] = g[k] } catch {}
}
globalThis.window = g
g.matchMedia = g.matchMedia || (() => ({ matches: false, media: '', addListener(){}, removeListener(){}, addEventListener(){}, removeEventListener(){}, dispatchEvent(){ return false } }))
g.requestAnimationFrame = cb => setTimeout(() => cb(Date.now()), 16)
g.cancelAnimationFrame = id => clearTimeout(id)
g.scrollTo = () => {}
g.HTMLElement.prototype.scrollIntoView = () => {}
g.HTMLElement.prototype.scrollTo = () => {}
g.HTMLElement.prototype.getBoundingClientRect = function () { return { top: 0, left: 0, right: 800, bottom: 600, width: 800, height: 600, x: 0, y: 0, toJSON(){} } }
g.Element.prototype.getBoundingClientRect = g.HTMLElement.prototype.getBoundingClientRect
if (!g.ResizeObserver) g.ResizeObserver = class { observe(){} unobserve(){} disconnect(){} }

const consoleErrors = []
const origErr = console.error
console.error = (...a) => { consoleErrors.push(a.map(String).join(' ')); }
const origWarn = console.warn
console.warn = (...a) => { consoleErrors.push('[warn] ' + a.map(String).join(' ')) }
process.on('uncaughtException', e => { console.log('UNCAUGHT:', e && (e.stack || e.message || String(e))); process.exit(3) })
process.on('unhandledRejection', e => { console.log('UNHANDLED REJECTION:', e && (e && (e.stack || e.message) || String(e))); process.exit(3) })

// --- module loading with our fetch shims ---
const { pathToFileURL } = await import('node:url')
const Module = await import('node:module')
const mod = new Module.Module('file://' + process.cwd().replace(/\\/g, '/') + '/x.mjs')
mod.constructor._load // noop
// Node ESM has no global loader hook; instead pre-wire fetch by importing api shim first via query param.
// Simplest: patch globalThis.fetch before App module graph loads. Use dynamic import with cache-bust after defining fetch.
const { createRequire } = await import('node:module')
const require = createRequire(import.meta.url)

// 1. Serve /api requests from local data instead of HTTP.
const raw = JSON.parse(readFileSync(process.argv[2], 'utf8'))
const header = raw.header ?? { type: 'session', version: 0, id: 'c41e7b39-871a-4f04-94d6-29d304821887', created_at: 1788439624567, cwd: 'D:\\code_project\\dsh-rs', sandbox: false }
const events = raw.events
const listSessions = JSON.parse(readFileSync(new URL('./fixtures/list-sessions.json', import.meta.url), 'utf8'))
const catalog = JSON.parse(readFileSync(new URL('./fixtures/catalog.json', import.meta.url), 'utf8'))
const providers = { providers: [], configurable: [] }
const settings = { namespaces: [], revision: 0 }

globalThis.fetch = async (input, init) => {
  const url = typeof input === 'string' ? input : (input && input.url) || String(input)
  const body = (code, obj) => new g.Response(JSON.stringify(obj), { status: code, headers: { 'content-type': 'application/json' } })
  console.log('[fetch]', url)
  if (url.includes('/api/sessions?') || url.endsWith('/api/sessions')) return body(200, listSessions)
  if (url.includes('/api/workspaces')) return body(200, { workspaces: listSessions.workspaces ?? [] })
  if (url.includes('/api/sessions/c41e7b39-871a-4f04-94d6-29d304821887/follow')) return new g.Response('', { status: 200 })
  if (url.includes('/api/sessions/c41e7b39-871a-4f04-94d6-29d304821887')) return body(200, { header, events })
  if (url.includes('/api/settings')) return body(200, settings)
  if (url.includes('/api/llm/catalog')) return body(200, catalog)
  if (url.includes('/api/llm/providers')) return body(200, providers)
  if (url.includes('/api/fs/capability')) return body(200, { kind: 'browse' })
  if (url.includes('/api/events')) return new g.Response('', { status: 200 })
  if (url.includes('/api/llm/default-model')) return body(200, catalog.default ?? {})
  if (url.includes('/context-breakdown')) return body(200, { breakdown: { systemTokens: 0, toolsTokens: 0, messageTokens: 0 }, pressure: {}, usage: { uncachedInputTokens: 0, outputTokens: 0, cacheReadTokens: 0, cacheWriteTokens: 0, reasoningTokens: 0 } })
  return body(404, { error: { code: 'nope', message: 'not mocked: ' + url } })
}

// 2. EventSource no-op.
class FakeEventSource { constructor(){ this.close=()=>{}; this.onopen=null; this.onerror=null; this.onmessage=null } }
globalThis.EventSource = FakeEventSource
g.EventSource = FakeEventSource

// 3. Load React + App from the real web/src graph.
const ReactCjs = await import('react')
const ReactDomCjs = await import('react-dom/client')
const React = ReactCjs.default ?? ReactCjs
const { createRoot } = ReactDomCjs
globalThis.React = React
console.log('IMPORTING APP');
console.log('IMPORT START: back in main')
const AppMod = await import('./app-bundle.mjs').catch(e => { console.log('APP IMPORT FAILED:', e && e.stack); throw e })
delete g.setTimeout; delete g.clearTimeout; delete g.setInterval; delete g.clearInterval;
for (const [k, v] of Object.entries(g)) { try { globalThis[k] = v } catch {} }
globalThis.setTimeout = setTimeout; globalThis.clearTimeout = clearTimeout; globalThis.setInterval = setInterval; globalThis.clearInterval = clearInterval;
console.log('modules loaded')

const root = createRoot(g.document.getElementById('root'))
await new Promise(r => setTimeout(r, 50))
root.render(React.createElement(AppMod.default))
console.log('BACK IN MAIN')
await new Promise(r => setTimeout(r, 3000))
console.log('rendered; DOM length:', g.document.getElementById('root').innerHTML.length)
console.log('--- console.error entries:', consoleErrors.length)
for (const line of consoleErrors.slice(0, 30)) console.log('CERR>', line.slice(0, 500))
process.exit(0)