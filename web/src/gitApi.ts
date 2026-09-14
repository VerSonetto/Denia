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

/**
 * 带错误码的 POST。
 *
 * 与上面的 `http` 分开是因为提交/推送的失败要**分类呈现**:后端给
 * `git/no-upstream`、`git/non-fast-forward`、`git/auth-failed` 这类错误码,
 * 前端据此决定是否显示"去建 upstream"之类的引导。只留 message 会把分类信息
 * 丢掉,而那正是用户下一步动作的依据。
 */
async function post<T>(path: string, body: unknown, timeoutMs = 120_000): Promise<T> {
  const response = await fetch(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
    signal: AbortSignal.timeout(timeoutMs),
  })
  const text = await response.text()
  const value = text ? (JSON.parse(text) as unknown) : {}
  if (!response.ok) {
    const failure = (value as { error?: { code?: string; message?: string } }).error
    throw new GitApiError(
      failure?.code ?? `http-${response.status}`,
      failure?.message ?? response.statusText,
    )
  }
  return value as T
}

/** 带错误码的 Git 操作失败(错误码决定前端给什么引导)。 */
export class GitApiError extends Error {
  constructor(
    public code: string,
    message: string,
  ) {
    super(message)
    this.name = 'GitApiError'
  }
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

/* ---- 提交 / 推送 / AI 生成提交信息 ---- */

export interface CommitResult {
  ok: boolean
  /** 提交后的短 SHA(成功反馈里显示)。 */
  head: string
  summary: string
}

/** 提交工作区全部改动(含未跟踪文件,与面板展示范围一致)。 */
export function commitChanges(workspacePath: string, message: string): Promise<CommitResult> {
  return post<CommitResult>('/api/git/commit', { path: workspacePath, message })
}

export interface PushResult {
  ok: boolean
  /** 实际推送到的上游(`origin/main`)。 */
  upstream: string
  summary: string
}

/** 推送到当前分支的 upstream。 */
export function pushChanges(workspacePath: string): Promise<PushResult> {
  return post<PushResult>('/api/git/push', { path: workspacePath })
}

export interface GenerateMessageOptions {
  path: string
  provider: string
  model: string
  /** 用户当前选的档位;后端会在此基础上改取最低延迟档。 */
  reasoningEffort?: string
}

/** 用模型生成一条提交信息(读当前改动 + 近 5 条提交作为风格参考)。 */
export function generateCommitMessage(
  options: GenerateMessageOptions,
  signal?: AbortSignal,
): Promise<{ message: string }> {
  return fetch('/api/git/commit-message', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({
      path: options.path,
      provider: options.provider,
      model: options.model,
      reasoning_effort: options.reasoningEffort,
    }),
    signal: signal ?? AbortSignal.timeout(120_000),
  }).then(async (response) => {
    const text = await response.text()
    const value = text ? (JSON.parse(text) as unknown) : {}
    if (!response.ok) {
      const failure = (value as { error?: { code?: string; message?: string } }).error
      throw new GitApiError(
        failure?.code ?? `http-${response.status}`,
        failure?.message ?? response.statusText,
      )
    }
    return value as { message: string }
  })
}

/**
 * 提交按钮的可用性。
 *
 * 三个条件缺一不可:有改动可提交、提交信息非空、当前没有在跑的操作。
 * 做成纯函数是为了能直接断言 —— 这类"按钮该不该亮"的判断散在 JSX 里
 * 最容易写漏(比如忘了判空信息,点了之后被后端 400 弹回来)。
 */
export function canCommit(options: {
  hasChanges: boolean
  message: string
  busy: boolean
}): boolean {
  return options.hasChanges && options.message.trim().length > 0 && !options.busy
}

/** 推送按钮的可用性:没有远程或游离 HEAD 时不给推(后端也会拒,但提前禁用更清楚)。 */
export function canPush(status: GitStatus | null, busy: boolean): boolean {
  if (busy || !status?.isRepository) return false
  if (status.detached) return false
  return true
}

/**
 * 失败提示的补充引导(按后端错误码)。
 *
 * 把"该做什么"和"错在哪"分开呈现:后端 message 里已经含了 git 原文,
 * 这里只补一句可照做的动作。返回空串表示不需要额外引导。
 */
export function failureHint(code: string): string {
  switch (code) {
    case 'git/no-upstream':
      return '在终端执行 git push -u origin <分支名> 建立上游后即可在此推送。'
    case 'git/no-remote':
      return '在终端执行 git remote add origin <仓库地址> 添加远程仓库。'
    case 'git/non-fast-forward':
      return '先拉取远程改动(git pull --rebase)再推送。'
    case 'git/auth-failed':
      return '检查 SSH key 或 personal access token 是否有效。'
    case 'git/identity-missing':
      return '这是首次提交前的必要配置,配置一次即可长期生效。'
    case 'git/nothing-to-commit':
      return '工作区干净时无需提交。'
    default:
      return ''
  }
}
