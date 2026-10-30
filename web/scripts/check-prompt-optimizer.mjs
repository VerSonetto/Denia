/**
 * 提示词优化行为回归：真实 SessionsPage/editor/useComposerSlash + jsdom。
 *   node web/scripts/check-prompt-optimizer.mjs
 * esbuild 内存 bundle 仅隔离 API/store/SSE、优化服务和无关展示组件。
 * 点击真实优化/撤销按钮，不复制页面 handler，也不发出网络请求。
 */
import assert from 'node:assert/strict'
import { createRequire, Module } from 'node:module'
import { join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const web = fileURLToPath(new URL('..', import.meta.url))
const src = join(web, 'src')
const bridgeKey = '__deniaPromptOptimizerRegression'
const workspace = { id: 'ws-test', title: 'Workspace', path: 'D:/mock-workspace', created_at: 1 }
const session = { id: 'session-test', title: 'Conversation', cwd: workspace.path, cwd_alive: true, created_at: 1 }
const bridge = { calls: [], notifications: [], networkAttempts: [], pending: null, memory: new Map() }
globalThis[bridgeKey] = bridge
const prelude = `const b = globalThis[${JSON.stringify(bridgeKey)}];`
const unexpectedApi = ['answerApproval', 'answerAsk', 'cancelAsk', 'cancelSession', 'compactSession',
  'forkSession', 'getCheckpointDiff', 'getCheckpoints', 'getSession', 'goalAction', 'rewindSession',
  'setSessionAgentPreset', 'setSessionPermission', 'chatCompletion', 'postPrompt', 'pickFiles']
const stubs = new Map([
  ['composerMemory', `${prelude}
    export async function loadComposer(scope) { return structuredClone(b.memory.get(scope) ?? { text: '', images: [], attachments: [], quotes: [], queue: [] }); }
    export function readComposerText(scope) { return b.memory.get(scope)?.text ?? null; }
    export async function saveComposer(scope, snapshot) { b.memory.set(scope, structuredClone(snapshot)); }
  `],
  ['api', `
    export async function getCatalog() { return { groups: [{ id: 'mock-provider', label: 'Mock', models: [{ id: 'mock-model', name: 'Mock', inputModalities: ['text'] }] }] }; }
    export async function getSettings() { return { namespaces: [] }; }
    export async function listAgentPresets() { return { modeSelection: false, default: null, presets: [] }; }
    export function subscribeEvents() { return () => {}; }
    export async function listSkills() { return { skills: [] }; }
    export async function getGoal() { return { goal: null, tokensUsed: 0, maxRounds: 10 }; }
    export async function pickerCapability() { return { kind: 'native', remote: false }; }
    export async function searchMentions() { return { items: [] }; }
    export async function getSessionPage() { return { events: [], hasMoreBefore: false }; }
    ${unexpectedApi.map(name => `export function ${name}() { throw new Error('Unexpected API: ${name}'); }`).join('\n')}
  `],
  ['promptOptimizer', `${prelude}
    export function optimizePromptText(options) {
      b.calls.push(options);
      return new Promise((resolve, reject) => { b.pending = { resolve, reject }; });
    }
  `],
  ['appStore', `${prelude}
    const sessions = ${JSON.stringify([session])}; const workspaces = ${JSON.stringify([workspace])};
    export const useSessions = () => sessions;
    export const useWorkspaces = () => workspaces;
    export const getActiveWorkspace = () => workspaces[0];
    export const useCatalogTick = () => 0;
    export const useCompactingFor = () => null;
    export const useRunningFor = () => false;
    export function notify(...args) { b.notifications.push(args); }
    export async function ensureSession() { throw new Error('Unexpected session creation'); }
    export function subscribeRunningStatus() { return () => {}; }
    export function setRunningStatus() {} export function setActiveId() {}
    export function markStarted() {} export function markCompacting() {}
    export function refreshList() {} export function addSessionLocal() {}
  `],
  ['sessionStreams', 'export function attach() { return () => {}; } export function ensureFollowing() {} export function invalidateSession() {}'],
  ['features/conversation/useSessionContext', 'export const useSessionContext = () => ({ context: null });'],
  ['streaming/useFollow', `import { useMemo } from 'react';
    export function useFollow(ref) { return useMemo(() => ({ stick: true, pressure: { speedCps: 0, speedAt: 0, scale: 1 }, snapToBottom() {}, release() {}, reset() {}, follow() {}, setPort(node) { ref.current = node; } }), [ref]); }
  `],
  ['streaming/pressure', `import { createContext } from 'react';
    export const SmoothStreamContext = createContext(null);
    export const useStreamPressure = () => null;
    export const createPressure = () => ({ speedCps: 0, speedAt: 0, scale: 1 });
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
    resolveDir: src, sourcefile: 'prompt-optimizer-entry.tsx', loader: 'tsx' },
  bundle: true, write: false, format: 'cjs', platform: 'node', target: 'node20', jsx: 'automatic',
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  loader: { '.css': 'empty' }, logLevel: 'silent', metafile: true,
  plugins: [{ name: 'prompt-optimizer-stubs', setup(plugin) {
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
for (const path of ['pages/SessionsPage.tsx', 'pages/editor.ts', 'features/conversation/useComposerSlash.ts']) {
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
const filename = join(web, 'node_modules', '.prompt-optimizer-regression.cjs')
const bundled = new Module(filename)
bundled.filename = filename
bundled.paths = Module._nodeModulePaths(join(web, 'node_modules'))
bundled.require = createRequire(filename)
bundled._compile(result.outputFiles[0].text, filename)
const { SessionsPage, editorApi, t, setLocale } = bundled.exports
setLocale('en')
const editor = () => document.querySelector('.prompt-editor')
const button = () => document.querySelector('.composer-opt-btn')
const text = () => editorApi.serializeEditor(editor())
async function setDraft(value) {
  await act(async () => {
    editor().focus()
    editor().textContent = value
    editorApi.selectRange(editor(), value.length, value.length)
    editor().dispatchEvent(new g.Event('input', { bubbles: true }))
  })
  assert.equal(text(), value, 'Input fixture must preserve whitespace before optimization')
}
async function clickOptimize() {
  assert.ok(button(), 'Real optimization button exists')
  assert.equal(button().disabled, false, 'Optimization/undo is available')
  await act(async () => button().click())
}
function assertIdle() {
  assert.equal(button().classList.contains('undo'), false, 'Undo state disappears')
  assert.equal(button().classList.contains('busy'), false, 'Busy state disappears')
  assert.equal(button().getAttribute('aria-label'), t('optimizePrompt'), 'Button returns to optimize action')
  assert.equal(button().disabled, false)
}
async function begin(original) {
  await setDraft(original)
  const before = bridge.calls.length
  await clickOptimize()
  assert.equal(bridge.calls.length, before + 1, 'Real page calls the mocked optimizer once')
  const call = bridge.calls.at(-1)
  assert.equal(call.prompt, original, 'Optimizer receives exact original whitespace')
  assert.equal(call.sessionId, session.id)
  assert.equal(call.selection.provider, 'mock-provider')
  assert.equal(call.selection.model, 'mock-model')
  assert.ok(call.signal instanceof AbortSignal)
  assert.equal(call.signal.aborted, false)
  assert.equal(text(), original, 'Pending optimization leaves the draft intact')
  assert.equal(button().disabled, true)
  assert.equal(button().classList.contains('busy'), true)
}
let root
try {
  root = createRoot(document.getElementById('root'))
  await act(async () => root.render(React.createElement(SessionsPage, {
    activeId: session.id, locked: false, hasStarted: true,
    onSelectWorkspace() {}, onOpenPicker() {}, onAddWorkspace() {},
  })))
  assert.ok(editor(), 'Real SessionsPage mounts')
  for (const [original, optimized] of [
    ['  请优化这个需求  \n\n保留原文的换行和空格。\n  ', '明确需求：实现并验证目标。\n输出结果。'],
    ['\n 第二轮使用不同原文   \n末行  ', '第二轮优化结果'],
  ]) {
    await begin(original)
    await act(async () => bridge.pending.resolve(optimized))
    assert.equal(text(), optimized, 'Success fills the real editor with optimized text')
    assert.equal(button().classList.contains('undo'), true, 'Success offers undo')
    assert.equal(button().getAttribute('aria-label'), t('optimizeUndo'))
    assert.deepEqual(bridge.notifications.at(-1), ['ok', t('optimizeDone')])
    await clickOptimize()
    assert.equal(text(), original, 'Undo restores the original character for character, including spaces/newlines')
    assertIdle()
  }
  const original = '  失败时不要改变我\n\n 最后一行  \n'
  await begin(original)
  await act(async () => bridge.pending.reject(new Error('MOCK_OPTIMIZE_FAILURE')))
  assert.equal(text(), original, 'Failure leaves the original draft unchanged')
  assertIdle()
  assert.deepEqual(bridge.notifications.at(-1), ['err', 'MOCK_OPTIMIZE_FAILURE'])
  assert.equal(bridge.calls.length, 3, 'Undo clicks never invoke the optimizer')
  assert.deepEqual(bridge.networkAttempts, [], 'No real API or network transport used')
  assert.deepEqual(uncaught, [], 'No uncaught DOM errors')
  assert.deepEqual(consoleErrors, [], 'No React/runtime errors')
  console.log('PASS prompt optimizer: two success/undo rounds preserve exact whitespace; failure preserves draft; no real network')
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
