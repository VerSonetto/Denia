/**
 * 连通性测试子页:对已保存网关发一条真实流式请求。
 * 阶段进度(连接→等待→流式→完成)+ TTFT + 总耗时 + token 用量;
 * 失败展示错误码 / HTTP 状态 / 网关返回(再脱敏),可重试并记住上次失败。
 * 计时/进度是独立叶子组件,自己 tick,不牵动父级渲染。
 */

import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { resolveSessionReasoningEffort } from '../../modelCatalog'
import { reasoningEffortLabel } from '../../reasoningEffort'
import { Button, ChipRadio } from './atoms/form'
import { formatMs, formatClock, sanitizeErrorText } from './catalogUtils'
import { DropdownField } from '../ui/controls'
import { IconActivity } from '../icons'
import type { ProbeFailureView, ProbeMonitor, ProviderRoute } from './types'
import type { ModelCatalog } from '../../types'
import styles from './ChatProbe.module.css'

/** 探测语:最小回复,压低成本;输出上限 256 防长推理失控。 */
const PROBE_PROMPT = '连通性测试:请只回复 "ok"'
const PROBE_MAX_TOKENS = 256

/** 连接→6%,等待→最高 24%,流式按字符量渐近 90%,完成 100%。 */
function progressFraction(monitor: ProbeMonitor): number {
  const elapsed = performance.now() - monitor.startedAt
  if (monitor.finish) return 100
  if (monitor.chars > 0) {
    return Math.min(90, 24 + 66 * (1 - Math.exp(-monitor.chars / 300)))
  }
  if (monitor.connected) return Math.min(24, 6 + (elapsed / 4000) * 18)
  return Math.min(6, 1 + elapsed / 1500)
}

function stageKey(monitor: ProbeMonitor): 'connect' | 'wait' | 'stream' | 'done' {
  if (monitor.finish) return 'done'
  if (monitor.chars > 0) return 'stream'
  if (monitor.connected) return 'wait'
  return 'connect'
}

/** 叶子:运行中的阶段进度 + 计时;自 tick,props 只有稳定的 ref。 */
const ProbeMonitorView = memo(function ProbeMonitorView({
  monitorRef,
}: {
  monitorRef: { current: ProbeMonitor | null }
}) {
  const [, setTick] = useState(0)
  useEffect(() => {
    const timer = window.setInterval(() => setTick((tick) => tick + 1), 100)
    return () => window.clearInterval(timer)
  }, [])
  const monitor = monitorRef.current
  if (!monitor) return null
  const stage = stageKey(monitor)
  const fraction = progressFraction(monitor)
  const elapsed = performance.now() - monitor.startedAt
  return (
    <div className={styles.monitor}>
      <div className={styles.monitorHead}>
        <span className={styles.stage}>
          {t(
            stage === 'connect'
              ? 'llm.probe.stage.connect'
              : stage === 'wait'
                ? 'llm.probe.stage.wait'
                : 'llm.probe.stage.stream',
          )}
        </span>
        <span className={styles.elapsed}>{formatMs(elapsed)}</span>
      </div>
      <div
        className={styles.bar}
        role="progressbar"
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={Math.round(fraction)}
      >
        <span className={styles.barFill} style={{ width: `${fraction}%` }} />
      </div>
    </div>
  )
})

export function ChatProbe({
  routes,
  catalog,
  initialProvider,
}: {
  routes: ProviderRoute[]
  catalog: ModelCatalog | null
  initialProvider?: string
}) {
  const [providerId, setProviderId] = useState(initialProvider ?? routes[0]?.route ?? '')
  const [modelId, setModelId] = useState('')
  const [effort, setEffort] = useState<string | undefined>(undefined)
  const [phase, setPhase] = useState<'idle' | 'running' | 'success' | 'error'>('idle')
  const [result, setResult] = useState<{
    ttftMs: number
    totalMs: number
    usage: { input: number; output: number } | null
    finish: string
  } | null>(null)
  const [failure, setFailure] = useState<ProbeFailureView | null>(null)
  const [lastFailed, setLastFailed] = useState<ProbeFailureView | null>(null)
  const monitorRef = useRef<ProbeMonitor | null>(null)
  const abortRef = useRef<AbortController | null>(null)

  // 切换初始提供方(从供应商卡片跳转过来时)。
  useEffect(() => {
    if (initialProvider) setProviderId(initialProvider)
  }, [initialProvider])

  const profile = useMemo(
    () => routes.find((route) => route.route === providerId)?.profile ?? null,
    [routes, providerId],
  )

  /* 模型清单:优先运行时目录,回退配置的模型列表。 */
  const models = useMemo(() => {
    const group = catalog?.groups.find((entry) => entry.id === providerId)
    if (group && group.models.length > 0) return group.models
    return (profile?.models ?? []).map((entry) => ({
      id: entry.id,
      name: entry.name ?? entry.id,
      description: entry.description,
      contextWindow: entry.contextWindow,
      inputModalities: entry.inputModalities,
      thinkingSupported: entry.thinkingSupported,
      reasoning: undefined,
    }))
  }, [catalog, providerId, profile])

  const efforts = useMemo(() => {
    const model = models.find((entry) => entry.id === modelId)
    return model?.reasoning?.efforts ?? []
  }, [models, modelId])

  /* 模型缺省/失效时回落第一个;effort 回落该模型默认档。 */
  useEffect(() => {
    if (models.length === 0) {
      if (modelId !== '') setModelId('')
      return
    }
    if (!models.some((entry) => entry.id === modelId)) {
      setModelId(models[0].id)
      return
    }
    const model = models.find((entry) => entry.id === modelId)
    const nextEfforts = model?.reasoning?.efforts ?? []
    setEffort((prev) => resolveSessionReasoningEffort(nextEfforts, prev))
  }, [models, modelId])

  useEffect(
    () => () => {
      abortRef.current?.abort()
    },
    [],
  )

  const run = useCallback(() => {
    if (!providerId || !modelId) return
    abortRef.current?.abort()
    const controller = new AbortController()
    abortRef.current = controller
    monitorRef.current = {
      startedAt: performance.now(),
      connected: false,
      ttftMs: null,
      chars: 0,
      usage: null,
      finish: null,
    }
    if (failure) setLastFailed(failure)
    setPhase('running')
    setResult(null)
    setFailure(null)

    void api.probeChatStream(
      { provider: providerId, model: modelId, reasoningEffort: effort, prompt: PROBE_PROMPT, maxTokens: PROBE_MAX_TOKENS },
      {
        onConnected: () => {
          if (monitorRef.current) monitorRef.current.connected = true
        },
        onFirstToken: () => {
          const monitor = monitorRef.current
          if (monitor && monitor.ttftMs === null) {
            monitor.ttftMs = performance.now() - monitor.startedAt
          }
        },
        onDelta: (text) => {
          if (monitorRef.current) monitorRef.current.chars += text.length
        },
        onUsage: (usage) => {
          if (monitorRef.current) {
            monitorRef.current.usage = { input: usage.inputTokens, output: usage.outputTokens }
          }
        },
        onSuccess: (reason) => {
          const monitor = monitorRef.current
          if (!monitor) return
          monitor.finish = reason
          setResult({
            ttftMs: monitor.ttftMs ?? 0,
            totalMs: performance.now() - monitor.startedAt,
            usage: monitor.usage,
            finish: reason,
          })
          setPhase('success')
          monitorRef.current = null
        },
        onFailure: (raw) => {
          // 用户主动停止:静默回 idle,不当失败渲染。
          if (raw.code === 'probe/aborted' && controller.signal.aborted) {
            monitorRef.current = null
            setPhase((prev) => (prev === 'running' ? 'idle' : prev))
            return
          }
          setFailure({
            code: raw.code,
            status: raw.status,
            message: sanitizeErrorText(raw.message),
            requestId: raw.requestId,
            retryAfterMs: raw.retryAfterMs,
            at: Date.now(),
          })
          setPhase('error')
          monitorRef.current = null
        },
      },
      controller.signal,
    )
  }, [providerId, modelId, effort, failure])

  const stop = useCallback(() => {
    abortRef.current?.abort()
    monitorRef.current = null
    setPhase((prev) => (prev === 'running' ? 'idle' : prev))
  }, [])

  if (routes.length === 0) {
    return (
      <div className={styles.root}>
        <div className={styles.empty}>
          <IconActivity size={28} />
          <p className={styles.emptyText}>{t('llm.probe.noRoutes')}</p>
        </div>
      </div>
    )
  }

  const providerOptions = routes.map((route) => ({
    id: route.route,
    label: route.profile.displayName || route.route,
  }))
  const modelOptions = models.map((entry) => ({ id: entry.id, label: entry.name ?? entry.id }))
  const finishLabel = (reason: string) =>
    reason === 'stop'
      ? t('llm.probe.finishStop')
      : reason === 'tool-calls'
        ? t('llm.probe.finishToolCalls')
        : reason

  return (
    <div className={styles.root}>
      <p className={styles.intro}>{t('llm.probe.intro')}</p>

      <div className={styles.controls}>
        <div className={styles.controlField}>
          <span className={styles.controlLabel}>{t('llm.probe.providerLabel')}</span>
          <DropdownField label="" value={providerId} options={providerOptions} onChange={setProviderId} />
        </div>
        <div className={styles.controlField}>
          <span className={styles.controlLabel}>{t('llm.probe.modelLabel')}</span>
          <DropdownField
            label=""
            value={modelId}
            options={modelOptions}
            onChange={setModelId}
            disabled={modelOptions.length === 0}
            placeholder={t('noModels')}
          />
        </div>
        <div className={styles.runBox}>
          {phase === 'running' ? (
            <Button variant="ghost" onClick={stop}>
              {t('stop')}
            </Button>
          ) : (
            <Button variant="primary" disabled={!modelId} onClick={run}>
              <IconActivity size={14} />
              {phase === 'error' || phase === 'success' ? t('retry') : t('llm.probe.start')}
            </Button>
          )}
        </div>
      </div>

      {efforts.length > 0 && (
        <div className={styles.effortRow}>
          <span className={styles.controlLabel}>{t('reasoningLabel')}</span>
          <ChipRadio
            value={effort ?? ''}
            ariaLabel={t('reasoningLabel')}
            options={efforts.map((entry) => ({
              id: entry.id,
              label: reasoningEffortLabel(entry.id),
            }))}
            onChange={setEffort}
          />
        </div>
      )}

      {phase === 'running' && <ProbeMonitorView monitorRef={monitorRef} />}

      {phase === 'success' && result && (
        <div className={styles.result}>
          <div className={styles.resultTitle}>{t('llm.probe.successTitle')}</div>
          <dl className={styles.metrics}>
            <div className={styles.metric}>
              <dt>{t('llm.probe.ttft')}</dt>
              <dd>{formatMs(result.ttftMs)}</dd>
            </div>
            <div className={styles.metric}>
              <dt>{t('llm.probe.total')}</dt>
              <dd>{formatMs(result.totalMs)}</dd>
            </div>
            <div className={styles.metric}>
              <dt>{t('inputTokens')}</dt>
              <dd>{result.usage ? result.usage.input.toLocaleString('en-US') : '—'}</dd>
            </div>
            <div className={styles.metric}>
              <dt>{t('outputTokens')}</dt>
              <dd>{result.usage ? result.usage.output.toLocaleString('en-US') : '—'}</dd>
            </div>
            <div className={styles.metric}>
              <dt>{t('llm.probe.finishReason')}</dt>
              <dd>{finishLabel(result.finish)}</dd>
            </div>
          </dl>
        </div>
      )}

      {phase === 'error' && failure && (
        <div className={styles.failure} role="alert">
          <div className={styles.resultTitle}>{t('llm.probe.errorTitle')}</div>
          <dl className={styles.errorList}>
            <div className={styles.errorLine}>
              <dt>{t('llm.probe.errCode')}</dt>
              <dd className={styles.mono}>{failure.code}</dd>
            </div>
            {failure.status !== undefined && (
              <div className={styles.errorLine}>
                <dt>{t('llm.probe.errStatus')}</dt>
                <dd className={styles.mono}>{failure.status}</dd>
              </div>
            )}
            {failure.retryAfterMs !== undefined && (
              <div className={styles.errorLine}>
                <dt></dt>
                <dd>{t('llm.probe.retryAfter', { s: Math.ceil(failure.retryAfterMs / 1000) })}</dd>
              </div>
            )}
            {failure.requestId && (
              <div className={styles.errorLine}>
                <dt>{t('llm.probe.requestId')}</dt>
                <dd className={styles.mono}>{failure.requestId}</dd>
              </div>
            )}
            <div className={styles.errorLine}>
              <dt>{t('llm.probe.errBody')}</dt>
              <dd className={styles.errorBody}>{failure.message}</dd>
            </div>
          </dl>
        </div>
      )}

      {phase !== 'error' && lastFailed && (
        <p className={styles.lastFailed}>
          {t('llm.probe.lastFailed', { time: formatClock(lastFailed.at) })} ·{' '}
          {sanitizeErrorText(lastFailed.message)}
        </p>
      )}

      {phase === 'idle' && !lastFailed && (
        <p className={styles.promptNote}>
          {t('llm.probe.promptLabel')}: <span className={styles.mono}>{PROBE_PROMPT}</span>
        </p>
      )}
    </div>
  )
}
