import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'
import React, { act, useEffect, useMemo } from 'react'
import { createRoot } from 'react-dom/client'

const webRoot = join(dirname(fileURLToPath(import.meta.url)), '..')
const output = join(webRoot, 'node_modules', '.shots', 'viewport-memory.mjs')
mkdirSync(dirname(output), { recursive: true })
await build({ stdin: { contents: `
export { ViewportRow } from './components/ViewportRow'
export { Transcript } from './components/transcript'
export { FileReaderPanel } from './components/FileReaderPanel'
export { CodeBlock } from './markdown/CodeBlock'
`, resolveDir: join(webRoot, 'src'), loader: 'tsx' }, outfile: output,
  bundle: true, format: 'esm', platform: 'node', jsx: 'automatic', loader: { '.css': 'empty' },
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'], logLevel: 'silent' })
const dom = new JSDOM('<!doctype html><div class="conversation-scroll"><div id="root"></div></div>', {
  url: 'http://localhost', pretendToBeVisual: true,
})
for (const key of ['window', 'document', 'HTMLElement', 'Element', 'Node', 'MutationObserver',
  'localStorage', 'navigator', 'getComputedStyle']) {
  Object.defineProperty(globalThis, key, { configurable: true, value: dom.window[key] })
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true
globalThis.CSS = { escape: (value) => value.replace(/[^a-zA-Z0-9_-]/g, (character) => `\\${character}`) }
dom.window.HTMLElement.prototype.scrollIntoView = function () {}
let connected = 0
let disconnected = 0
let mounted = 0
const observerInstances = []
class MockIntersectionObserver {
  targets = new Set()
  constructor(callback) { this.callback = callback; observerInstances.push(this); connected++ }
  observe(target) { this.targets.add(target) }
  unobserve(target) { this.targets.delete(target) }
  disconnect() { this.targets.clear(); disconnected++ }
  emit(visibleTargets) {
    this.callback([...this.targets].map((target) => ({ target, isIntersecting: visibleTargets.has(target) })))
  }
}
globalThis.IntersectionObserver = MockIntersectionObserver
const { ViewportRow, Transcript, FileReaderPanel, CodeBlock } = await import(pathToFileURL(output).href)
const container = document.getElementById('root')
const root = createRoot(container)
const element = React.createElement
function HeavyRow({ index }) {
  const cached = useMemo(() => Array.from({ length: 80 }, (_, line) => `${index}:${line}:${'cached'.repeat(20)}`), [index])
  useEffect(() => { mounted++; return () => { mounted-- } }, [])
  return element('pre', null, cached.map((text, line) => element('span', { key: line }, text)))
}
await act(async () => root.render(element(React.Fragment, null,
  Array.from({ length: 1000 }, (_, index) => element(ViewportRow, { key: index, eager: true }, element(HeavyRow, { index }))),
)))
assert.equal(mounted, 1000)
assert.equal(connected, 1)
const beforeNodes = container.querySelectorAll('*').length
globalThis.gc?.()
const beforeHeap = process.memoryUsage().heapUsed
await act(async () => {
  const observer = observerInstances.at(-1)
  observer.emit(new Set([...observer.targets].slice(-12)))
})
assert.equal(mounted, 12)
const afterNodes = container.querySelectorAll('*').length
assert.ok(afterNodes < beforeNodes * 0.03)
globalThis.gc?.()
const afterHeap = process.memoryUsage().heapUsed
console.log(`ok   1,000 code rows: mounted 1,000 -> ${mounted}; DOM ${beforeNodes} -> ${afterNodes}`)
if (globalThis.gc) console.log(`info synthetic JS heap: ${(beforeHeap / 1048576).toFixed(1)} -> ${(afterHeap / 1048576).toFixed(1)} MiB`)
await act(async () => {
  const observer = observerInstances.at(-1)
  observer.emit(new Set([...[...observer.targets].slice(0, 3), ...[...observer.targets].slice(-12)]))
})
assert.equal(mounted, 15)
assert.match(container.textContent, /0:0:cached/)
await act(async () => root.render(null))
assert.equal(mounted, 0)
assert.equal(disconnected, 1)
console.log('ok   scroll-back rehydrates content and unmount disconnects the shared observer')
const nodes = [
  { kind: 'user', text: 'original user message', anchor: 42 },
  ...Array.from({ length: 40 }, (_, index) => ({ kind: 'assistant', turn: index + 1, step: 1,
    streaming: false, interrupted: false, blocks: [{ kind: 'text', text: `answer ${index}` }] })),
]
await act(async () => root.render(element(Transcript, { nodes, pendingMessages: [{ text: 'sending' }] })))
await act(async () => observerInstances.at(-1).emit(new Set()))
assert.ok(container.querySelector('[data-user-anchor="42"]'))
assert.ok(container.querySelector('[data-user-anchor="pending"]'))
assert.equal(container.querySelectorAll('.markdown').length, 0)
await act(async () => {
  const observer = observerInstances.at(-1)
  observer.emit(new Set([...observer.targets].slice(0, 1)))
})
assert.match(container.textContent, /answer 0/)
await act(async () => root.render(null))
console.log('ok   transcript keeps history-jump anchors and pending rows while releasing markdown DOM')
let requests = []
globalThis.fetch = async (url, options) => {
  requests.push({ url: String(url), signal: options?.signal })
  return { ok: true, status: 200, json: async () => ({ content: `content ${new URL(url, 'http://localhost').searchParams.get('file')}`,
    size: 32, language: '', path: 'file' }) }
}
const files = Array.from({ length: 20 }, (_, index) => ({ path: `file-${index}.txt` }))
const fileProps = { workspacePath: 'D:/workspace', files, onActivate() {}, onCloseFile() {} }
await act(async () => root.render(element(FileReaderPanel, { ...fileProps, activeFile: files[0].path })))
assert.equal(requests.length, 1)
await act(async () => root.render(element(FileReaderPanel, { ...fileProps, activeFile: files[1].path })))
assert.equal(requests.length, 2)
assert.ok(requests[0].signal.aborted)
await act(async () => root.render(element(FileReaderPanel, { ...fileProps, activeFile: files[0].path })))
assert.equal(requests.length, 3)
assert.match(container.textContent, /content file-0.txt/)
await act(async () => root.render(null))
assert.ok(requests.at(-1).signal.aborted)
console.log('ok   20 file tabs load only the active file; switching releases old content and aborts stale requests')
const codeProps = { code: '{"ready":true}', lang: 'json', copyLabel: 'copy', copiedLabel: 'copied' }
await act(async () => {
  root.render(element(CodeBlock, codeProps))
})
await act(async () => { await new Promise((resolve) => setTimeout(resolve, 100)) })
assert.ok(container.querySelector('.shiki'))
assert.equal(container.querySelector('pre').textContent, codeProps.code)
await act(async () => root.render(element(CodeBlock, { ...codeProps, code: '{"ready":', streaming: true })))
assert.equal(container.querySelector('pre').textContent, '{"ready":')
await act(async () => root.render(element(CodeBlock, codeProps)))
assert.equal(container.querySelector('pre').textContent, codeProps.code)
await act(async () => root.render(element(CodeBlock, { ...codeProps, lang: 'rust', code: 'fn main() {}' })))
await act(async () => { await new Promise((resolve) => setTimeout(resolve, 100)) })
assert.ok(container.querySelector('.shiki'))
assert.equal(container.querySelector('pre').textContent, 'fn main() {}')
await act(async () => root.unmount())
dom.window.close()
console.log('ok   deferred highlighter loads JSON and Rust, preserving streaming/final text')
