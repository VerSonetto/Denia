/**
 * 段落 reveal 回归测试：批量阈值、markdown 安全边界、flush、追加与重置。
 *
 *   node scripts/check-paragraph-reveal.mjs
 */
import { mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const webRoot = join(here, '..')
const { build } = await import('esbuild')
const outDir = join(webRoot, 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const bundle = join(outDir, 'paragraph-reveal.mjs')
await build({
  entryPoints: [join(webRoot, 'src', 'markdown', 'paragraphReveal.ts')],
  outfile: bundle,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  external: ['react', 'react-dom', 'react/jsx-runtime'],
  logLevel: 'silent',
})

const reveal = await import(pathToFileURL(bundle).href)
let failed = 0
const ok = (label) => console.log(`ok   ${label}`)
const fail = (label, detail) => {
  failed++
  console.log(`FAIL ${label}${detail ? `\n     ${detail}` : ''}`)
}
const check = (condition, label, detail = '') => (condition ? ok(label) : fail(label, detail))

const {
  DEFAULT_PARAGRAPH_CHARS,
  paragraphTarget,
  extendToSafeMarkdown,
  useParagraphReveal,
} = reveal

// 1) 默认值与段落阈值：达到 500 字后才寻找下一段边界。
{
  const text = 'a'.repeat(DEFAULT_PARAGRAPH_CHARS) + '\n\n下一段'
  const target = paragraphTarget(text, 0, false)
  check(target === DEFAULT_PARAGRAPH_CHARS + 2, '默认 500 字阈值在段落边界批量揭示')
  check(paragraphTarget('a'.repeat(DEFAULT_PARAGRAPH_CHARS), 0, false) === 0, '没有段落边界时保持当前位置')
}

// 2) 未闭合围栏和表格：安全位置停在结构开始处，不展示其半成品。
{
  const prefix = '正文'.repeat(260) + '\n\n'
  const fenced = `${prefix}` + '```js\nconst value = 1\n'
  const fenceStart = fenced.indexOf('```')
  const candidate = prefix.length
  const safeFence = extendToSafeMarkdown(fenced, fenced.length - 1)
  check(safeFence === fenceStart, '未完成代码围栏回退到围栏起点', `got=${safeFence} expected=${fenceStart}`)

  const table = `${prefix}| 名称 | 值 |\n| --- | --- |\n| a | b |`
  const tableStart = table.indexOf('|')
  const safeTable = extendToSafeMarkdown(table, table.length - 1)
  check(safeTable === tableStart, '未完成表格回退到表格起点', `got=${safeTable} expected=${tableStart}`)
  check(paragraphTarget(fenced, 0, false) === fenceStart, '段落目标不切入未完成代码围栏', `candidate=${candidate}`)

  const leadingFence = '```js\nconst value = 1\n'
  check(extendToSafeMarkdown(leadingFence, leadingFence.length - 1) === 0, '文档起始未完成代码围栏保持为空')
  const leadingTable = '| 名称 | 值 |\n| --- | --- |\n| a | b |'
  check(extendToSafeMarkdown(leadingTable, leadingTable.length - 1) === 0, '文档起始未完成表格保持为空')
}

// 3) flush 无条件排空；完整结构也允许流式目标直接到全文。
{
  const text = '正文\n\n```js\nconst x = 1\n'
  check(paragraphTarget(text, 0, true) === text.length, '流结束 flush 立即排空全文')
  check(extendToSafeMarkdown(text, 2, true) === text.length, 'flush 忽略未完成 markdown 结构')
}

// 4) React hook：历史直接显示，流式按轮询批量，追加不重置，非追加清空旧正文。
const { JSDOM } = await import('jsdom')
const dom = new JSDOM('<!doctype html><html><body></body></html>', {
  url: 'http://localhost/',
  pretendToBeVisual: true,
})
const g = dom.window
for (const key of [
  'window', 'document', 'navigator', 'location', 'history', 'HTMLElement', 'Element', 'Node',
  'Event', 'CustomEvent', 'MutationObserver', 'getComputedStyle', 'requestAnimationFrame',
  'cancelAnimationFrame', 'localStorage', 'sessionStorage', 'DOMParser', 'Blob', 'URL', 'CSS',
]) {
  try {
    globalThis[key] = g[key]
  } catch {
    /* 只读全局跳过 */
  }
}
const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')
const H = React.createElement
const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms))

async function mount(options) {
  const state = { text: '', streaming: false, reveal: { text: '', done: true } }
  function Harness() {
    state.reveal = useParagraphReveal(state.text, state.streaming, options)
    return null
  }
  const host = g.document.createElement('div')
  g.document.body.appendChild(host)
  const root = createRoot(host)
  root.render(H(Harness))
  await wait(0)
  return {
    state,
    show(text, streaming) {
      state.text = text
      state.streaming = streaming
      root.render(H(Harness))
    },
  }
}

{
  const view = await mount({ minChars: 5, pollMs: 20 })
  const history = '历史正文，直接显示。'
  view.show(history, false)
  await wait(0)
  check(view.state.reveal.text === history && view.state.reveal.done, '非流式历史直接显示全文且 done')
}

{
  const first = '第一段内容。'.repeat(3)
  const second = '第二段追加内容。'.repeat(3)
  const view = await mount({ minChars: 5, pollMs: 20 })
  view.show(`${first}\n\n${second}`, true)
  await wait(35)
  const before = view.state.reveal.text.length
  view.show(`${first}\n\n${second}\n\n第三段`, true)
  await wait(0)
  check(view.state.reveal.text.length >= before, '追加正文保留已显示位置')

  view.show('全新的重试正文', true)
  await wait(0)
  check(view.state.reveal.text === '', '非追加正文先清空旧前缀')

  view.show('全新的重试正文', false)
  await wait(0)
  check(view.state.reveal.text === '全新的重试正文' && view.state.reveal.done, '流结束及时 flush 到全文')
}

if (failed > 0) {
  console.log(`\n✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('\n✓ 全部通过')
process.exit(0)
