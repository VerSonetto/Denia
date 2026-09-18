import { memo } from 'react'
import { t } from '../i18n'
import type { EditDiff } from '../toolDisplay'

/**
 * edit 工具的行级 diff 卡片:行号双列(旧/新)+ 符号列 + 正文。
 *
 * 纯展示组件(无状态),memo 化后流式帧不会带着它重渲染;折叠区只渲染
 * 一行占位,超长编辑也不会在工具行里挂一整屏。多处编辑一组多张卡,
 * `hidePath` 隐藏重复的文件名(路径已在外层展示过一次)。
 */
export const DiffCard = memo(function DiffCard({
  diff,
  hidePath = false,
}: {
  diff: EditDiff
  hidePath?: boolean
}) {
  // 藏路径的多 hunk 组里,首行锚点为 1 且无增删时头部没有任何内容,
  // 整条不渲染,避免留一条空带。
  const showHead = !hidePath || diff.startLine > 1 || diff.added > 0 || diff.removed > 0
  return (
    <div className="diff-card">
      {showHead && (
        <div className="diff-head">
          {!hidePath && (
            <span className="diff-path" title={diff.path}>
              {diff.path}
            </span>
          )}
          {diff.startLine > 1 && (
            <span className="diff-anchor">{t('diffAtLine', { line: diff.startLine })}</span>
          )}
          <DiffStat added={diff.added} removed={diff.removed} />
        </div>
      )}
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
 * 收纯数值而非 EditDiff:多处编辑的工具行头部要挂的是跨 hunk 的聚合值。
 *
 * `compact` 用于工具行头部(更小的字号);卡片顶部用默认尺寸。
 */
export function DiffStat({
  added,
  removed,
  compact = false,
}: {
  added: number
  removed: number
  compact?: boolean
}) {
  if (added === 0 && removed === 0) return null
  const parts: string[] = []
  if (removed > 0) parts.push(t('diffRemoved', { n: removed }))
  if (added > 0) parts.push(t('diffAdded', { n: added }))
  return (
    <span className={`diff-stat${compact ? ' compact' : ''}`} title={parts.join(' · ')}>
      {added > 0 && <em className="add">+{added}</em>}
      {removed > 0 && <em className="del">−{removed}</em>}
    </span>
  )
}
