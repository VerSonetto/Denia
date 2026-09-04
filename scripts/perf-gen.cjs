#!/usr/bin/env node
// 性能基线数据生成器:在指定 DENIA_HOME 下制造大量轻会话和一个超长会话。
// 用法:
//   node scripts/perf-gen.cjs <home> <lightCount> <longEvents> [longTargetMB]
// 例:
//   node scripts/perf-gen.cjs C:\tmp\denia-perf 10000 100000 50
'use strict'

const fs = require('node:fs')
const path = require('node:path')
const crypto = require('node:crypto')

const home = process.argv[2]
const lightCount = Number(process.argv[3] ?? 0)
const longEvents = Number(process.argv[4] ?? 0)
const longTargetMB = Number(process.argv[5] ?? 0)
const LONG_ID = 'aaaaaaaa-0000-4000-8000-000000000001'

if (!home) {
  console.error('usage: node scripts/perf-gen.cjs <home> <lightCount> <longEvents> [longTargetMB]')
  process.exit(2)
}

const sessionsRoot = path.join(home, 'sessions')
fs.mkdirSync(sessionsRoot, { recursive: true })

function json(o) {
  return JSON.stringify(o)
}

function header(id, cwd) {
  return {
    type: 'session',
    version: 0,
    id,
    created_at: Date.now() - 100000,
    cwd,
    sandbox: false,
  }
}

function writeLines(dir, lines) {
  fs.mkdirSync(dir, { recursive: true })
  fs.writeFileSync(path.join(dir, 'session.jsonl'), lines.join('\n') + '\n')
}

if (lightCount > 0) {
  const batch = []
  for (let i = 0; i < lightCount; i++) {
    const id = crypto.randomUUID()
    batch.push(
      path.join(sessionsRoot, id),
      json(header(id, '.denia-many')) + '\n' +
      json({ seq: 1, time: Date.now() - i * 1000, type: 'user-message', text: `批量会话 ${i}:待办任务描述很长的一句话,用于测试列表渲染性能与内存占用情况。`, injected: false }) + '\n',
    )
  }
  for (let i = 0; i < batch.length; i += 2) {
    const dir = batch[i]
    fs.mkdirSync(dir, { recursive: true })
    fs.writeFileSync(path.join(dir, 'session.jsonl'), batch[i + 1])
  }
  console.log(`light sessions written: ${lightCount}`)
}

if (longEvents > 0) {
  const dir = path.join(sessionsRoot, LONG_ID)
  fs.mkdirSync(dir, { recursive: true })
  const file = path.join(dir, 'session.jsonl')
  const fd = fs.openSync(file, 'w')
  let seq = 0
  fs.writeSync(fd, json(header(LONG_ID, '.denia-long')) + '\n')
  const push = (o) => {
    seq += 1
    fs.writeSync(fd, json({ seq, time: Date.now() + seq, ...o }) + '\n')
  }
  let turn = 1
  let step = 1
  // 简单但不拘泥真实轮次:每 4 条用户消息闭合一轮,工具结果带长文本凑体积。
  while (seq < longEvents) {
    const textLen = Math.max(64, Math.min(4096, Math.floor((seq * 131) % 4096)))
    const chunkText = '测'.repeat(8) + textLen.toString(36) + 'x'.repeat(textLen)
    push({ type: 'user-message', text: `长会话测试消息 ${turn}:请分析项目并总结。`, injected: false })
    push({ type: 'turn-start', turn })
    push({ type: 'step-start', turn, step })
    push({ type: 'system-prompt', turn, step, text: '你是 Denia,性能测试会话。' })
    for (let i = 0; i < 2 && seq < longEvents; i++) {
      push({ type: 'assistant-chunk', turn, step, chunk: { type: 'text-delta', index: 0, text: chunkText } })
    }
    if (seq < longEvents) {
      const blocks = [{ type: 'text', text: `第${turn}轮第${step}步的最终回答,包含一些代码示例和说明。` }]
      push({ type: 'assistant-message', turn, step, blocks })
    }
    if (step % 3 === 0 && seq < longEvents) {
      const callId = `call_${turn}_${step}`
      push({ type: 'tool-call', turn, step, call_id: callId, name: 'bash', arguments: '{"command":"echo hi"}' })
      push({ type: 'tool-result', turn, step, call_id: callId, content: 'hi from long test\n' + 'x'.repeat(2000), is_error: false })
    }
    if (seq < longEvents) {
      push({ type: 'step-end', turn, step })
    }
    if (step >= 4 || seq >= longEvents) {
      push({ type: 'turn-end', turn, reason: { kind: 'completed' } })
      turn += 1
      step = 1
    } else {
      step += 1
    }
  }
  fs.closeSync(fd)
  const stat = fs.statSync(file)
  console.log(`long session events: ${seq}`)
  console.log(`long session bytes: ${stat.size}`)
  if (longTargetMB > 0 && stat.size < longTargetMB * 1024 * 1024) {
    console.warn(`warning: generated ${(stat.size / 1024 / 1024).toFixed(1)}MB, target ${longTargetMB}MB; increase longEvents`)
  }
}
