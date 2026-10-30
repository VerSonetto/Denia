/**
 * 跟随引擎回归网:弹簧物理、预测预留、闭环背压,以及它们在"内容持续增长"这个
 * 真实前提下的行为。
 *
 *   node scripts/check-follow.mjs
 *
 * jsdom 没有布局引擎(scrollHeight 恒 0、scrollTop 恒 0),这里注入可控几何并按
 * 真实帧序列驱动,把几条不能退化的性质钉死:
 *   1) 折行的一跳不平移一帧 —— 单帧位移远小于整跳,几百毫秒内收敛到底;
 *   2) 掉帧自愈 —— 主线程停 300ms 后不在恢复帧里瞬移;
 *   3) 归属只由读者的滚动决定 —— 上翻脱跟后内容再长也不抢位置,回到底部才恢复;
 *   4) 预测预留 —— 流式期稳态停在离底 reserve px,生产端停手后收回;
 *   5) 闭环背压 —— 滞后占满容量时下发 0.55 的减速系数。
 */
import { mkdirSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const webRoot = join(here, '..')

let failed = 0
const ok = (label) => console.log(`ok   ${label}`)
const fail = (label, detail) => {
  failed++
  console.log(`FAIL ${label}${detail ? `\n     ${detail}` : ''}`)
}
const near = (a, b, tol = 0.51) => Math.abs(a - b) <= tol

const { build } = await import('esbuild')
const outDir = join(webRoot, 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const asUrl = (path) => path.replace(/\\/g, '/')
const entry = join(outDir, 'follow-entry.ts')
writeFileSync(entry, [
  `export * from ${JSON.stringify(asUrl(join(webRoot, 'src', 'streaming', 'useFollow.ts')))}`,
  `export * from ${JSON.stringify(asUrl(join(webRoot, 'src', 'streaming', 'physics.ts')))}`,
  '',
].join('\n'))
const bundle = join(outDir, 'follow.mjs')

await build({
  entryPoints: [entry],
  outfile: bundle,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  jsx: 'automatic',
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  logLevel: 'silent',
})

const { JSDOM } = await import('jsdom')
const dom = new JSDOM('<!doctype html><html><body></body></html>', {
  url: 'http://localhost/',
  pretendToBeVisual: true,
})
const g = dom.window
for (const key of [
  'window', 'document', 'navigator', 'location', 'history', 'HTMLElement', 'Element', 'Node',
  'Event', 'CustomEvent', 'MouseEvent', 'KeyboardEvent', 'EventTarget', 'MutationObserver',
  'getComputedStyle', 'requestAnimationFrame', 'cancelAnimationFrame', 'matchMedia',
  'localStorage', 'sessionStorage', 'DOMParser', 'Blob', 'URL', 'CSS', 'SVGSVGElement',
  'ResizeObserver', 'WheelEvent',
]) {
  try {
    globalThis[key] = g[key]
  } catch {
    /* 只读全局跳过 */
  }
}

// 帧回调走 Node 的定时器 + Node 的 performance.now():两者同域,也与 hook 内
// 读到的时钟一致(真实浏览器里 rAF 时间戳本来就属于 performance.now 的时间轴;
// jsdom 的 rAF 用的是建窗后从 0 起算的窗口时钟,和 Node 的进程时钟对不上)。
globalThis.requestAnimationFrame = (callback) => setTimeout(() => callback(performance.now()), 16)
globalThis.cancelAnimationFrame = (id) => clearTimeout(id)

const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')
const physics = await import(pathToFileURL(bundle).href)
const { useFollow } = physics
const H = React.createElement

/* ---------- 纯物理 ---------- */

{
  const { springStep } = physics
  // 折行一跳 28px:逐帧推进,单帧远小于整跳,累计收敛。
  let lag = 28
  let velocity = 0
  let moved = 0
  let biggestFrame = 0
  let monotone = true
  let previous = 0
  for (let frame = 0; frame < 60; frame += 1) {
    const step = springStep(16, lag, velocity)
    if (step.advancePx > biggestFrame) biggestFrame = step.advancePx
    if (step.advancePx < -1e-9) monotone = false
    lag -= step.advancePx
    moved += step.advancePx
    velocity = step.velocityPxPerSec
    if (moved < previous - 1e-9) monotone = false
    previous = moved
  }
  if (near(moved, 28, 0.6) && biggestFrame <= 8 && monotone) {
    ok(`28px 折行逐帧滑入:单帧最大 ${biggestFrame.toFixed(2)}px,60 帧收敛`)
  } else {
    fail('折行未按弹簧滑入', `moved=${moved.toFixed(2)} biggestFrame=${biggestFrame.toFixed(2)} monotone=${monotone}`)
  }
  if (springStep(0, 28).advancePx === 0 && springStep(16, 0).advancePx === 0) {
    ok('无位移/无时间时不推进')
  } else {
    fail('零位移或零时间仍在推进')
  }
  // 掉帧自愈:300ms 的间隔按 32ms 计,不把攒下的位移一次补完。
  if (near(springStep(300, 600).advancePx, springStep(32, 600).advancePx, 0.001)) {
    ok('掉帧后按 32ms 计物理时间,不在恢复帧瞬移')
  } else {
    fail('掉帧未被钳制', `${springStep(300, 600).advancePx} vs ${springStep(32, 600).advancePx}`)
  }
}

{
  const { reserveTargetFor, easeReserve } = physics
  const idle = reserveTargetFor(0)
  const slow = reserveTargetFor(120)
  const fast = reserveTargetFor(600)
  const beyond = reserveTargetFor(5000)
  if (idle === 0 && slow > 0 && fast > slow && near(fast, 56) && beyond === fast) {
    ok(`预测预留随揭示速率开合(120cps=${slow.toFixed(1)}px,600cps=${fast.toFixed(1)}px)`)
  } else {
    fail('预测预留曲线不对', `${idle} ${slow} ${fast} ${beyond}`)
  }
  const eased = easeReserve(0, 56, 16)
  if (eased > 0 && eased < 56) ok('预留按时间常数开合,不跳变')
  else fail('预留开合跳变', String(eased))
}

{
  const { revealScaleFor } = physics
  const free = revealScaleFor(0, 82)
  const mid = revealScaleFor(41, 82)
  const full = revealScaleFor(82, 82)
  if (free === 1 && mid < 1 && mid > 0.55 && near(full, 0.55) && revealScaleFor(0, 0) === 1) {
    ok(`闭环背压 1 → 0.55(半满 ${mid.toFixed(2)})`)
  } else {
    fail('背压曲线不对', `${free} ${mid} ${full}`)
  }
}

{
  const { queueRevealStep, drainCpsFor } = physics
  const step = queueRevealStep(500, 32, 0, 1)
  const scaled = queueRevealStep(500, 32, 0, 0.55)
  if (step.chars > scaled.chars && step.chars <= 500 && step.speedCps <= 900) {
    ok(`字符积分:背压 0.55 时揭示量减少(${step.chars} → ${scaled.chars})`)
  } else {
    fail('背压未作用到揭示量', `${step.chars} / ${scaled.chars}`)
  }
  // 小数债务:每帧都不满一个字时,若干帧后必须凑出字来,不能永远吐 0。
  let debt = 0
  let total = 0
  for (let frame = 0; frame < 5; frame += 1) {
    const next = queueRevealStep(3, 4, debt, 1)
    debt = next.debt
    total += next.chars
  }
  if (total >= 1) ok('小数债务不丢字(慢速也能逐字推进)')
  else fail('小数债务丢失,慢速永远不出字', String(total))
  const drain = drainCpsFor(60)
  if (drain >= 200 && drain <= 800) ok(`收尾排空速度有界(${drain.toFixed(0)} cps)`)
  else fail('收排速度越界', String(drain))
}

/* ---------- DOM:注入几何 ---------- */

function defineGeometry(el) {
  let scrollHeight = 1000
  let clientHeight = 400
  let scrollTop = 0
  Object.defineProperties(el, {
    scrollHeight: { get: () => scrollHeight, configurable: true },
    clientHeight: { get: () => clientHeight, configurable: true },
    scrollTop: {
      get: () => scrollTop,
      set: (value) => {
        scrollTop = Math.max(0, Math.min(value, Math.max(0, scrollHeight - clientHeight)))
      },
      configurable: true,
    },
  })
  return {
    setGeometry: (next) => {
      if (next.scrollHeight !== undefined) scrollHeight = next.scrollHeight
      if (next.clientHeight !== undefined) clientHeight = next.clientHeight
    },
    get scrollTop() {
      return scrollTop
    },
    get floor() {
      return Math.max(0, scrollHeight - clientHeight)
    },
    /** 读者拖动:先落盘位置再派发 scroll(与浏览器一致)。 */
    readerScrollTo(value) {
      el.scrollTop = value
      el.dispatchEvent(new g.Event('scroll'))
    },
    wheel(deltaY) {
      el.dispatchEvent(new g.WheelEvent('wheel', { deltaY, bubbles: true, cancelable: true }))
    },
    grow(to) {
      scrollHeight = to
    },
  }
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0))
/** 推进若干真实帧(jsdom 的 rAF 由 16ms 定时器驱动)。 */
const frames = (count) => new Promise((resolve) => setTimeout(resolve, count * 16 + 8))

/** 渲染只挂容器的 harness,返回几何控制器与 hook 句柄。 */
async function mount(options) {
  const box = { handle: null, geometry: null }
  function Harness() {
    const handle = useFollow({ current: null }, { active: false, ...options })
    box.handle = handle
    // 与真实用法一致:ref 回调标识必须稳定,否则 React 每次渲染都重挂一次
    // 元素,那会把读者的位置当成"新容器"重置掉。
    const attachRef = React.useCallback(
      (el) => {
        if (el !== null && box.geometry === null) box.geometry = defineGeometry(el)
        handle.setPort(el)
      },
      [handle.setPort],
    )
    return H('div', { ref: attachRef })
  }
  const host = g.document.createElement('div')
  g.document.body.appendChild(host)
  const root = createRoot(host)
  root.render(H(Harness))
  await settle()
  return { scroller: box.geometry, handle: () => box.handle, root }
}

/* ---------- DOM 用例 ---------- */

// 1) 回底按钮 / 发送消息:一步落到底部,不播动画。
{
  const { scroller, handle } = await mount()
  handle().snapToBottom()
  if (near(scroller.scrollTop, scroller.floor)) ok('回底:一步对齐到底')
  else fail('回底没落到底', `scrollTop=${scroller.scrollTop} floor=${scroller.floor}`)
}

// 2) 【核心】折行的一跳不平移一帧。
{
  const { scroller, handle } = await mount()
  handle().snapToBottom()
  await settle()
  const before = scroller.scrollTop
  scroller.grow(scroller.floor + 400 + 28)
  handle().follow()
  // 同一任务里不得动位置:位移只在帧回调里发生。
  if (scroller.scrollTop === before) ok('内容长高后不在同一任务里抢位置')
  else fail('follow() 当场写入了位置', `${before} → ${scroller.scrollTop}`)
  const deltas = []
  let previous = scroller.scrollTop
  for (let frame = 0; frame < 45; frame += 1) {
    await frames(1)
    const delta = scroller.scrollTop - previous
    if (delta > 0) deltas.push(delta)
    previous = scroller.scrollTop
  }
  const biggest = deltas.reduce((max, delta) => Math.max(max, delta), 0)
  if (deltas.length >= 4 && biggest <= 8 && near(scroller.scrollTop, scroller.floor, 1)) {
    ok(`28px 折行滑入:${deltas.length} 帧完成,单帧最大 ${biggest.toFixed(2)}px`)
  } else {
    fail(
      '折行没被摊平',
      `frames=${deltas.length} biggest=${biggest.toFixed(2)} top=${scroller.scrollTop} floor=${scroller.floor}`,
    )
  }
}

// 3) 掉帧自愈:停了 300ms 之后的一次暴涨不在恢复帧里瞬移。
{
  const { scroller, handle } = await mount()
  handle().snapToBottom()
  await settle()
  await frames(20)
  const before = scroller.scrollTop
  scroller.grow(scroller.floor + 400 + 600)
  handle().follow()
  await frames(1)
  const moved = scroller.scrollTop - before
  if (moved > 0 && moved < 120) ok(`掉帧后不瞬移(首帧 ${moved.toFixed(1)}px / 600px)`)
  else fail('掉帧后瞬移', `${moved.toFixed(1)}px`)
  await frames(90)
  if (near(scroller.scrollTop, scroller.floor, 1)) ok('暴涨之后仍然收敛到底')
  else fail('暴涨后未收敛', `top=${scroller.scrollTop} floor=${scroller.floor}`)
}

// 4) 读者上翻脱跟后,内容继续增长不得把他拉回底部。
{
  const { scroller, handle } = await mount()
  handle().snapToBottom()
  await settle()
  scroller.grow(scroller.floor + 400 + 1200)
  handle().follow()
  await frames(40)
  scroller.wheel(-120)
  scroller.readerScrollTo(scroller.floor - 800)
  await settle()
  if (handle().stick) {
    fail('读者上翻后仍处于吸底态')
  } else {
    const parked = scroller.scrollTop
    scroller.grow(scroller.floor + 400 + 2000)
    handle().follow()
    await frames(20)
    if (scroller.scrollTop === parked) ok('脱跟后内容增长不抢读者位置')
    else fail('脱跟后仍被程序拉走', `parked=${parked} now=${scroller.scrollTop}`)
  }
}

// 5) 读者回到真正的底部:恢复跟随,后续增长继续跟。
{
  const { scroller, handle } = await mount()
  handle().snapToBottom()
  await settle()
  scroller.grow(scroller.floor + 400 + 1200)
  handle().follow()
  await frames(40)
  scroller.wheel(-120)
  scroller.readerScrollTo(scroller.floor - 600)
  await settle()
  if (handle().stick) {
    fail('读者上翻后未脱跟,无法验证恢复路径')
  } else {
    scroller.readerScrollTo(scroller.floor)
    await settle()
    if (!handle().stick) {
      fail('读者回到底部未恢复跟随', `top=${scroller.scrollTop} floor=${scroller.floor}`)
    } else {
      scroller.grow(scroller.floor + 400 + 800 + 400)
      handle().follow()
      await frames(90)
      if (near(scroller.scrollTop, scroller.floor, 1)) ok('回到底部后恢复跟随,新内容自动跟随')
      else fail('恢复跟随后没落底', `top=${scroller.scrollTop} floor=${scroller.floor}`)
    }
  }
}

// 6) 程序写入不得被当成读者输入而脱跟。
{
  const { scroller, handle } = await mount()
  handle().snapToBottom()
  await settle()
  for (let round = 0; round < 4; round += 1) {
    scroller.grow(scroller.floor + 400 + 300)
    handle().follow()
    await frames(8)
  }
  if (handle().stick) ok('连续程序跟随不误判脱跟')
  else fail('程序跟随被误判为读者上翻')
}

// 7) 预测预留:流式期内容持续增长时,视口稳态停在离底 reserve px,
//    而不是每次都贴到新的底;生产端停手后预留收回。
{
  const { scroller, handle } = await mount({ active: true })
  handle().snapToBottom()
  await settle()
  scroller.setGeometry({ scrollHeight: 4000, clientHeight: 400 })
  handle().follow()
  await frames(30)
  // 模拟揭示引擎持续上报 600 cps,同时内容以 ~600px/s 匀速长高。
  let height = 4000
  for (let frame = 0; frame < 60; frame += 1) {
    handle().pressure.speedCps = 600
    handle().pressure.speedAt = performance.now()
    height += 10
    scroller.grow(height)
    handle().follow()
    await frames(1)
  }
  const retained = scroller.floor - scroller.scrollTop
  // 预留满档 56px;弹簧自身还会带一点滞后,合起来落在 40~120px。
  if (retained > 40 && retained < 120) {
    ok(`流式期稳态落后内容 ${retained.toFixed(1)}px(预测预留生效)`)
  } else {
    fail('预测预留未生效', `retained=${retained.toFixed(1)}`)
  }
  // 生产端停下:不再上报,预留 220ms 后视为空闲,位置收回到底。
  await frames(120)
  if (near(scroller.scrollTop, scroller.floor, 1)) ok('生产端停下后预留收回,回到真正的底')
  else fail('预留没有收回', `top=${scroller.scrollTop} floor=${scroller.floor}`)
}

// 8) 内容收缩(卡片收起 / 图片回流):不脱跟、不回弹。
{
  const { scroller, handle } = await mount()
  scroller.setGeometry({ scrollHeight: 3000, clientHeight: 400 })
  handle().snapToBottom()
  await settle()
  scroller.grow(2400)
  await frames(30)
  if (handle().stick) ok('内容收缩被钳底:仍保持跟随')
  else fail('内容收缩被误判为读者上翻')
}

console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 全部通过')
// 渲染过的 React 根与 jsdom 的定时器会一直挂住事件循环,显式收尾。
process.exit(0)
