// User-visible race behavior: new searches/cold sessions cannot receive old results.
import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const web = fileURLToPath(new URL('..', import.meta.url))
const out = join(web, 'node_modules', '.shots', 'conversation-hooks.mjs')
mkdirSync(join(web, 'node_modules', '.shots'), { recursive: true })
await build({ entryPoints: [join(web, 'src/features/conversation/useComposerMentions.ts')],
  outfile: out, bundle: true, format: 'esm', platform: 'node',
  external: ['react', 'react-dom', 'react/jsx-runtime'], logLevel: 'silent' })
const dom = new JSDOM('<body><div id="root"></div></body>', {url: 'http://localhost/', pretendToBeVisual: true})
for (const name of ['window', 'document', 'navigator']) {
  Object.defineProperty(globalThis, name, {value: dom.window[name], configurable: true})
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true
const React = await import('react')
const { act } = React
const { createRoot } = await import('react-dom/client')
const { useComposerMentions } = await import(pathToFileURL(out).href)
const requests = []
globalThis.fetch = (url, init) => new Promise(resolve => requests.push({url, signal: init.signal, resolve}))
let state
const noop = () => {}
function Probe({cwd}) {
  const promptRef = React.useRef(null)
  state = useComposerMentions({mentionCwd: cwd, promptRef, onDraftChange: noop, syncPromptHeight: noop})
  return null
}
const root = createRoot(document.getElementById('root'))
const waitForSearch = () => new Promise(resolve => setTimeout(resolve, 180))
const response = path => ({ok: true, json: async () => ({items: [{path, kind: 'file'}]})})
await act(async () => root.render(React.createElement(Probe, {cwd: '/a'})))
await act(async () => { state.refreshMentions('@old', 4); await waitForSearch() })
assert.equal(requests.length, 1)
await act(async () => state.refreshMentions('@new', 4))
assert.equal(requests[0].signal.aborted, true)
await act(async () => { requests[0].resolve(response('old')); await waitForSearch() })
assert.equal(state.mentionItems.length, 0)
assert.equal(requests.length, 2)
await act(async () => requests[1].resolve(response('new')))
assert.deepEqual(state.mentionItems.map(item => item.path), ['new'])
await act(async () => root.render(React.createElement(Probe, {cwd: '/b'})))
assert.equal(state.mentionOpen, false)
assert.equal(state.mentionItems.length, 0)
await act(async () => state.refreshMentions('@pending', 8))
await act(async () => root.unmount())
await waitForSearch()
assert.equal(requests.length, 2)
dom.window.close()
console.log('ok   stale searches cannot overwrite new results; cwd changes and unmount cancel work')
