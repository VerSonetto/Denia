import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'

const web = fileURLToPath(new URL('..', import.meta.url))
const shots = join(web, 'node_modules', '.shots')
mkdirSync(shots, { recursive: true })
const output = join(shots, 'composer-memory.mjs')
await build({ entryPoints: [join(web, 'src/composerMemory.ts')], outfile: output,
  bundle: true, platform: 'node', format: 'esm', logLevel: 'silent' })

// Node 22 clones File as Blob; browsers preserve File metadata in structured clone.
const nativeClone = globalThis.structuredClone
function browserClone(value) {
  if (value instanceof File) return new File([value], value.name, { type: value.type, lastModified: value.lastModified })
  if (Array.isArray(value)) return value.map(browserClone)
  if (value && Object.getPrototypeOf(value) === Object.prototype) {
    return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, browserClone(item)]))
  }
  return nativeClone(value)
}
globalThis.structuredClone = browserClone

// Model asynchronous request/transaction completion and the browser's clone boundary.
const records = new Map()
const texts = new Map()
const committed = []
let failOpen = false
let failWrite = false
let failBackup = false
let failReadBackup = false
let opened = 0
let closed = 0
let activeWrites = 0
let maxActiveWrites = 0
globalThis.localStorage = {
  getItem(key) {
    if (failReadBackup) throw new Error('Backup read denied')
    return texts.get(key) ?? null
  },
  setItem(key, value) {
    if (failBackup) throw new Error('Backup quota exceeded')
    texts.set(key, value)
  },
}
globalThis.indexedDB = {
  open() {
    const request = {}
    setTimeout(() => {
      if (failOpen) {
        request.error = new Error('Database unavailable')
        request.onerror()
        return
      }
      opened++
      request.result = {
        objectStoreNames: { contains: () => true },
        close() { closed++ },
        transaction(_name, mode) {
          const transaction = {}
          transaction.objectStore = () => ({
            get(key) {
              const get = {}
              setTimeout(() => {
                get.result = structuredClone(records.get(key))
                transaction.oncomplete()
              }, 1)
              return get
            },
            put(value, key) {
              assert.equal(mode, 'readwrite')
              const captured = structuredClone(value)
              const put = {}
              activeWrites++
              maxActiveWrites = Math.max(maxActiveWrites, activeWrites)
              setTimeout(() => {
                activeWrites--
                if (failWrite) {
                  transaction.error = new Error('Transaction aborted')
                  transaction.onabort()
                  return
                }
                records.set(key, captured)
                committed.push([key, captured.snapshot.text])
                put.result = key
                transaction.oncomplete()
              }, 3)
              return put
            },
          })
          return transaction
        },
      }
      request.onsuccess()
    }, 1)
    return request
  },
}

const memory = await import(pathToFileURL(output).href)
const empty = () => ({ text: '', images: [], attachments: [], quotes: [], queue: [] })
assert.deepEqual(await memory.loadComposer('session:missing'), empty())
assert.equal(memory.readComposerText('session:missing'), null)
await assert.rejects(memory.loadComposer('invalid'), /scope/)
await assert.rejects(memory.saveComposer('workspace:', empty()), /scope/)
const attachment = { name: 'example.txt', path: 'D:/docs/example.txt' }
const snapshot = {
  text: 'draft',
  images: [{ name: 'image.png', mime: 'image/png', data: 'YQ==', bytes: 1, preview: 'data:image/png;base64,YQ==' }],
  attachments: [attachment],
  quotes: [{ id: 'quote', title: 'Reference', text: 'body' }],
  queue: [{ id: 'q1', text: 'queued', attachments: [attachment], images: [], quotes: [] }],
}
const saving = memory.saveComposer('session:one', snapshot)
assert.equal(memory.readComposerText('session:one'), 'draft', 'backup must be synchronous')
snapshot.text = 'caller mutation'
snapshot.queue.push({ id: 'q2', text: 'not saved' })
await saving
const restored = await memory.loadComposer('session:one')
assert.equal(restored.text, 'draft')
assert.equal(restored.queue.length, 1)
assert.deepEqual(restored.images, snapshot.images)
assert.deepEqual(restored.quotes, snapshot.quotes)
for (const entry of [restored.attachments[0], restored.queue[0].attachments[0]]) {
  assert.equal(entry.name, 'example.txt')
  assert.equal(entry.path, 'D:/docs/example.txt')
}
// 旧草稿（只有 File 句柄、没有绝对路径）必须在恢复时丢弃，否则会在发送时才失败。
await memory.saveComposer('session:legacy', {
  ...empty(),
  attachments: [{ name: 'old.txt', mime: 'text/plain', file: new File(['x'], 'old.txt') }],
  queue: [{ id: 'q-legacy', text: 'old', images: [], quotes: [],
    attachments: [{ name: 'old.txt', mime: 'text/plain', file: new File(['x'], 'old.txt') }] }],
})
const legacy = await memory.loadComposer('session:legacy')
assert.deepEqual(legacy.attachments, [], '旧附件形状必须被过滤')
assert.deepEqual(legacy.queue[0].attachments, [], '队列里的旧附件同样过滤')
const writes = ['first', 'second', 'third'].map(text => memory.saveComposer('session:ordered', { ...empty(), text }))
await Promise.all(writes)
assert.equal(maxActiveWrites, 1)
assert.deepEqual(committed.filter(([scope]) => scope === 'session:ordered').map(([, text]) => text), ['first', 'second', 'third'])
assert.equal((await memory.loadComposer('session:ordered')).text, 'third')
await memory.saveComposer('workspace:D:/one', { ...empty(), text: 'workspace' })
assert.equal((await memory.loadComposer('session:one')).text, 'draft')
assert.equal((await memory.loadComposer('workspace:D:/one')).text, 'workspace')

failWrite = true
await assert.rejects(memory.saveComposer('session:one', { ...empty(), text: 'new backup' }), /aborted/)
failWrite = false
assert.equal((await memory.loadComposer('session:one')).text, 'new backup')
assert.equal((await memory.loadComposer('session:one')).queue.length, 1, 'failed save must not delete prior full payload')
await memory.saveComposer('session:one', empty())
assert.deepEqual(await memory.loadComposer('session:one'), empty(), 'explicit empty snapshot is persisted')

failBackup = true
await assert.rejects(memory.saveComposer('session:one', { ...empty(), text: 'database survives backup failure' }), /quota/)
failBackup = false
assert.equal((await memory.loadComposer('session:one')).text, 'database survives backup failure')
failOpen = true
await assert.rejects(memory.loadComposer('session:one'), /unavailable/)
await assert.rejects(memory.saveComposer('session:offline', { ...empty(), text: 'offline text' }), /unavailable/)
assert.equal(memory.readComposerText('session:offline'), 'offline text')
failOpen = false
assert.equal((await memory.loadComposer('session:offline')).text, 'offline text')
failReadBackup = true
assert.throws(() => memory.readComposerText('session:one'), /denied/)
await assert.rejects(memory.loadComposer('session:one'), /denied/)
failReadBackup = false
texts.set('denia:composer-text:session:broken', '{broken')
await assert.rejects(memory.loadComposer('session:broken'), SyntaxError)
assert.equal(texts.get('denia:composer-text:session:broken'), '{broken', 'do not delete corrupt user data')
records.set('session:bad-record', { revision: 1 })
await assert.rejects(memory.loadComposer('session:bad-record'), /Invalid composer snapshot/)
assert.ok(records.has('session:bad-record'))
assert.equal(opened, closed, 'every database connection must close')
globalThis.structuredClone = nativeClone
console.log('composer-memory checks passed (payloads, File, isolation, ordered writes, backup, failures)')
