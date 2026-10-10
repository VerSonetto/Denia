/**
 * 桌面标题栏三颗窗口按钮的光学对齐检查(纯几何,不需要浏览器)。
 *
 * ## 为什么只测图标几何
 *
 * 按钮盒由 `.desktop-window-btn` 统一给出:同宽同高 + flex 双向居中,三条完全一致。
 * 于是并排时"不在一条水平线上"只剩一个来源 —— **描边落在自己 viewBox 的哪一格**。
 *
 * 最小化那根横线画在 `M3 11.5h10`:16 格里偏下 3.5 格。按 size=15 渲染就是中线
 * 下方约 3.3px,肉眼看到的正是"最小化比方框和叉低一截"。盒子的对齐救不了它:
 * flex 居中的是一张空白偏多的画布,不是图形本身。
 *
 * ## 三层不变量
 *
 * 1. 四颗窗口图形(最小化/全屏/还原/关闭)的**描边中心线**包围盒,竖直中心
 *    == viewBox 中心。三颗并排时"竖直中心同一条"就是用户说的水平对齐。
 * 2. 按钮盒高度跟随标题栏内容盒(`height: 100%`)而不是硬编码像素:`.desktop-titlebar`
 *    是 `height: 38px` + `border-bottom: 1px`,可见带子只有 37px。写死 38px 的按钮
 *    比带子高 1px 且从顶部起画,三颗图形整体下沉 —— 彼此仍然齐,所以比第 1 条更隐蔽。
 * 3. 图形由 icons.tsx 的 `Svg` 包装渲染(自带 `display: block` + `flex: none`);
 *    换成裸 `<svg>` 会退回 inline 基线对齐,整张图再往下挪一截。
 *
 * 描边宽度不影响中心:方框 1.2、横线 1.25、叉 1.25 各自对称地往外扩,中心不动,
 * 所以只断言中心线包围盒。容差留 0.06 格(≈0.06px @16 格,即亚像素级)给手写坐标。
 *
 *   node scripts/check-desktop-titlebar.mjs
 */
import { readFileSync } from 'node:fs'
import { join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = fileURLToPath(new URL('.', import.meta.url))
const srcRoot = join(here, '..', 'src')

let failed = 0
const ok = (label) => console.log(`ok   ${label}`)
const fail = (label, detail) => {
  failed++
  console.log(`FAIL ${label}${detail ? `\n     ${detail}` : ''}`)
}

const GRID = 16
const CENTER = GRID / 2
const TOLERANCE = 0.06 // 约 0.06 格;size=15 时不到 0.06px
const RENDER_SIZE = 15 // 标题栏按这个尺寸渲染,换算成 px 只为把误差说人话

/* ---------- 路径 -> 中心线包围盒 ----------
 * 覆盖这几颗图形用到的指令:M/m L/l H/h V/v C/c Z/z。M 之后的隐式续坐标按 L 处理
 * (IconClose 就写成 `m4.5 4.5 7 7m0-7-7 7`)。
 * 限制(已知):曲线只取端点。圆角弧是凸包内部的 quarter-round,不外扩包围盒;
 * 若将来某颗图形的极值真的落在曲线弧上,这条会漏判,届时按弧做贝塞尔采样扩展。 */
const TOKEN = /[A-Za-z]|-?\d*\.?\d+(?:[eE][-+]?\d+)?/g

function* pointsOf(d) {
  const tokens = d.match(TOKEN) ?? []
  let i = 0
  let x = 0
  let y = 0
  let cmd = ''

  const num = () => {
    const t = tokens[i++]
    if (t === undefined || /[A-Za-z]/.test(t)) throw new Error(`路径坐标缺失: ${d}`)
    return Number(t)
  }

  while (i < tokens.length) {
    if (/[A-Za-z]/.test(tokens[i])) cmd = tokens[i++]
    const rel = cmd === cmd.toLowerCase()
    const abs = (v, base) => (rel ? base + v : v)

    switch (cmd.toUpperCase()) {
      case 'M':
        x = abs(num(), x)
        y = abs(num(), y)
        yield { x, y }
        cmd = cmd === 'M' ? 'L' : 'l'
        break
      case 'L':
        x = abs(num(), x)
        y = abs(num(), y)
        yield { x, y }
        break
      case 'H':
        x = abs(num(), x)
        yield { x, y }
        break
      case 'V':
        y = abs(num(), y)
        yield { x, y }
        break
      case 'C':
        num(); num(); num(); num() // 控制点:见上面的限制说明,不参与包围盒
        x = abs(num(), x)
        y = abs(num(), y)
        yield { x, y }
        break
      case 'Z':
        break
      default:
        throw new Error(`不支持的路径指令 "${cmd}",中心线包围盒需扩展: ${d}`)
    }
  }
}

function boxOf(shapes) {
  const pts = shapes.flatMap((s) =>
    s.d !== undefined
      ? [...pointsOf(s.d)]
      : [{ x: s.x, y: s.y }, { x: s.x + s.w, y: s.y + s.h }],
  )
  if (!pts.length) throw new Error('没有解析出任何几何')
  const xs = pts.map((p) => p.x)
  const ys = pts.map((p) => p.y)
  return { minX: Math.min(...xs), maxX: Math.max(...xs), minY: Math.min(...ys), maxY: Math.max(...ys) }
}

/* ---------- 从 icons.tsx 取原始几何 ----------
 * 读源码而不是 bundle 组件:断言的对象是"坐标写在第几格",只有源码里有权威形式。 */
const icons = readFileSync(join(srcRoot, 'components', 'icons.tsx'), 'utf8')
const NUMBER = '-?\\d*\\.?\\d+'

function shapesOf(name) {
  const marker = `export function ${name}(`
  const from = icons.indexOf(marker)
  if (from < 0) throw new Error(`icons.tsx 里找不到 ${name}`)
  const next = icons.indexOf('export function ', from + marker.length)
  const body = icons.slice(from, next < 0 ? undefined : next)

  const shapes = []
  for (const m of body.matchAll(/<path\s+d="([^"]+)"/g)) shapes.push({ d: m[1] })
  for (const m of body.matchAll(/<rect\b[^>]*>/g)) {
    const attr = (key) => {
      const hit = m[0].match(new RegExp(`\\b${key}="(${NUMBER})"`))
      if (!hit) throw new Error(`${name}: rect 缺 ${key}`)
      return Number(hit[1])
    }
    shapes.push({ x: attr('x'), y: attr('y'), w: attr('width'), h: attr('height') })
  }
  if (!shapes.length) throw new Error(`${name}: 没有 path/rect 几何`)
  return shapes
}

const caption = {
  IconWindowMinimize: '最小化',
  IconWindowMaximize: '全屏',
  IconWindowRestore: '还原',
  IconClose: '关闭',
}

const centers = {}
for (const [name, label] of Object.entries(caption)) {
  let box
  try {
    box = boxOf(shapesOf(name))
  } catch (error) {
    fail(`${label} 几何解析`, String(error?.message ?? error))
    continue
  }
  const cx = (box.minX + box.maxX) / 2
  const cy = (box.minY + box.maxY) / 2
  centers[name] = { cx, cy, label }

  const geometry = `x ${box.minX}–${box.maxX} y ${box.minY}–${box.maxY}`
  for (const [axis, c, tol] of [['竖直', cy, TOLERANCE], ['水平', cx, TOLERANCE]]) {
    const delta = c - CENTER
    if (Math.abs(delta) > tol) {
      const dir = axis === '竖直' ? (delta > 0 ? '下' : '上') : delta > 0 ? '右' : '左'
      fail(
        `${label} 图形${axis}居中`,
        `描边 ${geometry},中心 ${c.toFixed(2)} 比 viewBox 中线 ${CENTER} 偏${dir} ${Math.abs(delta).toFixed(2)} 格` +
          ` —— flex 居中的是画布,图形压不到按钮中线`,
      )
    } else {
      ok(`${label} 图形${axis}居中 (${geometry})`)
    }
  }
}

/* 症状直译:两两比对竖直中心。报错时说的是"这两颗不齐",不用读者再推一遍。 */
for (const [a, b] of [
  ['IconWindowMinimize', 'IconWindowMaximize'],
  ['IconWindowMaximize', 'IconClose'],
  ['IconWindowMinimize', 'IconWindowRestore'],
]) {
  const A = centers[a]
  const B = centers[b]
  if (!A || !B) continue
  const gap = A.cy - B.cy
  if (Math.abs(gap) > TOLERANCE) {
    fail(
      `${A.label} 与 ${B.label} 同线`,
      `竖直中心差 ${gap.toFixed(2)} 格(size=${RENDER_SIZE} 时 ${(Math.abs(gap) * RENDER_SIZE / GRID).toFixed(1)}px)`,
    )
  } else {
    ok(`${A.label} 与 ${B.label} 同线 (Δy=${gap.toFixed(2)})`)
  }
}

/* ---------- 第二层:按钮盒 ----------
 * 先剥注释:注释里常写"曾经是什么值"当解释(这里就写了 `height: 38px`),
 * 连着正文一起扫会把解释当成代码,报出一个不存在的硬编码。 */
const rawCss = readFileSync(join(srcRoot, 'styles', 'shell.css'), 'utf8')
const css = rawCss.replace(/\/\*[\s\S]*?\*\//g, '')

/** 取该选择器的全部规则块;媒体查询内外常各有一份(基础态 + 生效态)。 */
const blocksOf = (selector) => {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  return [...css.matchAll(new RegExp(`${escaped}\\s*\\{([^}]*)\\}`, 'g'))].map(([, body]) => body)
}
/** 生效态 = 带 display 的那一块;`display: none` 的基础态只用于隐藏整条。 */
const activeBlock = (selector) => {
  const blocks = blocksOf(selector)
  return blocks.find((b) => /display:\s*(?:inline-)?flex/.test(b)) ?? blocks.at(-1) ?? null
}

const barRule = activeBlock('.desktop-titlebar')
const actionsRule = activeBlock('.desktop-titlebar-actions')
const btnRule = activeBlock('.desktop-window-btn')
if (!barRule) fail('标题栏规则存在', 'shell.css 里找不到 .desktop-titlebar 规则块')
if (!actionsRule) fail('按钮容器规则存在', 'shell.css 里找不到 .desktop-titlebar-actions 规则块')
if (!btnRule) fail('按钮规则存在', 'shell.css 里找不到 .desktop-window-btn 规则块')

if (barRule) {
  const m = barRule.match(/height:\s*(\d+(?:\.\d+)?)px/)
  const border = /border-bottom:\s*1px/.test(barRule)
  if (!m) {
    fail('标题栏有显式高度', '找不到 height: Npx,按钮盒无从跟随内容盒')
  } else {
    const content = Number(m[1]) - (border ? 1 : 0)
    ok(`标题栏 height ${m[1]}px${border ? ' 含 1px 下边框,可见带子 ' + content + 'px' : ''}`)
  }
}

if (btnRule) {
  const need = [
    ['flex 布局', /display:\s*(?:inline-)?flex/],
    ['align-items: center', /align-items:\s*center/],
    ['justify-content: center', /justify-content:\s*center/],
    ['padding: 0', /padding:\s*0\s*[;}]/],
    ['height: 100%(跟随带子内容盒)', /height:\s*100%/],
  ]
  const missing = need.filter(([, re]) => !re.test(btnRule)).map(([label]) => label)
  if (missing.length) fail('按钮盒统一居中且贴合带子', `缺少 ${missing.join('、')}`)
  else ok('按钮盒统一居中且贴合带子(flex 双向 + 零 padding + height:100%)')

  if (/height:\s*\d+(?:\.\d+)?px/.test(btnRule)) {
    fail('按钮盒不写死像素高', '硬编码 px 与"标题栏 px + 1px 边框"必然差 1px,三颗整体下沉或上浮')
  }
  if (/line-height/.test(btnRule)) fail('按钮盒无 line-height 干扰', '规则里出现 line-height,会改变内容基线')
  else ok('按钮盒无 line-height 干扰')
}

/* 修饰类不许单独改盒尺寸:close 只该换 hover 配色。 */
const resized = [...css.matchAll(/\.desktop-window-btn\.[^{]*\{([^}]*)\}/g)]
  .filter(([, body]) => /(height|width|padding|margin|align-self|transform|line-height)\s*:/.test(body))
  .map(([, body]) => body.trim().replace(/\s*\n\s*/g, ' '))
if (resized.length) fail('修饰类不改盒尺寸', resized.join(' | '))
else ok('修饰类不改盒尺寸(close 只换 hover 配色)')

/* ---------- 第三层:渲染侧前提 ---------- */
const bar = readFileSync(join(srcRoot, 'components', 'DesktopTitleBar.tsx'), 'utf8')

if (/<svg\b/i.test(bar)) fail('标题栏不内联 svg', '组件里出现裸 <svg>,绕开 Svg 包装的 display:block / flex:none 前提')
else ok('标题栏只消费 icons.tsx 的 Svg 包装图形')

const used = [...bar.matchAll(/<(IconWindowMinimize|IconWindowMaximize|IconWindowRestore|IconClose)\b[^>]*?size=\{(\d+)\}/g)]
  .map(([, name, size]) => [caption[name], Number(size)])
if (used.length < 3) fail('三颗窗口按钮都在位', `只找到 ${used.length} 颗: ${used.map(([l]) => l).join(',')}`)
else ok(`三颗窗口按钮都在位 (${used.map(([l, s]) => `${l}=${s}`).join(' ')})`)

if (!/className="desktop-titlebar-actions"/.test(bar)) fail('按钮共用一个 actions 容器', '找不到 .desktop-titlebar-actions')
else ok('按钮共用一个 actions 容器(flex 居中同一条基线)')

const sized = new Set(used.map(([, s]) => s))
if (sized.size > 1) {
  console.log(`info 渲染尺寸不统一(${used.map(([l, s]) => `${l}=${s}`).join(' ')}):图形已各自居中,不影响对齐,只影响线条粗细观感`)
}

if (failed) {
  console.log(`\n${failed} FAIL`)
  process.exitCode = 1
} else {
  console.log('\n✓ 桌面标题栏对齐检查全部通过')
}
