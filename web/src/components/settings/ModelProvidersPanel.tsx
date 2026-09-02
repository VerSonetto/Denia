import { useMemo, useState } from 'react'
import {
  describeCredentials,
  discoverModels,
  replaceNamespace,
  setCredential,
} from '../../api'
import ModelListEditor from '../ModelListEditor'
import { NumberField } from '../ui/controls'
import { t } from '../../i18n'
import { type CatalogModelEntry, entryFromWire, entryToWire } from '../../modelCatalog'
import type { Notify } from '../../App'
import type {
  CredentialInfo,
  DiscoveredModel,
  NamespaceView,
  OpenAiProfile,
  SettingsDescribe,
} from '../../types'

const OPENAI_NS = 'llm-openai'
const ROUTE_ID_PATTERN = /^[a-z][a-z0-9-]*$/
const MODEL_ID_PATTERN = /^[a-zA-Z0-9][a-zA-Z0-9._-]*$/

function namespaceOf(settings: SettingsDescribe, ns: string): NamespaceView | undefined {
  return settings.namespaces.find((view) => view.ns === ns)
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

function credentialBadge(info: CredentialInfo | undefined) {
  if (!info || !info.configured) {
    return (
      <span className="setm-badge err">
        <span className="setm-badge-dot" />
        {t('apiKeyMissing')}
      </span>
    )
  }
  if (info.source === 'env') {
    return (
      <span className="setm-badge ok">
        <span className="setm-badge-dot" />
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
    <span className="setm-badge ok">
      <span className="setm-badge-dot" />
      {t('apiKeySet')} ({sourceLabel})
    </span>
  )
}

export function ModelProvidersPanel({
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
    <section className="setm-section setm-section-divider">
      <div className="setm-section-label">{t('customTitle')}</div>
      <p className="setm-section-hint">{t('customDescription')}</p>

      {routes.length === 0 && !adding && (
        <p className="setm-empty">{t('noProvidersConfigured')}</p>
      )}

      <div className="setm-provider-list">
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
            <article className="setm-provider-card" key={route}>
              <div className="setm-provider-head">
                <div>
                  <div className="setm-provider-name">{profile.displayName || route}</div>
                  <div className="setm-provider-id">{route}</div>
                </div>
                {credentialBadge(profile.apiKeyEnv ? credentials[profile.apiKeyEnv] : undefined)}
              </div>
              <div className="setm-provider-meta">
                <span>{profile.baseURL}</span>
                {profile.apiKeyEnv && <span>key: {profile.apiKeyEnv}</span>}
                <span>
                  {t('modelsLabel')}: {(profile.models ?? []).length || '—'}
                </span>
              </div>
              <div className="setm-provider-actions">
                <button className="setm-btn ghost small" type="button" onClick={() => setEditing(route)}>
                  {t('editProvider')}
                </button>
                <button className="setm-btn danger small" type="button" onClick={() => void removeProvider(route)}>
                  {t('removeProvider')}
                </button>
              </div>
            </article>
          ),
        )}
      </div>

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
        <button className="setm-btn ghost setm-add-provider" type="button" onClick={() => setAdding(true)}>
          + {t('addProvider')}
        </button>
      )}
    </section>
  )
}

export async function loadProviderCredentials(settings: SettingsDescribe): Promise<Record<string, CredentialInfo>> {
  const openai = namespaceOf(settings, OPENAI_NS)
  const providers = asRecord(openai?.value?.providers)
  const refs = new Set<string>()
  for (const profile of Object.values(providers)) {
    const ref = asRecord(profile).apiKeyEnv
    if (typeof ref === 'string' && ref) refs.add(ref)
  }
  if (refs.size === 0) return {}
  return describeCredentials([...refs])
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
    <div className="setm-provider-editor">
      <div className="setm-form-grid two">
        <label className="setm-field">
          <span className="setm-field-label">{t('routeIdLabel')}</span>
          <input
            type="text"
            className="setm-input mono"
            placeholder={t('routeIdPlaceholder')}
            value={routeId}
            disabled={route !== null}
            onChange={(event) => setRouteId(event.target.value)}
          />
        </label>
        <label className="setm-field">
          <span className="setm-field-label">{t('displayNameLabel')}</span>
          <input
            type="text"
            className="setm-input"
            value={displayName}
            onChange={(event) => setDisplayName(event.target.value)}
          />
        </label>
      </div>
      <div className="setm-form-grid two">
        <label className="setm-field">
          <span className="setm-field-label">{t('baseURLLabel')}</span>
          <input
            type="text"
            className="setm-input mono"
            placeholder="https://gateway.example.com/v1"
            value={baseURL}
            onChange={(event) => setBaseURL(event.target.value)}
          />
        </label>
        <label className="setm-field">
          <span className="setm-field-label">{t('defaultContextWindowLabel')}</span>
          <NumberField
            mono
            placeholder="262144"
            value={defaultContextWindow.trim() ? Number(defaultContextWindow) : undefined}
            onChange={(value) => setDefaultContextWindow(value != null ? String(value) : '')}
          />
        </label>
      </div>
      <div className="setm-form-grid key">
        <label className="setm-field">
          <span className="setm-field-label">{t('apiKeyLabel')}</span>
          <input
            type="password"
            className="setm-input mono"
            autoComplete="off"
            placeholder={t('apiKeyPlaceholder')}
            value={keyValue}
            onChange={(event) => setKeyValue(event.target.value)}
          />
        </label>
        <button className="setm-btn ghost" type="button" disabled={discovering} onClick={() => void discover()}>
          {discovering ? t('discovering') : t('discoverModels')}
        </button>
      </div>

      <div className="setm-editor-block">
        <div className="setm-section-label">{t('modelsSectionTitle')}</div>
        <p className="setm-section-hint">{t('modelsSectionHint')}</p>
        <ModelListEditor
          models={models}
          onChange={setModels}
          discovered={discovered}
          defaultContextWindow={parsedDefaultContext}
          disabled={busy}
        />
      </div>

      <div className="setm-actions">
        <button className="setm-btn primary" type="button" disabled={busy} onClick={() => void save()}>
          {t('save')}
        </button>
        <button className="setm-btn ghost" type="button" disabled={busy} onClick={onDone}>
          {t('cancel')}
        </button>
      </div>
    </div>
  )
}
