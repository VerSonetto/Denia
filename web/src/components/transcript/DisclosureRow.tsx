import { useState } from 'react'
import type { ReactNode } from 'react'
import { firstLine } from '../../toolDisplay'

export function DisclosureRow({
  title,
  icon,
  text,
}: {
  title: string
  icon: ReactNode
  text: string
}) {
  const [open, setOpen] = useState(false)
  const summary = firstLine(text, 72)
  return (
    <div className={`disc-row inject-row${open ? ' open' : ''}`}>
      <button className="disc-head" onClick={() => setOpen(!open)}>
        <span className="glyph">{icon}</span>
        <span className="title">{title}</span>
        {!open && (
          <>
            <span className="sep" />
            <span className="summary inject-summary">{summary}</span>
          </>
        )}
      </button>
      {open && (
        <div className="disc-body">
          <div className="code-card inject-card">
            <pre>{text}</pre>
          </div>
        </div>
      )}
    </div>
  )
}
