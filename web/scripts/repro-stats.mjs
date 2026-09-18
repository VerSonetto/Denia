// 临时复现:真实快照跑 stats.ts 的 compute,找白屏崩溃点。
import { readFileSync } from 'node:fs'
import * as stats from '../src/stats.ts'
import * as trajectory from '../src/trajectory.ts'

const file = process.argv[2]
const raw = JSON.parse(readFileSync(file, 'utf8'))
const events = raw.events

console.log('exports stats:', Object.keys(stats))
for (const [name, fn] of Object.entries(stats)) {
  if (typeof fn !== 'function') continue
  try {
    fn(events)
    console.log('ok  :', name)
  } catch (e) {
    console.error('THREW:', name, '->', e && e.message)
  }
}
console.log('exports trajectory:', Object.keys(trajectory))
for (const [name, fn] of Object.entries(trajectory)) {
  if (typeof fn !== 'function' || name === 'formatSelfDuration') continue
  try {
    if (name === 'quoteTrajectoryRecord' || name === 'quoteTrajectoryInterval') continue
    fn(events)
    console.log('ok  :', name)
  } catch (e) {
    console.error('THREW:', name, '->', e && e.message)
  }
}