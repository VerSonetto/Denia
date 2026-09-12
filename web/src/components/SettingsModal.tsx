import { McpSettings } from './settings/McpSettings'
import { RuntimeSettings } from './settings/RuntimeSettings'
import { MemorySettings } from './settings/MemorySettings'
import { useCallback, useEffect, useState, type ReactNode } from 'react'
import * as api from '../api'
import { t } from '../i18n'
import type { Notify } from '../App'
import {
  IconChevron,
  IconClose,
  IconGear,
  IconPrompt,
  IconSliders,
  IconStack,
  IconTool,
} from './icons'
import { LlmPanel } from './llm/LlmPanel'
import { normalizePermissionMode, type PermissionMode } from '../types'
import { PERMISSION_LEVELS, storePermission } from './PermissionSelector'

type SettingsTab =
  | 'runtime'
  | 'general'
  | 'prompts'
  | 'models'
  | 'mcp'
  | 'memory'
  | 'appearance'

const CONSOLE_NS = 'console'

const TABS: SettingsTab[] = [
  'general',
  'prompts',
  'models',
  'mcp',
  'memory',
  'runtime',
  'appearance',
]

/** 新建会话默认权限档位:计划模式只在会话内显式进入,不列为默认。 */
const DEFAULT_PERMISSION_OPTIONS: {
  id: PermissionMode
  label: string
  nameKey: Parameters<typeof t>[0]
}[] = PERMISSION_LEVELS.filter((level) => level.level !== 'plan').map((level) => ({
  id: level.level,
  label: t(level.nameKey),
  nameKey: level.nameKey,
}))

function normalizeDefaultPermission(raw: unknown): PermissionMode {
  const mode = typeof raw === 'string' ? normalizePermissionMode(raw) : 'auto-edit'
  return mode === 'plan' ? 'auto-edit' : mode
}

function tabIcon(tab: SettingsTab, size = 16) {
  switch (tab) {
    case 'runtime':
      return <IconGear size={size} />
    case 'general':
      return <IconStack size={size} />
    case 'prompts':
      return <IconPrompt size={size} />
    case 'models':
      return <IconSliders size={size} />
    case 'mcp':
      return <IconTool size={size} />
    case 'memory':
      return <IconStack size={size} />
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
    case 'prompts':
      return t('settingsTabPrompts')
    case 'models':
      return t('settingsTabModels')
    case 'mcp':
      return t('mcpTab')
    case 'memory':
      return t('memoryTab')
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
    case 'prompts':
      return t('settingsTabPromptsDesc')
    case 'models':
      return t('settingsTabModelsDesc')
    case 'mcp':
      return t('mcpTabDesc')
    case 'memory':
      return t('memoryTabDesc')
    case 'appearance':
      return t('settingsTabAppearanceDesc')
  }
}

function paneTitle(tab: SettingsTab): string {
  switch (tab) {
    case 'runtime':
      return t('runtimeSettings')
    case 'general':
      return t('settingsPaneGeneralTitle')
    case 'prompts':
      return t('settingsPanePromptsTitle')
    case 'models':
      return t('settingsPaneModelsTitle')
    case 'mcp':
      return t('mcpPaneTitle')
    case 'memory':
      return t('memoryPaneTitle')
    case 'appearance':
      return t('catAppearance')
  }
}

function paneDesc(tab: SettingsTab): string {
  switch (tab) {
    case 'runtime':
      return t('runtimeSettingsHint')
    case 'general':
      return t('settingsPaneGeneralDesc')
    case 'prompts':
      return t('settingsPanePromptsDesc')
    case 'models':
      return t('settingsPaneModelsDesc')
    case 'mcp':
      return t('mcpPaneDesc')
    case 'memory':
      return t('memoryPaneDesc')
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

/**
 * 面板内的卡片(标题 + 可选说明 + 可折叠正文)。
 *
 * 一个 pane 里可能堆多块内容(系统提示词、全局规则…),默认折叠只留
 * 标题行 + 状态摘要,展开才占面积 —— 块多了也不会把面板顶成长条。
 * 后续加内容:再套一层 `SetmCard`,样式与节奏自动对齐。
 */
function SetmCard({
  title,
  hint,
  summary,
  open,
  onToggle,
  children,
}: {
  title: string
  hint?: string
  /** 折叠时标题行右侧的一句话状态(如"未设置"/"已自定义")。 */
  summary?: string
  open: boolean
  onToggle: () => void
  children: ReactNode
}) {
  const bodyId = `setm-card-${title}`
  return (
    <section className={`setm-card${open ? ' open' : ''}`}>
      <button
        type="button"
        className="setm-card-head"
        aria-expanded={open}
        aria-controls={bodyId}
        onClick={onToggle}
      >
        <span className="setm-card-copy">
          <span className="setm-card-title">{title}</span>
          {hint && !open && <span className="setm-card-hint">{hint}</span>}
        </span>
        {summary && !open && <span className="setm-card-summary">{summary}</span>}
        <span className="setm-card-chevron">
          <IconChevron size={13} />
        </span>
      </button>
      {open && (
        <div className="setm-card-body" id={bodyId}>
          {hint && <p className="setm-card-desc">{hint}</p>}
          {children}
        </div>
      )}
    </section>
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
  const [defaultPermission, setDefaultPermission] = useState<PermissionMode>('auto-edit')
  const [consoleValue, setConsoleValue] = useState<Record<string, unknown>>({})
  const [consoleRevision, setConsoleRevision] = useState(0)
  const [promptText, setPromptText] = useState('')
  const [promptSource, setPromptSource] = useState<'file' | 'default'>('default')
  const [rulesText, setRulesText] = useState('')
  const [rulesSource, setRulesSource] = useState<'file' | 'default'>('default')
  const [rulesPath, setRulesPath] = useState('')
  // 卡片默认折叠:pane 只留标题行 + 状态,展开才占面积。
  const [openCard, setOpenCard] = useState<string | null>(null)
  const toggleCard = (id: string) => setOpenCard((current) => (current === id ? null : id))

  const load = useCallback(async () => {
    setLoading(true)
    try {
      const [nextDescribe, nextPrompt, nextRules] = await Promise.all([
        api.getSettings(),
        api.getSystemPrompt(),
        api.getGlobalRules(),
      ])
      setPromptText(nextPrompt.text)
      setPromptSource(nextPrompt.source)
      setRulesText(nextRules.text)
      setRulesSource(nextRules.source)
      setRulesPath(nextRules.displayPath)

      const console = nextDescribe.namespaces.find((n) => n.ns === CONSOLE_NS)
      if (console) {
        setConsoleValue(console.value)
        setTheme((console.value.theme as string) ?? 'system')
        setLocaleState((console.value.locale as string) ?? 'zh')
        setDefaultPermission(
          normalizeDefaultPermission(console.value.defaultPermissionMode),
        )
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

  /** 默认权限档位:写 console 设置,并同步本地草稿态档位记忆。 */
  const saveDefaultPermission = async () => {
    setSaving(true)
    try {
      const view = await api.replaceNamespace(
        CONSOLE_NS,
        { ...consoleValue, theme, locale, defaultPermissionMode: defaultPermission },
        consoleRevision,
      )
      setConsoleRevision((view as { revision: number }).revision)
      setConsoleValue((current) => ({
        ...current,
        defaultPermissionMode: defaultPermission,
      }))
      storePermission(defaultPermission)
      notify('ok', t('defaultPermissionSaved'))
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

  /** 全局规则:空正文 = 删除文件回到"未设置"(后端幂等)。 */
  const saveRules = async () => {
    setSaving(true)
    try {
      const view = await api.saveGlobalRules(rulesText)
      setRulesText(view.text)
      setRulesSource(view.source)
      setRulesPath(view.displayPath)
      notify('ok', rulesText.trim() ? t('globalRulesSaved') : t('globalRulesCleared'))
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
      void load()
    } finally {
      setSaving(false)
    }
  }

  const clearRules = async () => {
    setSaving(true)
    try {
      const view = await api.saveGlobalRules('')
      setRulesText('')
      setRulesSource('default')
      setRulesPath(view.displayPath)
      notify('ok', t('globalRulesCleared'))
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
                  <SetmCard
                    title={t('defaultPermissionTitle')}
                    hint={t('defaultPermissionHint')}
                    summary={t(
                      (DEFAULT_PERMISSION_OPTIONS.find((o) => o.id === defaultPermission) ??
                        DEFAULT_PERMISSION_OPTIONS[1]).nameKey,
                    )}
                    open={openCard === 'permission'}
                    onToggle={() => toggleCard('permission')}
                  >
                    <SetmTiles
                      value={defaultPermission}
                      options={DEFAULT_PERMISSION_OPTIONS}
                      onChange={setDefaultPermission}
                    />
                    <SetmActions>
                      <SetmBtn disabled={saving} onClick={() => void saveDefaultPermission()}>
                        {t('save')}
                      </SetmBtn>
                    </SetmActions>
                  </SetmCard>
                )}

                {tab === 'prompts' && (
                  <div className="setm-cards">
                    <SetmCard
                      title={t('systemPromptTitle')}
                      hint={t('systemPromptHint')}
                      summary={promptSource === 'file' ? t('customized') : t('factoryDefault')}
                      open={openCard === 'prompt'}
                      onToggle={() => toggleCard('prompt')}
                    >
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
                        <SetmBtn
                          variant="ghost"
                          disabled={saving}
                          onClick={() => void resetPrompt()}
                        >
                          {t('systemPromptReset')}
                        </SetmBtn>
                      </SetmActions>
                    </SetmCard>

                    <SetmCard
                      title={t('globalRulesTitle')}
                      hint={t('globalRulesHint')}
                      summary={rulesSource === 'file' ? t('customized') : t('notSet')}
                      open={openCard === 'rules'}
                      onToggle={() => toggleCard('rules')}
                    >
                      <p className="setm-card-meta">
                        <span>{t('globalRulesPathLabel')}</span>
                        <code>{rulesPath || '~/AGENTS.md'}</code>
                      </p>
                      {rulesSource === 'default' && !rulesText && (
                        <p className="setm-callout">{t('globalRulesEmptyHint')}</p>
                      )}
                      <div className="setm-prompt-shell">
                        <textarea
                          className="setm-textarea"
                          rows={10}
                          value={rulesText}
                          onChange={(event) => setRulesText(event.target.value)}
                          spellCheck={false}
                          placeholder={t('globalRulesPlaceholder')}
                        />
                      </div>
                      <SetmActions>
                        <SetmBtn disabled={saving} onClick={() => void saveRules()}>
                          {t('globalRulesSave')}
                        </SetmBtn>
                        <SetmBtn variant="ghost" disabled={saving} onClick={() => void clearRules()}>
                          {t('globalRulesClear')}
                        </SetmBtn>
                      </SetmActions>
                    </SetmCard>
                  </div>
                )}

                {tab === 'models' && <LlmPanel notify={notify} />}

                {tab === 'mcp' && <McpSettings notify={notify} />}

                {tab === 'memory' && <MemorySettings />}

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
