import assert from 'node:assert/strict'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'
const dom = new JSDOM('<div contenteditable="true" id="editor"></div>')
Object.assign(globalThis, { window: dom.window, document: dom.window.document, Node: dom.window.Node })
const { outputFiles } = await build({ entryPoints: ['src/pages/editor.ts'], bundle: true, write: false, format: 'esm', platform: 'node' })
const mod = await import(`data:text/javascript;base64,${Buffer.from(outputFiles[0].text).toString('base64')}`)
const editor = document.querySelector('#editor')
const kinds = new Map([['plan', 'command']])
const draft = '/plan 看 @src/index.ts 和 @"my docs/"\nuser@host @"unfinished'
mod.renderDraft(editor, draft, kinds)
assert.equal(mod.serializeEditor(editor), draft)
assert.equal(editor.querySelectorAll('.file-chip').length, 2)
assert.equal(editor.querySelectorAll('.slash-chip').length, 1)
const chip = editor.querySelector('.file-chip')
assert.equal(chip.getAttribute('contenteditable'), 'false')
assert.equal(chip.title, 'src/index.ts')
assert.equal(chip.querySelector('.file-chip-label').textContent, 'index.ts')
assert.equal(chip.dataset.fileMention, '@src/index.ts')
mod.setCaretOffset(editor, draft.indexOf('@src') + 2)
assert.equal(mod.caretOffsetIn(editor), draft.indexOf('@src') + '@src/index.ts'.length)
assert.equal(mod.caretRightAfterChip(editor), true)
const range = document.createRange()
range.setStart(chip.querySelector('.file-chip-label').firstChild, 1)
range.collapse(true)
window.getSelection().removeAllRanges()
window.getSelection().addRange(range)
assert.equal(mod.caretOffsetIn(editor), draft.indexOf('@src') + '@src/index.ts'.length)
mod.renderDraft(editor, '@src/a.ts 后续正文', kinds)
let text = mod.serializeEditor(editor)
assert.equal(mod.activeFileTokenIn(editor, text, text.length), undefined)
editor.append(document.createTextNode(' @src component'))
text = mod.serializeEditor(editor)
assert.equal(mod.activeFileTokenIn(editor, text, text.length).query, 'src component')
assert.equal(mod.createFileChip({ path: 'bad\npath', kind: 'file' }), null)
const commands = []
document.execCommand = (command, _ui, html) => {
  commands.push(command)
  const selected = window.getSelection().getRangeAt(0)
  selected.deleteContents()
  selected.insertNode(selected.createContextualFragment(html))
  return true
}
mod.renderDraft(editor, 'helloWORLD', kinds)
assert.equal(mod.insertFileChip(editor, { path: 'my docs', kind: 'directory' }, 5, 5), true)
assert.deepEqual(commands, ['insertHTML'])
assert.equal(mod.serializeEditor(editor), 'hello @"my docs/" WORLD')
assert.equal(mod.caretOffsetIn(editor), 'hello @"my docs/" '.length)
mod.renderDraft(editor, '看 @query next', kinds)
// A live query is plain editable text until the candidate is chosen.
editor.textContent = '看 @query next'
assert.equal(mod.insertFileChip(editor, { path: 'src/a.ts', kind: 'file' }, 2, 8), true)
assert.equal(mod.serializeEditor(editor), '看 @src/a.ts next')
assert.equal(mod.caretOffsetIn(editor), '看 @src/a.ts'.length)
// A browser-inserted placeholder <br> after deleting everything serializes to a
// newline; the placeholder hint must still come back, so emptiness is decided by
// structure (draftIsEmpty) and published on data-empty.
mod.renderDraft(editor, '', kinds)
editor.appendChild(document.createElement('br'))
assert.equal(mod.serializeEditor(editor), '\n')
assert.equal(mod.draftIsEmpty(editor), true)
assert.equal(mod.syncEmptyMark(editor), true)
assert.equal(mod.draftIsEmpty(editor), true)
editor.insertBefore(document.createTextNode('x'), editor.firstChild)
assert.equal(mod.draftIsEmpty(editor), false)
assert.equal(mod.syncEmptyMark(editor), false)
assert.equal(editor.dataset.empty, undefined)
// A real line break alone stays "empty" too — there is no content to send.
editor.textContent = ''
editor.appendChild(document.createElement('br'))
assert.equal(mod.draftIsEmpty(editor), true)
console.log('ALL PASS: file chips, draft roundtrip, atomic offsets, trigger boundaries, insertion and empty-draft detection')
