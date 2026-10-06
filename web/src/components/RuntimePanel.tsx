import { useEffect, useState } from 'react'
import { t } from '../i18n'
import { setActiveId } from '../appStore'
import './RuntimePanel.css'

/**
 * 子代理行：除运行状态外还带派遣快照摘要（profile、有效模型、权限上限、
 * 冻结工具数、项目级指令范围）与等待交互计数。
 *
 * 这些字段全部来自服务端 `GET /api/sessions/{id}/agents`（`Runtime::list`），
 * UI 不做任何本地推论：父代理看到的"这个 child 是谁、能用什么、在等什么"
 * 必须与执行层同源。
 */
type Agent = {
  id: string
  parentId: string
  label: string
  depth: number
  status: string
  selection: { provider: string; model: string; reasoningEffort?: string }
  profile?: { qualifiedId: string; revision: number; inline: boolean } | null
  name?: string | null
  description?: string | null
  toolCount?: number
  tools?: string[]
  permissionCeiling?: string
  createdPermissionMode?: string | null
  instructionScope?: string | null
  delegationAllowed?: boolean
  pendingApprovals?: number
  pendingAsks?: number
  waitingInteraction?: boolean
  result?: string | null
  migration?: { needsRedispatch: boolean; projection?: string; diagnostic: string } | null
}
type Job = { id: string; label: string; status: string; startedAt: number; finishedAt?: number; exitCode?: number }
async function request<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(path, { method: body === undefined ? 'GET' : 'POST', headers: { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(15000) })
  const value = await response.json()
  if (!response.ok) throw new Error(value.error?.message ?? response.statusText)
  return value as T
}
const statusKey = { running: 'runtimeRunning', waiting: 'runtimeWaiting', settled: 'runtimeSettled', stopping: 'runtimeStopping', killed: 'runtimeKilled', failed: 'runtimeFailed', completed: 'runtimeCompleted' } as const
function status(value: string) { return t(statusKey[value as keyof typeof statusKey] ?? 'runtimeSettled') }
function profileLabel(agent: Agent) {
  if (!agent.profile?.qualifiedId) return t('runtimeProfileUnknown')
  return agent.profile.inline ? `${agent.profile.qualifiedId} · ${t('runtimeProfileInline')}` : agent.profile.qualifiedId
}
function ceilingLabel(agent: Agent) {
  return agent.permissionCeiling === 'read-only' ? t('subagentsCeilingReadOnly') : t('subagentsCeilingInherit')
}
function waitingLabel(agent: Agent) {
  const approvals = agent.pendingApprovals ?? 0
  const asks = agent.pendingAsks ?? 0
  if (!approvals && !asks) return null
  return [approvals ? `${t('runtimeWaitingApproval')} ${approvals}` : '', asks ? `${t('runtimeWaitingAsk')} ${asks}` : '']
    .filter(Boolean)
    .join(' · ')
}

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
  const [detail, setDetail] = useState('')
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
  const detailAgent = agents.find(agent => agent.id === detail)

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
          {agents.map(agent => {
            const waiting = waitingLabel(agent)
            return <div className="runtime-row" key={agent.id} data-agent={agent.id}>
              <div>
                <button type="button" onClick={() => setActiveId(agent.id, null)}>{agent.name || agent.label}</button>
                <small title={profileLabel(agent)}>{agent.selection.provider}/{agent.selection.model} · {status(agent.status)} · {t('runtimeDepth')} {agent.depth}</small>
                <small className="runtime-meta" data-profile={agent.profile?.qualifiedId ?? ''}>
                  {profileLabel(agent)} · {t('subagentsToolsCount').replace('{n}', String(agent.toolCount ?? 0))} · {ceilingLabel(agent)}
                  {agent.instructionScope === 'project-only' && ` · ${t('runtimeProjectOnly')}`}
                </small>
                {waiting && <small className="runtime-waiting" role="status">⏳ {waiting}</small>}
                {agent.result && agent.status === 'settled' && <small className="runtime-result">{t('runtimeResult')}：{agent.result}</small>}
                {agent.migration && <small className="runtime-result" role="status" data-migration={agent.migration.projection ?? ''}>{agent.migration.diagnostic}</small>}
              </div>
              <button type="button" onClick={() => setDetail(detail === agent.id ? '' : agent.id)}>{t('runtimeSnapshot')}</button>
              {agent.parentId === id && <button type="button" disabled={busy} onClick={() => setTarget(agent.id)}>{t('runtimeMessage')}</button>}
              {agent.status === 'running' && <button type="button" disabled={busy} onClick={() => void act(async () => { await request(`${base}/agents/${agent.id}/interrupt`, {}) })}>{t('runtimeInterrupt')}</button>}
            </div>
          })}
          {detailAgent && <div className="runtime-snapshot" data-testid="runtime-snapshot">
            <div><strong>{t('runtimeSnapshot')}</strong><button type="button" onClick={() => setDetail('')}>{t('runtimeClose')}</button></div>
            <dl>
              <dt>{t('runtimeProfile')}</dt><dd>{profileLabel(detailAgent)}</dd>
              <dt>{t('subagentsDescription')}</dt><dd>{detailAgent.description || '-'}</dd>
              <dt>{t('subagentsModelTitle')}</dt><dd>{detailAgent.selection.provider}/{detailAgent.selection.model}{detailAgent.selection.reasoningEffort ? ` · ${detailAgent.selection.reasoningEffort}` : ''}</dd>
              <dt>{t('subagentsCeilingTitle')}</dt><dd>{ceilingLabel(detailAgent)}{detailAgent.createdPermissionMode ? ` · ${t('runtimeCreatedMode')} ${detailAgent.createdPermissionMode}` : ''}</dd>
              <dt>{t('subagentsToolsTitle')}</dt><dd>{(detailAgent.tools ?? []).join('、') || t('subagentsToolsNone')}</dd>
              <dt>{t('runtimeInstructionScope')}</dt><dd>{detailAgent.instructionScope ?? '-'}</dd>
              <dt>{t('runtimeDelegation')}</dt><dd>{t('runtimeDelegationDenied')}</dd>
              <dt>{t('runtimeResult')}</dt><dd>{detailAgent.result ?? '-'}</dd>
            </dl>
            <button type="button" onClick={() => setActiveId(detailAgent.id, null)}>{t('runtimeOpenChild')}</button>
          </div>}
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
