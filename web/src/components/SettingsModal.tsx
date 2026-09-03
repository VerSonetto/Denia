import { useCallback, useEffect, useState, type ReactNode } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import type { Notify } from '../App'
import { IconClose, IconGear, IconPrompt, IconShield, IconSliders } from './icons'
import { loadProviderCredentials, ModelProvidersPanel } from './settings/ModelProvidersPanel'
import { Toggle } from './ui/controls'
import type { SettingsDescribe } from '../types'

type SettingsTab = 'general' | 'models' | 'security' | 'appearance'

const CONSOLE_NS = 'console'

const TABS: SettingsTab[] = ['general', 'models', 'security', 'appearance']

function tabIcon(tab: SettingsTab, size = 16) {
  switch (tab) {
    case 'general':
      return <IconPrompt size={size} />
    case 'models':
      return <IconSliders size={size} />
    case 'security':
      return <IconShield size={size} />
    case 'appearance':
      return <IconGear size={size} />
  }
}

function tabLabel(tab: SettingsTab): string {
  switch (tab) {
    case 'general':
      return t('settingsTabGeneral')
    case 'models':
      return t('settingsTabModels')
    case 'security':
      return t('settingsTabSecurity')
    case 'appearance':
      return t('settingsTabAppearance')
  }
}

function tabNavDesc(tab: SettingsTab): string {
  switch (tab) {
    case 'general':
      return t('settingsTabGeneralDesc')
    case 'models':
      return t('settingsTabModelsDesc')
    case 'security':
      return t('settingsTabSecurityDesc')
    case 'appearance':
      return t('settingsTabAppearanceDesc')
  }
}

function paneTitle(tab: SettingsTab): string {
  switch (tab) {
    case 'general':
      return t('systemPromptTitle')
    case 'models':
      return t('settingsPaneModelsTitle')
    case 'security':
      return t('catSecurity')
    case 'appearance':
      return t('catAppearance')
  }
}

function paneDesc(tab: SettingsTab): string {
  switch (tab) {
    case 'general':
      return t('systemPromptHint')
    case 'models':
      return t('settingsPaneModelsDesc')
    case 'security':
      return t('sandboxDesc')
    case 'appearance':
      return t('settingsAppearanceHint')
  }
}

function SetmActions({ children }: { children: ReactNode }) {
  return <div className="setm-actions">{children}</div>
}

function SetmBtn({
  children,
  variant = 'primary',
  disabled,
  onClick,
}: {
  children: ReactNode
  variant?: 'primary' | 'ghost' | 'danger'
  disabled?: boolean
  onClick?: () => void
}) {
  return (
    <button
      type="button"
      className={`setm-btn ${variant}`}
      disabled={disabled}
      onClick={onClick}
    >
      {children}
    </button>
  )
}

function SetmTiles<T extends string>({
  value,
  options,
  onChange,
}: {
  value: T
  options: { id: T; label: string }[]
  onChange: (next: T) => void
}) {
  return (
    <div className="setm-tiles" role="radiogroup">
      {options.map((option) => (
        <button
          key={option.id}
          type="button"
          role="radio"
          aria-checked={value === option.id}
          className={`setm-tile${value === option.id ? ' active' : ''}`}
          onClick={() => onChange(option.id)}
        >
          {option.label}
        </button>
      ))}
    </div>
  )
}

export function SettingsModal({ notify, onClose }: { notify: Notify; onClose: () => void }) {
  const [tab, setTab] = useState<SettingsTab>('general')
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)
  const [settings, setSettings] = useState<SettingsDescribe | null>(null)
  const [credentials, setCredentials] = useState<Awaited<ReturnType<typeof loadProviderCredentials>>>({})

  const [sandbox, setSandbox] = useState(true)
  const [theme, setTheme] = useState('system')
  const [locale, setLocaleState] = useState('zh')
  const [consoleValue, setConsoleValue] = useState<Record<string, unknown>>({})
  const [consoleRevision, setConsoleRevision] = useState(0)
  const [promptText, setPromptText] = useState('')
  const [promptSource, setPromptSource] = useState<'file' | 'default'>('default')

  const load = useCallback(async () => {
    setLoading(true)
    try {
      const [nextDescribe, nextPrompt] = await Promise.all([
        api.getSettings(),
        api.getSystemPrompt(),
      ])
      setSettings(nextDescribe)
      setPromptText(nextPrompt.text)
      setPromptSource(nextPrompt.source)
      setCredentials(await loadProviderCredentials(nextDescribe))

      const console = nextDescribe.namespaces.find((n) => n.ns === CONSOLE_NS)
      if (console) {
        setConsoleValue(console.value)
        setSandbox((console.value.sandbox as boolean) ?? true)
        setTheme((console.value.theme as string) ?? 'system')
        setLocaleState((console.value.locale as string) ?? 'zh')
        setConsoleRevision(console.revision)
      }

    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      setLoading(false)
    }
  }, [notify])

  useEffect(() => {
    void load()
  }, [load])

  useEffect(() => {
    const source = new EventSource('/api/events')
    source.onmessage = (event) => {
      try {
        const payload = JSON.parse(event.data) as { type?: string }
        if (payload.type === 'settings-updated') void load()
      } catch {
        /* ignore malformed frame */
      }
    }
    return () => source.close()
  }, [load])

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  const saveConsole = async () => {
    setSaving(true)
    try {
      const view = await api.replaceNamespace(
        CONSOLE_NS,
        { ...consoleValue, sandbox, theme, locale },
        consoleRevision,
      )
      setConsoleRevision((view as { revision: number }).revision)
      notify('ok', t('settingsSaved'))
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
      void load()
    } finally {
      setSaving(false)
    }
  }

  const savePrompt = async () => {
    setSaving(true)
    try {
      const view = await api.saveSystemPrompt(promptText)
      setPromptText(view.text)
      setPromptSource(view.source)
      notify('ok', t('settingsSaved'))
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
      void load()
    } finally {
      setSaving(false)
    }
  }

  const resetPrompt = async () => {
    setSaving(true)
    try {
      await api.resetSystemPrompt()
      setPromptText('')
      setPromptSource('default')
      notify('ok', t('systemPromptResetDone'))
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
      void load()
    } finally {
      setSaving(false)
    }
  }

  const reloadProviders = async () => {
    try {
      const nextSettings = await api.getSettings()
      setSettings(nextSettings)
      setCredentials(await loadProviderCredentials(nextSettings))
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }

  return (
    <div className="setm-backdrop" onClick={onClose}>
      <div
        className="setm-shell"
        role="dialog"
        aria-modal="true"
        aria-label={t('settingsModalTitle')}
        onClick={(event) => event.stopPropagation()}
      >
        <aside className="setm-rail">
          <div className="setm-rail-head">
            <span className="setm-rail-kicker">Denia</span>
            <h2>{t('settingsModalTitle')}</h2>
            <p>{t('settingsModalSubtitle')}</p>
          </div>

          <nav className="setm-rail-nav" aria-label={t('settingsModalTitle')}>
            {TABS.map((item) => (
              <button
                key={item}
                type="button"
                className={`setm-nav-item${tab === item ? ' active' : ''}`}
                aria-current={tab === item ? 'page' : undefined}
                onClick={() => setTab(item)}
              >
                <span className="setm-nav-icon">{tabIcon(item)}</span>
                <span className="setm-nav-copy">
                  <span className="setm-nav-label">{tabLabel(item)}</span>
                  <span className="setm-nav-desc">{tabNavDesc(item)}</span>
                </span>
              </button>
            ))}
          </nav>

          <button type="button" className="setm-rail-close" onClick={onClose}>
            <IconClose size={15} />
            <span>{t('close')}</span>
          </button>
        </aside>

        <main className="setm-panel">
          <header className="setm-panel-head">
            <div>
              <h3>{paneTitle(tab)}</h3>
              <p>{paneDesc(tab)}</p>
            </div>
            <button type="button" className="setm-panel-close" onClick={onClose} aria-label={t('close')}>
              <IconClose size={18} />
            </button>
          </header>

          <div className="setm-panel-body">
            {loading ? (
              <div className="setm-loading">
                <span className="setm-loading-dot" />
                <p>{t('loading')}</p>
              </div>
            ) : (
              <>
                {tab === 'general' && (
                  <section className="setm-section">
                    {promptSource === 'default' && !promptText && (
                      <p className="setm-callout">{t('systemPromptEmptyHint')}</p>
                    )}
                    <div className="setm-prompt-shell">
                      <textarea
                        className="setm-textarea"
                        rows={14}
                        value={promptText}
                        onChange={(event) => setPromptText(event.target.value)}
                        spellCheck={false}
                        placeholder={t('systemPromptPlaceholder')}
                      />
                    </div>
                    <SetmActions>
                      <SetmBtn disabled={saving} onClick={() => void savePrompt()}>
                        {t('systemPromptSave')}
                      </SetmBtn>
                      <SetmBtn variant="ghost" disabled={saving} onClick={() => void resetPrompt()}>
                        {t('systemPromptReset')}
                      </SetmBtn>
                    </SetmActions>
                  </section>
                )}

                {tab === 'models' && settings && (
                  <ModelProvidersPanel
                    settings={settings}
                    credentials={credentials}
                    notify={notify}
                    onChanged={() => void reloadProviders()}
                  />
                )}

                {tab === 'security' && (
                  <section className="setm-section">
                    <div className="setm-option-row">
                      <div className="setm-option-copy">
                        <div className="setm-option-title">{t('sandboxLabel')}</div>
                        <div className="setm-option-desc">{t('sandboxDesc')}</div>
                      </div>
                      <Toggle checked={sandbox} label="" onChange={setSandbox} />
                    </div>
                    <SetmActions>
                      <SetmBtn disabled={saving} onClick={() => void saveConsole()}>
                        {t('save')}
                      </SetmBtn>
                    </SetmActions>
                  </section>
                )}

                {tab === 'appearance' && (
                  <section className="setm-section">
                    <div className="setm-field-block">
                      <div className="setm-field-label">{t('themeLabel')}</div>
                      <SetmTiles
                        value={theme}
                        options={[
                          { id: 'system', label: t('themeSystem') },
                          { id: 'light', label: t('themeLight') },
                          { id: 'dark', label: t('themeDark') },
                        ]}
                        onChange={setTheme}
                      />
                    </div>
                    <div className="setm-field-block">
                      <div className="setm-field-label">{t('languageLabel')}</div>
                      <SetmTiles
                        value={locale}
                        options={[
                          { id: 'zh', label: '中文' },
                          { id: 'en', label: 'English' },
                        ]}
                        onChange={setLocaleState}
                      />
                    </div>
                    <SetmActions>
                      <SetmBtn disabled={saving} onClick={() => void saveConsole()}>
                        {t('save')}
                      </SetmBtn>
                    </SetmActions>
                  </section>
                )}
              </>
            )}
          </div>
        </main>
      </div>
    </div>
  )
}
