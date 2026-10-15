import { IconUpdate } from '../icons'
import { openUpdateDialog, useUpdateState } from '../../updateStore'
import { updateText } from './updateText'
import './updates.css'

export function UpdateBadge({ collapsed = false }: { collapsed?: boolean }) {
  const { check, status, dialogOpen } = useUpdateState()
  if (!check?.updateAvailable || !check.latest || status.phase === 'launched') return null
  const text = updateText()
  const label = text.badgeLabel.replace('{version}', check.latest.version)
  return (
    <button
      type="button"
      className={`update-badge${collapsed ? ' update-badge-collapsed' : ''}`}
      aria-label={label}
      aria-haspopup="dialog"
      aria-expanded={dialogOpen}
      title={label}
      onClick={() => openUpdateDialog()}
    >
      <IconUpdate size={collapsed ? 17 : 13} />
      {!collapsed && <span>{text.badge}</span>}
      {collapsed && <span className="update-badge-dot" aria-hidden="true" />}
    </button>
  )
}
