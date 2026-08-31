import { useCallback, useEffect, useState } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import type { Notify } from '../App'
import { ModelPicker } from '../components/ModelPicker'
import type { ModelCatalog, ModelSelection } from '../types'

type Category = 'general' | 'security' | 'appearance'

const CONSOLE_NS = 'console'
const MODEL_NS = 'agent-default-model'

export default function SettingsPage({ notify }: { notify: Notify }) {
  const [category, setCategory] = useState<Category>('general')
  const [catalog, setCatalog] = useState<ModelCatalog | null>(null)

  const [sandbox, setSandbox] = useState(true)
  const [theme, setTheme] = useState('system')
  const [locale, setLocaleState] = useState('zh')
  const [consoleValue, setConsoleValue] = useState<Record<string, unknown>>({})
  const [modelDraft, setModelDraft] = useState<ModelSelection | null>(null)
  const [consoleRevision, setConsoleRevision] = useState(0)
  const [modelRevision, setModelRevision] = useState(0)
  const [saving, setSaving] = useState(false)

  const load = useCallback(async () => {
    try {
      const [nextDescribe, nextCatalog] = await Promise.all([
        api.getSettings(),
        api.getCatalog(),
      ])
      setCatalog(nextCatalog)
      const console = nextDescribe.namespaces.find((n) => n.ns === CONSOLE_NS)
      if (console) {
        setConsoleValue(console.value)
        setSandbox((console.value.sandbox as boolean) ?? true)
        setTheme((console.value.theme as string) ?? 'system')
        setLocaleState((console.value.locale as string) ?? 'zh')
        setConsoleRevision(console.revision)
      }
      const model = nextDescribe.namespaces.find((n) => n.ns === MODEL_NS)
      if (model) {
        setModelDraft({
          provider: (model.value.provider as string) ?? nextCatalog.default.provider,
          model: (model.value.model as string) ?? nextCatalog.default.model,
          reasoningEffort: (model.value.reasoningEffort as string) ?? undefined,
        })
        setModelRevision(model.revision)
      } else {
        setModelDraft(nextCatalog.default)
      }
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }, [notify])

  useEffect(() => {
    void load()
  }, [load])

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

  const saveModel = async () => {
    if (!modelDraft) return
    setSaving(true)
    try {
      const section: Record<string, unknown> = {
        provider: modelDraft.provider,
        model: modelDraft.model,
      }
      if (modelDraft.reasoningEffort) {
        section.reasoningEffort = modelDraft.reasoningEffort
      }
      const view = await api.replaceNamespace(MODEL_NS, section, modelRevision)
      setModelRevision((view as { revision: number }).revision)
      notify('ok', t('settingsSaved'))
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
      void load()
    } finally {
      setSaving(false)
    }
  }

  const categories: { id: Category; label: string }[] = [
    { id: 'general', label: t('catGeneral') },
    { id: 'security', label: t('catSecurity') },
    { id: 'appearance', label: t('catAppearance') },
  ]

  return (
    <div className="settings-main">
      <div className="settings-inner">
        <div className="settings-rail">
          {categories.map((item) => (
            <button
              key={item.id}
              className={`nav-row${category === item.id ? ' active' : ''}`}
              onClick={() => setCategory(item.id)}
            >
              {item.label}
            </button>
          ))}
        </div>
        <div className="settings-content">
          {category === 'general' && (
            <section className="card">
              <h2>{t('defaultModelLabel')}</h2>
              <p className="hint">{t('defaultModelHint')}</p>
              {catalog && modelDraft && (
                <>
                  <ModelPicker catalog={catalog} value={modelDraft} onChange={setModelDraft} />
                  <div className="row" style={{ marginTop: 12 }}>
                    <button className="btn" disabled={saving} onClick={() => void saveModel()}>
                      {t('save')}
                    </button>
                  </div>
                </>
              )}
            </section>
          )}
          {category === 'security' && (
            <section className="card">
              <h2>{t('catSecurity')}</h2>
              <p className="hint">{t('sandboxDesc')}</p>
              <div className="row">
                <label className="check-row">
                  <input
                    type="checkbox"
                    checked={sandbox}
                    onChange={(event) => setSandbox(event.target.checked)}
                  />
                  {t('sandboxLabel')}
                </label>
                <button className="btn" disabled={saving} onClick={() => void saveConsole()}>
                  {t('save')}
                </button>
              </div>
            </section>
          )}
          {category === 'appearance' && (
            <section className="card">
              <h2>{t('catAppearance')}</h2>
              <p className="hint">{t('themeLabel')}</p>
              <div className="row">
                <div className="field narrow">
                  <label>{t('themeLabel')}</label>
                  <select
                    className="plain"
                    value={theme}
                    onChange={(event) => setTheme(event.target.value)}
                  >
                    <option value="system">{t('themeSystem')}</option>
                    <option value="light">{t('themeLight')}</option>
                    <option value="dark">{t('themeDark')}</option>
                  </select>
                </div>
                <div className="field narrow">
                  <label>{t('languageLabel')}</label>
                  <select
                    className="plain"
                    value={locale}
                    onChange={(event) => setLocaleState(event.target.value)}
                  >
                    <option value="zh">中文</option>
                    <option value="en">English</option>
                  </select>
                </div>
                <button className="btn" disabled={saving} onClick={() => void saveConsole()}>
                  {t('save')}
                </button>
              </div>
            </section>
          )}
        </div>
      </div>
    </div>
  )
}
