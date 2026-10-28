import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'

const web = fileURLToPath(new URL('..', import.meta.url))
const output = join(web, 'node_modules', '.shots', 'session-streams-memory.mjs')
mkdirSync(dirname(output), { recursive: true })
await build({ entryPoints: [join(web, 'src', 'sessionStreams.ts')], outfile: output,
  bundle: true, format: 'esm', platform: 'node', logLevel: 'silent',
  plugins: [{ name: 'session-api-fixture', setup(builder) {
    builder.onLoad({ filter: /[\\/]api\.ts$/ }, () => ({ loader: 'ts', contents: `
export class ApiError extends Error { constructor(status) { super(); this.status = status } }
export function getSessionPage(id, options, signal) {
  return new Promise((resolve, reject) => globalThis.fixture.requests.push({ id, options, signal, resolve, reject }))
}
export function followSession(id, after, onEnvelope, onEnd) {
  const controller = new AbortController()
  globalThis.fixture.follows.push({ id, after, onEnvelope, onEnd, controller })
  return controller
}
` }))
  } }],
})
globalThis.window = globalThis
globalThis.document = { hidden: false }
globalThis.fixture = { requests: [], follows: [] }
const engine = await import(pathToFileURL(output).href)
const tick = async () => { await Promise.resolve(); await Promise.resolve() }
const snapshot = (seq = 7) => ({ header: { id: 'session', cwd: 'D:/workspace' },
  events: [{ seq, time: 1, type: 'user-message', text: 'fixture' }],
  total: 1, hasMoreBefore: false, anchors: [] })
const listener = () => ({ snapshots: 0, envelopes: 0,
  onSnapshot() { this.snapshots++ }, onEnvelope() { this.envelopes++ } })

try {
  const first = listener(), second = listener()
  const detachFirst = engine.attach('session', first)
  const detachSecond = engine.attach('session', second)
  engine.ensureFollowing('session')
  const initialRequests = fixture.requests.length
  for (const request of fixture.requests) request.resolve(snapshot())
  await tick()
  if (process.argv.includes('--baseline')) {
    console.log(`baseline: two subscribers + running startup = ${initialRequests} page requests, ${fixture.requests.filter(r => r.signal.aborted).length} aborted`)
  } else {
    assert.equal(initialRequests, 1, 'concurrent subscribers and running startup share one snapshot')
    assert.equal(fixture.requests[0].signal.aborted, false)
    assert.equal(first.snapshots, 1)
    assert.equal(second.snapshots, 1)
    assert.equal(fixture.follows.length, 1)
    assert.equal(fixture.follows[0].after, 7)
    fixture.follows[0].onEnvelope({ seq: 8, time: 2, type: 'turn-start', turn: 1 })
    assert.equal(first.envelopes, 1)
    assert.equal(second.envelopes, 1)
    engine.ensureFollowing('session')
    assert.equal(fixture.follows.length, 1)
    detachFirst()
    assert.equal(fixture.follows[0].controller.signal.aborted, false)
    detachSecond()
    assert.equal(fixture.follows[0].controller.signal.aborted, true)
    assert.equal(engine.streamCount(), 0)
    console.log('ok   two subscribers + running startup: 3 page requests -> 1; no restart or duplicate follow')
  }
  engine.resetEngine()
  fixture.requests = []; fixture.follows = []
  const browsing = listener()
  engine.attach('browse', browsing)
  fixture.requests[0].resolve(snapshot())
  await tick()
  assert.equal(fixture.follows.length, 0)
  engine.ensureFollowing('browse')
  if (process.argv.includes('--baseline')) {
    console.log(`baseline: start following a loaded conversation = ${fixture.requests.length} total page requests`)
  } else {
    assert.equal(fixture.requests.length, 1, 'following uses the confirmed cursor, not another page')
    assert.equal(fixture.follows.length, 1)
    const late = listener()
    const detachLate = engine.attach('browse', late)
    assert.equal(fixture.requests.length, 2, 'a late subscriber gets fresh state without retaining a second event cache')
    detachLate()
    fixture.requests[1].resolve(snapshot(9))
    await tick()
    assert.equal(late.snapshots, 0)
    assert.equal(browsing.snapshots, 2)
    assert.equal(fixture.follows.at(-1).after, 9)
    console.log('ok   loaded conversation starts following without reloading; detached listeners cannot receive stale snapshots')

    engine.resetEngine()
    fixture.requests = []; fixture.follows = []
    const stale = listener()
    const detachStale = engine.attach('reuse', stale)
    detachStale()
    const current = listener()
    const detachCurrent = engine.attach('reuse', current)
    fixture.requests[0].resolve(snapshot(50))
    await tick()
    assert.equal(current.snapshots, 0)
    fixture.requests[1].resolve(snapshot(2))
    await tick()
    assert.equal(stale.snapshots, 0)
    assert.equal(current.snapshots, 1)
    assert.equal(engine.cursorOf('reuse'), 2)
    detachCurrent()
    assert.equal(engine.streamCount(), 0)
    console.log('ok   switching away releases stream state; old requests cannot overwrite a reopened session')

    fixture.requests = []; fixture.follows = []
    const detachReentrant = engine.attach('reentrant', {
      onSnapshot() { engine.ensureFollowing('reentrant') }, onEnvelope() {},
    })
    fixture.requests[0].resolve(snapshot())
    await tick()
    assert.equal(fixture.follows.length, 1, 'snapshot listeners may request follow without opening it twice')
    detachReentrant()

    fixture.requests = []; fixture.follows = []
    const detachLeft = engine.attach('left', listener())
    const detachRight = engine.attach('right', listener())
    engine.ensureFollowing('left')
    engine.ensureFollowing('right')
    assert.equal(fixture.requests.length, 2)
    fixture.requests[0].resolve(snapshot(20))
    fixture.requests[1].resolve(snapshot(30))
    await tick()
    assert.deepEqual(fixture.follows.map(follow => [follow.id, follow.after]), [['left', 20], ['right', 30]])
    detachLeft(); detachRight()
    for (let index = 0; index < 100; index++) {
      const detach = engine.attach(`switch-${index}`, listener())
      detach()
    }
    assert.equal(engine.streamCount(), 0)
    assert.ok(fixture.requests.slice(2).every(request => request.signal.aborted))
    console.log('ok   reentrant callbacks open one follow; split sessions stay independent; 100 switches retain no stream states')
  }
} finally {
  engine.resetEngine()
}
