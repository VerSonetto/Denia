/**
 * 子代理设置页：定义管理（列表 + 编辑器）+ 调度。
 *
 * 事实源在服务端：UI 只持草稿；保存后以服务端返回的 view 更新列表（含
 * revision）。保存冲突时保留草稿并提示重新加载，绝不覆盖另一个窗口的修改。
 *
 * 三件不做的事（执行计划 11.2）：
 * - 不提供"开启全局 AGENTS.md"或"委派深度"开关：子代理继承主提示基础、
 *   只自动加载项目规则，且禁止派遣子代理是运行时硬规则；
 * - 不在未选父会话时宣称"最终可用工具"（只能显示定义请求集合）；
 * - 不手改 generated 类型：wire 类型来自 Rust。
 */
import { useCallback, useEffect, useMemo, useState } from 'react'
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
  const [sessions, setSessions] = useState<{ id: string; cwd?: string | null }[]>([])
  const [sessionId, setSessionId] = useState('')
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

  const scope = sessionId ? { sessionId } : {}

  const load = useCallback(async () => {
    setLoading(true)
    setError('')
    try {
      const [profileView, toolView, settings, sessionList] = await Promise.all([
        api.listSubagentProfiles(scope),
        api.listSubagentTools(scope),
        api.getSettings(),
        api.listSessions(),
      ])
      setCatalog(profileView)
      setTools(toolView)
      setSessions(
        sessionList.sessions.map((session) => ({ id: session.id, cwd: session.cwd ?? null })),
      )
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
  }, [sessionId])

  useEffect(() => {
    void load()
  }, [load])

  const startCreate = () => {
    const profile = blankProfile()
    setPreview(null)
    setDraft({
      kind: 'create',
      writeScope: sessionId && catalog?.projectDir ? 'project' : 'user',
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

  const update = (patch: Partial<SubagentProfile>) => {
    setDraft((current) =>
      current ? { ...current, profile: { ...current.profile, ...patch }, dirty: true } : current,
    )
  }

  return (
    <section className={styles.wrap} data-testid="subagent-settings">
      {error && (
        <p className={styles.error} role="alert">
          {error}
        </p>
      )}

      <div className={styles.policy}>
        <h4>{t('subagentsScheduleTitle')}</h4>
        <label className={styles.field}>
          <span>{t('subagentsMaxConcurrentRuns')}</span>
          <input
            type="number"
            min={1}
            max={64}
            value={policy ?? 8}
            onChange={(event) => {
              setPolicySaved(false)
              setPolicy(Number(event.target.value))
            }}
          />
        </label>
        <p className={styles.hint}>{t('subagentsScheduleHint')}</p>
        {catalogBudget !== null && (
          <p className={styles.hint}>
            {t('subagentsCatalogBudget')}：{catalogBudget}（runtime.subagentCatalogMaxBytes，改 YAML 生效；超限按 UTF-8 边界截断并明示）
          </p>
        )}
        <div className={styles.actions}>
          <button type="button" className={styles.primary} disabled={saving} onClick={() => void savePolicy()}>
            {t('subagentsSave')}
          </button>
          {policySaved && <span className={styles.saved}>{t('runtimeSaved')}</span>}
        </div>
      </div>

      <div className={styles.toolbar}>
        <input
          className={styles.search}
          value={search}
          placeholder={t('subagentsSearch')}
          onChange={(event) => setSearch(event.target.value)}
        />
        <select
          value={sessionId}
          onChange={(event) => {
            setSessionId(event.target.value)
            setDraft(null)
            setPreview(null)
          }}
        >
          <option value="">{t('subagentsScopeUser')}</option>
          {sessions.map((session) => (
            <option key={session.id} value={session.id}>
              {(session.cwd ?? session.id).split(/[\\/]/).pop() ?? session.id}
            </option>
          ))}
        </select>
        <button type="button" className={styles.primary} onClick={startCreate}>
          {t('subagentsNew')}
        </button>
      </div>
      <p className={styles.hint}>{t('subagentsInheritNote')}</p>
      <p className={styles.hint}>{t('subagentsNoDispatchNote')}</p>

      {loading ? (
        <p className={styles.hint}>{t('loading')}</p>
      ) : (
        <>
          <h4 className={styles.groupTitle}>{t('subagentsEffective')}</h4>
          {filtered.length === 0 && <p className={styles.hint}>{t('subagentsEmpty')}</p>}
          {SOURCE_ORDER.map((source) => {
            const rows = grouped.get(source) ?? []
            if (rows.length === 0) return null
            return (
              <div key={source} className={styles.group}>
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

          {(catalog?.shadowed.length ?? 0) > 0 && (
            <>
              <h4 className={styles.groupTitle}>{t('subagentsShadowed')}</h4>
              {catalog?.shadowed.map((view) => (
                <ProfileRow
                  key={view.qualifiedId}
                  view={view}
                  shadowed
                  onCopy={() => startEdit(view, 'copy')}
                />
              ))}
            </>
          )}

          {(catalog?.diagnostics.length ?? 0) > 0 && (
            <>
              <h4 className={styles.groupTitle}>{t('subagentsDiagnostics')}</h4>
              <ul className={styles.diagnostics}>
                {catalog?.diagnostics.map((issue, index) => (
                  <li key={`${issue.code}-${index}`}>
                    <code>{issue.code}</code> {issue.message}
                  </li>
                ))}
              </ul>
            </>
          )}
        </>
      )}

      {draft && (
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
      )}
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
    <div className={`${styles.row}${active ? ` ${styles.rowActive}` : ''}`} data-qualified={view.qualifiedId}>
      <div className={styles.rowMain}>
        <div className={styles.rowTitle}>
          <strong>{view.profile.name}</strong>
          <code>{view.qualifiedId}</code>
          {!view.profile.enabled && <span className={styles.badgeOff}>{t('subagentsDisabled')}</span>}
          {view.overridesBuiltin && <span className={styles.badge}>{t('subagentsEffective')}</span>}
        </div>
        <p className={styles.rowDesc}>{view.profile.description}</p>
        <p className={styles.rowMeta}>
          {tools} · {model} ·{' '}
          {view.profile.permissionCeiling === 'read-only'
            ? t('subagentsCeilingReadOnly')
            : t('subagentsCeilingInherit')}
          {diagnostic && ` · ${diagnostic}`}
        </p>
      </div>
      <div className={styles.rowActions}>
        {onEdit && (
          <button type="button" onClick={onEdit}>
            {t('subagentsEdit')}
          </button>
        )}
        {onCopy && (
          <button type="button" onClick={onCopy}>
            {t('subagentsCopy')}
          </button>
        )}
        {!shadowed && onToggle && (
          <button type="button" onClick={onToggle}>
            {view.profile.enabled ? t('subagentsDisable') : t('subagentsEnable')}
          </button>
        )}
        {!shadowed && onReset && view.source !== 'builtin' && (
          <button type="button" onClick={onReset}>
            {t('subagentsReset')}
          </button>
        )}
        {!shadowed && onDelete && view.source !== 'builtin' && (
          <button type="button" className={styles.danger} onClick={onDelete}>
            {t('subagentsDelete')}
          </button>
        )}
      </div>
    </div>
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
    <div className={styles.editor} data-testid="subagent-editor">
      <div className={styles.editorHead}>
        <h4>
          {draft.kind === 'edit' ? t('subagentsEdit') : draft.kind === 'copy' ? t('subagentsCopy') : t('subagentsNew')}
        </h4>
        <button type="button" onClick={onClose}>
          {t('runtimeClose')}
        </button>
      </div>

      <h5>{t('subagentsBasic')}</h5>
      <label className={styles.field}>
        <span>id</span>
        <input
          value={profile.id}
          disabled={draft.kind === 'edit'}
          placeholder="api-reviewer"
          onChange={(event) => onChange({ id: event.target.value })}
        />
      </label>
      <label className={styles.field}>
        <span>{t('subagentsDescription')}</span>
        <input value={profile.name} onChange={(event) => onChange({ name: event.target.value })} />
      </label>
      <label className={styles.field}>
        <span>{t('subagentsDescription')}</span>
        <textarea
          rows={3}
          value={profile.description}
          onChange={(event) => onChange({ description: event.target.value })}
        />
      </label>
      <label className={styles.field}>
        <span>{t('subagentsInstructions')}</span>
        <textarea
          rows={6}
          value={profile.instructions}
          onChange={(event) => onChange({ instructions: event.target.value })}
        />
      </label>
      <p className={styles.hint}>{t('subagentsInstructionsHint')}</p>

      <div className={styles.inline}>
        <label className={styles.check}>
          <input
            type="checkbox"
            checked={profile.enabled}
            onChange={(event) => onChange({ enabled: event.target.checked })}
          />
          <span>{t('subagentsEnabled')}</span>
        </label>
        <label className={styles.field}>
          <span>{t('subagentsScope')}</span>
          <select
            value={draft.writeScope}
            disabled={draft.kind === 'edit' && draft.writeScope === 'project'}
            onChange={(event) => onScopeChange(event.target.value as ProfileWriteScope)}
          >
            <option value="user">{t('subagentsScopeUser')}</option>
            <option value="project" disabled={!projectWritable}>
              {t('subagentsScopeProject')}
            </option>
          </select>
        </label>
        <label className={styles.field}>
          <span>{t('subagentsCeilingTitle')}</span>
          <select
            value={profile.permissionCeiling}
            onChange={(event) =>
              onChange({ permissionCeiling: event.target.value as SubagentProfile['permissionCeiling'] })
            }
          >
            <option value="inherit">{t('subagentsCeilingInherit')}</option>
            <option value="read-only">{t('subagentsCeilingReadOnly')}</option>
          </select>
        </label>
      </div>

      <h5>{t('subagentsToolsTitle')}</h5>
      <div className={styles.inline}>
        <label className={styles.check}>
          <input
            type="radio"
            checked={profile.tools.mode === 'inherit'}
            onChange={() => onToolsChange({ mode: 'inherit' })}
          />
          <span>{t('subagentsToolsInheritOption')}</span>
        </label>
        <label className={styles.check}>
          <input
            type="radio"
            checked={profile.tools.mode === 'allowlist'}
            onChange={() => onToolsChange({ mode: 'allowlist', names: allowlist })}
          />
          <span>{t('subagentsToolsAllowlistOption')}</span>
        </label>
      </div>
      {profile.tools.mode === 'allowlist' && (
        <>
          <p className={styles.hint}>
            {allowlist.length === 0 ? t('subagentsToolsNone') : t('subagentsToolsCount').replace('{n}', String(allowlist.length))}
          </p>
          <input
            className={styles.search}
            value={toolFilter}
            placeholder={t('subagentsSearch')}
            onChange={(event) => setToolFilter(event.target.value)}
          />
          <div className={styles.tools}>
            {categories.map((category) => (
              <div key={category}>
                <div className={styles.toolCategory}>{category}</div>
                {visible
                  .filter((row) => row.category === category)
                  .map((row) => (
                    <label
                      key={row.name}
                      className={styles.toolRow}
                      title={row.reason}
                      data-tool={row.name}
                    >
                      <input
                        type="checkbox"
                        checked={allowlist.includes(row.name)}
                        disabled={rowIsDisabled(row)}
                        onChange={() => toggleTool(row.name)}
                      />
                      <span className={styles.toolName}>{row.name}</span>
                      <span className={styles.toolEffect}>{row.effect}</span>
                      <span className={styles.toolReason}>
                        {row.hardDenied
                          ? t('subagentsToolHardDenied')
                          : row.granted === false
                            ? t('subagentsToolUngranted')
                            : row.reason}
                      </span>
                    </label>
                  ))}
              </div>
            ))}
          </div>
          {unknown.length > 0 && (
            <div className={styles.conflict} role="alert" data-testid="unknown-tools">
              <strong>{t('subagentsUnknownTools')}</strong>
              <p className={styles.hint}>{t('subagentsUnknownToolsHint')}</p>
              <ul>
                {unknown.map((name) => (
                  <li key={name}>
                    <code>{name}</code>
                    <button
                      type="button"
                      onClick={() =>
                        onToolsChange({
                          mode: 'allowlist',
                          names: toggleToolName(allowlist, name),
                        })
                      }
                    >
                      {t('subagentsRemoveTool')}
                    </button>
                  </li>
                ))}
              </ul>
            </div>
          )}
          {conflicts.length > 0 && (
            <div className={styles.conflict} role="alert" data-testid="readonly-conflicts">
              <strong>{t('subagentsReadOnlyConflict')}</strong>
              <p className={styles.hint}>{t('subagentsReadOnlyConflictHint')}</p>
              <code>{conflicts.join('、')}</code>
              <div className={styles.actions}>
                <button type="button" onClick={() => onChange({ permissionCeiling: 'inherit' })}>
                  {t('subagentsUseInheritCeiling')}
                </button>
                <button
                  type="button"
                  onClick={() =>
                    onToolsChange({
                      mode: 'allowlist',
                      names: allowlist.filter((name) => !conflicts.includes(name)),
                    })
                  }
                >
                  {t('subagentsDropConflicts')}
                </button>
              </div>
            </div>
          )}
        </>
      )}

      <h5>{t('subagentsModelTitle')}</h5>
      <div className={styles.inline}>
        <label className={styles.check}>
          <input
            type="radio"
            checked={profile.model.mode === 'inherit'}
            onChange={() => onModelChange({ mode: 'inherit' })}
          />
          <span>{t('subagentsModelInherit')}</span>
        </label>
        <label className={styles.check}>
          <input
            type="radio"
            checked={profile.model.mode === 'explicit'}
            onChange={() =>
              onModelChange({
                mode: 'explicit',
                selection: { provider: '', model: '', reasoningEffort: undefined },
              })
            }
          />
          <span>{t('subagentsModelExplicit')}</span>
        </label>
      </div>
      {profile.model.mode === 'explicit' && (
        <ExplicitModelFields model={profile.model.selection} onChange={onModelChange} />
      )}

      <div className={styles.actions}>
        <button type="button" className={styles.primary} disabled={saving} onClick={onSave}>
          {t('subagentsSave')}
        </button>
        <button type="button" disabled={saving} onClick={onPreview}>
          {t('subagentsPreview')}
        </button>
        {draft.dirty && <span className={styles.hint}>{t('subagentsDraftChanged')}</span>}
      </div>

      {preview && (
        <div className={styles.preview} data-testid="subagent-preview">
          <h5>{t('subagentsPreviewTitle')}</h5>
          {preview.final ? (
            <>
              <p className={styles.rowMeta}>
                {preview.profile?.qualifiedId} · {t('subagentsPreviewTitle')}
              </p>
              <p className={styles.hint}>
                {preview.model?.provider}/{preview.model?.model} ·{' '}
                {preview.permissionCeiling === 'read-only'
                  ? t('subagentsCeilingReadOnly')
                  : t('subagentsCeilingInherit')}{' '}
                · {preview.instructionScope}
              </p>
              <div className={styles.toolList}>
                {(preview.tools ?? []).map((name) => (
                  <code key={name}>{name}</code>
                ))}
                {(preview.tools ?? []).length === 0 && <span>{t('subagentsToolsNone')}</span>}
              </div>
            </>
          ) : (
            <p className={styles.hint}>
              {preview.note ?? t('subagentsPreviewHint')}（{t('subagentsPreviewNoSession')}）
            </p>
          )}
        </div>
      )}
    </div>
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
    <div className={styles.inline}>
      <label className={styles.field}>
        <span>provider</span>
        <input value={model.provider} onChange={(event) => patch({ provider: event.target.value })} />
      </label>
      <label className={styles.field}>
        <span>model</span>
        <input value={model.model} onChange={(event) => patch({ model: event.target.value })} />
      </label>
      <label className={styles.field}>
        <span>effort</span>
        <input
          value={model.reasoningEffort ?? ''}
          onChange={(event) => patch({ reasoningEffort: event.target.value || undefined })}
        />
      </label>
    </div>
  )
}
