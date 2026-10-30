import { useState } from 'react'
import { t } from '../../i18n'
import { transcriptNodeKey, type OverviewRow } from '../../fold'
import { formatDuration } from './formatDuration'
import { IconChevron } from '../icons'
import { NodeView } from './NodeView'

export function TurnOverview({ row }: { row: OverviewRow }) {
  const [open, setOpen] = useState(false)
  return (
    <div className="process-group">
      <button className="process-head" onClick={() => setOpen((o) => !o)}>
        <span className={`chev${open ? ' open' : ''}`}>
          <IconChevron size={11} />
        </span>
        <span className="title">
          {t('processOverview', {
            duration: formatDuration(row.durationMs),
            n: row.toolCount,
          })}
        </span>
      </button>
      {open && (
        <div className="process-body">
          {row.hidden.map((node) => (
            <NodeView key={transcriptNodeKey(node)} node={node} />
          ))}
        </div>
      )}
    </div>
  )
}
