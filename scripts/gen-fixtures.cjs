// 造性能测试数据:1 个长会话(~3000 事件)+ N 个轻会话
const fs = require('node:fs')
const path = require('node:path')
const crypto = require('node:crypto')

const root = path.join(process.env.USERPROFILE, '.dsh-rs', 'sessions')
const LONG_ID = 'aaaaaaaa-0000-4000-8000-000000000001'
const NWORK = Number(process.argv[2] ?? 120)

function json(o) { return JSON.stringify(o) }

function writeSession(dir, lines) {
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(path.join(dir, 'session.jsonl'), lines.join('\n') + '\n')
}

function header(id, cwd) {
  return { type: 'session', version: 0, id, created_at: Date.now() - 100000, cwd, sandbox: false }
}

// 长会话:30 个 turn × (user + turn-start + 4 step × 15 chunk + …) ≈ 5000 事件
{
  const lines = [json(header(LONG_ID, '.denia-long'))]
  let seq = 1
  const push = (o) => lines.push(json({ seq: seq++, time: Date.now() + seq, ...o }))
  for (let turn = 1; turn <= 30; turn++) {
    push({ type: 'user-message', text: `长会话测试消息 ${turn}: 请分析项目并总结。`, injected: false })
    push({ type: 'turn-start', turn })
    for (let step = 1; step <= 4; step++) {
      push({ type: 'step-start', turn, step })
      push({ type: 'system-prompt', turn, step, text: '你是 Denia,性能测试会话。' })
      for (let i = 0; i < 15; i++) {
        push({ type: 'assistant-chunk', turn, step, chunk: { type: 'text-delta', index: 0, text: `第${turn}轮第${step}步的第${i}个流式token片段……` } })
      }
      const blocks = [{ type: 'text', text: `第${turn}轮第${step}步的最终回答,包含一些代码示例和说明。` }]
      if (step < 4) blocks.push({ type: 'tool-call', id: `call_${turn}_${step}`, name: 'bash', arguments: '{"command":"echo hi"}' })
      push({ type: 'assistant-message', turn, step, blocks })
      if (step < 4) {
        push({ type: 'tool-call', turn, step, call_id: `call_${turn}_${step}`, name: 'bash', arguments: '{"command":"echo hi"}' })
        push({ type: 'tool-result', turn, step, call_id: `call_${turn}_${step}`, content: 'hi from long test\n' + 'x'.repeat(2000), is_error: false })
      }
      push({ type: 'step-end', turn, step })
    }
    push({ type: 'turn-end', turn, reason: { kind: 'completed' } })
  }
  writeSession(path.join(root, LONG_ID), lines)
  console.log('long session events:', seq - 1)
}

// 轻会话:header + 一条用户消息
for (let i = 0; i < NWORK; i++) {
  const id = crypto.randomUUID()
  const lines = [
    json(header(id, '.denia-many')),
    json({ seq: 1, time: Date.now() - i * 1000, type: 'user-message', text: `批量会话 ${i}:待办任务描述很长的一句话,用于测试列表渲染性能与内存占用情况。`, injected: false }),
  ]
  writeSession(path.join(root, id), lines)
}
console.log('light sessions written:', NWORK)
