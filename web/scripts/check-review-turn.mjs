/**
 * 审查面板「本轮」来源的回归网:
 *   node scripts/check-review-turn.mjs
 *
 * # 它守什么
 *
 * 审查面板其余三个来源全走 `git status`/`git diff`,工作区不是仓库时它们
 * 全是空的。曾经的后果是:对话末尾的变更卡片点「查看变更」会打开一个只有
 * "不是 Git 仓库"的空面板 —— 而本轮改动的路径、±行数、diff 就在卡片里。
 *
 * 所以断言三件事:
 *   1) 非仓库时自动落在「本轮」,而不是对着空面板;
 *   2) 本轮来源不请求后端 diff(diff 来自对话流,已在点击时交过来);
 *   3) 有仓库时 git 来源仍然照常工作(本轮来源是**并存**,不是替代)。
 */
import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const here = fileURLToPath(new URL('.', import.meta.url))
const srcRoot = join(here, '..', 'src')
const outDir = join(here, '..', 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })

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
const ok = (label) => console.log(`ok   ${label}`)
const fail = (label, detail) => {
  failed++
  console.log(`FAIL ${label}${detail ? `\n     ${detail}` : ''}`)
}

/* 本轮数据由 turnChangesStore 写入(卡片点击时),组件从同一个 store 读。
 * 面板与 store 必须共享同一份模块实例,这里把 store 打成外部依赖,避免
 * esbuild 各打一份导致"写进去读不到"。
 *
 * 订阅语义按生产实现(写入即通知),否则测不到"写入后组件重渲染"这条
 * 真实路径。用 useSyncExternalStore 而非裸读:`__seed` 在 render 之前调用,
 * 裸读也能过,但那样就测不出"运行期写入 → 面板跟着更新"这条真实路径。 */
/* 本轮数据由 turnChangesStore 写入(卡片点击时),组件从同一个 store 读。
 *
 * esbuild 会把 stub **内联进 bundle**,于是"测试里 import 的那份"与
 * "组件里用的那份"是两个模块实例、两张 Map。状态必须挂在 globalThis 上
 * 才能跨实例共享 —— 这不是权宜之计,生产里 App 与面板同样是两个模块,
 * 靠的正是模块级单例 + 订阅。
 */
const storeStub = join(outDir, 'turnChanges.store.mjs')
writeFileSync(storeStub, `
import { useSyncExternalStore } from 'react'
const g = globalThis
if (!g.__turnStore) g.__turnStore = { byScope: new Map(), listeners: new Set() }
const store = g.__turnStore
function emit() { for (const l of store.listeners) l() }
export function setTurnChanges(scope, snap) { store.byScope.set(scope, snap); emit() }
export function peekTurnChanges(scope) { return store.byScope.get(scope) ?? null }
export function useTurnChanges(scope) {
  return useSyncExternalStore(
    (fn) => { store.listeners.add(fn); return () => store.listeners.delete(fn) },
    () => store.byScope.get(scope) ?? null,
    () => null,
  )
}
export function __seed(scope, snap) { store.byScope.set(scope, snap); emit() }
`)

const bundle = join(outDir, 'review-turn.mjs')
await build({
  entryPoints: [join(srcRoot, 'components', 'ReviewPanel.tsx')],
  outfile: bundle,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  jsx: 'automatic',
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  logLevel: 'silent',
  loader: { '.css': 'empty' },
  plugins: [{
    name: 'store',
    setup(b) {
      b.onResolve({ filter: /turnChangesStore$/ }, () => ({ path: storeStub }))
    },
  }],
})

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

const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')
const { act } = await import('react')
const H = React.createElement
const mod = await import(`${pathToFileURL(bundle).href}?t=${Date.now()}`)
const store = await import(`${pathToFileURL(storeStub).href}?t=${Date.now()}`)
const doc = g.document
const rootEl = doc.getElementById('root')
const root = createRoot(rootEl)
/**
 * 面板要经历 fetch → setStatus → setLoading(false) → effect 切来源,每一次
 * 都是独立的提交;act 之外还会补跑 effect。这里多冲几轮,免得把"还没跑到"
 * 误判成"逻辑不对"。
 */
const settle = async () => {
  for (let i = 0; i < 4; i += 1) {
    await act(async () => { await new Promise((r) => setTimeout(r, 30)) })
  }
}

/* fetch 桩:记录请求,按场景返回 git 状态。 */
const calls = []
let gitStatus = { isRepository: false, entries: [] }
globalThis.fetch = async (url) => {
  calls.push(String(url))
  const body = gitStatus
  return {
    ok: true,
    status: 200,
    text: async () => JSON.stringify(body),
  }
}

function diff(path, added, removed) {
  return {
    path,
    startLine: 1,
    added,
    removed,
    skipped: 0,
    lines: [
      { kind: 'del', text: 'old', oldNo: 1 },
      { kind: 'add', text: 'new', newNo: 1 },
    ].slice(0, added + removed),
  }
}

const turnFiles = [
  { path: 'src/a.ts', kind: 'edit', added: 3, removed: 1, diffs: [diff('src/a.ts', 3, 1)] },
  { path: 'src/b.ts', kind: 'write', added: 10, removed: 0, diffs: [diff('src/b.ts', 10, 0)] },
]

/* ---- 场景 1:非仓库 + 有本轮数据 → 自动落在「本轮」并列出文件 ---- */
{
  gitStatus = { isRepository: false, entries: [] }
  calls.length = 0
  // 种数据要放进 act:它会触发订阅者重渲染,与组件挂载是同一次提交。
  await act(async () => {
    store.__seed('s1', { turn: 3, files: turnFiles, at: Date.now() })
    root.render(H(mod.default, { workspacePath: 'D:/work', sessionId: 's1' }))
    await settle()
  })
  await act(async () => { await settle() })
  const tabs = [...rootEl.querySelectorAll('.review-source')]
  const active = tabs.find((el) => el.getAttribute('aria-selected') === 'true')
  check('非仓库时自动落在「本轮」', active?.textContent, '本轮')
  const rows = [...rootEl.querySelectorAll('.review-row-path')].map((el) => el.textContent)
  check('列出本轮改动的文件', rows, ['src/a.ts', 'src/b.ts'])
  check('未显示「不是 Git 仓库」空态', rootEl.querySelector('.review-empty-title')?.textContent, undefined)
  check('本轮来源不请求 git diff', calls.filter((c) => c.includes('/api/git/diff')), [])
  check('只拉了一次 git 状态', calls.filter((c) => c.includes('/api/git/status')).length, 1)

  // 展开一行:diff 直接来自对话流,不再发请求。
  await act(async () => {
    rootEl.querySelector('.review-row-head').dispatchEvent(new g.window.MouseEvent('click', { bubbles: true }))
    await settle()
  })
  ok('展开行后出现 diff 内容')
  check('展开仍不请求后端 diff', calls.filter((c) => c.includes('/api/git/diff')), [])
}

/* ---- 场景 2:非仓库 + 无本轮数据 → 回到"不是 Git 仓库"的空态 ---- */
{
  gitStatus = { isRepository: false, entries: [] }
  await act(async () => {
    // 换 sessionId 即换 scope(组件读 scopeKey(sessionId)):上一个场景写进
    // s1 的数据不会串进来。不卸载重挂 —— 那反而丢掉"面板在同一位置被
    // 复用"这条真实路径。
    store.__seed('s2', null)
    root.render(H(mod.default, { workspacePath: 'D:/work', sessionId: 's2' }))
    await settle()
  })
  check(
    '无本轮数据时显示非仓库空态',
    rootEl.querySelector('.review-empty-title')?.textContent,
    '不是 Git 仓库',
  )
  const tabs = [...rootEl.querySelectorAll('.review-source')]
  ok(`来源栏仍提供「本轮」入口:${tabs.some((el) => el.textContent === '本轮')}`)
  check('本轮来源为空时不列文件', rootEl.querySelectorAll('.review-row-path').length, 0)
}

/* ---- 场景 3:是仓库 → git 来源照常可用(本轮来源不替代它) ---- */
{
  gitStatus = {
    isRepository: true,
    repoRoot: 'D:/work',
    branch: 'main',
    head: 'abc1234',
    entries: [
      { path: 'src/c.ts', indexStatus: 'M', worktreeStatus: 'M', staged: false, unstaged: true, untracked: false, conflicted: false, renamedFrom: null },
    ],
  }
  await act(async () => {
    store.__seed('s3', { turn: 4, files: turnFiles, at: Date.now() })
    root.render(H(mod.default, { workspacePath: 'D:/work', sessionId: 's3' }))
    await settle()
  })
  const tabs = [...rootEl.querySelectorAll('.review-source')]
  const active = tabs.find((el) => el.getAttribute('aria-selected') === 'true')
  // 面板被复用时上一个场景停在 turn 上;切会话必须回到 git 来源的默认,
  // 否则新会话会顶着上一轮的"本轮"空列表。
  check('切会话后来源回到 git 默认', active?.textContent, '未暂存')
  const rows = [...rootEl.querySelectorAll('.review-row-path')].map((el) => el.textContent)
  check('git 来源照常列出改动', rows, ['src/c.ts'])
  // 手动切到「本轮」也能用。
  const turnTab = tabs.find((el) => el.textContent === '本轮')
  await act(async () => {
    turnTab.dispatchEvent(new g.window.MouseEvent('click', { bubbles: true }))
    await settle()
  })
  const turnRows = [...rootEl.querySelectorAll('.review-row-path')].map((el) => el.textContent)
  check('仓库里也能切到本轮来源', turnRows, ['src/a.ts', 'src/b.ts'])
}

console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 本轮来源检查全部通过')
