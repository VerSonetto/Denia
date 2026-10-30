import { useMemo, useState } from 'react'
import { localeRevision, t } from '../../i18n'
import { MarkdownText } from '../../markdown/MarkdownText'
import type { MarkdownLabels } from '../../markdown/MarkdownText'
import type { TranscriptNode } from '../../fold'

export function CompactionRow({ node }: { node: Extract<TranscriptNode, { kind: 'compaction' }> }) {
  const [open, setOpen] = useState(false)
  const pre = node.preTokens ?? 0
  const post = node.postTokens ?? 0
  const saved = pre > post ? pre - post : 0
  // Hook 必须常驻组件顶层:放在条件 JSX 内会在展开/收起时改变 hook 数量,
  // 触发 React #310(Rendered more hooks than during the previous render)。
  const labels = useMemo<MarkdownLabels>(
    () => ({
      code: { copyLabel: t('copy'), copiedLabel: t('copied') },
      footnotes: t('footnotes'),
    }),
    [localeRevision()],
  )
  return (
    <div className="compaction-row">
      <button
        type="button"
        className="compaction-header"
        onClick={() => setOpen((v) => !v)}
        title={`replaces ${node.replacesFrom}..${node.replacesTo}, keep ${node.keepFrom}`}
      >
        <span className="badge">{t('contextCompacted')}</span>
        <span className="compaction-meta">
          {pre > 0 && post > 0
            ? t('contextCompactionTokens', { pre, post, saved })
            : t('contextCompactionRange', {
                from: node.replacesFrom,
                to: node.replacesTo,
              })}
        </span>
        <span className="chevron">{open ? '▾' : '▸'}</span>
      </button>
      {open && (
        <div className="compaction-body prose">
          <MarkdownText text={node.summary} streaming={false} labels={labels} />
        </div>
      )}
    </div>
  )
}
