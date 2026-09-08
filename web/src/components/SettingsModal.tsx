import { RuntimeSettings } from './settings/RuntimeSettings'
import { useCallback, useEffect, useState, type ReactNode } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import type { Notify } from '../App'
import { IconClose, IconGear, IconPrompt, IconSliders } from './icons'
import { LlmPanel } from './llm/LlmPanel'

type SettingsTab = 'runtime' | 'general' | 'models' | 'appearance'

const CONSOLE_NS = 'console'

const TABS: SettingsTab[] = ['general', 'models', 'runtime', 'appearance']

function tabIcon(tab: SettingsTab, size = 16) {
  switch (tab) {
    case 'runtime':
      return <IconGear size={size} />
    case 'general':
      return <IconPrompt size={size} />
    case 'models':
      return <IconSliders size={size} />
    case 'appearance':
      return <IconGear size={size} />
  }
}

function tabLabel(tab: SettingsTab): string {
  switch (tab) {
    case 'runtime':
      return t('runtimeSettings')
    case 'general':
      return t('settingsTabGeneral')
    case 'models':
      return t('settingsTabModels')
    case 'appearance':
      return t('settingsTabAppearance')
  }
}

function tabNavDesc(tab: SettingsTab): string {
  switch (tab) {
    case 'runtime':
      return t('runtimeSettingsHint')
    case 'general':
      return t('settingsTabGeneralDesc')
    case 'models':
      return t('settingsTabModelsDesc')
    case 'appearance':
      return t('settingsTabAppearanceDesc')
  }
}

function paneTitle(tab: SettingsTab): string {
  switch (tab) {
    case 'runtime':
      return t('runtimeSettings')
    case 'general':
      return t('systemPromptTitle')
    case 'models':
      return t('settingsPaneModelsTitle')
    case 'appearance':
      return t('catAppearance')
  }
}

function paneDesc(tab: SettingsTab): string {
  switch (tab) {
    case 'runtime':
      return t('runtimeSettingsHint')
    case 'general':
      return t('systemPromptHint')
    case 'models':
      return t('settingsPaneModelsDesc')
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
      setPromptText(nextPrompt.text)
      setPromptSource(nextPrompt.source)

      const console = nextDescribe.namespaces.find((n) => n.ns === CONSOLE_NS)
      if (console) {
        setConsoleValue(console.value)
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

  // 弹窗期间锁定 body 滚动并补偿滚动条宽度:遮罩下不再出现滚动条抖动。
  useEffect(() => {
    const body = document.body
    const scrollbar = window.innerWidth - document.documentElement.clientWidth
    const prevOverflow = body.style.overflow
    const prevPadding = body.style.paddingRight
    body.style.overflow = 'hidden'
    if (scrollbar > 0) body.style.paddingRight = `${scrollbar}px`
    return () => {
      body.style.overflow = prevOverflow
      body.style.paddingRight = prevPadding
    }
  }, [])

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      // 内层抽屉/确认框已 preventDefault 的 Esc 不连弹窗一起关。
      if (event.key === 'Escape' && !event.defaultPrevented) onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])

  const saveConsole = async () => {
    setSaving(true)
    try {
      const view = await api.replaceNamespace(
        CONSOLE_NS,
        { ...consoleValue, theme, locale },
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

  return (
    <div className="setm-backdrop">
      <div
        className="setm-shell"
        role="dialog"
        aria-modal="true"
        aria-label={t('settingsModalTitle')}
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
                {tab === 'runtime' && <RuntimeSettings />}
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

                {tab === 'models' && <LlmPanel notify={notify} />}

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
