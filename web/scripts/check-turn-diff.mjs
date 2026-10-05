/**
 * 收尾变更卡片(TurnDiffCard)的渲染回归:
 *   node scripts/check-turn-diff.mjs
 *
 * 断言的是"这轮改了什么"必须讲清楚,以及三处点击区不串门:
 *   1) 单文件:标题点名文件、直接摊开行内 diff(不用再悬停);
 *   2) 多文件:标题给数量与 ±行数,默认列 3 个,展开后全量可数;
 *   3) 头部 → 审查面板;文件行 → 打开文件(两个意图不能互相吞掉);
 *   4) 悬停文件行出 diff 预览,且浮层 portal 到 body(不被 content-visibility 裁掉)。
 */
import { mkdirSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'

const here = dirname(fileURLToPath(import.meta.url))
const webRoot = join(here, '..')
const srcRoot = join(webRoot, 'src')
const outDir = join(webRoot, 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const bundle = join(outDir, 'turn-diff.mjs')
const fileOpenStub = join(outDir, 'fileOpen.stub.mjs')
const reviewStub = join(outDir, 'reviewOpen.stub.mjs')

/**
 * 两个"打开"动作的桩。
 *
 * 卡片通过模块级注册接入(fileOpen / reviewOpen,与生产同一套机制)。
 * esbuild 会把依赖内联进 bundle,桩文件用 `alias` 顶掉真实模块;桩内部把
 * 调用记到 `globalThis.__turnDiffOpens` 上,断言直接从那里读。
 */
writeFileSync(fileOpenStub, `
export function registerOpenFile() { return () => {} }
export function openWorkspaceFile(path) {
  const bag = (globalThis.__turnDiffOpens ??= { files: [], review: 0 })
  bag.files.push(path)
  return true
}
export function useFileOpen() { return openWorkspaceFile }
`)
writeFileSync(reviewStub, `
export function registerOpenReview() { return () => {} }
export function useOpenReview() {
  return () => {
    const bag = (globalThis.__turnDiffOpens ??= { files: [], review: 0 })
    bag.review += 1
    return true
  }
}
`)

/** 把 ../fileOpen 与 ../reviewOpen 换成桩(esbuild 的 alias 只认包名,
 *  这里用 onResolve 按解析后的绝对路径精确顶掉)。 */
const stubPlugin = {
  name: 'turn-diff-stubs',
  setup(pluginBuild) {
    const stubs = new Map([
      [join(srcRoot, 'fileOpen.ts'), fileOpenStub],
      [join(srcRoot, 'reviewOpen.ts'), reviewStub],
    ])
    pluginBuild.onResolve({ filter: /^\.\.\/(fileOpen|reviewOpen)$/ }, (args) => {
      const target = join(dirname(args.resolveDir), `${args.path.slice(3)}.ts`)
      const stub = stubs.get(target.replace(/\\/g, '/')) ?? stubs.get(target)
      if (!stub) return null
      return { path: stub }
    })
  },
}

await build({
  entryPoints: [join(srcRoot, 'components', 'TurnDiffCard.tsx')],
  outfile: bundle,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  jsx: 'automatic',
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  plugins: [stubPlugin],
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

/** 造一个 EditDiff(卡片只读 added/removed/lines/path)。 */
function diff(path, added, removed, lines = ['a', 'b']) {
  return {
    path,
    startLine: 1,
    removed,
    added,
    skipped: 0,
    lines: lines.map((text, i) => ({ kind: i < removed ? 'del' : 'add', text })),
  }
}

const single = [
  { path: 'web/src/fold.ts', kind: 'edit', added: 12, removed: 3, diffs: [diff('fold.ts', 12, 3)] },
]
const many = [
  { path: 'web/src/fold.ts', kind: 'edit', added: 12, removed: 3, diffs: [diff('fold.ts', 12, 3)] },
  { path: 'web/src/i18n.ts', kind: 'edit', added: 24, removed: 0, diffs: [diff('i18n.ts', 24, 0)] },
  { path: 'web/src/api.ts', kind: 'edit', added: 5, removed: 1, diffs: [diff('api.ts', 5, 1)] },
  { path: 'web/README.md', kind: 'write', added: 40, removed: 0, diffs: [diff('README.md', 40, 0)] },
  { path: 'web/src/toolDisplay.ts', kind: 'edit', added: 2, removed: 2, diffs: [diff('toolDisplay.ts', 2, 2)] },
]

/* ---- 单文件 ---- */
await act(async () => {
  root.render(H(mod.TurnDiffCard, { files: single, cwd: 'D:/Denia' }))
  await settle()
})
{
  const card = rootEl.querySelector('.turn-diff-card')
  if (!card) {
    fail('单文件渲染出卡片', rootEl.innerHTML.slice(0, 300))
  } else {
    ok('单文件渲染出卡片')
    check('标题点名文件', card.querySelector('.turn-diff-title').textContent, '编辑 fold.ts')
    check('±行数汇总到副标题', card.querySelector('.turn-diff-sub-default').textContent, '+12−3')
    check('悬停副标题给出下一步', card.querySelector('.turn-diff-sub-hover').textContent, '查看变更')
    ok(`单文件不列文件列表(行内直接给 diff):${card.querySelector('.turn-diff-list') === null}`)
    const inline = card.querySelector('.turn-diff-inline')
    if (!inline) fail('单文件直接摊开行内 diff', '没有 .turn-diff-inline')
    else ok('单文件直接摊开行内 diff')
  }
}

/* ---- 多文件 ---- */
await act(async () => {
  root.render(H(mod.TurnDiffCard, { files: many, cwd: 'D:/Denia' }))
  await settle()
})
{
  const card = rootEl.querySelector('.turn-diff-card')
  const rows = [...card.querySelectorAll('.turn-diff-file')]
  check('标题给出文件数量', card.querySelector('.turn-diff-title').textContent, '编辑 5 个文件')
  check('±行数按本轮累计汇总', card.querySelector('.turn-diff-sub-default').textContent, '+83−6')
  check('默认只列 3 个文件', rows.length, 3)
  const toggle = card.querySelector('.turn-diff-toggle')
  if (!toggle) fail('超出的文件以「再显示 N 个」收尾', '没有 .turn-diff-toggle')
  else {
    ok('超出的文件以「再显示 N 个」收尾')
    check('收尾计数是未列出的数量', toggle.textContent, '再显示 2 个文件')
    check('收起态 aria-expanded', toggle.getAttribute('aria-expanded'), 'false')
    await act(async () => {
      toggle.dispatchEvent(new g.window.MouseEvent('click', { bubbles: true }))
      await settle()
    })
    const all = [...card.querySelectorAll('.turn-diff-file')]
    check('展开后列出全部文件', all.length, 5)
    check('展开后变回收起', card.querySelector('.turn-diff-toggle').textContent, '收起')
  }
  // 目录淡/文件名亮:两份 span,合起来才是完整路径。
  const pathEl = card.querySelector('.turn-diff-file-path')
  check(
    '路径拆成目录+文件名两段',
    [pathEl.querySelector('.turn-diff-file-dir')?.textContent, pathEl.querySelector('.turn-diff-file-name')?.textContent],
    ['web/src/', 'fold.ts'],
  )
}

/* ---- 悬停预览:浮层必须 portal 出渲染根,且指针移得过去、停得住 ---- */
{
  const row = rootEl.querySelector('.turn-diff-file')
  check('未悬停时没有预览浮层', doc.querySelector('.turn-diff-preview'), null)
  // 卡片绑的是 pointer 事件(pointerenter/leave),而 jsdom 不合成
  // pointerenter —— 用 pointerover 冒泡过去,React 的委托监听能收到。
  await act(async () => {
    row.dispatchEvent(new g.window.Event('pointerover', { bubbles: true }))
    await settle()
  })
  const preview = doc.querySelector('.turn-diff-preview')
  if (!preview) {
    fail('悬停文件行出 diff 预览', '没有 .turn-diff-preview')
  } else {
    ok('悬停文件行出 diff 预览')
    check('预览已 portal 出渲染根(不被 content-visibility 裁剪)', rootEl.contains(preview), false)
    check('预览挂在 body 上', preview.parentElement === doc.body, true)
    ok('预览里带该文件的 ±行数')
  }

  /* 核心回归:指针离开文件行后不能立刻消失 —— 浮层 portal 在 body 上,
     指针从行移到浮层必然先"离开"行。没有这段缓冲就永远滚不到内容。 */
  await act(async () => {
    row.dispatchEvent(new g.window.Event('pointerout', { bubbles: true }))
    await settle()
  })
  check('离开文件行后浮层仍在(留出移动时间)', doc.querySelector('.turn-diff-preview') !== null, true)

  // 指针进入浮层本体:取消正在跑的关闭计时,浮层留下(可以滚动/选中)。
  await act(async () => {
    preview.dispatchEvent(new g.window.Event('pointerover', { bubbles: true }))
    await new Promise((r) => setTimeout(r, 400))
  })
  check('进入浮层本体后浮层保留(可以停下来滚动)', doc.querySelector('.turn-diff-preview') !== null, true)

  // 离开浮层本体:计时跑完才关。
  await act(async () => {
    preview.dispatchEvent(new g.window.Event('pointerout', { bubbles: true }))
    await new Promise((r) => setTimeout(r, 400))
  })
  check('离开浮层本体后关闭', doc.querySelector('.turn-diff-preview'), null)
}

/* ---- 三处点击区各走各的:头部→审查、文件行→读文件、文件夹钮→文件管理器 ---- */
{
  globalThis.__turnDiffOpens = { files: [], review: 0 }
  const card = rootEl.querySelector('.turn-diff-card')

  // 头部整行可点(那层透明覆盖层)→ 审查面板。
  await act(async () => {
    card.querySelector('.turn-diff-head-hit')
      .dispatchEvent(new g.window.MouseEvent('click', { bubbles: true }))
    await settle()
  })
  check('点头部打开审查面板', globalThis.__turnDiffOpens.review, 1)
  check('点头部不误开文件', globalThis.__turnDiffOpens.files, [])

  // 尾部的「查看变更」按钮:同样去审查面板,但不该被下层覆盖层吃两次。
  await act(async () => {
    card.querySelector('.turn-diff-view-btn')
      .dispatchEvent(new g.window.MouseEvent('click', { bubbles: true }))
    await settle()
  })
  check('「查看变更」按钮同样去审查面板', globalThis.__turnDiffOpens.review, 2)

  // 文件行 → 打开该文件(不是审查面板)。
  await act(async () => {
    card.querySelector('.turn-diff-file-main')
      .dispatchEvent(new g.window.MouseEvent('click', { bubbles: true }))
    await settle()
  })
  check('点文件行打开该文件', globalThis.__turnDiffOpens.files, ['web/src/fold.ts'])
  check('点文件行不打开审查面板', globalThis.__turnDiffOpens.review, 2)
}

/* ---- 无产物不渲染 ---- */
await act(async () => {
  root.render(H(mod.TurnDiffCard, { files: [], cwd: 'D:/Denia' }))
  await settle()
})
check('没有产物时不渲染任何东西', rootEl.querySelector('.turn-diff-card'), null)

/* ---- 中断轮次:已落盘的文件照样列,但要标出"这轮没跑完" ---- */
{
  await act(async () => {
    root.render(H(mod.TurnDiffCard, { files: many, cwd: 'D:/Denia', interrupted: true }))
    await settle()
  })
  const partial = rootEl.querySelector('.turn-diff-partial')
  if (!partial) {
    fail('中断轮次在标题上留痕', '没有 .turn-diff-partial')
  } else {
    ok('中断轮次在标题上留痕')
    check('标记文案说明是中断前的产物', partial.getAttribute('title'), '本轮未正常结束,以上是中断前已落盘的改动')
    // 中断不吞产物:文件数与正常轮次一致(此处 5 个,承接上一段的展开态)。
    check('中断轮次仍完整列出文件', rootEl.querySelectorAll('.turn-diff-file').length, 5)
  }
  // 正常完成:不留痕。加这个限定是因为"所有轮次都挂标记"会让标记失去意义。
  await act(async () => {
    root.render(H(mod.TurnDiffCard, { files: many, cwd: 'D:/Denia' }))
    await settle()
  })
  check('正常完成的轮次不挂中断标记', rootEl.querySelector('.turn-diff-partial'), null)
}

console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 变更卡片检查全部通过')
