import { useCallback, useEffect, useState } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import type { Notify } from '../App'
import { ModelPicker } from '../components/ModelPicker'
import { SegmentControl, Toggle } from '../components/ui/controls'
import { resolveSessionReasoningEffort } from '../modelCatalog'
import type { ModelCatalog, ModelSelection } from '../types'

function normalizeModelDraft(catalog: ModelCatalog, draft: ModelSelection): ModelSelection {
  const group = catalog.groups.find((entry) => entry.id === draft.provider)
  const model = group?.models.find((entry) => entry.id === draft.model)
  return {
    ...draft,
    reasoningEffort: resolveSessionReasoningEffort(model?.reasoning?.efforts ?? [], draft.reasoningEffort),
  }
}

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
  const [promptText, setPromptText] = useState('')
  const [promptSource, setPromptSource] = useState<'file' | 'default'>('default')

  const load = useCallback(async () => {
    try {
      const [nextDescribe, nextCatalog, nextPrompt] = await Promise.all([
        api.getSettings(),
        api.getCatalog(),
        api.getSystemPrompt(),
      ])
      setPromptText(nextPrompt.text)
      setPromptSource(nextPrompt.source)
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
        setModelDraft(
          normalizeModelDraft(nextCatalog, {
            provider: (model.value.provider as string) ?? nextCatalog.default.provider,
            model: (model.value.model as string) ?? nextCatalog.default.model,
            reasoningEffort: (model.value.reasoningEffort as string) ?? undefined,
          }),
        )
        setModelRevision(model.revision)
      } else {
        setModelDraft(normalizeModelDraft(nextCatalog, nextCatalog.default))
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
            <>
              <section className="card">
                <h2>{t('systemPromptTitle')}</h2>
                <p className="hint">{t('systemPromptHint')}</p>
                {promptSource === 'default' && !promptText && (
                  <p className="hint">{t('systemPromptEmptyHint')}</p>
                )}
                <textarea
                  className="plain"
                  rows={14}
                  value={promptText}
                  onChange={(event) => setPromptText(event.target.value)}
                  spellCheck={false}
                />
                <div className="row" style={{ marginTop: 12 }}>
                  <button className="btn" disabled={saving} onClick={() => void savePrompt()}>
                    {t('systemPromptSave')}
                  </button>
                  <button className="btn secondary" disabled={saving} onClick={() => void resetPrompt()}>
                    {t('systemPromptReset')}
                  </button>
                </div>
              </section>
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
            </>
          )}
          {category === 'security' && (
            <section className="card">
              <h2>{t('catSecurity')}</h2>
              <p className="hint">{t('sandboxDesc')}</p>
              <div className="row">
                <Toggle
                  checked={sandbox}
                  label={t('sandboxLabel')}
                  onChange={setSandbox}
                />
                <button className="btn" disabled={saving} onClick={() => void saveConsole()}>
                  {t('save')}
                </button>
              </div>
            </section>
          )}
          {category === 'appearance' && (
            <section className="card">
              <h2>{t('catAppearance')}</h2>
              <div className="row" style={{ alignItems: 'flex-end' }}>
                <div className="field narrow">
                  <label>{t('themeLabel')}</label>
                  <SegmentControl
                    value={theme}
                    options={[
                      { id: 'system', label: t('themeSystem') },
                      { id: 'light', label: t('themeLight') },
                      { id: 'dark', label: t('themeDark') },
                    ]}
                    onChange={setTheme}
                  />
                </div>
                <div className="field narrow">
                  <label>{t('languageLabel')}</label>
                  <SegmentControl
                    value={locale}
                    options={[
                      { id: 'zh', label: '中文' },
                      { id: 'en', label: 'English' },
                    ]}
                    onChange={setLocaleState}
                  />
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
