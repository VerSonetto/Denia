/**
 * SessionsPage 添加上下文的接线回归（真实页面 + jsdom/esbuild）。
 *   node web/scripts/check-composer-context-integration.mjs
 *
 * 保留真实 ComposerContextMenu/PanePopover、editor、useComposerSlash、
 * useComposerMentions、composerContext、sessionState 以及 QueuedMessagePanel。
 * API/store/SSE 和与输入无关的组件/布局 hooks 用 esbuild 插件隔离。
 * 所有交互经过 DOM 和页面 handler；不复制或替换插入/发送/队列逻辑。
 * bundle 只驻内存，不生成 entry、产物或临时文件。jsdom 不验证 CSS 布局、
 * 原生文件对话框、Chromium execCommand/undo；execCommand 用真实 Range 模拟。
 */
import assert from 'node:assert/strict'
import { createRequire, Module } from 'node:module'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const web = fileURLToPath(new URL('..', import.meta.url))
const src = join(web, 'src')
const bridgeKey = '__deniaComposerContextIntegration'
const workspace = { id: 'ws-test', title: 'Integration workspace', path: 'D:/mock-workspace', created_at: 1 }
const active = { id: 'active-test', title: 'Current conversation', cwd: workspace.path, cwd_alive: true, excerpt: 'Existing dialogue', created_at: 1 }
const source = { id: 'source-test', title: 'Reference conversation', cwd: 'd:\\MOCK-WORKSPACE\\', cwd_alive: true, excerpt: 'Not the quoted body', created_at: 2 }
const sourceTwo = { ...source, id: 'source-two', title: 'Second reference' }
const envelope = (seq, data) => ({ seq, time: seq, ...data })
const recentEvents = [
  envelope(281, { type: 'system-prompt', text: 'PRIVATE_SYSTEM', turn: 1, step: 1 }),
  envelope(282, { type: 'user-message', text: 'Recent user question' }),
  envelope(283, { type: 'user-message', injected: true, text: 'PRIVATE_INJECTED' }),
  envelope(284, { type: 'assistant-message', turn: 1, step: 1, blocks: [
    { type: 'reasoning', text: 'PRIVATE_REASONING' },
    { type: 'text', text: 'Recent public answer' },
    { type: 'tool-call', name: 'bash', arguments: 'PRIVATE_TOOL_ARGUMENTS', id: 'tool-test' },
  ] }),
  envelope(285, { type: 'tool-result', call_id: 'tool-test', content: 'PRIVATE_TOOL_RESULT' }),
  envelope(286, { type: 'compaction-summary', summary: 'Recent public summary' }),
  envelope(287, { type: 'request-header', header: { config: { provider: 'mock-provider', model: 'mock-model' }, system: 'PRIVATE_REQUEST' } }),
]

const bridge = {
  calls: [], notifications: [], networkAttempts: [], execCalls: [], pickerClicks: [], queue: [],
  listeners: new Set(),
  snapshot: { sessions: [], workspaces: [workspace], activeWs: workspace, activeId: null, running: false },
  postFailures: 0,
  pages: new Map(),
  record(name, args) { this.calls.push({ name, args }) },
  publish(next) {
    this.snapshot = { ...this.snapshot, ...next }
    for (const listener of this.listeners) listener()
  },
  reset(overrides) {
    this.calls = []; this.notifications = []; this.execCalls = []; this.pickerClicks = []; this.queue = []
    this.postFailures = 0
    this.pages = new Map([[source.id, { events: recentEvents, hasMoreBefore: true }],
      [sourceTwo.id, { events: [envelope(401, { type: 'user-message', text: 'Independent second question' })], hasMoreBefore: false }]])
    this.snapshot = { sessions: [active, source, sourceTwo], workspaces: [workspace], activeWs: workspace,
      activeId: null, running: false, ...overrides }
  },
}
globalThis[bridgeKey] = bridge
const bridgePrelude = `const b = globalThis[${JSON.stringify(bridgeKey)}];`
const unexpectedApi = ['answerApproval', 'answerAsk', 'cancelAsk', 'cancelSession', 'compactSession',
  'forkSession', 'getCheckpointDiff', 'getCheckpoints', 'getSession', 'goalAction', 'rewindSession',
  'setSessionAgentPreset', 'setSessionPermission', 'chatCompletion']
const stubs = new Map([
  ['api', `${bridgePrelude}
    export async function getCatalog() {
      b.record('getCatalog', []);
      return { groups: [{ id: 'mock-provider', label: 'Mock', models: [{ id: 'mock-model', name: 'Mock', inputModalities: ['text', 'image'] }] }] };
    }
    export async function getSettings() { b.record('getSettings', []); return { namespaces: [] }; }
    export async function listAgentPresets() { b.record('listAgentPresets', []); return { modeSelection: false, default: null, presets: [] }; }
    export function subscribeEvents(...args) { b.record('subscribeEvents', args); return () => {}; }
    export async function listSkills(...args) { b.record('listSkills', args); return { skills: [] }; }
    export async function getGoal(...args) { b.record('getGoal', args); return { goal: null, tokensUsed: 0, maxRounds: 10 }; }
    export async function getTask(...args) { b.record('getTask', args); return { task: null }; }
    export async function searchMentions(...args) {
      b.record('searchMentions', args);
      if (args[0] !== 'D:/mock-workspace' || !args[2] || args[2].aborted) throw new Error('Unexpected mention scope/signal');
      return { items: [{ path: 'src/context file.ts', kind: 'file' }] };
    }
    export async function getSessionPage(...args) {
      b.record('getSessionPage', args);
      const page = b.pages.get(args[0]);
      if (!page) throw new Error('Unexpected session page: ' + args[0]);
      return page;
    }
    export async function postPrompt(...args) {
      b.record('postPrompt', args);
      if (b.postFailures > 0) { b.postFailures--; throw new Error('MOCK_POST_FAILURE'); }
      return { ok: true };
    }
    export async function uploadAttachment(...args) {
      b.record('uploadAttachment', args);
      return { path: 'mock-upload/' + args[0].name };
    }
    ${unexpectedApi.map(name => `export function ${name}(...args) { b.record('${name}', args); throw new Error('Unexpected API: ${name}'); }`).join('\n')}
  `],
  ['appStore', `import { useSyncExternalStore } from 'react'; ${bridgePrelude}
    const subscribe = listener => { b.listeners.add(listener); return () => b.listeners.delete(listener); };
    const snapshot = () => b.snapshot;
    const useStore = () => useSyncExternalStore(subscribe, snapshot, snapshot);
    export const useSessions = () => useStore().sessions;
    export const useWorkspaces = () => useStore().workspaces;
    export const useCatalogTick = () => 0;
    export const useCompactingFor = () => null;
    export const useRunningFor = id => { const s = useStore(); return !!id && s.running; };
    export const getActiveWorkspace = () => b.snapshot.activeWs;
    export async function ensureSession(ws) { b.record('ensureSession', [ws]); return b.snapshot.activeId ?? (ws ? 'mock-new-session' : null); }
    export function notify(...args) { b.notifications.push(args); }
    export function setRunningStatus(id, running) { b.record('setRunningStatus', [id, running]); b.publish({ running }); }
    export function setActiveId(...args) { b.record('setActiveId', args); }
    export function markStarted(...args) { b.record('markStarted', args); }
    export function markCompacting(...args) { b.record('markCompacting', args); }
    export function refreshList(...args) { b.record('refreshList', args); }
    export function addSessionLocal(...args) { b.record('addSessionLocal', args); }
  `],
  ['sessionStreams', `${bridgePrelude}
    export function attach(...args) { b.record('attach', args); return () => {}; }
    export function ensureFollowing(...args) { b.record('ensureFollowing', args); }
    export function invalidateSession(...args) { b.record('invalidateSession', args); }
  `],
  ['features/conversation/useSessionContext', 'export const useSessionContext = () => ({ context: null });'],
  ['hooks/useStickToBottom', `import { useMemo } from 'react';
    export function useStickToBottom(ref) { return useMemo(() => ({ stick: true, snapToBottom() {}, release() {}, reset() {}, follow() {}, setNode(node) { ref.current = node; } }), [ref]); }
  `],
  ['hooks/useContainedWheel', "import { useRef } from 'react'; export const useContainedWheel = () => useRef(null);"],
  ['components/PermissionSelector', `export const PermissionSelector = () => null;
    export const loadPermission = () => 'auto-edit'; export const hasStoredPermission = () => false;`],
  // Read-only observer: render the REAL queue panel and record exactly what the page passed it.
  ['components/QueuedMessagePanel', `import React from 'react';
    import { QueuedMessagePanel as RealQueue } from ${JSON.stringify(join(src, 'components/QueuedMessagePanel.tsx').replace(/\\/g, '/'))};
    ${bridgePrelude}
    export function QueuedMessagePanel(props) { b.queue = props.messages; return React.createElement(RealQueue, props); }
  `],
])
for (const name of ['RuntimePanel', 'SessionView', 'AgentPresetSelector', 'SessionPresetLabel', 'OpenInApp',
  'StatsBar', 'ComposerModelMenu', 'ComposerEffortControl', 'PlanReviewPanel', 'ApprovalDialog',
  'ConfirmDialog', 'ContextRing', 'TodoPanel', 'GoalBar', 'ConversationAxis']) {
  stubs.set(`components/${name}`, `export const ${name} = () => null;`)
}

const result = await build({
  // esbuild's metafile paths are relative to this explicit directory, never caller cwd.
  absWorkingDir: web,
  stdin: { contents: 'export { default as SessionsPage } from "./pages/SessionsPage"; export * as editorApi from "./pages/editor"; export { t, setLocale } from "./i18n";',
    resolveDir: src, sourcefile: 'composer-context-integration-entry.tsx', loader: 'tsx' },
  bundle: true, write: false, format: 'cjs', platform: 'node', target: 'node20', jsx: 'automatic',
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  loader: { '.css': 'empty' }, logLevel: 'silent', metafile: true,
  plugins: [{ name: 'composer-integration-stubs', setup(plugin) {
    plugin.onResolve({ filter: /^\./ }, args => {
      const resolved = join(args.resolveDir, args.path).replace(/\\/g, '/').replace(/\.(tsx?|jsx?)$/, '')
      const prefix = src.replace(/\\/g, '/') + '/'
      const key = resolved.startsWith(prefix) ? resolved.slice(prefix.length) : ''
      return stubs.has(key) ? { path: key, namespace: 'integration-stub' } : undefined
    })
    plugin.onLoad({ filter: /.*/, namespace: 'integration-stub' }, args => ({ contents: stubs.get(args.path), loader: 'tsx', resolveDir: src }))
  } }],
})
// Guard the test boundary: a future import cannot silently bundle a real transport/store.
const inputs = new Set(Object.keys(result.metafile.inputs)
  .filter(path => !path.startsWith('integration-stub:') && path !== '<stdin>')
  .map(path => resolve(web, path).replace(/\\/g, '/')))
const realInput = path => inputs.has(join(src, path).replace(/\\/g, '/'))
for (const path of ['api.ts', 'appStore.ts', 'sessionStreams.ts']) {
  assert.ok(!realInput(path), `Real ${path} must not be bundled`)
}
for (const path of ['pages/SessionsPage.tsx', 'components/ComposerContextMenu.tsx', 'components/PanePopover.tsx',
  'pages/editor.ts', 'features/conversation/useComposerSlash.ts', 'features/conversation/useComposerMentions.ts',
  'features/conversation/composerContext.ts', 'features/conversation/sessionState.ts', 'components/QueuedMessagePanel.tsx']) {
  assert.ok(realInput(path), `Must retain real ${path}`)
}

const dom = new JSDOM('<!doctype html><body><div id="root"></div></body>', {
  url: 'http://localhost/', pretendToBeVisual: true,
})
const g = dom.window
const previousGlobals = new Map()
function install(name, value) {
  if (!previousGlobals.has(name)) previousGlobals.set(name, Object.getOwnPropertyDescriptor(globalThis, name))
  Object.defineProperty(globalThis, name, { value, configurable: true, writable: true })
}
for (const name of ['window', 'document', 'navigator', 'location', 'history', 'HTMLElement', 'HTMLInputElement',
  'Element', 'Node', 'Event', 'MouseEvent', 'KeyboardEvent', 'MutationObserver', 'getComputedStyle',
  'requestAnimationFrame', 'cancelAnimationFrame', 'localStorage', 'sessionStorage', 'File', 'FileReader', 'Blob']) install(name, g[name])
install('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
install('IS_REACT_ACT_ENVIRONMENT', true)
g.HTMLElement.prototype.scrollIntoView = function () {}
const blockNetwork = kind => (...args) => {
  bridge.networkAttempts.push({ kind, args })
  throw new Error(`External transport forbidden: ${kind}`)
}
install('fetch', blockNetwork('fetch'))
g.fetch = globalThis.fetch
for (const name of ['XMLHttpRequest', 'WebSocket', 'EventSource']) {
  const blocked = blockNetwork(name)
  install(name, blocked)
  g[name] = blocked
}
const uncaught = []
g.addEventListener('error', event => { uncaught.push(event.error ?? new Error(event.message)); event.preventDefault() })
const consoleErrors = []
const originalConsoleError = console.error
console.error = (...args) => { consoleErrors.push(args.map(arg => arg?.stack ?? String(arg)).join(' ')) }

const React = await import('react')
const { act } = React
const { createRoot } = await import('react-dom/client')
// Resolve external React from web/node_modules without writing a bundle to disk.
const filename = join(web, 'node_modules', '.composer-context-integration.cjs')
const bundled = new Module(filename)
bundled.filename = filename
bundled.paths = Module._nodeModulePaths(join(web, 'node_modules'))
bundled.require = createRequire(filename)
bundled._compile(result.outputFiles[0].text, filename)
const { SessionsPage, editorApi, t, setLocale } = bundled.exports
setLocale('en')
const H = React.createElement
const calls = name => bridge.calls.filter(call => call.name === name)
const editor = () => document.querySelector('.prompt-editor')
const trigger = () => document.querySelector('.composer-context-trigger')
const menu = () => document.querySelector('.composer-context-menu')
const search = () => document.querySelector('.composer-context-search input')
const commandItem = name => document.querySelector(`[data-context-command="${name}"]`)
const chip = () => document.querySelector('.traj-quote-chip')
const sendButton = () => document.querySelector('.btn-send:not(.stop)')
const row = (kind, attr, value) => [...document.querySelectorAll(`[data-context-item="${kind}"]`)]
  .find(node => attr === undefined || node.getAttribute(attr) === value)
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms))
const settle = (ms = 0) => act(async () => { await sleep(ms) })
async function waitFor(predicate, message, timeout = 2000) {
  const deadline = Date.now() + timeout
  while (!predicate() && Date.now() < deadline) await settle(20)
  assert.ok(predicate(), message)
}
async function click(node) {
  assert.ok(node, 'DOM click target must exist')
  assert.equal(node.disabled, false, 'DOM click target must be enabled')
  await act(async () => {
    // Match the real menu trigger's preventDefault behavior before click steals focus.
    const down = new g.MouseEvent('mousedown', { bubbles: true, cancelable: true })
    node.dispatchEvent(down)
    if (!down.defaultPrevented) node.focus()
    node.click()
  })
}
const openMenu = async () => { await click(trigger()); assert.ok(menu(), 'Page + opens the context menu') }
const closeMenu = () => click(document.querySelector(`[aria-label="${t('contextClose')}"]`))
async function setDraft(text, start = text.length, end = start) {
  await act(async () => {
    editor().focus()
    editor().textContent = text
    editorApi.selectRange(editor(), start, end)
    editor().dispatchEvent(new g.Event('input', { bubbles: true }))
  })
}
async function chooseSession(id = source.id) {
  await openMenu()
  await click(row('session', 'data-context-session', id))
  await waitFor(() => chip() && !menu(), 'Session handler must create a quote chip and close the menu')
}
async function attachFile(name = 'context.txt', text = 'Attachment fixture') {
  const input = document.querySelector('input[type="file"][hidden]')
  const file = new g.File([text], name, { type: 'text/plain' })
  await act(async () => {
    Object.defineProperty(input, 'files', { value: [file], configurable: true })
    input.dispatchEvent(new g.Event('change', { bubbles: true }))
  })
  return file
}
// Never replace the draft wholesale here: DOM Range insertion must preserve existing chip identity.
g.document.execCommand = (command, _ui, value) => {
  assert.ok(['insertText', 'insertHTML', 'delete'].includes(command), `Unsupported execCommand ${command}`)
  const selection = g.getSelection()
  assert.ok(selection?.rangeCount, 'execCommand requires a selection')
  const range = selection.getRangeAt(0)
  assert.ok(editor()?.contains(range.startContainer) && editor()?.contains(range.endContainer), 'execCommand selection must be in the real editor')
  bridge.execCalls.push({ command, value, selection: editorApi.selectionOffsetsIn(editor()) })
  range.deleteContents()
  if (command !== 'delete') {
    const content = command === 'insertText' ? document.createTextNode(String(value)) : range.createContextualFragment(String(value))
    const tail = command === 'insertText' ? content : content.lastChild
    range.insertNode(content)
    if (tail) range.setStartAfter(tail)
  }
  range.collapse(true)
  selection.removeAllRanges()
  selection.addRange(range)
  return true
}
const nativeInputClick = g.HTMLInputElement.prototype.click
g.HTMLInputElement.prototype.click = function () {
  if (this.type === 'file') {
    bridge.pickerClicks.push({ hidden: this.hidden, menuWasOpen: !!menu(), inUploadGesture })
    return // jsdom cannot open an OS picker.
  }
  return nativeInputClick.call(this)
}
let inUploadGesture = false
let root
let pickerRequests = 0
let selectedWorkspaces = []
function Harness() {
  const snapshot = React.useSyncExternalStore(listener => {
    bridge.listeners.add(listener)
    return () => bridge.listeners.delete(listener)
  }, () => bridge.snapshot)
  return H(SessionsPage, {
    activeId: snapshot.activeId, locked: false, hasStarted: Boolean(snapshot.activeId),
    onSelectWorkspace: ws => { selectedWorkspaces.push(ws); bridge.publish({ activeWs: ws }) },
    onOpenPicker: () => { pickerRequests++ }, onAddWorkspace: () => { throw new Error('Unexpected workspace creation') },
  })
}
async function mount(overrides = {}) {
  bridge.reset(overrides)
  pickerRequests = 0; selectedWorkspaces = []
  window.localStorage.clear(); window.sessionStorage.clear()
  root = createRoot(document.getElementById('root'))
  await act(async () => root.render(H(Harness)))
  await waitFor(() => calls('getCatalog').length && editor(), 'SessionsPage must mount')
}
async function unmount() {
  if (root) await act(async () => root.unmount())
  root = null
  assert.equal(document.querySelector('.composer-context-popover'), null, 'Unmount removes the real portal')
  assert.equal(bridge.listeners.size, 0, 'Store subscriptions cleaned up')
}
const expectedTitle = summary => `Session reference: ${summary.title}`
function expectedQuote(summary = source) {
  const metadata = `${expectedTitle(summary)}\nSession: ${summary.id}\nWorkspace: ${summary.cwd}\n\n`
  return {
    id: `session:${summary.id}`, title: expectedTitle(summary),
    text: metadata + (summary.id === source.id
      ? 'Only recent conversation is included; earlier content was omitted.\n\n' +
        '### User · seq=282\nRecent user question\n\n### Assistant · seq=284\nRecent public answer\n\n### Conversation summary · seq=286\nRecent public summary'
      : '### User · seq=401\nIndependent second question'),
  }
}
function assertPostedQuote(call, summary = source, targetId = 'mock-new-session') {
  assert.equal(call.args[0], targetId, 'Prompt target is the mocked ensured session, not the source')
  const { id: _id, ...wireQuote } = expectedQuote(summary)
  assert.deepEqual(call.args[1].quoted, [wireQuote], 'Wire quote preserves source title/text/embedded id; wire has no id field')
  assert.doesNotMatch(call.args[1].quoted[0].text, /PRIVATE_|Not the quoted body/)
  assert.equal(call.args[1].provider, 'mock-provider')
  assert.equal(call.args[1].model, 'mock-model')
}
let passed = 0
let failed = 0
async function test(label, run) {
  const errorStart = consoleErrors.length
  const uncaughtStart = uncaught.length
  try {
    await run()
    assert.deepEqual(bridge.networkAttempts, [], 'No real fetch/XHR/WebSocket/EventSource attempt')
    assert.deepEqual(bridge.calls.filter(call => unexpectedApi.includes(call.name)), [], 'No unexpected mocked API side effects (including swallowed errors)')
    assert.deepEqual(uncaught.slice(uncaughtStart), [], 'No unhandled jsdom/React errors')
    assert.deepEqual(consoleErrors.slice(errorStart), [], 'No React errors or act warnings')
    passed++
    console.log(`ok   ${label}`)
  } catch (error) {
    failed++
    console.log(`FAIL ${label}\n${error.stack ?? error}`)
    if (consoleErrors.length > errorStart) console.log(consoleErrors.slice(errorStart).join('\n'))
  } finally {
    try { await unmount() } catch (error) { failed++; console.log(`FAIL cleanup\n${error.stack ?? error}`) }
  }
}

try {
  await test('1 页面 + 只开菜单；只有上传项同步点击 hidden file input', async () => {
    await mount()
    assert.equal(calls('searchMentions').length, 0, 'Closed menu does not query files')
    await openMenu()
    assert.equal(bridge.pickerClicks.length, 0)
    assert.equal(trigger().getAttribute('aria-expanded'), 'true')
    assert.equal(document.querySelector('.composer-context-popover').parentElement, document.body)
    assert.equal(document.querySelector('.composer-context-popover').dataset.positioned, 'true')
    assert.equal(document.activeElement, search())
    await closeMenu()
    assert.equal(bridge.pickerClicks.length, 0)
    await openMenu()
    await act(async () => {
      inUploadGesture = true
      try { row('upload').click() } finally { inUploadGesture = false }
    })
    assert.deepEqual(bridge.pickerClicks, [{ hidden: true, menuWasOpen: true, inUploadGesture: true }])
    assert.equal(menu(), null)
    assert.equal(calls('ensureSession').length, 0)
  })

  for (const placement of [
    { label: '前', start: 0, end: 0, expected: '@"src/context file.ts" /plan alpha omega', caret: 23 },
    { label: '中', start: 8, end: 8, expected: '/plan al @"src/context file.ts" pha omega', caret: 32 },
    { label: '后', start: 17, end: 17, expected: '/plan alpha omega @"src/context file.ts" ', caret: 41 },
    { label: '选区替换', start: 6, end: 11, expected: '/plan @"src/context file.ts" omega', caret: 28 },
  ]) {
    await test(`2 文件引用在保存选区${placement.label}插入，保留已有 /plan chip 原节点`, async () => {
      await mount({ activeId: active.id })
      // Establish the existing chip through the real slash hook and page's slash candidate UI.
      await setDraft('/pl')
      const planOption = [...document.querySelectorAll('.mention-menu-item')]
        .find(node => node.querySelector('.mention-menu-name')?.textContent === '/plan')
      assert.ok(planOption, 'Real slash hook supplies /plan')
      await act(async () => {
        planOption.dispatchEvent(new g.MouseEvent('mousedown', { bubbles: true, cancelable: true }))
        planOption.click()
      })
      const planChip = editor().querySelector('[data-slash="plan"]')
      assert.ok(planChip)
      await act(async () => {
        editorApi.setCaretOffset(editor(), editorApi.serializeEditor(editor()).length)
        document.execCommand('insertText', false, 'alpha omega')
        editor().dispatchEvent(new g.Event('input', { bubbles: true }))
        editorApi.selectRange(editor(), placement.start, placement.end)
      })
      assert.equal(editorApi.serializeEditor(editor()), '/plan alpha omega')
      await openMenu()
      assert.equal(document.activeElement, search())
      // Explicitly destroy the browser selection after onOpen. The page must use its snapshot.
      await act(async () => {
        const range = document.createRange()
        range.selectNodeContents(search())
        window.getSelection().removeAllRanges(); window.getSelection().addRange(range)
      })
      await waitFor(() => row('file'), 'Debounced real menu must expose a mocked file')
      await click(row('file'))
      assert.equal(editorApi.serializeEditor(editor()), placement.expected)
      assert.equal(editor().querySelector('[data-slash="plan"]'), planChip, 'Do not rebuild or flatten the existing slash-chip')
      assert.equal(planChip.getAttribute('contenteditable'), 'false')
      assert.deepEqual(bridge.execCalls.at(-1).selection, { start: placement.start, end: placement.end })
      assert.deepEqual(editorApi.selectionOffsetsIn(editor()), { start: placement.caret, end: placement.caret })
      assert.equal(document.activeElement, editor())
      assert.equal(bridge.pickerClicks.length, 0)
      assert.equal(calls('postPrompt').length, 0)
    })
  }

  await test('3 新会话实际选择工作区后文件搜索/插入可用，无需创建会话', async () => {
    await mount({ activeWs: null })
    assert.equal(trigger().disabled, true)
    assert.equal(editor().getAttribute('contenteditable'), 'false')
    assert.equal(calls('searchMentions').length, 0)
    await click(document.querySelector('.ws-chip'))
    await click(document.querySelector('.ws-menu-item:not(.create)'))
    assert.deepEqual(selectedWorkspaces, [workspace])
    assert.equal(trigger().disabled, false)
    assert.equal(editor().getAttribute('contenteditable'), 'true')
    await openMenu()
    await waitFor(() => row('file'), 'New draft files must work after choosing a workspace')
    assert.deepEqual(calls('searchMentions').map(call => call.args.slice(0, 2)), [[workspace.path, '']])
    await click(row('file'))
    assert.equal(editorApi.serializeEditor(editor()), '@"src/context file.ts" ')
    assert.equal(calls('ensureSession').length, 0)
    assert.equal(pickerRequests, 0)
  })

  await test('4 最近 events 生成会话 chip，来源信息完整，隔离 system/reasoning/tool/injected', async () => {
    await mount({ activeId: active.id })
    await chooseSession()
    const request = calls('getSessionPage')[0]
    assert.equal(request.args[0], source.id)
    assert.deepEqual(request.args[1], { limit: 300 }, 'No before/after cursor: request the latest event page')
    assert.ok(request.args[2] instanceof AbortSignal)
    assert.equal(chip().querySelector('.att-name').textContent, expectedTitle(source))
    assert.equal(document.activeElement, editor())
    assert.equal(editorApi.serializeEditor(editor()), '')
    assert.equal(sendButton().disabled, false)
    assert.equal(calls('postPrompt').length, 0, 'Selecting context never sends')
    // Dedup exercises the stable session quote id through the actual page state.
    await chooseSession()
    assert.equal(document.querySelectorAll('.traj-quote-chip').length, 1)
    await click(sendButton())
    assertPostedQuote(calls('postPrompt')[0], source, active.id)
    assert.equal(bridge.pickerClicks.length, 0)
  })

  await test('5 纯引用发送失败保留 chip，重试来源载荷不变；ensureSession 只返回 mock id', async () => {
    await mount()
    await chooseSession()
    bridge.postFailures = 1
    await click(sendButton())
    assert.equal(calls('postPrompt').length, 1)
    assertPostedQuote(calls('postPrompt')[0])
    assert.equal(calls('postPrompt')[0].args[1].prompt, expectedTitle(source), 'Quote-only message gets a readable fallback body')
    assert.deepEqual(calls('postPrompt')[0].args[1].images, [])
    assert.deepEqual(calls('postPrompt')[0].args[1].files, [])
    assert.ok(chip(), 'Failure must preserve the quote chip')
    assert.equal(editorApi.serializeEditor(editor()), '')
    assert.equal(sendButton().disabled, false)
    assert.ok(bridge.notifications.some(([kind, text]) => kind === 'err' && text === 'MOCK_POST_FAILURE'))
    assert.equal(calls('setRunningStatus').length, 0)
    await click(sendButton())
    assert.equal(calls('postPrompt').length, 2)
    assert.deepEqual(calls('postPrompt')[1].args, calls('postPrompt')[0].args)
    assert.deepEqual(calls('ensureSession').map(call => call.args), [[workspace], [workspace]])
    assert.equal(chip(), null)
  })

  await test('6 运行中引用+正文+附件入队，编辑回填及再次入队不丢 quote id/title/text', async () => {
    await mount({ activeId: active.id, running: true })
    await chooseSession()
    await setDraft('Queued with context')
    const file = await attachFile()
    await click(sendButton())
    assert.equal(calls('postPrompt').length, 0)
    assert.equal(calls('ensureSession').length, 0)
    assert.equal(chip(), null)
    assert.equal(editorApi.serializeEditor(editor()), '')
    assert.equal(document.querySelector('.queue-chip-name').textContent, expectedTitle(source))
    assert.deepEqual(bridge.queue[0].quotes, [expectedQuote()])
    assert.equal(bridge.queue[0].text, 'Queued with context')
    assert.equal(bridge.queue[0].attachments[0].file, file)
    await click(document.querySelector(`[aria-label="${t('queueEdit')}"]`))
    assert.equal(document.querySelector('.queue-panel'), null)
    assert.equal(chip().querySelector('.att-name').textContent, expectedTitle(source))
    assert.equal(editorApi.serializeEditor(editor()), 'Queued with context')
    assert.ok([...document.querySelectorAll('.composer-attachments .att-name')].some(node => node.textContent === file.name))
    await click(sendButton())
    assert.deepEqual(bridge.queue[0].quotes, [expectedQuote()])
    assert.equal(bridge.queue[0].attachments[0].file, file)
    await act(async () => bridge.publish({ running: false }))
    await waitFor(() => calls('postPrompt').length === 1, 'Finishing the run must flush the queue through page postMessage')
    assertPostedQuote(calls('postPrompt')[0], source, active.id)
    assert.equal(calls('postPrompt')[0].args[1].prompt, 'Queued with context')
    assert.deepEqual(calls('postPrompt')[0].args[1].files, ['mock-upload/context.txt'])
    assert.equal(calls('uploadAttachment')[0].args[0].data, Buffer.from('Attachment fixture').toString('base64'))
    assert.equal(document.querySelector('.queue-panel'), null)
  })

  await test('6 队列自动发送失败放回队首，编辑恢复旧引用，不误用现场新草稿/新引用', async () => {
    await mount({ activeId: active.id, running: true })
    await chooseSession()
    await click(sendButton())
    assert.deepEqual(bridge.queue[0].quotes, [expectedQuote()])
    assert.equal(bridge.queue[0].text, '')
    await chooseSession(sourceTwo.id)
    await setDraft('Unsent new draft')
    bridge.postFailures = 1
    await act(async () => bridge.publish({ running: false }))
    await waitFor(() => calls('postPrompt').length === 1 && bridge.queue.length === 1, 'Failed automatic send must restore its queue message')
    assertPostedQuote(calls('postPrompt')[0], source, active.id)
    assert.equal(calls('postPrompt')[0].args[1].prompt, expectedTitle(source))
    assert.deepEqual(bridge.queue[0].quotes, [expectedQuote()])
    assert.equal(chip().querySelector('.att-name').textContent, expectedTitle(sourceTwo))
    assert.equal(editorApi.serializeEditor(editor()), 'Unsent new draft')
    await click(document.querySelector(`[aria-label="${t('queueEdit')}"]`))
    assert.equal(editorApi.serializeEditor(editor()), '')
    assert.equal(chip().querySelector('.att-name').textContent, expectedTitle(source))
    await click(sendButton())
    assertPostedQuote(calls('postPrompt')[1], source, active.id)
    assert.deepEqual(calls('postPrompt')[1].args, calls('postPrompt')[0].args)
    assert.equal(chip(), null)
  })

  // Latest UI contract: modes stay available with existing text, regardless of session age.
  // They only edit the leading prefix; attachments/quotes suppress both to avoid consuming payload.
  async function selectMode(command, expectedDraft) {
    const sideEffects = ['ensureSession', 'postPrompt', 'goalAction', 'getGoal', 'compactSession',
      'setSessionPermission', 'setSessionAgentPreset', 'markStarted', 'setRunningStatus', 'cancelSession']
    // Existing sessions legitimately load getGoal on mount; selection must add no calls.
    const before = sideEffects.map(name => calls(name).length)
    const notificationsBefore = [...bridge.notifications]
    const permissionBefore = window.localStorage.getItem('denia.permission')
    await openMenu()
    for (const name of ['goal', 'plan']) {
      assert.ok(commandItem(name), `Draft without attachments must offer /${name}`)
      assert.equal(commandItem(name).querySelector('.composer-context-label').textContent,
        t(name === 'goal' ? 'contextGoal' : 'contextPlan'))
    }
    await click(commandItem(command))
    assert.equal(editorApi.serializeEditor(editor()), expectedDraft)
    const commandChip = editor().querySelector(`[data-slash="${command}"]`)
    assert.ok(commandChip, 'Page command handler must use the real slash draft renderer')
    assert.equal(commandChip, editor().firstChild, 'Mode prefix goes at the beginning, not the saved caret')
    assert.equal(commandChip.getAttribute('contenteditable'), 'false')
    assert.equal(commandChip.dataset.kind, 'command')
    const caret = expectedDraft.length
    assert.deepEqual(editorApi.selectionOffsetsIn(editor()), { start: caret, end: caret })
    assert.equal(document.activeElement, editor())
    assert.equal(menu(), null)
    assert.deepEqual(sideEffects.map(name => calls(name).length), before, `/${command} selection must not execute or send`)
    assert.deepEqual(bridge.notifications, notificationsBefore, 'Selection does not announce command execution')
    assert.equal(window.localStorage.getItem('denia.permission'), permissionBefore, '/plan selection does not immediately change permission')
    assert.equal(bridge.queue.length, 0, 'Running session command selection must not enqueue')
    assert.equal(bridge.pickerClicks.length, 0)
  }
  for (const context of [
    { label: 'new session', overrides: {} },
    { label: '已有会话', overrides: { activeId: active.id } },
    { label: '运行中会话', overrides: { activeId: active.id, running: true } },
  ]) {
    for (const body of ['', 'Keep existing draft\nSecond line']) {
      for (const command of ['goal', 'plan']) {
        await test(`7 ${context.label} ${body ? '已有正文' : '空草稿'} 提供 goal/plan；/${command} 插入前缀保留正文、不执行`, async () => {
          await mount(context.overrides)
          if (body) await setDraft(body, 5, 13) // An active selection must not replace part of the body.
          await selectMode(command, `/${command} ${body}`)
          await openMenu()
          for (const name of ['goal', 'plan']) assert.ok(commandItem(name), 'Mode entries remain available after a prefix is inserted')
        })
      }
    }
    await test(`7 ${context.label} goal/plan 切换替换 prefix，重复选择不叠加，保留多行正文`, async () => {
      await mount(context.overrides)
      const body = 'Preserve objective\n  Preserve indented details'
      await setDraft(body)
      await selectMode('goal', `/goal ${body}`)
      await selectMode('plan', `/plan ${body}`)
      assert.equal(editor().querySelector('[data-slash="goal"]'), null)
      await selectMode('plan', `/plan ${body}`)
      assert.equal(editor().querySelectorAll('[data-slash="plan"]').length, 1)
      await selectMode('goal', `/goal ${body}`)
      assert.equal(editor().querySelector('[data-slash="plan"]'), null)
      assert.equal(editor().querySelectorAll('[data-slash="goal"]').length, 1)
    })
  }

  for (const activeId of [null, active.id]) {
    for (const state of ['attachment', 'image', 'quote']) {
      await test(`7 ${activeId ? '已有会话' : 'new session'} ${state} 隐藏 goal/plan；移除后恢复入口`, async () => {
        await mount({ activeId })
        const body = 'Keep draft while attaching context'
        await setDraft(body)
        if (state === 'attachment') await attachFile()
        if (state === 'image') {
          await act(async () => {
            const image = new g.File(['mock image bytes'], 'pasted.png', { type: 'image/png' })
            const event = new g.Event('paste', { bubbles: true, cancelable: true })
            Object.defineProperty(event, 'clipboardData', { value: {
              items: [{ kind: 'file', type: image.type, getAsFile: () => image }],
            } })
            editor().dispatchEvent(event)
          })
          await waitFor(() => document.querySelector('.composer-image-thumb'), 'Pasted image must be loaded by the real page handler')
        }
        if (state === 'quote') await chooseSession()
        await openMenu()
        for (const name of ['goal', 'plan']) assert.equal(commandItem(name), null)
        assert.ok(row('upload'), 'Other menu items remain available')
        assert.equal(calls('postPrompt').length, 0)
        assert.equal(calls('goalAction').length, 0)
        assert.equal(calls('setSessionPermission').length, 0)
        await closeMenu()
        await click(document.querySelector(state === 'image' ? '.composer-image-remove' : '.composer-attachments .att-remove'))
        assert.equal(editorApi.serializeEditor(editor()), body)
        await openMenu()
        for (const name of ['goal', 'plan']) assert.ok(commandItem(name), 'Removing the last payload restores both modes')
      })
    }
  }

  assert.deepEqual(bridge.networkAttempts, [], 'All cases remained offline')
  console.log(`\n${passed} PASS, ${failed} FAIL`)
  console.log('Boundary: real SessionsPage/menu/editor/slash/mentions/quote/queue handlers; mocked API/store/SSE/peripheral UI/layout hooks.')
  console.log('Limitations: no CSS/real-browser geometry, OS file picker, native execCommand/undo, backend or real session creation; wire quoted contains title/text, source id is embedded in text.')
  process.exitCode = failed ? 1 : 0
} finally {
  if (root) await unmount()
  console.error = originalConsoleError
  dom.window.close()
  for (const [name, descriptor] of previousGlobals) {
    if (descriptor) Object.defineProperty(globalThis, name, descriptor)
    else delete globalThis[name]
  }
  delete globalThis[bridgeKey]
}
