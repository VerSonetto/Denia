/**
 * 侧栏「进行中任务」标记的运行时验收(jsdom)。
 *
 * `check-sidebar-memory.mjs` 只覆盖账本读写与源码断言;这条挂载真实的
 * SidebarWorkspaces,守住"静态扫描看不出来"的行为:
 * - running 结束(轮次完成)后,手动标记仍留在「进行中任务」里;
 * - 右键菜单在「全部任务」与「进行中任务」两种筛选下都能标记/移除;
 * - 移除标记且不在 running 时该行立即消失;
 * - 刷新(重新挂载)后标记从 localStorage 恢复;
 * - 空会话列表(首次加载失败时 sessionsLoaded 也会置真)不能清掉标记,
 *   拿到真实列表后才清理孤儿 id。
 *
 *   node scripts/check-sidebar-in-progress.mjs
 */
import assert from 'node:assert/strict'
import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const here = fileURLToPath(new URL('.', import.meta.url))
const srcRoot = join(here, '..', 'src')
const outDir = join(here, '..', 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })

/* appStore 会把整套网络栈拉进 bundle;这里只用到 bumpCatalogTick,
 * 用空实现替身隔离,测试只关心侧栏自身状态。 */
const stubAppStore = {
  name: 'stub-app-store',
  setup(build) {
    build.onResolve({ filter: /(^|\/)appStore$/ }, () => ({
      path: 'app-store',
      namespace: 'denia-stub',
    }))
    build.onLoad({ filter: /^app-store$/, namespace: 'denia-stub' }, () => ({
      contents: 'export const bumpCatalogTick = () => {}',
      loader: 'js',
    }))
  },
}

const entry = join(outDir, 'sidebar-in-progress-entry.tsx')
writeFileSync(
  entry,
  `import { SidebarWorkspaces } from ${JSON.stringify(join(srcRoot, 'components', 'sidebar', 'SidebarWorkspaces.tsx').replace(/\\/g, '/'))}
export { SidebarWorkspaces }
`,
)

const bundle = join(outDir, 'sidebar-in-progress.mjs')
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
  plugins: [stubAppStore],
  logLevel: 'silent',
})

const dom = new JSDOM('<!doctype html><html><body></body></html>', {
  url: 'http://localhost/',
  pretendToBeVisual: true,
})
const g = dom.window
for (const key of [
  'window', 'document', 'navigator', 'location', 'history',
  'HTMLElement', 'Element', 'Node', 'Event', 'CustomEvent', 'MouseEvent',
  'KeyboardEvent', 'EventTarget', 'MutationObserver', 'getComputedStyle',
  'requestAnimationFrame', 'cancelAnimationFrame', 'matchMedia',
  'localStorage', 'sessionStorage', 'DOMParser', 'Blob', 'URL', 'CSS',
  'SVGSVGElement', 'ResizeObserver', 'DataTransfer', 'DragEvent', 'PointerEvent',
]) {
  try {
    globalThis[key] = g[key]
  } catch {
    /* 只读全局跳过 */
  }
}

const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')
const { SidebarWorkspaces } = await import(pathToFileURL(bundle).href)
const H = React.createElement
const tick = (ms = 90) => new Promise((resolve) => setTimeout(resolve, ms))

const MARKS_KEY = 'denia.sidebar-in-progress.v1'
const FILTER_KEY = 'denia.sidebar-filter.v1'
const storedMarks = () => JSON.parse(g.localStorage.getItem(MARKS_KEY) ?? '[]')
const seedMarks = (ids) => {
  if (ids.length === 0) g.localStorage.removeItem(MARKS_KEY)
  else g.localStorage.setItem(MARKS_KEY, JSON.stringify(ids))
}

const now = Date.now()
const sessions = [
  { id: 's1', created_at: now - 60_000, excerpt: 'running session', title: null, cwd: 'D:/ws' },
  { id: 's2', created_at: now - 120_000, excerpt: 'marked session', title: null, cwd: 'D:/ws' },
  { id: 's3', created_at: now - 180_000, excerpt: 'idle session', title: null, cwd: 'D:/ws' },
]
const workspaces = [
  { id: 'w1', path: 'D:/ws', title: 'ws', createdAt: 0, sessionIds: ['s1', 's2', 's3'] },
]

const props = (overrides = {}) => ({
  sessions,
  workspaces,
  activeId: null,
  secondaryId: null,
  runningIds: { s1: true },
  sessionsLoaded: true,
  searchOpen: false,
  onSearchOpenChange: () => {},
  expandAllTick: 0,
  onAllExpandedChange: () => {},
  onOpenSession: () => {},
  onNewSession: () => {},
  onDeleteWorkspace: () => {},
  onDeleteSession: () => {},
  onDeleteUngrouped: () => {},
  ...overrides,
})

const mounted = []
function mount(nextProps) {
  const container = g.document.createElement('div')
  g.document.body.appendChild(container)
  const root = createRoot(container)
  root.render(H(SidebarWorkspaces, nextProps))
  mounted.push({ root, container })
  return {
    container,
    rerender: (overrides) => {
      root.render(H(SidebarWorkspaces, props(overrides)))
    },
    unmount: () => {
      root.unmount()
      container.remove()
    },
  }
}

const rows = (container) =>
  [...container.querySelectorAll('.session-row-wrap .excerpt')].map((node) => node.textContent)
const rowOf = (container, text) =>
  [...container.querySelectorAll('.session-row-wrap')].find((row) =>
    row.textContent.includes(text),
  )
const markCount = (container) =>
  container.querySelectorAll('[data-testid="in-progress-mark"]').length
const countBadge = (container) =>
  container.querySelector('.sidebar-filter-count')?.textContent ?? null
const clickFilter = (container, label) => {
  const tab = [...container.querySelectorAll('.sidebar-filter-tab')].find((node) =>
    node.textContent.includes(label),
  )
  assert.ok(tab, `找不到筛选标签:${label}`)
  tab.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))
}
const openMenu = (row) => {
  assert.ok(row, '要打开菜单的会话行不存在')
  row.dispatchEvent(
    new g.MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 40, clientY: 40 }),
  )
}
const menuLabels = () =>
  [...g.document.querySelectorAll('.sidebar-context-menu [role="menuitem"]')].map((node) =>
    node.textContent.trim(),
  )
const chooseMenu = (label) => {
  const item = [...g.document.querySelectorAll('.sidebar-context-menu [role="menuitem"]')].find(
    (node) => node.textContent.trim() === label,
  )
  assert.ok(item, `右键菜单里没有「${label}」:${menuLabels().join(' / ')}`)
  item.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))
}

try {
  /* 1) 进行中筛选 = running ∪ 手动标记;标记有图标,计数同口径。 */
  seedMarks(['s2'])
  g.localStorage.setItem(FILTER_KEY, 'running')
  const view = mount(props())
  await tick()
  assert.deepEqual(rows(view.container), ['running session', 'marked session'])
  assert.equal(markCount(view.container), 1, '只有手动标记的行显示图标')
  assert.ok(
    rowOf(view.container, 'marked session').querySelector('[data-testid="in-progress-mark"]'),
    '标记图标应挂在被标记的会话行上',
  )
  assert.equal(countBadge(view.container), '2', '角标 = running + 标记')

  /* 2) running 结束不自动移除标记:同一组件、runningIds 变空后仍在。 */
  view.rerender(props({ runningIds: {} }))
  await tick()
  assert.deepEqual(rows(view.container), ['marked session'], '轮次结束后标记仍在进行中')
  assert.equal(markCount(view.container), 1)
  assert.equal(countBadge(view.container), '1', 'running 结束后角标与列表同口径')

  /* 3) 进行中筛选下右键移除标记:不在 running 时该行立即消失。 */
  view.rerender(props())
  await tick()
  openMenu(rowOf(view.container, 'marked session'))
  await tick()
  assert.ok(menuLabels().includes('移除进行中标记'), '已标记会话的菜单应提供移除动作')
  chooseMenu('移除进行中标记')
  await tick()
  assert.deepEqual(rows(view.container), ['running session'], '移除标记后只剩 running 项')
  assert.equal(markCount(view.container), 0)
  assert.equal(countBadge(view.container), '1', '移除后角标同步减少')
  assert.deepEqual(storedMarks(), [], '移除后账本同步清空')

  /* 4) 「全部任务」下右键标记:行仍在,切回进行中筛选能看到。 */
  clickFilter(view.container, '全部任务')
  await tick()
  assert.deepEqual(rows(view.container), ['running session', 'marked session', 'idle session'])
  openMenu(rowOf(view.container, 'idle session'))
  await tick()
  assert.ok(menuLabels().includes('标记进行中'), '未标记会话的菜单应提供标记动作')
  chooseMenu('标记进行中')
  await tick()
  assert.deepEqual(storedMarks(), ['s3'], '标记写入本机账本')
  assert.equal(markCount(view.container), 1)
  assert.equal(countBadge(view.container), '2', '标记项计入进行中角标')

  /* 切换活跃会话只改高亮,不碰标记账本。 */
  view.rerender(props({ activeId: 's3' }))
  await tick()
  assert.equal(markCount(view.container), 1, '切换活跃会话后标记仍在')
  assert.ok(rowOf(view.container, 'idle session').classList.contains('active'))

  clickFilter(view.container, '进行中任务')
  await tick()
  assert.deepEqual(rows(view.container), ['running session', 'idle session'])

  /* 5) 模拟刷新:卸载后用同一份 localStorage 重新挂载,筛选与标记都恢复。 */
  view.unmount()
  const reloaded = mount(props({ runningIds: {} }))
  await tick()
  assert.deepEqual(rows(reloaded.container), ['idle session'], '刷新后标记仍在,running 已结束')
  assert.equal(markCount(reloaded.container), 1)
  reloaded.unmount()

  /* 6) 空列表保护:首屏失败时 sessionsLoaded 已置真但列表为空,不能清账本;
   *    拿到真实列表(其中没有该会话)后才清理孤儿。 */
  seedMarks(['s2'])
  const guard = mount(props({ sessions: [], runningIds: {}, sessionsLoaded: false }))
  await tick()
  assert.deepEqual(storedMarks(), ['s2'], '加载完成前不清理')
  guard.rerender({ sessions: [], runningIds: {}, sessionsLoaded: true })
  await tick()
  assert.deepEqual(storedMarks(), ['s2'], '空列表不能当作会话已全部删除')
  guard.rerender({
    sessions: sessions.filter((session) => session.id !== 's2'),
    runningIds: {},
    sessionsLoaded: true,
  })
  await tick()
  assert.deepEqual(storedMarks(), [], '拿到真实列表后清理已删除会话的孤儿标记')
  assert.deepEqual(rows(guard.container), [], '没有标记也没有 running 时列表为空')
  assert.equal(countBadge(guard.container), null, '空态不显示计数角标')
  assert.match(
    guard.container.querySelector('.empty-hint')?.textContent ?? '',
    /没有进行中的任务/,
    '空态提示与筛选口径一致',
  )
  guard.unmount()

  /* 7) 见过非空列表后,删掉最后一个会话仍要清理标记。 */
  seedMarks(['s1'])
  const last = mount(props({ sessions: [sessions[0]], runningIds: { s1: true } }))
  await tick()
  assert.deepEqual(storedMarks(), ['s1'], '会话仍在时不清理')
  last.rerender({ sessions: [], runningIds: {}, sessionsLoaded: true })
  await tick()
  assert.deepEqual(storedMarks(), [], '删掉最后一个会话后清理标记')
  last.unmount()

  console.log('ok sidebar in-progress mark: running union, menu, reload and empty-list guard')
} finally {
  for (const { root, container } of mounted) {
    try {
      root.unmount()
      container.remove()
    } catch {
      /* 已卸载 */
    }
  }
}
