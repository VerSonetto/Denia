/**
 * 左侧栏布局常量检查(纯源码,不引入浏览器)。
 *
 * 这里守的是几个"改一个数字就会错位,但单测看不出来"的约束:
 * - `.sidebar-primary` 的水平内边距 4px + 左对齐导航按钮的内边距 8px
 *   必须等于 `.sidebar-region-head` 的 12px 文字基线;
 * - 820px 断点下导航行高切到 44px,底部留白与区域间距保持 12px/6px 节奏;
 * - 收起态 56px rail 由 10px + 36px + 10px 撑满,不被展开态规则影响;
 * - 手动「进行中」标记是 flex 项且不参与压缩,标题自己省略而不是被图标遮挡。
 *
 *   node scripts/check-sidebar-layout.mjs
 */
import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('..', import.meta.url))
const shellCss = readFileSync(join(root, 'src/styles/shell.css'), 'utf8')
const foundationCss = readFileSync(join(root, 'src/styles/foundation.css'), 'utf8')
const responsiveCss = readFileSync(join(root, 'src/styles/responsive.css'), 'utf8')
const sessionRowSource = readFileSync(
  join(root, 'src/components/sidebar/SessionRow.tsx'),
  'utf8',
)

/** 从 `{` 开始按花括号配对取出块内容(已返回不含最外层括号)。 */
function blockAt(text, openIndex) {
  let depth = 0
  for (let index = openIndex; index < text.length; index += 1) {
    const char = text[index]
    if (char === '{') depth += 1
    else if (char === '}') {
      depth -= 1
      if (depth === 0) return text.slice(openIndex + 1, index)
    }
  }
  throw new Error(`花括号不配对(offset ${openIndex})`)
}

/** 选择器逐字转义后按"行首规则"匹配,避免 `.session-row` 命中
 * `.session-row-wrap .session-row` 这类后缀相同但优先级更高的规则。 */
function ruleStart(text, selector, label) {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const match = new RegExp(`(?:^|\\n)[ \\t]*${escaped}[ \\t]*\\{`).exec(text)
  assert.ok(match, `缺少 ${label}:${selector}`)
  return match.index + match[0].length - 1
}

/** 按完整选择器取出第一条规则的声明块。 */
function rule(text, selector) {
  return blockAt(text, ruleStart(text, selector, 'CSS 规则'))
}

/** 取 `@media (query) { ... }` 的整块内容。 */
function media(text, query) {
  return blockAt(text, ruleStart(text, `@media ${query}`, '媒体查询'))
}

/** CSS 声明 → Map;注释先剥离,避免注释里的分号污染前一条声明。 */
function declarations(body) {
  const clean = body.replace(/\/\*[\s\S]*?\*\//g, '')
  const result = new Map()
  for (const chunk of clean.split(';')) {
    const colon = chunk.indexOf(':')
    if (colon < 0) continue
    const name = chunk.slice(0, colon).trim()
    if (name) result.set(name, chunk.slice(colon + 1).trim())
  }
  return result
}

/** `padding` 简写 → 四边数值;只接受 px。 */
function sides(value) {
  assert.ok(value, '缺少 padding')
  const parts = value.trim().split(/\s+/).map((part) => {
    const number = Number.parseFloat(part)
    assert.ok(Number.isFinite(number), `无法解析长度:${part}`)
    return number
  })
  const [top, right = top, bottom = top, left = right] = parts
  return { top, right, bottom, left }
}

const value = (map, name, selector) => {
  const found = map.get(name)
  assert.ok(found !== undefined, `${selector} 缺少 ${name}`)
  return found
}

/* ---------- 展开态:水平基线 ---------- */

const sidebarDecls = declarations(rule(shellCss, '.sidebar'))
const primaryDecls = declarations(rule(shellCss, '.sidebar-primary'))
const newButtonDecls = declarations(rule(shellCss, '.sidebar-new-btn'))
const regionHeadDecls = declarations(rule(shellCss, '.sidebar-region-head'))
const regionDecls = declarations(rule(shellCss, '.sidebar-region'))
const wideDecls = declarations(rule(shellCss, '.sidebar-wide'))

const sidebarPadding = sides(value(sidebarDecls, 'padding', '.sidebar'))
const primaryPadding = sides(value(primaryDecls, 'padding', '.sidebar-primary'))
const newButtonPadding = sides(value(newButtonDecls, 'padding', '.sidebar-new-btn'))
const regionHeadPadding = sides(value(regionHeadDecls, 'padding', '.sidebar-region-head'))

assert.equal(value(primaryDecls, 'gap', '.sidebar-primary'), '6px')
assert.equal(value(newButtonDecls, 'gap', '.sidebar-new-btn'), '10px')
assert.equal(primaryPadding.bottom, 12, '.sidebar-primary 底部留白为 12px')
assert.equal(value(regionDecls, 'gap', '.sidebar-region'), '6px', '区域间距同为 6px')

/* 侧栏内容起点(12) + .sidebar-primary 4 + 左对齐按钮 8 = 24;
 * 侧栏内容起点(12) + 区域标题 12 = 24 —— 两侧文字左缘对齐。 */
assert.equal(
  sidebarPadding.left + primaryPadding.left + newButtonPadding.left,
  sidebarPadding.left + regionHeadPadding.left,
  '新建/扩展导航与工作区标题的 12px 基线不一致',
)
assert.equal(sidebarPadding.left, 12, '侧栏水平内边距应为 12px')
assert.equal(primaryPadding.left, 4, '.sidebar-primary 水平内边距应为 4px')
assert.equal(newButtonPadding.left, 8, '左对齐导航按钮内边距应为 8px')
assert.equal(regionHeadPadding.left, 12, '.sidebar-region-head 水平内边距应为 12px')

/* 宽态内容宽度吃满侧栏减左右 12px;若侧栏 padding 改了这里必须同步。 */
const rootDecls = declarations(rule(foundationCss, ':root'))
assert.equal(value(rootDecls, '--sidebar-w', ':root'), '280px', '展开态侧栏宽度')
assert.equal(
  value(wideDecls, 'width', '.sidebar-wide'),
  `calc(var(--sidebar-w) - ${sidebarPadding.left + sidebarPadding.right}px)`,
  '.sidebar-wide 宽度与侧栏水平内边距不同步',
)

/* 主按钮(新建会话)是居中填充按钮,不需要额外 margin 破坏 6px 间距节奏。 */
const primaryButtonDecls = declarations(
  rule(shellCss, '.sidebar-new-btn:not(.sidebar-extensions-btn)'),
)
assert.equal(primaryButtonDecls.get('margin-bottom'), undefined, '主按钮不应再叠 margin-bottom')
assert.equal(sides(value(primaryButtonDecls, 'padding', '主按钮')).top, 0)
assert.equal(sides(value(primaryButtonDecls, 'padding', '主按钮')).bottom, 0)

/* ---------- 窄屏断点 ---------- */

assert.equal(value(primaryDecls, '--nav-height', '.sidebar-primary'), '39px')
const media820 = media(shellCss, '(max-width: 820px)')
assert.match(
  media820,
  /\.sidebar-primary\s*\{\s*--nav-height:\s*44px;?\s*\}/,
  '820px 断点下导航行高应切到 44px',
)

const mobileSidebar = declarations(rule(responsiveCss, '.shell[data-mobile] .sidebar'))
assert.equal(value(mobileSidebar, 'width', '移动端侧栏'), 'min(86vw, 320px)')
assert.equal(value(mobileSidebar, 'padding-right', '移动端侧栏'), '12px')
assert.match(
  value(mobileSidebar, 'padding-left', '移动端侧栏'),
  /^max\(12px, env\(safe-area-inset-left\)\)$/,
  '移动端左内边距应保留 12px 下限并兼容安全区',
)
/* 320px 视口下抽屉只有 ≈275px,冻结 256px 的宽态内容会超出 251px 的
 * 可用内容区并被 overflow 裁掉;移动端必须改成吃满。 */
const mobileWide = declarations(rule(shellCss, '.shell[data-mobile] .sidebar-wide'))
assert.equal(value(mobileWide, 'width', '移动端宽态内容'), '100%')
assert.ok(media820.includes('.shell[data-mobile] .sidebar-wide'), '移动端宽度覆盖应位于 820px 断点内')

/* ---------- 收起态 rail ---------- */

const collapsedShell = declarations(rule(shellCss, '.shell[data-collapsed]'))
const collapsedSidebar = declarations(rule(shellCss, '.shell[data-collapsed] .sidebar'))
const railButton = declarations(rule(shellCss, '.rail-btn'))
assert.match(
  value(collapsedShell, 'grid-template-columns', '收起态 shell'),
  /^56px\b/,
  '收起态栅格列宽应为 56px',
)
assert.equal(value(collapsedSidebar, 'padding-left', '收起态侧栏'), '10px')
assert.equal(value(collapsedSidebar, 'padding-right', '收起态侧栏'), '10px')
assert.equal(value(railButton, 'width', '.rail-btn'), '36px')
assert.equal(10 + 36 + 10, 56, '收起态 56px 应恰好容纳 36px 控件与 10px 边距')

/* ---------- 「进行中」标记不会挤掉标题/时间/动作 ---------- */

const markDecls = declarations(rule(shellCss, '.in-progress-mark'))
assert.equal(value(markDecls, 'flex', '.in-progress-mark'), 'none', '标记不能参与压缩')
assert.equal(value(markDecls, 'color', '.in-progress-mark'), 'var(--accent)')
assert.equal(markDecls.get('position'), undefined, '标记不应脱离文档流覆盖标题')

const sessionRowDecls = declarations(rule(shellCss, '.session-row'))
const excerptDecls = declarations(rule(shellCss, '.session-row .excerpt'))
const timeDecls = declarations(rule(shellCss, '.session-time'))
const actionsDecls = declarations(rule(shellCss, '.row-actions'))
assert.equal(value(sessionRowDecls, 'gap', '.session-row'), '6px')
assert.equal(value(excerptDecls, 'flex', '.session-row .excerpt'), '1')
assert.equal(value(excerptDecls, 'min-width', '.session-row .excerpt'), '0')
assert.equal(value(excerptDecls, 'text-overflow', '.session-row .excerpt'), 'ellipsis')
assert.equal(value(timeDecls, 'flex', '.session-time'), 'none')
assert.equal(value(actionsDecls, 'flex', '.row-actions'), 'none')

/* 标记位于标题之后、行尾时间之前:宽度不够时先省略标题。 */
const excerptIndex = sessionRowSource.indexOf('className="excerpt"')
const markIndex = sessionRowSource.indexOf('className="in-progress-mark"')
const timeIndex = sessionRowSource.indexOf('className="session-time"')
assert.ok(excerptIndex >= 0 && markIndex >= 0 && timeIndex >= 0)
assert.ok(excerptIndex < markIndex, '标记应排在标题之后')
assert.ok(markIndex < timeIndex, '标记应排在行尾时间之前')
assert.ok(sessionRowSource.includes('marked &&'), '标记必须受 marked 控制')
assert.ok(
  sessionRowSource.includes('data-testid="in-progress-mark"'),
  '标记缺少稳定的测试锚点',
)

/* ---------- 深浅主题 ---------- */

for (const selector of [':root', ":root[data-theme='light']", ":root:not([data-theme='dark'])"]) {
  const theme = declarations(rule(foundationCss, selector))
  assert.ok(
    /^#[0-9a-f]{6}$/i.test(value(theme, '--accent', selector)),
    `${selector} 下 --accent 必须是实色,标记才在深浅主题都可见`,
  )
}

console.log('ok sidebar layout: 12px baseline, 44px/820px, rail geometry and mark flex')
