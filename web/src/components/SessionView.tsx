import { useCallback, useEffect, useRef, useState } from 'react'
import * as api from '../api'
import { applyEnvelope, foldEvents, hasOpenTurn } from '../fold'
import type { TranscriptNode } from '../fold'
import { t } from '../i18n'
import type { Notify } from '../App'
import { IconFolder } from './icons'
import { Transcript } from './transcript'
import type { SessionHeader } from '../types'

/** dsh FOLLOW_THRESHOLD:离开底部超过该距离即停止自动跟随。 */
const FOLLOW_THRESHOLD = 24

/** Transcript pane for one session: snapshot + follow-SSE, incremental fold. */
export function SessionView({
  id,
  notify,
  onRunningChange,
}: {
  id: string
  notify: Notify
  onRunningChange?: (id: string, running: boolean) => void
}) {
  const [nodes, setNodes] = useState<TranscriptNode[]>([])
  const [header, setHeader] = useState<SessionHeader | null>(null)
  const [running, setRunning] = useState(false)
  const cursor = useRef(0)
  const follow = useRef<AbortController | null>(null)
  const scrollRef = useRef<HTMLDivElement | null>(null)
  const resnapshotRef = useRef<() => void>(() => {})
  // dsh 式跟随:贴底时内容变化才吸附;上翻越过阈值即停,回到底部恢复。
  const atBottomRef = useRef(true)
  const [, setAtBottom] = useState(true)

  const handleScroll = useCallback(() => {
    const el = scrollRef.current
    if (el === null) return
    const bottom = el.scrollHeight - el.scrollTop - el.clientHeight <= FOLLOW_THRESHOLD + 1
    if (bottom !== atBottomRef.current) {
      atBottomRef.current = bottom
      setAtBottom(bottom)
    }
  }, [])

  useEffect(() => {
    const el = scrollRef.current
    if (el === null || !atBottomRef.current) return
    el.scrollTop = el.scrollHeight
  }, [nodes, running])

  useEffect(() => {
    onRunningChange?.(id, running)
    return () => onRunningChange?.(id, false)
  }, [id, running, onRunningChange])

  resnapshotRef.current = () => {
    const resnapshot = async () => {
      try {
        const data = await api.getSession(id)
        cursor.current = data.events.length
          ? data.events[data.events.length - 1].seq
          : 0
        setNodes(foldEvents(data.events))
        setHeader(data.header)
        setRunning(hasOpenTurn(data.events))
        follow.current?.abort()
        follow.current = api.followSession(
          id,
          cursor.current,
          (envelope) => {
            if (envelope.seq <= cursor.current) return
            if (envelope.seq === cursor.current + 1) {
              cursor.current = envelope.seq
              setNodes((previous) => applyEnvelope(previous, envelope))
            } else {
              resnapshotRef.current()
            }
            if (envelope.type === 'turn-end') setRunning(false)
          },
          () => resnapshotRef.current(),
        )
      } catch (error) {
        notify('err', error instanceof Error ? error.message : String(error))
      }
    }
    void resnapshot()
  }

  useEffect(() => {
    setNodes([])
    setHeader(null)
    cursor.current = 0
    setRunning(false)
    atBottomRef.current = true
    setAtBottom(true)
    resnapshotRef.current()
    return () => follow.current?.abort()
  }, [id])

  return (
    <div className="column">
      <div className="column-head">
        {header && (
          <span className="cwd-chip" title={header.cwd}>
            <IconFolder size={12} />
            {header.cwd}
          </span>
        )}
      </div>
      <div className="transcript" ref={scrollRef} onScroll={handleScroll}>
        <Transcript nodes={nodes} />
        {running && <StatusLine />}
      </div>
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
