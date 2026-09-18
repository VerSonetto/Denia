/**
 * 审查面板「提交 / 推送 / AI 生成」的回归网(纯函数 + jsdom 挂载)。
 *   node scripts/check-review-commit.mjs
 *
 * # 这两层各守什么
 *
 * - **纯函数层**:按钮可用性(`canCommit`/`canPush`)与错误引导文案。
 *   这类判断最容易写漏一个条件 —— 比如忘了判空提交信息,点了之后被后端
 *   400 弹回来;或者 detached HEAD 时仍然亮着推送按钮。散在 JSX 里很难发现。
 * - **组件层**:真挂载 ReviewPanel,断言
 *   "输入信息 → 点提交 → 发的是哪个请求、body 是什么"、
 *   "AI 生成的结果回填进编辑框"、"失败时显示错误与引导"。
 *   fetch 是 stub:守的是前端行为,后端由 cargo test 的 git 测试覆盖。
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

const outDir = join(here, '..', 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const pureOut = join(outDir, 'review-commit-pure.mjs')
await build({
  entryPoints: [join(srcRoot, 'gitApi.ts')],
  outfile: pureOut,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  logLevel: 'silent',
})
const pure = await import(pathToFileURL(pureOut).href)

{
  // canCommit:三个条件缺一不可。
  check(
    'canCommit:有改动 + 有信息 + 不忙 → 可提交',
    pure.canCommit({ hasChanges: true, message: 'feat(web):x', busy: false }),
    true,
  )
  check(
    'canCommit:没有改动 → 不可提交',
    pure.canCommit({ hasChanges: false, message: 'feat(web):x', busy: false }),
    false,
  )
  check(
    'canCommit:信息为空 → 不可提交',
    pure.canCommit({ hasChanges: true, message: '', busy: false }),
    false,
  )
  check(
    'canCommit:信息只有空白 → 不可提交(trim 后为空)',
    pure.canCommit({ hasChanges: true, message: '   \n\t ', busy: false }),
    false,
  )
  check(
    'canCommit:正在跑操作 → 不可提交(防重复点击)',
    pure.canCommit({ hasChanges: true, message: 'feat(web):x', busy: true }),
    false,
  )
}

{
  const repo = { isRepository: true, entries: [] }
  check('canPush:正常仓库 → 可推送', pure.canPush(repo, false), true)
  check(
    'canPush:游离 HEAD → 不可推送',
    pure.canPush({ ...repo, detached: true }, false),
    false,
  )
  check('canPush:不是仓库 → 不可推送', pure.canPush({ isRepository: false }, false), false)
  check('canPush:还没有状态 → 不可推送', pure.canPush(null, false), false)
  check('canPush:正在跑操作 → 不可推送', pure.canPush(repo, true), false)
}

{
  // 失败引导:每条错误码都要给出"下一步该做什么",未知码返回空串(只显示原文)。
  const cases = [
    ['git/no-upstream', 'git push -u'],
    ['git/no-remote', 'git remote add'],
    ['git/non-fast-forward', 'git pull --rebase'],
    ['git/auth-failed', 'SSH key'],
    ['git/identity-missing', '配置一次即可长期生效'],
    ['git/nothing-to-commit', '无需提交'],
  ]
  for (const [code, needle] of cases) {
    const hint = pure.failureHint(code)
    if (!hint.includes(needle)) {
      fail(`failureHint:${code}`, `引导里应包含 "${needle}",实际 "${hint}"`)
    } else {
      ok(`failureHint:${code} → ${hint.slice(0, 24)}…`)
    }
  }
  check('failureHint:未知错误码返回空串', pure.failureHint('git/something-else'), '')
}

/* ---------- 第二层:组件挂载 ---------- */

const entryFile = join(outDir, 'review-commit-entry.tsx')
writeFileSync(
  entryFile,
  `
import { ReviewPanel } from ${JSON.stringify(join(srcRoot, 'components', 'ReviewPanel.tsx').replace(/\\/g, '/'))}
export { ReviewPanel }
`,
)
const bundle = join(outDir, 'review-commit-bundle.mjs')
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

/** 记录每个请求:url + method + body。 */
let requests = []
/** 可编程的响应表:key 是 url 前缀。 */
let routes = {}

const jsonResponse = (value, status = 200) => ({
  ok: status >= 200 && status < 300,
  status,
  statusText: status === 200 ? 'OK' : 'Bad Request',
  text: async () => JSON.stringify(value),
  json: async () => value,
})

globalThis.fetch = async (url, init = {}) => {
  const href = String(url)
  requests.push({
    url: href,
    method: init.method ?? 'GET',
    body: init.body ? JSON.parse(init.body) : null,
  })
  for (const [prefix, handler] of Object.entries(routes)) {
    if (href.startsWith(prefix)) return handler(href, init)
  }
  return jsonResponse({ error: { code: 'test/unrouted', message: `没有配路由:${href}` } }, 400)
}

const statusWithChanges = {
  isRepository: true,
  repoRoot: 'D:/ws',
  branch: 'main',
  head: 'abc1234',
  detached: false,
  ahead: 0,
  behind: 0,
  entries: [
    {
      path: 'src/a.ts',
      indexStatus: ' ',
      worktreeStatus: 'M',
      staged: false,
      unstaged: true,
      untracked: false,
      conflicted: false,
      renamedFrom: null,
    },
  ],
}

const React = (await import('react')).default
const { createRoot } = await import('react-dom/client')
const mod = await import(pathToFileURL(bundle).href)
const H = React.createElement
const tick = (ms = 80) => new Promise((r) => setTimeout(r, ms))

async function mount(props) {
  const errors = []
  const original = console.error
  console.error = (...args) => errors.push(args.map((a) => a?.message ?? String(a)).join(' '))
  const root = createRoot(g.document.getElementById('root'))
  root.render(H(mod.ReviewPanel, props))
  await tick(140)
  console.error = original
  return { root, html: g.document.getElementById('root').innerHTML, errors }
}

/** 请求的路径(去掉 query),用于精确比较 —— 前缀匹配会让
 *  `/api/git/commit-message` 被误认成 `/api/git/commit`。 */
const pathOf = (request) => request.url.split('?')[0]

const q = (selector) => g.document.getElementById('root').querySelector(selector)

/** 给受控 textarea 赋值并触发 React 的 onChange。 */
function setTextareaValue(el, value) {
  const descriptor = Object.getOwnPropertyDescriptor(g.HTMLTextAreaElement.prototype, 'value')
  descriptor.set.call(el, value)
  el.dispatchEvent(new g.Event('input', { bubbles: true }))
}

const click = (el) => el.dispatchEvent(new g.MouseEvent('click', { bubbles: true }))

/* 1) 挂载:提交区在,提交按钮因"没有信息"而禁用 */
{
  routes = { '/api/git/status': () => jsonResponse(statusWithChanges) }
  requests = []
  const { errors } = await mount({ workspacePath: 'D:/ws', sessionId: 's1', selection: null })
  const hookError = errors.find((line) => /hook/i.test(line))
  if (hookError) fail('挂载', hookError)
  else if (!q('.review-commit')) fail('挂载', '没有渲染提交区')
  else if (!q('.review-commit-input')) fail('挂载', '没有提交信息编辑框')
  else if (!q('.review-commit-submit').disabled) {
    fail('挂载', '没有提交信息时提交按钮应当禁用')
  } else if (!q('.review-commit-generate').disabled) {
    fail('挂载', '没有选模型时 AI 生成按钮应当禁用')
  } else {
    ok('挂载:提交区就位,无信息/无模型时对应按钮禁用')
  }
}

/* 2) 填信息 → 提交按钮可用 → 点提交发出正确的请求 */
{
  routes = {
    '/api/git/status': () => jsonResponse(statusWithChanges),
    '/api/git/commit': () => jsonResponse({ ok: true, head: 'def5678', summary: '1 file changed' }),
  }
  requests = []
  const { root } = await mount({ workspacePath: 'D:/ws', sessionId: 's1', selection: null })
  const input = q('.review-commit-input')
  setTextareaValue(input, 'feat(web):加个提交区')
  await tick(60)
  const submit = q('.review-commit-submit')
  if (submit.disabled) {
    fail('提交', '填了信息后提交按钮应当可用')
  } else {
    click(submit)
    await tick(140)
    const commitReq = requests.find((item) => item.url.startsWith('/api/git/commit'))
    if (!commitReq) {
      fail('提交', `没有发出提交请求,实际:${JSON.stringify(requests.map((r) => r.url))}`)
    } else if (commitReq.method !== 'POST') {
      fail('提交', `提交应当用 POST,实际 ${commitReq.method}`)
    } else if (commitReq.body?.message !== 'feat(web):加个提交区') {
      fail('提交', `提交信息不对:${JSON.stringify(commitReq.body)}`)
    } else if (commitReq.body?.path !== 'D:/ws') {
      fail('提交', `路径不对:${JSON.stringify(commitReq.body)}`)
    } else if (q('.review-commit-input').value !== '') {
      fail('提交', '提交成功后编辑框应当清空')
    } else if (!g.document.getElementById('root').innerHTML.includes('def5678')) {
      fail('提交', '成功后应当显示提交号')
    } else {
      ok('提交:POST 正确 body,成功后清空输入并显示提交号')
    }
    // 提交后应重新拉状态(改动列表要刷新)。
    if (!requests.some((item) => item.url.startsWith('/api/git/status') && requests.indexOf(item) > 0)) {
      fail('提交', '提交成功后应当重新拉取状态')
    }
  }
  root.unmount()
  await tick(30)
}

/* 3) 提交失败:显示错误与"下一步"引导,输入内容保留(不能让用户重写) */
{
  routes = {
    '/api/git/status': () => jsonResponse(statusWithChanges),
    '/api/git/commit': () =>
      jsonResponse(
        {
          error: {
            code: 'git/identity-missing',
            message: '提交需要先配置 Git 身份:git config user.name ...',
          },
        },
        400,
      ),
  }
  requests = []
  const { root } = await mount({ workspacePath: 'D:/ws', sessionId: 's1', selection: null })
  setTextareaValue(q('.review-commit-input'), 'feat(web):会被拒绝')
  await tick(60)
  click(q('.review-commit-submit'))
  await tick(140)
  const html = g.document.getElementById('root').innerHTML
  if (!html.includes('review-commit-feedback')) {
    fail('提交失败', '应当显示失败反馈')
  } else if (!html.includes('git config user.name')) {
    fail('提交失败', '应当显示后端返回的原文')
  } else if (!html.includes('配置一次即可长期生效')) {
    fail('提交失败', '应当按错误码给出下一步引导')
  } else if (q('.review-commit-input').value !== 'feat(web):会被拒绝') {
    fail('提交失败', '失败后必须保留用户输入的内容')
  } else {
    ok('提交失败:显示错误 + 引导,且保留输入')
  }
  root.unmount()
  await tick(30)
}

/* 4) AI 生成:结果回填编辑框(不自动提交) */
{
  routes = {
    '/api/git/status': () => jsonResponse(statusWithChanges),
    '/api/git/commit-message': () => jsonResponse({ message: 'feat(server):由模型生成的提交信息' }),
  }
  requests = []
  const { root } = await mount({
    workspacePath: 'D:/ws',
    sessionId: 's1',
    selection: { provider: 'deepseek', model: 'deepseek-v4-flash' },
  })
  const generate = q('.review-commit-generate')
  if (generate.disabled) {
    fail('AI 生成', '选了模型后生成按钮应当可用')
  } else {
    click(generate)
    await tick(160)
    const req = requests.find((item) => item.url.startsWith('/api/git/commit-message'))
    if (!req) {
      fail('AI 生成', `没有发出生成请求:${JSON.stringify(requests.map((r) => r.url))}`)
    } else if (req.body?.provider !== 'deepseek' || req.body?.model !== 'deepseek-v4-flash') {
      fail('AI 生成', `应当复用对话区选择的模型:${JSON.stringify(req.body)}`)
    } else if (req.body?.path !== 'D:/ws') {
      fail('AI 生成', `路径不对:${JSON.stringify(req.body)}`)
    } else if (q('.review-commit-input').value !== 'feat(server):由模型生成的提交信息') {
      fail('AI 生成', `结果应当回填编辑框,实际 "${q('.review-commit-input').value}"`)
    } else if (requests.some((item) => pathOf(item) === '/api/git/commit')) {
      fail('AI 生成', '生成后不该自动提交(用户要先确认/修改)')
    } else {
      ok('AI 生成:复用对话区模型,结果回填编辑框且不自动提交')
    }
  }
  root.unmount()
  await tick(30)
}

/* 5) 推送:成功后刷新状态;无 upstream 时显示专门引导 */
{
  routes = {
    '/api/git/status': () => jsonResponse(statusWithChanges),
    '/api/git/push': () => jsonResponse({ ok: true, upstream: 'origin/main', summary: 'To github.com' }),
  }
  requests = []
  const { root } = await mount({ workspacePath: 'D:/ws', sessionId: 's1', selection: null })
  const push = q('.review-commit-push')
  if (push.disabled) {
    fail('推送', '正常仓库的推送按钮应当可用')
  } else {
    click(push)
    await tick(160)
    const req = requests.find((item) => item.url.startsWith('/api/git/push'))
    if (!req || req.method !== 'POST') {
      fail('推送', `推送应当是 POST,实际 ${JSON.stringify(req)}`)
    } else if (!g.document.getElementById('root').innerHTML.includes('origin/main')) {
      fail('推送', '成功反馈里应当带上游分支名')
    } else {
      ok('推送:POST 且成功反馈显示上游分支')
    }
  }
  root.unmount()
  await tick(30)
}

{
  routes = {
    '/api/git/status': () => jsonResponse(statusWithChanges),
    '/api/git/push': () =>
      jsonResponse(
        {
          error: {
            code: 'git/no-upstream',
            message: "当前分支 'main' 没有上游分支,无法推送。可在终端执行:git push -u origin main",
          },
        },
        400,
      ),
  }
  requests = []
  const { root } = await mount({ workspacePath: 'D:/ws', sessionId: 's1', selection: null })
  click(q('.review-commit-push'))
  await tick(160)
  const html = g.document.getElementById('root').innerHTML
  if (!html.includes('没有上游分支')) {
    fail('推送失败', '应当显示后端返回的原因')
  } else if (!html.includes('git push -u origin')) {
    fail('推送失败', '应当给出建立 upstream 的引导')
  } else {
    ok('推送失败(无 upstream):显示原因 + 建立上游的引导')
  }
  root.unmount()
  await tick(30)
}

/* 6) 游离 HEAD:推送按钮禁用(不该让用户点了才被后端拒) */
{
  routes = {
    '/api/git/status': () => jsonResponse({ ...statusWithChanges, detached: true, branch: null }),
  }
  requests = []
  const { root } = await mount({ workspacePath: 'D:/ws', sessionId: 's1', selection: null })
  if (!q('.review-commit-push').disabled) {
    fail('游离 HEAD', 'detached 时推送按钮应当禁用')
  } else {
    ok('游离 HEAD:推送按钮禁用')
  }
  root.unmount()
  await tick(30)
}

/* 7) 不是仓库:不渲染提交区(没有 git 就没有提交可言) */
{
  routes = { '/api/git/status': () => jsonResponse({ isRepository: false, entries: [] }) }
  requests = []
  const { root } = await mount({ workspacePath: 'D:/notrepo', sessionId: 's1', selection: null })
  if (q('.review-commit')) {
    fail('非仓库', '不是 git 仓库时不该渲染提交区')
  } else {
    ok('非仓库:不渲染提交区')
  }
  root.unmount()
  await tick(30)
}

/* ---------- 汇总 ---------- */
console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 审查面板提交/推送检查全部通过')
