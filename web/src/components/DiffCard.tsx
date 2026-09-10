import { memo } from 'react'
import { t } from '../i18n'
import type { EditDiff } from '../toolDisplay'

/**
 * edit 工具的行级 diff 卡片:行号双列(旧/新)+ 符号列 + 正文。
 *
 * 纯展示组件(无状态),memo 化后流式帧不会带着它重渲染;折叠区只渲染
 * 一行占位,超长编辑也不会在工具行里挂一整屏。
 */
export const DiffCard = memo(function DiffCard({ diff }: { diff: EditDiff }) {
  return (
    <div className="diff-card">
      <div className="diff-head">
        <span className="diff-path" title={diff.path}>
          {diff.path}
        </span>
        {diff.startLine > 1 && (
          <span className="diff-anchor">{t('diffAtLine', { line: diff.startLine })}</span>
        )}
        <DiffStat diff={diff} />
      </div>
      <div className="diff-body">
        {diff.lines.map((line, index) =>
          line.fold ? (
            <div className="diff-line fold" key={index}>
              <span className="diff-gutter">⋯</span>
              <span className="diff-text">{t('diffFolded', { n: diff.skipped })}</span>
            </div>
          ) : (
            <div className={`diff-line ${line.kind}`} key={index}>
              <span className="diff-gutter">{line.oldNo ?? ''}</span>
              <span className="diff-gutter">{line.newNo ?? ''}</span>
              <span className="diff-sign">
                {line.kind === 'add' ? '+' : line.kind === 'del' ? '−' : ''}
              </span>
              <span className="diff-text">{line.text === '' ? ' ' : line.text}</span>
            </div>
          ),
        )}
      </div>
    </div>
  )
})

/**
 * 增删计数:`+N` 在左、`−N` 在右;值为 0 的一侧不展示(纯新增就不挂 `−0`)。
 * 两侧都是 0 时整块不渲染 —— 那种编辑(old==new)没有可展示的增删事实。
 *
 * `compact` 用于工具行头部(更小的字号);卡片顶部用默认尺寸。
 */
export function DiffStat({ diff, compact = false }: { diff: EditDiff; compact?: boolean }) {
  if (diff.added === 0 && diff.removed === 0) return null
  const parts: string[] = []
  if (diff.removed > 0) parts.push(t('diffRemoved', { n: diff.removed }))
  if (diff.added > 0) parts.push(t('diffAdded', { n: diff.added }))
  return (
    <span className={`diff-stat${compact ? ' compact' : ''}`} title={parts.join(' · ')}>
      {diff.added > 0 && <em className="add">+{diff.added}</em>}
      {diff.removed > 0 && <em className="del">−{diff.removed}</em>}
    </span>
  )
}
