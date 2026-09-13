/**
 * hook 纪律回归网:
 *   1) 静态扫描:组件里"提前 return 之后仍调用 use*" —— React #310 隐患;
 *   2) 运行时用例:在 jsdom 里真实渲染 Transcript,走
 *      「空态 → 有乐观行」这条切换路径 —— 这正是回退后重发消息把整页
 *      打白屏的那条路径(React 18 无边界时会卸载整棵根树)。
 *
 *   node scripts/check-hooks.mjs
 *
 * 为什么需要运行时用例:静态扫描只能发现"hook 写在 return 之后"这一种
 * 形态;渲染期异常无法用 try/catch,只能靠真实渲染暴露。两者互补。
 */
import { readdirSync, readFileSync, statSync, mkdirSync } from 'node:fs'
import { spawnSync } from 'node:child_process'
import { dirname, join, extname, relative } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const webRoot = join(here, '..')
const srcRoot = join(webRoot, 'src')

let failed = 0
function ok(label) {
  console.log(`ok   ${label}`)
}
function fail(label, detail) {
  failed++
  console.log(`FAIL ${label}${detail ? `\n  ${detail}` : ''}`)
}

/* ---------- 1) 静态扫描:hook 不得出现在提前 return 之后 ---------- */

const ts = (await import(
  pathToFileURL(join(webRoot, 'node_modules', 'typescript', 'lib', 'typescript.js')).href
)).default

function walk(dir, out = []) {
  for (const name of readdirSync(dir)) {
    const full = join(dir, name)
    if (statSync(full).isDirectory()) walk(full, out)
    else if (['.ts', '.tsx'].includes(extname(name))) out.push(full)
  }
  return out
}

const isHookCall = (node) => {
  if (!ts.isCallExpression(node)) return false
  const callee = node.expression
  if (ts.isIdentifier(callee)) return /^use[A-Z]/.test(callee.text)
  if (ts.isPropertyAccessExpression(callee) && ts.isIdentifier(callee.expression)) {
    return callee.expression.text === 'React' && /^use[A-Z]/.test(callee.name.text)
  }
  return false
}

/** 该语句(含嵌套块,但不深入嵌套函数)里 return 语句的行号。 */
function returnsIn(stmt, sf) {
  const out = []
  const scan = (node) => {
    if (
      ts.isFunctionDeclaration(node) ||
      ts.isFunctionExpression(node) ||
      ts.isArrowFunction(node) ||
      ts.isMethodDeclaration(node)
    ) {
      return // 另一个 hook 作用域
    }
    if (ts.isReturnStatement(node)) {
      out.push(ts.getLineAndCharacterOfPosition(sf, node.getStart()).line + 1)
    }
    ts.forEachChild(node, scan)
  }
  scan(stmt)
  return out
}

const hookFindings = []
for (const file of walk(srcRoot)) {
  const sf = ts.createSourceFile(
    file,
    readFileSync(file, 'utf8'),
    ts.ScriptTarget.Latest,
    true,
    ts.ScriptKind.TSX,
  )
  const visitFn = (fn) => {
    const body = fn.body
    if (!body || !ts.isBlock(body)) return
    const statements = body.statements
    const sf2 = fn.getSourceFile()
    for (let i = 0; i < statements.length; i++) {
      const stmt = statements[i]
      const returnLines = returnsIn(stmt, sf2)
      if (returnLines.length === 0) continue
      // 末尾的普通 return 之后没有语句,安全。
      if (i === statements.length - 1 && !ts.isIfStatement(stmt)) continue
      const laterHooks = []
      for (let j = i + 1; j < statements.length; j++) {
        const scan = (node) => {
          if (isHookCall(node)) {
            laterHooks.push({
              name: ts.isIdentifier(node.expression)
                ? node.expression.text
                : `React.${node.expression.name.text}`,
              line: ts.getLineAndCharacterOfPosition(sf2, node.getStart()).line + 1,
            })
          }
          ts.forEachChild(node, scan)
        }
        scan(statements[j])
      }
      if (laterHooks.length > 0) {
        hookFindings.push({
          file: relative(webRoot, file).replace(/\\/g, '/'),
          fn: fn.name?.text ?? '(anonymous)',
          returnLine: returnLines[0],
          hooks: laterHooks,
        })
      }
    }
  }
  const visit = (node) => {
    if (
      ts.isFunctionDeclaration(node) ||
      ts.isFunctionExpression(node) ||
      ts.isArrowFunction(node) ||
      ts.isMethodDeclaration(node)
    ) {
      visitFn(node)
    }
    ts.forEachChild(node, visit)
  }
  visit(sf)
}

if (hookFindings.length === 0) {
  ok('静态扫描:没有"提前 return 之后的 hook"')
} else {
  for (const f of hookFindings) {
    fail(
      `静态扫描:${f.file} 的 ${f.fn}() 第 ${f.returnLine} 行提前 return,其后仍调用 hook`,
      `     ${f.hooks.map((h) => `${h.name}() @${h.line}`).join(', ')}\n` +
        '     回退把日志截断为空后走空态分支,再发消息即触发 React #310。',
    )
  }
}

/* ---------- 2) 运行时:真实渲染 Transcript 的空态 → 有乐观行 切换 ---------- */

const { build } = await import('esbuild')
const outDir = join(webRoot, 'node_modules', '.shots')
mkdirSync(outDir, { recursive: true })
const bundle = join(outDir, 'transcript.mjs')

await build({
  entryPoints: [join(srcRoot, 'components', 'transcript.tsx')],
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

// 每个场景独立 jsdom:React 抛错后进程内的渲染器状态不可复用。
function makeEnv() {
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
  return g
}

/** 在独立环境下渲染并返回结果;异常/React 报错都算失败。 */
async function renderScenario(name, buildSteps) {
  const g = makeEnv()
  const errors = []
  const origError = console.error
  console.error = (...a) => { errors.push(a.map((x) => (x && x.message) || String(x)).join(' ')) }
  try {
    const React = (await import('react')).default
    const { createRoot } = await import('react-dom/client')
    const mod = await import(`${pathToFileURL(bundle).href}?t=${Date.now()}-${name}`)
    const Transcript = mod.Transcript
    const H = React.createElement

    const root = createRoot(g.document.getElementById('root'))
    for (const { element, settle = 60 } of buildSteps(H, Transcript)) {
      root.render(element)
      await new Promise((r) => setTimeout(r, settle))
    }
    const html = g.document.getElementById('root').innerHTML
    const hookError = errors.find((line) => /hook/i.test(line))
    return { html, hookError, errorCount: errors.length }
  } catch (error) {
    return { html: '', hookError: error?.message ?? String(error), errorCount: errors.length }
  } finally {
    console.error = origError
  }
}

/**
 * 在子进程里跑一个场景。
 *
 * React 在渲染期异常后会把错误重新抛出到宿主环境:同一进程内渲染器
 * 状态已损坏,后续场景全部失真。每个场景一个进程,互不污染。
 */
function runScenarioInChild(name) {
  const result = spawnSync(process.execPath, [fileURLToPath(import.meta.url)], {
    env: { ...process.env, CHECK_SCENARIO: name },
    encoding: 'utf8',
    timeout: 120000,
  })
  const line = (result.stdout || '').trim().split('\n').filter(Boolean).pop() ?? ''
  const marker = 'SCENARIO_RESULT '
  const hit = (result.stdout || '')
    .split('\n')
    .find((l) => l.startsWith(marker))
  if (!hit) {
    return {
      crashed: true,
      hookError:
        (result.stderr || '').split('\n').find((l) => /hook/i.test(l))?.trim() ??
        (result.stderr || '').slice(-300) ??
        line,
      html: '',
    }
  }
  try {
    return { crashed: false, ...JSON.parse(hit.slice(marker.length)) }
  } catch {
    return { crashed: true, hookError: hit.slice(-200), html: '' }
  }
}

/** 场景定义:子进程按名字取用。 */
const SCENARIOS = {
  'rewind-resend': (H, Transcript) => [
    { element: H(Transcript, { nodes: [], pendingMessages: [] }) },
    { element: H(Transcript, { nodes: [], pendingMessages: [{ text: '重新发送的消息' }] }) },
  ],
  'back-to-empty': (H, Transcript) => [
    { element: H(Transcript, { nodes: [], pendingMessages: [{ text: 'x' }] }) },
    { element: H(Transcript, { nodes: [], pendingMessages: [] }) },
  ],
  oscillate: (H, Transcript) => {
    const node = { kind: 'user', text: '历史消息', anchor: 1 }
    return [
      { element: H(Transcript, { nodes: [], pendingMessages: [] }) },
      { element: H(Transcript, { nodes: [node], pendingMessages: [] }) },
      { element: H(Transcript, { nodes: [], pendingMessages: [{ text: 'y' }] }) },
      { element: H(Transcript, { nodes: [node], pendingMessages: [] }) },
    ]
  },
  'compacting-flag': (H, Transcript) => [
    { element: H(Transcript, { nodes: [], pendingMessages: [], compactingAt: Date.now() }) },
    { element: H(Transcript, { nodes: [], pendingMessages: [], compactingAt: null }) },
    { element: H(Transcript, { nodes: [], pendingMessages: [{ text: 'z' }], compactingAt: null }) },
  ],
}

// 子进程分支:跑单个场景并以机器可读行回报。
if (process.env.CHECK_SCENARIO) {
  const name = process.env.CHECK_SCENARIO
  const result = await renderScenario(name, SCENARIOS[name])
  process.stdout.write(
    `SCENARIO_RESULT ${JSON.stringify({
      html: result.html.slice(0, 400),
      hookError: result.hookError ?? null,
    })}\n`,
  )
  process.exit(0)
}

/** 跑一个场景并断言。 */
function checkScenario(name, label, expectText) {
  const result = runScenarioInChild(name)
  if (result.hookError) {
    fail(`${label} 抛错`, `     ${String(result.hookError).slice(0, 220)}`)
    return
  }
  if (expectText && !String(result.html).includes(expectText)) {
    fail(`${label} 未渲染出预期内容`, `     DOM: ${String(result.html).slice(0, 160)}`)
    return
  }
  ok(label)
}

checkScenario('rewind-resend', '运行时:空态 → 乐观行(回退后重发的原始路径)', '重新发送的消息')
checkScenario('back-to-empty', '运行时:乐观行 → 空态 切回')
checkScenario('oscillate', '运行时:空态 ↔ 有内容 反复切换')
checkScenario('compacting-flag', '运行时:压缩占位行开合(compactingAt 切换)', 'z')

/* ---------- 3) 运行时:ErrorBoundary 确实能兜住崩溃(不白屏) ---------- */

const boundaryBundle = join(outDir, 'boundary.mjs')
await build({
  entryPoints: [join(srcRoot, 'components', 'ErrorBoundary.tsx')],
  outfile: boundaryBundle,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  jsx: 'automatic',
  external: ['react', 'react-dom', 'react-dom/client', 'react/jsx-runtime'],
  logLevel: 'silent',
  loader: { '.css': 'empty' },
})

if (process.env.CHECK_BOUNDARY) {
  const g = makeEnv()
  const origError = console.error
  console.error = () => {}
  const React = (await import('react')).default
  const { createRoot } = await import('react-dom/client')
  const { ErrorBoundary } = await import(`${pathToFileURL(boundaryBundle).href}?t=${Date.now()}`)
  const H = React.createElement

  function Boom() {
    throw new Error('模拟渲染期异常')
  }
  const rootEl = g.document.getElementById('root')
  const root = createRoot(rootEl)
  try {
    root.render(
      H('div', { className: 'shell' },
        H('aside', { className: 'sidebar' }, '侧栏'),
        H(ErrorBoundary, null, H(Boom)),
      ),
    )
  } catch { /* 边界会吞掉渲染异常 */ }
  await new Promise((r) => setTimeout(r, 80))
  console.error = origError
  process.stdout.write(`SCENARIO_RESULT ${JSON.stringify({ html: rootEl.innerHTML.slice(0, 500) })}\n`)
  process.exit(0)
}

{
  const result = spawnSync(process.execPath, [fileURLToPath(import.meta.url)], {
    env: { ...process.env, CHECK_BOUNDARY: '1' },
    encoding: 'utf8',
    timeout: 120000,
  })
  const marker = 'SCENARIO_RESULT '
  const hit = (result.stdout || '').split('\n').find((l) => l.startsWith(marker))
  const html = hit ? JSON.parse(hit.slice(marker.length)).html : ''
  if (!html.includes('侧栏')) {
    fail('运行时:ErrorBoundary 未保住兄弟节点(侧栏消失 = 白屏)', `     DOM: ${html.slice(0, 200)}`)
  } else if (!html.includes('render-error')) {
    fail('运行时:ErrorBoundary 没渲染降级视图', `     DOM: ${html.slice(0, 200)}`)
  } else {
    ok('运行时:ErrorBoundary 兜住崩溃,兄弟节点存活(不白屏)')
  }
}

/* ---------- 汇总 ---------- */
console.log('')
if (failed > 0) {
  console.log(`✗ ${failed} 项失败`)
  process.exit(1)
}
console.log('✓ 全部通过')
