/**
 * contentEditable 输入器的纯 DOM 操作层(零 React 依赖)。
 *
 * 结构纪律:编辑器内只有三类顶层节点——文本节点、`<br>`(Shift+Enter 产生)、
 * slash 卡片(`contenteditable="false"` 的 `.slash-chip`,携带 data-slash)。
 * 序列化时卡片展开为 `/name`,与 textarea 时代的草稿字符串完全同构,发送
 * 链路(命令裁定/技能收集/后端手势)因此零改动;粘贴统一降级纯文本,结构
 * 不会被富 HTML 污染。
 *
 * 「草稿偏移」= 序列化字符串里的字符下标,是所有触发检测/插入/光标操作的
 * 唯一坐标系:卡片占 `1 + name.length` 个字符(即 `/name`),卡片内部偏移
 * 一律钳到卡片之后。
 */

import { COMMAND_GLYPHS, glyphSvg } from '../lib/slashGlyphs'
import { scanSlashTokens } from './slash'

export type SlashChipKind = 'command' | 'skill'

/** 卡片在草稿里的字符数(`/${name}`)。 */
const chipTokenLength = (name: string): number => name.length + 1

/** 节点若是 slash 卡片,返回其技能/命令名。 */
function chipName(node: Node): string | null {
  if (node.nodeType !== Node.ELEMENT_NODE) return null
  const name = (node as HTMLElement).dataset?.slash
  return name || null
}

/** 单个节点的草稿长度(text 按字符、chip 按 `/name`、br 按 1、其余递归)。 */
function draftLength(node: Node): number {
  if (node.nodeType === Node.TEXT_NODE) return (node as Text).length
  const name = chipName(node)
  if (name) return chipTokenLength(name)
  if ((node as HTMLElement).tagName === 'BR') return 1
  let sum = 0
  node.childNodes.forEach((child) => {
    sum += draftLength(child)
  })
  return sum
}

/** 序列化编辑器为草稿字符串(卡片展开为 `/name`,br 为换行)。 */
export function serializeEditor(editor: HTMLElement): string {
  let out = ''
  const walk = (node: Node) => {
    if (node.nodeType === Node.TEXT_NODE) {
      out += node.textContent ?? ''
      return
    }
    if (node.nodeType !== Node.ELEMENT_NODE) return
    const el = node as HTMLElement
    const name = chipName(el)
    if (name) {
      out += `/${name}`
      return
    }
    if (el.tagName === 'BR') {
      out += '\n'
      return
    }
    // 浏览器异常结构兜底(理论上不会出现):块级元素视为换行,递归其内容。
    const block = el.tagName === 'DIV' || el.tagName === 'P'
    if (block && out.length > 0 && !out.endsWith('\n')) out += '\n'
    el.childNodes.forEach(walk)
    if (block && !out.endsWith('\n')) out += '\n'
  }
  editor.childNodes.forEach(walk)
  return out
}

/** 光标定位结果:容器 + 容器内偏移(容器可能是 editor 本身,偏移为子节点序号)。 */
interface CaretPosition {
  container: Node
  offset: number
}

/** 草稿偏移 → DOM 定位;落在卡片内部的偏移钳到卡片之后。 */
function locateOffset(editor: HTMLElement, offset: number): CaretPosition {
  const nodes = Array.from(editor.childNodes)
  let total = 0
  for (let index = 0; index < nodes.length; index++) {
    const node = nodes[index]!
    const length = draftLength(node)
    if (offset === total) return { container: editor, offset: index }
    if (offset > total && offset < total + length) {
      if (node.nodeType === Node.TEXT_NODE) return { container: node, offset: offset - total }
      return { container: editor, offset: index + 1 }
    }
    total += length
  }
  return { container: editor, offset: nodes.length }
}

/** 当前光标的草稿偏移(选区不在编辑器内时返回 0)。 */
export function caretOffsetIn(editor: HTMLElement): number {
  const selection = window.getSelection()
  if (!selection || selection.rangeCount === 0) return 0
  const { startContainer, startOffset } = selection.getRangeAt(0)
  if (startContainer === editor) {
    let total = 0
    const nodes = Array.from(editor.childNodes)
    for (let index = 0; index < Math.min(startOffset, nodes.length); index++) {
      total += draftLength(nodes[index]!)
    }
    return total
  }
  let total = 0
  for (const node of Array.from(editor.childNodes)) {
    if (node === startContainer) {
      return total + (node.nodeType === Node.TEXT_NODE ? startOffset : 0)
    }
    if (node.contains(startContainer)) {
      // 罕见嵌套兜底:递归求内部偏移(卡片不可编辑,不会成为容器)。
      let inner = 0
      let found = false
      const walk = (root: Node): void => {
        for (const child of Array.from(root.childNodes)) {
          if (found) return
          if (child === startContainer) {
            inner += child.nodeType === Node.TEXT_NODE ? startOffset : 0
            found = true
            return
          }
          if (child.contains(startContainer)) {
            walk(child)
            if (found) return
          }
          inner += draftLength(child)
        }
      }
      walk(node)
      return total + inner
    }
    total += draftLength(node)
  }
  return total
}

/** 把折叠光标放到草稿偏移处。 */
export function setCaretOffset(editor: HTMLElement, offset: number): void {
  const position = locateOffset(editor, Math.max(0, offset))
  const selection = window.getSelection()
  if (!selection) return
  const range = document.createRange()
  range.setStart(position.container, position.offset)
  range.collapse(true)
  selection.removeAllRanges()
  selection.addRange(range)
}

/** 选中草稿区间 [start, end)(供 execCommand 替换 token 用)。 */
export function selectRange(editor: HTMLElement, start: number, end: number): void {
  const from = locateOffset(editor, Math.max(0, Math.min(start, end)))
  const to = locateOffset(editor, Math.max(start, end))
  const selection = window.getSelection()
  if (!selection) return
  const range = document.createRange()
  range.setStart(from.container, from.offset)
  range.setEnd(to.container, to.offset)
  selection.removeAllRanges()
  selection.addRange(range)
}

/** 光标是否紧贴在某张卡片之后(此时序列化里的 `/name` 不该再触发直调弹层)。 */
export function caretRightAfterChip(editor: HTMLElement): boolean {
  const selection = window.getSelection()
  if (!selection || selection.rangeCount === 0) return false
  const { startContainer, startOffset } = selection.getRangeAt(0)
  if (startContainer !== editor || startOffset === 0) return false
  const previous = editor.childNodes[startOffset - 1]
  return previous ? chipName(previous) !== null : false
}

/** kind 兜底:command = 终端提示符,skill = 四角星(路径取自 components/icons)。 */
const CHIP_ICONS: Record<SlashChipKind, string> = {
  command:
    '<svg viewBox="0 0 16 16" width="11" height="11" fill="none" aria-hidden="true"><path d="m3 4.5 3 3-3 3M8 11.5h5" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  skill:
    '<svg viewBox="0 0 16 16" width="11" height="11" fill="none" aria-hidden="true"><path d="M6 2.5 7 5.2l2.7 1-2.7 1-1 2.7-1-2.7-2.7-1 2.7-1L6 2.5Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/><path d="m11.8 9.6.55 1.45 1.45.55-1.45.55-.55 1.45-.55-1.45-1.45-.55 1.45-.55.55-1.45Z" fill="currentColor"/></svg>',
}

/**
 * 图标按名字优先、kind 兜底:`/plan` 是计划、`/compact` 是压缩,都不是
 * shell 命令,不该长一个终端符。专属图标的数据源与候选菜单共用
 * (lib/slashGlyphs),改一处两边同步。
 */
const iconFor = (name: string, kind: SlashChipKind): string =>
  COMMAND_GLYPHS[name] ? glyphSvg(COMMAND_GLYPHS[name]!, 11) : CHIP_ICONS[kind]

/**
 * 构造一张卡片元素(原子、不可编辑内部:图标 + 名字,序列化为 `/name`)。
 * 名字不再渲染前导 `/`——识别交给图标(它是卡片的视觉锚点,配色提到
 * secondary),皮肤保持无底无框;序列化与草稿偏移仍按 `1 + name.length`
 * 计算,发送时展开为 `/name`,草稿链路零影响。
 */
export function createSlashChip(name: string, kind: SlashChipKind): HTMLElement {
  const chip = document.createElement('span')
  chip.className = 'slash-chip'
  chip.setAttribute('contenteditable', 'false')
  chip.dataset.slash = name
  chip.dataset.kind = kind
  const icon = document.createElement('span')
  icon.className = 'slash-chip-icon'
  icon.innerHTML = iconFor(name, kind)
  const label = document.createElement('span')
  label.className = 'slash-chip-label'
  label.textContent = name
  chip.append(icon, label)
  return chip
}

/**
 * 用草稿字符串整体重建编辑器内容(外部写路径:清空/回填/撤销优化),命中
 * 词典的 token 重建为卡片;不动 undo 栈——这些场景本来就是整段替换。
 */
export function renderDraft(
  editor: HTMLElement,
  text: string,
  kinds: ReadonlyMap<string, SlashChipKind>,
): void {
  const fragment = document.createDocumentFragment()
  const pushText = (value: string) => {
    const parts = value.split('\n')
    parts.forEach((part, index) => {
      if (index > 0) fragment.appendChild(document.createElement('br'))
      if (part) fragment.appendChild(document.createTextNode(part))
    })
  }
  for (const segment of scanSlashTokens(text, [...kinds.keys()])) {
    if (segment.name) {
      fragment.appendChild(createSlashChip(segment.name, kinds.get(segment.name) ?? 'skill'))
    } else {
      pushText(segment.text)
    }
  }
  editor.replaceChildren(fragment)
}
