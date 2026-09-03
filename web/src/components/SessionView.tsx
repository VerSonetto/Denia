import { useEffect, useRef, useState } from 'react'
import { applyEnvelope, foldEvents } from '../fold'
import type { TranscriptNode } from '../fold'
import { t } from '../i18n'
import { attach } from '../sessionStreams'
import type { SessionEnvelope, TodoItem, UserMessageImage } from '../types'
import type { TrajectoryQuote } from '../trajectory'
import { Transcript } from './transcript'
import { TrajectoryView } from './TrajectoryView'

/** Latest todo snapshot wins; both snapshot and live frames feed it. */
function latestTodos(events: { type: string; todos?: TodoItem[] }[]): TodoItem[] {
  let todos: TodoItem[] = []
  for (const event of events) {
    if (event.type === 'todo-write' && Array.isArray(event.todos)) todos = event.todos
  }
  return todos
}

/**
 * 会话内容视图:transcript + 乐观用户行 + todo 快照。
 *
 * ## 性能
 * 流式帧经 rAF 合并派发——浏览器一帧最多一次 setNodes,
 * 高频 chunk(数百/s)不会触发等量 React 渲染;事件不丢失,
 * 逐条 fold 后一次性应用。
 *
 * ## 生命周期
 * - 卸载时取消 rAF 与流订阅,不残留定时器/帧回调。
 * - 会话被删除(404)→ `onNotFound` 通知父级清视图。
 */
export function SessionView({
  id,
  view = 'chat',
  pendingMessages,
  onTodosChange,
  onNotFound,
  onPendingSettled,
  onNodesChange,
  onRewind,
  onFork,
  onQuote,
}: {
  id: string
  /** 内容视图:对话 transcript 或轨迹台账(共用同一事件流订阅)。 */
  view?: 'chat' | 'trajectory'
  /** 已发送未获服务端确认的用户消息(乐观行,渲染在 transcript 尾部)。 */
  pendingMessages: { text: string; images?: UserMessageImage[] }[]
  onTodosChange?: (todos: TodoItem[]) => void
  /** 会话已被删除:父级应清空 activeId。 */
  onNotFound?: () => void
  /** 某条乐观消息已被服务端事件确认(从 pending 列表移除)。 */
  onPendingSettled?: (message: { text: string; images?: UserMessageImage[] }) => void
  /** 内容变化时通知父级(对话轴/统计条/贴底跟随)。 */
  onNodesChange?: (nodes: TranscriptNode[]) => void
  /** 用户消息回退按钮触发。 */
  onRewind?: (seq: number) => void
  /** 轮次收尾消息分支按钮触发(以该消息 seq 为锚点开新会话)。 */
  onFork?: (seq: number) => void
  /** 轨迹引用(单条/区间)提交到 composer。 */
  onQuote?: (quote: TrajectoryQuote) => void
}) {
  const [nodes, setNodes] = useState<TranscriptNode[]>([])
  // 原始事件流:轨迹视图的 fold 源(与 transcript 共用一次订阅)。
  const [events, setEvents] = useState<SessionEnvelope[]>([])
  const [loading, setLoading] = useState(true)
  const settleRef = useRef(onPendingSettled)
  settleRef.current = onPendingSettled
  const nodesChangeRef = useRef(onNodesChange)
  nodesChangeRef.current = onNodesChange

  // rAF 批处理:流式帧入队,每帧最多一次渲染。
  const queueRef = useRef<SessionEnvelope[]>([])
  const rafRef = useRef<number | undefined>(undefined)

  useEffect(() => {
    nodesChangeRef.current?.(nodes)
  }, [nodes])

  useEffect(() => {
    const flush = () => {
      rafRef.current = undefined
      const batch = queueRef.current
      queueRef.current = []
      if (batch.length === 0) return
      setEvents((previous) => [...previous, ...batch])
      setNodes((previous) => batch.reduce((acc, env) => applyEnvelope(acc, env), previous))
    }
    const settlePending = (events: SessionEnvelope[]) => {
      for (const event of events) {
        if (event.type === 'user-message' && !event.injected) {
          settleRef.current?.({ text: event.text, images: event.images })
        }
      }
    }
    const unsubscribe = attach(id, {
      onSnapshot: (_header, snapshot) => {
        queueRef.current = []
        setEvents(snapshot)
        setNodes(foldEvents(snapshot))
        onTodosChange?.(latestTodos(snapshot))
        settlePending(snapshot)
        setLoading(false)
      },
      onEnvelope: (envelope) => {
        settlePending([envelope])
        if (envelope.type === 'todo-write') {
          onTodosChange?.(envelope.todos)
        }
        queueRef.current.push(envelope)
        if (rafRef.current === undefined) {
          rafRef.current = window.requestAnimationFrame(flush)
        }
      },
      onNotFound: () => {
        onNotFound?.()
      },
    })
    return () => {
      unsubscribe()
      if (rafRef.current !== undefined) window.cancelAnimationFrame(rafRef.current)
      rafRef.current = undefined
      queueRef.current = []
      setEvents([])
    }
    // 仅依赖 id:监听回调经 ref 透传,避免每次渲染重挂流。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id])

  useEffect(() => {
    return () => {
      onTodosChange?.([])
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [id])

  if (loading) {
    return <div className="empty-hint">{t('loading')}</div>
  }

  if (view === 'trajectory') {
    return <TrajectoryView events={events} onQuote={onQuote} />
  }

  return (
    <div className="transcript-pane">
      <Transcript nodes={nodes} pendingMessages={pendingMessages} onRewind={onRewind} onFork={onFork} />
    </div>
  )
}
