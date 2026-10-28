import assert from 'node:assert/strict'
import { build } from 'esbuild'
import { mkdtempSync, rmSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
const root = fileURLToPath(new URL('..', import.meta.url))
const dir = mkdtempSync(join(tmpdir(), 'denia-session-alias-'))
try {
  const store = new Map()
  globalThis.window = {
    localStorage: {
      getItem: (key) => store.get(key) ?? null,
      setItem: (key, value) => { store.set(key, String(value)) },
      removeItem: (key) => { store.delete(key) },
    },
  }
  store.set('denia.session-aliases.v1', JSON.stringify({ s1: '手动名', bad: 1, empty: '  ' }))

  const aliasOut = join(dir, 'aliases.mjs')
  await build({ entryPoints: [join(root, 'src/sessionAliases.ts')], outfile: aliasOut, bundle: true, format: 'esm', platform: 'node', logLevel: 'silent' })
  const { sessionAlias, setSessionAlias } = await import(pathToFileURL(aliasOut).href)
  assert.equal(sessionAlias('s1'), '手动名', '读取已有重命名')
  assert.equal(sessionAlias('bad'), undefined, '非字符串值丢弃')
  assert.equal(sessionAlias('empty'), undefined, '空白名称丢弃')
  assert.equal(sessionAlias('none'), undefined)
  setSessionAlias('s2', '  新名字  ')
  assert.equal(sessionAlias('s2'), '新名字', '重命名去首尾空白')
  assert.ok(store.get('denia.session-aliases.v1').includes('新名字'))
  setSessionAlias('s1', '')
  assert.equal(sessionAlias('s1'), undefined, '空名称恢复自动标题')

  store.set('denia.session-aliases.v1', JSON.stringify({ d1: '手动标题' }))
  const displayOut = join(dir, 'display.mjs')
  await build({ entryPoints: [join(root, 'src/sessionDisplay.ts')], outfile: displayOut, bundle: true, format: 'esm', platform: 'node', logLevel: 'silent' })
  const { sessionDisplayTitle } = await import(pathToFileURL(displayOut).href)
  assert.equal(sessionDisplayTitle({ id: 'd1', title: 'AI 标题', excerpt: '首条' }), '手动标题', '手动名优先于 AI 标题')
  assert.equal(sessionDisplayTitle({ id: 'd2', title: 'AI 标题', excerpt: '首条' }), 'AI 标题')
  assert.equal(sessionDisplayTitle({ id: 'd3', title: null, excerpt: '首条' }), '首条')
  assert.equal(sessionDisplayTitle({ id: 'd4', title: null, excerpt: null }), '新会话')
  console.log('ok session alias persistence and display precedence')
} finally { rmSync(dir, { recursive: true, force: true }) }
