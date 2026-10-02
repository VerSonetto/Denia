import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'

const webRoot = join(dirname(fileURLToPath(import.meta.url)), '..')
const output = join(webRoot, 'node_modules', '.shots', 'session-memory.mjs')
mkdirSync(dirname(output), { recursive: true })
await build({ entryPoints: [join(webRoot, 'src', 'sessionMemory.ts')], outfile: output,
  bundle: true, format: 'esm', platform: 'node', logLevel: 'silent' })
const { appendSessionEvents, liveWindowStart, LIVE_EVENT_LIMIT } = await import(pathToFileURL(output).href)
let seq = 0
const event = (type, fields = {}) => ({ type, seq: ++seq, time: seq, ...fields })
const chunk = (turn = 1, step = 1) => event('assistant-chunk', {
  turn, step, chunk: { type: 'text-delta', index: 0, text: 'fragment' },
})
const message = (turn = 1, step = 1) => event('assistant-message', {
  turn, step, blocks: [{ type: 'text', text: 'settled answer' }],
})
const events = []
appendSessionEvents(events, [event('turn-start', { turn: 1 }), ...Array.from({ length: 10_000 }, () => chunk())])
assert.equal(events.length, 10_001)
appendSessionEvents(events, [message()])
assert.equal(events.length, 2)
console.log('ok   10,000 settled chunks released without losing the final message')
const retry = [chunk(), message(), chunk()]
const retained = []
appendSessionEvents(retained, retry)
assert.deepEqual(retained.map((entry) => entry.seq), retry.slice(1).map((entry) => entry.seq))
appendSessionEvents(retained, [event('turn-end', { turn: 1 }), chunk(2)])
assert.equal(retained.filter((entry) => entry.type === 'assistant-chunk').length, 1)
assert.equal(retained.at(-1).turn, 2)
console.log('ok   retry and next-turn chunks remain intact')
let windowEvents = []
let trimmed = 0
for (let turn = 1; turn <= 2000; turn++) {
  appendSessionEvents(windowEvents, [event('user-message', { text: `turn ${turn}` }),
    event('turn-start', { turn }), message(turn), event('turn-end', { turn })])
  const start = liveWindowStart(windowEvents)
  if (start) {
    trimmed += start
    windowEvents = windowEvents.slice(start)
    assert.equal(windowEvents[0].type, 'user-message')
  }
  assert.ok(windowEvents.length <= LIVE_EVENT_LIMIT)
}
assert.equal(trimmed + windowEvents.length, 8000)
assert.equal(liveWindowStart(Array.from({ length: 1000 }, () => message())), 0)
console.log(`ok   2,000 turns: ${windowEvents.length}/8,000 events retained; ${trimmed} remain reloadable from disk`)
