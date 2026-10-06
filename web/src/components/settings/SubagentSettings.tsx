import { useCallback, useEffect, useMemo, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { Badge, Button, Field, NumberInput, TextInput } from '../llm/atoms/form'
import styles from './SubagentSettings.module.css'

type Profile = Record<string, unknown>
type Row = api.SubagentProfileRowView

/** 编辑器的草稿:与服务端定义同形,另加 UI 侧的作用域与 id。 */
interface Draft {
  qualifiedId: string | null
  scope: 'user' | 'project'
  id: string
  revision: string | null
  name: string
  description: string
  instructions: string
  enabled: boolean
  toolsMode: 'inherit' | 'allowlist'
  tools: string[]
  modelMode: 'inherit' | 'explicit'
  provider: string
  model: string
  effort: string
  ceiling: 'inherit' | 'read-only'
}

function draftFrom(row?: Row): Draft {
  const profile = (row?.profile ?? {}) as Profile
  const tools = profile.tools as { mode: string; names?: string[] } | undefined
  const model = profile.model as
    | { mode: string; selection?: { provider: string; model: string; reasoningEffort?: string } }
    | undefined
  return {
    qualifiedId: row?.qualifiedId ?? null,
    scope: row && row.source === 'project' ? 'project' : 'user',
    id: row?.id ?? '',
    revision: row?.revision ?? null,
    name: (profile.name as string) ?? '',
    description: (profile.description as string) ?? '',
    instructions: (profile.instructions as string) ?? '',
    enabled: (profile.enabled as boolean) ?? true,
    toolsMode: tools?.mode === 'allowlist' ? 'allowlist' : 'inherit',
    tools: tools?.names ?? [],
    modelMode: model?.mode === 'explicit' ? 'explicit' : 'inherit',
    provider: model?.selection?.provider ?? '',
    model: model?.selection?.model ?? '',
    effort: model?.selection?.reasoningEffort ?? '',
    ceiling: profile.permissionCeiling === 'read-only' ? 'read-only' : 'inherit',
  }
}

function bodyOf(draft: Draft): Record<string, unknown> {
  return {
    schemaVersion: 1,
    id: draft.id.trim(),
    name: draft.name.trim(),
    description: draft.description.trim(),
    instructions: draft.instructions,
    enabled: draft.enabled,
    tools:
      draft.toolsMode === 'inherit'
        ? { mode: 'inherit' }
        : { mode: 'allowlist', names: draft.tools },
    model:
      draft.modelMode === 'inherit'
        ? { mode: 'inherit' }
        : {
            mode: 'explicit',
            selection: {
              provider: draft.provider.trim(),
              model: draft.model.trim(),
              ...(draft.effort.trim() ? { reasoningEffort: draft.effort.trim() } : {}),
            },
          },
    permissionCeiling: draft.ceiling,
  }
}

export function SubagentSettings() {
  const [rows, setRows] = useState<Row[]>([])
  const [paths, setPaths] = useState<{ user: string; project?: string; deprecations?: string }>({
    user: '',
  })
  const [tools, setTools] = useState<api.SubagentToolEntry[]>([])
  const [query, setQuery] = useState('')
  const [draft, setDraft] = useState<Draft | null>(null)
  const [error, setError] = useState('')
  const [notice, setNotice] = useState('')
  const [busy, setBusy] = useState(false)
  const [conflict, setConflict] = useState(false)
  const [maxConcurrent, setMaxConcurrent] = useState<number | undefined>()
  const [policyRevision, setPolicyRevision] = useState(0)

  const load = useCallback(async () => {
    try {
      const view = await api.listSubagentProfiles()
      setRows(view.profiles)
      setPaths({
        user: view.userRoot,
        project: view.projectRoot,
        deprecations: view.deprecations,
      })
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    }
  }, [])

  useEffect(() => {
    void load()
    api
      .listSubagentTools()
      .then(d => setTools(d.tools))
      .catch(() => setTools([]))
    api
      .getSettings()
      .then(d => {
        const ns = d.namespaces.find(n => n.ns === 'subagent-policy')
        if (!ns) return
        setPolicyRevision(ns.revision)
        const value = ns.value as { maxConcurrentRuns?: number }
        setMaxConcurrent(value.maxConcurrentRuns)
      })
      .catch(() => {})
  }, [load])

  const grantable = useMemo(() => tools.filter(tool => tool.grantable), [tools])
  const filtered = useMemo(() => {
    const needle = query.trim().toLowerCase()
    if (!needle) return rows
    return rows.filter(
      row =>
        row.id.toLowerCase().includes(needle) ||
        ((row.profile?.name as string | undefined) ?? '').toLowerCase().includes(needle) ||
        ((row.profile?.description as string | undefined) ?? '')
          .toLowerCase()
          .includes(needle),
    )
  }, [rows, query])

  const save = async () => {
    if (!draft) return
    setBusy(true)
    setError('')
    setNotice('')
    setConflict(false)
    try {
      const body = {
        scope: draft.scope,
        profile: bodyOf(draft),
        ...(draft.revision ? { expectedRevision: draft.revision } : {}),
      }
      const result = draft.qualifiedId
        ? await api.updateSubagentProfile(draft.qualifiedId, body)
        : await api.createSubagentProfile(body)
      setDraft(draftFrom(result.profile))
      setNotice(t('subagentsSaved'))
      await load()
    } catch (e) {
      // 保存冲突:保留草稿,提示重新加载(不能覆盖另一窗口的修改)。
      const message = e instanceof Error ? e.message : String(e)
      setError(message)
      if (message.includes('revision-conflict') || message.includes('已被其他窗口')) {
        setConflict(true)
      }
    } finally {
      setBusy(false)
    }
  }

  const toggleEnabled = async (row: Row) => {
    if (!row.profile) return
    setBusy(true)
    setError('')
    try {
      await api.updateSubagentProfile(row.qualifiedId, {
        scope: row.source === 'builtin' ? 'user' : row.source,
        profile: { ...(row.profile as Record<string, unknown>), enabled: !row.profile.enabled },
        expectedRevision: row.revision,
      })
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  const copyRow = async (row: Row) => {
    const next = draftFrom(row)
    const suffix = next.id === '' ? 'copy' : `${next.id}-copy`.slice(0, 64)
    setDraft({
      ...next,
      qualifiedId: null,
      revision: null,
      scope: 'user',
      id: suffix,
      name: `${next.name} 副本`,
    })
    setNotice('')
  }

  const removeRow = async (row: Row) => {
    setBusy(true)
    setError('')
    try {
      await api.deleteSubagentProfile(row.qualifiedId)
      if (draft?.qualifiedId === row.qualifiedId) setDraft(null)
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  const resetRow = async (row: Row) => {
    setBusy(true)
    setError('')
    try {
      await api.resetSubagentProfile(`user:${row.id}`)
      await load()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  const savePolicy = async () => {
    setBusy(true)
    setError('')
    try {
      await api.replaceNamespace('subagent-policy', { maxConcurrentRuns: maxConcurrent }, policyRevision)
      const ns = (await api.getSettings()).namespaces.find(n => n.ns === 'subagent-policy')
      if (ns) {
        setPolicyRevision(ns.revision)
        setMaxConcurrent((ns.value as { maxConcurrentRuns?: number }).maxConcurrentRuns)
      }
      setNotice(t('subagentsSaved'))
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="setm-section">
      {error && (
        <p className={styles.error} role="alert">
          {error}
          {conflict && <span> {t('subagentsConflict')}</span>}
        </p>
      )}
      {notice && (
        <p className={styles.saved} role="status">
          {notice}
        </p>
      )}
      {paths.deprecations && <p className={styles.hint}>{paths.deprecations}</p>}
      <p className={styles.hint}>
        {t('subagentsRootHint')}：<code>{paths.user}</code>
        {paths.project && (
          <>
            {' · '}
            {t('subagentsProjectHint')}：<code>{paths.project}</code>
          </>
        )}
      </p>

      <div className={styles.toolbar}>
        <TextInput
          value={query}
          onChange={setQuery}
          placeholder={t('subagentsSearch')}
        />
        <Button variant="ghost" type="button" onClick={() => setDraft(draftFrom())}>
          {t('subagentsNew')}
        </Button>
      </div>

      <ul className={styles.list}>
        {filtered.map(row => (
          <li
            key={row.qualifiedId}
            className={`${styles.row}${draft?.qualifiedId === row.qualifiedId ? ` ${styles.active}` : ''}`}
          >
            <button
              type="button"
              className={styles.rowMain}
              onClick={() => setDraft(draftFrom(row))}
            >
              <span className={styles.rowName}>
                {(row.profile?.name as string | undefined) ?? row.id}
                <Badge>{sourceLabel(row.source)}</Badge>
                {row.overridesBuiltin && <Badge>{t('subagentsOverridesBuiltin')}</Badge>}
                {row.shadowed && <Badge>{t('subagentsShadowed')}</Badge>}
                {row.broken && <Badge>{t('subagentsBroken')}</Badge>}
                {row.profile && !row.profile.enabled && <Badge>{t('subagentsDisabledBadge')}</Badge>}
              </span>
              <span className={styles.rowMeta}>{row.id}</span>
              <span className={styles.rowDesc}>
                {row.broken ??
                  (row.profile?.description as string | undefined) ??
                  ''}
              </span>
              <span className={styles.rowMeta}>{summarize(row)}</span>
            </button>
            <span className={styles.rowActions}>
              <Button variant="ghost" type="button" disabled={busy || !row.profile} onClick={() => void toggleEnabled(row)}>
                {row.profile?.enabled ? t('subagentsDisable') : t('subagentsEnable')}
              </Button>
              <Button variant="ghost" type="button" disabled={busy || !row.profile} onClick={() => void copyRow(row)}>
                {t('subagentsCopy')}
              </Button>
              {row.source === 'builtin' ? (
                <Button variant="ghost" type="button" disabled={busy || !row.overridesBuiltin} onClick={() => void resetRow(row)}>
                  {t('subagentsReset')}
                </Button>
              ) : (
                <Button variant="ghost" type="button" disabled={busy} onClick={() => void removeRow(row)}>
                  {t('subagentsDelete')}
                </Button>
              )}
            </span>
          </li>
        ))}
        {filtered.length === 0 && <li className={styles.empty}>{t('subagentsEmpty')}</li>}
      </ul>

      {draft && (
        <form
          className={styles.editor}
          onSubmit={e => {
            e.preventDefault()
            void save()
          }}
        >
          <p className={styles.hint}>{t('subagentsEditNote')}</p>
          <div className={styles.grid}>
            <Field label={t('subagentsName')}>
              <TextInput value={draft.name} onChange={v => setDraft({ ...draft, name: v })} />
            </Field>
            <Field label={t('subagentsId')}>
              <TextInput
                mono
                value={draft.id}
                disabled={draft.qualifiedId !== null}
                onChange={v => setDraft({ ...draft, id: v })}
              />
            </Field>
            <Field label={t('subagentsScope')}>
              <select
                className={styles.select}
                value={draft.scope}
                onChange={e =>
                  setDraft({ ...draft, scope: e.target.value as 'user' | 'project' })
                }
              >
                <option value="user">{t('subagentsUser')}</option>
                <option value="project">{t('subagentsProject')}</option>
              </select>
            </Field>
            <Field label={t('subagentsCeiling')}>
              <select
                className={styles.select}
                value={draft.ceiling}
                onChange={e =>
                  setDraft({ ...draft, ceiling: e.target.value as 'inherit' | 'read-only' })
                }
              >
                <option value="inherit">{t('subagentsCeilingInherit')}</option>
                <option value="read-only">{t('subagentsCeilingReadOnly')}</option>
              </select>
            </Field>
          </div>
          <Field label={t('subagentsDescription')}>
            <TextInput
              value={draft.description}
              onChange={v => setDraft({ ...draft, description: v })}
            />
          </Field>
          <Field label={t('subagentsInstructions')}>
            <textarea
              className={styles.textarea}
              rows={5}
              value={draft.instructions}
              onChange={e => setDraft({ ...draft, instructions: e.target.value })}
            />
          </Field>
          <div className={styles.grid}>
            <Field label={t('subagentsToolMode')} hint={t('subagentsToolsHint')}>
              <select
                className={styles.select}
                value={draft.toolsMode}
                onChange={e =>
                  setDraft({ ...draft, toolsMode: e.target.value as 'inherit' | 'allowlist' })
                }
              >
                <option value="inherit">{t('subagentsToolsInherit')}</option>
                <option value="allowlist">{t('subagentsToolsAllowlist')}</option>
              </select>
            </Field>
            <Field label={t('subagentsModelMode')}>
              <select
                className={styles.select}
                value={draft.modelMode}
                onChange={e =>
                  setDraft({ ...draft, modelMode: e.target.value as 'inherit' | 'explicit' })
                }
              >
                <option value="inherit">{t('subagentsInheritModel')}</option>
                <option value="explicit">{t('subagentsExplicitModel')}</option>
              </select>
            </Field>
          </div>
          {draft.modelMode === 'explicit' && (
            <div className={styles.grid}>
              <Field label="provider">
                <TextInput mono value={draft.provider} onChange={v => setDraft({ ...draft, provider: v })} />
              </Field>
              <Field label="model">
                <TextInput mono value={draft.model} onChange={v => setDraft({ ...draft, model: v })} />
              </Field>
              <Field label={t('subagentsEffort')}>
                <TextInput mono value={draft.effort} onChange={v => setDraft({ ...draft, effort: v })} />
              </Field>
            </div>
          )}
          {draft.toolsMode === 'allowlist' && (
            <fieldset className={styles.tools}>
              <legend>{t('subagentsToolsAllowlist')}</legend>
              {grantable.length === 0 && <p className={styles.hint}>{t('subagentsNoGrantable')}</p>}
              {grantable.map(tool => (
                <label key={tool.name} className={styles.toolRow}>
                  <input
                    type="checkbox"
                    checked={draft.tools.includes(tool.name)}
                    onChange={e =>
                      setDraft({
                        ...draft,
                        tools: e.target.checked
                          ? [...draft.tools, tool.name].sort()
                          : draft.tools.filter(name => name !== tool.name),
                      })
                    }
                  />
                  <span className={styles.toolName}>{tool.name}</span>
                  <span className={styles.rowMeta}>{tool.category}</span>
                </label>
              ))}
              {draft.tools.length === 0 && <p className={styles.hint}>{t('subagentsNoTools')}</p>}
            </fieldset>
          )}
          <div className={styles.actions}>
            <Button variant="primary" type="submit" disabled={busy}>
              {busy ? t('loading') : t('save')}
            </Button>
            <Button variant="ghost" type="button" onClick={() => void load()}>
              {t('subagentsReload')}
            </Button>
          </div>
        </form>
      )}

      <div className={styles.policy}>
        <h4>{t('subagentsPolicyTitle')}</h4>
        <Field label={t('subagentsMaxConcurrent')} hint={t('subagentsPolicyHint')}>
          <NumberInput value={maxConcurrent} onChange={setMaxConcurrent} />
        </Field>
        <Button variant="primary" type="button" disabled={busy} onClick={() => void savePolicy()}>
          {t('save')}
        </Button>
      </div>
    </section>
  )
}

/** 列表行的一句话摘要:工具模式 + 模型。 */
function summarize(row: Row): string {
  if (!row.profile) return ''
  const tools = row.profile.tools as { mode: string; names?: string[] }
  const toolText =
    tools.mode === 'inherit'
      ? t('subagentsInheritTools')
      : tools.names && tools.names.length > 0
        ? `${tools.names.length} tools`
        : t('subagentsNoTools')
  const model = row.profile.model as { mode: string; selection?: { provider: string; model: string } }
  const modelText =
    model.mode === 'inherit'
      ? t('subagentsInheritModel')
      : `${model.selection?.provider ?? ''}/${model.selection?.model ?? ''}`
  return `${toolText} · ${modelText}`
}

/** 来源标签:定义来自哪个作用域。 */
function sourceLabel(source: Row['source']): string {
  if (source === 'builtin') return t('subagentsBuiltin')
  if (source === 'project') return t('subagentsProject')
  return t('subagentsUser')
}
