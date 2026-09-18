/**
 * slash 触发词法的回归网:`/` 与 `@` 必须同规则(只在行首或空白后触发)。
 *   node scripts/check-slash.mjs
 */
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'

const dir = mkdtempSync(join(tmpdir(), 'slash-'))
const file = join(dir, 'slash.mjs')
await build({
  entryPoints: [fileURLToPath(new URL('../src/pages/slash.ts', import.meta.url))],
  outfile: file,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  logLevel: 'silent',
})
const mod = await import(pathToFileURL(file).href)

let failed = 0
function check(label, actual, expected) {
  const a = JSON.stringify(actual)
  const e = JSON.stringify(expected)
  if (a !== e) {
    failed++
    console.log(`FAIL ${label}\n  actual   ${a}\n  expected ${e}`)
  } else {
    console.log(`ok   ${label}`)
  }
}

/** 光标在末尾时的活跃 token 名(无则 null)。 */
const at = (text) => mod.activeSlashToken(text, text.length)?.query ?? null

/* 1) 应触发:行首、空白之后(含换行,与 @ 一致)。 */
check('行首裸 /', at('/'), '')
check('行首 /compact', at('/compact'), 'compact')
check('空格后', at('帮我 /compact'), 'compact')
check('换行后', at('帮我\n/compact'), 'compact')
check('制表符后', at('帮我\t/compact'), 'compact')

/* 2) 不应触发:汉字后随手敲的 / —— 本次修复的核心(与 @ 对齐)。 */
check('汉字后不触发', at('修复/compact'), null)
check('汉字后裸 / 不触发', at('修复/'), null)
check('看/plan 不触发', at('看/plan'), null)

/* 3) 不应触发:URL / 路径 / 分数里的 / 。 */
check('https: 单斜杠不触发', at('https:/'), null)
check('URL 双斜杠不触发', at('https://'), null)
check('路径 a/b 不触发', at('a/b'), null)
check('分数 1/2 不触发', at('1/2'), null)

/* 4) 全角标点后同样不触发(原 CJK 类里含 \\uff00-\\uffef)。 */
check('全角逗号后不触发', at('你好，/compact'), null)

/* 5) 前缀整段可替换:选中候选时要把 `/query` 一起换掉。 */
const token = mod.activeSlashToken('帮我 /com', 9)
check('前缀含斜杠', token?.prefix, '/com')
check('查询串', token?.query, 'com')

/* 6) 全文扫描:命中词典才成卡片,且不受汉字前缀影响(扫描是独立的
       greedy 词法,已存在的卡片仍能正确重建)。 */
const segs = mod.scanSlashTokens('帮我 /compact 一下', ['compact'])
check('扫描命中 compact', segs.filter((s) => s.name).map((s) => s.name), ['compact'])
check('未命中的 /word 保持文本', mod.scanSlashTokens('/nope', ['compact']).filter((s) => s.name).length, 0)

console.log(failed === 0 ? '\nALL PASS' : `\n${failed} FAILED`)
process.exit(failed === 0 ? 0 : 1)
