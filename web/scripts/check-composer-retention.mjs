/**
 * 草稿、队列持久化和输入历史行为回归：真实 SessionsPage + jsdom。
 *   node web/scripts/check-composer-retention.mjs
 * 存储用 scope Map、历史用 API fixture；保留真实持久化/历史 hooks 和队列面板。
 * 通过真实 DOM 输入、按键、发送按钮与页面切换验证，不复制页面 handler。
 */
import assert from 'node:assert/strict'
import { createRequire, Module } from 'node:module'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const web = fileURLToPath(new URL('..', import.meta.url))
const src = join(web, 'src')
const bridgeKey = '__deniaComposerRetentionRegression'
const workspace = { id: 'ws-test', title: 'Workspace', path: 'D:/mock-workspace', created_at: 1 }
const session = { id: 'session-test', title: 'Conversation', cwd: workspace.path, cwd_alive: true, created_at: 1 }
const second = { ...session, id: 'session-second', title: 'Second conversation' }
const bridge = { calls: [], notifications: [], networkAttempts: [], memory: new Map(), running: false }
const history = [
  { seq: 1, time: 1, type: 'user-message', text: 'Older request' },
  { seq: 2, time: 2, type: 'user-message', text: 'Recent request' },
  { seq: 3, time: 3, type: 'user-message', text: 'Private injected', injected: true },
]
globalThis[bridgeKey] = bridge
const prelude = `const b = globalThis[${JSON.stringify(bridgeKey)}];`
const unexpectedApi = ['answerApproval', 'answerAsk', 'cancelAsk', 'cancelSession', 'compactSession',
  'forkSession', 'getCheckpointDiff', 'getCheckpoints', 'getSession', 'goalAction', 'rewindSession',
  'setSessionAgentPreset', 'setSessionPermission', 'chatCompletion', 'postPrompt', 'pickFiles']
const stubs = new Map([
  ['composerMemory', `${prelude}
    const empty = () => ({ text: '', images: [], attachments: [], quotes: [], queue: [] });
    export async function loadComposer(scope) { return structuredClone(b.memory.get(scope) ?? empty()); }
    export function readComposerText(scope) { return b.memory.get(scope)?.text ?? null; }
    export async function saveComposer(scope, snapshot) { b.memory.set(scope, structuredClone(snapshot)); }
  `],
  ['api', `${prelude}
    export async function getCatalog() { return { groups: [{ id: 'mock-provider', label: 'Mock', models: [{ id: 'mock-model', name: 'Mock', inputModalities: ['text'] }] }] }; }
    export async function getSettings() { return { namespaces: [] }; }
    export async function listAgentPresets() { return { modeSelection: false, default: null, presets: [] }; }
    export function subscribeEvents() { return () => {}; }
    export async function listSkills() { return { skills: [] }; }
    export async function getGoal() { return { goal: null, tokensUsed: 0, maxRounds: 10 }; }
    export async function pickerCapability() { return { kind: 'native', remote: false }; }
    export async function searchMentions() { return { items: [] }; }
    export async function getSessionPage(id) { b.calls.push(id); return { events: id === ${JSON.stringify(session.id)} ? ${JSON.stringify(history)} : [], hasMoreBefore: false }; }
    ${unexpectedApi.map(name => `export function ${name}() { throw new Error('Unexpected API: ${name}'); }`).join('\n')}
  `],
  ['promptOptimizer', `${prelude}
    export function optimizePromptText() { throw new Error('Unexpected optimization'); }
  `],
  ['appStore', `${prelude}
    const sessions = ${JSON.stringify([session, second])}; const workspaces = ${JSON.stringify([workspace])};
    export const useSessions = () => sessions;
    export const useWorkspaces = () => workspaces;
    export const getActiveWorkspace = () => workspaces[0];
    export const useCatalogTick = () => 0;
    export const useCompactingFor = () => null;
    export const useRunningFor = () => b.running;
    export function notify(...args) { b.notifications.push(args); }
    export async function ensureSession() { throw new Error('Unexpected session creation'); }
    export function setRunningStatus() {} export function setActiveId() {}
    export function markStarted() {} export function markCompacting() {}
    export function refreshList() {} export function addSessionLocal() {}
  `],
  ['sessionStreams', 'export function attach() { return () => {}; } export function ensureFollowing() {} export function invalidateSession() {}'],
  ['features/conversation/useSessionContext', 'export const useSessionContext = () => ({ context: null });'],
  ['hooks/useStickToBottom', `import { useMemo } from 'react';
    export function useStickToBottom(ref) { return useMemo(() => ({ stick: true, snapToBottom() {}, release() {}, reset() {}, follow() {}, setNode(node) { ref.current = node; } }), [ref]); }
  `],
  ['hooks/useContainedWheel', "import { useRef } from 'react'; export const useContainedWheel = () => useRef(null);"],
  ['components/PermissionSelector', "export const PermissionSelector = () => null; export const loadPermission = () => 'auto-edit'; export const hasStoredPermission = () => false;"],
])
for (const name of ['RuntimePanel', 'SessionView', 'AgentPresetSelector', 'SessionPresetLabel', 'OpenInApp',
  'StatsBar', 'ComposerModelMenu', 'ComposerEffortControl', 'PlanReviewPanel', 'ApprovalDialog',
  'ConfirmDialog', 'ContextRing', 'TodoPanel', 'GoalBar', 'ConversationAxis']) {
  stubs.set(`components/${name}`, `export const ${name} = () => null;`)
}
const result = await build({
  absWorkingDir: web,
  stdin: { contents: 'export { default as SessionsPage } from "./pages/SessionsPage"; export * as editorApi from "./pages/editor"; export { t, setLocale } from "./i18n";',
    resolveDir: src, sourcefile: 'composer-retention-entry.tsx', loader: 'tsx' },
  bundle: true, write: false, format: 'cjs', platform: 'node', target: 'node20', jsx: 'automatic',
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  loader: { '.css': 'empty' }, logLevel: 'silent', metafile: true,
  plugins: [{ name: 'composer-retention-stubs', setup(plugin) {
    plugin.onResolve({ filter: /^\./ }, args => {
      const resolved = join(args.resolveDir, args.path).replace(/\\/g, '/').replace(/\.(tsx?|jsx?)$/, '')
      const prefix = src.replace(/\\/g, '/') + '/'
      const key = resolved.startsWith(prefix) ? resolved.slice(prefix.length) : ''
      return stubs.has(key) ? { path: key, namespace: 'optimizer-stub' } : undefined
    })
    plugin.onLoad({ filter: /.*/, namespace: 'optimizer-stub' }, args => ({ contents: stubs.get(args.path), loader: 'tsx', resolveDir: src }))
  } }],
})
const inputs = new Set(Object.keys(result.metafile.inputs)
  .filter(path => !path.startsWith('optimizer-stub:') && path !== '<stdin>')
  .map(path => resolve(web, path).replace(/\\/g, '/')))
const realInput = path => inputs.has(join(src, path).replace(/\\/g, '/'))
for (const path of ['api.ts', 'appStore.ts', 'sessionStreams.ts', 'promptOptimizer.ts', 'composerMemory.ts']) {
  assert.ok(!realInput(path), `Real ${path} must not be bundled`)
}
for (const path of ['pages/SessionsPage.tsx', 'pages/editor.ts', 'features/conversation/useComposerSlash.ts',
  'features/conversation/useComposerPersistence.ts', 'features/conversation/useInputHistory.ts', 'components/QueuedMessagePanel.tsx']) {
  assert.ok(realInput(path), `Must retain real ${path}`)
}

const dom = new JSDOM('<!doctype html><body><div id="root"></div></body>', { url: 'http://localhost/', pretendToBeVisual: true })
const g = dom.window
const previousGlobals = new Map()
function install(name, value) {
  previousGlobals.set(name, Object.getOwnPropertyDescriptor(globalThis, name))
  Object.defineProperty(globalThis, name, { value, configurable: true, writable: true })
}
for (const name of ['window', 'document', 'navigator', 'location', 'history', 'HTMLElement', 'HTMLInputElement',
  'Element', 'Node', 'Event', 'MouseEvent', 'KeyboardEvent', 'MutationObserver', 'getComputedStyle',
  'requestAnimationFrame', 'cancelAnimationFrame', 'localStorage', 'sessionStorage', 'File', 'FileReader', 'Blob']) install(name, g[name])
install('ResizeObserver', class { observe() {} unobserve() {} disconnect() {} })
install('IS_REACT_ACT_ENVIRONMENT', true)
g.HTMLElement.prototype.scrollIntoView = function () {}
for (const name of ['fetch', 'XMLHttpRequest', 'WebSocket', 'EventSource']) {
  const blocked = (...args) => { bridge.networkAttempts.push({ name, args }); throw new Error(`External transport forbidden: ${name}`) }
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
const filename = join(web, 'node_modules', '.composer-retention-regression.cjs')
const bundled = new Module(filename)
bundled.filename = filename
bundled.paths = Module._nodeModulePaths(join(web, 'node_modules'))
bundled.require = createRequire(filename)
bundled._compile(result.outputFiles[0].text, filename)
const { SessionsPage, editorApi, t, setLocale } = bundled.exports
setLocale('en')
const editor = () => document.querySelector('.prompt-editor')
const queue = () => [...document.querySelectorAll('.queue-text')].map(node => node.textContent)
const text = () => editorApi.serializeEditor(editor())
async function setDraft(value) {
  await act(async () => {
    editor().focus()
    editor().textContent = value
    editorApi.selectRange(editor(), value.length, value.length)
    editor().dispatchEvent(new g.Event('input', { bubbles: true }))
  })
  assert.equal(text(), value, 'Input fixture must preserve whitespace')
}
let root
async function render(id = session.id) {
  root ??= createRoot(document.getElementById('root'))
  await act(async () => root.render(React.createElement(SessionsPage, {
    key: id, activeId: id, locked: false, hasStarted: true,
    onSelectWorkspace() {}, onOpenPicker() {}, onAddWorkspace() {},
  })))
  assert.ok(editor(), 'Real SessionsPage mounts')
}
async function remount(id = session.id) {
  await act(async () => root.unmount())
  root = null
  await render(id)
}
async function key(name, offset = text().length, options = {}) {
  const event = new g.KeyboardEvent('keydown', { key: name, bubbles: true, cancelable: true, ...options })
  await act(async () => {
    editor().focus()
    editorApi.selectRange(editor(), offset, offset)
    editor().dispatchEvent(event)
  })
  return event.defaultPrevented
}
try {
  await render()
  const draftA = '  草稿 A\n\n末行  '
  const draftB = '\n 草稿 B  '
  await setDraft(draftA)
  await render(second.id)
  assert.equal(text(), '', 'New session never inherits previous session draft')
  await setDraft(draftB)
  await render()
  assert.equal(text(), draftA, 'Returning to A restores exact whitespace')
  await remount()
  assert.equal(text(), draftA, 'Unmount/remount restores A draft')
  await render(second.id)
  assert.equal(text(), draftB, 'B retains its independent draft')
  await render()

  await setDraft('Unsent draft')
  assert.equal(await key('ArrowUp'), true)
  assert.equal(text(), 'Recent request', 'Up recalls newest non-injected user input')
  await key('ArrowUp', 0)
  assert.equal(text(), 'Older request', 'Repeated Up recalls older input')
  await key('ArrowDown')
  assert.equal(text(), 'Recent request')
  await key('ArrowDown')
  assert.equal(text(), 'Unsent draft', 'Down past newest restores unsent draft')
  await key('ArrowUp')
  await key('ArrowUp', 0)
  assert.equal(text(), 'Older request')
  await setDraft('Manually edited history')
  await key('ArrowUp', 0)
  assert.equal(text(), 'Recent request', 'Editing recalled input resets browsing to newest history')
  await key('ArrowDown')
  assert.equal(text(), 'Manually edited history', 'Edited text becomes the new unsent draft')
  const multiline = 'first\nmiddle\nlast'
  await setDraft(multiline)
  for (const direction of ['ArrowUp', 'ArrowDown']) {
    assert.equal(await key(direction, 8), false, 'Middle line keeps native caret navigation')
    assert.equal(text(), multiline)
  }
  await setDraft('IME draft')
  assert.equal(await key('ArrowUp', 0, { isComposing: true }), false, 'Native IME event does not browse history')
  await act(async () => editor().dispatchEvent(new g.CompositionEvent('compositionstart', { bubbles: true })))
  assert.equal(await key('ArrowUp', 0), false, 'Composition state does not browse history')
  assert.equal(text(), 'IME draft')
  await act(async () => editor().dispatchEvent(new g.CompositionEvent('compositionend', { bubbles: true })))

  bridge.running = true
  await render()
  await setDraft('Queued while running')
  const send = document.querySelector('.btn-send:not(.stop)')
  assert.ok(send, 'Running session with draft offers the real queue/send button')
  assert.equal(send.disabled, false)
  await act(async () => send.click())
  assert.deepEqual(queue(), ['Queued while running'])
  assert.equal(text(), '', 'Queueing clears only the submitted draft')
  await render(second.id)
  assert.deepEqual(queue(), [], 'Queue does not leak to another session')
  assert.equal(text(), draftB)
  await render()
  assert.deepEqual(queue(), ['Queued while running'], 'Queue survives session switch')
  await remount()
  assert.deepEqual(queue(), ['Queued while running'], 'Queue survives page remount while still running')
  assert.equal(text(), '')
  assert.deepEqual(bridge.memory.get(`session:${session.id}`).queue.map(item => item.text), ['Queued while running'])
  assert.ok(bridge.calls.includes(session.id), 'History came from mocked session API')
  assert.deepEqual(bridge.notifications.filter(args => args[0] === 'err'), [], 'No storage or send errors')
  assert.deepEqual(bridge.networkAttempts, [], 'No real API or network transport used')
  assert.deepEqual(uncaught, [], 'No uncaught DOM errors')
  assert.deepEqual(consoleErrors, [], 'No React/runtime errors')
  console.log('PASS composer retention: isolated drafts and queued messages survive switching/remount; history preserves draft and respects multiline/IME')
} finally {
  if (root) await act(async () => root.unmount())
  console.error = originalConsoleError
  dom.window.close()
  for (const [name, descriptor] of previousGlobals) {
    if (descriptor) Object.defineProperty(globalThis, name, descriptor)
    else delete globalThis[name]
  }
  delete globalThis[bridgeKey]
}
