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

/* 7) 压缩占位行:不来自事件流,由前端挂载/移除,且不污染 fold 结果。 */
const base = mod.foldEvents([sp(1, SYS_A)])
const withRow = mod.withCompacting(base, 1000)
check('挂上压缩中行', withRow.filter((n) => n.kind === 'compacting').length, 1)
check('压缩中行在末尾', withRow[withRow.length - 1].kind, 'compacting')
// 幂等:连点两次不能出现两行。
const twice = mod.withCompacting(withRow, 2000)
check('重复挂载不产生第二行', twice.filter((n) => n.kind === 'compacting').length, 1)
check('重复挂载保留首次时刻', twice.find((n) => n.kind === 'compacting').startedAt, 1000)
check('移除压缩中行', mod.withoutCompacting(withRow).filter((n) => n.kind === 'compacting').length, 0)
// 移除时不能误伤其它节点(尤其刚到达的 compaction 摘要)。
check('移除后原节点仍在', mod.withoutCompacting(withRow).length, base.length)
check('无压缩行时移除是幂等的', mod.withoutCompacting(base).length, base.length)
// 压缩中行不能被 turn 折叠吞掉:它在最后一个 turn-end 之后,应独立成行。
const rowsWithCompacting = mod.groupTranscript(withRow)
const compactingRows = rowsWithCompacting.filter((r) => r.kind === 'node' && r.node.kind === 'compacting')
check('压缩中行不被折叠吞掉', compactingRows.length, 1)

/* 8) context-injection 不去重(内容即使相近也是真信息,如"取代之前所有")。 */
const injection = (seq, text) => ({ seq, time: seq * 100, type: 'agent-delivery', turn: 1, step: 1, text })
const inj = mod.foldEvents([injection(1, '工作区指令:基线 A'), injection(2, '工作区指令:基线 A')])
check('context-injection 不去重', inj.filter((n) => n.kind === 'context-injection').length, 2)

/* 9) turn-end 用量:冷启动与增量两条路径必须一致。
      增量路径曾经只累加 input/output,把缓存读与推理丢了 —— 表现为
      "刷新后能看到缓存命中,实时流跑完却看不到",本轮用量卡片会照出这个洞。 */
const usageEvents = [
  { seq: 1, time: 1000, type: 'turn-start', turn: 1 },
  {
    seq: 2,
    time: 1100,
    type: 'assistant-message',
    turn: 1,
    step: 1,
    blocks: [{ type: 'text', text: 'hi' }],
    usage: { inputTokens: 100, outputTokens: 20, cacheReadTokens: 900, reasoningTokens: 7 },
  },
  { seq: 3, time: 2000, type: 'turn-end', turn: 1, reason: { kind: 'completed' } },
]
const coldUsage = mod.foldEvents(usageEvents).find((n) => n.kind === 'turn-end').usage
check('冷启动:缓存读进 turn-end', coldUsage.cacheReadTokens, 900)
check('冷启动:推理进 turn-end', coldUsage.reasoningTokens, 7)

const incrUsage = mod
  .applyEnvelopes(mod.foldEvents([usageEvents[0]]), usageEvents.slice(1))
  .find((n) => n.kind === 'turn-end').usage
check('增量:缓存读进 turn-end', incrUsage.cacheReadTokens, 900)
check('增量:推理进 turn-end', incrUsage.reasoningTokens, 7)
check('两条路径的 turn-end 用量逐字一致', incrUsage, coldUsage)

/* 10) 缓存命中率分母回归:`TokenUsage` 契约里 cacheRead 与 inputTokens 互斥
       (inputTokens 只含未缓存部分),所以分母是两者之和,命中率 = 命中/总prompt。

       这里用一个真实会话的两组数当夹具,注意它们的**口径**:
       - (未缓存 12429, 缓存 9088):命中率 42.24%
       - (未缓存 1996, 缓存 13454):命中率 87.08%  ← 健康前缀缓存的典型形状
       第二组是刻意加的:健康会话里命中率应当很高(九成上下),而不是四成。
       如果哪天 wire 层又把含缓存的总 prompt 塞进 inputTokens,第二组会掉到
       45% 左右(分母翻倍),这条用例就会失败 —— 它是那个 bug 的哨兵。 */
const statsFile = join(dir, 'stats.mjs')
await build({
  entryPoints: [fileURLToPath(new URL('../src/stats.ts', import.meta.url))],
  outfile: statsFile,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  logLevel: 'silent',
})
const stats = await import(pathToFileURL(statsFile).href)

check('计费输入 = 未缓存 + 缓存读', stats.billedInputTokens(12429, 9088), 21517)
check('缓存命中率以计费输入为分母', stats.cacheHitPercent(12429, 9088), '42.24')
check(
  '健康会话命中率应在九成上下(口径哨兵)',
  stats.cacheHitPercent(1996, 13454),
  '87.08',
)
check('全命中为 100%', stats.cacheHitPercent(0, 500), '100.00')
check('无缓存为 0%', stats.cacheHitPercent(500, 0), '0.00')
check('无计费输入返回 null', stats.cacheHitPercent(0, 0), null)

console.log(failed === 0 ? '\nALL PASS' : `\n${failed} FAILED`)
process.exit(failed === 0 ? 0 : 1)
