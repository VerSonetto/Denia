// 临时复现:真实快照跑 web/src/lib/exportSession.ts 的导出逻辑(先探测导出形状)。
import { readFileSync } from 'node:fs'
const mod = await import('../src/lib/exportSession.ts')
console.log('exports:', Object.keys(mod))
const file = process.argv[2]
const raw = JSON.parse(readFileSync(file, 'utf8'))
const events = raw.events
for (const [name, fn] of Object.entries(mod)) {
  if (typeof fn !== 'function') continue
  try {
    const out = fn('c41e7b39-871a-4f04-94d6-29d304821887', events)
    console.log('ok  :', name, '->', typeof out, out instanceof Promise ? '(promise)' : '')
  } catch (e) {
    console.error('THREW:', name, '->', e && e.message, '\n', e && e.stack)
  }
}