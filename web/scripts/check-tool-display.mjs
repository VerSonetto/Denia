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

/* ---- todo_write ---- */

const todos = (items) => JSON.stringify({ todos: items })
const t1 = [
  { content: '解析参数', status: 'pending' },
  { content: '写 diff', status: 'pending' },
]
check('todo 头部是进度不是 JSON', mod.toolCallSummary('todo_write', todos(t1)), '0/2')

check(
  'todo 全部完成时头部',
  mod.toolCallSummary(
    'todo_write',
    todos([
      { content: 'a', status: 'completed' },
      { content: 'b', status: 'completed' },
    ]),
  ),
  // 全部完成不再挂对勾:2/2 本身已经说明一切。
  '2/2',
)

// 首次清单:没有上一次快照,不该把整表标成新增。
const first = mod.todoSnapshot(todos(t1))
check('首次清单无变化项', first.changes.size, 0)
check('首次清单未变计数', first.unchanged, 2)
check('首次清单总数', [first.done, first.total], [0, 2])

// 第二次:一条完成、一条开做、一条新增 → 三条都被标出(未变项此时为 0)。
const second = mod.todoSnapshot(
  todos([
    { content: '解析参数', status: 'completed' },
    { content: '写 diff', status: 'in_progress' },
    { content: '补测试', status: 'pending' },
  ]),
  t1,
)
check('第二次标出 3 条变化', second.changes.size, 3)
check('第二次未变计数', second.unchanged, 0)
check('完成的条目 → done', second.changes.get('解析参数'), 'done')
check('开做的条目 → started', second.changes.get('写 diff'), 'started')
check('新增条目 → new', second.changes.get('补测试'), 'new')
check('进度', [second.done, second.total], [1, 3])

// 回退重做:completed → in_progress。
const reopened = mod.todoSnapshot(
  todos([{ content: '解析参数', status: 'in_progress' }]),
  [{ content: '解析参数', status: 'completed' }],
)
check('回退重做 → reopened', reopened.changes.get('解析参数'), 'reopened')

// 状态没动 → 不计入变化。
const same = mod.todoSnapshot(todos(t1), t1)
check('状态未动无变化', same.changes.size, 0)

// 半截 JSON:卡片仍应抢救出已收到的条目。
const partial = mod.todoSnapshot(
  '{"todos":[{"content":"解析参数","status":"completed"},{"content":"写 dif',
)
check('半截 JSON 抢救条目', partial?.total, 1)
check('半截 JSON 抢救状态', partial?.todos[0].status, 'completed')

// 长清单:只动一条时,其余压成 unchanged(卡片里"另有 N 项未变")。
const many = Array.from({ length: 8 }, (_, i) => ({ content: `task${i}`, status: 'pending' }))
const manyNext = many.map((item, i) =>
  i === 3 ? { content: `task${i}`, status: 'in_progress' } : item,
)
const longList = mod.todoSnapshot(todos(manyNext), many)
check('长清单只标 1 条', longList.changes.size, 1)
check('长清单未变计数', longList.unchanged, 7)

check('todo 非 todo 工具', mod.todoSnapshot('{"x":1}', t1)?.total ?? null, null)

/* ---- bash:ANSI 清洗与终端噪声 ---- */

// git --color=always 的真实形态:ESC[33m + ESC[m。
check(
  '剥 SGR 彩色',
  mod.stripAnsi('[33m74fd909[m feat(web): todo'),
  '74fd909 feat(web): todo',
)
// 多属性序列(粗体+前景+背景)与 256 色。
check(
  '剥多属性与 256 色',
  mod.stripAnsi('[1;38;5;208m警告[0m [38;2;255;0;0m红[0m'),
  '警告 红',
)
// 光标控制/清行/清屏:进度条与全屏 TUI 会大量产生。输入是 a+[2K+b+[H+b+[J+c
// → 序列剥掉后剩 a、b、b、c(两个 b 都在,序列只是光标动作)。
check('剥光标与清屏序列', mod.stripAnsi('a[2Kb[Hb[Jc'), 'abbc')
// OSC 标题(设置窗口标题)与字符集切换。
check('剥 OSC 与字符集', mod.stripAnsi(']0;titlex(B'), 'x')
// 半截/孤立 ESC:捕获截断时常见,不能留可见垃圾。
check('剥孤立 ESC', mod.stripAnsi('ab'), 'ab')
check('无序列时原样', mod.stripAnsi('plain 中文'), 'plain 中文')

// CRLF 归一。
check('CRLF 归一', mod.normalizeTerminalText('a\r\nb\r\nc'), 'a\nb\nc')
// 进度条:同一行被 \r 反复覆盖 → 只留最终一段(屏幕上留下的那一行)。
check(
  '回车覆盖取最后一段',
  mod.normalizeTerminalText('  0%\r 50%\r100%\ndone'),
  '100%\ndone',
)
// 组合拳:彩色 + CRLF + 覆盖。
check(
  '清洗组合',
  mod.cleanToolOutput('[32m 10%\r[32m100%[0m\r\n[31mfail[0m\r\n'),
  '100%\nfail\n',
)

// 命令正文:剥后端注入的输出编码前缀。
check(
  '剥输出编码前缀',
  mod.cleanBashCommand(
    '[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; git status',
  ),
  'git status',
)
check('无前缀时原样', mod.cleanBashCommand('cargo test'), 'cargo test')

// 结果切段:首行的"退出码: N"由头部徽标呈现,正文里不再重复。
const parts = mod.bashOutputParts('退出码: 0\nline 1\nline 2\n')
check('剥首行退出码', parts.stdout, 'line 1\nline 2')
check('无 stderr 段', parts.stderr, undefined)

const withErr = mod.bashOutputParts(
  '退出码: 1\nstdout line\n--- stderr ---\nerror: boom\n',
)
check('切出 stdout', withErr.stdout, 'stdout line')
check('切出 stderr', withErr.stderr, 'error: boom')
check('无 stderr 分隔时不误切', mod.bashOutputParts('a\n--- stderr\nb').stderr, undefined)

console.log(failed === 0 ? '\nALL PASS' : `\n${failed} FAILED`)
process.exit(failed === 0 ? 0 : 1)
