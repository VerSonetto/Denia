import { useCallback, useEffect, useMemo, useState } from 'react'
import {
  describeCredentials,
  discoverModels,
  getSettings,
  replaceNamespace,
  setCredential,
} from '../api'
import ModelListEditor from '../components/ModelListEditor'
import { NumberField } from '../components/ui/controls'
import { t } from '../i18n'
import { type CatalogModelEntry, entryFromWire, entryToWire } from '../modelCatalog'
import type { Notify } from '../App'
import type {
  CredentialInfo,
  DiscoveredModel,
  NamespaceView,
  OpenAiProfile,
  SettingsDescribe,
} from '../types'

const OPENAI_NS = 'llm-openai'
const ROUTE_ID_PATTERN = /^[a-z][a-z0-9-]*$/
const MODEL_ID_PATTERN = /^[a-zA-Z0-9][a-zA-Z0-9._-]*$/

function namespaceOf(settings: SettingsDescribe | null, ns: string): NamespaceView | undefined {
  return settings?.namespaces.find((view) => view.ns === ns)
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === 'object' && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {}
}

function loadOpenAiModels(profile: OpenAiProfile | null): CatalogModelEntry[] {
  return (profile?.models ?? []).map((entry) => entryFromWire(entry as unknown as Record<string, unknown>))
}

function validateModels(models: CatalogModelEntry[]): boolean {
  const seen = new Set<string>()
  for (const entry of models) {
    const id = entry.id.trim()
    if (!id || !MODEL_ID_PATTERN.test(id) || seen.has(id)) return false
    seen.add(id)
  }
  return true
}

export default function ModelsPage({ notify }: { notify: Notify }) {
  const [settings, setSettings] = useState<SettingsDescribe | null>(null)
  const [credentials, setCredentials] = useState<Record<string, CredentialInfo>>({})
  const [error, setError] = useState<string | null>(null)
  const [reloadKey, setReloadKey] = useState(0)

  const load = useCallback(async () => {
    try {
      setError(null)
      const nextSettings = await getSettings()
      setSettings(nextSettings)

      const refs = new Set<string>()
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

  if (error && !settings) {
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

  if (!settings) {
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
        <ProvidersSection
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

function ProvidersSection({
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
          {t('noProvidersConfigured')}
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
  const [keyValue, setKeyValue] = useState('')
  const [defaultContextWindow, setDefaultContextWindow] = useState(
    initial?.defaultContextWindow ? String(initial.defaultContextWindow) : '',
  )
  const [models, setModels] = useState<CatalogModelEntry[]>(() => loadOpenAiModels(initial))
  const [discovered, setDiscovered] = useState<DiscoveredModel[] | null>(null)
  const [discovering, setDiscovering] = useState(false)
  const [busy, setBusy] = useState(false)

  const deriveKeyRef = (id: string) =>
    id.toUpperCase().replace(/[^A-Z0-9]+/g, '_') + '_API_KEY'

  const effectiveKeyRef = routeId.trim() ? deriveKeyRef(routeId.trim()) : ''

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
    if (!validateModels(models)) {
      notify('err', t('modelsInvalid'))
      return
    }
    setBusy(true)
    try {
      if (keyValue.trim()) {
        await setCredential(effectiveKeyRef, keyValue.trim())
      }
      const profile: Record<string, unknown> = {
        baseURL: baseURL.trim(),
        displayName: displayName.trim() || undefined,
        apiKeyEnv: effectiveKeyRef || undefined,
        models: models.map((entry) => entryToWire(entry)),
      }
      const ctx = defaultContextWindow.trim()
      if (ctx) profile.defaultContextWindow = Number(ctx)
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

  const parsedDefaultContext = defaultContextWindow.trim()
    ? Number(defaultContextWindow.trim())
    : undefined

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
          <label>{t('defaultContextWindowLabel')}</label>
          <NumberField
            mono
            placeholder="262144"
            value={defaultContextWindow.trim() ? Number(defaultContextWindow) : undefined}
            onChange={(value) => setDefaultContextWindow(value != null ? String(value) : '')}
          />
        </div>
      </div>
      <div className="row row-end">
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
        <button
          className="btn small secondary form-action"
          disabled={discovering}
          onClick={() => void discover()}
        >
          {discovering ? t('discovering') : t('discoverModels')}
        </button>
      </div>

      <div className="section-divider" />

      <h3 className="subsection-title">{t('modelsSectionTitle')}</h3>
      <p className="hint">{t('modelsSectionHint')}</p>
      <ModelListEditor
        models={models}
        onChange={setModels}
        discovered={discovered}
        defaultContextWindow={parsedDefaultContext}
        disabled={busy}
      />

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
