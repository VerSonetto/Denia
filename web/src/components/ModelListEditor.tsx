import { useMemo } from 'react'
import { t } from '../i18n'
import {
  REASONING_EFFORT_IDS,
  REASONING_INTENSITY_IDS,
  type CatalogModelEntry,
  type ReasoningEffortId,
  defaultReasoningEfforts,
  entryFromDiscovered,
  formatContextWindow,
  hasVision,
  setVision,
} from '../modelCatalog'
import { reasoningEffortLabel } from '../reasoningEffort'
import type { DiscoveredModel } from '../types'
import { NumberField, TextField, ToggleCell } from './ui/controls'

function toggleEffort(
  efforts: ReasoningEffortId[] | undefined,
  effort: ReasoningEffortId,
  enabled: boolean,
): ReasoningEffortId[] {
  const current = new Set(efforts ?? defaultReasoningEfforts())
  if (enabled) {
    current.add(effort)
    if (effort === 'off') {
      for (const id of REASONING_INTENSITY_IDS) current.delete(id)
    } else {
      current.delete('off')
    }
  } else {
    current.delete(effort)
    if (current.size === 0) current.add('off')
  }
  return REASONING_EFFORT_IDS.filter((id) => current.has(id))
}

function updateEntry(
  models: CatalogModelEntry[],
  index: number,
  patch: Partial<CatalogModelEntry>,
): CatalogModelEntry[] {
  return models.map((entry, i) => (i === index ? { ...entry, ...patch } : entry))
}

export default function ModelListEditor({
  models,
  onChange,
  discovered,
  defaultContextWindow,
  disabled,
}: {
  models: CatalogModelEntry[]
  onChange: (models: CatalogModelEntry[]) => void
  discovered?: DiscoveredModel[] | null
  defaultContextWindow?: number
  disabled?: boolean
}) {
  const unadded = useMemo(
    () =>
      (discovered ?? []).filter(
        (model) => !models.some((entry) => entry.id === model.id),
      ),
    [discovered, models],
  )

  const addBlank = () => {
    onChange([
      ...models,
      {
        id: '',
        inputModalities: ['text'],
        thinkingSupported: false,
        contextWindow: defaultContextWindow,
      },
    ])
  }

  const importDiscovered = (model: DiscoveredModel) => {
    onChange([...models, entryFromDiscovered(model.id, model.name)])
  }

  const importAllDiscovered = () => {
    const next = [...models]
    for (const model of unadded) {
      next.push(entryFromDiscovered(model.id, model.name))
    }
    onChange(next)
  }

  return (
    <div className="model-list-editor">
      <div className="model-table-wrap">
        <table className="model-table">
          <thead>
            <tr>
              <th>{t('modelIdColumn')}</th>
              <th>{t('modelNameColumn')}</th>
              <th>{t('contextWindowColumn')}</th>
              <th className="center">{t('visionColumn')}</th>
              <th className="center">{t('thinkingColumn')}</th>
              <th>{t('effortLevelsColumn')}</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {models.length === 0 && (
              <tr>
                <td colSpan={7} className="empty">
                  {t('noModelsConfigured')}
                </td>
              </tr>
            )}
            {models.map((entry, index) => (
              <tr key={`${entry.id || 'new'}-${index}`}>
                <td>
                  <TextField
                    mono
                    placeholder={t('modelIdPlaceholder')}
                    value={entry.id}
                    disabled={disabled}
                    onChange={(id) => onChange(updateEntry(models, index, { id }))}
                  />
                </td>
                <td>
                  <TextField
                    placeholder={entry.id || t('modelNamePlaceholder')}
                    value={entry.name ?? ''}
                    disabled={disabled}
                    onChange={(name) =>
                      onChange(
                        updateEntry(models, index, {
                          name: name || undefined,
                        }),
                      )
                    }
                  />
                </td>
                <td>
                  <NumberField
                    mono
                    narrow
                    placeholder={
                      defaultContextWindow
                        ? formatContextWindow(defaultContextWindow)
                        : t('contextWindowPlaceholder')
                    }
                    value={entry.contextWindow}
                    disabled={disabled}
                    onChange={(contextWindow) =>
                      onChange(updateEntry(models, index, { contextWindow }))
                    }
                  />
                </td>
                <td className="center">
                  <ToggleCell
                    checked={hasVision(entry)}
                    disabled={disabled}
                    title={t('visionColumn')}
                    onChange={(enabled) =>
                      onChange(updateEntry(models, index, setVision(entry, enabled)))
                    }
                  />
                </td>
                <td className="center">
                  <ToggleCell
                    checked={entry.thinkingSupported === true}
                    disabled={disabled}
                    title={t('thinkingColumn')}
                    onChange={(enabled) => {
                      onChange(
                        updateEntry(models, index, {
                          thinkingSupported: enabled,
                          reasoningEfforts: enabled ? defaultReasoningEfforts() : undefined,
                        }),
                      )
                    }}
                  />
                </td>
                <td>
                  {entry.thinkingSupported ? (
                    <div className="effort-chips">
                      {REASONING_EFFORT_IDS.map((effort) => {
                        const active = (entry.reasoningEfforts ?? defaultReasoningEfforts()).includes(
                          effort,
                        )
                        return (
                          <button
                            key={effort}
                            type="button"
                            className={`effort-chip${active ? ' active' : ''}`}
                            disabled={disabled}
                            onClick={() =>
                              onChange(
                                updateEntry(models, index, {
                                  reasoningEfforts: toggleEffort(
                                    entry.reasoningEfforts,
                                    effort,
                                    !active,
                                  ),
                                }),
                              )
                            }
                          >
                            {reasoningEffortLabel(effort)}
                          </button>
                        )
                      })}
                    </div>
                  ) : (
                    <span className="hint-inline">—</span>
                  )}
                </td>
                <td className="actions">
                  <button
                    type="button"
                    className="btn small danger"
                    disabled={disabled}
                    onClick={() => onChange(models.filter((_, i) => i !== index))}
                  >
                    {t('removeModel')}
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <div className="row" style={{ marginTop: 10 }}>
        <button type="button" className="btn small secondary" disabled={disabled} onClick={addBlank}>
          + {t('addModel')}
        </button>
        {unadded.length > 0 && (
          <button
            type="button"
            className="btn small secondary"
            disabled={disabled}
            onClick={importAllDiscovered}
          >
            {t('importDiscovered', { n: unadded.length })}
          </button>
        )}
      </div>

      {discovered && discovered.length > 0 && (
        <div className="discovered-pool">
          <p className="hint" style={{ margin: '10px 0 6px' }}>
            {t('discoveredCount', { n: discovered.length })}
          </p>
          <div className="discovered-tags">
            {discovered.map((model) => {
              const added = models.some((entry) => entry.id === model.id)
              return (
                <button
                  key={model.id}
                  type="button"
                  className={`discovered-tag${added ? ' added' : ''}`}
                  disabled={disabled || added}
                  title={added ? t('modelAlreadyAdded') : t('addDiscoveredModel')}
                  onClick={() => importDiscovered(model)}
                >
                  {model.id}
                </button>
              )
            })}
          </div>
        </div>
      )}

      {defaultContextWindow ? (
        <p className="hint" style={{ marginTop: 8, marginBottom: 0 }}>
          {t('contextWindowDefaultHint', { n: formatContextWindow(defaultContextWindow) })}
        </p>
      ) : null}
    </div>
  )
}
