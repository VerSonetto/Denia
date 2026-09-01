import { useCallback, useEffect, useRef, useState } from 'react'
import { applyEnvelope, foldEvents } from '../fold'
import type { TranscriptNode } from '../fold'
import { t } from '../i18n'
import type { StreamListener } from '../hooks/useSessionStreams'
import { IconChevron, IconFolder } from './icons'
import { Transcript } from './transcript'
import type { SessionHeader } from '../types'

/** dsh FOLLOW_THRESHOLD:离开底部超过该距离即停止自动跟随。 */
const FOLLOW_THRESHOLD = 24

/**
 * Transcript pane for one session. The event stream (snapshot + follow,
 * dedupe, gap self-heal) lives in the App-level stream bus; this view only
 * subscribes and folds.
 */
export function SessionView({
  id,
  running,
  scrollTick,
  attach,
}: {
  id: string
  running: boolean
  /** 发送成功后递增:强制吸附回底部(离开底部时发送也拉回)。 */
  scrollTick?: number
  attach: (id: string, listener: StreamListener) => () => void
}) {
  const [nodes, setNodes] = useState<TranscriptNode[]>([])
  const [header, setHeader] = useState<SessionHeader | null>(null)
  const [loading, setLoading] = useState(true)
  const scrollRef = useRef<HTMLDivElement | null>(null)
  // dsh 式跟随:贴底时内容变化才吸附;上翻越过阈值即停,回到底部恢复。
  const atBottomRef = useRef(true)
  const [atBottom, setAtBottom] = useState(true)

  const handleScroll = useCallback(() => {
    const el = scrollRef.current
    if (el === null) return
    const bottom = el.scrollHeight - el.scrollTop - el.clientHeight <= FOLLOW_THRESHOLD + 1
    if (bottom !== atBottomRef.current) {
      atBottomRef.current = bottom
      setAtBottom(bottom)
    }
  }, [])

  // 贴底跟随:内容变化且仍在底部时才吸附。
  useEffect(() => {
    const el = scrollRef.current
    if (el === null || !atBottomRef.current) return
    el.scrollTop = el.scrollHeight
  }, [nodes, running])

  // 发送意图:无条件拉回底部。
  useEffect(() => {
    if (scrollTick === undefined || scrollTick === 0) return
    const el = scrollRef.current
    if (el === null) return
    el.scrollTop = el.scrollHeight
    atBottomRef.current = true
    setAtBottom(true)
  }, [scrollTick])

  useEffect(() => {
    setNodes([])
    setHeader(null)
    setLoading(true)
    atBottomRef.current = true
    setAtBottom(true)
    return attach(id, {
      onSnapshot: (nextHeader, events) => {
        setNodes(foldEvents(events))
        setHeader(nextHeader)
        setLoading(false)
      },
      onEnvelope: (envelope) => {
        setNodes((previous) => applyEnvelope(previous, envelope))
      },
    })
  }, [id, attach])

  const jumpToBottom = () => {
    const el = scrollRef.current
    if (el === null) return
    el.scrollTop = el.scrollHeight
    atBottomRef.current = true
    setAtBottom(true)
  }

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
        {loading ? (
          <div className="empty-hint">{t('loading')}</div>
        ) : (
          <Transcript nodes={nodes} />
        )}
        {running && <StatusLine />}
      </div>
      {!atBottom && (
        <button className="jump-bottom" onClick={jumpToBottom} title={t('jumpToBottom')}>
          <IconChevron size={14} />
        </button>
      )}
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