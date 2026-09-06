import { useEffect, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { Button, Field, NumberInput } from '../llm/atoms/form'
import styles from './RuntimeSettings.module.css'

const fields = [
  ['maxAgents', 'runtimeMaxAgents'], ['maxDepth', 'runtimeMaxDepth'],
  ['maxJobs', 'runtimeMaxJobs'], ['retainedJobs', 'runtimeRetainedJobs'],
  ['outputBytes', 'runtimeOutputBytes'], ['maxWaitMs', 'runtimeMaxWait'],
  ['jobTimeoutMs', 'runtimeJobTimeout'], ['maxPendingMessages', 'runtimePending'],
  ['maxConsecutiveWakes', 'runtimeWakes'],
] as const

export function RuntimeSettings() {
  const [value, setValue] = useState<Record<string, unknown> | null>(null)
  const [revision, setRevision] = useState(0)
  const [error, setError] = useState('')
  const [saving, setSaving] = useState(false)
  const [saved, setSaved] = useState(false)
  useEffect(() => {
    let disposed = false
    api.getSettings().then(d => {
      const ns = d.namespaces.find(n => n.ns === 'runtime')
      if (ns && !disposed) { setValue(ns.value); setRevision(ns.revision) }
    }).catch(e => { if (!disposed) setError(String(e)) })
    return () => { disposed = true }
  }, [])
  const save = async () => {
    // 与原生 required 语义对齐:任一数字字段为空就不提交。
    if (value && fields.some(([key]) => typeof value[key] !== 'number')) {
      setError(t('llm.error.numberPositive'))
      return
    }
    setSaving(true); setError(''); setSaved(false)
    try {
      await api.replaceNamespace('runtime', value, revision)
      const ns = (await api.getSettings()).namespaces.find(n => n.ns === 'runtime')
      if (ns) { setRevision(ns.revision); setValue(ns.value) }
      setSaved(true)
    } catch (e) { setError(e instanceof Error ? e.message : String(e)) } finally { setSaving(false) }
  }
  return <section className="setm-section">
    {error && <p className={styles.error} role="alert">{error}</p>}
    {value && <form onSubmit={e => { e.preventDefault(); void save() }}>
      <div className={styles.grid}>
        {fields.map(([key, label]) => (
          <Field key={key} label={t(label)}>
            <NumberInput
              mono
              value={typeof value[key] === 'number' ? (value[key] as number) : undefined}
              onChange={(next) => { setSaved(false); setValue({ ...value, [key]: next }) }}
            />
          </Field>
        ))}
      </div>
      {/* customSkillDirs 不再提供编辑入口;value 原样透传,保存不丢已有配置。 */}
      <p className={styles.hint}>{t('runtimeConfigHint')}</p>
      <div className={styles.actions}>
        <Button variant="primary" type="submit" disabled={saving}>{saving ? t('loading') : t('save')}</Button>
        {saved && <span className={styles.saved} role="status">{t('runtimeSaved')}</span>}
      </div>
    </form>}
  </section>
}
