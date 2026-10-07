import assert from 'node:assert/strict'
import { mkdirSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const web = fileURLToPath(new URL('..', import.meta.url))
const shots = join(web, 'node_modules', '.shots')
mkdirSync(shots, { recursive: true })
const output = join(shots, 'composer-context.mjs')
await build({ entryPoints: [join(web, 'src/features/conversation/composerContext.ts')],
  outfile: output, bundle: true, platform: 'node', format: 'esm', logLevel: 'silent' })
const context = await import(pathToFileURL(output).href)
assert.equal(context.sameWorkspacePath('D:\\Denia\\', 'd:/denia'), true)
assert.equal(context.sameWorkspacePath('D:/', 'd:\\'), true)
assert.equal(context.sameWorkspacePath('\\\\host\\share\\', '//HOST/share'), true)
assert.equal(context.sameWorkspacePath('/Code', '/code'), false)
assert.equal(context.sameWorkspacePath('/a', '/a/b'), false)
assert.equal(context.sameWorkspacePath(null, null), false)
assert.equal(context.utf8Prefix('中文😀abc', 7), '中文')
assert.equal(context.utf8Prefix('中文😀abc', 10), '中文😀')

const session = { id: 'source', cwd: 'D:/Denia', title: '参考会话', excerpt: null, created_at: 1 }
const event = (seq, data) => ({ seq, time: seq, ...data })
const events = [
  event(1, { type: 'system-prompt', text: 'PRIVATE SYSTEM', turn: 1, step: 1 }),
  event(2, { type: 'user-message', text: '用户问题' }),
  event(3, { type: 'user-message', text: 'PRIVATE INJECTION', injected: true }),
  event(4, { type: 'assistant-message', blocks: [
    { type: 'reasoning', text: 'PRIVATE REASONING' },
    { type: 'text', text: '回答内容' },
    { type: 'tool-call', name: 'bash', arguments: 'PRIVATE ARGUMENTS', id: 'call' },
  ], turn: 1, step: 1 }),
  event(5, { type: 'tool-result', content: 'PRIVATE TOOL', call_id: 'call' }),
]
const quote = context.buildSessionContextQuote(session, events, false)
assert.equal(quote.id, 'session:source')
assert.match(quote.text, /Session: source/)
assert.match(quote.text, /Workspace: D:\/Denia/)
assert.match(quote.text, /用户问题/)
assert.match(quote.text, /回答内容/)
assert.doesNotMatch(quote.text, /PRIVATE/)
context.validateContextQuotes([quote])
assert.throws(() => context.buildSessionContextQuote(session, [events[0]], false), /没有可引用/)
const huge = context.buildSessionContextQuote({ ...session, title: '标题😀'.repeat(100) }, [
  event(1, { type: 'user-message', text: '旧问题'.repeat(30000) }),
  event(2, { type: 'assistant-message', blocks: [{ type: 'text', text: '最近回答😀' }], turn: 1, step: 1 }),
], true)
assert.ok(Buffer.byteLength(huge.text) <= 64 * 1024)
assert.ok(Buffer.byteLength(huge.title) <= 200)
assert.ok(huge.text.endsWith('最近回答😀'))
assert.match(huge.text, /较早内容已省略/)
assert.doesNotMatch(huge.text, /\uFFFD/)
context.validateContextQuotes([huge])
assert.throws(() => context.validateContextQuotes([{ ...quote, text: '中'.repeat(22000) }]), /64 KiB/)
assert.throws(() => context.validateContextQuotes([{ ...quote, title: '中'.repeat(67) }]), /200/)
assert.throws(() => context.validateContextQuotes([{ ...quote, text: '   ' }]), /非空/)
assert.throws(() => context.validateContextQuotes(Array.from({ length: 5 }, () => ({ ...quote, text: 'a'.repeat(60000) }))), /256 KiB/)
console.log('ok   workspace scope, dialogue-only session quotes, UTF-8 truncation and quote budgets')

const dom = new JSDOM('<body><div id="editor"></div><button>menu</button></body>', { url: 'http://localhost/' })
for (const key of ['window', 'document', 'Node', 'HTMLElement']) globalThis[key] = dom.window[key]
const editorOutput = join(shots, 'composer-context-editor.mjs')
await build({ entryPoints: [join(web, 'src/pages/editor.ts')], outfile: editorOutput,
  bundle: true, platform: 'node', format: 'esm', logLevel: 'silent' })
const editorApi = await import(pathToFileURL(editorOutput).href)
const editor = document.getElementById('editor')
editorApi.renderDraft(editor, 'before /plan after', new Map([['plan', 'command']]))
editorApi.selectRange(editor, 7, 12)
assert.deepEqual(editorApi.selectionOffsetsIn(editor), { start: 7, end: 12 })
assert.equal(editorApi.caretOffsetIn(editor), 7)
editorApi.setCaretOffset(editor, 17)
assert.deepEqual(editorApi.selectionOffsetsIn(editor), { start: 17, end: 17 })
const outsideRange = document.createRange()
outsideRange.selectNodeContents(document.querySelector('button'))
window.getSelection().removeAllRanges()
window.getSelection().addRange(outsideRange)
assert.deepEqual(editorApi.selectionOffsetsIn(editor), { start: 18, end: 18 })
assert.equal(editorApi.caretOffsetIn(editor), 0)
editor.replaceChildren(document.createTextNode('one'), document.createElement('br'), document.createTextNode('two'))
editorApi.selectRange(editor, 1, 6)
assert.deepEqual(editorApi.selectionOffsetsIn(editor), { start: 1, end: 6 })
dom.window.close()
console.log('ok   add-context snapshots preserve selection ranges, slash chips and multiline offsets')
