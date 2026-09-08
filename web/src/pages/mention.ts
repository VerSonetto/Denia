/**
 * `@` 文件/文件夹提及的纯函数核心(对照 dsh file-reference grammar)。
 * 零 React/DOM 依赖,便于单测。
 */

/** 一条候选(结构上与 `api.MentionCandidate` 一致,此处不引 http 层)。 */
export interface MentionCandidate {
  path: string
  kind: 'file' | 'directory'
}

/** 光标处活跃的 `@` token(对照 dsh `ActiveAtToken`)。 */
export interface ActiveAtToken {
  /** 从行首/空白处到光标前的完整 token,选中候选时整段替换。 */
  prefix: string
  /** `@`(或 `@"`)之后的路径查询串。 */
  query: string
  /** 是否以 `@"` 打开(路径可含空格)。 */
  quoted: boolean
}

/**
 * 提取光标处的 `@path` / `@"path with spaces` token。`@` 前必须是行首或
 * 空白(user@host 这类嵌入 token 不是触发点)。draft 为 textarea 全文,
 * 正则里的 `\s` 天然覆盖换行,与逐行语义等价。
 */
export function activeAtToken(draft: string, caret: number): ActiveAtToken | undefined {
  const beforeCursor = draft.slice(0, Math.max(0, Math.min(caret, draft.length)))
  const quoted = /(?:^|\s)(@"([^"]*))$/u.exec(beforeCursor)
  if (quoted?.[1] !== undefined && quoted[2] !== undefined) {
    return { prefix: quoted[1], query: quoted[2], quoted: true }
  }
  const plain = /(?:^|\s)(@([^\s]*))$/u.exec(beforeCursor)
  if (plain?.[1] === undefined || plain[2] === undefined) return undefined
  return { prefix: plain[1], query: plain[2], quoted: false }
}

/**
 * 把选中的候选格式化为插入文本(对照 dsh `formatFileMention`):
 * 目录尾补 `/`;路径含空白或原本就在 `@"` 里时用引号形式——文件闭合引号、
 * 目录保持引号打开(可继续在引号内下钻);无法安全表示的路径返回 undefined。
 */
export function formatFileMention(
  candidate: MentionCandidate,
  preserveQuote: boolean,
): string | undefined {
  const path = candidate.kind === 'directory' ? `${candidate.path}/` : candidate.path
  if (/[\u0000-\u001f\u007f-\u009f"]/u.test(path)) return undefined
  const quoted = preserveQuote || /\s/u.test(path)
  if (!quoted) return `@${path}`
  if (candidate.kind === 'directory') return `@"${path}`
  return `@"${path}"`
}

/** 一次插入的结果:完整新文本 + 光标落点。 */
export interface MentionInsertion {
  text: string
  caret: number
}

/**
 * 用 mention 文本替换光标处的 token 并在末尾补一个空格(光标移到其后,
 * 后续输入不会和提及粘连);后面若已有空白就不再叠一个。
 */
export function applyMentionInsertion(
  draft: string,
  caret: number,
  token: ActiveAtToken,
  mention: string,
): MentionInsertion {
  const start = Math.max(0, caret - token.prefix.length)
  const rest = draft.slice(caret)
  const needsSpace = rest.length === 0 || !/^\s/u.test(rest)
  const inserted = needsSpace ? `${mention} ` : mention
  return { text: `${draft.slice(0, start)}${inserted}${rest}`, caret: start + inserted.length }
}
