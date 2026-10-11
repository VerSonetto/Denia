import { useCallback, useEffect, useRef, useState } from 'react'
import type { KeyboardEvent } from 'react'
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

/** 行操作按 (种类, id, 动作) 三元组记 pending —— 一个卡住不该冻结整块面板。 */
type PendingKey = string
const pendingKey = (kind: 'agent' | 'job', id: string, op: string): PendingKey => `${kind}:${id}:${op}`

/** 后台任务输出：加载 / 就绪 / 失败三态，按 job id 存档（切 tab 不会丢）。 */
type Output =
  | { phase: 'loading' }
  | { phase: 'ready'; text: string }
  | { phase: 'error'; message: string }

/** 行的视觉编码分档：文字之外还要能被一眼扫出来。 */
type Tone = 'running' | 'waiting' | 'stopping' | 'failed' | 'idle'

async function request<T>(path: string, body?: unknown): Promise<T> {
  const response = await fetch(path, { method: body === undefined ? 'GET' : 'POST', headers: { 'content-type': 'application/json' }, body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(15000) })
  const value = await response.json()
  if (!response.ok) throw new Error(value.error?.message ?? response.statusText)
  return value as T
}

const statusKey = { running: 'runtimeRunning', waiting: 'runtimeWaiting', settled: 'runtimeSettled', stopping: 'runtimeStopping', killed: 'runtimeKilled', failed: 'runtimeFailed', completed: 'runtimeCompleted' } as const
function status(value: string) { return t(statusKey[value as keyof typeof statusKey] ?? 'runtimeSettled') }
function tone(value: string): Tone {
  if (value === 'running') return 'running'
  if (value === 'waiting') return 'waiting'
  if (value === 'stopping') return 'stopping'
  if (value === 'failed') return 'failed'
  return 'idle'
}
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

/** 轮询节奏：展开时盯紧，折叠时放慢 —— 省请求又不至于看着不动。 */
const INTERVAL_OPEN = 1500
const IDLE_INTERVAL = 4000

export function RuntimePanel({ id }: { id: string }) {
  const [open, setOpen] = useState(false)
  const [tab, setTab] = useState<'agents' | 'jobs'>('agents')
  const [agents, setAgents] = useState<Agent[]>([])
  const [jobs, setJobs] = useState<Job[]>([])
  const [error, setError] = useState('')
  const [pending, setPending] = useState<Record<PendingKey, boolean>>({})
  const [outputs, setOutputs] = useState<Record<string, Output>>({})
  const [outputTarget, setOutputTarget] = useState('')
  const [target, setTarget] = useState('')
  const [drafts, setDrafts] = useState<Record<string, string>>({})
  const [detail, setDetail] = useState('')
  const rootRef = useRef<HTMLElement | null>(null)

  const activeAgents = agents.filter((agent) => agent.status === 'running' || agent.status === 'waiting').length
  const activeJobs = jobs.filter((job) => !job.finishedAt && job.status !== 'killed' && job.status !== 'failed').length
  const base = `/api/sessions/${encodeURIComponent(id)}`

  const isPending = (key: PendingKey) => pending[key] === true
  const draft = target ? drafts[target] ?? '' : ''
  const messaging = target ? isPending(pendingKey('agent', target, 'message')) : false
  const setPendingFlag = (key: PendingKey, value: boolean) => {
    setPending((current) => {
      if (current[key] === value) return current
      const next = { ...current }
      if (value) next[key] = true
      else delete next[key]
      return next
    })
  }

  // 一侧归零时把 tab 带到还有内容的那一侧；两侧都空时不动（面板自己会收起）。
  useEffect(() => {
    if (activeAgents === 0 && activeJobs > 0) setTab('jobs')
    else if (activeAgents > 0 && activeJobs === 0) setTab('agents')
  }, [activeAgents, activeJobs])

  /**
   * 自动刷新。轮询本身永远在跑（活动数就是从这份列表算出来的），展开时切到
   * 1.5s —— open 进依赖里等于"一展开立刻重取一次"，不需要任何手动刷新。
   */
  const refresh = useCallback(async () => {
    const [a, j] = await Promise.all([request<{ agents: Agent[] }>(`${base}/agents`), request<{ jobs: Job[] }>(`${base}/jobs`)])
    setAgents(a.agents)
    setJobs(j.jobs)
  }, [base])

  useEffect(() => {
    let disposed = false
    let timer: ReturnType<typeof setTimeout> | undefined
    const loop = async () => {
      if (!disposed && document.visibilityState === 'visible') {
        try { await refresh() } catch (e) { if (!disposed) setError(e instanceof Error ? e.message : String(e)) }
      }
      if (!disposed) timer = setTimeout(loop, open ? INTERVAL_OPEN : IDLE_INTERVAL)
    }
    void loop()
    return () => { disposed = true; if (timer) clearTimeout(timer) }
  }, [open, refresh])

  // 浮层开着时点外面 / 按 Esc 收起，别让它一直挂在会话头上。
  useEffect(() => {
    if (!open) return
    const onPointerDown = (event: PointerEvent) => {
      const node = event.target
      if (!(node instanceof Node)) return
      if (rootRef.current?.contains(node) === true) return
      setOpen(false)
    }
    const onKeyDown = (event: globalThis.KeyboardEvent) => {
      if (event.key !== 'Escape') return
      // 发消息表单开着时,Esc 归表单自己处理(收起表单而不是连带关掉面板)。
      // 这里跑在捕获阶段,拦不住表单的 stopPropagation,所以显式让位。
      if (target) return
      event.stopPropagation()
      setOpen(false)
    }
    document.addEventListener('pointerdown', onPointerDown)
    document.addEventListener('keydown', onKeyDown, true)
    return () => {
      document.removeEventListener('pointerdown', onPointerDown)
      document.removeEventListener('keydown', onKeyDown, true)
    }
  }, [open, target])

  const run = async (key: PendingKey, operation: () => Promise<void>) => {
    setPendingFlag(key, true)
    setError('')
    try {
      await operation()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setPendingFlag(key, false)
    }
  }

  const sendMessage = async () => {
    const body = draft.trim()
    if (!body || !target) return
    const key = pendingKey('agent', target, 'message')
    await run(key, async () => {
      await request(`${base}/agents/${target}/message`, { message: body })
      setDrafts((current) => ({ ...current, [target]: '' }))
      setTarget('')
    })
  }

  const readOutput = async (job: Job) => {
    const key = pendingKey('job', job.id, 'output')
    setOutputTarget(job.id)
    setOutputs((current) => ({ ...current, [job.id]: { phase: 'loading' } }))
    await run(key, async () => {
      try {
        const value = await request<{ output: string }>(`${base}/jobs/${job.id}/output`)
        setOutputs((current) => ({ ...current, [job.id]: { phase: 'ready', text: value.output } }))
      } catch (e) {
        const message = e instanceof Error ? e.message : String(e)
        setOutputs((current) => ({ ...current, [job.id]: { phase: 'error', message } }))
      }
    })
  }

  if (activeAgents === 0 && activeJobs === 0) return null

  const tabs = [
    ...(activeAgents > 0 ? (['agents'] as const) : []),
    ...(activeJobs > 0 ? (['jobs'] as const) : []),
  ]
  const detailAgent = agents.find((agent) => agent.id === detail)
  const output = outputTarget ? outputs[outputTarget] : undefined
  const outputJob = jobs.find((job) => job.id === outputTarget)

  const onTabKeyDown = (event: KeyboardEvent<HTMLButtonElement>, index: number) => {
    const keys = ['ArrowLeft', 'ArrowRight', 'Home', 'End']
    if (!keys.includes(event.key) || tabs.length === 0) return
    event.preventDefault()
    const next = event.key === 'Home' ? 0
      : event.key === 'End' ? tabs.length - 1
        : event.key === 'ArrowRight' ? (index + 1) % tabs.length
          : (index - 1 + tabs.length) % tabs.length
    setTab(tabs[next])
  }

  return <section className={`runtime-panel${open ? ' open' : ''}`} ref={rootRef} data-testid="runtime-panel">
    <button type="button" className="runtime-toggle" aria-expanded={open} aria-haspopup="dialog" onClick={() => setOpen(!open)} title={t('runtimeTitle')}>
      <span className="runtime-toggle-icon" aria-hidden="true">✦</span>
      {(activeAgents > 0 || activeJobs > 0) && <span className="runtime-activity-dot" aria-hidden="true" />}
      {(activeAgents > 0 || activeJobs > 0) && <span className="runtime-toggle-label">{activeAgents + activeJobs} {t('runtimeActive')}</span>}
    </button>
    {open && <div className="runtime-content" role="dialog" aria-label={t('runtimeTitle')} data-testid="runtime-content">
      <div className="runtime-tabs" role="tablist" aria-label={t('runtimeTitle')}>
        {tabs.map((key, index) => <button
          type="button"
          key={key}
          role="tab"
          id={`runtime-tab-${key}`}
          aria-controls={`runtime-tabpanel-${key}`}
          aria-selected={tab === key}
          tabIndex={tab === key ? 0 : -1}
          onKeyDown={(event) => onTabKeyDown(event, index)}
          onClick={() => setTab(key)}
        >{t(key === 'agents' ? 'runtimeAgents' : 'runtimeJobs')}</button>)}
      </div>
      {error && <p role="alert" className="runtime-error">{error}</p>}
      {tab === 'agents' && <div className="runtime-rows" role="tabpanel" id="runtime-tabpanel-agents" aria-labelledby="runtime-tab-agents" data-testid="runtime-agents-panel">
        {!agents.length && <p className="runtime-empty">{t('runtimeNoAgents')}</p>}
        {agents.map((agent) => {
          const waiting = waitingLabel(agent)
          const interruptKey = pendingKey('agent', agent.id, 'interrupt')
          const interrupting = isPending(interruptKey)
          const label = agent.name || agent.label
          return <div className="runtime-row" key={agent.id} data-agent={agent.id} data-tone={tone(agent.status)} data-testid={`runtime-agent-${agent.id}`}>
            <div className="runtime-row-main">
              <button type="button" className="runtime-row-name" title={label} onClick={() => setActiveId(agent.id, null)}>{label}</button>
              <small className="runtime-row-status">
                <span className="runtime-status" data-tone={tone(agent.status)}>
                  <i className="runtime-status-dot" aria-hidden="true" />
                  {status(agent.status)}
                </span>
                <span className="runtime-row-model">{agent.selection.provider}/{agent.selection.model} · {t('runtimeDepth')} {agent.depth}</span>
              </small>
              <small className="runtime-meta" data-profile={agent.profile?.qualifiedId ?? ''} title={profileLabel(agent)}>
                {profileLabel(agent)} · {t('subagentsToolsCount').replace('{n}', String(agent.toolCount ?? 0))} · {ceilingLabel(agent)}
                {agent.instructionScope === 'project-only' && ` · ${t('runtimeProjectOnly')}`}
              </small>
              {waiting && <small className="runtime-waiting" role="status">⏳ {waiting}</small>}
              {agent.result && agent.status === 'settled' && <small className="runtime-result">{t('runtimeResult')}：{agent.result}</small>}
              {agent.migration && <small className="runtime-result" role="status" data-migration={agent.migration.projection ?? ''}>{agent.migration.diagnostic}</small>}
            </div>
            <div className="runtime-row-actions">
              <button type="button" className="runtime-action" aria-expanded={detail === agent.id} onClick={() => setDetail(detail === agent.id ? '' : agent.id)}>{t('runtimeSnapshot')}</button>
              {agent.parentId === id && <button type="button" className="runtime-action" aria-expanded={target === agent.id} onClick={() => setTarget(target === agent.id ? '' : agent.id)}>{t('runtimeMessage')}</button>}
              {agent.status === 'running' && <button
                type="button"
                className="runtime-action runtime-action-danger"
                disabled={interrupting}
                aria-busy={interrupting}
                data-pending={interrupting}
                onClick={() => void run(interruptKey, async () => { await request(`${base}/agents/${agent.id}/interrupt`, {}) })}
              >{interrupting ? <><i className="runtime-spinner" aria-hidden="true" />{t('runtimeLoading')}</> : t('runtimeInterrupt')}</button>}
            </div>
          </div>
        })}
        {detailAgent && <div className="runtime-snapshot" data-testid="runtime-snapshot">
          <div><strong>{t('runtimeSnapshot')}</strong><button type="button" className="runtime-action" onClick={() => setDetail('')}>{t('runtimeClose')}</button></div>
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
          <button type="button" className="runtime-action" onClick={() => setActiveId(detailAgent.id, null)}>{t('runtimeOpenChild')}</button>
        </div>}
        {target && <form
          className="runtime-message"
          data-testid="runtime-message-form"
          data-busy={messaging}
          onSubmit={(event) => { event.preventDefault(); void sendMessage() }}
          onKeyDown={(event) => {
            // Enter 发送 / Shift+Enter 换行；Esc 只收起这条表单，不连带关掉面板。
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault()
              void sendMessage()
            } else if (event.key === 'Escape') {
              event.preventDefault()
              event.stopPropagation()
              setTarget('')
            }
          }}
        >
          <label htmlFor={`runtime-message-input-${target}`}>{agents.find((agent) => agent.id === target)?.label}</label>
          <textarea
            id={`runtime-message-input-${target}`}
            autoFocus
            value={draft}
            placeholder={t('runtimeMessageHint')}
            onChange={(event) => setDrafts((current) => ({ ...current, [target]: event.target.value }))}
          />
          <small className="runtime-message-hint">{t('runtimeMessageKeys')}</small>
          <div className="runtime-message-actions">
            <button type="submit" className="runtime-action runtime-action-primary" disabled={messaging || !draft.trim()} aria-busy={messaging} data-pending={messaging}>
              {messaging ? <><i className="runtime-spinner" aria-hidden="true" />{t('runtimeSending')}</> : t('runtimeSend')}
            </button>
            <button type="button" className="runtime-action" onClick={() => setTarget('')}>{t('runtimeClose')}</button>
          </div>
        </form>}
      </div>}
      {tab === 'jobs' && <div className="runtime-rows" role="tabpanel" id="runtime-tabpanel-jobs" aria-labelledby="runtime-tab-jobs" data-testid="runtime-jobs-panel">
        {!jobs.length && <p className="runtime-empty">{t('runtimeNoJobs')}</p>}
        {jobs.map((job) => {
          const outputKey = pendingKey('job', job.id, 'output')
          const killKey = pendingKey('job', job.id, 'kill')
          const reading = isPending(outputKey)
          const killing = isPending(killKey)
          return <div className="runtime-row" key={job.id} data-job={job.id} data-tone={tone(job.status)} data-testid={`runtime-job-${job.id}`}>
            <div className="runtime-row-main">
              <span className="runtime-row-name" title={job.label}>{job.label}</span>
              <small className="runtime-row-status">
                <span className="runtime-status" data-tone={tone(job.status)}>
                  <i className="runtime-status-dot" aria-hidden="true" />
                  {status(job.status)}{job.exitCode != null && ` · ${t('runtimeExit')} ${job.exitCode}`}
                </span>
              </small>
            </div>
            <div className="runtime-row-actions">
              <button
                type="button"
                className="runtime-action"
                disabled={reading}
                aria-busy={reading}
                data-pending={reading}
                onClick={() => void readOutput(job)}
              >{reading ? <><i className="runtime-spinner" aria-hidden="true" />{t('runtimeLoading')}</> : t('runtimeOutput')}</button>
              {!job.finishedAt && <button
                type="button"
                className="runtime-action runtime-action-danger"
                disabled={killing || job.status === 'stopping'}
                aria-busy={killing}
                data-pending={killing}
                onClick={() => void run(killKey, async () => { await request(`${base}/jobs/${job.id}/kill`, {}) })}
              >{killing ? <><i className="runtime-spinner" aria-hidden="true" />{t('runtimeLoading')}</> : t('runtimeInterrupt')}</button>}
            </div>
          </div>
        })}
      </div>}

      {outputJob && <div className="runtime-preview" data-testid="runtime-preview" data-phase={output?.phase ?? 'idle'}>
        <div className="runtime-preview-head">
          <strong>{outputJob.label}</strong>
          <button type="button" className="runtime-action" onClick={() => setOutputTarget('')}>{t('runtimeClose')}</button>
        </div>
        {output?.phase === 'loading' && <p className="runtime-preview-state" role="status"><i className="runtime-spinner" aria-hidden="true" />{t('runtimeLoading')}</p>}
        {output?.phase === 'error' && <p className="runtime-preview-state" role="alert" data-error="true">{output.message}</p>}
        {output?.phase === 'ready' && (output.text
          ? <pre>{output.text}</pre>
          : <p className="runtime-preview-state">{t('runtimeNoOutput')}</p>)}
      </div>}
    </div>}
  </section>
}