import { useCallback, useEffect, useRef, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import type { TranslationKey } from '../../i18n'
import { IconCheck, IconClose, IconPencil, IconPlus, IconTrash } from '../icons'
import type { PresetFeatures, PresetSpec } from '../../types'
import { ChipRadio, Field, TextInput } from '../llm/atoms/form'
import { SetmActions, SetmBtn } from './setm'
import styles from './AgentPresetSettings.module.css'

/** 默认 preset 所在的设置命名空间(与服务端 `agent_presets::SETTINGS_NS` 同名)。 */
const NS = 'agent-presets'

/** 预设 id 的合法形状(与服务端校验一致):小写字母/数字开头,其后同类字符加连字符。 */
const PRESET_ID_RE = /^[a-z0-9][a-z0-9-]*$/

type Notify = (kind: 'ok' | 'err', text: string) => void

/** 功能开关的展示顺序与短名(关闭项会在卡片上列出)。 */
const FEATURE_KEYS: { key: keyof PresetFeatures; labelKey: TranslationKey }[] = [
  { key: 'agentsMd', labelKey: 'agentPresetFeatureAgentsMd' },
  { key: 'memory', labelKey: 'agentPresetFeatureMemory' },
  { key: 'compaction', labelKey: 'agentPresetFeatureCompaction' },
  { key: 'goal', labelKey: 'agentPresetFeatureGoal' },
  { key: 'taskLedger', labelKey: 'agentPresetFeatureTaskLedger' },
  { key: 'skills', labelKey: 'agentPresetFeatureSkills' },
  { key: 'subagents', labelKey: 'agentPresetFeatureSubagents' },
  { key: 'jobs', labelKey: 'agentPresetFeatureJobs' },
  { key: 'browser', labelKey: 'agentPresetFeatureBrowser' },
  { key: 'ask', labelKey: 'agentPresetFeatureAsk' },
  { key: 'planMode', labelKey: 'agentPresetFeaturePlanMode' },
]

/** 全部开启的功能集合(服务端语义:省略的键 = 开启)。 */
function allFeatures(): PresetFeatures {
  return {
    agentsMd: true,
    memory: true,
    compaction: true,
    goal: true,
    taskLedger: true,
    skills: true,
    subagents: true,
    jobs: true,
    browser: true,
    ask: true,
    planMode: true,
  }
}

function withFeature(features: PresetFeatures, key: keyof PresetFeatures, on: boolean): PresetFeatures {
  return { ...features, [key]: on }
}

/** 一个 preset 关闭的功能短名列表(服务端快照缺键时按开启处理)。 */
function disabledFeatureLabels(features: PresetFeatures | undefined): string[] {
  if (!features) return []
  return FEATURE_KEYS.filter(({ key }) => features[key] === false).map(({ labelKey }) =>
    t(labelKey),
  )
}

/** 编辑器草稿:白名单模式下 `tools` 就是完整名单(勾选项与追加项共用一份,天然去重)。 */
interface PresetDraft {
  id: string
  name: string
  description: string
  persona: string
  personaComplete: boolean
  toolsMode: 'all' | 'allowlist'
  tools: string[]
  features: PresetFeatures
}

function blankDraft(): PresetDraft {
  return {
    id: '',
    name: '',
    description: '',
    persona: '',
    personaComplete: false,
    toolsMode: 'all',
    tools: [],
    features: allFeatures(),
  }
}

/** 服务端 spec → 草稿(`tools` 缺省 = 全量工具,空数组 = 空白名单)。 */
function draftFromSpec(spec: PresetSpec): PresetDraft {
  return {
    id: spec.id,
    name: spec.name ?? '',
    description: spec.description ?? '',
    persona: spec.persona ?? '',
    personaComplete: spec.personaComplete === true,
    toolsMode: spec.tools ? 'allowlist' : 'all',
    tools: spec.tools ?? [],
    features: { ...allFeatures(), ...spec.features },
  }
}

interface EditorState {
  mode: 'create' | 'edit'
  /** 编辑目标 id(新建时为空)。 */
  target: string
  draft: PresetDraft
  /** 每次打开自增:驱动"打开即滚入视野",也用来重建编辑器的本地状态。 */
  seq: number
}

/**
 * Agent 预设(preset)设置分区:名册、默认值、新建/编辑/删除与只读查看。
 *
 * 自定义预设除了直接改 `preset.yml`,也可以在这里由内置编辑器新建与覆盖
 * 编辑(服务端按 `PresetSpec` 校验);随附预设只读。文件变化由服务端监听并
 * 广播,这里收到 `agent-presets-updated` 后重拉。
 */
export function AgentPresetSettings({ notify }: { notify: Notify }) {
  const [roster, setRoster] = useState<api.AgentPresetsView | null>(null)
  const [revision, setRevision] = useState(0)
  const [defaultId, setDefaultId] = useState('')
  const [modeSelection, setModeSelection] = useState<boolean | null>(null)
  const [busy, setBusy] = useState(false)
  const [saving, setSaving] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null)
  const [copyFrom, setCopyFrom] = useState<api.AgentPresetRow | null>(null)
  const [copyId, setCopyId] = useState('')
  const [copyName, setCopyName] = useState('')
  const [viewer, setViewer] = useState<{ id: string; text: string } | null>(null)
  const [editor, setEditor] = useState<EditorState | null>(null)
  const [editorError, setEditorError] = useState('')
  const editorHost = useRef<HTMLDivElement | null>(null)
  /** 编辑器打开次数:既是渲染 key,也是"打开即滚入视野"的触发值,单调递增。 */
  const editorSeq = useRef(0)

  const load = useCallback(async () => {
    try {
      const [views, settings] = await Promise.all([api.listAgentPresets(), api.getSettings()])
      setRoster(views)
      const section = settings.namespaces.find((ns) => ns.ns === NS)
      if (section) {
        setRevision(section.revision)
        setDefaultId(typeof section.value.default === 'string' ? section.value.default : views.default)
        const stored = section.value.modeSelectionEnabled
        setModeSelection(typeof stored === 'boolean' ? stored : views.modeSelection)
      } else {
        setDefaultId(views.default)
        setModeSelection(views.modeSelection)
      }
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }, [notify])

  useEffect(() => {
    void load()
  }, [load])

  useEffect(() => {
    return api.subscribeEvents((type) => {
      if (type === 'agent-presets-updated') void load()
    })
  }, [load])

  // 编辑器在列表上方:打开时滚进视野,避免"点了编辑却没反应"的错觉。
  useEffect(() => {
    if (editor) editorHost.current?.scrollIntoView({ block: 'nearest', behavior: 'smooth' })
  }, [editor?.seq])

  const setDefault = async (id: string) => {
    setBusy(true)
    try {
      await api.updateNamespace(NS, { default: id }, revision)
      setDefaultId(id)
      notify('ok', t('agentPresetsDefaultSaved'))
      await load()
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      setBusy(false)
    }
  }

  const toggleModeSelection = async (enabled: boolean) => {
    setBusy(true)
    try {
      await api.updateNamespace(NS, { modeSelectionEnabled: enabled }, revision)
      setModeSelection(enabled)
      notify('ok', t('agentPresetsModeSelectionSaved'))
      await load()
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      setBusy(false)
    }
  }

  const copyPath = async (path: string) => {
    try {
      await navigator.clipboard.writeText(path)
      notify('ok', t('agentPresetsPathCopied'))
    } catch {
      notify('err', t('agentPresetsPathCopyFailed'))
    }
  }

  /** 编辑器、复制表单与只读查看器三者互斥:同一时刻只开一个。 */
  const openEditor = (next: Omit<EditorState, 'seq'>) => {
    setCopyFrom(null)
    setViewer(null)
    setEditorError('')
    editorSeq.current += 1
    setEditor({ ...next, seq: editorSeq.current })
  }

  const startCreate = () => {
    openEditor({ mode: 'create', target: '', draft: blankDraft() })
  }

  const startEdit = async (preset: api.AgentPresetRow) => {
    setBusy(true)
    try {
      const { spec } = await api.getAgentPreset(preset.id)
      if (!spec) {
        notify('err', t('agentPresetsSpecMissing'))
        return
      }
      openEditor({ mode: 'edit', target: preset.id, draft: draftFromSpec(spec) })
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      setBusy(false)
    }
  }

  const closeEditor = () => {
    setEditor(null)
    setEditorError('')
  }

  const patchDraft = (patch: Partial<PresetDraft>) => {
    setEditor((current) =>
      current ? { ...current, draft: { ...current.draft, ...patch } } : current,
    )
  }

  const submitEditor = async () => {
    if (!editor) return
    const { draft } = editor
    const id = draft.id.trim()
    const name = draft.name.trim()
    if (!name || (editor.mode === 'create' && !PRESET_ID_RE.test(id))) return
    // 空白名单服务端直接拒绝(它不表达"全量",那是切回全量模式的事):
    // 在提交前拦住,不让用户吃一次无意义的 400。
    if (draft.toolsMode === 'allowlist' && draft.tools.length === 0) return
    const spec: PresetSpec = {
      id,
      name,
      description: draft.description.trim(),
      persona: draft.persona.trim() ? draft.persona : undefined,
      personaComplete: draft.personaComplete,
      tools: draft.toolsMode === 'allowlist' ? draft.tools : undefined,
      features: draft.features,
    }
    setSaving(true)
    setEditorError('')
    try {
      if (editor.mode === 'create') {
        await api.createAgentPreset(spec)
        notify('ok', t('agentPresetsCreated', { id }))
      } else {
        await api.updateAgentPreset(editor.target, spec)
        notify('ok', t('agentPresetsSaved', { id: editor.target }))
      }
      setEditor(null)
      await load()
    } catch (error) {
      // 服务端错误 message 可直接展示(非法 id / 未知工具名 / id 被占用)。
      setEditorError(error instanceof Error ? error.message : String(error))
    } finally {
      setSaving(false)
    }
  }

  const startCopy = (preset: api.AgentPresetRow) => {
    setEditor(null)
    setEditorError('')
    setViewer(null)
    setCopyFrom(preset)
    setCopyId('')
    setCopyName('')
  }

  const submitCopy = async () => {
    if (!copyFrom || !copyId.trim()) return
    setBusy(true)
    try {
      const name = copyName.trim()
      const { preset } = await api.copyAgentPreset({
        from: copyFrom.id,
        id: copyId.trim(),
        name: name || undefined,
      })
      notify('ok', t('agentPresetsCopied', { id: preset.id }))
      setCopyFrom(null)
      await load()
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      setBusy(false)
    }
  }

  const remove = async (id: string) => {
    setBusy(true)
    try {
      await api.deleteAgentPreset(id)
      notify('ok', t('agentPresetsDeleted', { id }))
      setConfirmDelete(null)
      await load()
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      setBusy(false)
    }
  }

  const openViewer = async (id: string) => {
    try {
      const { text } = await api.getAgentPreset(id)
      setEditor(null)
      setEditorError('')
      setCopyFrom(null)
      setViewer({ id, text })
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }

  const presets = roster?.presets ?? []
  const knownTools = roster?.tools ?? []

  return (
    <div className={styles.wrap}>
      <p className={styles.hint}>{t('agentPresetsHint')}</p>
      <div className={styles.toggleRow}>
        <label className={styles.toggle}>
          <input
            type="checkbox"
            checked={modeSelection ?? true}
            disabled={busy || modeSelection === null}
            onChange={(event) => void toggleModeSelection(event.target.checked)}
          />
          <span className={styles.toggleLabel}>{t('agentPresetsModeSelection')}</span>
        </label>
      </div>
      <p className={styles.hint}>{t('agentPresetsModeSelectionHint')}</p>
      {roster && roster.root && (
        <div className={styles.root}>
          <span className={styles.rootLabel}>{t('agentPresetsRoot')}</span>
          <code>{roster.root}</code>
        </div>
      )}

      <div className={styles.listHead}>
        <button
          type="button"
          className={styles.primary}
          disabled={busy || editor != null}
          onClick={startCreate}
        >
          <IconPlus size={13} />
          {t('agentPresetsNew')}
        </button>
      </div>

      {editor && (
        <div ref={editorHost}>
          <PresetEditor
            key={editor.seq}
            mode={editor.mode}
            draft={editor.draft}
            knownTools={knownTools}
            saving={saving}
            error={editorError}
            onChange={patchDraft}
            onSave={() => void submitEditor()}
            onClose={closeEditor}
          />
        </div>
      )}

      {presets.length === 0 && <div className={styles.empty}>{t('agentPresetsLoading')}</div>}

      <ul className={styles.list}>
        {presets.map((preset) => {
          const selected = preset.id === defaultId
          const offFeatures = disabledFeatureLabels(preset.features)
          return (
            <li
              key={`${preset.trust}:${preset.id}`}
              className={`${styles.row}${preset.broken ? ` ${styles.rowBroken}` : ''}`}
            >
              <div className={styles.rowMain}>
                <div className={styles.rowTitle}>
                  <span className={styles.name}>{preset.name}</span>
                  <span className={styles.badge}>
                    {preset.trust === 'shipped' ? t('agentPresetsShipped') : t('agentPresetsUser')}
                  </span>
                  {selected && (
                    <span className={styles.badgeDefault}>
                      <IconCheck size={11} />
                      {t('agentPresetDefaultBadge')}
                    </span>
                  )}
                  {preset.broken && (
                    <span className={styles.badgeBroken}>{t('agentPresetsBrokenBadge')}</span>
                  )}
                </div>
                <div className={styles.desc}>
                  {preset.broken ?? preset.description}
                </div>
                <div className={styles.meta}>
                  <span>
                    {preset.tools
                      ? t('agentPresetsToolsCount', { n: preset.tools.length })
                      : t('agentPresetsToolsAll')}
                  </span>
                  {preset.tools && preset.tools.length > 0 && (
                    <span className={styles.tools}>{preset.tools.join(' · ')}</span>
                  )}
                  {offFeatures.length > 0 && (
                    <span className={styles.featuresOff}>
                      {t('agentPresetsFeaturesOff', {
                        list: offFeatures.join(t('agentPresetsJoiner')),
                      })}
                    </span>
                  )}
                </div>
                {preset.writable && preset.path && (
                  <button
                    type="button"
                    className={styles.pathRow}
                    title={t('agentPresetsPathCopy')}
                    onClick={() => void copyPath(preset.path!)}
                  >
                    <code>{preset.path}</code>
                  </button>
                )}
              </div>
              <div className={styles.actions}>
                {!preset.broken && !selected && (
                  <button
                    type="button"
                    className={styles.btn}
                    disabled={busy}
                    onClick={() => void setDefault(preset.id)}
                  >
                    <IconCheck size={13} />
                    {t('agentPresetsSetDefault')}
                  </button>
                )}
                {!preset.broken && preset.writable && (
                  <button
                    type="button"
                    className={styles.btn}
                    disabled={busy || editor != null}
                    onClick={() => void startEdit(preset)}
                  >
                    <IconPencil size={13} />
                    {t('agentPresetsEdit')}
                  </button>
                )}
                <button
                  type="button"
                  className={styles.btn}
                  onClick={() => void openViewer(preset.id)}
                >
                  {t('agentPresetsView')}
                </button>
                <button
                  type="button"
                  className={styles.btn}
                  disabled={busy}
                  onClick={() => startCopy(preset)}
                >
                  <IconPlus size={13} />
                  {t('agentPresetsCopy')}
                </button>
                {preset.writable &&
                  (confirmDelete === preset.id ? (
                    <span className={styles.confirm}>
                      <button
                        type="button"
                        className={styles.danger}
                        disabled={busy}
                        onClick={() => void remove(preset.id)}
                      >
                        {t('agentPresetsDelete')}
                      </button>
                      <button
                        type="button"
                        className={styles.btn}
                        onClick={() => setConfirmDelete(null)}
                      >
                        {t('agentPresetsCancel')}
                      </button>
                    </span>
                  ) : (
                    <button
                      type="button"
                      className={styles.btnDanger}
                      disabled={busy}
                      onClick={() => setConfirmDelete(preset.id)}
                    >
                      <IconTrash size={13} />
                    </button>
                  ))}
              </div>
              {confirmDelete === preset.id && (
                <div className={styles.confirmHint}>
                  {t('agentPresetsDeleteConfirm', { name: preset.name })}
                </div>
              )}
            </li>
          )
        })}
      </ul>

      {copyFrom && (
        <div className={styles.form}>
          <div className={styles.formTitle}>{t('agentPresetsCopyTitle')}</div>
          <label className={styles.field}>
            <span>{t('agentPresetsCopyFrom')}</span>
            <input value={`${copyFrom.name} (${copyFrom.id})`} readOnly />
          </label>
          <label className={styles.field}>
            <span>{t('agentPresetsCopyId')}</span>
            <input
              value={copyId}
              spellCheck={false}
              placeholder={`${copyFrom.id}-copy`}
              onChange={(event) => setCopyId(event.target.value)}
            />
          </label>
          <label className={styles.field}>
            <span>{t('agentPresetsCopyName')}</span>
            <input value={copyName} onChange={(event) => setCopyName(event.target.value)} />
          </label>
          <div className={styles.formActions}>
            <button
              type="button"
              className={styles.primary}
              disabled={busy || !copyId.trim()}
              onClick={() => void submitCopy()}
            >
              {t('agentPresetsCopyConfirm')}
            </button>
            <button type="button" className={styles.btn} onClick={() => setCopyFrom(null)}>
              {t('agentPresetsCancel')}
            </button>
          </div>
        </div>
      )}

      {viewer && (
        <div className={styles.form}>
          <div className={styles.formTitle}>
            {t('agentPresetsView')} · <code>{viewer.id}/preset.yml</code>
          </div>
          <p className={styles.hint}>{t('agentPresetsViewHint')}</p>
          <pre className={styles.viewer}>{viewer.text}</pre>
          <div className={styles.formActions}>
            <button type="button" className={styles.btn} onClick={() => setViewer(null)}>
              {t('agentPresetsClose')}
            </button>
          </div>
        </div>
      )}
    </div>
  )
}

/**
 * 内联预设编辑器:骨架与设置弹窗其它页同源(`setm-card` + `setm-card-body`),
 * 字段用共享原子(Field/TextInput/ChipRadio),列表行仍走本页 module 样式。
 */
function PresetEditor({
  mode,
  draft,
  knownTools,
  saving,
  error,
  onChange,
  onSave,
  onClose,
}: {
  mode: 'create' | 'edit'
  draft: PresetDraft
  knownTools: string[]
  saving: boolean
  error: string
  onChange: (patch: Partial<PresetDraft>) => void
  onSave: () => void
  onClose: () => void
}) {
  const [toolDraft, setToolDraft] = useState('')
  const id = draft.id.trim()
  const name = draft.name.trim()
  const idInvalid = mode === 'create' && id.length > 0 && !PRESET_ID_RE.test(id)
  const tools = draft.tools
  // 空白名单不可保存:服务端把它当非法输入(全量工具集要切回"全量"模式)。
  const emptyAllowlist = draft.toolsMode === 'allowlist' && tools.length === 0
  const canSave =
    name.length > 0 && (mode === 'edit' || PRESET_ID_RE.test(id)) && !emptyAllowlist
  // 追加过的、或服务端带来的未知工具名单独成 chip;已知工具在复选框里表达。
  const customTools = tools.filter((tool) => !knownTools.includes(tool))

  const addTool = () => {
    const next = toolDraft.trim()
    if (!next) return
    setToolDraft('')
    if (tools.includes(next)) return
    onChange({ tools: [...tools, next] })
  }

  const toggleTool = (name: string, on: boolean) => {
    onChange({ tools: on ? [...tools, name] : tools.filter((tool) => tool !== name) })
  }

  return (
    <section className="setm-card open" data-testid="agent-preset-editor">
      <header className={styles.editorHead}>
        <div className={styles.editorTitle}>
          {mode === 'edit' ? t('agentPresetsEdit') : t('agentPresetsNew')}
        </div>
        <p className={styles.editorNote}>{t('agentPresetsEditorHint')}</p>
      </header>
      <div className="setm-card-body">
        <div className="setm-form-grid two">
          <Field
            label={t('agentPresetsNameLabel')}
            error={name.length > 0 ? null : t('agentPresetsNameRequired')}
          >
            <TextInput
              value={draft.name}
              onChange={(value) => onChange({ name: value })}
              placeholder="My preset"
            />
          </Field>
          {mode === 'create' ? (
            <Field
              label={t('agentPresetsIdLabel')}
              hint={t('agentPresetsIdHint')}
              error={idInvalid ? t('agentPresetsIdInvalid') : null}
            >
              <TextInput
                value={draft.id}
                mono
                onChange={(value) => onChange({ id: value })}
                placeholder="my-preset"
              />
            </Field>
          ) : (
            <Field label={t('agentPresetsIdLabel')} hint={t('agentPresetsIdImmutable')} error={null}>
              <TextInput value={draft.id} mono disabled onChange={() => {}} />
            </Field>
          )}
        </div>

        <Field label={t('agentPresetsDescriptionLabel')} error={null}>
          <TextInput
            value={draft.description}
            onChange={(value) => onChange({ description: value })}
          />
        </Field>

        <Field label={t('agentPresetsPersonaLabel')} hint={t('agentPresetsPersonaHint')} error={null}>
          <textarea
            className={styles.textarea}
            rows={6}
            spellCheck={false}
            value={draft.persona}
            onChange={(event) => onChange({ persona: event.target.value })}
          />
        </Field>
        <label className={styles.checkbox}>
          <input
            type="checkbox"
            checked={draft.personaComplete}
            onChange={(event) => onChange({ personaComplete: event.target.checked })}
          />
          <span>{t('agentPresetsPersonaComplete')}</span>
        </label>

        <div className="setm-field-label">{t('agentPresetsToolsTitle')}</div>
        <ChipRadio
          value={draft.toolsMode}
          options={[
            { id: 'all', label: t('agentPresetsToolsAll') },
            { id: 'allowlist', label: t('agentPresetsToolsAllowlist') },
          ]}
          onChange={(value) => onChange({ toolsMode: value })}
        />
        {draft.toolsMode === 'allowlist' && (
          <>
            <p className={styles.hint}>
              {t('agentPresetsToolsAllowlistHint')} ·{' '}
              {t('agentPresetsToolsCount', { n: tools.length })}
            </p>
            {knownTools.length > 0 && (
              <div className={styles.toolPanel}>
                <div className={styles.toolGrid}>
                  {knownTools.map((tool) => (
                    <label key={tool} className={styles.checkbox}>
                      <input
                        type="checkbox"
                        checked={tools.includes(tool)}
                        onChange={(event) => toggleTool(tool, event.target.checked)}
                      />
                      <span className={styles.toolName}>{tool}</span>
                    </label>
                  ))}
                </div>
              </div>
            )}
            <div className={styles.addRow}>
              <input
                className={styles.toolInput}
                value={toolDraft}
                spellCheck={false}
                aria-label={t('agentPresetsToolsPlaceholder')}
                placeholder={t('agentPresetsToolsPlaceholder')}
                onChange={(event) => setToolDraft(event.target.value)}
                onKeyDown={(event) => {
                  // IME 组合期间的回车是上屏,不是提交。
                  if (event.nativeEvent.isComposing) return
                  if (event.key === 'Enter') {
                    event.preventDefault()
                    addTool()
                  }
                }}
              />
              <SetmBtn small variant="ghost" disabled={!toolDraft.trim()} onClick={addTool}>
                {t('agentPresetsToolsAdd')}
              </SetmBtn>
            </div>
            {customTools.length > 0 && (
              <div className={styles.chipRow}>
                {customTools.map((tool) => (
                  <span key={tool} className={styles.chip} title={t('agentPresetsToolsUnknown')}>
                    {tool}
                    <button
                      type="button"
                      className={styles.chipRemove}
                      aria-label={t('agentPresetsToolsRemove')}
                      title={t('agentPresetsToolsRemove')}
                      onClick={() => onChange({ tools: tools.filter((item) => item !== tool) })}
                    >
                      <IconClose size={10} />
                    </button>
                  </span>
                ))}
              </div>
            )}
            {emptyAllowlist && (
              <p className={styles.error} role="alert">
                {t('agentPresetsToolsEmpty')}
              </p>
            )}
          </>
        )}

        <div className="setm-field-label">{t('agentPresetsFeaturesTitle')}</div>
        <p className={styles.hint}>{t('agentPresetsFeaturesHint')}</p>
        <div className={styles.checkGrid}>
          {FEATURE_KEYS.map(({ key, labelKey }) => (
            <label key={key} className={styles.checkbox}>
              <input
                type="checkbox"
                checked={draft.features[key]}
                onChange={(event) =>
                  onChange({ features: withFeature(draft.features, key, event.target.checked) })
                }
              />
              <span>{t(labelKey)}</span>
            </label>
          ))}
        </div>

        {error && (
          <p className={styles.error} role="alert">
            {error}
          </p>
        )}
        <SetmActions>
          <SetmBtn disabled={saving || !canSave} onClick={onSave}>
            {t('agentPresetsSave')}
          </SetmBtn>
          <SetmBtn variant="ghost" disabled={saving} onClick={onClose}>
            {t('agentPresetsCancel')}
          </SetmBtn>
        </SetmActions>
      </div>
    </section>
  )
}
