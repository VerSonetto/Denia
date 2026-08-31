import { t } from '../i18n'
import type { ModelCatalog, ModelSelection } from '../types'

/**
 * Provider / model / effort drill-down driven by the live catalog.
 * Controlled: the parent owns the selection.
 */
export function ModelPicker({
  catalog,
  value,
  onChange,
}: {
  catalog: ModelCatalog
  value: ModelSelection
  onChange: (next: ModelSelection) => void
}) {
  const group = catalog.groups.find((g) => g.id === value.provider)
  const model = group?.models.find((m) => m.id === value.model)
  const efforts = model?.reasoning?.efforts ?? []

  return (
    <div className="row">
      <div className="field">
        <label>{t('providerLabel')}</label>
        <select
          className="plain"
          value={value.provider}
          onChange={(event) => {
            const provider = event.target.value
            const nextGroup = catalog.groups.find((g) => g.id === provider)
            const firstModel = nextGroup?.models[0]
            onChange({
              provider,
              model: firstModel?.id ?? '',
              reasoningEffort: firstModel?.reasoning?.defaultEffort,
            })
          }}
        >
          {catalog.groups.map((g) => (
            <option key={g.id} value={g.id}>
              {g.name}
            </option>
          ))}
        </select>
      </div>
      <div className="field">
        <label>{t('modelLabel')}</label>
        <select
          className="plain"
          value={value.model}
          onChange={(event) => {
            const nextModel = group?.models.find((m) => m.id === event.target.value)
            onChange({
              ...value,
              model: event.target.value,
              reasoningEffort: nextModel?.reasoning?.defaultEffort ?? value.reasoningEffort,
            })
          }}
        >
          {(group?.models ?? []).map((m) => (
            <option key={m.id} value={m.id}>
              {m.name}
            </option>
          ))}
        </select>
      </div>
      {efforts.length > 0 && (
        <div className="field narrow">
          <label>{t('reasoningLabel')}</label>
          <select
            className="plain"
            value={value.reasoningEffort ?? ''}
            onChange={(event) => {
              const next = event.target.value
              onChange({ ...value, reasoningEffort: next || undefined })
            }}
          >
            <option value="">{t('providerDefault')}</option>
            {efforts.map((effort) => (
              <option key={effort.id} value={effort.id}>
                {effort.name}
              </option>
            ))}
          </select>
        </div>
      )}
    </div>
  )
}
