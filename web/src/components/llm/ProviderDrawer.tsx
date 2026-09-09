/**
 * 提供方编辑抽屉:
 * - 新增走两步:网关 → 密钥(+模型);步骤间做就地校验(fail loud)。
 * - 编辑平铺三段,路由 ID 锁死。
 * Esc 关抽屉(拦截冒泡,不连设置弹窗一起关);Cmd/Ctrl+Enter 提交。
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import {
  WIRE_PROTOCOLS,
  entryFromWire,
  entryToWire,
  type CatalogModelEntry,
} from '../../modelCatalog'
import { Badge, Button, ChipRadio, Field, NumberInput, TextInput } from './atoms/form'
import { ModelRowsEditor } from './ModelRowsEditor'
import type { Notify } from '../../App'
import type { CredentialInfo, OpenAiProfile, WireProtocol } from '../../types'
import styles from './ProviderDrawer.module.css'

const OPENAI_NS = 'llm-openai'

function deriveKeyRef(routeId: string): string {
  return routeId.toUpperCase().replace(/[^A-Z0-9]+/g, '_') + '_API_KEY'
}

interface FormErrors {
  routeId?: string
  baseURL?: string
  ctx?: string
  maxTokens?: string
  models?: string
}

export function ProviderDrawer({
  route,
  initial,
  revision,
  providers,
  credentials,
  notify,
  onClose,
  onSaved,
}: {
  /** 编辑的路由 id;null = 新增两步流程。 */
  route: string | null
  initial: OpenAiProfile | null
  revision: number
  providers: Record<string, unknown>
  credentials: Record<string, CredentialInfo>
  notify: Notify
  onClose: () => void
  onSaved: () => void
}) {
  const isNew = route === null
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
  const [errors, setErrors] = useState<FormErrors>({})
  const [busy, setBusy] = useState(false)
  const keyValueRef = useRef<HTMLInputElement | null>(null)
  const drawerRef = useRef<HTMLDivElement | null>(null)

  // 密钥引用不暴露给用户:编辑沿用已存引用(不丢已配密钥),新增按路由派生。
  const effectiveKeyRef = initial?.apiKeyEnv ?? (routeId.trim() ? deriveKeyRef(routeId.trim()) : '')
  const credentialForRef = credentials[effectiveKeyRef]
  const keyConfigured = keyValue.trim().length > 0 || credentialForRef?.configured === true

  /* ---- 校验(提交/步进时全量跑,就地报错) ---- */

  const validateGateway = useCallback((): boolean => {
    const nextErrors: FormErrors = {}
    const id = routeId.trim()
    // 只做非空与重复校验,不做字符格式限制(网关 id 常含点、下划线等)。
    if (!id) {
      nextErrors.routeId = t('routeIdRequired')
    } else if (isNew && id in providers) {
      nextErrors.routeId = t('llm.error.routeIdExists', { id })
    }
    const url = baseURL.trim()
    if (!url) {
      nextErrors.baseURL = t('baseURLRequired')
    } else if (!/^https?:\/\//i.test(url)) {
      nextErrors.baseURL = t('llm.error.baseURLFormat')
    }
    setErrors((prev) => ({ ...prev, ...nextErrors, models: undefined }))
    return Object.keys(nextErrors).length === 0
  }, [routeId, baseURL, providers, isNew])

  const validateAll = useCallback((): boolean => {
    const nextErrors: FormErrors = {}
    const id = routeId.trim()
    if (!id) {
      nextErrors.routeId = t('routeIdRequired')
    } else if (isNew && id in providers) {
      nextErrors.routeId = t('llm.error.routeIdExists', { id })
    }
    const url = baseURL.trim()
    if (!url) {
      nextErrors.baseURL = t('baseURLRequired')
    } else if (!/^https?:\/\//i.test(url)) {
      nextErrors.baseURL = t('llm.error.baseURLFormat')
    }
    if (ctxWindow !== undefined && (!Number.isFinite(ctxWindow) || ctxWindow <= 0)) {
      nextErrors.ctx = t('llm.error.numberPositive')
    }
    if (maxTokens !== undefined && (!Number.isFinite(maxTokens) || maxTokens <= 0)) {
      nextErrors.maxTokens = t('llm.error.numberPositive')
    }
    const seen = new Set<string>()
    for (const entry of models) {
      const modelId = entry.id.trim()
      // 只拦非空与重复;不校验字符格式(网关模型 id 常含 `/`、`:` 等)。
      if (!modelId) {
        nextErrors.models = t('modelsInvalid')
        break
      }
      if (seen.has(modelId)) {
        nextErrors.models = t('llm.models.duplicate', { id: modelId })
        break
      }
      seen.add(modelId)
    }
    setErrors(nextErrors)
    return Object.keys(nextErrors).length === 0
  }, [routeId, baseURL, providers, isNew, ctxWindow, maxTokens, models])

  /* ---- 保存 ---- */

  const save = useCallback(async () => {
    if (!validateAll()) return
    const id = routeId.trim()
    setBusy(true)
    try {
      if (keyValue.trim()) {
        await api.setCredential(effectiveKeyRef, keyValue.trim())
      }
      const profile: Record<string, unknown> = {
        baseURL: baseURL.trim(),
        displayName: displayName.trim() || undefined,
        apiKeyEnv: effectiveKeyRef || undefined,
        protocol,
        models: models.map((entry) => entryToWire(entry)),
      }
      if (ctxWindow !== undefined && ctxWindow > 0) profile.defaultContextWindow = ctxWindow
      if (maxTokens !== undefined && maxTokens > 0) profile.defaultMaxTokens = maxTokens
      const next = { ...providers, [id]: profile }
      await api.replaceNamespace(OPENAI_NS, { providers: next }, revision)
      notify('ok', t('providerSaved'))
      onSaved()
      onClose()
    } catch (err) {
      notify('err', err instanceof Error ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }, [
    validateAll,
    routeId,
    keyValue,
    effectiveKeyRef,
    baseURL,
    displayName,
    protocol,
    models,
    ctxWindow,
    maxTokens,
    providers,
    revision,
    notify,
    onSaved,
    onClose,
  ])

  /* ---- 步进 ---- */

  const goNext = () => {
    if (step === 1) {
      if (!validateGateway()) return
      setStep(2)
    }
  }

  const goBack = () => {
    setErrors({})
    setStep(1)
  }

  /* ---- 键盘:Esc 关抽屉(preventDefault 阻断弹窗同关);Cmd/Ctrl+Enter 推进/提交 ---- */

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault()
        event.stopPropagation()
        onClose()
        return
      }
      if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') {
        event.preventDefault()
        if (isNew && step !== 2) goNext()
        else void save()
      }
    }
    window.addEventListener('keydown', onKeyDown, true)
    return () => window.removeEventListener('keydown', onKeyDown, true)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [onClose, isNew, step, save])

  const discoverParams = useCallback(
    () => ({
      baseURL: baseURL.trim(),
      apiKey: keyValue.trim() || undefined,
      apiKeyEnv: keyValue.trim() ? undefined : effectiveKeyRef || undefined,
      protocol,
    }),
    [baseURL, keyValue, effectiveKeyRef, protocol],
  )

  const stepItems: { id: 1 | 2; label: string }[] = [
    { id: 1, label: t('llm.step.gateway') },
    { id: 2, label: t('llm.step.key') },
  ]

  return (
    <div className={styles.overlay}>
      <div className={styles.backdrop} onClick={onClose} aria-hidden="true" />
      <div
        ref={drawerRef}
        className={styles.drawer}
        role="dialog"
        aria-modal="true"
        aria-label={isNew ? t('llm.drawer.new') : t('llm.drawer.edit')}
      >
        <header className={styles.head}>
          <h3>{isNew ? t('llm.drawer.new') : t('llm.drawer.edit')}</h3>
          <button type="button" className={styles.close} onClick={onClose} aria-label={t('close')}>
            ✕
          </button>
        </header>

        {isNew && (
          <nav className={styles.steps} aria-label={t('llm.drawer.new')}>
            {stepItems.map((item) => (
              <button
                key={item.id}
                type="button"
                className={`${styles.stepItem}${
                  step === item.id ? ` ${styles.stepActive}` : ''
                }${step > item.id ? ` ${styles.stepDone}` : ''}`}
                onClick={() => {
                  if (item.id < step) {
                    setErrors({})
                    setStep(item.id)
                  } else if (item.id === 2 && step === 1) {
                    goNext()
                  }
                }}
              >
                <span className={styles.stepIndex}>{step > item.id ? '✓' : item.id}</span>
                {item.label}
              </button>
            ))}
          </nav>
        )}

        <div className={styles.body}>
          {/* 步骤 1(新增):网关信息;编辑时常驻 */}
          {(isNew ? step === 1 : true) && (
            <section className={styles.section}>
              {!isNew && <h4 className={styles.sectionTitle}>{t('llm.section.basic')}</h4>}
              <div className={styles.gridTwo}>
                <Field label={t('routeIdLabel')} hint={t('llm.field.routeIdHint')} error={errors.routeId}>
                  <TextInput
                    mono
                    placeholder={t('routeIdPlaceholder')}
                    value={routeId}
                    disabled={!isNew}
                    onChange={setRouteId}
                  />
                </Field>
                <Field label={t('displayNameLabel')} hint={t('llm.field.displayNameHint')}>
                  <TextInput value={displayName} onChange={setDisplayName} />
                </Field>
              </div>
              <Field label={t('baseURLLabel')} hint={t('llm.field.baseURLHint')} error={errors.baseURL}>
                <TextInput
                  mono
                  placeholder="https://gateway.example.com/v1"
                  value={baseURL}
                  onChange={setBaseURL}
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
              <div className={styles.gridTwo}>
                <Field label={t('defaultContextWindowLabel')} error={errors.ctx}>
                  <NumberInput mono value={ctxWindow} onChange={setCtxWindow} placeholder="262144" />
                </Field>
                <Field label={t('llm.field.maxTokensLabel')} hint={t('llm.field.maxTokensHint')} error={errors.maxTokens}>
                  <NumberInput mono value={maxTokens} onChange={setMaxTokens} placeholder="8192" />
                </Field>
              </div>
              {isNew && step === 1 && (
                <div className={styles.footRow}>
                  <Button variant="primary" onClick={goNext}>
                    {t('llm.drawer.next')}
                  </Button>
                </div>
              )}
            </section>
          )}

          {/* 步骤 2(新增):密钥 + 模型;编辑时常驻 */}
          {(isNew ? step === 2 : true) && (
            <>
              <section className={styles.section}>
                <h4 className={styles.sectionTitle}>{t('llm.section.key')}</h4>
                <div ref={keyValueRef} className={styles.keyValueWrap}>
                  <Field label={t('llm.field.keyValueLabel')} hint={t('llm.field.keyValueHint')}>
                    <TextInput
                      mono
                      type="password"
                      autoComplete="off"
                      placeholder={t('apiKeyPlaceholder')}
                      value={keyValue}
                      onChange={setKeyValue}
                    />
                  </Field>
                  <span className={styles.keyOptional}>{t('llm.field.keyPasteOptional')}</span>
                </div>
                {!keyConfigured && effectiveKeyRef && (
                  credentialForRef && !credentialForRef.configured ? (
                    <div className={styles.keyWarn}>
                      <Badge tone="err">{t('apiKeyMissing')}</Badge>
                      <span className={styles.keyWarnText}>{t('llm.credential.missingHint')}</span>
                      <Button small onClick={() => keyValueRef.current?.scrollIntoView({ block: 'center' })}>
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
                  disabled={busy}
                />
                {errors.models && (
                  <p className={styles.modelsError} role="alert">
                    {errors.models}
                  </p>
                )}
              </section>
              {isNew && step === 2 && (
                <div className={styles.footRow}>
                  <Button onClick={goBack}>{t('llm.drawer.back')}</Button>
                </div>
              )}
            </>
          )}
        </div>

        <footer className={styles.foot}>
          {errors.models && <span className={styles.footError}>{errors.models}</span>}
          <div className={styles.footActions}>
            {!isNew && (
              <Button onClick={onClose} disabled={busy}>
                {t('cancel')}
              </Button>
            )}
            {(!isNew || step === 2) && (
              <Button variant="primary" disabled={busy} onClick={() => void save()}>
                {busy ? t('loading') : t('save')}
              </Button>
            )}
          </div>
        </footer>
      </div>
    </div>
  )
}
