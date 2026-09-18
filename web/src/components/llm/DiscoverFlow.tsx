/**
 * 模型发现流程:POST /api/llm/discover → 骨架屏 → 命中池逐个导入或一键全导。
 * 失败可重试,并保留上次失败时间与原因(fail loud,不静默吞错)。
 */

import { useRef, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { IconSearch, IconCheck } from '../icons'
import { Button, Skeleton } from './atoms/form'
import { formatClock, sanitizeErrorText } from './catalogUtils'
import type { DiscoveredModel, WireProtocol } from '../../types'
import styles from './DiscoverFlow.module.css'

export interface DiscoverParams {
  baseURL: string
  apiKey?: string
  apiKeyEnv?: string
  protocol: WireProtocol
  /** 一次性附加请求头(值支持 `${REF}` 引用),仅本次探测使用。 */
  headers?: Record<string, string>
}

export function DiscoverFlow({
  readParams,
  isAdded,
  onImport,
  disabled,
}: {
  /** 调用瞬间读取表单参数(避免父级每次键击重渲染本组件)。 */
  readParams: () => DiscoverParams
  isAdded: (id: string) => boolean
  onImport: (models: DiscoveredModel[]) => void
  disabled?: boolean
}) {
  const [phase, setPhase] = useState<'idle' | 'running' | 'done'>('idle')
  const [models, setModels] = useState<DiscoveredModel[]>([])
  const [error, setError] = useState<{ message: string; at: number } | null>(null)
  const abortRef = useRef<AbortController | null>(null)

  const run = async () => {
    const params = readParams()
    if (!params.baseURL.trim()) {
      setError({ message: t('baseURLRequired'), at: Date.now() })
      return
    }
    abortRef.current?.abort()
    const controller = new AbortController()
    abortRef.current = controller
    setPhase('running')
    setError(null)
    try {
      // discoverModels 走 api 层 15s 默认超时;超时错误原样落进 catch 展示给用户重试。
      const found = await api.discoverModels(
        params.baseURL.trim(),
        params.apiKey?.trim() || undefined,
        params.apiKey?.trim() ? undefined : params.apiKeyEnv?.trim() || undefined,
        params.protocol,
        params.headers,
      )
      if (controller.signal.aborted) return
      setModels(found)
      setPhase('done')
    } catch (err) {
      if (controller.signal.aborted) return
      setError({
        message: err instanceof Error ? err.message : String(err),
        at: Date.now(),
      })
      setPhase('idle')
    }
  }

  const unadded = models.filter((model) => !isAdded(model.id))

  return (
    <div className={styles.root}>
      <div className={styles.headRow}>
        <Button small variant="ghost" disabled={disabled || phase === 'running'} onClick={() => void run()}>
          <IconSearch size={13} />
          {error ? t('llm.models.discoverRetry') : t('discoverModels')}
        </Button>
        <span className={styles.hint}>{t('llm.models.discoverHint')}</span>
      </div>

      {phase === 'running' && (
        <div className={styles.pool} aria-label={t('discovering')} aria-busy="true">
          {Array.from({ length: 8 }, (_, index) => (
            <Skeleton key={index} width={`${20 + ((index * 37) % 22)}%`} height={26} />
          ))}
        </div>
      )}

      {phase === 'idle' && error && (
        <div className={styles.errorRow} role="alert">
          <span className={styles.errorText}>
            {t('discoverFailed')}: {sanitizeErrorText(error.message)}
            <span className={styles.errorMeta}> · {t('llm.models.discoverFailedAt', { time: formatClock(error.at) })}</span>
          </span>
        </div>
      )}

      {phase === 'done' && (
        <div className={styles.doneBlock}>
          <div className={styles.doneHead}>
            <span className={styles.count}>{t('discoveredCount', { n: models.length })}</span>
            {unadded.length > 0 && (
              <Button small variant="ghost" disabled={disabled} onClick={() => onImport(unadded)}>
                {t('llm.models.importAll', { n: unadded.length })}
              </Button>
            )}
          </div>
          {models.length === 0 ? (
            <p className={styles.empty}>—</p>
          ) : (
            <div className={styles.pool}>
              {models.map((model) => {
                const added = isAdded(model.id)
                return (
                  <button
                    key={model.id}
                    type="button"
                    className={`${styles.tag}${added ? ` ${styles.tagAdded}` : ''}`}
                    disabled={disabled || added}
                    title={added ? t('modelAlreadyAdded') : t('addDiscoveredModel')}
                    onClick={() => onImport([model])}
                  >
                    {added && <IconCheck size={11} />}
                    <span className={styles.tagId}>{model.id}</span>
                  </button>
                )
              })}
            </div>
          )}
        </div>
      )}
    </div>
  )
}
