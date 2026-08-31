import { useEffect, useMemo, useRef, useState } from 'react'
import * as api from '../api'
import { foldEvents, hasOpenTurn } from '../fold'
import type { TranscriptNode } from '../fold'
import { t } from '../i18n'
import type { Notify } from '../App'
import { IconFolder, IconSend, IconStop } from './icons'
import { Transcript } from './transcript'
import type { ModelCatalog, ModelSelection, SessionEnvelope, SessionHeader } from '../types'

export function SessionView({
  id,
  catalog,
  notify,
  onRunningChange,
}: {
  id: string
  catalog: ModelCatalog | null
  notify: Notify
  onRunningChange?: (id: string, running: boolean) => void
}) {
  const [events, setEvents] = useState<SessionEnvelope[]>([])
  const [header, setHeader] = useState<SessionHeader | null>(null)
  const [running, setRunning] = useState(false)
  const [selection, setSelection] = useState<ModelSelection | null>(null)
  const [prompt, setPrompt] = useState('')
  const [startedAt, setStartedAt] = useState<number | null>(null)
  const [now, setNow] = useState(0)
  const cursor = useRef(0)
  const follow = useRef<AbortController | null>(null)
  const scrollRef = useRef<HTMLDivElement | null>(null)
  const resnapshotRef = useRef<() => void>(() => {})

  useEffect(() => {
    if (catalog && !selection) setSelection(catalog.default)
  }, [catalog, selection])

  useEffect(() => {
    if (running) {
      setStartedAt(Date.now())
      setNow(Date.now())
      const interval = window.setInterval(() => setNow(Date.now()), 250)
      return () => window.clearInterval(interval)
    }
    setStartedAt(null)
  }, [running])

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
        setEvents(data.events)
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
              setEvents((previous) => [...previous, envelope])
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
    setEvents([])
    setHeader(null)
    cursor.current = 0
    setRunning(false)
    resnapshotRef.current()
    return () => follow.current?.abort()
  }, [id])

  useEffect(() => {
    scrollRef.current?.scrollTo({ top: scrollRef.current.scrollHeight })
  }, [events, running])

  const nodes: TranscriptNode[] = useMemo(() => foldEvents(events), [events])

  const group = catalog?.groups.find((g) => g.id === selection?.provider)
  const model = group?.models.find((m) => m.id === selection?.model)
  const efforts = model?.reasoning?.efforts ?? []

  const send = async () => {
    const message = prompt.trim()
    if (!message || !selection) return
    try {
      await api.postPrompt(id, {
        prompt: message,
        provider: selection.provider,
        model: selection.model,
        reasoningEffort: selection.reasoningEffort,
      })
      setPrompt('')
      setRunning(true)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }

  const stop = async () => {
    try {
      await api.cancelSession(id)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
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
      <div className="transcript" ref={scrollRef}>
        <Transcript nodes={nodes} />
        {running && (
          <div className="status-line">
            <span className="shimmer-text">{t('runningLabel')}</span>
            {startedAt && (
              <span className="clock">{((now - startedAt) / 1000).toFixed(1)}s</span>
            )}
          </div>
        )}
      </div>
      <div className="composer-zone">
        <div className="composer">
          <textarea
            placeholder={t('chatPlaceholder')}
            value={prompt}
            onChange={(event) => setPrompt(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter' && !event.shiftKey) {
                event.preventDefault()
                if (!running) void send()
              }
            }}
          />
          <div className="composer-toolbar">
            {selection && (
              <>
                <select
                  className="chip-select"
                  value={selection.provider}
                  onChange={(event) => {
                    const provider = event.target.value
                    const nextGroup = catalog?.groups.find((g) => g.id === provider)
                    const first = nextGroup?.models[0]
                    setSelection({
                      provider,
                      model: first?.id ?? '',
                      reasoningEffort: first?.reasoning?.defaultEffort,
                    })
                  }}
                >
                  {(catalog?.groups ?? []).map((g) => (
                    <option key={g.id} value={g.id}>
                      {g.name}
                    </option>
                  ))}
                </select>
                <select
                  className="chip-select"
                  value={selection.model}
                  onChange={(event) => {
                    const next = group?.models.find((m) => m.id === event.target.value)
                    setSelection({
                      ...selection,
                      model: event.target.value,
                      reasoningEffort: next?.reasoning?.defaultEffort ?? selection.reasoningEffort,
                    })
                  }}
                >
                  {(group?.models ?? []).map((m) => (
                    <option key={m.id} value={m.id}>
                      {m.name}
                    </option>
                  ))}
                </select>
                {efforts.length > 0 && (
                  <select
                    className="chip-select"
                    value={selection.reasoningEffort ?? ''}
                    onChange={(event) => {
                      const value = event.target.value
                      setSelection({ ...selection, reasoningEffort: value || undefined })
                    }}
                  >
                    <option value="">{t('providerDefault')}</option>
                    {efforts.map((effort) => (
                      <option key={effort.id} value={effort.id}>
                        {effort.name}
                      </option>
                    ))}
                  </select>
                )}
              </>
            )}
            <span className="spacer" />
            {running ? (
              <button className="btn-send stop" onClick={() => void stop()} title={t('stop')}>
                <IconStop size={14} />
              </button>
            ) : (
              <button
                className="btn-send"
                disabled={!prompt.trim()}
                onClick={() => void send()}
                title={t('run')}
              >
                <IconSend size={16} />
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}
