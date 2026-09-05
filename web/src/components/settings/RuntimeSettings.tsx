import { useEffect, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'

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
    setSaving(true); setError(''); setSaved(false)
    try {
      await api.replaceNamespace('runtime', value, revision)
      const ns = (await api.getSettings()).namespaces.find(n => n.ns === 'runtime')
      if (ns) { setRevision(ns.revision); setValue(ns.value) }
      setSaved(true)
    } catch (e) { setError(e instanceof Error ? e.message : String(e)) } finally { setSaving(false) }
  }
  return <section className="setm-section">
    {error && <p role="alert">{error}</p>}
    {value && <form onSubmit={e => { e.preventDefault(); void save() }}>
      {fields.map(([key, label]) => <label key={key} style={{ display: 'flex', justifyContent: 'space-between', gap: 16, marginBottom: 12 }}>{t(label)}<input type="number" min={1} step={1} required value={Number(value[key])} onChange={e => { setSaved(false); setValue({ ...value, [key]: e.target.valueAsNumber }) }} /></label>)}
      <label>{t('runtimeCustomDirs')}<textarea className="setm-textarea" rows={4} value={(value.customSkillDirs as string[]).join('\n')} onChange={e => { setSaved(false); setValue({ ...value, customSkillDirs: e.target.value.split('\n').map(s => s.trim()).filter(Boolean) }) }} /></label>
      <p>{t('runtimeConfigHint')}</p>
      <button type="submit" disabled={saving}>{saving ? t('loading') : t('save')}</button>{saved && <span role="status"> {t('runtimeSaved')}</span>}
    </form>}
  </section>
}
