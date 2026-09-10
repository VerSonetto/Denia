/**
 * fold 的回归网:系统提示词按内容去重。
 *   node scripts/check-fold.mjs
 *
 * fold.ts 依赖 types/toolDisplay,用 esbuild bundle 成单文件后 import。
 */
import { mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'

const dir = mkdtempSync(join(tmpdir(), 'fold-'))
const file = join(dir, 'fold.mjs')
await build({
  entryPoints: [fileURLToPath(new URL('../src/fold.ts', import.meta.url))],
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

const SYS_A = '你是 denia,一个 AI 编码 agent。'
const SYS_B = '你是 denia,一个 AI 编码 agent。(换了 persona)'

/** 造一条 system-prompt 事件。 */
const sp = (turn, text) => ({ seq: turn * 10, time: turn * 1000, type: 'system-prompt', turn, step: 1, text })
/** 数出 fold 结果里的 system-prompt 节点。 */
const count = (nodes) => nodes.filter((n) => n.kind === 'system-prompt').length

/* 1) 核心场景:连续多轮内容一字未变 → 只保留第一条。 */
const repeated = mod.foldEvents([sp(1, SYS_A), sp(2, SYS_A), sp(3, SYS_A), sp(4, SYS_A), sp(5, SYS_A), sp(6, SYS_A)])
check('6 条一模一样只留 1 条', count(repeated), 1)
check('保留的是第一条的全文', repeated.find((n) => n.kind === 'system-prompt').text, SYS_A)

/* 2) 内容真的变了(切模型 / 改提示词 / 换 persona)→ 必须显示,不能被吞。 */
const changed = mod.foldEvents([sp(1, SYS_A), sp(2, SYS_A), sp(3, SYS_B), sp(4, SYS_B)])
check('变化后新增一条', count(changed), 2)
check('变化后显示的是新文本', changed.filter((n) => n.kind === 'system-prompt').map((n) => n.text), [SYS_A, SYS_B])

/* 3) 变回旧文本(来回切模型)→ 也算变化,要显示。 */
const flip = mod.foldEvents([sp(1, SYS_A), sp(2, SYS_B), sp(3, SYS_A)])
check('切回旧值仍算一次变化', count(flip), 3)

/* 4) 空文本不参与去重(初始 lastSystemPrompt 是 null,不是 '')。 */
const empty = mod.foldEvents([sp(1, ''), sp(2, '')])
check('空文本首条保留、后续去重', count(empty), 1)

/* 5) 增量路径与冷启动同规则:内容未变不新增节点。 */
let live = mod.foldEvents([sp(1, SYS_A)])
live = mod.applyEnvelope(live, sp(2, SYS_A))
check('增量:相同内容不新增', count(live), 1)
live = mod.applyEnvelope(live, sp(3, SYS_B))
check('增量:变化后新增', count(live), 2)

/* 6) 切会话后增量暂存的污染回归:
      先折一个会话(SYS_A),再冷启动一个首条就是 SYS_A 的新会话。
      若模块级暂存没被冷启动重置,新会话首条会被误吞(实际应为 1 条)。 */
const other = mod.foldEvents([sp(1, SYS_B), sp(2, SYS_B)])
check('另一会话冷启动不受影响', count(other), 1)
const sameAsBefore = mod.foldEvents([sp(1, SYS_A)])
check('新会话首条与旧会话同文本也照常显示', count(sameAsBefore), 1)
// 冷启动之后继续走增量:必须基于新会话自己的基准,而不是上一个会话的。
let after = mod.applyEnvelope(sameAsBefore, sp(2, SYS_A))
check('切会话后增量从新基准继续', count(after), 1)

/* 7) context-injection 不去重(内容即使相近也是真信息,如"取代之前所有")。 */
const injection = (seq, text) => ({ seq, time: seq * 100, type: 'agent-delivery', turn: 1, step: 1, text })
const inj = mod.foldEvents([injection(1, '工作区指令:基线 A'), injection(2, '工作区指令:基线 A')])
check('context-injection 不去重', inj.filter((n) => n.kind === 'context-injection').length, 2)

console.log(failed === 0 ? '\nALL PASS' : `\n${failed} FAILED`)
process.exit(failed === 0 ? 0 : 1)
