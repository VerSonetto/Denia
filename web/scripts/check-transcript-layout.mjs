import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'
import React, { act } from 'react'
import { createRoot } from 'react-dom/client'

const webRoot = join(dirname(fileURLToPath(import.meta.url)), '..')
const output = join(webRoot, 'node_modules', '.shots', 'transcript-layout.mjs')
mkdirSync(dirname(output), { recursive: true })
await build({ stdin: { contents: `
export { Transcript } from './components/transcript'
export { ViewportRow } from './components/ViewportRow'
export { foldEvents, applyEnvelope } from './fold'
`, resolveDir: join(webRoot, 'src'), loader: 'tsx' }, outfile: output,
  bundle: true, format: 'esm', platform: 'node', jsx: 'automatic', loader: { '.css': 'empty' },
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'], logLevel: 'silent' })
const dom = new JSDOM('<!doctype html><div class="conversation-scroll"><div class="transcript-pane" id="root"></div></div>', {
  url: 'http://localhost', pretendToBeVisual: true,
})
for (const key of ['window', 'document', 'HTMLElement', 'Element', 'Node', 'MutationObserver',
  'localStorage', 'navigator', 'getComputedStyle']) {
  Object.defineProperty(globalThis, key, { configurable: true, value: dom.window[key] })
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true
globalThis.requestAnimationFrame = () => 1
globalThis.cancelAnimationFrame = () => {}
dom.window.HTMLElement.prototype.scrollIntoView = function () {}
let measuredHeight = 24
dom.window.HTMLElement.prototype.getBoundingClientRect = function () {
  const height = this.classList.contains('transcript-viewport-row')
    ? this.style.height ? Number.parseFloat(this.style.height) : measuredHeight
    : 0
  return { x: 0, y: 0, top: 0, left: 0, right: 800, bottom: height, width: 800, height }
}
const intersectionObservers = []
class MockIntersectionObserver {
  targets = new Set()
  constructor(callback) { this.callback = callback; intersectionObservers.push(this) }
  observe(target) { this.targets.add(target) }
  unobserve(target) { this.targets.delete(target) }
  disconnect() { this.targets.clear() }
  emit(visibleTargets) {
    this.callback([...this.targets].map(target => ({ target, isIntersecting: visibleTargets.has(target) })))
  }
}
const resizeObservers = []
class MockResizeObserver {
  targets = new Set()
  constructor(callback) { this.callback = callback; resizeObservers.push(this) }
  observe(target) { this.targets.add(target) }
  disconnect() { this.targets.clear() }
  emit() { this.callback([...this.targets].map(target => ({ target }))) }
}
globalThis.IntersectionObserver = MockIntersectionObserver
globalThis.ResizeObserver = MockResizeObserver
const { Transcript, ViewportRow, foldEvents, applyEnvelope } = await import(pathToFileURL(output).href)
const container = document.getElementById('root')
const root = createRoot(container)
const element = React.createElement
const marker = { kind: 'turn-start', turn: 1, time: Date.now() }
const end = { kind: 'turn-end', turn: 1, time: marker.time + 1000, reason: { kind: 'completed' } }
const tool = callId => ({ kind: 'tool', callId, name: 'grep', args: JSON.stringify({ pattern: callId }),
  result: { content: 'ok', isError: false } })
const assistant = (step, blocks = [], extra = {}) => ({ kind: 'assistant', turn: 1, step,
  streaming: false, interrupted: false, blocks, ...extra })
const toolBlock = { kind: 'tool-call', text: '', id: 'call', name: 'grep', args: '{}' }
const rows = () => [...container.querySelectorAll(':scope > .transcript-viewport-row')]
const render = async (nodes, props = {}) => {
  await act(async () => root.render(element(Transcript, { nodes, ...props })))
}
async function check(label, run) {
  await act(async () => root.render(null))
  measuredHeight = 24
  await run()
  console.log(`ok   ${label}`)
}

await check('tool-only, empty and whitespace assistant messages never create flex items', async () => {
  for (const streaming of [false, true]) {
    for (const blocks of [[], [toolBlock], [{ kind: 'text', text: '' }],
      [{ kind: 'text', text: ' \n\t' }], [{ kind: 'reasoning', text: '' }],
      [{ kind: 'reasoning', text: ' \n' }]]) {
      await render([marker, tool('first'), assistant(1, blocks, { streaming }), tool('last')])
      assert.equal(rows().length, 2)
      assert.ok(rows().every(row => row.querySelector('.tool-row')))
    }
  }
})
await check('cold replay and incremental tool events have identical visible rows', async () => {
  const events = [
    { seq: 1, time: marker.time, type: 'turn-start', turn: 1 },
    { seq: 2, time: marker.time + 1, type: 'assistant-chunk', turn: 1, step: 1,
      chunk: { type: 'block-start', index: 0, block_type: 'tool-call' } },
    { seq: 3, time: marker.time + 2, type: 'assistant-message', turn: 1, step: 1,
      blocks: [{ type: 'tool-call', id: 'call', name: 'grep', arguments: '{}' }] },
    { seq: 4, time: marker.time + 3, type: 'tool-call', turn: 1, step: 1,
      call_id: 'call', name: 'grep', arguments: '{}' },
  ]
  let live = []
  for (const event of events) {
    live = applyEnvelope(live, event)
    await render(live)
    assert.equal(rows().length, event.type === 'tool-call' ? 1 : 0)
  }
  await render(foldEvents(events))
  assert.equal(rows().length, 1)
  assert.ok(rows()[0].querySelector('.tool-row'))
})
await check('visible text, reasoning and interruption markers remain available', async () => {
  for (const node of [assistant(1, [{ kind: 'text', text: 'answer' }]),
    assistant(1, [{ kind: 'reasoning', text: 'thinking' }]),
    assistant(1, [{ kind: 'reasoning', text: 'thinking' }], { streaming: true }),
    assistant(1, [], { interrupted: true })]) {
    await render([marker, node])
    assert.equal(rows().length, 1)
    assert.ok(rows()[0].querySelector('.msg-assistant'))
  }
  await render([marker, assistant(1, [{ kind: 'text', text: '' },
    { kind: 'text', text: 'answer' }, { kind: 'reasoning', text: ' ' }])])
  assert.equal(container.querySelector('.msg-assistant').childElementCount, 1)
})
await check('final actions render only when a real copy or branch button exists', async () => {
  const blank = assistant(1, [toolBlock], { seq: 50 })
  await render([marker, blank, end])
  assert.equal(container.querySelector('.msg-assistant'), null)
  let forked
  await render([marker, blank, end], { onFork: seq => { forked = seq } })
  const branch = container.querySelector('.branch')
  assert.ok(branch)
  await act(async () => branch.dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true })))
  assert.equal(forked, 50)
  await render([marker, assistant(1, [], { seq: undefined }), end], { onFork() {} })
  assert.equal(container.querySelector('.message-actions'), null)
  await render([marker, assistant(1, [{ kind: 'text', text: 'answer' }]), end])
  assert.equal(container.querySelectorAll('.message-actions button').length, 1)
})
await check('eager rendering counts visible rows rather than hidden assistant nodes', async () => {
  const hidden = Array.from({ length: 40 }, (_, index) => assistant(index + 1, [toolBlock]))
  await render([marker, tool('first'), ...hidden, tool('last')])
  assert.equal(rows().length, 2)
  assert.equal(container.querySelectorAll('.tool-row').length, 2)
})
await check('row insertion, removal and streaming settle preserve existing row identity', async () => {
  const first = tool('first')
  const last = tool('last')
  await render([marker, first, last])
  const lastRow = rows()[1]
  await act(async () => lastRow.querySelector('button').dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true })))
  assert.ok(lastRow.querySelector('.disc-body'))
  const text = assistant(1, [{ kind: 'text', text: 'answer' }], { streaming: true })
  await render([marker, text, first, last])
  assert.equal(rows()[2], lastRow)
  assert.ok(lastRow.querySelector('.disc-body'))
  const textRow = rows()[0]
  await render([marker, { ...text, streaming: false, seq: 30 }, first, last])
  assert.equal(rows()[0], textRow)
  assert.equal(rows()[2], lastRow)
  await render([marker, first, last])
  assert.equal(rows()[1], lastRow)
  assert.ok(lastRow.querySelector('.disc-body'))
})
await check('closing a turn does not transfer row state to later turns', async () => {
  const nextMarker = { kind: 'turn-start', turn: 2, time: marker.time + 2000 }
  const nextTool = tool('next-turn')
  const first = tool('first')
  const blank = assistant(1, [{ kind: 'reasoning', text: ' ' }])
  await render([marker, blank, first, nextMarker, nextTool])
  const nextRow = rows()[1]
  await act(async () => nextRow.querySelector('button').dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true })))
  await render([marker, blank, first, end, nextMarker, nextTool])
  assert.equal(rows().at(-1), nextRow)
  assert.ok(nextRow.querySelector('.disc-body'))
  const overview = container.querySelector('.process-group')
  assert.ok(overview)
  await act(async () => overview.querySelector('button').dispatchEvent(new dom.window.MouseEvent('click', { bubbles: true })))
  assert.equal(overview.querySelector('.msg-assistant'), null)
  assert.equal(overview.querySelectorAll('.tool-row').length, 1)
})
await check('measured zero height replaces the initial placeholder estimate', async () => {
  measuredHeight = 0
  await act(async () => root.render(element(ViewportRow, { eager: true }, null)))
  const row = rows()[0]
  await act(async () => intersectionObservers.at(-1).emit(new Set()))
  assert.equal(row.style.height, '0px')
})
await check('resize to zero clears a previously measured nonzero height', async () => {
  measuredHeight = 240
  await act(async () => root.render(element(ViewportRow, { eager: true }, element('div', null, 'content'))))
  const row = rows()[0]
  measuredHeight = 0
  await act(async () => {
    root.render(element(ViewportRow, { eager: true }, null))
    for (const observer of resizeObservers) {
      if (observer.targets.has(row)) observer.emit()
    }
  })
  await act(async () => intersectionObservers.at(-1).emit(new Set()))
  assert.equal(row.style.height, '0px')
})
await check('leaving the viewport measures the latest height before a queued resize', async () => {
  measuredHeight = 240
  await act(async () => root.render(element(ViewportRow, { eager: true }, element('div', null, 'content'))))
  const row = rows()[0]
  measuredHeight = 0
  await act(async () => root.render(element(ViewportRow, { eager: true }, null)))
  await act(async () => intersectionObservers.at(-1).emit(new Set()))
  assert.equal(row.style.height, '0px')
})
await check('unmeasured rows retain an estimate and measured rows preserve their actual height', async () => {
  measuredHeight = 180
  await act(async () => root.render(element(ViewportRow, null, element('div', null, 'content'))))
  const row = rows()[0]
  assert.equal(row.style.height, '96px')
  await act(async () => intersectionObservers.at(-1).emit(new Set([row])))
  assert.equal(row.style.height, '')
  await act(async () => intersectionObservers.at(-1).emit(new Set()))
  assert.equal(row.style.height, '180px')
  await act(async () => intersectionObservers.at(-1).emit(new Set([row])))
  assert.match(row.textContent, /content/)
})

await act(async () => root.unmount())
dom.window.close()
console.log('\nALL PASS')
