/**
 * 一次性验证脚本:用 esbuild 把 toolDisplay.ts 转成 ESM 后跑断言。
 * 项目没有测试框架,这个脚本就是 diff 逻辑的回归网(改动后手动跑一次)。
 *   node scripts/check-tool-display.mjs
 */
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'
import { transform } from 'esbuild'

const source = await transform(
  await (await import('node:fs/promises')).readFile(
    new URL('../src/toolDisplay.ts', import.meta.url),
    'utf8',
  ),
  { loader: 'ts', format: 'esm', target: 'node20' },
)
const dir = mkdtempSync(join(tmpdir(), 'tooldisplay-'))
const file = join(dir, 'toolDisplay.mjs')
await (await import('node:fs/promises')).writeFile(file, source.code, 'utf8')
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

const fmt = (diff) =>
  diff.lines.map((l) => `${l.kind}:${l.text}:${l.oldNo ?? ''}/${l.newNo ?? ''}`)

/* 1) 参数里的 old_string 含 `}` → 摘要必须是文件名,不是代码片段。 */
const args = JSON.stringify({
  path: 'web/src/toolDisplay.ts',
  old_string: 'function f() {\n  return { a: 1 }\n}',
  new_string: 'function f() {\n  return { a: 2 }\n}',
})
check('summary 取文件名', mod.toolCallSummary('edit', args), '…/src/toolDisplay.ts')

/* 2) 流式半截 JSON:path 抢救仍生效。 */
check(
  '半截 JSON 抢救 path',
  mod.toolCallSummary('edit', '{"path":"web/src/a.ts","old_string":"const a"},'),
  '…/src/a.ts',
)

/* 3) 单行替换:一删一加,行号正确。 */
const d = mod.editDiff('edit', args)
check('diff removed/added', [d.removed, d.added], [1, 1])
check('diff path', d.path, '…/src/toolDisplay.ts')
check('diff 行序列', fmt(d), [
  'context:function f() {:1/1',
  'del:  return { a: 1 }:2/',
  'add:  return { a: 2 }:/2',
  'context:}:3/3',
])

/* 4) 带起始行号:startLine 透传到行号列。 */
const at10 = mod.editDiff('edit', args, 10)
check('startLine 透传', fmt(at10)[0], 'context:function f() {:10/10')

/* 5) 纯新增(不删行)。 */
const add = mod.editDiff(
  'edit',
  JSON.stringify({ path: 'a.ts', old_string: 'a\nb', new_string: 'a\nb\nc' }),
)
check('纯新增', [add.removed, add.added], [0, 1])

/* 6) 多行改写:LCS 找回未变行,不整段推倒。 */
const lcs = mod.editDiff(
  'edit',
  JSON.stringify({
    path: 'a.ts',
    old_string: 'a\nb\nc\nd',
    new_string: 'a\nB\nc\nd',
  }),
)
check('LCS 只标真变的行', [lcs.removed, lcs.added], [1, 1])
check('LCS 行序列', fmt(lcs), [
  'context:a:1/1',
  'del:b:2/',
  'add:B:/2',
  'context:c:3/3',
  'context:d:4/4',
])

/* 7) 超长 diff 折叠:头尾各 3 行 + 一行省略占位。 */
const long = (i) => Array.from({ length: 60 }, (_, k) => (k === 30 ? 'LINE30' : `line${k}`)).join('\n')
const folded = mod.editDiff(
  'edit',
  JSON.stringify({ path: 'a.ts', old_string: Array.from({ length: 60 }, (_, k) => `line${k}`).join('\n'), new_string: long() }),
)
check('折叠后行数', folded.lines.length, 7)
check('折叠占位行', folded.lines[3].fold, true)
check('折叠计数', folded.skipped, 60 - 6 + 1) // 含新增的一行,diff 总长 61

/* 8) 非 edit 工具不产 diff;缺字段返回 null。 */
check('非 edit 返回 null', mod.editDiff('bash', args), null)
check('缺 new_string 返回 null', mod.editDiff('edit', JSON.stringify({ path: 'a.ts', old_string: 'x' })), null)

/* 9) write_file:整文件覆盖写 = 全量新增(旧内容不随参数给出)。 */
const write = mod.writeDiff(
  'write_file',
  JSON.stringify({ path: 'web/src/new.ts', content: 'export const a = 1\nexport const b = 2\n' }),
)
check('write path', write.path, '…/src/new.ts')
check('write 全量新增', [write.removed, write.added], [0, 2])
check('write 行号从 1 起', fmt(write), [
  'add:export const a = 1:/1',
  'add:export const b = 2:/2',
])

/* 10) write_file 拿到写入前内容时:按行号对齐算增删。 */
const before = 'a\nb\nc'
const after = 'a\nB\nc\nd'
const overwrite = mod.writeDiff(
  'write_file',
  JSON.stringify({ path: 'a.ts', content: after }),
  before,
)
check('write 对齐后增删', [overwrite.removed, overwrite.added], [1, 2])
check('write 对齐行序列', fmt(overwrite), [
  'context:a:1/1',
  'del:b:2/',
  'add:B:/2',
  'context:c:3/3',
  'add:d:/4',
])
check('write 空内容', mod.writeDiff('write_file', JSON.stringify({ path: 'a.ts', content: '' })).added, 0)
check('write 非 write 工具', mod.writeDiff('edit', args), null)

console.log(failed === 0 ? '\nALL PASS' : `\n${failed} FAILED`)
process.exit(failed === 0 ? 0 : 1)
