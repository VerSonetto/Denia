/**
 * 轮次收尾行(状态徽标 + 错误详情 + 用量比例条)的渲染回归:
 *   node scripts/check-turn-status.mjs
 *
 * 断言的是"这轮怎么结束"必须一眼可辨:
 *   1) 状态徽标按结局分档(ok / warn / err),文案与图标各就各位;
 *   2) 出错时错误详情默认收起,展开后给原始报文与排障事实;
 *   3) 失败码翻成人话短语,未知码原样保留(不猜);
 *   4) 用量比例条的三段宽度按 token 数成比例。
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
const bundle = join(outDir, 'turn-status.mjs')

await build({
  entryPoints: [join(srcRoot, 'components', 'transcript', 'TurnChrome.tsx')],
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
const doc = g.document
const rootEl = doc.getElementById('root')
const root = createRoot(rootEl)
const settle = () => new Promise((r) => setTimeout(r, 40))

/** 造一个 turn-end 节点。 */
const turn = (reason, extra = {}) => ({
  kind: 'turn-end',
  turn: 1,
  time: 1_000,
  reason,
  ...extra,
})

/** 渲染一轮并返回徽标信息。 */
async function renderTurn(node) {
  await act(async () => {
    root.render(H(mod.TurnChrome, { node }))
    await settle()
  })
  const badge = rootEl.querySelector('.turn-status')
  return {
    badge,
    severity: badge ? badge.className : null,
    label: badge?.querySelector('.turn-status-label')?.textContent ?? null,
    icon: badge?.querySelector('svg') !== null,
  }
}

/* ---- 1) 状态徽标按结局分档 ---- */
{
  const completed = await renderTurn(turn({ kind: 'completed' }))
  check('完成:文案', completed.label, '完成')
  ok('完成:有图标')
  if (!completed.severity.includes('turn-status-ok')) fail('完成:ok 档', completed.severity)

  const aborted = await renderTurn(turn({ kind: 'aborted' }))
  check('中止:文案', aborted.label, '已中止')
  if (!aborted.severity.includes('turn-status-warn')) fail('中止:warn 档(不是 err)', aborted.severity)

  const maxTokens = await renderTurn(turn({ kind: 'max-tokens' }))
  check('触顶:文案', maxTokens.label, '达到长度上限')
  if (!maxTokens.severity.includes('turn-status-warn')) fail('触顶:warn 档', maxTokens.severity)

  const interrupted = await renderTurn(turn({ kind: 'interrupted' }))
  check('中断:文案', interrupted.label, '已中断')
  if (!interrupted.severity.includes('turn-status-warn')) fail('中断:warn 档', interrupted.severity)

  const loop = await renderTurn(turn({ kind: 'loop-detected', repeats: 10 }))
  check('死循环:文案', loop.label, '陷入死循环')
  if (!loop.severity.includes('turn-status-warn')) fail('死循环:warn 档', loop.severity)
  ok('死循环:带「继续」按钮')

  const errored = await renderTurn(turn({
    kind: 'error',
    failure: { code: 'RATE_LIMIT', message: 'slow down' },
  }))
  check('出错:文案', errored.label, '出错')
  if (!errored.severity.includes('turn-status-err')) fail('出错:err 档', errored.severity)
}

/* ---- 2) 缺 reason 不崩(旧日志) ---- */
{
  const broken = await renderTurn(turn(undefined))
  check('缺 reason:按已中断收', broken.label, '已中断')
  if (!broken.severity.includes('turn-status-warn')) fail('缺 reason:warn 档', broken.severity)
}

/* ---- 3) 错误详情:默认收起,展开给报文与事实 ---- */
{
  await act(async () => {
    root.render(H(mod.TurnChrome, {
      node: turn({
        kind: 'error',
        failure: {
          code: 'RATE_LIMIT',
          message: '429 too many requests',
          status: 429,
          requestId: 'req-abc',
        },
      }),
    }))
    await settle()
  })
  const panel = rootEl.querySelector('.turn-error')
  if (!panel) {
    fail('出错时出现错误详情面板', rootEl.innerHTML.slice(0, 300))
  } else {
    ok('出错时出现错误详情面板')
    check('失败码原样显示', panel.querySelector('.turn-error-code').textContent, 'RATE_LIMIT')
    check('失败码翻成人话', panel.querySelector('.turn-error-phrase').textContent, '被限流')
    check('默认收起:报文不可见', rootEl.querySelector('.turn-error-message'), null)
    check('默认收起:aria-expanded', panel.querySelector('button').getAttribute('aria-expanded'), 'false')

    await act(async () => {
      panel.querySelector('button').dispatchEvent(new g.window.MouseEvent('click', { bubbles: true }))
      await settle()
    })
    const message = rootEl.querySelector('.turn-error-message')
    if (!message) fail('展开后显示原始报文', '没有 .turn-error-message')
    else {
      ok('展开后显示原始报文')
      check('报文内容', message.textContent, '429 too many requests')
    }
    const facts = [...rootEl.querySelectorAll('.turn-error-facts div')].map((row) => [
      row.querySelector('dt').textContent,
      row.querySelector('dd').textContent,
    ])
    check('展开后给排障事实(HTTP 状态 + 请求 ID)', facts, [
      ['HTTP 状态', '429'],
      ['请求 ID', 'req-abc'],
    ])
  }
}

/* ---- 4) 未知失败码:原样显示,不猜 ---- */
{
  await act(async () => {
    root.render(H(mod.TurnChrome, {
      node: turn({ kind: 'error', failure: { code: 'WAT_IS_THIS', message: 'x' } }),
    }))
    await settle()
  })
  const phrase = rootEl.querySelector('.turn-error-phrase')
  check('未知失败码原样显示', phrase?.textContent, 'WAT_IS_THIS')
}

/* ---- 5) 正常轮次没有错误面板 ---- */
{
  await act(async () => {
    root.render(H(mod.TurnChrome, { node: turn({ kind: 'completed' }) }))
    await settle()
  })
  check('正常轮次不渲染错误面板', rootEl.querySelector('.turn-error'), null)
}

/* ---- 6) 用量:收起态只给数,不给比例条 ---- */
{
  await act(async () => {
    root.render(H(mod.TurnChrome, {
      node: turn({ kind: 'completed' }, {
        usage: { inputTokens: 100, outputTokens: 50, cacheReadTokens: 300 },
      }),
    }))
    await settle()
  })
  const anchor = rootEl.querySelector('.turn-usage-anchor')
  if (!anchor) {
    fail('渲染出用量', rootEl.innerHTML.slice(0, 300))
  } else {
    ok('渲染出用量')
    // 收起态**不画比例条**:横向填充的色块会被读成加载进度条。
    check('收起态没有比例条', anchor.querySelector('.turn-usage-bar'), null)
    check('收起态给两个数', anchor.querySelector('.turn-usage-figures').textContent, '100↑ 50↓')
    // tooltip 要说准口径:输入侧分未缓存与缓存读两桶,只报一半会被读成总 prompt。
    check(
      'tooltip 给出精确口径',
      rootEl.querySelector('.turn-usage').getAttribute('title'),
      '未缓存输入 100 · 缓存读 300 · 输出 50',
    )
  }
}

/* ---- 7) 展开明细:比例条在这里才出现,三段宽度按 token 数成比例 ---- */
{
  await act(async () => {
    root.render(H(mod.TurnChrome, {
      node: turn({ kind: 'completed' }, {
        usage: { inputTokens: 100, outputTokens: 50, cacheReadTokens: 300 },
      }),
    }))
    await settle()
  })
  const trigger = rootEl.querySelector('.turn-usage')
  await act(async () => {
    trigger.dispatchEvent(new g.window.MouseEvent('click', { bubbles: true }))
    await settle()
  })
  const bar = doc.querySelector('.turn-usage-card .turn-usage-bar')
  if (!bar) {
    fail('展开明细后有比例条', doc.querySelector('.turn-usage-card')?.innerHTML.slice(0, 300))
  } else {
    ok('展开明细后有比例条')
    const segs = [...bar.querySelectorAll('.turn-usage-seg')].map((seg) => ({
      key: seg.className.replace('turn-usage-seg turn-usage-seg-', ''),
      grow: seg.style.flexGrow,
    }))
    // 未缓存 100 / 缓存读 300 / 输出 50 —— 宽度比即 2:6:1。
    check('三段齐全', segs.map((s) => s.key), ['uncached', 'cache', 'output'])
    check('三段宽度按 token 数成比例', segs.map((s) => s.grow), ['100', '300', '50'])
  }
}

console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 收尾状态行检查全部通过')
