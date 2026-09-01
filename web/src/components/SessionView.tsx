import { useEffect, useState } from 'react'
import { applyEnvelope, foldEvents } from '../fold'
import type { TranscriptNode } from '../fold'
import { t } from '../i18n'
import type { StreamListener } from '../hooks/useSessionStreams'
import { Transcript } from './transcript'

/**
 * Transcript content for one session. Scroll, follow, and jump-to-bottom live
 * on the parent conversation scrollport (dsh ConversationRoot pattern).
 */
export function SessionView({
  id,
  running,
  attach,
  onNodesChange,
}: {
  id: string
  running: boolean
  attach: (id: string, listener: StreamListener) => () => void
  /** 内容变化时通知父级滚动区做贴底跟随。 */
  onNodesChange?: (nodes: TranscriptNode[]) => void
}) {
  const [nodes, setNodes] = useState<TranscriptNode[]>([])
  const [loading, setLoading] = useState(true)

  useEffect(() => {
    onNodesChange?.(nodes)
  }, [nodes, onNodesChange])

  useEffect(() => {
    setNodes([])
    setLoading(true)
    return attach(id, {
      onSnapshot: (_header, events) => {
        setNodes(foldEvents(events))
        setLoading(false)
      },
      onEnvelope: (envelope) => {
        setNodes((previous) => applyEnvelope(previous, envelope))
      },
    })
  }, [id, attach])

  if (loading) {
    return <div className="empty-hint">{t('loading')}</div>
  }

  return (
    <div className="transcript-pane">
      <Transcript nodes={nodes} />
      {running && <StatusLine />}
    </div>
  )
}

/** Isolated ticking clock so 4Hz re-renders never touch the transcript. */
function StatusLine() {
  const [startedAt] = useState(() => Date.now())
  const [now, setNow] = useState(startedAt)
  useEffect(() => {
    const interval = window.setInterval(() => setNow(Date.now()), 250)
    return () => window.clearInterval(interval)
  }, [])
  return (
    <div className="status-line">
      <span className="shimmer-text">{t('runningLabel')}</span>
      <span className="clock">{((now - startedAt) / 1000).toFixed(1)}s</span>
    </div>
  )
}
