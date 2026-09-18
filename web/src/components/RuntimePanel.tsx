import { useEffect, useState } from 'react'
import { t } from '../i18n'
import { setActiveId } from '../appStore'
import './RuntimePanel.css'

type Agent = { id: string; parentId: string; label: string; depth: number; status: string; selection: { model: string } }
type Job = { id: string; label: string; status: string; startedAt: number; finishedAt?: number; exitCode?: number }
async function request<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(path, { method: body === undefined ? 'GET' : 'POST', headers: { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(15000) })
  const value = await response.json()
  if (!response.ok) throw new Error(value.error?.message ?? response.statusText)
  return value as T
}
const statusKey = { running: 'runtimeRunning', waiting: 'runtimeWaiting', settled: 'runtimeSettled', stopping: 'runtimeStopping', killed: 'runtimeKilled', failed: 'runtimeFailed', completed: 'runtimeCompleted' } as const
function status(value: string) { return t(statusKey[value as keyof typeof statusKey] ?? 'runtimeSettled') }

export function RuntimePanel({ id }: { id: string }) {
  const [open, setOpen] = useState(false)
  const [tab, setTab] = useState<'agents' | 'jobs'>('agents')
  const [agents, setAgents] = useState<Agent[]>([])
  const [jobs, setJobs] = useState<Job[]>([])
  const [error, setError] = useState('')
  const [busy, setBusy] = useState(false)
  const [preview, setPreview] = useState('')
  const [message, setMessage] = useState('')
  const [target, setTarget] = useState('')
  const activeAgents = agents.filter((agent) => agent.status === 'running' || agent.status === 'waiting').length
  const activeJobs = jobs.filter((job) => !job.finishedAt && job.status !== 'killed' && job.status !== 'failed').length
  const base = `/api/sessions/${encodeURIComponent(id)}`
  useEffect(() => {
    if (activeAgents === 0 && activeJobs > 0) setTab('jobs')
    else if (activeAgents > 0 && activeJobs === 0) setTab('agents')
  }, [activeAgents, activeJobs])
  useEffect(() => {
    let disposed = false
    let timer: ReturnType<typeof setTimeout>
    const refresh = async () => {
      try {
        if (document.visibilityState === 'visible') {
          const [a, j] = await Promise.all([request<{ agents: Agent[] }>(`${base}/agents`), request<{ jobs: Job[] }>(`${base}/jobs`)])
          if (!disposed) { setAgents(a.agents); setJobs(j.jobs) }
        }
      } catch (e) { if (!disposed) setError(String(e)) }
      if (!disposed) timer = setTimeout(refresh, 2000)
    }
    void refresh()
    return () => { disposed = true; clearTimeout(timer) }
  }, [base])
  const act = async (operation: () => Promise<void>) => {
    setBusy(true); setError('')
    try { await operation() } catch (e) { setError(e instanceof Error ? e.message : String(e)) } finally { setBusy(false) }
  }
  if (activeAgents === 0 && activeJobs === 0) return null

  const tabs = [
    ...(activeAgents > 0 ? (['agents'] as const) : []),
    ...(activeJobs > 0 ? (['jobs'] as const) : []),
  ]

  return <section className={`runtime-panel${open ? ' open' : ''}`}>
    <button type="button" className="runtime-toggle" aria-expanded={open} onClick={() => setOpen(!open)} title={t('runtimeTitle')}>
      <span className="runtime-toggle-icon" aria-hidden="true">✦</span>
      {(activeAgents > 0 || activeJobs > 0) && <span className="runtime-activity-dot" aria-label={t('runtimeActive')} />}
      {(activeAgents > 0 || activeJobs > 0) && <span className="runtime-toggle-label">{activeAgents + activeJobs} {t('runtimeActive')}</span>}
    </button>
    {open && <div className="runtime-content">
      <div className="runtime-tabs" role="tablist" aria-label={t('runtimeTitle')}>
        {tabs.map(key => <button type="button" key={key} role="tab" aria-selected={tab === key} onClick={() => { setTab(key); setPreview(''); setError('') }}>{t(key === 'agents' ? 'runtimeAgents' : 'runtimeJobs')}</button>)}
      </div>
      {error && <p role="alert" className="runtime-error">{error}</p>}
      <div className="runtime-rows">
        {tab === 'agents' && <>
          {!agents.length && <p className="runtime-empty">{t('runtimeNoAgents')}</p>}
          {agents.map(agent => <div className="runtime-row" key={agent.id}>
            <div><button type="button" onClick={() => setActiveId(agent.id, null)}>{agent.label}</button><small>{agent.selection.model} · {status(agent.status)} · {t('runtimeDepth')} {agent.depth}</small></div>
            {agent.parentId === id && <button type="button" disabled={busy} onClick={() => setTarget(agent.id)}>{t('runtimeMessage')}</button>}
            {agent.status === 'running' && <button type="button" disabled={busy} onClick={() => void act(async () => { await request(`${base}/agents/${agent.id}/interrupt`, {}) })}>{t('runtimeInterrupt')}</button>}
          </div>)}
          {target && <form className="runtime-message" onSubmit={e => { e.preventDefault(); void act(async () => { await request(`${base}/agents/${target}/message`, { message }); setMessage(''); setTarget('') }) }}>
            <label>{agents.find(a => a.id === target)?.label}<textarea value={message} onChange={e => setMessage(e.target.value)} placeholder={t('runtimeMessageHint')} /></label>
            <button disabled={busy || !message.trim()}>{t('runtimeSend')}</button><button type="button" onClick={() => setTarget('')}>{t('runtimeClose')}</button>
          </form>}
        </>}
        {tab === 'jobs' && <>
          {!jobs.length && <p className="runtime-empty">{t('runtimeNoJobs')}</p>}
          {jobs.map(job => <div className="runtime-row" key={job.id}>
            <div><span>{job.label}</span><small>{status(job.status)}{job.exitCode != null && ` · ${t('runtimeExit')} ${job.exitCode}`}</small></div>
            <button type="button" disabled={busy} onClick={() => void act(async () => { const value = await request<{ output: string }>(`${base}/jobs/${job.id}/output`); setPreview(value.output || t('runtimeNoOutput')) })}>{t('runtimeOutput')}</button>
            {!job.finishedAt && <button type="button" disabled={busy || job.status === 'stopping'} onClick={() => void act(async () => { await request(`${base}/jobs/${job.id}/kill`, {}) })}>{t('runtimeInterrupt')}</button>}
          </div>)}
        </>}

      </div>
      {preview && <div className="runtime-preview"><button type="button" onClick={() => setPreview('')}>{t('runtimeClose')}</button><pre>{preview}</pre></div>}
    </div>}
  </section>
}
