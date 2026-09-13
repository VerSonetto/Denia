/**
 * 吸底跟随回归网:useStickToBottom 的判定必须在"内容持续增长"这个真实前提下成立。
 *
 *   node scripts/check-stick.mjs
 *
 * 为什么要专测这个 hook:它出过两次同类故障 —— 判定拿"当前的底"去比"读者的
 * 历史位置",中间隔着流式新长出来的内容,"刚滚到底"于是被判成"已离开底部",
 * 读者手动滚到底也恢复不了跟随。jsdom 没有布局引擎(scrollHeight 恒 0),这里
 * 注入可控几何,再按真实事件序列驱动,把这几条路径钉死:
 *   1) 内容长高后跟随落到新底;
 *   2) 【核心回归】读者拖到底的瞬间内容又长了 —— 必须仍判定为贴底;
 *   3) 读者上翻脱跟后,内容继续增长不得把他拉回底部;
 *   4) 读者回到当前底部 —— 必须恢复跟随;
 *   5) 程序性落底(心跳/回底按钮)不得被误判成读者输入;
 *   6) 内容收缩(展开的卡片收起)导致的钳底不得当成读者上翻。
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
const bundle = join(outDir, 'stick.mjs')

await build({
  entryPoints: [join(webRoot, 'src', 'hooks', 'useStickToBottom.ts')],
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

const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')
const { useStickToBottom } = await import(pathToFileURL(bundle).href)
const H = React.createElement

/**
 * 注入可控几何:scrollHeight/clientHeight 由测试写入,scrollTop 落盘即读,
 * 并像浏览器那样钳在 [0, floor]。jsdom 不注入的话三个值恒为 0。
 */
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
    /** 程序落底的回执:位置已经设定好,只补一次 scroll 事件。 */
    echoScroll() {
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

/** 渲染只挂容器的 harness,返回几何控制器与 hook 句柄。 */
async function mount(options) {
  const box = { handle: null, geometry: null }
  function Harness() {
    const handle = useStickToBottom({ current: null }, { active: false, ...options })
    box.handle = handle
    // 与真实用法一致:ref 回调标识必须稳定,否则 React 每次渲染都重挂一次
    // 元素(setNode(null) → setNode(el)),那会把读者的位置当成"新容器"重置掉。
    const attachRef = React.useCallback(
      (el) => {
        if (el !== null && box.geometry === null) box.geometry = defineGeometry(el)
        handle.setNode(el)
      },
      [handle.setNode],
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

/* ---------- 用例 ---------- */

// 1) 内容长高后 follow() 落到新底,保持吸底。
{
  const { scroller, handle } = await mount()
  handle().snapToBottom()
  if (scroller.scrollTop !== scroller.floor) {
    fail('程序落底后 scrollTop 应等于 floor', `scrollTop=${scroller.scrollTop} floor=${scroller.floor}`)
  } else {
    scroller.grow(1600)
    handle().follow()
    if (scroller.scrollTop === scroller.floor && handle().stick) {
      ok('内容长高后 follow() 落到新底,保持吸底')
    } else {
      fail(
        '内容长高后未落到新底',
        `scrollTop=${scroller.scrollTop} floor=${scroller.floor} stick=${handle().stick}`,
      )
    }
  }
}

// 2) 【核心回归】读者拖到底的瞬间内容又长了 —— 必须仍判定为贴底。
//    旧实现:判定时 floor 已涨到 1800,floor - top 远超阈值 → 误判脱跟。
{
  const { scroller, handle } = await mount()
  scroller.setGeometry({ scrollHeight: 1000, clientHeight: 400 }) // floor = 600
  scroller.readerScrollTo(0)
  // 读者把滚动条拖到底(= 当时的 floor 600)。
  scroller.readerScrollTo(scroller.floor)
  // 内容在他松手前又长了一大段(流式常态)。
  scroller.grow(2200) // 新 floor = 1800
  scroller.echoScroll()
  await settle()
  if (handle().stick) {
    ok('拖到底时内容又增长:仍判定贴底(旧 bug 正面回归)')
  } else {
    fail('拖到底时内容又增长,被误判为离开底部', `scrollTop=${scroller.scrollTop} floor=${scroller.floor}`)
  }
}

// 3) 读者上翻脱跟后,内容继续增长不得把他拉回底部。
{
  const { scroller, handle } = await mount()
  handle().snapToBottom()
  scroller.grow(3000)
  handle().follow()
  await settle()
  scroller.wheel(-240)
  scroller.readerScrollTo(scroller.floor - 800)
  await settle()
  if (handle().stick) {
    fail('读者上翻后仍处于吸底态', `scrollTop=${scroller.scrollTop} floor=${scroller.floor}`)
  } else {
    const parked = scroller.scrollTop
    scroller.grow(4200)
    handle().follow()
    await settle()
    if (scroller.scrollTop === parked) {
      ok('脱跟后内容增长不抢读者位置')
    } else {
      fail('脱跟后仍被程序拉走', `parked=${parked} now=${scroller.scrollTop}`)
    }
  }
}

// 4) 读者回到当前底部 —— 恢复跟随,随后新内容自动跟随。
{
  const { scroller, handle } = await mount()
  handle().snapToBottom()
  scroller.grow(3000)
  handle().follow()
  await settle()
  scroller.wheel(-240)
  scroller.readerScrollTo(scroller.floor - 800)
  await settle()
  if (handle().stick) {
    fail('读者上翻后未脱跟,无法验证恢复路径')
  } else {
    scroller.readerScrollTo(scroller.floor)
    await settle()
    if (!handle().stick) {
      fail('读者回到当前底部未恢复跟随', `scrollTop=${scroller.scrollTop} floor=${scroller.floor}`)
    } else {
      scroller.grow(scroller.floor + 1200 + 400)
      handle().follow()
      if (scroller.scrollTop === scroller.floor) {
        ok('读者回到底部后恢复跟随,新内容自动跟随')
      } else {
        fail('恢复跟随后又没落底', `scrollTop=${scroller.scrollTop} floor=${scroller.floor}`)
      }
    }
  }
}

// 5) 程序落底(心跳 write)不得被当成读者输入而脱跟。
{
  const { scroller, handle } = await mount()
  handle().snapToBottom()
  for (let i = 0; i < 6; i += 1) {
    scroller.grow(1200 + i * 400)
    handle().follow()
    scroller.echoScroll() // 程序滚动的回执
    await settle()
  }
  if (handle().stick && scroller.scrollTop === scroller.floor) {
    ok('连续程序落底不误判脱跟')
  } else {
    fail('程序落底被误判', `stick=${handle().stick} scrollTop=${scroller.scrollTop} floor=${scroller.floor}`)
  }
}

// 6) 内容收缩把位置钳到新底:不是读者上翻,不得脱跟。
{
  const { scroller, handle } = await mount()
  scroller.setGeometry({ scrollHeight: 3000, clientHeight: 400 })
  handle().snapToBottom()
  scroller.echoScroll()
  await settle()
  // 展开的卡片收起 / 图片回流:高度骤减,浏览器把 scrollTop 钳到新 floor。
  scroller.grow(1400)
  scroller.readerScrollTo(scroller.floor)
  await settle()
  if (handle().stick) {
    ok('内容收缩被钳底:仍保持跟随')
  } else {
    fail('内容收缩被误判为读者上翻', `scrollTop=${scroller.scrollTop} floor=${scroller.floor}`)
  }
}

// 7) 读者小幅度上翻(未达阈值):不得脱跟,且不得被立刻按回底部。
{
  const { scroller, handle } = await mount()
  handle().snapToBottom()
  scroller.grow(3000)
  handle().follow()
  await settle()
  const before = scroller.scrollTop
  // 滚轮背隙级别的小位移(低于阈值):不算读者要脱跟。
  scroller.readerScrollTo(before - 8)
  await settle()
  if (!handle().stick) {
    fail('微小上翻被误判为脱跟', `scrollTop=${scroller.scrollTop}`)
  } else {
    // 立刻补内容:避让窗口内不该把读者按回底部。
    scroller.grow(3600)
    handle().follow()
    await settle()
    ok(`微小上翻不改归属(scrollTop=${scroller.scrollTop})`)
  }
}

// 8) 折叠 → 重新展开:新容器按需对齐到底部。
{
  const { scroller, handle, root } = await mount({ alignOnAttach: true })
  scroller.setGeometry({ scrollHeight: 3000, clientHeight: 400 })
  handle().snapToBottom()
  scroller.echoScroll()
  await settle()
  // 模拟重新挂载一个新容器(展开态重建):几何重置。
  const again = await mount({ alignOnAttach: true })
  again.scroller.setGeometry({ scrollHeight: 3000, clientHeight: 400 })
  again.handle().snapToBottom()
  if (again.handle().stick && again.scroller.scrollTop === again.scroller.floor) {
    ok('新展开的容器默认对齐底部')
  } else {
    fail(
      '新展开的容器未对齐底部',
      `scrollTop=${again.scroller.scrollTop} floor=${again.scroller.floor}`,
    )
  }
  void root
}

console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 全部通过')
