/**
 * 右侧面板的运行时渲染检查(jsdom)。
 *
 * `check-side-pane.mjs` 只测纯函数;这条补上"组件真的能挂载并按交互改变 DOM"
 * —— 静态扫描发现不了"点 `+` 菜单没反应"这类问题。
 *
 * 另两条外显行为也在这里守:
 * - 清空标签 → 收起面板(且不卸载内容);
 * - 宽度由**内联样式**驱动 —— 这是展开/收起过渡的前提(`width:0` 写死在
 *   CSS 里就永远不会有终点,过渡直接失效)。
 *
 *   node scripts/check-side-pane-render.mjs
 */
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const here = fileURLToPath(new URL('.', import.meta.url))
const srcRoot = join(here, '..', 'src')

let failed = 0
const ok = (label) => console.log(`ok   ${label}`)
const fail = (label, detail) => {
  failed++
  console.log(`FAIL ${label}${detail ? `\n     ${detail}` : ''}`)
}

/* 把 SidePane 及其依赖 bundle 成单文件(排除 react,CSS 置空)。
 *
 * 产物必须落在 `web/` 内(而不是系统临时目录):bundle 里 `react` 是
 * external,靠 Node 从**产物所在目录**向上找 node_modules 解析;
 * 放在 tmp 下会直接 ERR_MODULE_NOT_FOUND。 */
const outDir = join(here, '..', 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const entry = join(outDir, 'side-pane-entry.tsx')

// 入口包一层:导出 SidePane 与纯函数,便于断言。
writeFileSync(
  entry,
  `
import { SidePane } from ${JSON.stringify(join(srcRoot, 'components', 'SidePane.tsx').replace(/\\/g, '/'))}
import * as pure from ${JSON.stringify(join(srcRoot, 'sidePane.ts').replace(/\\/g, '/'))}
export { SidePane, pure }
`,
)

const bundle = join(outDir, 'bundle.mjs')
await build({
  entryPoints: [entry],
  outfile: bundle,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  jsx: 'automatic',
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  loader: { '.css': 'empty' },
  logLevel: 'silent',
})

/* jsdom 环境:React 18 需要这些全局。 */
const dom = new JSDOM('<!doctype html><html><body><div id="root"></div></body></html>', {
  url: 'http://localhost/',
  pretendToBeVisual: true,
})
const g = dom.window
for (const key of [
  'window',
  'document',
  'navigator',
  'location',
  'history',
  'HTMLElement',
  'Element',
  'Node',
  'Event',
  'CustomEvent',
  'MouseEvent',
  'KeyboardEvent',
  'EventTarget',
  'MutationObserver',
  'getComputedStyle',
  'requestAnimationFrame',
  'cancelAnimationFrame',
  'matchMedia',
  'localStorage',
  'sessionStorage',
  'DOMParser',
  'Blob',
  'URL',
  'CSS',
  'SVGSVGElement',
  'ResizeObserver',
  'DataTransfer',
  'DragEvent',
]) {
  try {
    globalThis[key] = g[key]
  } catch {
    /* 只读全局跳过 */
  }
}

const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')
const mod = await import(pathToFileURL(bundle).href)
const { SidePane, pure } = mod
const H = React.createElement

const tick = (ms = 60) => new Promise((r) => setTimeout(r, ms))

/** 渲染一次并返回容器;同时收集 React 报错。 */
async function render(props) {
  const errors = []
  const original = console.error
  console.error = (...args) => errors.push(args.map((a) => a?.message ?? String(a)).join(' '))
  const root = createRoot(g.document.getElementById('root'))
  root.render(H(SidePane, props))
  await tick(90)
  console.error = original
  return { html: g.document.getElementById('root').innerHTML, errors }
}

const baseProps = {
  sessionId: 'sess-1',
  collapsed: false,
  workspacePath: 'D:\\code_project\\denia',
  supportsBrowser: true,
  recentClosed: [],
  onChange: () => {},
  onCollapsedChange: () => {},
  onRememberClosed: () => {},
  onForgetClosed: () => {},
  onOpenPanel: () => {},
}

/**
 * 受控外壳:把 `onChange` / `onCollapsedChange` 接到真实的 React state 上。
 *
 * 行为类断言必须用它 —— 给 SidePane 传一组固定 props 是测不出"关标签 →
 * 状态变化 → 收起"这条因果链的:不重渲染就看不到结果。
 */
function Harness({ initial, collapsedCalls }) {
  const [state, setState] = React.useState(initial)
  const [collapsed, setCollapsed] = React.useState(false)
  return H(SidePane, {
    ...baseProps,
    state,
    collapsed,
    onChange: (updater) => setState((current) => updater(current)),
    onCollapsedChange: (value) => {
      collapsedCalls.push(value)
      setCollapsed(value)
    },
  })
}

/** 挂一个受控外壳,返回容器 / root / 收起回调记录。 */
async function mountHarness(initial) {
  const collapsedCalls = []
  const container = g.document.createElement('div')
  g.document.body.appendChild(container)
  const root = createRoot(container)
  root.render(H(Harness, { initial, collapsedCalls }))
  await tick(90)
  return { container, root, collapsedCalls }
}

/** 在容器里点一下(找不到元素返回 false)。 */
function clickIn(container, selector) {
  const node = container.querySelector(selector)
  if (!node) return false
  node.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))
  return true
}

/** 造一个终端标签(只填断言关心的字段)。 */
function tab(id, type = 'terminal') {
  return { id, type, openedAt: Date.now() }
}

/* 1) 空态:应渲染引导页与三个面板按钮 */
{
  const { html, errors } = await render({ ...baseProps, state: pure.EMPTY_SIDE_PANE })
  const hookError = errors.find((line) => /hook/i.test(line))
  if (hookError) fail('空态渲染', hookError)
  else if (!html.includes('pane-open-shell')) fail('空态渲染', '没有引导页容器')
  else if (!html.includes('data-pane-open-item="review"')) fail('空态渲染', '没有审查按钮')
  else if (!html.includes('data-pane-open-item="terminal"')) fail('空态渲染', '没有终端按钮')
  else if (!html.includes('data-pane-open-item="browser"')) fail('空态渲染', '没有浏览器按钮')
  else if (!html.includes('data-pane-open-item="files"')) fail('空态渲染', '没有工作区文件按钮')
  else ok('空态渲染:引导页 + 四个面板按钮')
}

/* 2) 有标签:应渲染标签栏、标签、空态消失 */
{
  const state = {
    tabs: [
      { id: 'r1', type: 'review', openedAt: Date.now() },
      { id: 't1', type: 'terminal', openedAt: Date.now(), title: 'pwsh' },
      { id: 't2', type: 'terminal', openedAt: Date.now() },
    ],
    activeTabId: 't1',
  }
  const { html, errors } = await render({ ...baseProps, state })
  const hookError = errors.find((line) => /hook/i.test(line))
  if (hookError) fail('标签栏渲染', hookError)
  else if (html.includes('pane-open-shell')) fail('标签栏渲染', '有标签时不应显示引导页')
  else if (!html.includes('data-pane-tab-viewport')) fail('标签栏渲染', '没有标签滚动区')
  else if (!html.includes('data-tab-id="r1"')) fail('标签栏渲染', '缺 review 标签')
  else if (!html.includes('data-tab-id="t1"')) fail('标签栏渲染', '缺 t1 标签')
  else if (!html.includes('data-tab-id="t2"')) fail('标签栏渲染', '缺 t2 标签')
  // 激活标签必须有 data-active(样式与 aria 都靠它)
  else if (!/data-tab-id="t1"[^>]*data-active/.test(html)) fail('标签栏渲染', 't1 未标记为激活')
  // 终端标签显示自定义标题
  else if (!html.includes('pwsh')) fail('标签栏渲染', '终端标签未显示 shell 标题')
  // 关闭按钮常驻
  else if (!html.includes('pane-tab-close')) fail('标签栏渲染', '缺常驻关闭按钮')
  else ok('标签栏渲染:3 个标签 + 激活态 + 关闭按钮')
}

/* 3) 折叠:整体加 collapsed,内容仍在 DOM(保活) */
{
  const state = {
    tabs: [{ id: 't1', type: 'terminal', openedAt: Date.now() }],
    activeTabId: 't1',
  }
  const { html } = await render({ ...baseProps, state, collapsed: true })
  if (!html.includes('side-pane collapsed') && !html.includes('collapsed')) {
    fail('折叠渲染', '没有 collapsed 标记')
  } else if (!html.includes('side-pane-expand')) {
    fail('折叠渲染', '折叠后应有展开把手')
  } else if (!html.includes('pane-tabs')) {
    // 折叠时标签栏仍在 DOM 里(只是宽度归零),这是保活的关键。
    fail('折叠渲染', '折叠后内容不应被卸载(保活)')
  } else {
    ok('折叠渲染:宽度归零但内容保留(保活)+ 展开把手')
  }
}

/* 4) 浏览器能力关闭时:浏览器按钮/标签入口消失 */
{
  const { html } = await render({
    ...baseProps,
    state: pure.EMPTY_SIDE_PANE,
    supportsBrowser: false,
  })
  if (html.includes('data-pane-open-item="browser"')) {
    fail('能力过滤', '不支持浏览器时不应出现浏览器入口')
  } else if (!html.includes('data-pane-open-item="terminal"')) {
    fail('能力过滤', '终端入口不应受浏览器能力影响')
  } else {
    ok('能力过滤:不支持浏览器时隐藏其入口,终端不受影响')
  }
}

/* 4b) 没有工作区时:工作区文件入口消失(树是相对工作区根的,没有根就没有内容) */
{
  const { html } = await render({
    ...baseProps,
    state: pure.EMPTY_SIDE_PANE,
    workspacePath: null,
  })
  if (html.includes('data-pane-open-item="files"')) {
    fail('能力过滤(工作区)', '没有工作区时不应出现工作区文件入口')
  } else if (!html.includes('data-pane-open-item="terminal"')) {
    fail('能力过滤(工作区)', '终端入口不应受工作区影响')
  } else {
    ok('能力过滤:没有工作区时隐藏工作区文件入口,终端不受影响')
  }
}

/* 5) 审查标签已开:菜单里不再出现 review(单例) */
{
  const state = {
    tabs: [{ id: 'r1', type: 'review', openedAt: Date.now() }],
    activeTabId: 'r1',
  }
  const { html } = await render({ ...baseProps, state })
  // 空态不在,`data-pane-open-item` 只可能来自 `+` 菜单(未展开时不渲染)。
  // 这里断言的是:有 review 标签时,空态引导页不会重复给 review 入口。
  if (html.includes('data-pane-open-item="review"')) {
    fail('单例约束', '审查已开时不应再出现空态入口')
  } else {
    ok('单例约束:审查已开时空态不重复提供入口')
  }
}

/* 6) `+` 菜单必须 portal 到 body 且**定位到锚点旁**(不被 overflow 裁切) */
{
  const state = {
    tabs: [{ id: 't1', type: 'terminal', openedAt: Date.now() }],
    activeTabId: 't1',
  }
  const errors = []
  const original = console.error
  console.error = (...args) => errors.push(args.map((a) => a?.message ?? String(a)).join(' '))
  const root = createRoot(g.document.getElementById('root'))
  root.render(H(SidePane, { ...baseProps, state }))
  await tick(90)

  const addTrigger = g.document.querySelector('[data-pane-add-trigger]')
  if (!addTrigger) {
    console.error = original
    fail('浮层 portal', '找不到 `+` 按钮')
  } else {
    addTrigger.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))
    await tick(90)
    console.error = original

    const menu = g.document.querySelector('.pane-menu')
    if (!menu) {
      fail('浮层 portal', '点开 `+` 后没有出现菜单')
    } else {
      // 关键断言 1:菜单**不在** SidePane 子树里 —— 说明 portal 到了 body。
      // 内联渲染时它会被 `.pane-tab-viewport` 的 overflow 裁掉(实测 bug)。
      const insidePane = g.document.querySelector('.side-pane .pane-menu')
      const inBody = menu.parentElement === g.document.body
      // 关键断言 2:必须已完成定位(`data-positioned="true"`)。
      //
      // 这条守的是另一个实测 bug:锚点 ref 漏挂时,定位逻辑拿不到锚点,
      // 浮层会停在默认位置(曾表现为"菜单逃逸到界面最左侧")。
      // 只断言"portal 到 body"抓不到这个 —— 它确实在 body 里,只是位置错。
      // 用 `data-positioned` 而不是嗅探内联样式:React 把 `visibility: hidden`
      // 序列化成无空格的 `visibility:hidden`,字符串匹配极易写错而假通过。
      const positioned = menu.getAttribute('data-positioned') === 'true'
      const style = menu.getAttribute('style') ?? ''

      if (insidePane) {
        fail('浮层 portal', '菜单仍内联在面板内,会被 overflow 裁切')
      } else if (!inBody) {
        fail('浮层 portal', `菜单父节点不是 body:${menu.parentElement?.className}`)
      } else if (!positioned) {
        fail('浮层定位', `菜单停在测量态(锚点 ref 漏挂?):${style}`)
      } else {
        ok('浮层 portal+定位:`+` 菜单挂 body 且已定位到锚点旁')
      }
    }
  }
  root.unmount()
  await tick(30)
}

/* 7) 标签总览同样 portal 且定位 */
{
  const state = {
    tabs: [{ id: 't1', type: 'terminal', openedAt: Date.now() }],
    activeTabId: 't1',
  }
  const root = createRoot(g.document.getElementById('root'))
  root.render(H(SidePane, { ...baseProps, state }))
  await tick(90)
  const overviewTrigger = g.document.querySelector('.pane-overview')
  if (!overviewTrigger) {
    fail('浮层 portal(总览)', '找不到总览按钮')
  } else {
    overviewTrigger.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))
    await tick(90)
    const pop = g.document.querySelector('.pane-overview-pop')
    if (!pop) {
      fail('浮层 portal(总览)', '点开后没有出现总览')
    } else if (g.document.querySelector('.side-pane .pane-overview-pop')) {
      fail('浮层 portal(总览)', '总览仍内联在面板内')
    } else if (pop.getAttribute('data-positioned') !== 'true') {
      fail(
        '浮层定位(总览)',
        `总览停在测量态(锚点 ref 漏挂?):${pop.getAttribute('style') ?? ''}`,
      )
    } else {
      ok('浮层 portal+定位:标签总览挂 body 且已定位')
    }
  }
  root.unmount()
  await tick(30)
}

/* 8) 关掉最后一个标签 → 收起面板(只关掉其中一个则不动) */
{
  const { container, root, collapsedCalls } = await mountHarness({
    tabs: [tab('t1'), tab('t2')],
    activeTabId: 't1',
  })

  const closedOne = clickIn(container, '[data-tab-id="t2"] .pane-tab-close')
  await tick(90)
  const left = container.querySelectorAll('[data-tab-id]').length

  if (!closedOne) fail('清空即收起', '找不到第二个标签的关闭按钮')
  else if (left !== 1) fail('清空即收起', `关掉一个后应剩 1 个标签,实际 ${left}`)
  else if (collapsedCalls.length !== 0) {
    fail('清空即收起', `还有标签却被收起了:${JSON.stringify(collapsedCalls)}`)
  } else ok('清空即收起:关掉其中一个标签不动面板')

  clickIn(container, '[data-tab-id="t1"] .pane-tab-close')
  await tick(90)
  const aside = container.querySelector('.side-pane')
  const keptAlive = container.querySelector('.pane-open-shell') !== null

  if (collapsedCalls.length !== 1 || collapsedCalls[0] !== true) {
    fail('清空即收起', `应调用 onCollapsedChange(true),实际 ${JSON.stringify(collapsedCalls)}`)
  } else if (container.querySelectorAll('[data-tab-id]').length !== 0) {
    fail('清空即收起', '标签没有清空')
  } else if (!aside.className.includes('collapsed')) {
    fail('清空即收起', '面板没有进入折叠态')
  } else if (!keptAlive) {
    fail('清空即收起', '收起后内容被卸载了(应该保活)')
  } else {
    ok('清空即收起:关掉最后一个标签后面板折叠,且内容仍在 DOM(保活)')
  }

  root.unmount()
  await tick(30)
}

/* 9) 关闭全部标签:同样收起(按钮在 jsdom 里一定溢出可见) */
{
  const { container, root, collapsedCalls } = await mountHarness({
    tabs: [tab('a', 'review'), tab('b')],
    activeTabId: 'a',
  })

  const clicked = clickIn(container, '.pane-close-all')
  await tick(90)

  if (!clicked) fail('关闭全部即收起', '找不到「关闭全部」按钮')
  else if (collapsedCalls.length !== 1 || collapsedCalls[0] !== true) {
    fail('关闭全部即收起', `应调用 onCollapsedChange(true),实际 ${JSON.stringify(collapsedCalls)}`)
  } else if (container.querySelectorAll('[data-tab-id]').length !== 0) {
    fail('关闭全部即收起', '标签没有清空')
  } else {
    ok('关闭全部即收起:标签清空后收起面板')
  }

  root.unmount()
  await tick(30)
}

/* 10) 过渡的前提:宽度由内联样式驱动(0 ↔ 记忆宽度)+ 裁切层 + 常驻把手 */
{
  const state = { tabs: [tab('t1')], activeTabId: 't1' }
  const container = g.document.createElement('div')
  g.document.body.appendChild(container)
  const root = createRoot(container)
  const paint = (collapsed) => root.render(H(SidePane, { ...baseProps, state, collapsed }))

  paint(false)
  await tick(90)
  const expandedPane = container.querySelector('.side-pane')
  const expandedStyle = expandedPane?.getAttribute('style') ?? ''
  const clipLayer = container.querySelector('.side-pane > .side-pane-clip > .side-pane-inner')
  const handle = container.querySelector('.side-pane-expand')

  // jsdom 里父容器量到宽度 0 → 回落到 MIN_PANE_WIDTH(240px)。
  if (!/width:\s*240px/.test(expandedStyle)) {
    fail('宽度内联驱动', `展开态宽度应写在内联样式里(240px),实际:${expandedStyle}`)
  } else if (!clipLayer) {
    fail('宽度内联驱动', '缺少裁切层 .side-pane-clip > .side-pane-inner')
  } else if (!handle) {
    fail('宽度内联驱动', '展开把手应常驻 DOM(新挂载的元素跑不了过渡)')
  } else if (handle.getAttribute('aria-hidden') !== 'true' || handle.getAttribute('tabindex') !== '-1') {
    fail('宽度内联驱动', '展开态下把手应从无障碍树隐藏且不可聚焦')
  } else ok('过渡前提:展开态宽度内联 + 裁切层 + 把手常驻且已隐藏')

  paint(true)
  await tick(90)
  const collapsedStyle = container.querySelector('.side-pane')?.getAttribute('style') ?? ''
  const collapsedHandle = container.querySelector('.side-pane-expand')

  if (!/width:\s*0px/.test(collapsedStyle)) {
    // 写死 `width:0` 在 CSS 里就没有过渡终点可言 —— 这条守的就是它。
    fail('宽度内联驱动', `收起态宽度应为内联的 0px,实际:${collapsedStyle}`)
  } else if (collapsedHandle?.getAttribute('aria-hidden') === 'true') {
    fail('宽度内联驱动', '收起后把手应可被点击与聚焦')
  } else {
    ok('过渡前提:收起态宽度内联为 0px,把手恢复可点')
  }

  root.unmount()
  await tick(30)
}

/* 11) 过渡的载体在 CSS 里(组件测不到样式表,只能静态断言三条机制) */
{
  const css = readFileSync(new URL('../src/components/SidePane.css', import.meta.url), 'utf8')
  if (!/transition:\s*width var\(--side-pane-dur\)/.test(css)) {
    fail('过渡样式', '缺少 .side-pane 的 width 过渡')
  } else if (!/\.side-pane-clip\s*\{[^}]*overflow:\s*hidden/.test(css)) {
    fail('过渡样式', '.side-pane-clip 缺少 overflow:hidden(冻结宽度无处裁切)')
  } else if (!/\.side-pane-inner\s*\{[^}]*--side-pane-content-w/.test(css)) {
    fail('过渡样式', '.side-pane-inner 没有用冻结宽度变量')
  } else if (!/\.side-pane\.collapsed\s*\.side-pane-expand/.test(css)) {
    fail('过渡样式', '展开把手缺少两态样式(折叠末尾淡入)')
  } else if (!/@media \(prefers-reduced-motion: reduce\)/.test(css)) {
    fail('过渡样式', '缺少 prefers-reduced-motion 兜底')
  } else {
    ok('过渡样式:宽度过渡 + 裁切层 + 冻结宽度 + 把手淡入 + reduced-motion')
  }
}

/* 汇总 */
console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 渲染检查全部通过')