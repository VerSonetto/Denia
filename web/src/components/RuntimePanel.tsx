import { useCallback, useEffect, useMemo, useState } from 'react'
import type { KeyboardEvent } from 'react'
import { t } from '../i18n'
import { setActiveId } from '../appStore'
import type { SessionEnvelope } from '../types'
import { getSessionPage } from '../api'
import { toolCallSummary } from '../toolDisplay'
import { AgentAvatar, useAgentAvatar } from './AgentAvatar'
import './RuntimePanel.css'

/** 子代理状态来自服务端运行快照，进展来自子会话事件。 */
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
function waitingLabel(agent: Agent) {
  const approvals = agent.pendingApprovals ?? 0
  const asks = agent.pendingAsks ?? 0
  if (!approvals && !asks) return null
  return [approvals ? `${t('runtimeWaitingApproval')} ${approvals}` : '', asks ? `${t('runtimeWaitingAsk')} ${asks}` : '']
    .filter(Boolean)
    .join(' · ')
}

/** 稳定排序，避免轮询返回顺序变化导致折叠列表抖动。 */
function agentPriority(agent: Agent): number {
  if (agent.waitingInteraction || (agent.pendingApprovals ?? 0) > 0 || (agent.pendingAsks ?? 0) > 0) return 0
  if (agent.status === 'running' || agent.status === 'waiting') return 1
  return 2
}

/** 轮询节奏：展开时盯紧，折叠时放慢 —— 省请求又不至于看着不动。 */
const INTERVAL_OPEN = 1500

/** 只呈现实际产生的动作与正文，不展示推理或完整工具结果。 */
export function agentProgress(events: SessionEnvelope[]): { action: string; progress: string } {
  let action = ''
  let progress = ''
  for (const event of events) {
    if (event.type === 'user-message' && !event.injected) { action = ''; progress = '' }
    if (event.type === 'tool-call') {
      const summary = toolCallSummary(event.name, event.arguments)
      action = [event.name, summary].filter(Boolean).join(' · ')
    }
    if (event.type === 'assistant-message') {
      const text = event.blocks.filter((block) => block.type === 'text').map((block) => block.text).join(' ').trim()
      if (text) progress = text
    }
  }
  return { action: action.slice(0, 180), progress: progress.slice(-400) }
}

function AgentProgress({ id, running }: { id: string; running: boolean }) {
  const [progress, setProgress] = useState<{ action: string; progress: string } | null>(null)
  useEffect(() => {
    const controller = new AbortController()
    let timer: ReturnType<typeof setTimeout> | undefined
    const load = async () => {
      try {
        const page = await getSessionPage(id, { limit: 50 }, controller.signal)
        if (!controller.signal.aborted) setProgress(agentProgress(page.events))
      } catch {
        // 状态和操作仍可用；网络失败不伪造进度，下次轮询重试。
      }
      if (running && !controller.signal.aborted) timer = setTimeout(load, 3000)
    }
    void load()
    return () => { controller.abort(); if (timer) clearTimeout(timer) }
  }, [id, running])
  if (!progress?.action && !progress?.progress) return null
  return <div className="runtime-progress" data-testid="agent-progress">
    {progress.action && <p><span>{t('runtimeLatestAction')}</span><span className="runtime-progress-text" title={progress.action}>{progress.action}</span></p>}
    {progress.progress && <p><span>{t('runtimeLatestProgress')}</span><span className="runtime-progress-text" title={progress.progress}>{progress.progress}</span></p>}
  </div>
}

function RuntimeAvatar({ id, cwd }: { id: string; cwd?: string | null }) {
  const avatar = useAgentAvatar(id, cwd)
  return <AgentAvatar avatar={avatar} size={30} />
}

export function RuntimePanel({ id, cwd, events, pendingCount, onVisibilityChange }: { id: string; cwd?: string | null; events: SessionEnvelope[]; pendingCount: number; onVisibilityChange(visible: boolean): void }) {
  const [tab, setTab] = useState<'agents' | 'jobs'>('agents')
  const [agents, setAgents] = useState<Agent[]>([])
  const [jobs, setJobs] = useState<Job[]>([])
  const [error, setError] = useState('')
  const [pending, setPending] = useState<Record<PendingKey, boolean>>({})
  const [outputs, setOutputs] = useState<Record<string, Output>>({})
  const [outputTarget, setOutputTarget] = useState('')
  const [target, setTarget] = useState('')
  const [drafts, setDrafts] = useState<Record<string, string>>({})
  const [agentsExpanded, setAgentsExpanded] = useState(false)
  // 本轮工具调用及结果中出现的任务 ID 才属于当前轮次。活跃任务跨轮保留；
  // 乐观用户消息到达时立即隐藏上轮结束项，与 Todo 的收起时机一致。
  const currentTurn = useMemo(() => {
    let start = 0
    events.forEach((event, index) => { if (event.type === 'user-message' && !event.injected) start = index })
    return JSON.stringify(events.slice(start).filter((event) => event.type === 'tool-call' || event.type === 'tool-result'))
  }, [events])
  const visibleAgents = agents.filter((agent) => agent.status === 'running' || agent.status === 'waiting' || (!pendingCount && currentTurn.includes(agent.id)))
  const orderedAgents = [...visibleAgents].sort((a, b) => agentPriority(a) - agentPriority(b) || a.id.localeCompare(b.id))
  const displayedAgents = agentsExpanded ? orderedAgents : orderedAgents.slice(0, 2)
  const hiddenAgentCount = orderedAgents.length - 2
  const visibleJobs = jobs.filter((job) => (!job.finishedAt && job.status !== 'killed' && job.status !== 'failed') || (!pendingCount && currentTurn.includes(job.id)))
  const activeAgents = visibleAgents.length
  const activeJobs = visibleJobs.length
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
      if (!disposed) timer = setTimeout(loop, INTERVAL_OPEN)
    }
    void loop()
    return () => { disposed = true; if (timer) clearTimeout(timer) }
  }, [refresh])

  useEffect(() => {
    onVisibilityChange(activeAgents + activeJobs > 0)
    return () => onVisibilityChange(false)
  }, [activeAgents, activeJobs, onVisibilityChange])

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

  return <section className="runtime-panel open" aria-label={t('runtimeTitle')} data-testid="runtime-panel">
    <div className="runtime-card-heading"><strong>{t('runtimeTitle')}</strong><span className="runtime-card-count">{activeAgents + activeJobs}</span></div>
    <div className="runtime-content" data-testid="runtime-content">
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
        ><span>{t(key === 'agents' ? 'runtimeAgents' : 'runtimeJobs')}</span><span className="runtime-tab-count">{key === 'agents' ? activeAgents : activeJobs}</span></button>)}
      </div>
      {error && <p role="alert" className="runtime-error">{error}</p>}
      {tab === 'agents' && <div className="runtime-rows" role="tabpanel" id="runtime-tabpanel-agents" aria-labelledby="runtime-tab-agents" data-testid="runtime-agents-panel">
        {!agents.length && <p className="runtime-empty">{t('runtimeNoAgents')}</p>}
        {displayedAgents.map((agent) => {
          const waiting = waitingLabel(agent)
          const interruptKey = pendingKey('agent', agent.id, 'interrupt')
          const interrupting = isPending(interruptKey)
          const label = agent.name || agent.label
          return <div className="runtime-row" key={agent.id} data-agent={agent.id} data-tone={tone(agent.status)} data-testid={`runtime-agent-${agent.id}`}>
            <div className="runtime-row-main">
              <div className="runtime-row-heading">
              <RuntimeAvatar id={agent.profile?.qualifiedId || agent.id} cwd={cwd} />
              <button type="button" className="runtime-row-name" title={label} onClick={() => setActiveId(agent.id, null)}>{label}</button>
              <small className="runtime-row-status">
                <span className="runtime-status" data-tone={tone(agent.status)}>
                  <i className="runtime-status-dot" aria-hidden="true" />
                  {status(agent.status)}
                </span>
              </small>
              </div>
              {waiting && <small className="runtime-waiting" role="status">{waiting}</small>}
              <AgentProgress id={agent.id} running={agent.status === 'running' || agent.status === 'waiting'} />
              {agent.result && agent.status === 'settled' && <small className="runtime-result">{t('runtimeResult')}：{agent.result}</small>}
              {agent.migration && <small className="runtime-result" role="status" data-migration={agent.migration.projection ?? ''}>{agent.migration.diagnostic}</small>}
            </div>
            <div className="runtime-row-actions">
              <button type="button" className="runtime-action" onClick={() => setActiveId(agent.id, null)}>{t('runtimeOpenChild')}</button>
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
        {hiddenAgentCount > 0 && <button
          type="button"
          className="runtime-show-agents"
          aria-expanded={agentsExpanded}
          onClick={() => {
            if (agentsExpanded && target && !orderedAgents.slice(0, 2).some((agent) => agent.id === target)) setTarget('')
            setAgentsExpanded((expanded) => !expanded)
          }}
        >
          <span>{agentsExpanded ? t('runtimeShowFewer') : t('runtimeShowMoreAgents', { n: hiddenAgentCount })}</span>
          <svg className={agentsExpanded ? 'expanded' : undefined} width="12" height="12" viewBox="0 0 16 16" fill="none" aria-hidden="true"><path d="m4 6 4 4 4-4" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" /></svg>
        </button>}
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
        {visibleJobs.map((job) => {
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
    </div>
  </section>
}
