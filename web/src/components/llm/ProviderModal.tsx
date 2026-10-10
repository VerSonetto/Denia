/**
 * 提供方编辑弹窗(居中模态):全自动保存,**没有保存按钮**。
 * - 表单快照随时按 payload 比对,变化后停笔约 0.8s 自动落盘;没变化不重复写。
 * - 新增的路由 ID 以失焦为「开工」信号:在此之前不落盘,避免半途碎片建出孤儿网关;
 *   建出来之后 ID 锁死(改名 = 新网关,不做原地重命名)。
 * - 密钥值是显式提交点(失焦才写凭据存储),不参与 debounce。
 * - 校验就地即时跑;不合法的字段保持上一次的已存值,不把坏 URL 推给在跑的路由。
 * Esc / 遮罩双击 / 「完成」都先 flush 在途改动再关窗。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import {
  WIRE_PROTOCOLS,
  entryFromWire,
  entryToWire,
  type CatalogModelEntry,
} from '../../modelCatalog'
import { Badge, Button, ChipRadio, Field, NumberInput, TextInput } from './atoms/form'
import { AutosaveStatus } from './AutosaveStatus'
import { ModelRowsEditor } from './ModelRowsEditor'
import type { ProviderAutosave } from './useProviderAutosave'
import type { Notify } from '../../App'
import type { CredentialInfo, OpenAiProfile, WireProtocol } from '../../types'
import styles from './ProviderModal.module.css'

/** 密钥引用名:由路由 ID 派生,必须是合法环境变量名(数字开头补 `_`)。 */
function deriveKeyRef(routeId: string): string {
  const base = routeId.toUpperCase().replace(/[^A-Z0-9]+/g, '_')
  const head = base.length > 0 && base[0] >= '0' && base[0] <= '9' ? '_' : ''
  return head + base + '_API_KEY'
}

/** HTTP 请求头名称的 token 语法(与后端 reqwest 校验一致)。 */
const HEADER_NAME_PATTERN = /^[!#%&'*+\-.^_`|~0-9A-Za-z$]+$/

const BASE_URL_PATTERN = /^https?:\/\//i

/** 停笔多久落盘:够跨过一次中文补选的停顿,又不至于让人怀疑"存了没"。 */
const AUTOSAVE_DEBOUNCE_MS = 800

interface FormErrors {
  routeId?: string
  baseURL?: string
  ctx?: string
  maxTokens?: string
  models?: string
  headers?: string
}

interface HeaderRow {
  name: string
  value: string
}

/** 一次落盘的完整内容:key = providers 的键,value = 该键的 profile。 */
interface Payload {
  id: string
  profile: Record<string, unknown>
  modelsRaw: string
}

function headerErrorOf(rows: HeaderRow[]): string | undefined {
  for (const row of rows) {
    const name = row.name.trim()
    if (!name) continue
    if (!HEADER_NAME_PATTERN.test(name) || !row.value.trim()) {
      return t('llm.field.headersInvalid')
    }
  }
  return undefined
}

function modelsErrorOf(entries: CatalogModelEntry[]): string | undefined {
  const seen = new Set<string>()
  for (const entry of entries) {
    const modelId = entry.id.trim()
    // 只拦非空与重复;不校验字符格式(网关模型 id 常含 `/`、`:` 等)。
    if (!modelId) return t('modelsInvalid')
    if (seen.has(modelId)) return t('llm.models.duplicate', { id: modelId })
    seen.add(modelId)
  }
  return undefined
}

export function ProviderModal({
  route,
  initial,
  credentials,
  notify,
  autosave,
  onChanged,
  onClose,
}: {
  /** 编辑的路由 id;null = 新增两步流程。 */
  route: string | null
  initial: OpenAiProfile | null
  credentials: Record<string, CredentialInfo>
  notify: Notify
  autosave: ProviderAutosave
  /** 凭据等旁路改动后重拉设置(刷新密钥徽标)。 */
  onChanged: () => void
  onClose: () => void
}) {
  /** 已落盘的路由 id;null = 还没建出来(新增且尚未提交过)。 */
  const [createdId, setCreatedId] = useState<string | null>(route)
  const isFresh = createdId === null
  const [step, setStep] = useState<1 | 2>(1)
  const [routeId, setRouteId] = useState(route ?? '')
  const [displayName, setDisplayName] = useState(initial?.displayName ?? '')
  const [baseURL, setBaseURL] = useState(initial?.baseURL ?? '')
  const [protocol, setProtocol] = useState<WireProtocol>(initial?.protocol ?? 'openai-completions')
  const [ctxWindow, setCtxWindow] = useState<number | undefined>(initial?.defaultContextWindow)
  const [maxTokens, setMaxTokens] = useState<number | undefined>(initial?.defaultMaxTokens)
  const [models, setModels] = useState<CatalogModelEntry[]>(() =>
    (initial?.models ?? []).map(
      (entry) => entryFromWire(entry as unknown as Record<string, unknown>),
    ),
  )
  const [keyValue, setKeyValue] = useState('')
  const [headers, setHeaders] = useState<HeaderRow[]>(() =>
    Object.entries(initial?.headers ?? {}).map(([name, value]) => ({ name, value })),
  )
  const [errors, setErrors] = useState<FormErrors>({})
  /** 新增时:路由 ID 失焦过 = 允许建网关。 */
  const [routeIdTouched, setRouteIdTouched] = useState(route !== null)
  const [closing, setClosing] = useState(false)
  const keySectionRef = useRef<HTMLDivElement | null>(null)
  const timerRef = useRef<number | null>(null)
  /** 最近一次已落盘/已在途的 payload 指纹,用来跳过「没变化」的写入。 */
  const writtenRef = useRef<string | null>(null)
  const inFlightRef = useRef<Promise<boolean> | null>(null)
  // autosave 对象每次渲染都是新的;解构出身份稳定的回调再进依赖数组。
  const { write, has } = autosave

  // 编辑沿用已存引用(不丢已配密钥);新建在网关落地那一刻按 id 派生。
  const effectiveKeyRef =
    initial?.apiKeyEnv ?? (createdId ? deriveKeyRef(createdId) : '')
  const credentialForRef = credentials[effectiveKeyRef]
  const keyConfigured = keyValue.trim().length > 0 || credentialForRef?.configured === true

  /* ---- 表单 → payload(纯派生) ---- */

  const payload = useMemo<Payload | null>(() => {
    const id = (createdId ?? routeId).trim()
    const url = baseURL.trim()
    if (!id) return null
    if (!BASE_URL_PATTERN.test(url)) return null
    const profile: Record<string, unknown> = {
      baseURL: url,
      displayName: displayName.trim() || undefined,
      apiKeyEnv: effectiveKeyRef || undefined,
      protocol,
      models: models.map((entry) => entryToWire(entry)),
    }
    if (ctxWindow && ctxWindow > 0) profile.defaultContextWindow = ctxWindow
    if (maxTokens && maxTokens > 0) profile.defaultMaxTokens = maxTokens
    const headerEntries = headers
      .map((row) => [row.name.trim(), row.value] as const)
      .filter(([name]) => name !== '')
    if (headerEntries.length > 0) profile.headers = Object.fromEntries(headerEntries)
    return { id, profile, modelsRaw: JSON.stringify(profile.models) }
  }, [
    createdId,
    routeId,
    baseURL,
    displayName,
    effectiveKeyRef,
    protocol,
    models,
    ctxWindow,
    maxTokens,
    headers,
  ])

  const fingerprint = payload ? `${payload.id}\u0000${JSON.stringify(payload.profile)}` : null

  /** payload 之外还缺什么:决定是「静默不写」还是「写了但报错」。 */
  const blocking = useMemo(() => {
    const next: FormErrors = {}
    const id = routeId.trim()
    if (!id) next.routeId = t('routeIdRequired')
    else if (isFresh && autosave.has(id)) next.routeId = t('llm.error.routeIdExists', { id })
    const url = baseURL.trim()
    if (!url) next.baseURL = t('baseURLRequired')
    else if (!BASE_URL_PATTERN.test(url)) next.baseURL = t('llm.error.baseURLFormat')
    const headersErr = headerErrorOf(headers)
    if (headersErr) next.headers = headersErr
    if (ctxWindow !== undefined && (!Number.isFinite(ctxWindow) || ctxWindow <= 0)) {
      next.ctx = t('llm.error.numberPositive')
    }
    if (maxTokens !== undefined && (!Number.isFinite(maxTokens) || maxTokens <= 0)) {
      next.maxTokens = t('llm.error.numberPositive')
    }
    const modelsErr = modelsErrorOf(models)
    if (modelsErr) next.models = modelsErr
    return next
  }, [routeId, baseURL, headers, ctxWindow, maxTokens, models, isFresh, has])

  const hasBlocking = Object.keys(blocking).length > 0

  useEffect(() => {
    setErrors((prev) => {
      const next: FormErrors = { ...prev }
      let changed = false
      for (const key of ['routeId', 'baseURL', 'headers', 'ctx', 'maxTokens', 'models'] as const) {
        if (next[key] !== blocking[key]) {
          next[key] = blocking[key]
          changed = true
        }
      }
      return changed ? next : prev
    })
  }, [blocking])

  /* ---- 自动落盘 ---- */

  const push = useCallback(
    async (target: Payload): Promise<boolean> => {
      const ok = await write(target.id, target.profile)
      if (ok) setCreatedId((prev) => (prev === null ? target.id : prev))
      return ok
    },
    [write],
  )

  /**
   * 唯一的写入触发点:payload 指纹变了就 debounce 写一次。
   * 新增在路由 ID 失焦前不写(不建半成品网关)。
   */
  useEffect(() => {
    if (!payload || !fingerprint) return
    if (hasBlocking) return
    if (isFresh && !routeIdTouched) return
    if (fingerprint === writtenRef.current) return
    if (timerRef.current !== null) window.clearTimeout(timerRef.current)
    const target = payload
    timerRef.current = window.setTimeout(() => {
      timerRef.current = null
      writtenRef.current = fingerprint
      inFlightRef.current = push(target).then((ok) => {
        if (!ok) writtenRef.current = null
        return ok
      })
    }, AUTOSAVE_DEBOUNCE_MS)
    return () => {
      if (timerRef.current !== null) {
        window.clearTimeout(timerRef.current)
        timerRef.current = null
      }
    }
  }, [payload, fingerprint, hasBlocking, isFresh, routeIdTouched, push])

  /** 立刻落盘(跳过 debounce),用于失焦与关窗。 */
  const flushNow = useCallback(async (): Promise<boolean> => {
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current)
      timerRef.current = null
    }
    const pending = inFlightRef.current
    if (pending) {
      const ok = await pending
      inFlightRef.current = null
      const still = inFlightRef.current
      if (still) await still
      return ok
    }
    if (!payload || !fingerprint || hasBlocking) return true
    if (fingerprint === writtenRef.current) return true
    writtenRef.current = fingerprint
    const done = push(payload)
    inFlightRef.current = done
    const ok = await done
    inFlightRef.current = null
    if (!ok) writtenRef.current = null
    return ok
  }, [payload, fingerprint, hasBlocking, push])

  useEffect(() => {
    return () => {
      if (timerRef.current !== null) window.clearTimeout(timerRef.current)
    }
  }, [])

  /* ---- 密钥:失焦即写凭据存储(不参与 debounce) ---- */

  const commitKey = useCallback(async () => {
    const value = keyValue.trim()
    if (!value || !effectiveKeyRef) return
    try {
      await api.setCredential(effectiveKeyRef, value)
      setKeyValue('')
      notify('ok', t('keySaved'))
      onChanged()
    } catch (err) {
      notify('err', err instanceof Error ? err.message : String(err))
    }
  }, [keyValue, effectiveKeyRef, notify, onChanged])

  /* ---- 关窗:先 flush,失败不关 ---- */

  const requestClose = useCallback(async () => {
    if (closing) return
    setClosing(true)
    const dirty = payload !== null && fingerprint !== null && fingerprint !== writtenRef.current
    const ok = await flushNow()
    if (!ok) {
      setClosing(false)
      return
    }
    if (dirty && hasBlocking) {
      notify('err', t('llm.autosave.dropped'))
    }
    onClose()
  }, [closing, payload, fingerprint, hasBlocking, flushNow, notify, onClose])

  /* ---- 键盘:Esc 关窗(preventDefault 阻断设置弹窗同关) ---- */

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault()
        event.stopPropagation()
        void requestClose()
      }
    }
    window.addEventListener('keydown', onKeyDown, true)
    return () => window.removeEventListener('keydown', onKeyDown, true)
  }, [requestClose])

  const discoverParams = useCallback(
    () => ({
      baseURL: baseURL.trim(),
      apiKey: keyValue.trim() || undefined,
      apiKeyEnv: keyValue.trim() ? undefined : effectiveKeyRef || undefined,
      protocol,
      headers: Object.fromEntries(
        headers.map((row) => [row.name.trim(), row.value]).filter(([name]) => name !== ''),
      ),
    }),
    [baseURL, keyValue, effectiveKeyRef, protocol, headers],
  )

  const stepItems: { id: 1 | 2; label: string }[] = [
    { id: 1, label: t('llm.step.gateway') },
    { id: 2, label: t('llm.step.key') },
  ]

  const goStep = (next: 1 | 2) => {
    void flushNow()
    setStep(next)
  }

  return (
    <div className={styles.overlay}>
      {/* 遮罩单击不关窗(与设置弹窗一致:弹层里点空白太容易误触),双击才退出。 */}
      <div className={styles.backdrop} onDoubleClick={() => void requestClose()} aria-hidden="true" />
      <div
        className={styles.panel}
        role="dialog"
        aria-modal="true"
        aria-label={isFresh ? t('llm.modal.new') : t('llm.modal.edit')}
      >
        <header className={styles.head}>
          <h3>{isFresh ? t('llm.modal.new') : t('llm.modal.edit')}</h3>
          <button
            type="button"
            className={styles.close}
            onClick={() => void requestClose()}
            aria-label={t('close')}
          >
            ✕
          </button>
        </header>

        {isFresh && (
          <nav className={styles.steps} aria-label={t('llm.modal.new')}>
            {stepItems.map((item) => (
              <button
                key={item.id}
                type="button"
                className={`${styles.stepItem}${step === item.id ? ` ${styles.stepActive}` : ''}${
                  step > item.id ? ` ${styles.stepDone}` : ''
                }`}
                onClick={() => goStep(item.id)}
              >
                <span className={styles.stepIndex}>{step > item.id ? '✓' : item.id}</span>
                {item.label}
              </button>
            ))}
          </nav>
        )}

        <div className={styles.body}>
          {/* 步骤 1(新增):网关信息;编辑/已建时常驻 */}
          {(isFresh ? step === 1 : true) && (
            <section className={styles.section}>
              {!isFresh && <h4 className={styles.sectionTitle}>{t('llm.section.basic')}</h4>}
              <div className={styles.gridTwo}>
                <Field
                  label={t('routeIdLabel')}
                  hint={t(isFresh && !createdId ? 'llm.field.routeIdHint' : 'llm.field.routeIdLocked')}
                  error={errors.routeId}
                >
                  <TextInput
                    mono
                    placeholder={t('routeIdPlaceholder')}
                    value={routeId}
                    disabled={createdId !== null}
                    onChange={setRouteId}
                    onCommit={() => {
                      setRouteIdTouched(true)
                      void flushNow()
                    }}
                  />
                </Field>
                <Field label={t('displayNameLabel')} hint={t('llm.field.displayNameHint')}>
                  <TextInput value={displayName} onChange={setDisplayName} onCommit={() => void flushNow()} />
                </Field>
              </div>
              <Field label={t('baseURLLabel')} hint={t('llm.field.baseURLHint')} error={errors.baseURL}>
                <TextInput
                  mono
                  placeholder="https://gateway.example.com/v1"
                  value={baseURL}
                  onChange={setBaseURL}
                  onCommit={() => void flushNow()}
                />
              </Field>
              <Field label={t('protocolLabel')} hint={t('protocolHint')}>
                <ChipRadio
                  value={protocol}
                  ariaLabel={t('protocolLabel')}
                  options={WIRE_PROTOCOLS.map(({ id, label, hintKey }) => ({
                    id,
                    label,
                    hint: t(hintKey),
                  }))}
                  onChange={setProtocol}
                />
              </Field>
              <Field
                label={t('llm.field.headersLabel')}
                hint={t('llm.field.headersHint')}
                error={errors.headers}
              >
                <div className={styles.headerRows}>
                  {headers.map((row, index) => (
                    <div key={index} className={styles.headerRow}>
                      <TextInput
                        mono
                        placeholder={t('llm.field.headerNamePlaceholder')}
                        value={row.name}
                        onChange={(name) =>
                          setHeaders((rows) =>
                            rows.map((entry, i) => (i === index ? { ...entry, name } : entry)),
                          )
                        }
                        onCommit={() => void flushNow()}
                      />
                      <TextInput
                        mono
                        placeholder={t('llm.field.headerValuePlaceholder')}
                        value={row.value}
                        onChange={(value) =>
                          setHeaders((rows) =>
                            rows.map((entry, i) => (i === index ? { ...entry, value } : entry)),
                          )
                        }
                        onCommit={() => void flushNow()}
                      />
                      <button
                        type="button"
                        className={styles.headerRemove}
                        title={t('llm.field.headersRemove')}
                        aria-label={`${t('llm.field.headersRemove')} ${row.name || index + 1}`}
                        onClick={() => {
                          setHeaders((rows) => rows.filter((_, i) => i !== index))
                          void flushNow()
                        }}
                      >
                        ✕
                      </button>
                    </div>
                  ))}
                  <button
                    type="button"
                    className={styles.headerAdd}
                    onClick={() => setHeaders((rows) => [...rows, { name: '', value: '' }])}
                  >
                    + {t('llm.field.headersAdd')}
                  </button>
                </div>
              </Field>
              <div className={styles.gridTwo}>
                <Field label={t('defaultContextWindowLabel')} error={errors.ctx}>
                  <NumberInput
                    mono
                    value={ctxWindow}
                    onChange={setCtxWindow}
                    onCommit={() => void flushNow()}
                    placeholder="262144"
                  />
                </Field>
                <Field
                  label={t('llm.field.maxTokensLabel')}
                  hint={t('llm.field.maxTokensHint')}
                  error={errors.maxTokens}
                >
                  <NumberInput
                    mono
                    value={maxTokens}
                    onChange={setMaxTokens}
                    onCommit={() => void flushNow()}
                    placeholder="8192"
                  />
                </Field>
              </div>
              {isFresh && step === 1 && (
                <div className={styles.footRow}>
                  <Button onClick={() => goStep(2)} disabled={hasBlocking}>
                    {t('llm.modal.next')}
                  </Button>
                </div>
              )}
            </section>
          )}

          {/* 步骤 2(新增):密钥 + 模型;编辑/已建时常驻 */}
          {(!isFresh || step === 2) && (
            <>
              <section className={styles.section}>
                <h4 className={styles.sectionTitle}>{t('llm.section.key')}</h4>
                <div ref={keySectionRef} className={styles.keyValueWrap}>
                  <Field label={t('llm.field.keyValueLabel')} hint={t('llm.field.keyValueHint')}>
                    <TextInput
                      mono
                      type="password"
                      autoComplete="off"
                      placeholder={t('apiKeyPlaceholder')}
                      value={keyValue}
                      onChange={setKeyValue}
                      onCommit={() => void commitKey()}
                    />
                  </Field>
                  <span className={styles.keyOptional}>{t('llm.field.keyPasteOptional')}</span>
                </div>
                {!keyConfigured && effectiveKeyRef && (
                  credentialForRef && !credentialForRef.configured ? (
                    <div className={styles.keyWarn}>
                      <Badge tone="err">{t('apiKeyMissing')}</Badge>
                      <span className={styles.keyWarnText}>{t('llm.credential.missingHint')}</span>
                      <Button
                        small
                        onClick={() => keySectionRef.current?.scrollIntoView({ block: 'center' })}
                      >
                        {t('llm.credential.setCta')}
                      </Button>
                    </div>
                  ) : (
                    <p className={styles.keyUnknown}>{t('llm.field.keyUnknownHint')}</p>
                  )
                )}
                {keyConfigured && <Badge tone="ok">{t('apiKeySet')}</Badge>}
              </section>

              <section className={styles.section}>
                <h4 className={styles.sectionTitle}>{t('modelsSectionTitle')}</h4>
                <p className={styles.sectionHint}>{t('modelsSectionHint')}</p>
                <ModelRowsEditor
                  models={models}
                  onChange={setModels}
                  defaultContextWindow={ctxWindow}
                  discoverParams={discoverParams}
                  onCommit={() => void flushNow()}
                />
                {errors.models && (
                  <p className={styles.modelsError} role="alert">
                    {errors.models}
                  </p>
                )}
              </section>
              {isFresh && step === 2 && (
                <div className={styles.footRow}>
                  <Button onClick={() => goStep(1)}>{t('llm.modal.back')}</Button>
                </div>
              )}
            </>
          )}
        </div>

        <footer className={styles.foot}>
          <AutosaveStatus autosave={autosave} align="left" />
          <div className={styles.footActions}>
            <Button disabled={closing} onClick={() => void requestClose()}>
              {closing ? t('llm.autosave.saving') : t('llm.modal.done')}
            </Button>
          </div>
        </footer>
      </div>
    </div>
  )
}
