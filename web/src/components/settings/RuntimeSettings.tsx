import { useEffect, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { Button, Field, NumberInput } from '../llm/atoms/form'
import styles from './RuntimeSettings.module.css'

// 旧的子代理字段（maxAgents / maxDepth）已删：并发上限迁到「子代理」页的
// subagent-policy 命名空间；委派深度不复存在——禁止子代理再派遣是运行时
// 硬规则，不是可调参数。
const fields = [
  ['maxJobs', 'runtimeMaxJobs'], ['retainedJobs', 'runtimeRetainedJobs'],
  ['outputBytes', 'runtimeOutputBytes'],
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
      // 整体替换前剔除已废弃字段：旧客户端带来的 maxAgents/maxDepth 会让
      // 严格反序列化失败，这里主动不把它们写回配置。
      const next: Record<string, unknown> = { ...(value ?? {}) }
      delete next.maxAgents
      delete next.maxDepth
      await api.replaceNamespace('runtime', next, revision)
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
      <p className={styles.hint}>{t('runtimeConfigHint')}</p>
      <div className={styles.actions}>
        <Button variant="primary" type="submit" disabled={saving}>{saving ? t('loading') : t('save')}</Button>
        {saved && <span className={styles.saved} role="status">{t('runtimeSaved')}</span>}
      </div>
    </form>}
  </section>
}
