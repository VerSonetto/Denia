/**
 * 揭示引擎回归网:字符积分、单帧上限、追加/重置、尾部收束与压力上报。
 *
 *   node scripts/check-reveal.mjs
 *
 * 纯速度定律已在 check-follow.mjs 里钉过(queueRevealStep / drainCpsFor),这里
 * 只钉 hook 的对外契约:`{ text, done }` —— 显示文本永远是全文的前缀且单调增长;
 * 消费端(seal)在 done 之前必须继续走流式渲染路径;结算后排空到全文而不是跳。
 */
import { mkdirSync } from 'node:fs'
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

const { build } = await import('esbuild')
const outDir = join(webRoot, 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const bundle = join(outDir, 'reveal.mjs')
await build({
  entryPoints: [join(webRoot, 'src', 'typewriter.ts')],
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
const { useTypewriter } = await import(pathToFileURL(bundle).href)
const H = React.createElement

const settle = () => new Promise((resolve) => setTimeout(resolve, 0))
const frames = (count) => new Promise((resolve) => setTimeout(resolve, count * 16 + 8))

/** 渲染一个只跑揭示引擎的 harness,`state` 可在两次渲染之间改写。 */
async function mount(pressure = null) {
  const state = { text: '', streaming: false, reveal: { text: '', done: true } }
  function Harness() {
    const reveal = useTypewriter(state.text, state.streaming, pressure === null ? undefined : { pressure })
    state.reveal = reveal
    return null
  }
  const host = g.document.createElement('div')
  g.document.body.appendChild(host)
  const root = createRoot(host)
  root.render(H(Harness))
  await settle()
  return {
    get reveal() {
      return state.reveal
    },
    show(text, streaming) {
      state.text = text
      state.streaming = streaming
      root.render(H(Harness))
    },
  }
}

/* ---------- 用例 ---------- */

// 1) 非流式(历史回放 / 已定稿):一上来就是全文,不播尾巴。
{
  const view = await mount()
  view.show('已经定稿的一段回答。', false)
  await settle()
  if (view.reveal.text === '已经定稿的一段回答。' && view.reveal.done) {
    ok('非流式:立即全量且 done')
  } else {
    fail('非流式没有立即全量', JSON.stringify(view.reveal))
  }
}

// 2) 流式:显示是全文前缀、单调增长,且一帧不倾泻整段。
{
  const full = '流'.repeat(400)
  const view = await mount()
  view.show(full, true)
  await settle()
  const samples = []
  for (let frame = 0; frame < 24; frame += 1) {
    await frames(1)
    samples.push(view.reveal.text.length)
  }
  const monotone = samples.every((value, index) => index === 0 || value >= samples[index - 1])
  const prefix = samples.every((length) => full.startsWith(full.slice(0, length)))
  const grown = samples[samples.length - 1] > samples[0]
  const biggest = samples.reduce((max, value, index) => (
    index === 0 ? Math.max(max, value) : Math.max(max, value - samples[index - 1])
  ), 0)
  if (monotone && prefix && grown && biggest <= 40 && samples[samples.length - 1] < full.length) {
    ok(`流式:单调增长、不倾泻(单帧最多 ${biggest} 字 / 400 字)`)
  } else {
    fail(
      '流式揭示不符合契约',
      `monotone=${monotone} prefix=${prefix} grown=${grown} biggest=${biggest} last=${samples[samples.length - 1]}`,
    )
  }
}

// 3) 追加:后续分块不重置已有显示。
{
  const head = '第一段已经到达的文字。'.repeat(20)
  const view = await mount()
  view.show(head, true)
  await frames(10)
  const before = view.reveal.text.length
  view.show(head + '第二段追加', true)
  await settle()
  if (view.reveal.text.length >= before && head.startsWith(view.reveal.text)) {
    ok('追加分块:位置不重置')
  } else {
    fail('追加时位置被重置', `before=${before} now=${view.reveal.text.length}`)
  }
}

// 4) 非追加(重试生成):从头播,不显示错位文本。
{
  const view = await mount()
  view.show('甲'.repeat(200), true)
  await frames(6)
  view.show('乙'.repeat(200), true)
  await settle()
  const text = view.reveal.text
  if (text.split('').every((char) => char === '乙')) ok('非追加目标:从头播且不串字')
  else fail('非追加目标串了旧文本', JSON.stringify(text.slice(0, 40)))
}

// 5) 尾部收束:settle 时残余逐帧排空,done 之前不跳全文。
{
  const full = '结'.repeat(600)
  const view = await mount()
  view.show(full, true)
  await frames(10)
  const beforeSettle = view.reveal.text.length
  view.show(full, false)
  await settle()
  const atSettle = view.reveal.text.length
  // 结算那一帧不得直接跳到全文。
  if (atSettle < full.length) ok(`settle 不跳全文(残余 ${full.length - atSettle} 字)`)
  else fail('settle 直接跳到全文', `${beforeSettle} → ${atSettle}`)
  await frames(30)
  if (view.reveal.text === full && view.reveal.done) ok('收尾排空在若干帧内补齐并 done')
  else fail('收尾未补齐', `len=${view.reveal.text.length} done=${view.reveal.done}`)
}

// 6) 压力通道:背压 0.55 时揭示明显更慢;揭示期间上报速率与时刻。
{
  const fast = await mount()
  fast.show('压'.repeat(400), true)
  await frames(8)
  const fastLength = fast.reveal.text.length
  const pressure = { speedCps: 0, speedAt: 0, scale: 0.55 }
  const slow = await mount(pressure)
  slow.show('压'.repeat(400), true)
  await frames(8)
  const slowLength = slow.reveal.text.length
  if (slowLength < fastLength) ok(`背压生效:同帧数下 ${fastLength} → ${slowLength} 字`)
  else fail('背压没有作用到揭示速度', `fast=${fastLength} slow=${slowLength}`)
  if (pressure.speedCps > 0 && pressure.speedAt > 0) ok('揭示期间上报速率与时刻')
  else fail('未上报揭示速率', JSON.stringify(pressure))
}

console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 全部通过')
// 渲染过的 React 根与 jsdom 定时器会挂住事件循环,显式收尾。
process.exit(0)
