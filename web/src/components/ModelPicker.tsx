import { resolveSessionReasoningEffort } from '../modelCatalog'
import { reasoningEffortLabel } from '../reasoningEffort'
import { t } from '../i18n'
import type { ModelCatalog, ModelSelection } from '../types'
import { DropdownField, SegmentControl } from './ui/controls'

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

  const providerOptions = catalog.groups.map((g) => ({
    id: g.id,
    label: g.name,
  }))

  const modelOptions = (group?.models ?? []).map((m) => ({
    id: m.id,
    label: m.name,
    hint: m.description,
  }))

  const activeEffort = resolveSessionReasoningEffort(efforts, value.reasoningEffort)

  const effortOptions = efforts.map((effort) => ({
    id: effort.id,
    label: reasoningEffortLabel(effort.id),
    hint: effort.description,
  }))

  return (
    <div className="row">
      <DropdownField
        label={t('providerLabel')}
        value={value.provider}
        options={providerOptions}
        placeholder={t('noModels')}
        onChange={(provider) => {
          const nextGroup = catalog.groups.find((g) => g.id === provider)
          const firstModel = nextGroup?.models[0]
          onChange({
            provider,
            model: firstModel?.id ?? '',
            reasoningEffort: resolveSessionReasoningEffort(firstModel?.reasoning?.efforts ?? []),
          })
        }}
      />
      <DropdownField
        label={t('modelLabel')}
        value={value.model}
        options={modelOptions}
        placeholder={t('noModels')}
        onChange={(modelId) => {
          const nextModel = group?.models.find((m) => m.id === modelId)
          onChange({
            ...value,
            model: modelId,
            reasoningEffort: resolveSessionReasoningEffort(
              nextModel?.reasoning?.efforts ?? [],
              value.reasoningEffort,
            ),
          })
        }}
      />
      {efforts.length > 0 && (
        <div className="field narrow">
          <label>{t('reasoningLabel')}</label>
          <SegmentControl
            value={activeEffort ?? ''}
            options={effortOptions}
            onChange={(next) => onChange({ ...value, reasoningEffort: next })}
          />
        </div>
      )}
    </div>
  )
}
