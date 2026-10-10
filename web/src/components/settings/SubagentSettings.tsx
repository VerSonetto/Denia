/**
 * 子代理设置页：定义管理（调度 + 定义清单 + 编辑器 + 预览）。
 *
 * 事实源在服务端：UI 只持草稿；保存后以服务端返回的 view 更新列表（含
 * revision）。保存冲突时保留草稿并提示重新加载，绝不覆盖另一个窗口的修改。
 *
 * 版式与设置弹窗的其它页面同源：骨架用全局 `setm-section`/`setm-card`/
 * `setm-field-block`/`setm-actions` 与共享原件 [`setm.tsx`](./setm.tsx)，
 * 表单控件用共享原子（Field/TextInput/ChipRadio/NumberInput）与
 * `DropdownField`；本页的模块样式只负责定义卡片、工具选择面板、冲突提示与
 * 预览块，且只取主题 token。
 *
 * 三件不做的事（执行计划 11.2）：
 * - 不提供"开启全局 AGENTS.md"或"委派深度"开关：子代理继承主提示基础、
 *   只自动加载项目规则，且禁止派遣子代理是运行时硬规则；
 * - 不在未选工作区时宣称"最终可用工具"（只能显示定义请求集合）；
 * - 不手改 generated 类型：wire 类型来自 Rust。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import type { Notify } from '../../App'
import type {
  ModelSelection,
  ProfileSource,
  ProfileWriteScope,
  SubagentCatalogView,
  SubagentPreview,
  SubagentProfile,
  SubagentProfileView,
  SubagentToolRow,
  SubagentToolsView,
  ToolSelection,
} from '../../types'
import { ChipRadio, Field, NumberInput, TextInput } from '../llm/atoms/form'
import { DropdownField, type SelectOption } from '../ui/controls'
import { SetmActions, SetmBtn, SetmCard } from './setm'
import {
  allowlistOf,
  blankProfile,
  cloneProfile,
  conflictKeepsDraft,
  copyProfile,
  draftAfterSave,
  readOnlyConflicts,
  rowIsDisabled,
  toggleToolName,
  toolSelectionSummary,
  unknownToolNames,
} from './subagentDraft'
import styles from './SubagentSettings.module.css'

const POLICY_NS = 'subagent-policy'

const SOURCE_LABELS: Record<ProfileSource, string> = {
  builtin: 'subagentsSourceBuiltin',
  user: 'subagentsSourceUser',
  project: 'subagentsSourceProject',
}

const SOURCE_ORDER: ProfileSource[] = ['project', 'user', 'builtin']

interface Draft {
  kind: 'create' | 'edit' | 'copy'
  /** 编辑目标（edit 时为被编辑的 qualifiedId）。 */
  target?: string
  writeScope: ProfileWriteScope
  revision: number
  profile: SubagentProfile
  dirty: boolean
}

export function SubagentSettings({ notify }: { notify: Notify }) {
  // 项目上下文:工作区列表(一工作区一条),替代早期的"每个会话一条"下拉。
  // scope 载体是 cwd(服务端用它推导项目根),会话只是 cwd 的代理,不再枚举。
  const [workspaces, setWorkspaces] = useState<{ id: string; title: string; path: string }[]>([])
  const [workspaceId, setWorkspaceId] = useState('')
  const [catalog, setCatalog] = useState<SubagentCatalogView | null>(null)
  const [tools, setTools] = useState<SubagentToolsView | null>(null)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [preview, setPreview] = useState<SubagentPreview | null>(null)
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState('')
  const [search, setSearch] = useState('')
  const [policy, setPolicy] = useState<number | null>(null)
  const [catalogBudget, setCatalogBudget] = useState<number | null>(null)
  const [policyRevision, setPolicyRevision] = useState(0)
  const [policySaved, setPolicySaved] = useState(false)
  // 调度是"设一次就不管"的配置，定义清单才是这页的主体：默认折叠。
  const [policyOpen, setPolicyOpen] = useState(false)
  const editorRef = useRef<HTMLDivElement | null>(null)

  /** 编辑卡在列表上方打开：进入视野，避免"点了编辑却没反应"的错觉。 */
  useEffect(() => {
    if (draft) editorRef.current?.scrollIntoView({ block: 'start', behavior: 'smooth' })
  }, [draft?.kind, draft?.target])

  const cwd = workspaces.find((entry) => entry.id === workspaceId)?.path ?? ''
  const scope = cwd ? { cwd } : {}

  const load = useCallback(async () => {
    setLoading(true)
    setError('')
    try {
      const [profileView, toolView, settings, workspaceList] = await Promise.all([
        api.listSubagentProfiles(scope),
        api.listSubagentTools(scope),
        api.getSettings(),
        api.listWorkspaces(),
      ])
      setCatalog(profileView)
      setTools(toolView)
      const rows = workspaceList.workspaces.map((entry) => ({
        id: entry.id,
        title: entry.title,
        path: entry.path,
      }))
      setWorkspaces(rows)
      // 工作区被删时回退到用户级,不挂死在一个非法 cwd 上。
      setWorkspaceId((current) => (current && rows.some((row) => row.id === current) ? current : ''))
      const ns = settings.namespaces.find((entry) => entry.ns === POLICY_NS)
      if (ns) {
        setPolicy(typeof ns.value.maxConcurrentRuns === 'number' ? (ns.value.maxConcurrentRuns as number) : 8)
        setPolicyRevision(ns.revision)
      }
      // 目录预算属于 runtime 命名空间（与工作区指令预算并列，改 YAML 生效）。
      const runtimeNs = settings.namespaces.find((entry) => entry.ns === 'runtime')
      const budget = runtimeNs?.value?.subagentCatalogMaxBytes
      setCatalogBudget(typeof budget === 'number' ? budget : null)
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason))
    } finally {
      setLoading(false)
    }
  }, [workspaceId])

  useEffect(() => {
    void load()
  }, [load])

  const startCreate = () => {
    const profile = blankProfile()
    setPreview(null)
    setDraft({
      kind: 'create',
      writeScope: cwd && catalog?.projectDir ? 'project' : 'user',
      revision: 0,
      profile,
      dirty: true,
    })
  }

  const startEdit = (view: SubagentProfileView, kind: 'edit' | 'copy') => {
    setPreview(null)
    const profile = kind === 'copy' ? copyProfile(view) : cloneProfile(view)
    setDraft({
      kind,
      target: kind === 'edit' ? view.qualifiedId : undefined,
      writeScope: kind === 'edit' && view.source === 'project' ? 'project' : 'user',
      revision: view.revision,
      profile,
      dirty: true,
    })
  }

  const save = async () => {
    if (!draft) return
    // 只读上限与写/命令类工具的冲突必须由用户显式解决：不偷偷放宽上限，
    // 也不静默丢掉已选工具。
    const conflicts = readOnlyConflicts(
      draft.profile.permissionCeiling,
      allowlistOf(draft.profile) ?? [],
      tools?.tools ?? [],
    )
    if (conflicts.length > 0) {
      setError(
        `${t('subagentsReadOnlyConflict')}：${conflicts.join('、')}。${t('subagentsReadOnlyConflictHint')}`,
      )
      return
    }
    setSaving(true)
    setError('')
    try {
      if (draft.kind === 'edit' && draft.target) {
        const result = await api.updateSubagentProfile(draft.target, {
          profile: draft.profile,
          expectedRevision: draft.revision,
          ...scope,
        })
        // 以服务端返回为准：内置定义的编辑落到用户覆盖层，目标随返回的
        // qualifiedId 与 revision 更新（不靠本地推论）。
        setDraft({
          kind: 'edit',
          dirty: false,
          ...draftAfterSave(draft.writeScope, result.profile),
        })
        notify('ok', t('subagentsSaved'))
      } else {
        const result = await api.createSubagentProfile({
          scope: draft.writeScope,
          profile: draft.profile,
          ...scope,
        })
        setDraft({
          kind: 'edit',
          dirty: false,
          ...draftAfterSave(draft.writeScope, result.profile),
        })
        notify('ok', t('subagentsCreated'))
      }
      await load()
    } catch (reason) {
      const message = reason instanceof Error ? reason.message : String(reason)
      // 保存冲突：草稿保留，提示重新加载。
      const status = reason instanceof api.ApiError ? reason.status : 0
      const { conflict } = conflictKeepsDraft(draft, status)
      setError(conflict ? `${t('subagentsConflict')}（${message}）` : message)
      notify('err', message)
    } finally {
      setSaving(false)
    }
  }

  const runAction = async (action: () => Promise<unknown>, okKey: Parameters<typeof t>[0]) => {
    setSaving(true)
    setError('')
    try {
      await action()
      notify('ok', t(okKey))
      setDraft(null)
      setPreview(null)
      await load()
    } catch (reason) {
      const message = reason instanceof Error ? reason.message : String(reason)
      setError(message)
      notify('err', message)
    } finally {
      setSaving(false)
    }
  }

  const refreshPreview = async () => {
    if (!draft) return
    if (draft.kind === 'edit' && draft.target) {
      setPreview(await api.previewSubagentProfile({ profileId: draft.target, ...scope }))
      return
    }
    const { profile } = draft
    setPreview(
      await api.previewSubagentProfile({
        inline: {
          name: profile.name || 'inline',
          description: profile.description || 'inline',
          instructions: profile.instructions,
          tools: profile.tools,
          model: profile.model,
          permissionCeiling: profile.permissionCeiling,
        },
        ...scope,
      }),
    )
  }

  const savePolicy = async () => {
    if (policy === null) return
    setSaving(true)
    setError('')
    try {
      await api.replaceNamespace(POLICY_NS, { maxConcurrentRuns: policy }, policyRevision)
      const settings = await api.getSettings()
      const ns = settings.namespaces.find((entry) => entry.ns === POLICY_NS)
      if (ns) setPolicyRevision(ns.revision)
      setPolicySaved(true)
      notify('ok', t('runtimeSaved'))
    } catch (reason) {
      const message = reason instanceof Error ? reason.message : String(reason)
      setError(message)
      notify('err', message)
    } finally {
      setSaving(false)
    }
  }

  const filtered = useMemo(() => {
    const list = catalog?.effective ?? []
    const needle = search.trim().toLowerCase()
    if (!needle) return list
    return list.filter((view) =>
      [view.profile.id, view.profile.name, view.profile.description]
        .join(' ')
        .toLowerCase()
        .includes(needle),
    )
  }, [catalog, search])

  const grouped = useMemo(() => {
    const map = new Map<ProfileSource, SubagentProfileView[]>()
    for (const source of SOURCE_ORDER) map.set(source, [])
    for (const view of filtered) map.get(view.source)?.push(view)
    return map
  }, [filtered])

  // 项目下拉选项:首项用户级,其余一工作区一条(label 取标题尾段,hint 取全路径)。
  const workspaceOptions = useMemo<SelectOption<string>[]>(
    () => [
      { id: '', label: t('subagentsScopeUser'), hint: t('subagentsSessionHint') },
      ...workspaces.map((entry) => ({
        id: entry.id,
        label: (entry.title || entry.path).split(/[\\/]/).filter(Boolean).pop() ?? entry.path,
        hint: entry.path,
      })),
    ],
    [workspaces],
  )

  const update = (patch: Partial<SubagentProfile>) => {
    setDraft((current) =>
      current ? { ...current, profile: { ...current.profile, ...patch }, dirty: true } : current,
    )
  }

  const effective = catalog?.effective ?? []
  const shadowed = catalog?.shadowed ?? []
  const diagnostics = catalog?.diagnostics ?? []
  const effectiveCount = effective.length
  const enabledCount = effective.filter((view) => view.profile.enabled).length

  return (
    <section className="setm-section" data-testid="subagent-settings">
      {error && (
        <p className={styles.error} role="alert">
          {error}
        </p>
      )}

      <header className={styles.head}>
        <div>
          <p className={styles.summary}>
            {t('subagentsEffective')} {effectiveCount} · {t('subagentsEnabled')} {enabledCount} ·{' '}
            {cwd ? t('subagentsScopeProject') : t('subagentsScopeUser')}
          </p>
          <p className="setm-section-hint">
            {t('subagentsInheritNote')} {t('subagentsNoDispatchNote')}
          </p>
        </div>
        <SetmBtn disabled={draft != null} onClick={startCreate}>
          {t('subagentsNew')}
        </SetmBtn>
      </header>

      <div className={styles.toolbar}>
        <Field label={t('subagentsSearch')} error={null}>
          <TextInput value={search} onChange={setSearch} placeholder={t('subagentsSearch')} />
        </Field>
        <DropdownField
          label={t('subagentsSessionLabel')}
          value={workspaceId}
          options={workspaceOptions}
          onChange={(next) => {
            setWorkspaceId(next)
            setDraft(null)
            setPreview(null)
          }}
        />
      </div>

      <SetmCard
        title={t('subagentsScheduleTitle')}
        hint={t('subagentsScheduleHint')}
        summary={policy === null ? undefined : String(policy)}
        open={policyOpen}
        onToggle={() => setPolicyOpen((open) => !open)}
      >
        <div className="setm-field-block">
          <div className="setm-field-label">{t('subagentsMaxConcurrentRuns')}</div>
          <NumberInput
            value={policy ?? undefined}
            onChange={(next) => {
              setPolicySaved(false)
              setPolicy(next ?? null)
            }}
          />
        </div>
        {catalogBudget !== null && (
          <p className="setm-callout">
            {t('subagentsCatalogBudget')}：{catalogBudget}（runtime.subagentCatalogMaxBytes，改 YAML
            生效；超限按 UTF-8 边界截断并明示）
          </p>
        )}
        <SetmActions>
          <SetmBtn disabled={saving || policy === null} onClick={() => void savePolicy()}>
            {t('subagentsSave')}
          </SetmBtn>
          {policySaved && <span className={styles.saved}>{t('runtimeSaved')}</span>}
        </SetmActions>
      </SetmCard>

      {draft && (
        <div ref={editorRef}>
          <DraftEditor
            draft={draft}
            tools={tools}
            preview={preview}
            projectWritable={Boolean(catalog?.projectDir)}
            saving={saving}
            onChange={update}
            onScopeChange={(writeScope) => setDraft({ ...draft, writeScope, dirty: true })}
            onToolsChange={(next) => setDraft({ ...draft, profile: { ...draft.profile, tools: next }, dirty: true })}
            onModelChange={(next) => setDraft({ ...draft, profile: { ...draft.profile, model: next }, dirty: true })}
            onSave={() => void save()}
            onPreview={() => void refreshPreview()}
            onClose={() => {
              setDraft(null)
              setPreview(null)
            }}
          />
        </div>
      )}

      <div className="setm-section">
        <div className="setm-section-label">{t('subagentsDefinitions')}</div>

        {loading ? (
          <p className="setm-empty">{t('loading')}</p>
        ) : filtered.length === 0 ? (
          <p className="setm-empty">{t('subagentsEmpty')}</p>
        ) : (
          <div className={styles.list}>
            {SOURCE_ORDER.map((source) => {
              const rows = grouped.get(source) ?? []
              if (rows.length === 0) return null
              return (
                <div key={source} className={styles.list}>
                  <h5 className={styles.groupHead}>
                    {t(SOURCE_LABELS[source] as Parameters<typeof t>[0])}
                  </h5>
                  {rows.map((view) => (
                    <ProfileRow
                      key={view.qualifiedId}
                      view={view}
                      active={draft?.target === view.qualifiedId}
                      onEdit={() => startEdit(view, 'edit')}
                      onCopy={() => startEdit(view, 'copy')}
                      onToggle={() =>
                        void runAction(
                          () =>
                            api.updateSubagentProfile(view.qualifiedId, {
                              profile: { ...view.profile, enabled: !view.profile.enabled },
                              expectedRevision: view.revision,
                              ...scope,
                            }),
                          'subagentsSaved',
                        )
                      }
                      onReset={() =>
                        void runAction(
                          () =>
                            api.resetSubagentProfile(view.qualifiedId, {
                              scope: view.source === 'project' ? 'project' : 'user',
                              ...scope,
                            }),
                          'subagentsSaved',
                        )
                      }
                      onDelete={() =>
                        void runAction(
                          () =>
                            api.deleteSubagentProfile(view.qualifiedId, {
                              scope: view.source === 'project' ? 'project' : 'user',
                              expectedRevision: view.revision,
                              ...scope,
                            }),
                          'subagentsDeleted',
                        )
                      }
                    />
                  ))}
                </div>
              )
            })}
          </div>
        )}

        {shadowed.length > 0 && (
          <div className={styles.list}>
            <h5 className={styles.groupHead}>{t('subagentsShadowed')}</h5>
            {shadowed.map((view) => (
              <ProfileRow
                key={view.qualifiedId}
                view={view}
                shadowed
                onCopy={() => startEdit(view, 'copy')}
              />
            ))}
          </div>
        )}

        {diagnostics.length > 0 && (
          <div className={styles.list}>
            <h5 className={styles.groupHead}>{t('subagentsDiagnostics')}</h5>
            <ul className={styles.alertList}>
              {diagnostics.map((issue, index) => (
                <li key={`${issue.code}-${index}`}>
                  <code>{issue.code}</code> {issue.message}
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
    </section>
  )
}

function ProfileRow({
  view,
  active,
  shadowed,
  onEdit,
  onCopy,
  onToggle,
  onReset,
  onDelete,
}: {
  view: SubagentProfileView
  active?: boolean
  shadowed?: boolean
  onEdit?: () => void
  onCopy?: () => void
  onToggle?: () => void
  onReset?: () => void
  onDelete?: () => void
}) {
  // 摘要由共享纯函数产出：inherit 与"空列表"必须给出不同文案。
  const tools = toolSelectionSummary(view.profile.tools, {
    inherit: t('subagentsToolsInherit'),
    none: t('subagentsToolsNone'),
    count: (n) => t('subagentsToolsCount').replace('{n}', String(n)),
  })
  const model =
    view.profile.model.mode === 'inherit'
      ? t('subagentsModelInherit')
      : `${view.profile.model.selection.provider}/${view.profile.model.selection.model}`
  const diagnostic = view.diagnostics?.[0]?.message
  return (
    <article
      className={`${styles.def}${active ? ` ${styles.defActive}` : ''}`}
      data-qualified={view.qualifiedId}
    >
      <div className={styles.defHead}>
        <div>
          <div className={styles.defTitle}>
            <span className={styles.defName}>{view.profile.name}</span>
            <code className={styles.defId}>{view.qualifiedId}</code>
          </div>
        </div>
        <div className={styles.defTitle}>
          {!view.profile.enabled && (
            <span className={`${styles.tag} ${styles.tagMuted}`}>{t('subagentsDisabled')}</span>
          )}
          {view.overridesBuiltin && (
            <span className={`${styles.tag} ${styles.tagAccent}`}>
              {t('subagentsSourceUser')}
            </span>
          )}
          <span className={styles.tag}>
            {t(SOURCE_LABELS[view.source] as Parameters<typeof t>[0])}
          </span>
        </div>
      </div>
      {view.profile.description && <p className={styles.defDesc}>{view.profile.description}</p>}
      <p className={styles.defMeta}>
        <span>{tools}</span>
        <span>{model}</span>
        <span>
          {view.profile.permissionCeiling === 'read-only'
            ? t('subagentsCeilingReadOnly')
            : t('subagentsCeilingInherit')}
        </span>
        {diagnostic && <span>{diagnostic}</span>}
      </p>
      <div className={styles.defActions}>
        {onEdit && (
          <SetmBtn small variant="ghost" onClick={onEdit}>
            {t('subagentsEdit')}
          </SetmBtn>
        )}
        {onCopy && (
          <SetmBtn small variant="ghost" onClick={onCopy}>
            {t('subagentsCopy')}
          </SetmBtn>
        )}
        {!shadowed && onToggle && (
          <SetmBtn small variant="ghost" onClick={onToggle}>
            {view.profile.enabled ? t('subagentsDisable') : t('subagentsEnable')}
          </SetmBtn>
        )}
        {!shadowed && onReset && view.source !== 'builtin' && (
          <SetmBtn small variant="ghost" onClick={onReset}>
            {t('subagentsReset')}
          </SetmBtn>
        )}
        {!shadowed && onDelete && view.source !== 'builtin' && (
          <SetmBtn small variant="danger" onClick={onDelete}>
            {t('subagentsDelete')}
          </SetmBtn>
        )}
      </div>
    </article>
  )
}

function DraftEditor({
  draft,
  tools,
  preview,
  projectWritable,
  saving,
  onChange,
  onScopeChange,
  onToolsChange,
  onModelChange,
  onSave,
  onPreview,
  onClose,
}: {
  draft: Draft
  tools: SubagentToolsView | null
  preview: SubagentPreview | null
  projectWritable: boolean
  saving: boolean
  onChange: (patch: Partial<SubagentProfile>) => void
  onScopeChange: (scope: ProfileWriteScope) => void
  onToolsChange: (next: ToolSelection) => void
  onModelChange: (next: SubagentProfile['model']) => void
  onSave: () => void
  onPreview: () => void
  onClose: () => void
}) {
  const [toolFilter, setToolFilter] = useState('')
  const { profile } = draft
  const allowlist = profile.tools.mode === 'allowlist' ? profile.tools.names : []
  const rows: SubagentToolRow[] = tools?.tools ?? []
  const needle = toolFilter.trim().toLowerCase()
  const visible = rows.filter((row) => !needle || row.name.toLowerCase().includes(needle))
  const categories = Array.from(new Set(visible.map((row) => row.category)))
  // 离线/已卸载工具与只读冲突都在这里算：保存前拦住冲突，未知项始终可见。
  const unknown = unknownToolNames(allowlist, rows)
  const conflicts = readOnlyConflicts(profile.permissionCeiling, allowlist, rows)

  const toggleTool = (name: string) => {
    onToolsChange({ mode: 'allowlist', names: toggleToolName(allowlist, name) })
  }

  return (
    <section className={`setm-card open ${styles.block}`} data-testid="subagent-editor">
      <header className={styles.editorHead}>
        <div>
          <div className={styles.editorTitle}>
            {draft.kind === 'edit'
              ? t('subagentsEdit')
              : draft.kind === 'copy'
                ? t('subagentsCopy')
                : t('subagentsNew')}
          </div>
          <p className={styles.editorNote}>{t('subagentsEditorHint')}</p>
        </div>
        <SetmBtn small variant="ghost" onClick={onClose}>
          {t('runtimeClose')}
        </SetmBtn>
      </header>

      <div className="setm-card-body">
        {/* ---------- 基本信息 ---------- */}
        <div className="setm-field-label">{t('subagentsBasic')}</div>
        <div className="setm-form-grid two">
          <Field label="id" hint="^[a-z0-9][a-z0-9-]{0,63}$" error={null}>
            <TextInput
              value={profile.id}
              mono
              disabled={draft.kind === 'edit'}
              placeholder="api-reviewer"
              onChange={(value) => onChange({ id: value })}
            />
          </Field>
          <Field label={t('subagentsName')} error={null}>
            <TextInput value={profile.name} onChange={(value) => onChange({ name: value })} />
          </Field>
        </div>
        <Field label={t('subagentsDescription')} error={null}>
          <textarea
            className={styles.textArea}
            rows={3}
            value={profile.description}
            onChange={(event) => onChange({ description: event.target.value })}
          />
        </Field>

        <div className="setm-form-grid two">
          <Field label={t('subagentsEnabled')} error={null}>
            <ChipRadio
              value={profile.enabled ? 'on' : 'off'}
              options={[
                { id: 'on', label: t('subagentsEnable') },
                { id: 'off', label: t('subagentsDisable') },
              ]}
              onChange={(value) => onChange({ enabled: value === 'on' })}
            />
          </Field>
          <Field label={t('subagentsCeilingTitle')} hint={t('subagentsCeilingHint')} error={null}>
            <ChipRadio
              value={profile.permissionCeiling}
              options={[
                { id: 'inherit', label: t('subagentsCeilingInherit') },
                { id: 'read-only', label: t('subagentsCeilingReadOnly') },
              ]}
              onChange={(value) =>
                onChange({ permissionCeiling: value as SubagentProfile['permissionCeiling'] })
              }
            />
          </Field>
          <Field label={t('subagentsScope')} hint={t('subagentsScopeHint')} error={null}>
            <ChipRadio
              value={draft.writeScope}
              options={[
                { id: 'user', label: t('subagentsScopeUser') },
                { id: 'project', label: t('subagentsScopeProject') },
              ]}
              disabled={draft.kind === 'edit' && draft.writeScope === 'project'}
              onChange={(value) => onScopeChange(value as ProfileWriteScope)}
            />
          </Field>
        </div>
        {!projectWritable && <p className={styles.hint}>{t('subagentsScopeProjectUnavailable')}</p>}

        {/* ---------- 角色提示 ---------- */}
        <div className="setm-field-label">{t('subagentsInstructions')}</div>
        <div className="setm-prompt-shell">
          <textarea
            className="setm-textarea"
            rows={9}
            spellCheck={false}
            value={profile.instructions}
            onChange={(event) => onChange({ instructions: event.target.value })}
          />
        </div>
        <p className={styles.hint}>{t('subagentsInstructionsHint')}</p>

        {/* ---------- 工具选择 ---------- */}
        <div className="setm-field-label">{t('subagentsToolsTitle')}</div>
        <ChipRadio
          value={profile.tools.mode}
          options={[
            { id: 'inherit', label: t('subagentsToolsInheritOption') },
            { id: 'allowlist', label: t('subagentsToolsAllowlistOption') },
          ]}
          onChange={(value) =>
            onToolsChange(
              value === 'inherit' ? { mode: 'inherit' } : { mode: 'allowlist', names: allowlist },
            )
          }
        />
        {profile.tools.mode === 'allowlist' && (
          <>
            <p className={styles.hint}>
              {allowlist.length === 0
                ? t('subagentsToolsNone')
                : t('subagentsToolsCount').replace('{n}', String(allowlist.length))}{' '}
              · {t('subagentsToolsSectionHint')}
            </p>
            <div className={styles.toolFilter}>
              <Field label={t('subagentsSearch')} error={null}>
                <TextInput value={toolFilter} onChange={setToolFilter} placeholder="bash" />
              </Field>
            </div>
            <div className={styles.toolPanel} data-testid="subagent-tools">
              {visible.length === 0 && <p className={styles.hint}>{t('subagentsToolNoMatch')}</p>}
              {categories.map((category) => (
                <div key={category}>
                  <div className={styles.toolCategory}>{category}</div>
                  {visible
                    .filter((row) => row.category === category)
                    .map((row) => {
                      const selected = allowlist.includes(row.name)
                      const note = row.hardDenied
                        ? t('subagentsToolHardDenied')
                        : row.granted === false
                          ? t('subagentsToolUngranted')
                          : row.reason
                      return (
                        <button
                          key={row.name}
                          type="button"
                          role="switch"
                          aria-checked={selected}
                          disabled={rowIsDisabled(row)}
                          title={note}
                          className={`${styles.toolRow}${selected ? ` ${styles.toolRowOn}` : ''}`}
                          data-tool={row.name}
                          onClick={() => toggleTool(row.name)}
                        >
                          <span className={styles.toolName}>
                            {selected ? '✓ ' : ''}
                            {row.name}
                          </span>
                          <span className={styles.toolEffect}>{row.effect || note}</span>
                          <span className={`${styles.toolState}${selected ? ` ${styles.toolOn}` : ''}`}>
                            {selected ? t('subagentsToolSelected') : note ? t('subagentsToolBlocked') : ''}
                          </span>
                        </button>
                      )
                    })}
                </div>
              ))}
            </div>
            {unknown.length > 0 && (
              <div className={styles.alert} role="alert" data-testid="unknown-tools">
                <span className={styles.alertTitle}>{t('subagentsUnknownTools')}</span>
                <p className={styles.hint}>{t('subagentsUnknownToolsHint')}</p>
                <ul className={styles.alertList}>
                  {unknown.map((name) => (
                    <li key={name} className={styles.alertItem}>
                      <code>{name}</code>
                      <SetmBtn small variant="ghost" onClick={() => toggleTool(name)}>
                        {t('subagentsRemoveTool')}
                      </SetmBtn>
                    </li>
                  ))}
                </ul>
              </div>
            )}
            {conflicts.length > 0 && (
              <div className={styles.alert} role="alert" data-testid="readonly-conflicts">
                <span className={styles.alertTitle}>{t('subagentsReadOnlyConflict')}</span>
                <p className={styles.hint}>{t('subagentsReadOnlyConflictHint')}</p>
                <code>{conflicts.join('、')}</code>
                <div className={styles.alertActions}>
                  <SetmBtn small variant="ghost" onClick={() => onChange({ permissionCeiling: 'inherit' })}>
                    {t('subagentsUseInheritCeiling')}
                  </SetmBtn>
                  <SetmBtn
                    small
                    variant="ghost"
                    onClick={() =>
                      onToolsChange({
                        mode: 'allowlist',
                        names: allowlist.filter((name) => !conflicts.includes(name)),
                      })
                    }
                  >
                    {t('subagentsDropConflicts')}
                  </SetmBtn>
                </div>
              </div>
            )}
          </>
        )}

        {/* ---------- 模型 ---------- */}
        <div className="setm-field-label">{t('subagentsModelTitle')}</div>
        <ChipRadio
          value={profile.model.mode}
          options={[
            { id: 'inherit', label: t('subagentsModelInherit') },
            { id: 'explicit', label: t('subagentsModelExplicit') },
          ]}
          onChange={(value) =>
            onModelChange(
              value === 'inherit'
                ? { mode: 'inherit' }
                : {
                    mode: 'explicit',
                    selection: { provider: '', model: '', reasoningEffort: undefined },
                  },
            )
          }
        />
        {profile.model.mode === 'explicit' && (
          <ExplicitModelFields model={profile.model.selection} onChange={onModelChange} />
        )}

        <SetmActions>
          <SetmBtn disabled={saving} onClick={onSave}>
            {t('subagentsSave')}
          </SetmBtn>
          <SetmBtn variant="ghost" disabled={saving} onClick={onPreview}>
            {t('subagentsPreview')}
          </SetmBtn>
          {draft.dirty && <span className={styles.saved}>{t('subagentsDraftChanged')}</span>}
        </SetmActions>

        {preview && (
          <div className={styles.preview} data-testid="subagent-preview">
            <span className={styles.previewTitle}>{t('subagentsPreviewTitle')}</span>
            {preview.final ? (
              <>
                <p className={styles.defMeta}>
                  <span>{preview.profile?.qualifiedId}</span>
                  <span>
                    {preview.model?.provider}/{preview.model?.model}
                  </span>
                  <span>
                    {preview.permissionCeiling === 'read-only'
                      ? t('subagentsCeilingReadOnly')
                      : t('subagentsCeilingInherit')}
                  </span>
                  <span>{preview.instructionScope}</span>
                </p>
                <span className={styles.sectionLabel}>{t('subagentsPreviewTools')}</span>
                {(preview.tools ?? []).length === 0 ? (
                  <p className={styles.hint}>{t('subagentsToolsNone')}</p>
                ) : (
                  <ul className={styles.chipList}>
                    {(preview.tools ?? []).map((name) => (
                      <li key={name} className={styles.chip}>
                        {name}
                      </li>
                    ))}
                  </ul>
                )}
              </>
            ) : (
              <p className={styles.hint}>
                {preview.note ?? t('subagentsPreviewHint')}（{t('subagentsPreviewNoSession')}）
              </p>
            )}
          </div>
        )}
      </div>
    </section>
  )
}

function ExplicitModelFields({
  model,
  onChange,
}: {
  model: ModelSelection
  onChange: (next: SubagentProfile['model']) => void
}) {
  const patch = (part: Partial<ModelSelection>) =>
    onChange({ mode: 'explicit', selection: { ...model, ...part } })
  return (
    <div className="setm-form-grid two">
      <Field label="provider" error={null}>
        <TextInput value={model.provider} onChange={(value) => patch({ provider: value })} />
      </Field>
      <Field label="model" error={null}>
        <TextInput value={model.model} onChange={(value) => patch({ model: value })} />
      </Field>
      <Field label="reasoning effort" hint={t('subagentsEffortHint')} error={null}>
        <TextInput
          value={model.reasoningEffort ?? ''}
          onChange={(value) => patch({ reasoningEffort: value || undefined })}
        />
      </Field>
    </div>
  )
}
