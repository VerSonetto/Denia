// Run with: node web/scripts/check-composer-context-menu.mjs
// Real component + PanePopover + API, bundled like check-file-tree/check-conversation-hooks.
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { createRequire, Module } from 'node:module'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const web = fileURLToPath(new URL('..', import.meta.url))
// Compile in memory with an explicit working directory. No shared .shots filename,
// no on-disk bundle import cache, and no caller-cwd-dependent package resolution.
const result = await build({
  absWorkingDir: web,
  stdin: {
    contents: `
      import { useRef } from 'react'
      import { ComposerContextMenu } from './components/ComposerContextMenu'
      export { t } from './i18n'
      export function ComposerHarness(props) {
        const panelAnchorRef = useRef(null)
        return <div ref={panelAnchorRef} data-composer-panel-anchor="true">
          <ComposerContextMenu {...props} panelAnchorRef={panelAnchorRef} />
        </div>
      }
    `,
    resolveDir: join(web, 'src'), sourcefile: 'composer-context-menu-test.tsx', loader: 'tsx',
  },
  bundle: true, write: false, format: 'cjs', platform: 'node', target: 'node20', jsx: 'automatic',
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  loader: { '.css': 'empty' }, logLevel: 'silent',
})
const dom = new JSDOM('<body><div id="root"></div><div id="editor" tabindex="0"></div><button id="outside">outside</button></body>', {
  url: 'http://localhost/', pretendToBeVisual: true,
})
const g = dom.window
const style = g.document.createElement('style')
style.textContent = readFileSync(join(web, 'src/components/ComposerContextMenu.css'), 'utf8')
g.document.head.append(style)
for (const name of ['window', 'document', 'navigator', 'HTMLElement', 'Element', 'Node', 'Event', 'MouseEvent',
  'KeyboardEvent', 'getComputedStyle', 'requestAnimationFrame', 'cancelAnimationFrame']) {
  Object.defineProperty(globalThis, name, { value: g[name], configurable: true })
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true
let scrolled = []
g.HTMLElement.prototype.scrollIntoView = function (options) { scrolled.push({ key: this.id, options }) }
// jsdom has no layout. Keep the input wrapper and trigger intentionally distinct, and model
// the measured panel from its actual inline width/available-height rather than fixed output.
const initialViewport = { width: g.innerWidth, height: g.innerHeight }
let composerRect = { left: 100, top: 500, width: 600, height: 180 }
let naturalPopoverHeight = 420
const rect = ({ left, top, width, height }) => ({
  x: left, y: top, left, top, width, height, right: left + width, bottom: top + height,
  toJSON() { return { left, top, width, height } },
})
const originalGetRect = g.HTMLElement.prototype.getBoundingClientRect
g.HTMLElement.prototype.getBoundingClientRect = function () {
  if (this.hasAttribute('data-composer-panel-anchor')) return rect(composerRect)
  if (this.classList.contains('composer-context-trigger')) return rect({ left: 110, top: 640, width: 24, height: 24 })
  if (this.classList.contains('composer-context-popover')) {
    const availableHeight = Number.parseFloat(this.style.getPropertyValue('--popover-available-height'))
    // jsdom exposes declarations but does not evaluate min()/viewport units. Derive the
    // simulated height cap from the real CSS instead of duplicating its desktop maximum.
    const panelMaxHeight = g.getComputedStyle(this.querySelector('.composer-context-panel')).maxHeight
    const heightCaps = [...panelMaxHeight.matchAll(/(?<![\w-])(\d+(?:\.\d+)?)px/g)].map(match => Number(match[1]))
    assert.ok(heightCaps.length, 'panel max-height must expose its CSS pixel cap')
    const viewportHeightOffset = panelMaxHeight.match(/100d?vh\s*-\s*(\d+(?:\.\d+)?)px/)
    assert.ok(viewportHeightOffset, 'panel max-height must preserve viewport height containment')
    // The calc offset is not itself a height cap.
    const desktopHeightCaps = heightCaps.filter(cap => cap !== Number(viewportHeightOffset[1]))
    return rect({
      left: Number.parseFloat(this.style.left) || 0,
      top: Number.parseFloat(this.style.top) || 0,
      width: Math.min(Number.parseFloat(this.style.width) || 340, g.innerWidth - 16),
      height: Math.max(0, Math.min(naturalPopoverHeight, ...desktopHeightCaps,
        g.innerHeight - Number(viewportHeightOffset[1]), Number.isFinite(availableHeight) ? availableHeight : Infinity)),
    })
  }
  return originalGetRect.call(this)
}
const observers = new Set()
class MockResizeObserver {
  targets = new Set()
  constructor(callback) { this.callback = callback; observers.add(this) }
  observe(target) { this.targets.add(target) }
  unobserve(target) { this.targets.delete(target) }
  disconnect() { this.targets.clear(); observers.delete(this) }
}
Object.defineProperty(globalThis, 'ResizeObserver', { value: MockResizeObserver, configurable: true })
Object.defineProperty(g, 'ResizeObserver', { value: MockResizeObserver, configurable: true })
function notifyResize(target) {
  for (const observer of [...observers]) {
    if (observer.targets.has(target)) observer.callback([{ target, contentRect: target.getBoundingClientRect() }], observer)
  }
}
const React = await import('react')
const { act } = React
const { createRoot } = await import('react-dom/client')
const filename = join(web, 'scripts', '__composer-context-menu-harness.cjs')
const bundled = new Module(filename)
bundled.filename = filename
bundled.paths = Module._nodeModulePaths(web)
bundled.require = createRequire(filename)
bundled._compile(result.outputFiles[0].text, filename)
const { ComposerHarness, t } = bundled.exports
assert.equal(typeof ComposerHarness, 'function', 'compiled harness must export ComposerHarness')
assert.equal(typeof t, 'function', 'compiled harness must export the real i18n translator')
const H = React.createElement
const sleep = (ms = 175) => new Promise(resolve => setTimeout(resolve, ms))
const flushSearch = () => act(async () => { await sleep() })
const editor = document.getElementById('editor')
const requests = []
let respondAutomatically = false
let fixture = []
const response = items => ({ ok: true, status: 200, json: async () => ({ items }) })
globalThis.fetch = (url, init) => new Promise(resolve => {
  const parsed = new URL(url, 'http://localhost/')
  assert.equal(parsed.pathname, '/api/fs/mentions')
  const request = {
    path: parsed.searchParams.get('path'), query: parsed.searchParams.get('query') ?? '',
    signal: init.signal, done: false,
    resolve(value) { request.done = true; resolve(value) },
  }
  requests.push(request)
  // Intentionally ignores abort, so stale-response guards are exercised as well as signals.
  if (respondAutomatically) request.resolve(response(fixture))
})
const file = path => ({ path, kind: 'file' })
const directory = path => ({ path, kind: 'directory' })
const session = (id, extra = {}) => ({ id, title: `Title ${id}`, excerpt: `Excerpt ${id}`, cwd: 'D:/work', created_at: 1, ...extra })
const trigger = () => document.querySelector('.composer-context-trigger')
const menu = () => document.querySelector('[role="menu"]')
const search = () => document.querySelector('.composer-context-search input')
const fileRows = () => [...document.querySelectorAll('[data-context-item="file"]')]
const sessionRows = () => [...document.querySelectorAll('[data-context-item="session"]')]
const row = (kind, value) => [...document.querySelectorAll(`[data-context-item="${kind}"]`)]
  .find(node => value === undefined || node.dataset.contextPath === value || node.dataset.contextSession === value || node.dataset.contextCommand === value)
const closeButton = () => document.querySelector(`[aria-label="${t('contextClose')}"]`)
const retryButtons = () => [...document.querySelectorAll('[data-context-item="retry"]')]
const click = node => act(async () => { assert.ok(node, 'click target must exist'); node.click() })
const open = () => click(trigger())
const setQuery = value => act(async () => {
  const input = search()
  Object.getOwnPropertyDescriptor(g.HTMLInputElement.prototype, 'value').set.call(input, value)
  input.dispatchEvent(new g.Event('input', { bubbles: true }))
})
async function key(node, value, options = {}) {
  const event = new g.KeyboardEvent('keydown', { key: value, bubbles: true, cancelable: true, ...options })
  await act(async () => node.dispatchEvent(event))
  return event
}
const settle = (request, items) => act(async () => request.resolve(response(items)))
let root
let props
let calls
let sessionHandler
async function mount(overrides = {}) {
  calls = { open: 0, upload: 0, files: [], commands: [], sessions: [], sent: 0, focusAtOpen: null, uploadWhileOpen: false }
  sessionHandler = async () => { editor.focus() }
  props = {
    workspacePath: 'D:/work', activeId: 'active', sessions: [session('active'), session('other')],
    commands: [{ name: 'plan', label: 'Plan', description: 'Make a plan' }, { name: 'compact', label: 'Compact', description: 'Summarize context' }],
    onOpen() { calls.open++; calls.focusAtOpen = document.activeElement },
    onUpload() { calls.upload++; calls.uploadWhileOpen = !!menu() },
    onFile(candidate) { calls.files.push(candidate); editor.focus() },
    onCommand(name) { calls.commands.push(name); editor.focus() },
    onSession(summary, signal) { calls.sessions.push({ summary, signal }); return sessionHandler(summary, signal) },
    ...overrides,
  }
  root = createRoot(document.getElementById('root'))
  await render()
}
async function render(overrides = {}) {
  props = { ...props, ...overrides }
  await act(async () => root.render(H('div', { onKeyDown: event => { if (event.key === 'Enter') calls.sent++ } }, H(ComposerHarness, props))))
}
async function unmount() {
  if (root) await act(async () => root.unmount())
  root = null
  await act(async () => { for (const request of requests) if (!request.done) request.resolve(response([])) })
}
function deferred() {
  let resolve, reject
  const promise = new Promise((yes, no) => { resolve = yes; reject = no })
  return { promise, resolve, reject }
}
async function test(label, run) {
  requests.length = 0
  scrolled = []
  fixture = [directory('src'), file('README.md')]
  respondAutomatically = true
  composerRect = { left: 100, top: 500, width: 600, height: 180 }
  naturalPopoverHeight = 420
  Object.defineProperty(g, 'innerWidth', { value: initialViewport.width, writable: true, configurable: true })
  Object.defineProperty(g, 'innerHeight', { value: initialViewport.height, writable: true, configurable: true })
  try {
    await run()
    console.log(`ok   ${label}`)
  } finally { await unmount() }
}

try {
  await test('+ only opens a portal and snapshots selection; only upload invokes the picker synchronously', async () => {
    await mount()
    await flushSearch()
    assert.equal(requests.length, 0, 'closed menu never fetches')
    editor.focus()
    const mouseDown = new g.MouseEvent('mousedown', { bubbles: true, cancelable: true })
    await act(async () => trigger().dispatchEvent(mouseDown))
    assert.equal(mouseDown.defaultPrevented, true)
    assert.equal(document.activeElement, editor)
    await open()
    assert.equal(calls.open, 1)
    assert.equal(calls.focusAtOpen, editor)
    assert.equal(calls.upload, 0)
    assert.equal(document.activeElement, search())
    assert.equal(trigger().getAttribute('aria-expanded'), 'true')
    assert.ok(!document.getElementById('root').contains(menu()), 'menu is portalled')
    assert.equal(menu().getAttribute('aria-label'), t('addContext'))
    await flushSearch()
    assert.deepEqual(requests.map(({ path, query }) => ({ path, query })), [{ path: 'D:/work', query: '' }])
    await click(row('upload'))
    assert.equal(calls.upload, 1)
    assert.equal(calls.uploadWhileOpen, true, 'onUpload runs before dismissal, within the click')
    assert.equal(menu(), null)
    assert.equal(trigger().getAttribute('aria-expanded'), 'false')
  })

  await test('popover spans the whole composer above its top, clamps narrow viewport width and observes anchor/content resize', async () => {
    await mount(); await open()
    const anchor = document.querySelector('[data-composer-panel-anchor]')
    const popover = document.querySelector('.composer-context-popover')
    assert.equal(popover.dataset.positioned, 'true')
    assert.deepEqual(anchor.getBoundingClientRect().toJSON(), { left: 100, top: 500, width: 600, height: 180 })
    assert.deepEqual(trigger().getBoundingClientRect().toJSON(), { left: 110, top: 640, width: 24, height: 24 })
    assert.equal(Number.parseFloat(popover.style.left), 100, 'horizontal origin is composer, not the + button')
    assert.equal(Number.parseFloat(popover.style.width), 600, 'inline width matches the full input wrapper')
    assert.equal(popover.style.getPropertyValue('--popover-available-height'), '488px')
    assert.ok(popover.getBoundingClientRect().bottom <= 496, 'panel bottom leaves 4px above input top')
    assert.ok([...observers].some(observer => observer.targets.has(anchor) && observer.targets.has(popover)),
      'ResizeObserver watches both the position anchor and dynamic menu content')

    // Oversized results must consume available space above the input, not flip over it.
    naturalPopoverHeight = 900
    await act(async () => notifyResize(popover))
    assert.equal(popover.getBoundingClientRect().height, 488)
    assert.ok(popover.getBoundingClientRect().top >= 8)
    assert.ok(popover.getBoundingClientRect().bottom <= 496)

    // Growing the editor upwards with its bottom fixed changes the input top, not the button.
    composerRect = { ...composerRect, top: 380, height: 300 }
    await act(async () => notifyResize(anchor))
    assert.equal(popover.style.getPropertyValue('--popover-available-height'), '368px')
    assert.equal(popover.getBoundingClientRect().height, 368)
    assert.ok(popover.getBoundingClientRect().bottom <= 376, 'anchor resize keeps menu above expanded input')
    assert.equal(Number.parseFloat(popover.style.left), 100)

    await act(async () => {
      g.innerWidth = 360
      g.dispatchEvent(new g.Event('resize'))
      await sleep(40)
    })
    assert.equal(Number.parseFloat(popover.style.width), 344)
    assert.ok(Number.parseFloat(popover.style.width) <= g.innerWidth - 16)
    assert.equal(popover.getBoundingClientRect().left, 8)
    assert.ok(popover.getBoundingClientRect().right <= g.innerWidth - 8)
    assert.ok(popover.getBoundingClientRect().bottom <= 376)

    // The wrapper only measures position. The real trigger remains an inside-click target
    // and still owns Escape focus restoration, regardless of its different geometry.
    await act(async () => trigger().dispatchEvent(new g.Event('pointerdown', { bubbles: true })))
    assert.ok(menu())
    await key(search(), 'Escape')
    assert.equal(menu(), null)
    assert.equal(document.activeElement, trigger())
    assert.equal(observers.size, 0, 'close disconnects the geometry observer')
  })

  await test('empty file listing caps at 10; selected file/directory preserves the candidate and editor focus', async () => {
    fixture = [directory('src'), ...Array.from({ length: 15 }, (_, i) => file(`file-${i}.ts`))]
    await mount()
    await open(); await flushSearch()
    assert.equal(fileRows().length, 10)
    await click(row('file', 'file-0.ts'))
    assert.deepEqual(calls.files, [file('file-0.ts')])
    assert.equal(menu(), null)
    assert.equal(document.activeElement, editor)
    await open(); await flushSearch()
    assert.ok(row('file', 'src').textContent.endsWith('/'))
    await click(row('file', 'src'))
    assert.deepEqual(calls.files.at(-1), directory('src'))
    assert.equal(calls.upload, 0)
    assert.equal(document.activeElement, editor)
  })

  await test('150ms search debounce reaches candidates past the initial 10 and rejects stale query results', async () => {
    respondAutomatically = false
    await mount(); await open(); await flushSearch()
    await settle(requests[0], [file('root')])
    await setQuery('old')
    await flushSearch()
    const old = requests[1]
    await setQuery('not-final')
    await setQuery(' final ')
    assert.equal(old.signal.aborted, true)
    await settle(old, [file('stale')])
    assert.equal(fileRows().length, 0)
    await flushSearch()
    assert.equal(requests.length, 3)
    assert.equal(requests[2].query, 'final')
    const many = Array.from({ length: 14 }, (_, i) => file(`found-${i}`))
    await settle(requests[2], many)
    assert.equal(fileRows().length, 14, 'non-empty searches do not truncate file matches')
    await click(row('file', 'found-13'))
    assert.deepEqual(calls.files, [file('found-13')])
    await open(); await flushSearch()
    assert.equal(requests.at(-1).query, '', 'reopen resets search')
    await settle(requests.at(-1), [])
    assert.ok(menu().textContent.includes(t('contextFilesEmpty')))
    await setQuery('   '); await flushSearch()
    assert.equal(requests.at(-1).query, '', 'whitespace changes cannot strand an aborted search')
  })

  await test('workspace sessions normalize paths, exclude current/missing/foreign cwd, cap at 10, search title/id/excerpt', async () => {
    const matching = Array.from({ length: 13 }, (_, i) => session(`s${i}`, { cwd: 'd:\\WORK\\',
      ...(i === 12 ? { title: 'Hidden title', excerpt: 'Unique excerpt marker' } : {}) }))
    await mount({ sessions: [session('active'), session('foreign', { cwd: 'D:/else' }), session('unknown', { cwd: undefined }),
      session('prefix', { cwd: 'D:/work-other' }), ...matching] })
    await open(); await flushSearch()
    assert.deepEqual(sessionRows().map(node => node.dataset.contextSession), matching.slice(0, 10).map(item => item.id))
    await setQuery('hidden TITLE')
    assert.deepEqual(sessionRows().map(node => node.dataset.contextSession), ['s12'])
    await setQuery('s12')
    assert.deepEqual(sessionRows().map(node => node.dataset.contextSession), ['s12'])
    await setQuery('unique EXCERPT')
    assert.deepEqual(sessionRows().map(node => node.dataset.contextSession), ['s12'])
    await click(row('session', 's12'))
    assert.equal(calls.sessions[0].summary, matching[12])
    assert.equal(menu(), null)
    assert.equal(document.activeElement, editor)
    await open(); await setQuery('no matching session')
    assert.ok(menu().textContent.includes(t('contextSessionsEmpty')))
  })

  await test('commands are searchable by name, label and description; command selection never uploads', async () => {
    await mount(); await open()
    assert.equal(document.querySelector('.composer-context-modes'), null, 'modes use ordinary list rows, not a card grid')
    for (const command of document.querySelectorAll('[data-context-item="command"]')) {
      assert.equal(command.className, row('upload').className, 'mode and attachment rows share the same styling')
      assert.ok(command.querySelector('svg'), 'mode row renders a library icon')
    }
    const popoverRule = [...style.sheet.cssRules].find(rule => rule.selectorText === '.composer-context-popover')
    assert.equal(popoverRule.style.getPropertyValue('background'), 'var(--bg-layer-1)', 'menu uses the clean surface, not the gray elevated layer')
    await setQuery('summarize')
    assert.equal(row('command', 'plan'), undefined)
    assert.ok(row('command', 'compact'))
    await click(row('command', 'compact'))
    assert.deepEqual(calls.commands, ['compact'])
    assert.equal(calls.upload, 0)
    assert.equal(menu(), null)
    assert.equal(document.activeElement, editor)
  })

  await test('close cancels both timers and requests; late results cannot populate a reopened menu', async () => {
    respondAutomatically = false
    await mount(); await open(); await click(closeButton()); await flushSearch()
    assert.equal(requests.length, 0, 'close cancels pre-request debounce')
    await open(); await flushSearch()
    const old = requests[0]
    await click(closeButton())
    assert.equal(old.signal.aborted, true)
    await open(); await flushSearch()
    await settle(requests[1], [file('current')])
    await settle(old, [file('late')])
    assert.deepEqual(fileRows().map(node => node.dataset.contextPath), ['current'])
  })

  await test('network errors remain visible and retry starts a new search', async () => {
    respondAutomatically = false
    await mount(); await open(); await flushSearch()
    await act(async () => requests[0].resolve({ ok: false, status: 503, statusText: 'Unavailable',
      json: async () => ({ error: { code: 'offline', message: 'Network unavailable' } }) }))
    assert.ok(menu().textContent.includes('Network unavailable'))
    assert.equal(document.querySelector('[role="alert"]').textContent, 'Network unavailable')
    assert.equal(retryButtons().length, 1)
    await click(retryButtons()[0]); await flushSearch()
    assert.equal(requests.length, 2)
    await settle(requests[1], [file('recovered')])
    assert.ok(row('file', 'recovered'))
    assert.equal(document.querySelector('[role="alert"]'), null)
  })

  await test('session pending prevents concurrent selections, failure stays open and explicit retry succeeds', async () => {
    await mount(); await open(); await flushSearch()
    const first = deferred()
    sessionHandler = () => first.promise
    await click(row('session', 'other'))
    assert.equal(menu().getAttribute('aria-busy'), 'true')
    assert.ok([...menu().querySelectorAll('[role="menuitem"]')].every(node => node.disabled))
    assert.equal(search().disabled, true)
    assert.ok(menu().textContent.includes(t('contextLoading')))
    await click(row('upload')); await click(row('file', 'README.md')); await click(row('command', 'plan'))
    await click(row('session', 'other')); await key(menu(), 'Enter')
    assert.equal(calls.sessions.length, 1)
    assert.equal(calls.upload, 0)
    assert.deepEqual(calls.files, [])
    assert.deepEqual(calls.commands, [])
    await act(async () => first.reject(new Error('Session could not be read')))
    assert.ok(menu())
    assert.ok(menu().textContent.includes('Session could not be read'))
    assert.equal(menu().getAttribute('aria-busy'), 'false')
    assert.equal(search().disabled, false)
    sessionHandler = async () => { editor.focus() }
    await click(retryButtons()[0])
    assert.equal(calls.sessions.length, 2)
    assert.equal(menu(), null)
    assert.equal(document.activeElement, editor)
  })

  await test('closing a pending session aborts injection; an old rejection cannot damage the reopened menu', async () => {
    await mount(); await open()
    const work = deferred()
    let injected = 0
    sessionHandler = async (_summary, signal) => { await work.promise; if (!signal.aborted) injected++ }
    await click(row('session', 'other'))
    const signal = calls.sessions[0].signal
    await key(menu(), 'Escape')
    assert.equal(signal.aborted, true)
    assert.equal(menu(), null)
    assert.equal(document.activeElement, trigger())
    await open()
    await act(async () => work.resolve())
    assert.equal(injected, 0)
    assert.ok(menu())
    assert.equal(menu().getAttribute('aria-busy'), 'false')
    const failed = deferred()
    sessionHandler = () => failed.promise
    await click(row('session', 'other')); await click(closeButton()); await open()
    await act(async () => failed.reject(new Error('late failure')))
    assert.equal(document.querySelector('[role="alert"]'), null)
    assert.ok(menu())
  })

  await test('disabled/workspace changes and unmount abort outstanding file and session work', async () => {
    respondAutomatically = false
    await mount(); await open(); await flushSearch()
    const pending = deferred()
    sessionHandler = () => pending.promise
    await click(row('session', 'other'))
    await render({ disabled: true })
    assert.equal(menu(), null)
    assert.equal(trigger().disabled, true)
    assert.equal(requests[0].signal.aborted, true)
    assert.equal(calls.sessions[0].signal.aborted, true)
    await click(trigger())
    assert.equal(calls.open, 1)
    await act(async () => pending.resolve())
    await render({ disabled: false }); await open(); await flushSearch()
    const work = deferred()
    sessionHandler = () => work.promise
    await click(row('session', 'other'))
    await render({ workspacePath: 'D:/new' })
    assert.equal(menu(), null)
    assert.equal(requests[1].signal.aborted, true)
    assert.equal(calls.sessions[1].signal.aborted, true)
    await act(async () => work.resolve())
    await open(); await flushSearch()
    assert.equal(requests[2].path, 'D:/new')
    await settle(requests[1], [file('wrong-workspace')])
    assert.equal(fileRows().length, 0)
    const last = requests[2]
    await act(async () => root.unmount())
    root = null
    assert.equal(last.signal.aborted, true)
    assert.equal(menu(), null)
  })

  await test('unmount also cancels the debounce and pending session signal', async () => {
    await mount(); await open()
    const work = deferred()
    sessionHandler = () => work.promise
    await click(row('session', 'other'))
    const signal = calls.sessions[0].signal
    await act(async () => root.unmount()); root = null
    assert.equal(signal.aborted, true)
    await act(async () => work.resolve())
    await flushSearch()
    assert.equal(requests.length, 0)
  })

  await test('dead/no workspace never fetches files; dead workspace still lists its sessions and cancels earlier search', async () => {
    respondAutomatically = false
    await mount({ filesAvailable: false }); await open(); await flushSearch()
    assert.equal(requests.length, 0)
    assert.ok(menu().textContent.includes(t('contextWorkspaceUnavailable')))
    assert.deepEqual(sessionRows().map(node => node.dataset.contextSession), ['other'])
    await render({ filesAvailable: true }); await flushSearch()
    assert.equal(requests.length, 1)
    await render({ filesAvailable: false })
    assert.equal(requests[0].signal.aborted, true)
    await settle(requests[0], [file('not-available')])
    assert.equal(fileRows().length, 0)
    assert.ok(menu())
    await click(row('session', 'other'))
    assert.equal(calls.sessions.length, 1)
    await render({ workspacePath: null }); await open(); await flushSearch()
    assert.equal(requests.length, 1)
    assert.equal(sessionRows().length, 0)
    assert.ok(row('upload'))
    assert.ok(row('command', 'plan'))
  })

  await test('keyboard navigation/scroll/Enter/Escape, native Tab targets, and IME bypass never send composer messages', async () => {
    await mount(); await open(); await flushSearch()
    assert.equal(document.activeElement, search())
    const down = await key(search(), 'ArrowDown')
    assert.equal(down.defaultPrevented, true)
    assert.equal(document.querySelector('[data-kb="true"]').dataset.contextCommand, 'plan')
    assert.equal(search().getAttribute('aria-activedescendant'), row('command', 'plan').id)
    assert.ok(scrolled.some(entry => entry.key === row('command', 'plan').id && entry.options.block === 'nearest'))
    await key(search(), 'End')
    assert.equal(document.querySelector('[data-kb="true"]').dataset.contextSession, 'other')
    await key(search(), 'ArrowUp')
    assert.equal(document.querySelector('[data-kb="true"]').dataset.contextPath, 'README.md')
    await key(search(), 'Home')
    assert.equal(document.querySelector('[data-kb="true"]').dataset.contextItem, 'upload')
    const before = document.querySelector('[data-kb="true"]').id
    const imeDown = await key(search(), 'ArrowDown', { isComposing: true })
    assert.equal(imeDown.defaultPrevented, false)
    assert.equal(document.querySelector('[data-kb="true"]').id, before)
    const imeEnter = await key(search(), 'Enter', { isComposing: true })
    assert.equal(imeEnter.defaultPrevented, false)
    await key(search(), 'Enter', { keyCode: 229 })
    await act(async () => search().dispatchEvent(new g.CompositionEvent('compositionstart', { bubbles: true })))
    await key(search(), 'Enter'); await key(search(), 'Escape')
    assert.ok(menu())
    assert.equal(calls.upload, 0)
    await act(async () => search().dispatchEvent(new g.CompositionEvent('compositionend', { bubbles: true })))
    const tab = await key(search(), 'Tab')
    assert.equal(tab.defaultPrevented, false)
    assert.ok([...menu().querySelectorAll('[role="menuitem"]')].every(node => node.tabIndex === 0))
    await act(async () => row('file', 'src').focus())
    await key(document.activeElement, 'ArrowDown')
    assert.equal(document.activeElement, row('file', 'README.md'))
    await key(search(), 'ArrowDown')
    assert.equal(document.querySelector('[data-kb="true"]').dataset.contextSession, 'other')
    const enter = await key(search(), 'Enter')
    assert.equal(enter.defaultPrevented, true)
    assert.equal(calls.sessions.length, 1)
    assert.equal(calls.sent, 0)
    assert.equal(menu(), null)
    assert.equal(document.activeElement, editor)
    await open()
    const esc = await key(search(), 'Escape')
    assert.equal(esc.defaultPrevented, true)
    assert.equal(menu(), null)
    assert.equal(document.activeElement, trigger())
  })

  await test('dynamic candidates keep stable selected IDs and clamp removed rows; Escape works after native Tab leaves', async () => {
    await mount({ sessions: [session('s1'), session('s2'), session('s3')] })
    await open(); await flushSearch()
    await key(search(), 'End')
    const selected = row('session', 's3').id
    await render({ sessions: [session('new'), ...props.sessions] })
    assert.equal(document.querySelector('[data-kb="true"]').id, selected, 'insertion keeps the same selected session')
    await render({ sessions: [session('s1'), session('s2')] })
    assert.equal(document.querySelector('[data-kb="true"]').dataset.contextSession, 's2', 'removed selection clamps to last valid row')
    await act(async () => document.getElementById('outside').focus())
    await key(document.activeElement, 'Escape', { isComposing: true })
    assert.ok(menu(), 'IME Escape cannot reach PanePopover dismissal')
    await key(document.activeElement, 'Escape')
    assert.equal(menu(), null)
    assert.equal(document.activeElement, trigger())
  })

  await test('outside pointer dismisses without stealing destination focus or accepting late files', async () => {
    respondAutomatically = false
    await mount(); await open(); await flushSearch()
    const outside = document.getElementById('outside')
    await act(async () => {
      outside.focus()
      outside.dispatchEvent(new g.Event('pointerdown', { bubbles: true }))
    })
    assert.equal(menu(), null)
    assert.equal(requests[0].signal.aborted, true)
    assert.equal(document.activeElement, outside)
    await settle(requests[0], [file('late')])
    assert.equal(menu(), null)
    assert.deepEqual(calls.files, [])
  })
  console.log('Composer context menu checks passed (16 scenarios).')
} finally {
  await unmount()
  dom.window.close()
}
