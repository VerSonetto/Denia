/** Human-readable tool call labels; short paths instead of full absolute paths. */

function normalizeSlashes(path: string): string {
  return path.replace(/\\/g, '/').replace(/\/+/g, '/')
}

/** Collapse long absolute paths to a short, relative-looking tail. */
export function shortPath(raw: string): string {
  const trimmed = raw.trim()
  if (!trimmed) return trimmed
  let path = normalizeSlashes(trimmed)
  path = path.replace(/^[A-Za-z]:\//, '')
  if (path.startsWith('/')) path = path.slice(1)
  const parts = path.split('/').filter(Boolean)
  if (parts.length <= 2) return parts.join('/')
  return `…/${parts.slice(-2).join('/')}`
}

/**
 * 从工具参数里取出第一个 JSON 对象。
 *
 * 朴素的花括号配平会被内容里的 `}` 提前闭合(edit 的 old_string 常是代码
 * 片段),于是解析失败、回退到"整段原文首行"——这正是 edit 行过去显示匹配
 * 内容而非文件名的根因。这里做字符串感知扫描(转义与引号状态),把所有
 * "深度回 0"的候选闭括号记下来,从后往前逐个试 parse,取第一个成功的。
 */
function parseArgsObject(args: string): Record<string, unknown> | null {
  const start = args.indexOf('{')
  if (start < 0) return null
  const candidates: number[] = []
  let depth = 0
  let inString = false
  let escaped = false
  for (let i = start; i < args.length; i++) {
    const ch = args[i]
    if (escaped) {
      escaped = false
      continue
    }
    if (ch === '\\') {
      if (inString) escaped = true
      continue
    }
    if (ch === '"') {
      inString = !inString
      continue
    }
    if (inString) continue
    if (ch === '{') depth++
    else if (ch === '}') {
      depth--
      // 深度归零 = 一个候选结尾;深度变负 = 字符串已经不配平,停。
      if (depth === 0) candidates.push(i)
      else if (depth < 0) break
    }
  }
  for (let i = candidates.length - 1; i >= 0; i--) {
    try {
      const value: unknown = JSON.parse(args.slice(start, candidates[i] + 1))
      if (value !== null && typeof value === 'object' && !Array.isArray(value)) {
        return value as Record<string, unknown>
      }
    } catch {
      // 候选不对(被内容里的 `}` 截短),继续往前试。
    }
  }
  return null
}

function readString(obj: Record<string, unknown>, key: string): string | undefined {
  const value = obj[key]
  return typeof value === 'string' ? value : undefined
}

/** One-line summary for a tool row header. */
export function toolCallSummary(name: string, args: string): string {
  const parsed = parseArgsObject(args)
  if (parsed) {
    switch (name) {
      case 'spawn_agent':
      case 'fork_agent':
        return readString(parsed, 'description') ?? (readString(parsed, 'prompt') ?? '').slice(0, 90)
      case 'skill':
        return readString(parsed, 'name') ?? ''
      case 'send_message':
      case 'interrupt_agent':
      case 'wait_agent':
        return readString(parsed, 'target') ?? ''
      case 'job_output':
      case 'job_kill':
        return readString(parsed, 'id') ?? ''
      case 'job_start':
        return (readString(parsed, 'label') ?? readString(parsed, 'command') ?? '').slice(0, 90)
      case 'read_file':
      case 'write_file': {
        const path = readString(parsed, 'path')
        if (path) return shortPath(path)
        break
      }
      case 'ls': {
        const path = readString(parsed, 'path')
        const depth = parsed['depth']
        const label = path ? shortPath(path) : '.'
        return typeof depth === 'number' && depth > 1 ? `${label}  depth ${depth}` : label
      }
      case 'bash': {
        const command = readString(parsed, 'command')
        if (command) return command.length > 90 ? `${command.slice(0, 90)}…` : command
        break
      }
      case 'glob':
      case 'grep': {
        const pattern = readString(parsed, 'pattern')
        if (pattern) return pattern.length > 90 ? `${pattern.slice(0, 90)}…` : pattern
        break
      }
      case 'edit': {
        const path = readString(parsed, 'path')
        if (path) return shortPath(path)
        break
      }
    }
  }
  const line = args.trim().split('\n')[0]?.trim() ?? ''
  // 参数解析失败(多为流式未收完的半截 JSON)时,"首行"往往是
  // `{"path": "...",` 这类噪音;edit 至少还能认出 path 就先认 path。
  if (name === 'edit') {
    const path = extractStringField(args, 'path')
    if (path) return shortPath(path)
  }
  return line.length > 90 ? `${line.slice(0, 90)}…` : line
}

/** 半截 JSON 里的字段抢救:`{"path": "a.ts",` → `a.ts`。仅用于展示摘要。 */
function extractStringField(args: string, key: string): string | undefined {
  const match = args.match(new RegExp(`"${key}"\\s*:\\s*"((?:[^"\\\\]|\\\\.)*)"`))
  if (!match) return undefined
  try {
    return JSON.parse(`"${match[1]}"`) as string
  } catch {
    return match[1]
  }
}

/** Compact input body when a tool row is expanded. */
export function toolCallInput(name: string, args: string): string | null {
  const parsed = parseArgsObject(args)
  if (!parsed) return args.trim() || null
  switch (name) {
    case 'read_file': {
      const path = readString(parsed, 'path')
      return path ? shortPath(path) : null
    }
    case 'ls': {
      const path = readString(parsed, 'path')
      return path ? shortPath(path) : null
    }
    case 'write_file': {
      const path = readString(parsed, 'path')
      const content = readString(parsed, 'content')
      if (!path) return null
      const head = shortPath(path)
      if (!content) return head
      const preview =
        content.length > 240 ? `${content.slice(0, 240).trimEnd()}…` : content
      return `${head}\n\n${preview}`
    }
    case 'bash': {
      const command = readString(parsed, 'command')
      return command ?? null
    }
    default:
      return null
  }
}

/* ---- edit 的 diff 视图 ---- */

/** diff 的一行;`kind` 决定配色与前缀符号。 */
export interface DiffLine {
  kind: 'context' | 'del' | 'add'
  text: string
  /** 旧文本中的行号(仅 context/del 有);1-based。 */
  oldNo?: number
  /** 新文本中的行号(仅 context/add 有);1-based。 */
  newNo?: number
  /** 折叠占位行:表示此处省略了 skipped 行。 */
  fold?: boolean
}

export interface EditDiff {
  path: string
  /** 旧文本首行在文件中的行号(1-based,未知为 1)。 */
  startLine: number
  lines: DiffLine[]
  removed: number
  added: number
  /** diff 过长被折叠:中间省略的行数。 */
  skipped: number
}

/** 头部与折叠区各保留的上下文行数。 */
const CONTEXT = 3
/** 超过这个行数就折叠中间(工具行里不该挂一整屏 diff)。 */
const MAX_LINES = 40

/**
 * edit 工具的行级 diff:从参数里取出 path/old_string/new_string,交给
 * [`diffLines`]。参数解析失败(流式半截 JSON)时退化为字段抢救;
 * 取不到 old/new 返回 null,调用方走原来的"文件名 + 结果"展示。
 */
/**
 * write_file 的行级 diff:整文件覆盖写成的一次性"全量 diff"。
 *
 * 语义与 edit 不同:write 没有"改了哪一段"的信息,只有"最终内容"。
 * 这里按行号把新内容对齐到原文件的对应行上算增删:
 * - 新行 i 与旧行 i 相同 → context;不同 → 该行 add + del 成对出现
 *   (展示成"一行删掉、一行加上",与 edit 的观感一致);
 * - 新内容比原文件短 → 多出来的旧行全 del;比原文件长 → 多出来的新行全 add。
 * `previous` 是写入前的文件内容,缺省时退化为"纯新增"(新建文件的形态)。
 */
export function writeDiff(
  name: string,
  args: string,
  previous?: string,
): EditDiff | null {
  if (name !== 'write_file') return null
  const parsed = parseArgsObject(args)
  const path = (parsed && readString(parsed, 'path')) ?? extractStringField(args, 'path') ?? ''
  const content = (parsed && readString(parsed, 'content')) ?? extractStringField(args, 'content')
  if (!path || content === undefined) return null
  // 没有原内容(新建/未取到旧文)= 纯新增:old 为空串,diff 天然全是 +。
  return diffLines(path, previous ?? '', content, 1)
}

/**
 * edit 工具的行级 diff:从参数里取出 path/old_string/new_string,交给
 * [`diffLines`]。参数解析失败(流式半截 JSON)时退化为字段抢救;
 * 取不到 old/new 返回 null,调用方走原来的"文件名 + 结果"展示。
 */
export function editDiff(name: string, args: string, startLine?: number): EditDiff | null {
  if (name !== 'edit') return null
  const parsed = parseArgsObject(args)
  const path = (parsed && readString(parsed, 'path')) ?? extractStringField(args, 'path') ?? ''
  const oldString =
    (parsed && readString(parsed, 'old_string')) ?? extractStringField(args, 'old_string')
  const newString =
    (parsed && readString(parsed, 'new_string')) ?? extractStringField(args, 'new_string')
  if (!path || oldString === undefined || newString === undefined) return null
  return diffLines(path, oldString, newString, startLine ?? 1)
}

/**
 * 两段文本的行级 diff(纯函数,导出以便单测)。
 *
 * 算法:先剥掉公共前缀与公共后缀,剩下的"中间段"用 LCS 找回未变的行
 * —— 让"改几行"的编辑只标出真正变了的那些行,而不是整段推倒重来。
 * LCS 的 DP 单元格数超过 [`LCS_CELL_BUDGET`] 就放弃对齐,中段整段替换。
 */

/** 文本 → 行数组:统一换行、丢掉末尾换行产生的空尾元素;空串算 0 行,
 *  否则"写入空文件"会被算成"删了一行空行"。 */
function splitLines(text: string): string[] {
  return text === '' ? [] : text.replace(/\r\n/g, '\n').replace(/\n$/, '').split('\n')
}

export function diffLines(
  path: string,
  oldText: string,
  newText: string,
  startLine = 1,
): EditDiff {
  const oldLines = splitLines(oldText)
  const newLines = splitLines(newText)
  // 公共前缀/后缀:它们的交集之外就是唯一的替换块。
  let head = 0
  while (head < oldLines.length && head < newLines.length && oldLines[head] === newLines[head]) {
    head++
  }
  let tail = 0
  while (
    tail < oldLines.length - head &&
    tail < newLines.length - head &&
    oldLines[oldLines.length - 1 - tail] === newLines[newLines.length - 1 - tail]
  ) {
    tail++
  }
  const midOld = oldLines.slice(head, oldLines.length - tail)
  const midNew = newLines.slice(head, newLines.length - tail)
  // 中段内部用 LCS 找回未变的行(超过预算就放弃,直接整段替换)。
  const mid: MidLine[] =
    midOld.length * midNew.length <= LCS_CELL_BUDGET
      ? lcsSplit(midOld, midNew)
      : [
          ...midOld.map((text) => ({ kind: 'del' as const, text })),
          ...midNew.map((text) => ({ kind: 'add' as const, text })),
        ]

  const lines: DiffLine[] = []
  for (let i = 0; i < head; i++) {
    lines.push({ kind: 'context', text: oldLines[i], oldNo: startLine + i, newNo: startLine + i })
  }
  // 中段:del 与 add 各自按出现顺序交织输出(先删后加,与 git diff 一致)。
  let oldNo = startLine + head
  let newNo = startLine + head
  for (const item of mid) {
    if (item.kind === 'same') {
      lines.push({ kind: 'context', text: item.text, oldNo: oldNo++, newNo: newNo++ })
    } else if (item.kind === 'del') {
      lines.push({ kind: 'del', text: item.text, oldNo: oldNo++ })
    } else {
      lines.push({ kind: 'add', text: item.text, newNo: newNo++ })
    }
  }
  const tailStart = startLine + head + midOld.length
  for (let i = 0; i < tail; i++) {
    lines.push({
      kind: 'context',
      text: oldLines[oldLines.length - tail + i],
      oldNo: tailStart + i,
      newNo: tailStart + i,
    })
  }
  const removed = mid.filter((item) => item.kind === 'del').length
  const added = mid.filter((item) => item.kind === 'add').length
  return { path: shortPath(path), startLine, ...foldMiddle(lines), removed, added }
}

/** 折叠过长的 diff:只保留头尾各 CONTEXT 行,中间压成一行占位。 */
function foldMiddle(lines: DiffLine[]): { lines: DiffLine[]; skipped: number } {
  if (lines.length <= MAX_LINES) return { lines, skipped: 0 }
  const headSlice = lines.slice(0, CONTEXT)
  const tailSlice = lines.slice(lines.length - CONTEXT)
  const skipped = lines.length - headSlice.length - tailSlice.length
  if (skipped <= CONTEXT) return { lines, skipped: 0 }
  return {
    lines: [...headSlice, { kind: 'context', text: '', fold: true }, ...tailSlice],
    skipped,
  }
}

/** LCS 的 DP 单元格预算:超出即放弃逐行对齐(保护超大替换不卡 UI)。 */
const LCS_CELL_BUDGET = 250_000

type MidLine = { kind: 'same' | 'del' | 'add'; text: string }

/**
 * 两段中段的逐行对齐:标准 LCS DP(滚动数组)+ 回溯,输出 same/del/add 序列。
 * 相等行优先判 same,于是"只改一行"的编辑只产生一删一加。
 */
function lcsSplit(oldLines: string[], newLines: string[]): MidLine[] {
  const n = oldLines.length
  const m = newLines.length
  if (n === 0) return newLines.map((text) => ({ kind: 'add' as const, text }))
  if (m === 0) return oldLines.map((text) => ({ kind: 'del' as const, text }))
  // table[j] = 以 old[i] / new[j] 结尾的 LCS 长度;滚动覆盖上一行。
  let prev = new Uint32Array(m + 1)
  let curr = new Uint32Array(m + 1)
  // 回溯需要完整表;n*m 已在预算内,存下每行快照即可。
  const table: Uint32Array[] = []
  for (let i = 1; i <= n; i++) {
    for (let j = 1; j <= m; j++) {
      curr[j] =
        oldLines[i - 1] === newLines[j - 1]
          ? prev[j - 1] + 1
          : Math.max(prev[j], curr[j - 1])
    }
    table.push(curr.slice())
    const swap = prev
    prev = curr
    curr = swap
    curr.fill(0)
  }
  const out: MidLine[] = []
  let i = n
  let j = m
  // table[i-1][j] 即 dp[i][j](dp 行 i 以 oldLines[i-1] 结尾)。
  while (i > 0 || j > 0) {
    if (i > 0 && j > 0 && oldLines[i - 1] === newLines[j - 1]) {
      out.push({ kind: 'same', text: oldLines[i - 1] })
      i--
      j--
      continue
    }
    if (i === 0) {
      out.push({ kind: 'add', text: newLines[j - 1] })
      j--
      continue
    }
    if (j === 0) {
      out.push({ kind: 'del', text: oldLines[i - 1] })
      i--
      continue
    }
    const up = i >= 2 ? table[i - 2][j] : 0 // dp[i-1][j]
    const left = table[i - 1][j - 1] // dp[i][j-1]
    // 平局走 add:回溯是逆序的,先压 add 会让它在正序里排在 del 之后
    // (git diff 的"先删后加")。
    if (left >= up) {
      out.push({ kind: 'add', text: newLines[j - 1] })
      j--
    } else {
      out.push({ kind: 'del', text: oldLines[i - 1] })
      i--
    }
  }
  out.reverse()
  return out
}
