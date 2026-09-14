import { useCallback, useEffect, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { IconCheck, IconPlus, IconTrash } from '../icons'
import styles from './AgentPresetSettings.module.css'

/** 默认 preset 所在的设置命名空间(与服务端 `agent_presets::SETTINGS_NS` 同名)。 */
const NS = 'agent-presets'

type Notify = (kind: 'ok' | 'err', text: string) => void

/**
 * Agent 组装(preset)设置分区:名册、默认值、复制创作、删除与只读查看。
 *
 * 创作路径只有复制(抄 dsh):调用方从不提交组装文本,因此一次复制不会
 * 授予名册尚未携带的能力;改组装用用户自己的编辑器改 `preset.yml`。
 * 文件变化由服务端监听并广播,这里收到 `agent-presets-updated` 后重拉。
 */
export function AgentPresetSettings({ notify }: { notify: Notify }) {
  const [roster, setRoster] = useState<api.AgentPresetsView | null>(null)
  const [revision, setRevision] = useState(0)
  const [defaultId, setDefaultId] = useState('')
  const [busy, setBusy] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null)
  const [copyFrom, setCopyFrom] = useState<api.AgentPresetRow | null>(null)
  const [copyId, setCopyId] = useState('')
  const [copyName, setCopyName] = useState('')
  const [viewer, setViewer] = useState<{ id: string; text: string } | null>(null)

  const load = useCallback(async () => {
    try {
      const [views, settings] = await Promise.all([api.listAgentPresets(), api.getSettings()])
      setRoster(views)
      const section = settings.namespaces.find((ns) => ns.ns === NS)
      if (section) {
        setRevision(section.revision)
        setDefaultId(typeof section.value.default === 'string' ? section.value.default : views.default)
      } else {
        setDefaultId(views.default)
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

  const startCopy = (preset: api.AgentPresetRow) => {
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
      setCopyFrom(null)
      setViewer({ id, text })
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }

  const presets = roster?.presets ?? []

  return (
    <div className={styles.wrap}>
      <p className={styles.hint}>{t('agentPresetsHint')}</p>
      {roster && roster.root && (
        <div className={styles.root}>
          <span className={styles.rootLabel}>{t('agentPresetsRoot')}</span>
          <code>{roster.root}</code>
        </div>
      )}

      {presets.length === 0 && <div className={styles.empty}>{t('agentPresetsLoading')}</div>}

      <ul className={styles.list}>
        {presets.map((preset) => {
          const selected = preset.id === defaultId
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
                </div>
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
