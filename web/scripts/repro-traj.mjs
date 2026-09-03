// 临时复现:真实快照跑 deriveTrajectory,找白屏崩溃点。
import { readFileSync } from 'node:fs'
import { deriveTrajectory } from '../src/trajectory.ts'

const file = process.argv[2]
const raw = JSON.parse(readFileSync(file, 'utf8'))
const events = raw.events
console.log('events:', events.length)
try {
  const traj = deriveTrajectory(events)
  console.log('deriveTrajectory ok, records:', traj.records?.length ?? JSON.stringify(Object.keys(traj)))
} catch (e) {
  console.error('TRAJECTORY THREW:', e && e.stack)
  process.exit(1)
}