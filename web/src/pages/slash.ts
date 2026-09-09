/**
 * `/` 斜杠命令/技能的纯函数核心(对照 dsh input-trigger 的 detect + command grammar)。
 * 零 React/DOM 依赖,便于单测。触发、装饰、发送收集共用同一套 token 词法,
 * 保证「弹层里选的、输入框里亮的、发送时注入的」是同一个东西。
 */

/**
 * token 边界字符类:`/` 前允许行首、空白或 CJK 字符/标点(中文输入习惯
 * 不打空格);ASCII 字母数字紧邻(`/` 在 `https:`、`//`、`1/2` 里)不触发。
 */
const SLASH_BEFORE = `\\s\\u3000-\\u303f\\uff00-\\uffef\\u3400-\\u9fff`

/** 弹层触发:光标前的 `/name` token(name 可为空 = 刚敲下 `/`)。 */
const ACTIVE_SLASH_RE = new RegExp(`(?:^|[${SLASH_BEFORE}])(\\/([a-zA-Z0-9_-]*))$`, 'u')

/** 全文扫描:与触发同一词法(greedy 吃满 name,靠词典精确命中,无需后向边界)。 */
const SCAN_SLASH_RE = new RegExp(`(?<=^|[${SLASH_BEFORE}])\\/([a-zA-Z0-9_-]+)`, 'gu')

/** 与后端 `skill_gesture` 一致的 kebab-case 名词法(仅小写)。 */
const SKILL_NAME_RE = /^[a-z0-9]+(?:-[a-z0-9]+)*$/u

/** 光标处活跃的 `/` token。 */
export interface ActiveSlashToken {
  /** 从行首/空白/CJK 边界后到光标前的完整 token(含 `/`),选中候选时整段替换。 */
  prefix: string
  /** `/` 之后的名称查询串。 */
  query: string
}

/**
 * 提取光标处的 `/name` token:`/` 前必须是行首、空白或 CJK(URL 的 `//`、
 * `https:/` 与分数 `1/2` 因前一字符不在边界类里天然不触发)。draft 为
 * textarea 全文,正则里的 `\s` 天然覆盖换行。
 */
export function activeSlashToken(draft: string, caret: number): ActiveSlashToken | undefined {
  const beforeCursor = draft.slice(0, Math.max(0, Math.min(caret, draft.length)))
  const match = ACTIVE_SLASH_RE.exec(beforeCursor)
  if (match?.[1] === undefined || match[2] === undefined) return undefined
  return { prefix: match[1], query: match[2] }
}

/** 一段扫描结果:name 存在表示该段是命中词典的 slash token(渲染成卡片)。 */
export interface SlashSegment {
  text: string
  /** 段在全文中的起始偏移(分段连续覆盖全文,供卡片区间定位)。 */
  start: number
  name?: string
}

/**
 * 按词典把全文切成「普通文本 / 命中 token」分段,供编辑器重建卡片节点
 * (renderDraft)使用。词典未命中(未知 /word)保持普通文本;greedy 匹配
 * 保证 `/plans` 不会命中 `plan`。
 */
export function scanSlashTokens(text: string, names: readonly string[]): SlashSegment[] {
  if (text.length === 0) return []
  const known = new Set(names)
  const segments: SlashSegment[] = []
  let last = 0
  for (const match of text.matchAll(SCAN_SLASH_RE)) {
    const name = match[1]
    if (!known.has(name)) continue
    const start = match.index ?? 0
    if (start > last) segments.push({ text: text.slice(last, start), start: last })
    segments.push({ text: text.slice(start, start + match[0].length), start, name })
    last = start + match[0].length
  }
  if (last < text.length) segments.push({ text: text.slice(last), start: last })
  return segments
}

/**
 * 收集正文中可直调的技能名(去重、首见顺序),随 prompt body 的 `skills`
 * 数组发送。首行恰好是裸 `/name` 的场景由后端 `skill_gesture` 注入,这里
 * 剔除同名 token 避免双重注入;后端对 `skills` 数组按 user_invocable 校验,
 * 调用方必须只传 user-invocable 词典里的名字。
 */
export function collectSkillTokens(text: string, names: readonly string[]): string[] {
  const known = new Set(names)
  const found: string[] = []
  for (const match of text.matchAll(SCAN_SLASH_RE)) {
    const name = match[1]
    if (!known.has(name) || found.includes(name)) continue
    found.push(name)
  }
  const firstLine = text.split('\n', 1)[0]?.trim() ?? ''
  const bare = firstLine.startsWith('/') ? firstLine.slice(1) : undefined
  const bareName = bare !== undefined && SKILL_NAME_RE.test(bare) ? bare : undefined
  return found.filter((name) => name !== bareName)
}

/** 首行开头的内置命令判定结果。 */
export interface LeadingCommand {
  kind: 'plan' | 'compact' | 'goal'
  /** token 之后的正文;undefined = 裸命令(token 后无任何字符)。 */
  rest?: string
}

/**
 * 解析首行开头的内置命令(命令优先于技能,对齐 dsh matchEnter 的行认领):
 * `/plan`、`/compact`、`/goal` 位于文本最前,后随空白或结尾才算;`/plans`、
 * 正文中的 `/plan` 不算。仅前导空白被容忍。
 */
export function parseLeadingCommand(text: string): LeadingCommand | undefined {
  const match = /^\/(plan|compact|goal)(?:$|[\s]([\s\S]*)$)/u.exec(text.replace(/^\s+/u, ''))
  if (!match) return undefined
  return { kind: match[1] as LeadingCommand['kind'], rest: match[2] }
}
