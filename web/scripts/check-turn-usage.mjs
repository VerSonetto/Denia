/**
 * 轮次用量卡片的渲染回归:
 *   node scripts/check-turn-usage.mjs
 *
 * 断言三件事(前两件是需求本体,第三件是"没数据不冒充"):
 *   1) 收尾行渲染出可点开的用量按钮;
 *   2) 点开后明细出现,且缓存读/命中率/推理都算对;
 *   3) 提供方没报缓存与推理时,不显示 0 值行。
 */
import { mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'

const here = dirname(fileURLToPath(import.meta.url))
const webRoot = join(here, '..')
const srcRoot = join(webRoot, 'src')
const outDir = join(webRoot, 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const bundle = join(outDir, 'turn-usage.mjs')

await build({
  entryPoints: [join(srcRoot, 'components', 'TurnUsageCard.tsx')],
  outfile: bundle,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  jsx: 'automatic',
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  logLevel: 'silent',
  loader: { '.css': 'empty' },
})

const { JSDOM } = await import('jsdom')
const dom = new JSDOM('<!doctype html><html><body><div id="root"></div></body></html>', {
  url: 'http://localhost/',
  pretendToBeVisual: true,
})
const g = dom.window
for (const k of [
  'window', 'document', 'navigator', 'location', 'history', 'HTMLElement', 'Element', 'Node',
  'Event', 'CustomEvent', 'MouseEvent', 'KeyboardEvent', 'EventTarget', 'MutationObserver',
  'getComputedStyle', 'requestAnimationFrame', 'cancelAnimationFrame', 'matchMedia',
  'localStorage', 'sessionStorage', 'DOMParser', 'Blob', 'URL', 'CSS', 'SVGSVGElement',
  'ResizeObserver',
]) {
  try { globalThis[k] = g[k] } catch { /* 只读全局跳过 */ }
}

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
function ok(label) { console.log(`ok   ${label}`) }
function fail(label, detail) {
  failed++
  console.log(`FAIL ${label}${detail ? `\n  ${detail}` : ''}`)
}

const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')
const { act } = await import('react')
const mod = await import(`${pathToFileURL(bundle).href}?t=${Date.now()}`)
const H = React.createElement
// 卡片 portal 到 body,所以断言要查整篇文档而不是渲染根 —— 这正是修复点:
// 浮层必须逃出带 content-visibility 的会话行,否则会被 paint containment 裁掉。
const doc = g.document
const rootEl = doc.getElementById('root')
const root = createRoot(rootEl)

const settle = () => new Promise((r) => setTimeout(r, 40))

/** 用真实日志量级:12429 未缓存 + 9088 缓存 + 211 输出 + 115 推理。 */
await act(async () => {
  root.render(H(mod.TurnUsageCard, {
    usage: { inputTokens: 12429, outputTokens: 211, cacheReadTokens: 9088, reasoningTokens: 115 },
  }))
  await settle()
})

const trigger = doc.querySelector('button')
if (!trigger) {
  fail('渲染出可点开的用量触发件', rootEl.innerHTML.slice(0, 200))
} else {
  ok('渲染出可点开的用量触发件')
  check('触发件是弹层按钮(aria-haspopup)', trigger.getAttribute('aria-haspopup'), 'dialog')
  check('未展开时明细不存在', doc.querySelector('.turn-usage-card'), null)
  check('收起态 aria-expanded', trigger.getAttribute('aria-expanded'), 'false')

  // 点开:走真实 click,验证明细出现且数字正确。
  await act(async () => {
    trigger.dispatchEvent(new g.window.MouseEvent('click', { bubbles: true }))
    await settle()
  })
  const card = doc.querySelector('.turn-usage-card')
  if (!card) {
    fail('点开后出现明细卡片', rootEl.innerHTML.slice(0, 300))
  } else {
    ok('点开后出现明细卡片')
    check('展开态 aria-expanded', trigger.getAttribute('aria-expanded'), 'true')
    // 裁剪回归闸:卡片必须 portal 到 body,不能留在渲染根内。
    // 会话行 .turn-chrome 带 content-visibility:auto(paint containment),
    // 留在根内的浮层会被整块裁掉 —— 卡片在 DOM 里却一个像素都画不出来。
    check('卡片已 portal 出渲染根(不被 content-visibility 裁剪)', rootEl.contains(card), false)
    check('卡片挂在 body 上', card.parentElement === doc.body, true)
    const rows = [...card.querySelectorAll('.turn-usage-rows div')].map((row) => [
      row.querySelector('dt').textContent,
      row.querySelector('dd').textContent,
    ])
    // 未缓存输入 12429 → 12.4k;缓存读 9088 → 9.1k;命中率 42.24%;输出 211 + 推理注脚。
    check('明细行内容与顺序', rows, [
      ['未缓存输入', '12.4k'],
      ['缓存读', '9.1k'],
      ['缓存命中率', '42.24%'],
      ['输出', '211(其中推理 115)'],
    ])
    // 总量 = 计费输入(12429+9088) + 输出 211 = 21728 → 21.7k。
    check('总量按计费输入+输出', card.querySelector('.turn-usage-total').textContent, '21.7k')

    // Escape 关闭。
    await act(async () => {
      g.document.dispatchEvent(new g.window.KeyboardEvent('keydown', { key: 'Escape', bubbles: true }))
      await settle()
    })
    check('Escape 关闭明细', doc.querySelector('.turn-usage-card'), null)
  }
}

// 提供方没报缓存/推理:不拿 0 冒充,只留必然存在的两行。
await act(async () => {
  root.render(H(mod.TurnUsageCard, { usage: { inputTokens: 500, outputTokens: 20 } }))
  await settle()
})
const bareTrigger = doc.querySelector('button')
await act(async () => {
  bareTrigger.dispatchEvent(new g.window.MouseEvent('click', { bubbles: true }))
  await settle()
})
const bareCard = doc.querySelector('.turn-usage-card')
const bareRows = bareCard
  ? [...bareCard.querySelectorAll('.turn-usage-rows div')].map((row) => row.querySelector('dt').textContent)
  : []
check('无缓存/推理数据时不显示 0 值行', bareRows, ['未缓存输入', '输出'])

console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 全部通过')
