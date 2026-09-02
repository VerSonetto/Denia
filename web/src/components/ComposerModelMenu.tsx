import { useEffect, useRef, useState } from 'react'
import { resolveSessionReasoningEffort } from '../modelCatalog'
import { reasoningEffortLabel } from '../reasoningEffort'
import { t } from '../i18n'
import type { ModelCatalog, ModelSelection } from '../types'
import { IconChevron } from './icons'

/**
 * Model + reasoning effort in one popover (composer chip).
 */
export function ComposerModelMenu({
  catalog,
  selection,
  disabled,
  onChange,
}: {
  catalog: ModelCatalog
  selection: ModelSelection
  disabled?: boolean
  onChange: (next: ModelSelection) => void
}) {
  const [open, setOpen] = useState(false)
  const rootRef = useRef<HTMLDivElement | null>(null)

  const group = catalog.groups.find((g) => g.id === selection.provider)
  const model = group?.models.find((m) => m.id === selection.model)
  const efforts = model?.reasoning?.efforts ?? []
  const activeEffort = resolveSessionReasoningEffort(efforts, selection.reasoningEffort)
  const effortName = activeEffort ? reasoningEffortLabel(activeEffort) : ''

  useEffect(() => {
    if (!open) return
    const onDoc = (event: MouseEvent) => {
      if (rootRef.current?.contains(event.target as Node)) return
      setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    return () => document.removeEventListener('mousedown', onDoc)
  }, [open])

  const pickModel = (provider: string, modelId: string) => {
    const nextGroup = catalog.groups.find((g) => g.id === provider)
    const nextModel = nextGroup?.models.find((m) => m.id === modelId)
    const nextEfforts = nextModel?.reasoning?.efforts ?? []
    const effort = resolveSessionReasoningEffort(
      nextEfforts,
      selection.reasoningEffort &&
        nextEfforts.some((entry) => entry.id === selection.reasoningEffort)
        ? selection.reasoningEffort
        : undefined,
    )
    setOpen(false)
    onChange({ provider, model: modelId, reasoningEffort: effort })
  }

  return (
    <div className={`model-menu-anchor${open ? ' open' : ''}`} ref={rootRef}>
      <button
        type="button"
        className={`model-chip${open ? ' open' : ''}`}
        disabled={disabled}
        aria-expanded={open}
        title={t('sessionModelHint')}
        onClick={() => setOpen((value) => !value)}
      >
        <span className="model-chip-label">{model?.name ?? selection.model}</span>
        {efforts.length > 0 && (
          <>
            <span className="model-chip-dot" aria-hidden="true" />
            <span className="model-chip-effort">{effortName}</span>
          </>
        )}
        <IconChevron size={11} />
      </button>
      {open && (
        <>
          <div className="menu-backdrop" onClick={() => setOpen(false)} />
          <div className="model-menu" role="dialog" aria-label={t('modelLabel')}>
            <div className="model-menu-scroll">
              {catalog.groups.map((g) => (
                <section key={g.id} className="model-menu-section">
                  <div className="model-menu-heading">{g.name}</div>
                  {g.models.map((m) => {
                    const active =
                      selection.provider === g.id && selection.model === m.id
                    return (
                      <button
                        key={m.id}
                        type="button"
                        className={`model-menu-item${active ? ' active' : ''}`}
                        onClick={() => pickModel(g.id, m.id)}
                      >
                        <span className="name">{m.name}</span>
                        {m.description && (
                          <span className="hint">{m.description}</span>
                        )}
                      </button>
                    )
                  })}
                </section>
              ))}
            </div>
            {efforts.length > 0 && (
              <div className="model-menu-efforts">
                <div className="model-menu-heading">{t('reasoningLabel')}</div>
                <div className="effort-row">
                  {efforts.map((effort) => (
                    <button
                      key={effort.id}
                      type="button"
                      className={`effort-chip${
                        activeEffort === effort.id ? ' active' : ''
                      }`}
                      title={effort.description}
                      onClick={() =>
                        onChange({ ...selection, reasoningEffort: effort.id })
                      }
                    >
                      {reasoningEffortLabel(effort.id)}
                    </button>
                  ))}
                </div>
              </div>
            )}
          </div>
        </>
      )}
    </div>
  )
}
