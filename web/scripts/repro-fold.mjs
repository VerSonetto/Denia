// 临时复现脚本:用真实会话快照跑 foldEvents + groupTranscript,找白屏崩溃点。
import { readFileSync } from 'node:fs'
import { foldEvents, groupTranscript } from '../src/fold.ts'

const file = process.argv[2]
const raw = JSON.parse(readFileSync(file, 'utf8'))
const events = raw.events
console.log('events:', events.length)

let nodes
try {
  nodes = foldEvents(events)
  console.log('foldEvents ok, nodes:', nodes.length)
} catch (e) {
  console.error('FOLD THREW:', e && e.stack)
  process.exit(1)
}

try {
  const rows = groupTranscript(nodes)
  console.log('groupTranscript ok, rows:', rows.length)
} catch (e) {
  console.error('GROUP THREW:', e && e.stack)
  process.exit(1)
}