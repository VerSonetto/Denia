import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  describeCredentials,
  discoverModels,
  getCatalog,
  getSettings,
  replaceNamespace,
  setCredential,
  unsetCredential,
  updateNamespace,
} from '../api'
import { t } from '../i18n'
import type { Notify } from '../App'
import type {
  CredentialInfo,
  DiscoveredModel,
  ModelCatalog,
  NamespaceView,
  OpenAiProfile,
  SettingsDescribe,
} from '../types'

const DEEPSEEK_NS = 'llm-deepseek'
const OPENAI_NS = 'llm-openai'
const ROUTE_ID_PATTERN = /^[a-z][a-z0-9-]*$/

function namespaceOf(settings: SettingsDescribe | null, ns: string): NamespaceView | undefined {
  return settings?.namespaces.find((view) => view.ns === ns)
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {}
}

export default function ModelsPage({ notify }: { notify: Notify }) {
  const [catalog, setCatalog] = useState<ModelCatalog | null>(null)
  const [settings, setSettings] = useState<SettingsDescribe | null>(null)
  const [credentials, setCredentials] = useState<Record<string, CredentialInfo>>({})
  const [error, setError] = useState<string | null>(null)
  const [reloadKey, setReloadKey] = useState(0)

  const load = useCallback(async () => {
    try {
      setError(null)
      const [nextCatalog, nextSettings] = await Promise.all([getCatalog(), getSettings()])
      setCatalog(nextCatalog)
      setSettings(nextSettings)

      const refs = new Set<string>()
      const deepseek = namespaceOf(nextSettings, DEEPSEEK_NS)
      const deepseekKeyRef = (deepseek?.value?.apiKeyEnv as string) || 'DEEPSEEK_API_KEY'
      refs.add(deepseekKeyRef)
      const openai = namespaceOf(nextSettings, OPENAI_NS)
      const providers = asRecord(openai?.value?.providers)
      for (const profile of Object.values(providers)) {
        const ref = asRecord(profile).apiKeyEnv
        if (typeof ref === 'string' && ref) refs.add(ref)
      }
      setCredentials(await describeCredentials([...refs]))
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load, reloadKey])

  useEffect(() => {
    const source = new EventSource('/api/events')
    source.onmessage = () => setReloadKey((key) => key + 1)
    return () => source.close()
  }, [])

  if (error && !catalog) {
    return (
      <div className="page-inner">
        <div className="card">
          <h2>{t('error')}</h2>
          <p className="hint">{error}</p>
          <button className="btn" onClick={() => void load()}>
            {t('retry')}
          </button>
        </div>
      </div>
    )
  }

  if (!catalog || !settings) {
    return (
      <div className="page-inner">
        <div className="card">
          <p className="hint">{t('loading')}</p>
        </div>
      </div>
    )
  }

  return (
    <div className="models-main">
      <div className="models-inner">
      <DeepSeekCard settings={settings} credentials={credentials} notify={notify} />
      <CustomProvidersSection
        settings={settings}
        credentials={credentials}
        notify={notify}
        onChanged={() => setReloadKey((key) => key + 1)}
      />
      </div>
    </div>
  )
}

function credentialBadge(info: CredentialInfo | undefined) {
  if (!info || !info.configured) {
    return (
      <span className="badge err">
        <span className="dot" />
        {t('apiKeyMissing')}
      </span>
    )
  }
  if (info.source === 'env') {
    return (
      <span className="badge ok">
        <span className="dot" />
        {t('apiKeyEnvShadowed')} ({t('apiKeyFromEnv')})
      </span>
    )
  }
  const sourceLabel =
    info.source === 'file'
      ? t('apiKeyFromFile')
      : info.source === 'project-env'
        ? t('apiKeyFromProjectEnv')
        : t('apiKeyFromUserEnv')
  return (
    <span className="badge ok">
      <span className="dot" />
      {t('apiKeySet')} ({sourceLabel})
    </span>
  )
}

function DeepSeekCard({
  settings,
  credentials,
  notify,
}: {
  settings: SettingsDescribe
  credentials: Record<string, CredentialInfo>
  notify: Notify
}) {
  const view = namespaceOf(settings, DEEPSEEK_NS)
  const value = asRecord(view?.value)
  const keyRef = (value.apiKeyEnv as string) || 'DEEPSEEK_API_KEY'
  const [baseURL, setBaseURL] = useState('')
  const [thinking, setThinking] = useState<'enabled' | 'disabled'>('enabled')
  const [keyValue, setKeyValue] = useState('')
  const [busy, setBusy] = useState(false)
  const loadedFrom = useRef<string>('')

  // Seed the local form from the resolved section exactly once per server value.
  const seedSignature = `${view?.revision ?? 0}:${JSON.stringify(value)}`
  useEffect(() => {
    if (loadedFrom.current === seedSignature) return
    loadedFrom.current = seedSignature
    setBaseURL((value.baseURL as string) ?? '')
    setThinking((value.thinking as 'enabled' | 'disabled') ?? 'enabled')
  }, [seedSignature, value])

  const saveConfig = async () => {
    if (!view) return
    setBusy(true)
    try {
      const patch: Record<string, unknown> = { thinking }
      if (baseURL.trim()) patch.baseURL = baseURL.trim()
      await updateNamespace(DEEPSEEK_NS, patch, view.revision)
      notify('ok', t('providerConfigSaved'))
    } catch (err) {
      notify('err', err instanceof Error ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }

  const saveKey = async () => {
    if (!keyValue.trim()) return
    setBusy(true)
    try {
      await setCredential(keyRef, keyValue.trim())
      setKeyValue('')
      notify('ok', t('keySaved'))
    } catch (err) {
      notify('err', err instanceof Error ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }

  const removeKey = async () => {
    setBusy(true)
    try {
      await unsetCredential(keyRef)
      notify('ok', t('keyRemoved'))
    } catch (err) {
      notify('err', err instanceof Error ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }

  const keyInfo = credentials[keyRef]

  return (
    <section className="card">
      <h2>{t('deepseekTitle')}</h2>
      <p className="hint">{t('deepseekDescription')}</p>

      <div className="row">
        <span className="code-ref">{keyRef}</span>
        {credentialBadge(keyInfo)}
      </div>

      <div className="row">
        <div className="field">
          <label>{t('apiKeyLabel')}</label>
          <input
            type="password"
            className="mono"
            autoComplete="off"
            placeholder={t('apiKeyPlaceholder')}
            value={keyValue}
            onChange={(event) => setKeyValue(event.target.value)}
          />
        </div>
        <button className="btn small" disabled={busy || !keyValue.trim()} onClick={() => void saveKey()}>
          {t('saveKey')}
        </button>
        {keyInfo?.configured && keyInfo.writable && (
          <button className="btn small danger" disabled={busy} onClick={() => void removeKey()}>
            {t('removeKey')}
          </button>
        )}
      </div>

      <div className="section-divider" />

      <div className="row">
        <div className="field">
          <label>{t('baseURLLabel')}</label>
          <input
            type="text"
            className="mono"
            placeholder="https://api.deepseek.com"
            value={baseURL}
            onChange={(event) => setBaseURL(event.target.value)}
          />
        </div>
        <div className="field narrow">
          <label>{t('thinkingLabel')}</label>
          <select
            value={thinking}
            onChange={(event) => setThinking(event.target.value as 'enabled' | 'disabled')}
          >
            <option value="enabled">{t('thinkingEnabled')}</option>
            <option value="disabled">{t('thinkingDisabled')}</option>
          </select>
        </div>
        <button className="btn" disabled={busy} onClick={() => void saveConfig()}>
          {t('saveProviderConfig')}
        </button>
      </div>
    </section>
  )
}

function CustomProvidersSection({
  settings,
  credentials,
  notify,
  onChanged,
}: {
  settings: SettingsDescribe
  credentials: Record<string, CredentialInfo>
  notify: Notify
  onChanged: () => void
}) {
  const view = namespaceOf(settings, OPENAI_NS)
  const providers = asRecord(view?.value?.providers)
  const routes = useMemo(
    () =>
      Object.entries(providers).map(([route, raw]) => ({
        route,
        profile: asRecord(raw) as unknown as OpenAiProfile,
      })),
    [providers],
  )
  const [editing, setEditing] = useState<string | null>(null)
  const [adding, setAdding] = useState(false)

  const removeProvider = async (route: string) => {
    if (!view) return
    const next = { ...providers }
    delete next[route]
    try {
      await replaceNamespace(OPENAI_NS, { providers: next }, view.revision)
      notify('ok', t('providerRemoved'))
      onChanged()
    } catch (err) {
      notify('err', err instanceof Error ? err.message : String(err))
    }
  }

  return (
    <section className="card">
      <h2>{t('customTitle')}</h2>
      <p className="hint">{t('customDescription')}</p>

      {routes.length === 0 && !adding && (
        <p className="hint" style={{ marginBottom: 8 }}>
          —
        </p>
      )}

      {routes.map(({ route, profile }) =>
        editing === route ? (
          <ProviderEditor
            key={route}
            route={route}
            initial={profile}
            revision={view?.revision ?? 0}
            providers={providers}
            notify={notify}
            onChanged={onChanged}
            onDone={() => setEditing(null)}
          />
        ) : (
          <div className="provider-card" key={route}>
            <div className="head">
              <span className="name">{profile.displayName || route}</span>
              <span className="id">{route}</span>
              {credentialBadge(profile.apiKeyEnv ? credentials[profile.apiKeyEnv] : undefined)}
              <span className="spacer" />
              <button className="btn small secondary" onClick={() => setEditing(route)}>
                {t('editProvider')}
              </button>
              <button className="btn small danger" onClick={() => void removeProvider(route)}>
                {t('removeProvider')}
              </button>
            </div>
            <div className="row" style={{ marginTop: 8 }}>
              <span className="code-ref">{profile.baseURL}</span>
              {profile.apiKeyEnv && <span className="code-ref">key: {profile.apiKeyEnv}</span>}
              <span className="code-ref">
                {t('modelsLabel')}: {(profile.models ?? []).length || '—'}
              </span>
            </div>
          </div>
        ),
      )}

      {adding ? (
        <ProviderEditor
          route={null}
          initial={null}
          revision={view?.revision ?? 0}
          providers={providers}
          notify={notify}
          onChanged={onChanged}
          onDone={() => setAdding(false)}
        />
      ) : (
        <div className="row" style={{ marginTop: 12 }}>
          <button className="btn secondary" onClick={() => setAdding(true)}>
            + {t('addProvider')}
          </button>
        </div>
      )}
    </section>
  )
}

function ProviderEditor({
  route,
  initial,
  revision,
  providers,
  notify,
  onChanged,
  onDone,
}: {
  route: string | null
  initial: OpenAiProfile | null
  revision: number
  providers: Record<string, unknown>
  notify: Notify
  onChanged: () => void
  onDone: () => void
}) {
  const [routeId, setRouteId] = useState(route ?? '')
  const [displayName, setDisplayName] = useState(initial?.displayName ?? '')
  const [baseURL, setBaseURL] = useState(initial?.baseURL ?? '')
  const [keyRef, setKeyRef] = useState(initial?.apiKeyEnv ?? '')
  const [keyValue, setKeyValue] = useState('')
  const [models, setModels] = useState<string[]>((initial?.models ?? []).map((m) => m.id))
  const [discovered, setDiscovered] = useState<DiscoveredModel[] | null>(null)
  const [discovering, setDiscovering] = useState(false)
  const [busy, setBusy] = useState(false)

  const deriveKeyRef = (id: string) =>
    id.toUpperCase().replace(/[^A-Z0-9]+/g, '_') + '_API_KEY'

  const effectiveKeyRef = keyRef.trim() || (routeId.trim() ? deriveKeyRef(routeId.trim()) : '')

  const discover = async () => {
    if (!baseURL.trim()) {
      notify('err', t('baseURLRequired'))
      return
    }
    setDiscovering(true)
    try {
      const found = await discoverModels(
        baseURL.trim(),
        keyValue.trim() || undefined,
        keyValue.trim() ? undefined : effectiveKeyRef || undefined,
      )
      setDiscovered(found)
      if (models.length === 0) setModels(found.map((m) => m.id))
    } catch (err) {
      notify('err', `${t('discoverFailed')}: ${err instanceof Error ? err.message : String(err)}`)
    } finally {
      setDiscovering(false)
    }
  }

  const save = async () => {
    const id = routeId.trim()
    if (!ROUTE_ID_PATTERN.test(id)) {
      notify('err', t('routeIdInvalid'))
      return
    }
    if (!baseURL.trim()) {
      notify('err', t('baseURLRequired'))
      return
    }
    setBusy(true)
    try {
      if (keyValue.trim()) {
        await setCredential(effectiveKeyRef, keyValue.trim())
      }
      const profile: OpenAiProfile = {
        baseURL: baseURL.trim(),
        displayName: displayName.trim() || undefined,
        apiKeyEnv: effectiveKeyRef || undefined,
        models: models.map((modelId) => ({ id: modelId })),
      }
      const next = { ...providers, [id]: profile }
      if (route && route !== id) delete next[route]
      await replaceNamespace(OPENAI_NS, { providers: next }, revision)
      notify('ok', t('providerSaved'))
      onChanged()
      onDone()
    } catch (err) {
      notify('err', err instanceof Error ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="provider-card" style={{ marginTop: 10 }}>
      <div className="row">
        <div className="field narrow">
          <label>{t('routeIdLabel')}</label>
          <input
            type="text"
            className="mono"
            placeholder={t('routeIdPlaceholder')}
            value={routeId}
            disabled={route !== null}
            onChange={(event) => setRouteId(event.target.value)}
          />
        </div>
        <div className="field">
          <label>{t('displayNameLabel')}</label>
          <input
            type="text"
            value={displayName}
            onChange={(event) => setDisplayName(event.target.value)}
          />
        </div>
      </div>
      <div className="row">
        <div className="field">
          <label>{t('baseURLLabel')}</label>
          <input
            type="text"
            className="mono"
            placeholder="https://gateway.example.com/v1"
            value={baseURL}
            onChange={(event) => setBaseURL(event.target.value)}
          />
        </div>
        <div className="field narrow">
          <label>{t('keyRefLabel')}</label>
          <input
            type="text"
            className="mono"
            placeholder={effectiveKeyRef || 'API_KEY_ENV'}
            value={keyRef}
            onChange={(event) => setKeyRef(event.target.value)}
          />
        </div>
      </div>
      <div className="row">
        <div className="field">
          <label>{t('apiKeyLabel')}</label>
          <input
            type="password"
            className="mono"
            autoComplete="off"
            placeholder={t('apiKeyPlaceholder')}
            value={keyValue}
            onChange={(event) => setKeyValue(event.target.value)}
          />
        </div>
        <button className="btn small secondary" disabled={discovering} onClick={() => void discover()}>
          {discovering ? t('discovering') : t('discoverModels')}
        </button>
      </div>

      {discovered && (
        <>
          <p className="hint" style={{ marginTop: 10, marginBottom: 0 }}>
            {t('discoveredCount', { n: discovered.length })}
          </p>
          <div className="checklist">
            {discovered.map((model) => (
              <label key={model.id}>
                <input
                  type="checkbox"
                  checked={models.includes(model.id)}
                  onChange={(event) => {
                    setModels((current) =>
                      event.target.checked
                        ? [...current, model.id]
                        : current.filter((id) => id !== model.id),
                    )
                  }}
                />
                {model.id}
              </label>
            ))}
          </div>
        </>
      )}

      <div className="row" style={{ marginTop: 12 }}>
        <button className="btn" disabled={busy} onClick={() => void save()}>
          {t('save')}
        </button>
        <button className="btn secondary" disabled={busy} onClick={onDone}>
          {t('cancel')}
        </button>
      </div>
    </div>
  )
}
