import { useEffect, useState } from 'react'
import { applyEnvelope, foldEvents } from '../fold'
import type { TranscriptNode } from '../fold'
import { t } from '../i18n'
import type { StreamListener } from '../hooks/useSessionStreams'
import type { TodoItem } from '../types'
import { Transcript } from './transcript'

/** Latest todo snapshot wins; both snapshot and live frames feed it. */
function latestTodos(events: { type: string; todos?: TodoItem[] }[]): TodoItem[] {
  let todos: TodoItem[] = []
  for (const event of events) {
    if (event.type === 'todo-write' && event.todos) todos = event.todos
  }
  return todos
}

/**
 * Transcript content for one session. Scroll, follow, and jump-to-bottom live
 * on the parent conversation scrollport (dsh ConversationRoot pattern).
 */
export function SessionView({
  id,
  running,
  attach,
  onNodesChange,
  onTodosChange,
}: {
  id: string
  running: boolean
  attach: (id: string, listener: StreamListener) => () => void
  /** 内容变化时通知父级滚动区做贴底跟随。 */
  onNodesChange?: (nodes: TranscriptNode[]) => void
  /** todo 快照变化时通知父级(面板挂在 composer 上方,不在本组件树里)。 */
  onTodosChange?: (todos: TodoItem[]) => void
}) {
  const [nodes, setNodes] = useState<TranscriptNode[]>([])
  const [loading, setLoading] = useState(true)

  useEffect(() => {
    onNodesChange?.(nodes)
  }, [nodes, onNodesChange])

  useEffect(() => {
    setNodes([])
    setLoading(true)
    onTodosChange?.([])
    return attach(id, {
      onSnapshot: (_header, events) => {
        setNodes(foldEvents(events))
        onTodosChange?.(latestTodos(events))
        setLoading(false)
      },
      onEnvelope: (envelope) => {
        setNodes((previous) => applyEnvelope(previous, envelope))
        if (envelope.type === 'todo-write') onTodosChange?.(envelope.todos)
      },
    })
  }, [id, attach, onTodosChange])

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
