import assert from 'node:assert/strict'
import { mkdirSync, readFileSync } from 'node:fs'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { join } from 'node:path'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const web = fileURLToPath(new URL('..', import.meta.url))
const outDir = join(web, 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const bundle = join(outDir, 'contained-wheel.mjs')
await build({
  stdin: {
    contents: `export { useContainedWheel } from './src/hooks/useContainedWheel';
      export { PlanReviewPanel } from './src/components/PlanReviewPanel';`,
    resolveDir: web,
  },
  outfile: bundle, bundle: true, format: 'esm', platform: 'node', jsx: 'automatic',
  external: ['react', 'react-dom', 'react/jsx-runtime', '@radix-ui/react-slider', 'motion/react'],
  loader: { '.css': 'empty' },
  logLevel: 'silent',
})
const dom = new JSDOM('<!doctype html><body><div id="root"></div></body>', {
  url: 'http://localhost/', pretendToBeVisual: true,
})
const w = dom.window
for (const key of ['window', 'document', 'navigator', 'HTMLElement', 'Element', 'Node', 'Event', 'WheelEvent', 'getComputedStyle', 'localStorage']) {
  Object.defineProperty(globalThis, key, { value: key === 'window' ? w : w[key], configurable: true })
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true
const React = await import('react')
const { act } = React
const { createRoot } = await import('react-dom/client')
const { useContainedWheel, PlanReviewPanel } = await import(pathToFileURL(bundle).href)
const root = createRoot(w.document.getElementById('root'))
const H = React.createElement
let ancestorWheels = 0
let toggle
function Harness() {
  const containedRef = useContainedWheel()
  const [show, setShow] = React.useState(true)
  toggle = setShow
  return H('div', { className: 'messages', onWheel: () => ancestorWheels++ },
    show && H('div', { className: 'prompt-scroll', ref: containedRef, style: { overflowY: 'auto', overflowX: 'hidden', lineHeight: '26px' } },
      H('div', { className: 'prompt-editor', contentEditable: true, suppressContentEditableWarning: true, style: { overflow: 'hidden' } },
        H('span', null, 'Long draft'))),
    H(PlanReviewPanel, { title: 'Plan', planText: '## Long plan\n\nContent', catalog: null, selection: null, onDecision: async () => {}, onExit() {} }),
  )
}
const query = selector => w.document.querySelector(selector)
function geometry(el, { height = 100, content = 500, width = 100, wide = 100 } = {}) {
  Object.defineProperties(el, {
    clientHeight: { configurable: true, value: height },
    scrollHeight: { configurable: true, value: content },
    clientWidth: { configurable: true, value: width },
    scrollWidth: { configurable: true, value: wide },
  })
  el.scrollTop = 0
  el.scrollLeft = 0
}
async function wheel(el, options) {
  const event = new w.WheelEvent('wheel', { bubbles: true, cancelable: true, ...options })
  await act(async () => el.dispatchEvent(event))
  return event
}
const check = (label, fn) => { fn(); console.log(`ok   ${label}`) }
try {
  await act(async () => root.render(H(React.StrictMode, null, H(Harness))))
  const messages = query('.messages')
  geometry(messages, { content: 2000 })
  messages.scrollTop = 900
  const prompt = query('.prompt-scroll')
  const editor = query('.prompt-editor')
  geometry(prompt)
  geometry(editor, { height: 500, content: 500 })
  const first = await wheel(editor.firstElementChild, { deltaY: 60 })
  check('long contentEditable scrolls its wrapper, never the conversation', () => {
    assert.equal(prompt.scrollTop, 60)
    assert.equal(editor.scrollTop, 0)
    assert.equal(messages.scrollTop, 900)
    assert(first.defaultPrevented)
    assert.equal(ancestorWheels, 0)
  })
  await wheel(editor, { deltaY: -20 })
  check('upward input scroll stays inside the wrapper', () => assert.equal(prompt.scrollTop, 40))
  prompt.scrollTop = 390
  await wheel(editor, { deltaY: 100 })
  await wheel(editor, { deltaY: 100 })
  check('large wheel deltas and bottom boundary do not escape', () => {
    assert.equal(prompt.scrollTop, 400)
    assert.equal(messages.scrollTop, 900)
    assert.equal(ancestorWheels, 0)
  })
  prompt.scrollTop = 0
  await wheel(editor, { deltaY: -100 })
  check('top boundary is contained', () => assert.equal(prompt.scrollTop, 0))
  geometry(prompt, { height: 100, content: 100 })
  await wheel(editor, { deltaY: 100 })
  check('short drafts cannot wheel-scroll the conversation', () => {
    assert.equal(prompt.scrollTop, 0)
    assert.equal(ancestorWheels, 0)
  })
  geometry(prompt)
  await wheel(editor, { deltaY: 2, deltaMode: 1 })
  check('line-mode deltas use the scroller line height', () => assert.equal(prompt.scrollTop, 52))
  await wheel(editor, { deltaY: 1, deltaMode: 2 })
  check('page-mode deltas use the scroller viewport', () => assert.equal(prompt.scrollTop, 152))
  const zoom = await wheel(editor, { deltaY: -100, ctrlKey: true })
  check('Ctrl+wheel preserves browser zoom', () => {
    assert(!zoom.defaultPrevented)
    assert.equal(prompt.scrollTop, 152)
  })
  ancestorWheels = 0

  const body = query('.plan-review-body')
  body.style.overflowY = 'auto'
  geometry(body)
  const paragraph = body.querySelector('p')
  await wheel(paragraph, { deltaY: 80 })
  check('plan paragraph wheels scroll the plan body', () => {
    assert.equal(body.scrollTop, 80)
    assert.equal(messages.scrollTop, 900)
    assert.equal(ancestorWheels, 0)
  })
  const clipped = w.document.createElement('div')
  clipped.style.overflowY = 'hidden'
  clipped.textContent = 'Clipped markdown content'
  paragraph.append(clipped)
  geometry(clipped)
  await wheel(clipped.firstChild, { deltaY: 30 })
  check('overflow:hidden geometry is not mistaken for a wheel scroller', () => {
    assert.equal(clipped.scrollTop, 0)
    assert.equal(body.scrollTop, 110)
  })
  clipped.style.overflowY = 'clip'
  await wheel(clipped, { deltaY: 20 })
  check('overflow:clip descendants also route to the real scroller', () => assert.equal(body.scrollTop, 130))
  const code = w.document.createElement('pre')
  code.style.overflowY = 'auto'
  code.style.overflowX = 'auto'
  geometry(code, { content: 200, wide: 500 })
  body.append(code)
  await wheel(code, { deltaY: 30 })
  check('a nested code scroller has priority', () => {
    assert.equal(code.scrollTop, 30)
    assert.equal(body.scrollTop, 130)
  })
  code.scrollTop = 100
  await wheel(code, { deltaY: 25 })
  check('nested boundary falls back only within the plan panel', () => assert.equal(body.scrollTop, 155))
  await wheel(code, { deltaX: 40, deltaY: 20 })
  check('diagonal wheels independently scroll both axes inside the panel', () => {
    assert.equal(code.scrollLeft, 40)
    assert.equal(body.scrollTop, 175)
  })
  await wheel(code, { deltaY: 20, shiftKey: true })
  check('Shift+wheel retains horizontal code scrolling', () => {
    assert.equal(code.scrollLeft, 60)
    assert.equal(body.scrollTop, 175)
  })
  body.scrollTop = 400
  await wheel(clipped, { deltaY: 100 })
  body.scrollTop = 0
  await wheel(paragraph, { deltaY: -100 })
  await wheel(query('.plan-review-head'), { deltaY: 100 })
  check('plan boundaries and non-scrollable panel chrome cannot reach the messages', () => {
    assert.equal(body.scrollTop, 0)
    assert.equal(messages.scrollTop, 900)
    assert.equal(ancestorWheels, 0)
  })
  const feedback = query('.plan-review-feedback-input')
  feedback.style.overflowY = 'auto'
  geometry(feedback)
  await wheel(feedback, { deltaY: 25 })
  check('plan feedback textarea remains independently scrollable', () => assert.equal(feedback.scrollTop, 25))
  const childHandler = event => event.preventDefault()
  code.addEventListener('wheel', childHandler, { passive: false })
  await wheel(code, { deltaX: 30 })
  check('a child control that handles wheel is not handled twice', () => assert.equal(code.scrollLeft, 60))
  code.removeEventListener('wheel', childHandler)

  await act(async () => toggle(false))
  const detached = await wheel(editor, { deltaY: 30 })
  check('conditional unmount removes the old native listener', () => assert(!detached.defaultPrevented))
  await act(async () => toggle(true))
  const replacement = query('.prompt-scroll')
  geometry(replacement)
  await wheel(query('.prompt-editor'), { deltaY: 30 })
  check('conditional remount installs one listener on the new wrapper', () => assert.equal(replacement.scrollTop, 30))
  const source = readFileSync(join(web, 'src/pages/SessionsPage.tsx'), 'utf8')
  check('the production composer attaches containment to its actual scroll wrapper', () => {
    assert.match(source, /<div className="prompt-scroll" ref={promptScrollRef}>/)
    assert.doesNotMatch(source, /onPromptWheel/)
  })
} finally {
  await act(async () => root.unmount())
  w.close()
}
