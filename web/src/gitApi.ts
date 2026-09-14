/**
 * 审查面板的 API 客户端:Git 状态与单文件 Diff。
 *
 * 后端直接调 `git` 可执行文件(理由见 `crates/server/src/api/git.rs`),
 * 所以这里的形状就是 git 输出的结构化版本。
 */

/** 一个改动文件。 */
export interface GitEntry {
  path: string
  /** 索引区状态字符(`M`/`A`/`D`/`R`/`?`…)。 */
  indexStatus: string
  /** 工作区状态字符。 */
  worktreeStatus: string
  staged: boolean
  unstaged: boolean
  untracked: boolean
  conflicted: boolean
  renamedFrom: string | null
}

export interface GitStatus {
  isRepository: boolean
  repoRoot?: string
  branch?: string | null
  head?: string | null
  detached?: boolean
  ahead?: number | null
  behind?: number | null
  entries: GitEntry[]
}

/** Diff 可用性:决定前端走哪条渲染路径(三级降级)。 */
export type DiffAvailability = 'patch' | 'binary' | 'truncated' | 'unavailable'

export interface GitDiff {
  path: string
  availability: DiffAvailability
  patch: string | null
  beforeContent: string | null
  afterContent: string | null
  summary: string | null
}

/** 审查面板的「来源」:与 ZCode 的四个 source 对齐(去掉 last-turn)。 */
export type GitSource = 'unstaged' | 'staged' | 'branch'

export const GIT_SOURCES: GitSource[] = ['unstaged', 'staged', 'branch']

async function http<T>(path: string, timeoutMs = 30_000): Promise<T> {
  const response = await fetch(path, { signal: AbortSignal.timeout(timeoutMs) })
  const text = await response.text()
  const value = text ? (JSON.parse(text) as unknown) : {}
  if (!response.ok) {
    const message =
      (value as { error?: { message?: string } }).error?.message ?? response.statusText
    throw new Error(message)
  }
  return value as T
}

export function fetchGitStatus(workspacePath: string): Promise<GitStatus> {
  const query = new URLSearchParams({ path: workspacePath })
  return http<GitStatus>(`/api/git/status?${query.toString()}`)
}

export function fetchGitDiff(
  workspacePath: string,
  file: string,
  source: GitSource,
): Promise<GitDiff> {
  const query = new URLSearchParams({ path: workspacePath, file, source })
  return http<GitDiff>(`/api/git/diff?${query.toString()}`)
}

/* ---- 派生:把 entries 按来源切成四组(与 git 的四个 section 对齐) ---- */

export interface GitSection {
  id: 'staged' | 'unstaged' | 'untracked' | 'conflicted'
  entries: GitEntry[]
}

/**
 * 按来源过滤 + 分组。
 *
 * 一个文件可能同时"已暂存 + 又改了"(索引区与工作区状态都非空),
 * 所以它会同时出现在 staged 与 unstaged 两个分组里 —— 这与
 * `git status` 的短格式语义一致,不是重复。
 */
export function sectionize(entries: GitEntry[], source: GitSource): GitSection[] {
  const pick = (predicate: (entry: GitEntry) => boolean) =>
    entries.filter(predicate).sort((a, b) => a.path.localeCompare(b.path))

  if (source === 'staged') {
    const staged: GitSection = { id: 'staged', entries: pick((entry) => entry.staged) }
    return staged.entries.length > 0 ? [staged] : []
  }
  if (source === 'branch') {
    // 分支比较用不上分段:把所有改动平铺。
    const all: GitSection = { id: 'unstaged', entries: pick(() => true) }
    return all.entries.length > 0 ? [all] : []
  }
  const sections: GitSection[] = [
    { id: 'conflicted', entries: pick((entry) => entry.conflicted) },
    { id: 'unstaged', entries: pick((entry) => entry.unstaged && !entry.conflicted) },
    { id: 'untracked', entries: pick((entry) => entry.untracked) },
  ]
  return sections.filter((section) => section.entries.length > 0)
}

/** 所有来源下的全部文件(搜索用)。 */
export function flattenSections(sections: GitSection[]): GitEntry[] {
  return sections.flatMap((section) => section.entries)
}
