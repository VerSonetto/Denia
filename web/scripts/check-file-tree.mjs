/**
 * 工作区文件树的回归网(纯函数 + jsdom 挂载)。
 *   node scripts/check-file-tree.mjs
 *
 * # 为什么这两层都要测
 *
 * 树这东西的 bug 几乎都藏在两个地方,而且都不是"看代码能看出来"的:
 * - **折叠语义**(纯函数层):祖先收起后后代必须整段消失、展开态要在刷新
 *   后保留、旧子项消失时节点要跟着清掉 —— 用鼠标点几下很难覆盖全;
 * - **懒加载**(组件层):展开时才发请求、重复展开不重复请求、切工作区
 *   整棵树重来 —— 这些是"发了几个请求"级别的断言,只有真挂载才看得到。
 *
 * 组件部分 stub 掉 `fetch`:它守的是"什么时候发请求、请求哪个目录",
 * 不是后端行为(后端由 `cargo test` 的 `tree_tests` 覆盖)。
 */
import { mkdirSync, writeFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'
import { JSDOM } from 'jsdom'

const here = fileURLToPath(new URL('.', import.meta.url))
const srcRoot = join(here, '..', 'src')

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

/* ---------- 第一层:纯函数 ---------- */

const tmp = join(here, '..', 'node_modules', '.shots')
mkdirSync(tmp, { recursive: true })
const pureOut = join(tmp, 'file-tree-pure.mjs')
await build({
  entryPoints: [join(srcRoot, 'fileTree.ts')],
  outfile: pureOut,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  logLevel: 'silent',
})
const pure = await import(pathToFileURL(pureOut).href)

const entry = (path, kind) => ({ path, name: path.slice(path.lastIndexOf('/') + 1), kind })

{
  // 根目录列举:目录优先,顺序即后端给的顺序。
  let tree = pure.emptyTree('D:/ws')
  tree = pure.withListing(
    tree,
    '',
    [entry('src', 'directory'), entry('a.ts', 'file')],
    false,
  )
  check('rows:根目录两层展开后列出两项', pure.visibleRows(tree).map((r) => r.entry.path), [
    'src',
    'a.ts',
  ])
  check('rows:深度从 0 起', pure.visibleRows(tree).map((r) => r.depth), [0, 0])

  // 展开 src 前:子项不可见。
  const collapsed = pure.toggleDir(tree, 'src')
  check('toggle:展开未加载的目录需要拉数据', collapsed.needLoad, true)
  check('toggle:展开后 expanded 为真', collapsed.state.dirs.src.expanded, true)
  check('toggle:加载中不再重复拉', pure.toggleDir(collapsed.state, 'src').needLoad, false)

  // 加载完 src 的子项:必须紧跟父行出现,且深度 +1。
  let loaded = pure.withListing(collapsed.state, 'src', [entry('src/main.rs', 'file')], false)
  check(
    'rows:子项紧跟父行且深度 +1',
    pure.visibleRows(loaded).map((r) => [r.entry.path, r.depth]),
    [
      ['src', 0],
      ['src/main.rs', 1],
      ['a.ts', 0],
    ],
  )

  // 收起 src:后代整段消失(不是留着只藏一层)。
  const reclosed = pure.toggleDir(loaded, 'src').state
  check(
    'rows:收起父目录后后代整段隐藏',
    pure.visibleRows(reclosed).map((r) => r.entry.path),
    ['src', 'a.ts'],
  )
  // 再展开:已有缓存,不该再拉。
  check('toggle:再展开用缓存不重复拉', pure.toggleDir(reclosed, 'src').needLoad, false)

  // 刷新:保留展开态(用户视角是"内容更新"而不是"树被重置")。
  const refreshed = pure.resetTree(loaded)
  check('refresh:清空已加载的子项', refreshed.dirs[''].children, null)
  check('refresh:保留子目录的展开态', refreshed.dirs.src?.expanded, true)
  check('refresh:清空已展开目录的子项(要重拉)', refreshed.dirs.src?.children, null)
  check('refresh:列出需要补拉的展开目录', pure.expandedDirs(refreshed), ['src'])
  check('refresh:丢弃全部旧条目', refreshed.entries['src/main.rs'], undefined)

  // 旧子项从磁盘消失:节点必须一起清掉,否则展开态挂在不存在的路径上。
  const relisted = pure.withListing(loaded, 'src', [], false)
  check('relist:消失的子项节点被清掉', relisted.dirs['src/main.rs'], undefined)
  check(
    'relist:父目录仍展开但没有子行',
    pure.visibleRows(relisted).map((r) => r.entry.path),
    ['src', 'a.ts'],
  )
}

{
  // 错误:展开失败要记在目录上,并保持展开(用户能看见错误与重试入口)。
  let tree = pure.emptyTree('D:/ws')
  tree = pure.withListing(tree, '', [entry('src', 'directory')], false)
  tree = pure.toggleDir(tree, 'src').state
  tree = pure.withError(tree, 'src', '读不到')
  check('error:记在对应目录上', pure.dirError(tree, 'src'), '读不到')
  check('error:根目录没有错误', pure.dirError(tree, ''), '')
  check('error:失败后仍是展开态', tree.dirs.src.expanded, true)
  // 收起(错误保留)→ 再展开:必须重试,并清掉旧错误。
  const collapsed = pure.toggleDir(tree, 'src').state
  check('error:收起时保留错误信息', pure.dirError(collapsed, 'src'), '读不到')
  const retry = pure.toggleDir(collapsed, 'src')
  check('error:收起再展开会重试', retry.needLoad, true)
  check('error:重试时清掉旧错误', pure.dirError(retry.state, 'src'), '')
}

{
  // 截断标记只挂在展开的目录行上。
  let tree = pure.emptyTree('D:/ws')
  tree = pure.withListing(tree, '', [entry('big', 'directory')], false)
  tree = pure.toggleDir(tree, 'big').state
  tree = pure.withListing(tree, 'big', [entry('big/f.txt', 'file')], true)
  const rows = pure.visibleRows(tree)
  check('truncated:标记挂在该目录行上', rows[0].truncated, true)
  check('truncated:根行不受影响', rows[1].truncated, false)
}

/* ---- 拖拽载荷 ---- */

{
  const payload = { path: 'src/main.rs', kind: 'file' }
  check('dnd:载荷往返', pure.decodeReference(pure.encodeReference(payload)), payload)
  // 目录与文件的引用文本必须与手打 `@` 的结果一致:目录带尾 `/`。
  check('dnd:文件引用文本', pure.referenceText({ path: 'a.ts', kind: 'file' }), '@a.ts')
  check('dnd:目录引用文本带尾斜杠', pure.referenceText({ path: 'src', kind: 'directory' }), '@src/')
  check(
    'dnd:含空格的路径用引号形式',
    pure.referenceText({ path: 'my docs/read me.md', kind: 'file' }),
    '@"my docs/read me.md"',
  )
  check(
    'dnd:含空格的目录保持引号打开',
    pure.referenceText({ path: 'my docs', kind: 'directory' }),
    '@"my docs/',
  )
  // 垃圾载荷不能抛(拖拽数据来自浏览器,什么都可能是)。
  check('dnd:非 JSON 返回 null', pure.decodeReference('not json'), null)
  check('dnd:形状不对返回 null', pure.decodeReference('{"path":"a.ts"}'), null)
  check('dnd:空路径返回 null', pure.decodeReference('{"path":"","kind":"file"}'), null)
  check('dnd:自定义 MIME 不是 text/plain', pure.REFERENCE_MIME === 'text/plain', false)
}

{
  // 落点插入:前后空白边界处理与 `applyMentionInsertion` 同一规则。
  check(
    'insert:空草稿直接插入',
    pure.insertReferenceAt('', 0, '@a.ts'),
    { text: '@a.ts ', caret: 6 },
  )
  check(
    'insert:插在中间不粘连',
    pure.insertReferenceAt('看这个', 1, '@a.ts'),
    { text: '看 @a.ts 这个', caret: 8 },
  )
  check(
    'insert:后面已有空白不叠空格',
    pure.insertReferenceAt('看 这个', 1, '@a.ts'),
    { text: '看 @a.ts 这个', caret: 7 },
  )
  check(
    'insert:落点越界钳到末尾',
    pure.insertReferenceAt('abc', 99, '@a.ts'),
    { text: 'abc @a.ts ', caret: 10 },
  )
}

/* ---------- 第二层:组件挂载(jsdom) ---------- */

const outDir = join(here, '..', 'node_modules', '.shots')
const entryFile = join(outDir, 'file-tree-entry.tsx')
writeFileSync(
  entryFile,
  `
import { FileTreePanel } from ${JSON.stringify(join(srcRoot, 'components', 'FileTreePanel.tsx').replace(/\\/g, '/'))}
export { FileTreePanel }
`,
)
const bundle = join(outDir, 'file-tree-bundle.mjs')
await build({
  entryPoints: [entryFile],
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
]) {
  try {
    globalThis[key] = g[key]
  } catch {
    /* 只读全局跳过 */
  }
}

/** 假的文件系统:path 前缀 → 该层的条目。同时记录每次请求的 dir。 */
const treeFixture = {
  '': [
    { name: 'src', path: 'src', kind: 'directory' },
    { name: 'README.md', path: 'README.md', kind: 'file' },
  ],
  src: [
    { name: 'nested', path: 'src/nested', kind: 'directory' },
    { name: 'main.rs', path: 'src/main.rs', kind: 'file' },
  ],
  'src/nested': [],
}
const requested = []
globalThis.fetch = async (url) => {
  const parsed = new URL(String(url), 'http://localhost')
  const dir = parsed.searchParams.get('dir') ?? ''
  requested.push(dir)
  if (!(dir in treeFixture)) {
    return {
      ok: false,
      status: 400,
      statusText: 'Bad Request',
      json: async () => ({ error: { code: 'fs/tree-bad-directory', message: '目录不存在' } }),
    }
  }
  return {
    ok: true,
    status: 200,
    statusText: 'OK',
    json: async () => ({
      root: 'D:/ws',
      dir,
      entries: treeFixture[dir],
      truncated: false,
    }),
  }
}

const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')
const mod = await import(pathToFileURL(bundle).href)
const H = React.createElement
const tick = (ms = 60) => new Promise((r) => setTimeout(r, ms))

async function mount(props) {
  const errors = []
  const original = console.error
  console.error = (...args) => errors.push(args.map((a) => a?.message ?? String(a)).join(' '))
  const root = createRoot(g.document.getElementById('root'))
  root.render(H(mod.FileTreePanel, props))
  await tick(120)
  console.error = original
  return { root, html: g.document.getElementById('root').innerHTML, errors }
}

const paths = () => Array.from(g.document.querySelectorAll('[data-tree-path]')).map((el) => el.getAttribute('data-tree-path'))

/**
 * 给受控 input 赋值并触发 React 的 onChange。
 *
 * jsdom 里直接改 `input.value` 不会让 React 认为值变了(React 装了 value 的
 * 属性劫持,只认它自己那套 tracker),所以必须走原型上的原生 setter 再补发
 * input 事件 —— 否则断言会假失败在"筛选没生效"上,而实际是测试没触发。
 */
function setInputValue(input, value) {
  const descriptor = Object.getOwnPropertyDescriptor(g.HTMLInputElement.prototype, 'value')
  descriptor.set.call(input, value)
  input.dispatchEvent(new g.Event('input', { bubbles: true }))
}

/* 1) 首屏:只拉根目录一次,列出根的两项 */
{
  requested.length = 0
  const { html, errors } = await mount({ workspacePath: 'D:/ws', sessionId: 's1' })
  const hookError = errors.find((line) => /hook/i.test(line))
  if (hookError) fail('首屏挂载', hookError)
  else if (JSON.stringify(requested) !== JSON.stringify([''])) {
    fail('首屏挂载', `应当只请求根目录一次,实际请求了 ${JSON.stringify(requested)}`)
  } else if (JSON.stringify(paths()) !== JSON.stringify(['src', 'README.md'])) {
    fail('首屏挂载', `行内容不对:${JSON.stringify(paths())}`)
  } else if (!html.includes('data-tree-kind="directory"')) {
    fail('首屏挂载', '目录行没有标记 kind')
  } else {
    ok('首屏挂载:只拉一次根目录,目录优先排序')
  }
}

/* 2) 点目录展开:才去拉那一层;重复点击用缓存 */
{
  requested.length = 0
  const root = g.document.getElementById('root')
  const srcRow = root.querySelector('[data-tree-path="src"]')
  if (!srcRow) {
    fail('懒加载', '找不到 src 行')
  } else {
    srcRow.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))
    await tick(120)
    if (JSON.stringify(requested) !== JSON.stringify(['src'])) {
      fail('懒加载', `展开应只拉 src,实际 ${JSON.stringify(requested)}`)
    } else if (JSON.stringify(paths()) !== JSON.stringify(['src', 'src/nested', 'src/main.rs', 'README.md'])) {
      fail('懒加载', `子项没有紧跟父行:${JSON.stringify(paths())}`)
    } else if (srcRow.getAttribute('aria-expanded') !== 'true') {
      fail('懒加载', '展开后 aria-expanded 不是 true')
    } else {
      ok('懒加载:展开才拉该层,子项紧跟父行')
    }
    // 收起再展开:已有缓存,不该再发请求。
    requested.length = 0
    srcRow.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))
    await tick(60)
    srcRow.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))
    await tick(120)
    if (requested.length !== 0) {
      fail('懒加载缓存', `重复展开不该再拉,实际 ${JSON.stringify(requested)}`)
    } else if (!paths().includes('src/main.rs')) {
      fail('懒加载缓存', '再展开后子项应立刻可见(缓存)')
    } else {
      ok('懒加载缓存:收起再展开不发请求')
    }
  }
}

/* 3) 拖拽:每行带自定义 MIME 的引用载荷 */
{
  const srcRow = g.document.getElementById('root').querySelector('[data-tree-path="src"]')
  const data = new Map()
  const fakeEvent = new g.Event('dragstart', { bubbles: true })
  fakeEvent.dataTransfer = {
    setData: (type, value) => data.set(type, value),
    effectAllowed: '',
  }
  srcRow.dispatchEvent(fakeEvent)
  const raw = data.get('application/x-denia-reference')
  const payload = raw ? pure.decodeReference(raw) : null
  if (!payload) fail('拖拽载荷', 'dragstart 没有写入自定义 MIME 数据')
  else if (payload.path !== 'src' || payload.kind !== 'directory') {
    fail('拖拽载荷', `载荷不对:${JSON.stringify(payload)}`)
  } else if (data.get('text/plain') !== 'src') {
    fail('拖拽载荷', '缺 text/plain 兜底(拖到外部目标时要有个可读路径)')
  } else {
    ok('拖拽载荷:自定义 MIME + text/plain 兜底')
  }
}

/* 4) 筛选:只在已加载的可见行里过滤,不触发新请求 */
{
  requested.length = 0
  const input = g.document.getElementById('root').querySelector('.file-tree-filter')
  setInputValue(input, 'main')
  await tick(80)
  if (requested.length !== 0) {
    fail('筛选', `筛选不该读盘,实际请求 ${JSON.stringify(requested)}`)
  } else if (JSON.stringify(paths()) !== JSON.stringify(['src/main.rs'])) {
    fail('筛选', `筛选结果不对:${JSON.stringify(paths())}`)
  } else {
    ok('筛选:在已加载行内过滤,零额外读盘')
  }
  setInputValue(input, '')
  await tick(60)
}

/* 5) 刷新:根目录 + 已展开目录一起重拉(只重拉根的话,展开子树会停在旧内容) */
{
  requested.length = 0
  const refresh = g.document.getElementById('root').querySelector('[data-file-tree-refresh]')
  refresh.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))
  await tick(160)
  if (JSON.stringify(requested) !== JSON.stringify(['', 'src'])) {
    fail('刷新', `刷新应重拉根目录与已展开目录,实际 ${JSON.stringify(requested)}`)
  } else if (!paths().includes('src/main.rs')) {
    fail('刷新', '刷新后已展开的子树应仍然可见')
  } else {
    ok('刷新:重拉根目录 + 已展开目录,展开态保留')
  }
}

/* 6) 读盘失败:显示错误与重试入口,而不是静默空树 */
{
  const { root } = await mount({ workspacePath: 'D:/gone', sessionId: 's2' })
  // 换掉 fixture 让它读不到(用一个不在 fixture 里的根)。
  globalThis.fetch = async () => ({
    ok: false,
    status: 400,
    statusText: 'Bad Request',
    json: async () => ({ error: { code: 'fs/tree-not-a-directory', message: '不是目录' } }),
  })
  const refresh = g.document.getElementById('root').querySelector('[data-file-tree-refresh]')
  refresh.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))
  await tick(120)
  const html = g.document.getElementById('root').innerHTML
  if (!html.includes('file-tree-error')) fail('失败态', '读盘失败应显示错误块')
  else if (!html.includes('file-tree-retry')) fail('失败态', '错误块里应有重试按钮')
  else ok('失败态:显示错误与重试入口')
  root.unmount()
  await tick(30)
}

/* ---------- 汇总 ---------- */
console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 文件树检查全部通过')
