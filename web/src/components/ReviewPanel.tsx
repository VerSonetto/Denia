/**
 * 审查面板:Git 来源切换 + 改动文件列表 + 展开看 Diff。
 *
 * # 与 ZCode 的对应
 *
 * ZCode 的审查面板结构(`MOt`):
 * ```
 * section
 * ├─ header: 来源下拉(Select) + 刷新按钮
 * └─ body:   虚拟列表(行高 32px,overscan 14) 或 空态
 *     └─ 行: 折叠头(文件路径 + ±行数 + chevron)+ 展开后 Diff
 * ```
 * 这里保持同样的信息层级,但做了两处**有意的简化**:
 *
 * 1. **不做虚拟列表**。ZCode 用 `@tanstack/react-virtual` 是因为它要扛
 *    超大改动集;denia 的定位是单机单仓库,一次几百个文件已是极端情况,
 *    而虚拟列表会带来"展开状态与虚拟窗口不同步"的一类 bug。先做朴素列表,
 *    真遇到性能问题再换 —— 数据结构已经按"按需懒加载 diff"设计好了。
 * 2. **Diff 用 `<pre>` + 行着色**,不引 diff 渲染库。patch 本身已经是
 *    带 `+`/`-`/` ` 前缀的文本,按前缀上色就够读;引库会带来主题适配、
 *    大文件卡顿、包体积三个负担。
 *
 * Diff 的三级降级(抄 ZCode 的 `SOt`)保持不变:
 *   patch(有 patch 文本) → 全文对比(有前后内容) → 提示(二进制/过大/不可用)
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { t } from '../i18n'
import {
  GIT_SOURCES,
  fetchGitDiff,
  fetchGitStatus,
  flattenSections,
  sectionize,
  type GitDiff,
  type GitEntry,
  type GitSource,
  type GitStatus,
} from '../gitApi'
import { IconBranch, IconChevronDown, IconFile, IconRefresh, IconSpinner } from './icons'
import './ReviewPanel.css'

/** 单个文件行的展开态。 */
interface RowState {
  expanded: boolean
  loading: boolean
  diff: GitDiff | null
  error: string
}

export interface ReviewPanelProps {
  /** 工作区绝对路径。 */
  workspacePath: string
  /** 会话 id:状态按会话缓存,切会话重拉。 */
  sessionId: string | null
}

export function ReviewPanel({ workspacePath, sessionId }: ReviewPanelProps) {
  const [status, setStatus] = useState<GitStatus | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState('')
  const [source, setSource] = useState<GitSource>('unstaged')
  const [rows, setRows] = useState<Record<string, RowState>>({})
  const [filter, setFilter] = useState('')
  /** 请求代号:防止慢响应覆盖新状态(切来源/刷新时的竞态)。 */
  const revisionRef = useRef(0)

  const refresh = useCallback(async () => {
    revisionRef.current += 1
    const revision = revisionRef.current
    setLoading(true)
    setError('')
    try {
      const next = await fetchGitStatus(workspacePath)
      if (revisionRef.current !== revision) return
      setStatus(next)
      // 状态变了:丢弃所有已缓存的 diff(行内容可能已过期)。
      setRows({})
    } catch (cause) {
      if (revisionRef.current !== revision) return
      setError(cause instanceof Error ? cause.message : String(cause))
      setStatus(null)
    } finally {
      if (revisionRef.current === revision) setLoading(false)
    }
  }, [workspacePath])

  useEffect(() => {
    void refresh()
  }, [refresh, sessionId])

  const sections = useMemo(
    () => (status?.isRepository ? sectionize(status.entries, source) : []),
    [status, source],
  )

  const visibleSections = useMemo(() => {
    const query = filter.trim().toLowerCase()
    if (!query) return sections
    return sections
      .map((section) => ({
        ...section,
        entries: section.entries.filter((entry) =>
          entry.path.toLowerCase().includes(query),
        ),
      }))
      .filter((section) => section.entries.length > 0)
  }, [sections, filter])

  const totalFiles = useMemo(() => flattenSections(sections).length, [sections])

  /** 展开/收起一行,首次展开时懒加载 diff。 */
  const toggle = useCallback(
    (entry: GitEntry) => {
      const current = rows[entry.path]
      if (current?.expanded) {
        setRows((prev) => ({ ...prev, [entry.path]: { ...current, expanded: false } }))
        return
      }
      setRows((prev) => ({
        ...prev,
        [entry.path]: {
          expanded: true,
          // 已经有 diff 就不重复拉(收起再展开是常见操作)。
          loading: !current?.diff,
          diff: current?.diff ?? null,
          error: '',
        },
      }))
      if (current?.diff) return
      const revision = revisionRef.current
      fetchGitDiff(workspacePath, entry.path, source)
        .then((diff) => {
          // 期间刷新过状态:结果已过期,丢弃。
          if (revisionRef.current !== revision) return
          setRows((prev) => ({
            ...prev,
            [entry.path]: { expanded: true, loading: false, diff, error: '' },
          }))
        })
        .catch((cause: unknown) => {
          if (revisionRef.current !== revision) return
          setRows((prev) => ({
            ...prev,
            [entry.path]: {
              expanded: true,
              loading: false,
              diff: null,
              error: cause instanceof Error ? cause.message : String(cause),
            },
          }))
        })
    },
    [rows, workspacePath, source],
  )

  const sourceLabel = (value: GitSource) =>
    value === 'staged'
      ? t('sidePaneReviewStaged')
      : value === 'branch'
        ? t('sidePaneReviewBranch')
        : t('sidePaneReviewUnstaged')

  return (
    <div className="review-panel">
      <div className="review-head">
        <div className="review-sources" role="tablist" aria-label={t('sidePaneReviewSource')}>
          {GIT_SOURCES.map((value) => (
            <button
              key={value}
              type="button"
              role="tab"
              aria-selected={source === value}
              className="review-source"
              onClick={() => {
                setSource(value)
                setRows({})
              }}
            >
              {sourceLabel(value)}
            </button>
          ))}
        </div>
        <button
          type="button"
          className="review-refresh"
          title={t('sidePaneReviewRefresh')}
          aria-label={t('sidePaneReviewRefresh')}
          disabled={loading}
          onClick={() => void refresh()}
        >
          {loading ? <IconSpinner size={14} /> : <IconRefresh size={14} />}
        </button>
      </div>

      {status?.isRepository && (
        <div className="review-meta">
          <span className="review-branch" title={status.branch ?? undefined}>
            <IconBranch size={12} />
            {status.detached
              ? `${t('sidePaneReviewDetached')} ${status.head ?? ''}`.trim()
              : (status.branch ?? t('sidePaneReviewNoBranch'))}
          </span>
          {status.ahead != null && status.behind != null && (
            <span className="review-ahead-behind">
              ↑{status.ahead} ↓{status.behind}
            </span>
          )}
        </div>
      )}

      {error && (
        <p className="review-error" role="alert">
          {error}
        </p>
      )}

      {status && !status.isRepository && !loading && (
        <div className="review-empty">
          <IconFile size={28} />
          <p className="review-empty-title">{t('sidePaneReviewNotRepo')}</p>
          <p className="review-empty-desc">{t('sidePaneReviewNotRepoHint')}</p>
        </div>
      )}

      {status?.isRepository && totalFiles > 0 && (
        <div className="review-filter">
          <input
            type="search"
            className="review-filter-input"
            value={filter}
            placeholder={t('sidePaneReviewFilter')}
            onChange={(event) => setFilter(event.target.value)}
          />
        </div>
      )}

      {status?.isRepository && totalFiles === 0 && !loading && (
        <div className="review-empty">
          <IconFile size={28} />
          <p className="review-empty-title">{t('sidePaneReviewClean')}</p>
          <p className="review-empty-desc">{t('sidePaneReviewCleanHint')}</p>
        </div>
      )}

      {status?.isRepository && totalFiles > 0 && (
        <div className="review-list">
          {visibleSections.length === 0 && (
            <p className="review-empty-line">{t('sidePaneReviewNoMatch')}</p>
          )}
          {visibleSections.map((section) => (
            <section key={section.id} className="review-section">
              <h3 className="review-section-title">
                {section.id === 'staged'
                  ? t('sidePaneReviewSectionStaged')
                  : section.id === 'untracked'
                    ? t('sidePaneReviewSectionUntracked')
                    : section.id === 'conflicted'
                      ? t('sidePaneReviewSectionConflicted')
                      : t('sidePaneReviewSectionUnstaged')}
                <span className="review-section-count">{section.entries.length}</span>
              </h3>
              {section.entries.map((entry) => {
                const state = rows[entry.path]
                const expanded = state?.expanded ?? false
                return (
                  <div className="review-row" key={`${section.id}:${entry.path}`}>
                    <button
                      type="button"
                      className="review-row-head"
                      aria-expanded={expanded}
                      onClick={() => toggle(entry)}
                    >
                      <span className="review-row-icon" aria-hidden="true">
                        <IconFile size={14} />
                      </span>
                      <span className="review-row-path" title={entry.path}>
                        {entry.path}
                        {entry.renamedFrom && (
                          <span className="review-row-renamed"> ← {entry.renamedFrom}</span>
                        )}
                      </span>
                      <span className="review-row-status" aria-hidden="true">
                        {statusLabel(entry)}
                      </span>
                      <IconChevronDown
                        size={14}
                        className={`review-row-chevron${expanded ? ' open' : ''}`}
                      />
                    </button>
                    {expanded && (
                      <div className="review-diff">
                        {state?.loading && (
                          <div className="review-diff-hint">
                            <IconSpinner size={14} />
                            {t('loading')}
                          </div>
                        )}
                        {state?.error && (
                          <div className="review-diff-hint error">{state.error}</div>
                        )}
                        {state?.diff && <DiffBody diff={state.diff} />}
                      </div>
                    )}
                  </div>
                )
              })}
            </section>
          ))}
        </div>
      )}

      {loading && !status && (
        <div className="review-empty">
          <IconSpinner size={24} />
          <p className="review-empty-title">{t('loading')}</p>
        </div>
      )}
    </div>
  )
}

/** 状态字符 → 短标签(`M` 修改 / `A` 新增 / `D` 删除 / `?` 未跟踪 / `R` 重命名)。 */
function statusLabel(entry: GitEntry): string {
  if (entry.conflicted) return 'U'
  if (entry.untracked) return '?'
  const code = entry.staged ? entry.indexStatus : entry.worktreeStatus
  return code.trim() || '·'
}

/**
 * Diff 正文:三级降级。
 *
 * 顺序与 ZCode 的 `SOt` 一致 —— 有 patch 就渲染 patch(信息最全);
 * 否则用前后全文自建对比;都没有就按 `availability` 给一句可读的说明。
 */
function DiffBody({ diff }: { diff: GitDiff }) {
  if (diff.availability === 'binary') {
    return <div className="review-diff-hint">{t('sidePaneDiffBinary')}</div>
  }
  if (diff.availability === 'truncated') {
    return <div className="review-diff-hint">{t('sidePaneDiffTruncated')}</div>
  }
  if (diff.availability === 'unavailable') {
    return (
      <div className="review-diff-hint">
        {t('sidePaneDiffUnavailable')}
        {diff.summary ? ` · ${diff.summary}` : ''}
      </div>
    )
  }

  if (diff.patch) return <PatchView patch={diff.patch} />

  // 没有 patch 但有全文:新增文件的场景(未跟踪文件没有 diff 基线)。
  if (diff.afterContent != null && diff.beforeContent == null) {
    return <FullTextAdd content={diff.afterContent} />
  }
  if (diff.beforeContent != null && diff.afterContent != null) {
    return <SideBySide before={diff.beforeContent} after={diff.afterContent} />
  }
  return <div className="review-diff-hint">{t('sidePaneDiffUnavailable')}</div>
}

/** 逐行给 patch 上色。 */
function PatchView({ patch }: { patch: string }) {
  const lines = useMemo(() => patch.split('\n'), [patch])
  return (
    <pre className="review-patch">
      {lines.map((line, index) => {
        const kind = line.startsWith('+++') || line.startsWith('---')
          ? 'meta'
          : line.startsWith('@@')
            ? 'hunk'
            : line.startsWith('+')
              ? 'add'
              : line.startsWith('-')
                ? 'del'
                : line.startsWith('diff ') || line.startsWith('index ')
                  ? 'meta'
                  : 'ctx'
        return (
          // patch 行没有稳定 key(内容可能重复),用下标 —— 列表是只读且不重排的。
          <div className={`review-line ${kind}`} key={index}>
            {line || ' '}
          </div>
        )
      })}
    </pre>
  )
}

/** 整文件新增(未跟踪文件):全部按"新增"着色。 */
function FullTextAdd({ content }: { content: string }) {
  const lines = useMemo(() => content.split('\n'), [content])
  return (
    <pre className="review-patch">
      {lines.map((line, index) => (
        <div className="review-line add" key={index}>
          {line || ' '}
        </div>
      ))}
    </pre>
  )
}

/**
 * 前后全文对比:用最朴素的 LCS 逐行 diff。
 *
 * 为什么不引 diff 库:这里只需要"能看出改了哪几行",而 patch 路径已经
 * 覆盖了绝大多数场景(有 git 基线时)。全文对比只在"git 给不出 patch"时
 * 兜底,朴素算法足够;引库的收益不抵它的包体积与主题适配成本。
 *
 * 复杂度 O(n*m) 在超大文件上会炸,所以先按行数截断 —— 超过就直接平铺,
 * 不做对齐。
 */
const LCS_LINE_LIMIT = 1500

function SideBySide({ before, after }: { before: string; after: string }) {
  const rows = useMemo(() => {
    const left = before.split('\n')
    const right = after.split('\n')
    if (left.length > LCS_LINE_LIMIT || right.length > LCS_LINE_LIMIT) {
      return [
        ...left.map((line) => ({ kind: 'del' as const, text: line })),
        ...right.map((line) => ({ kind: 'add' as const, text: line })),
      ]
    }
    return diffLines(left, right)
  }, [before, after])

  return (
    <pre className="review-patch">
      {rows.map((row, index) => (
        <div className={`review-line ${row.kind}`} key={index}>
          {row.text || ' '}
        </div>
      ))}
    </pre>
  )
}

interface DiffRow {
  kind: 'ctx' | 'add' | 'del'
  text: string
}

/** 标准 LCS 回溯。 */
function diffLines(left: string[], right: string[]): DiffRow[] {
  const rows = left.length
  const cols = right.length
  // 二维表按 (rows+1) x (cols+1);用 Int32Array 扁平化省内存。
  const table = new Int32Array((rows + 1) * (cols + 1))
  const at = (i: number, j: number) => i * (cols + 1) + j
  for (let i = rows - 1; i >= 0; i -= 1) {
    for (let j = cols - 1; j >= 0; j -= 1) {
      table[at(i, j)] =
        left[i] === right[j]
          ? table[at(i + 1, j + 1)]! + 1
          : Math.max(table[at(i + 1, j)]!, table[at(i, j + 1)]!)
    }
  }
  const out: DiffRow[] = []
  let i = 0
  let j = 0
  while (i < rows && j < cols) {
    if (left[i] === right[j]) {
      out.push({ kind: 'ctx', text: left[i]! })
      i += 1
      j += 1
    } else if (table[at(i + 1, j)]! >= table[at(i, j + 1)]!) {
      out.push({ kind: 'del', text: left[i]! })
      i += 1
    } else {
      out.push({ kind: 'add', text: right[j]! })
      j += 1
    }
  }
  while (i < rows) {
    out.push({ kind: 'del', text: left[i]! })
    i += 1
  }
  while (j < cols) {
    out.push({ kind: 'add', text: right[j]! })
    j += 1
  }
  return out
}

export default ReviewPanel
