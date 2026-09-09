/**
 * 提供方抽屉里的模型行编辑器:ID/名称/上下文窗口/识图/思考/强度档位。
 * 行数少(个位数量级),直接受控渲染;批量目录展示走 ModelsPanel 的窗口化列表。
 */

import { memo } from 'react'
import { t } from '../../i18n'
import {
  REASONING_EFFORT_IDS,
  REASONING_INTENSITY_IDS,
  type CatalogModelEntry,
  type ReasoningEffortId,
  defaultReasoningEfforts,
  formatContextWindow,
  hasVision,
  setVision,
} from '../../modelCatalog'
import { reasoningEffortLabel } from '../../reasoningEffort'
import type { DiscoveredModel } from '../../types'
import { NumberInput, TextInput } from './atoms/form'
import { DiscoverFlow, type DiscoverParams } from './DiscoverFlow'
import styles from './ModelRowsEditor.module.css'

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

const EffortChips = memo(function EffortChips({
  efforts,
  disabled,
  onToggle,
}: {
  efforts: ReasoningEffortId[] | undefined
  disabled?: boolean
  onToggle: (effort: ReasoningEffortId, enabled: boolean) => void
}) {
  return (
    <div className={styles.efforts} role="group" aria-label={t('effortLevelsColumn')}>
      {REASONING_EFFORT_IDS.map((effort) => {
        const active = (efforts ?? defaultReasoningEfforts()).includes(effort)
        return (
          <button
            key={effort}
            type="button"
            className={`${styles.effort}${active ? ` ${styles.effortActive}` : ''}`}
            disabled={disabled}
            aria-pressed={active}
            onClick={() => onToggle(effort, !active)}
          >
            {reasoningEffortLabel(effort)}
          </button>
        )
      })}
    </div>
  )
})

export function ModelRowsEditor({
  models,
  onChange,
  defaultContextWindow,
  discoverParams,
  disabled,
}: {
  models: CatalogModelEntry[]
  onChange: (models: CatalogModelEntry[]) => void
  defaultContextWindow?: number
  /** 提供方探测参数;baseURL 为空时探测按钮自会报错。 */
  discoverParams: () => DiscoverParams
  disabled?: boolean
}) {
  const update = (index: number, patch: Partial<CatalogModelEntry>) => {
    onChange(models.map((entry, i) => (i === index ? { ...entry, ...patch } : entry)))
  }

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

  const importDiscovered = (found: DiscoveredModel[]) => {
    const next = [...models]
    for (const model of found) {
      if (next.some((entry) => entry.id === model.id)) continue
      next.push({
        id: model.id,
        name: model.name && model.name !== model.id ? model.name : undefined,
        inputModalities: ['text'],
        thinkingSupported: false,
      })
    }
    onChange(next)
  }

  return (
    <div className={styles.root}>
      <DiscoverFlow
        readParams={discoverParams}
        isAdded={(id) => models.some((entry) => entry.id === id)}
        onImport={importDiscovered}
        disabled={disabled}
      />

      <div className={styles.rows}>
        {models.length === 0 && (
          <p className={styles.empty}>{t('llm.models.emptyDrawer')}</p>
        )}
        {models.map((entry, index) => (
          <div key={index} className={styles.row}>
            <div className={styles.rowGrid}>
              <TextInput
                mono
                placeholder={t('modelIdPlaceholder')}
                value={entry.id}
                disabled={disabled}
                onChange={(id) => update(index, { id })}
              />
              <TextInput
                placeholder={entry.id || t('modelNamePlaceholder')}
                value={entry.name ?? ''}
                disabled={disabled}
                onChange={(name) => update(index, { name: name || undefined })}
              />
              <NumberInput
                value={entry.contextWindow}
                disabled={disabled}
                mono
                placeholder={
                  defaultContextWindow
                    ? `${t('contextWindowPlaceholder')} ${formatContextWindow(defaultContextWindow)}`
                    : t('contextWindowPlaceholder')
                }
                onChange={(contextWindow) => update(index, { contextWindow })}
              />
              <button
                type="button"
                className={`${styles.cap}${hasVision(entry) ? ` ${styles.capOn}` : ''}`}
                disabled={disabled}
                aria-pressed={hasVision(entry)}
                title={t('visionColumn')}
                onClick={() => update(index, setVision(entry, !hasVision(entry)))}
              >
                {t('visionColumn')}
              </button>
              <button
                type="button"
                className={`${styles.cap}${entry.thinkingSupported ? ` ${styles.capOn}` : ''}`}
                disabled={disabled}
                aria-pressed={entry.thinkingSupported === true}
                title={t('thinkingColumn')}
                onClick={() =>
                  update(index, {
                    thinkingSupported: entry.thinkingSupported !== true,
                    reasoningEfforts:
                      entry.thinkingSupported !== true ? defaultReasoningEfforts() : undefined,
                  })
                }
              >
                {t('thinkingColumn')}
              </button>
              <button
                type="button"
                className={styles.remove}
                disabled={disabled}
                title={t('removeModel')}
                aria-label={`${t('removeModel')} ${entry.id || index + 1}`}
                onClick={() => onChange(models.filter((_, i) => i !== index))}
              >
                ✕
              </button>
            </div>
            {entry.thinkingSupported && (
              <EffortChips
                efforts={entry.reasoningEfforts}
                disabled={disabled}
                onToggle={(effort, enabled) =>
                  update(index, {
                    reasoningEfforts: toggleEffort(entry.reasoningEfforts, effort, enabled),
                  })
                }
              />
            )}
          </div>
        ))}
      </div>

      <div>
        <button
          type="button"
          className={styles.addBtn}
          disabled={disabled}
          onClick={addBlank}
        >
          + {t('llm.models.addManual')}
        </button>
        {defaultContextWindow ? (
          <span className={styles.ctxHint}>
            {t('contextWindowDefaultHint', { n: formatContextWindow(defaultContextWindow) })}
          </span>
        ) : null}
      </div>
    </div>
  )
}
