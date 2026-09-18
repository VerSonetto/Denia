/**
 * 显示级修复模型写歪的 ATX 标题。
 *
 * CommonMark 要求 `#` 序列之后是空白或行尾,模型却常把标题写成紧贴正文的
 * 形式:漏掉分隔空格(`##验证结果`),或者标题与紧随的表格、正文黏在同一行
 * (`##验证结果| 检查项 | 结果 |`、`##最大的三处收益**1.…**`)。这些行因此
 * 整段降级成普通段落——井号与表格分隔行原样露在正文里,黏连处还会连带把
 * 表格一起吞掉。历史会话里这类写法有数百处,是模型侧的笔误,不是渲染器的
 * 语法问题。
 *
 * 修复只作用于渲染:会话日志是 append-only 的模型输出记录,不能因为显示
 * 需要就去改写它(事件源纪律)。这里是渲染层对模型输出的宽容解析,与
 * `cjkFriendlyStrong` 属于同一层级的适配。
 *
 * 判定按优先级:
 *
 * 1. 行含 `|` 且下一行是表格分隔行 → 在第一个 `|` 处断行,恢复被吞的表格;
 * 2. 行内还有 `##` 及以上的标题标记 → 在它前面断行(两个标题黏在一起);
 * 3. 行内有加粗或单个行内代码标记、且它前面不是空白或左括号 → 在标记处断行;
 * 4. 长且带句末标点(`。！？；`)→ 是正文黏连,去掉标题标记;
 * 5. 其余 → 补一个空格,按标题渲染。
 *
 * 规则 3 看的是标记**前面**那个字符:`##改了什么**每次…**，实现上…` 里
 * "了"直接顶着 `**`,是标题接正文,该断;而 `##交付物(都在 \`D:\zcode-mod\`)`
 * 与 `###3.2 \`# Harness\`（identity段…）` 里标记前是空格或左括号,说明这段
 * 行内代码本来就属于标题,不该断。规则 3 还要求标记前的部分本身不像正文
 * (没有句末标点),否则 `##一点要留意…退出，它**可能**会…` 会被从句子中间
 * 劈开。
 *
 * 规则 4 的句末标点要看长度:`##为什么？` 是标题,而 `##根因日志里模型输出
 * 的是 …——这不是模型不会用工具。` 是正文——两者都带标点,差别在长短。
 *
 * 不动的情况:围栏代码块内;shebang(`#!`)与 C 预处理器指令;行内含
 * ```/~~~ 的行——那里的围栏标记是被吃掉换行后黏上来的,把它挪到行首会凭空
 * 造出一个代码块、把后面的正文整段吞掉,而它的内容(原本是代码)已经无法
 * 还原,所以整行原样保留。
 */

/** 疑似畸形标题:至多三个前导空格后,`#` 序列紧跟非空白、非 `#` 字符。 */
const MALFORMED = /^ {0,3}#{1,6}[^\s#]/m

/** 围栏起始:至多三个前导空格后跟三个以上反引号或波浪号。 */
const FENCE = /^ {0,3}(`{3,}|~{3,})/

/** 行内的围栏标记:见到它就不再处理该行(见文件头说明)。 */
const FENCE_RUN = /`{3,}|~{3,}/

/** 行首 ATX 标题:前导缩进、标记、以及紧贴其后的正文。 */
const HEADING = /^( {0,3})(#{1,6})([^\s#].*)$/

/** 行内 `##` 及以上的标题标记。 */
const NESTED_HEADING = /#{2,6}(?=[^\s#])/

/** 标记前若是这些字符,说明标记属于标题本身(行内代码嵌在标题里)。 */
const TITLE_CONTEXT = /[\s([（【「『"'`]/

/** 句末标点:出现它基本可以断定这一行是正文而不是标题。 */
const SENTENCE = /[。！？；]/

/** 表格分隔行(`|---|`、`| --- | --- |`);调用方另行要求该行含 `|`。 */
const DELIMITER_ROW = /^[ \t]*\|?[ \t]*:?-+:?[ \t]*(?:\|[ \t]*:?-+:?[ \t]*)*\|?[ \t]*$/

/** 行首 `#` 属于语法的 C 预处理器指令。 */
const PREPROCESSOR = /^(?:include|define|undef|ifdef|ifndef|elif|endif|pragma|error|warning)\b/

/**
 * 标题与正文的黏连点必须落在这个长度内。实测规范标题的 90 分位是 40 字符,
 * 更长的前缀基本是正文,不该被当作标题。
 */
const MAX_TITLE_LENGTH = 40

/**
 * 兜底长度:没有句末标点、也没有拆点,但长到这个地步的只能按正文渲染,
 * 免得凭空造出一个巨行标题。
 */
const MAX_TITLE_FALLBACK = 120

/** 一行的修复判定。 */
type Repair =
  /** 在 `at` 处断行:前半是标题,后半留给下一轮继续判定。 */
  | { kind: 'split'; at: number }
  /** 整行就是一个标题,补空格即可。 */
  | { kind: 'title' }
  /** 是正文黏连,去掉标题标记。 */
  | { kind: 'prose' }
  /** 不动这一行。 */
  | { kind: 'keep' }

/**
 * 正文里第一个可作为黏连点的标记:`**`,或**单个**反引号。成对与成串的
 * 反引号是行内代码/围栏的分隔符,不是黏连点。
 * @param body - 标题标记之后紧贴的正文。
 * @returns 标记偏移;没有则返回 -1。
 */
function firstTitleMark(body: string): number {
  let at = body.indexOf('**')
  for (let index = 0; index < body.length; index++) {
    if (body[index] !== '`') continue
    let end = index
    while (end < body.length && body[end] === '`') end++
    if (end - index === 1) {
      if (at < 0 || index < at) at = index
      break
    }
    index = end - 1
  }
  return at
}

/**
 * 判定一行畸形标题该怎么修。
 * @param body - 标题标记之后紧贴的正文。
 * @param next - 源文本里该行的下一行,用于确认表格分隔行。
 * @returns 该行的修复方式。
 */
function classify(body: string, next: string | undefined): Repair {
  if (body.startsWith('!') || PREPROCESSOR.test(body) || FENCE_RUN.test(body)) {
    return { kind: 'keep' }
  }

  const pipe = body.indexOf('|')
  if (pipe > 0 && next !== undefined && next.includes('|') && DELIMITER_ROW.test(next)) {
    return { kind: 'split', at: pipe }
  }

  const nested = NESTED_HEADING.exec(body)
  if (nested !== null && nested.index > 0) return { kind: 'split', at: nested.index }

  const mark = firstTitleMark(body)
  if (
    mark > 0 && mark <= MAX_TITLE_LENGTH
    && !TITLE_CONTEXT.test(body[mark - 1])
    && !SENTENCE.test(body.slice(0, mark))
  ) {
    return { kind: 'split', at: mark }
  }

  if (body.length <= MAX_TITLE_LENGTH) return { kind: 'title' }
  // 长且带句末标点的只能是正文黏连;短标题以问号结尾(如 `##为什么？`)仍是
  // 标题,所以句末标点这条要看长度。
  if (SENTENCE.test(body) || body.length > MAX_TITLE_FALLBACK) return { kind: 'prose' }
  return { kind: 'title' }
}

/** 一个行区间修完后的结果。 */
interface RepairedRange {
  /** 修复后的行。 */
  lines: string[]
  /** 处理完这些行之后的围栏状态。 */
  fence: string | null
}

/**
 * 修复一个行区间里的畸形标题。行与行之间只通过表格判定(要看下一行)与围栏
 * 状态关联,所以区间可以接着上一次的围栏状态继续处理。
 * @param lines - 待修复的行,不含行尾换行。
 * @param fence - 进入本区间时的围栏状态。
 * @param lookahead - 区间之后紧跟的那一行,只用于最后一行的表格判定。
 * @returns 修复后的行与出口围栏状态。
 */
function repairLines(
  lines: readonly string[],
  fence: string | null,
  lookahead?: string,
): RepairedRange {
  const out: string[] = []
  let state = fence
  for (const [index, source] of lines.entries()) {
    let line = source
    const next = index + 1 < lines.length ? lines[index + 1] : lookahead
    // 拆出的续行还要再判一次(`##标题###小节正文` 会连着拆两次),所以内层
    // 循环而不是单遍判定。
    for (;;) {
      const marker = FENCE.exec(line)
      if (marker !== null) {
        const character = marker[1][0]
        if (state === null) state = character
        else if (state === character) state = null
        break
      }
      if (state !== null) break
      const heading = HEADING.exec(line)
      if (heading === null) break
      const [, indent, hashes, body] = heading
      const repair = classify(body, next)
      if (repair.kind === 'keep') break
      if (repair.kind === 'title') {
        line = `${indent}${hashes} ${body}`
        break
      }
      if (repair.kind === 'prose') {
        line = body
        break
      }
      out.push(`${indent}${hashes} ${body.slice(0, repair.at).trimEnd()}`)
      line = body.slice(repair.at)
    }
    out.push(line)
  }
  return { lines: out, fence: state }
}

/**
 * 取某偏移处那一行的内容(不含换行)。
 * @param text - 全文。
 * @param start - 行起始偏移。
 * @returns 该行内容。
 */
function lineAt(text: string, start: number): string {
  const end = text.indexOf('\n', start)
  return end < 0 ? text.slice(start) : text.slice(start, end)
}

/**
 * 最后两行的起始偏移:倒数第二行的判定要看最后一行,而最后一行还没写完,
 * 所以两者都不算定型。这个偏移就是"已定型"与"还会变"的分界。
 * @param text - 当前累计的原文。
 * @returns 分界偏移。
 */
function unstableTailStart(text: string): number {
  const lastBreak = text.lastIndexOf('\n')
  if (lastBreak <= 0) return 0
  return text.lastIndexOf('\n', lastBreak - 1) + 1
}

/**
 * 一次性的全量修复(定稿渲染用)。
 * @param text - 模型输出的 Markdown 原文。
 * @returns 渲染用文本;无可修之处时原样返回。
 */
export function repairMalformedHeadings(text: string): string {
  if (!MALFORMED.test(text)) return text
  return repairLines(text.split('\n'), null).lines.join('\n')
}

/**
 * 流式渲染用的增量修复:定型的行只修一次就缓存住,每帧只重算末尾两行,所以
 * 每帧开销与新增文本同阶,而不是整篇重扫(与 `IncrementalMarkdownParser`
 * 同一个理由)。
 *
 * 一行定型之后不会被改写:它的判定只依赖自己与下一行,而这两行到那时都已
 * 写完,所以增量结果与 {@link repairMalformedHeadings} 对同一段文本给出的
 * 结果一致。
 *
 * 拆分会让输出比输入多出换行,所以修复后的文本不是输入的逐字前缀——增量
 * 解析器遇到这种输入会重置冻结前缀,这是刻意的代价(拆分本身依赖后到的
 * 信息,任何"只追加"的方案都做不到)。
 */
export class HeadingRepair {
  private input = ''
  private output = ''
  private committedText = ''
  private committedInput = 0
  private committedFence: string | null = null

  /**
   * 修复当前累计的原文。
   * @param text - 全文(追加式增长;非追加输入会丢弃缓存重来)。
   * @returns 渲染用文本。
   */
  repair(text: string): string {
    if (text === this.input) return this.output
    if (!text.startsWith(this.input)) {
      this.committedText = ''
      this.committedInput = 0
      this.committedFence = null
    }
    this.input = text
    if (!MALFORMED.test(text)) {
      this.output = text
      return text
    }
    const boundary = unstableTailStart(text)
    if (boundary > this.committedInput) {
      const chunk = text.slice(this.committedInput, boundary).split('\n')
      // 切片以换行结尾,split 会多出一个空元素。
      chunk.pop()
      const repaired = repairLines(chunk, this.committedFence, lineAt(text, boundary))
      this.committedText += `${repaired.lines.join('\n')}\n`
      this.committedFence = repaired.fence
      this.committedInput = boundary
    }
    const tail = repairLines(text.slice(boundary).split('\n'), this.committedFence)
    this.output = this.committedText + tail.lines.join('\n')
    return this.output
  }
}
