// 复现:重试导致的重复 block-start 产生"空思考块"UI bug。
// 现场来自真实会话:seq 3773 传输错误 → seq 3774 retry-attempt →
// seq 3775 模型重发 block-start(index 仍为 0),前端却多出一个空思考块。
import { foldEvents, applyEnvelope } from '../src/fold.ts'

// 模拟"思考中重试"的 chunk 序列:第一次尝试 delta 若干 → 重试 →
// 第二次尝试重发 block-start(index=0)+ 从头输出 delta。
const events = [
  { seq: 1, time: 1, type: 'turn-start', turn: 1 },
  { seq: 2, time: 2, type: 'step-start', turn: 1, step: 1 },
  { seq: 3, time: 3, type: 'assistant-chunk', turn: 1, step: 1,
    chunk: { type: 'block-start', index: 0, block_type: 'reasoning' } },
  { seq: 4, time: 4, type: 'assistant-chunk', turn: 1, step: 1,
    chunk: { type: 'reasoning-delta', index: 0, text: '第一次尝试的思考' } },
  // 重试后:模型从头重新生成,再次 block-start(index 仍是 0)
  { seq: 5, time: 5, type: 'assistant-chunk', turn: 1, step: 1,
    chunk: { type: 'block-start', index: 0, block_type: 'reasoning' } },
  { seq: 6, time: 6, type: 'assistant-chunk', turn: 1, step: 1,
    chunk: { type: 'reasoning-delta', index: 0, text: '重试后的思考' } },
]

const report = (label, nodes) => {
  const a = nodes.find((n) => n.kind === 'assistant')
  const reasoning = a.blocks.filter((b) => b.kind === 'reasoning')
  const empty = reasoning.filter((b) => b.text === '')
  console.log(`[${label}] blocks=${a.blocks.length} reasoning=${reasoning.length} 空思考块=${empty.length}`)
  console.log(`  内容: ${JSON.stringify(reasoning.map((b) => b.text))}`)
  return empty.length
}

// 完整 fold(冷加载路径)。
const emptyFold = report('foldEvents', foldEvents(events))

// 增量 fold(实时流路径)。
let inc = []
for (const ev of events) inc = applyEnvelope(inc, ev)
const emptyInc = report('applyEnvelope', inc)

// 判定:任何一条路径出现空思考块即 bug。
if (emptyFold > 0 || emptyInc > 0) {
  console.log('FAIL: 存在空思考块(bug 复现)')
  process.exit(1)
}
console.log('PASS: 无空思考块')
