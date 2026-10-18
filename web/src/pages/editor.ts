/**
 * contentEditable 输入器的纯 DOM 操作层(零 React 依赖)。
 *
 * 结构纪律:编辑器内只有文本节点、`<br>`(Shift+Enter 产生)和不可编辑卡片。
 * slash 卡片携带 data-slash,文件卡片携带完整引用 data-file-mention。
 * 序列化时卡片展开为 `/name` 或 `@path`,与纯文本草稿完全同构,发送
 * 链路(命令裁定/技能收集/后端手势)因此零改动;粘贴统一降级纯文本,结构
 * 不会被富 HTML 污染。
 *
 * 「草稿偏移」= 序列化字符串里的字符下标,是所有触发检测/插入/光标操作的
 * 唯一坐标系:卡片长度按其完整序列化 token 计算,卡片内部偏移
 * 一律钳到卡片之后。
 */

import { COMMAND_GLYPHS, glyphSvg } from '../lib/slashGlyphs'
import { scanSlashTokens } from './slash'
import { activeAtToken, formatFileMention, type MentionCandidate, type ActiveAtToken } from './mention'

export type SlashChipKind = 'command' | 'skill'

function chipToken(node: Node): string | null {
  if (node.nodeType !== Node.ELEMENT_NODE) return null
  const element = node as HTMLElement
  return element.dataset.fileMention || (chipName(node) ? `/${chipName(node)}` : null)
}

/** 节点若是 slash 卡片,返回其技能/命令名。 */
function chipName(node: Node): string | null {
  if (node.nodeType !== Node.ELEMENT_NODE) return null
  const name = (node as HTMLElement).dataset?.slash
  return name || null
}

/** 单个节点的草稿长度(text 按字符、chip 按完整 token、br 按 1、其余递归)。 */
function draftLength(node: Node): number {
  if (node.nodeType === Node.TEXT_NODE) return (node as Text).length
  const token = chipToken(node)
  if (token) return token.length
  if ((node as HTMLElement).tagName === 'BR') return 1
  let sum = 0
  node.childNodes.forEach((child) => {
    sum += draftLength(child)
  })
  return sum
}

/** 序列化编辑器为草稿字符串(卡片展开为完整 token,br 为换行)。 */
export function serializeEditor(editor: HTMLElement): string {
  let out = ''
  const walk = (node: Node) => {
    if (node.nodeType === Node.TEXT_NODE) {
      out += node.textContent ?? ''
      return
    }
    if (node.nodeType !== Node.ELEMENT_NODE) return
    const el = node as HTMLElement
    const token = chipToken(el)
    if (token) {
      out += token
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

/**
 * 编辑器是否"实际为空":只有 `<br>` 与空文本节点才算空。
 *
 * 不能只看 [`serializeEditor`] 的字符串:浏览器删掉最后一段文字后常补一个
 * 占位 `<br>`(contenteditable 的固有行为),它会被序列化成 `"\n"`,
 * 于是"非空"成立、占位提示消失。占位提示必须跟**内容**走,不跟 DOM 结构走。
 */
export function draftIsEmpty(node: Node): boolean {
  for (const child of Array.from(node.childNodes)) {
    if (child.nodeType === Node.TEXT_NODE) {
      if ((child.textContent ?? '').length > 0) return false
      continue
    }
    if (child.nodeType !== Node.ELEMENT_NODE) continue
    if (chipToken(child)) return false
    if ((child as HTMLElement).tagName === 'BR') continue
    if (!draftIsEmpty(child)) return false
  }
  return true
}

/**
 * 把"空/非空"记到 `data-empty` 上:占位提示的显隐只认这个标记,不看
 * `:empty`(它会被占位 `<br>` 骗过)。
 */
export function syncEmptyMark(editor: HTMLElement): boolean {
  const empty = draftIsEmpty(editor)
  if (empty) editor.dataset.empty = 'true'
  else delete editor.dataset.empty
  return empty
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

function offsetIn(editor: HTMLElement, startContainer: Node, startOffset: number): number {
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
    if (chipToken(node) && (node === startContainer || node.contains(startContainer))) {
      return total + draftLength(node)
    }
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

/** 当前光标的草稿偏移(选区不在编辑器内时返回 0)。 */
export function caretOffsetIn(editor: HTMLElement): number {
  const selection = window.getSelection()
  if (!selection || selection.rangeCount === 0) return 0
  const range = selection.getRangeAt(0)
  if (!editor.contains(range.startContainer)) return 0
  return offsetIn(editor, range.startContainer, range.startOffset)
}

/** 菜单夺焦前保存整个选区;没有编辑器选区时在草稿末尾插入。 */
export function selectionOffsetsIn(editor: HTMLElement): { start: number; end: number } {
  const selection = window.getSelection()
  const range = selection?.rangeCount ? selection.getRangeAt(0) : null
  if (!range || !editor.contains(range.startContainer) || !editor.contains(range.endContainer)) {
    const end = serializeEditor(editor).length
    return { start: end, end }
  }
  return {
    start: offsetIn(editor, range.startContainer, range.startOffset),
    end: offsetIn(editor, range.endContainer, range.endOffset),
  }
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
  return previous ? chipToken(previous) !== null : false
}

/**
 * 把视口坐标解析成草稿偏移(拖拽落点用)。
 *
 * 用 `caretRangeFromPoint`(Chromium/Safari)或 `caretPositionFromPoint`
 * (Firefox)拿到"该像素落在哪个文本位置",再换算成草稿偏移。落点不在编辑器
 * 内时返回 null,调用方据此决定是插入到末尾还是忽略这次拖拽 —— 这一点必须
 * 由调用方显式决定,不能在这里默默回退(否则"拖到输入框外面"会被当成
 * "拖到了末尾")。
 */
export function caretOffsetAtPoint(editor: HTMLElement, x: number, y: number): number | null {
  if (typeof document === 'undefined') return null
  let container: Node | null = null
  let offset = 0
  const anyDocument = document as Document & {
    caretRangeFromPoint?: (x: number, y: number) => Range | null
    caretPositionFromPoint?: (x: number, y: number) => { offsetNode: Node; offset: number } | null
  }
  const range = anyDocument.caretRangeFromPoint?.(x, y)
  if (range) {
    container = range.startContainer
    offset = range.startOffset
  } else {
    const position = anyDocument.caretPositionFromPoint?.(x, y)
    if (position) {
      container = position.offsetNode
      offset = position.offset
    }
  }
  if (!container || !editor.contains(container)) return null
  if (container === editor) {
    let total = 0
    const nodes = Array.from(editor.childNodes)
    for (let index = 0; index < Math.min(offset, nodes.length); index++) {
      total += draftLength(nodes[index]!)
    }
    return total
  }
  const containingChip = Array.from(editor.childNodes).find((node) => chipToken(node) && (node === container || node.contains(container)))
  if (containingChip) return offsetIn(editor, container, offset)
  // 文本节点内的落点:累加它之前所有兄弟节点的草稿长度。
  let total = 0
  const walk = (root: Node): boolean => {
    for (const child of Array.from(root.childNodes)) {
      if (child === container) {
        total += child.nodeType === Node.TEXT_NODE ? offset : 0
        return true
      }
      if (child.contains(container)) {
        if (walk(child)) return true
        return false
      }
      total += draftLength(child)
    }
    return false
  }
  return walk(editor) ? total : null
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

export function createFileChip(candidate: MentionCandidate, preserveQuote = false): HTMLElement | null {
  const token = formatFileMention(candidate, preserveQuote)
  if (!token) return null
  const chip = document.createElement('span')
  chip.className = 'file-chip'
  chip.setAttribute('contenteditable', 'false')
  chip.dataset.fileMention = token
  chip.dataset.kind = candidate.kind
  chip.title = candidate.path
  const icon = document.createElement('span')
  icon.className = 'file-chip-icon'
  icon.innerHTML = candidate.kind === 'directory'
    ? '<svg viewBox="0 0 16 16" width="12" height="12" fill="none" aria-hidden="true"><path d="M2 4h5l1.5 2H14v7H2V4Z" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/></svg>'
    : '<svg viewBox="0 0 16 16" width="12" height="12" fill="none" aria-hidden="true"><path d="M4 2h5l3 3v9H4V2Z M9 2v4h3" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round"/></svg>'
  const label = document.createElement('span')
  label.className = 'file-chip-label'
  label.textContent = candidate.path.replace(/[\\/]+$/u, '').split(/[\\/]/u).pop() || candidate.path
  chip.append(icon, label)
  return chip
}

/** 文件标签与其空白在同一次原生编辑操作中插入,保留撤销事务。 */
export function insertFileChip(
  editor: HTMLElement,
  candidate: MentionCandidate,
  start: number,
  end: number,
  preserveQuote = false,
): boolean {
  const chip = createFileChip(candidate, preserveQuote)
  if (!chip) return false
  const text = serializeEditor(editor)
  const from = Math.max(0, Math.min(start, end, text.length))
  const to = Math.max(from, Math.min(Math.max(start, end), text.length))
  const leading = from > 0 && !/\s/u.test(text[from - 1]!) ? ' ' : ''
  const trailing = to === text.length || !/\s/u.test(text[to]!) ? ' ' : ''
  editor.focus()
  selectRange(editor, from, to)
  if (!document.execCommand('insertHTML', false, `${leading}${chip.outerHTML}${trailing}`)) return false
  setCaretOffset(editor, from + leading.length + chip.dataset.fileMention!.length + trailing.length)
  return true
}

/** 已完成的原子标签不能成为新的多关键词查询起点。 */
export function activeFileTokenIn(editor: HTMLElement, text: string, caret: number): ActiveAtToken | undefined {
  let boundary = 0
  let offset = 0
  for (const node of Array.from(editor.childNodes)) {
    offset += draftLength(node)
    if (chipToken(node) && offset <= caret) boundary = offset
    else if (chipToken(node) && offset - draftLength(node) < caret) return undefined
  }
  return activeAtToken(text.slice(boundary), caret - boundary)
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
  const pushPlainText = (value: string) => {
    const parts = value.split('\n')
    parts.forEach((part, index) => {
      if (index > 0) fragment.appendChild(document.createElement('br'))
      if (part) fragment.appendChild(document.createTextNode(part))
    })
  }
  const pushText = (value: string) => {
    for (const segment of scanSlashTokens(value, [...kinds.keys()])) {
      if (segment.name) fragment.appendChild(createSlashChip(segment.name, kinds.get(segment.name) ?? 'skill'))
      else pushPlainText(segment.text)
    }
  }
  const references = /(^|\s)@(?:"([^"\r\n]+)"|([^\s"]+))(?=\s|$)/gu
  let end = 0
  for (const match of text.matchAll(references)) {
    const start = match.index! + match[1]!.length
    const path = match[2] ?? match[3]!
    const chip = createFileChip({ path, kind: path.endsWith('/') ? 'directory' : 'file' }, match[2] !== undefined)
    if (!chip) continue
    pushText(text.slice(end, start))
    fragment.appendChild(chip)
    end = match.index! + match[0].length
  }
  pushText(text.slice(end))
  editor.replaceChildren(fragment)
}
