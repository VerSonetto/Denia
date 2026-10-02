import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'
import React, { act } from 'react'
import { createRoot } from 'react-dom/client'

const webRoot = join(dirname(fileURLToPath(import.meta.url)), '..')
const output = join(webRoot, 'node_modules', '.shots', 'session-view-memory.mjs')
mkdirSync(dirname(output), { recursive: true })
await build({ entryPoints: [join(webRoot, 'src', 'components', 'SessionView.tsx')], outfile: output,
  bundle: true, format: 'esm', platform: 'node', jsx: 'automatic', loader: { '.css': 'empty' },
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'], logLevel: 'silent',
  plugins: [{ name: 'session-stream-fixture', setup(builder) {
    builder.onLoad({ filter: /sessionStreams\.ts$/ }, () => ({ loader: 'ts', contents: `
export function attach(id, listener) {
  globalThis.fixtureListener = listener
  return () => { if (globalThis.fixtureListener === listener) globalThis.fixtureListener = null }
}
export function dropSession() {}
` }))
  } }],
})
const dom = new JSDOM('<!doctype html><div class="conversation-scroll"><div id="root"></div></div>', {
  url: 'http://localhost', pretendToBeVisual: true,
})
for (const key of ['window', 'document', 'HTMLElement', 'Element', 'Node', 'MutationObserver',
  'localStorage', 'navigator', 'getComputedStyle', 'requestAnimationFrame', 'cancelAnimationFrame']) {
  Object.defineProperty(globalThis, key, { configurable: true, value: dom.window[key] })
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true
globalThis.CSS = { escape: (value) => value }
dom.window.HTMLElement.prototype.scrollIntoView = function () {}
const { SessionView } = await import(pathToFileURL(output).href)
const root = createRoot(document.getElementById('root'))
let seq = 0
const event = (type, fields = {}) => ({ type, seq: ++seq, time: seq, ...fields })
const turn = (number) => [event('user-message', { text: `turn ${number}` }),
  event('turn-start', { turn: number }), event('assistant-message', { turn: number, step: 1,
    blocks: [{ type: 'text', text: `answer ${number}` }] }),
  event('turn-end', { turn: number, reason: { kind: 'completed' } })]
const header = { id: 'memory', cwd: 'D:/workspace' }
let nodes = []
let anchors = []
const props = { id: 'memory', pendingMessages: [], onNodesChange: (value) => { nodes = value },
  onAnchorsChange: (value) => { anchors = value } }
await act(async () => root.render(React.createElement(SessionView, props)))
await act(async () => fixtureListener.onSnapshot(header, [], { total: 0, hasMoreBefore: false, anchors: [] }))
await act(async () => {
  for (let number = 1; number <= 200; number++) for (const envelope of turn(number)) fixtureListener.onEnvelope(envelope)
  await new Promise((resolve) => setTimeout(resolve, 50))
})
assert.ok(nodes.filter((node) => node.kind === 'user').length <= 60)
assert.equal(anchors.length, 200)
assert.ok(document.querySelector('.load-older-btn'))
assert.match(document.querySelector('[data-user-anchor]').textContent, /turn 151/)
console.log('ok   live SessionView trims settled bottom history and keeps all 200 navigation anchors')
const scroller = document.querySelector('.conversation-scroll')
Object.defineProperty(scroller, 'scrollHeight', { configurable: true, value: 10000 })
await act(async () => {
  for (let number = 201; number <= 350; number++) for (const envelope of turn(number)) fixtureListener.onEnvelope(envelope)
  await new Promise((resolve) => setTimeout(resolve, 50))
})
assert.equal(nodes.filter((node) => node.kind === 'user').length, 200)
assert.equal(anchors.length, 350)
console.log('ok   reading older content does not trigger automatic window pruning')
let hidden = false
Object.defineProperty(document, 'hidden', { configurable: true, get: () => hidden })
await act(async () => {
  fixtureListener.onEnvelope(event('user-message', { text: 'hidden-window marker' }))
  hidden = true
  document.dispatchEvent(new dom.window.Event('visibilitychange'))
  await new Promise((resolve) => setTimeout(resolve, 160))
})
assert.equal(nodes.filter((node) => node.kind === 'user').length, 201)
assert.equal(anchors.length, 351)
console.log('ok   hiding the window flushes queued events without relying on animation frames')
let pendingRequest
globalThis.fetch = (url, options) => new Promise((resolve) => {
  pendingRequest = { url: String(url), signal: options.signal, resolve }
})
await act(async () => document.querySelector('.load-older-btn').click())
assert.ok(pendingRequest)
assert.match(pendingRequest.url, /limit=200/)
await act(async () => fixtureListener.onSnapshot(header, turn(400), { total: 4, hasMoreBefore: false, anchors: [] }))
assert.ok(pendingRequest.signal.aborted)
await act(async () => {
  pendingRequest.resolve({ ok: true, status: 200, json: async () => ({ header, events: turn(1), total: 1000,
    hasMoreBefore: true, anchors: [] }) })
})
assert.equal(nodes.filter((node) => node.kind === 'user').length, 1)
assert.match(document.querySelector('[data-user-anchor]').textContent, /turn 400/)
console.log('ok   new snapshots abort stale pagination and prevent old history from reappearing')
await act(async () => root.unmount())
assert.equal(globalThis.fixtureListener, null)
dom.window.close()
