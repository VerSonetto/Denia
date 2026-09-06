/**
 * Composer 模型选择器:单面板分组下拉(按提供方分组)。
 * - 搜索词非空时切平铺命中列表;方向键 + 回车直接选定
 * - 模型行带上下文窗口与模态图标(文本/图像),思考模型内联强度档位
 * - 底部「设为默认」勾选:决定本次切换是否写入 localStorage 供未来会话沿用
 */

import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from 'react'
import { formatContextWindow, resolveSessionReasoningEffort } from '../../modelCatalog'
import { reasoningEffortLabel } from '../../reasoningEffort'
import { t } from '../../i18n'
import type {
  CatalogModel,
  ModelCatalog,
  ModelProviderGroup,
  ModelSelection,
} from '../../types'
import { IconCheck, IconChevron, IconImage, IconSearch, IconThink } from '../icons'
import styles from './ModelPicker.module.css'

/** 分组下拉的平铺条目(键盘导航按此顺序走模型行)。 */
interface FlatEntry {
  group: ModelProviderGroup
  model: CatalogModel
}

export function ModelPicker({
  catalog,
  selection,
  disabled,
  onChange,
}: {
  catalog: ModelCatalog
  selection: ModelSelection
  disabled?: boolean
  onChange: (next: ModelSelection, options?: { setDefault?: boolean }) => void
}) {
  const [open, setOpen] = useState(false)
  const [query, setQuery] = useState('')
  const [kbIndex, setKbIndex] = useState(0)
  const [flip, setFlip] = useState(false)
  // 本次菜单内「设为默认」勾选状态;默认勾上(与历史行为一致)。
  const [setDefault, setSetDefault] = useState(true)

  const rootRef = useRef<HTMLDivElement | null>(null)
  const chipRef = useRef<HTMLButtonElement | null>(null)
  const menuRef = useRef<HTMLDivElement | null>(null)
  const searchRef = useRef<HTMLInputElement | null>(null)

  const group = catalog.groups.find((entry) => entry.id === selection.provider)
  const model = group?.models.find((entry) => entry.id === selection.model)
  const efforts = model?.reasoning?.efforts ?? []
  const activeEffort = resolveSessionReasoningEffort(efforts, selection.reasoningEffort)
  const effortName = activeEffort ? reasoningEffortLabel(activeEffort) : ''

  /* ---- 平铺条目(分组顺序;搜索时按命中过滤) ---- */

  const q = query.trim().toLowerCase()
  const searching = q.length > 0

  const flatEntries = useMemo<FlatEntry[]>(() => {
    const entries: FlatEntry[] = []
    for (const entryGroup of catalog.groups) {
      const groupMatched = entryGroup.name.toLowerCase().includes(q)
      for (const candidate of entryGroup.models) {
        if (
          !searching ||
          groupMatched ||
          candidate.name.toLowerCase().includes(q) ||
          candidate.id.toLowerCase().includes(q) ||
          (candidate.description ?? '').toLowerCase().includes(q)
        ) {
          entries.push({ group: entryGroup, model: candidate })
        }
      }
    }
    return entries
  }, [catalog, q, searching])

  /* 搜索结果按命中聚拢;常态视图跳过无模型的空分组。 */
  const groupedView = useMemo(() => {
    if (searching) {
      const seen = new Set(flatEntries.map((entry) => entry.group.id))
      return catalog.groups.filter((entryGroup) => seen.has(entryGroup.id))
    }
    return catalog.groups.filter((entryGroup) => entryGroup.models.length > 0)
  }, [catalog, flatEntries, searching])

  const closeMenu = () => {
    setOpen(false)
    setQuery('')
    setKbIndex(0)
  }

  useEffect(() => closeMenu, [])

  useEffect(() => {
    if (!open) return
    const onDoc = (event: MouseEvent) => {
      if (rootRef.current?.contains(event.target as Node)) return
      closeMenu()
    }
    document.addEventListener('mousedown', onDoc)
    return () => document.removeEventListener('mousedown', onDoc)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  // 打开时:左缘放不下则右对齐翻转;聚焦搜索框。
  useLayoutEffect(() => {
    if (!open) return
    const menu = menuRef.current
    if (menu) {
      const rect = menu.getBoundingClientRect()
      setFlip(rect.left < 0)
    }
    searchRef.current?.focus()
  }, [open])

  // 键盘高亮/搜索变化跟随滚动与索引收敛。
  useEffect(() => {
    setKbIndex((index) => Math.min(index, Math.max(0, flatEntries.length - 1)))
  }, [flatEntries.length])

  useEffect(() => {
    if (!open) return
    menuRef.current
      ?.querySelector(`[data-kb-idx="${kbIndex}"]`)
      ?.scrollIntoView({ block: 'nearest' })
  }, [open, kbIndex])

  const pick = (providerId: string, modelId: string, effortId?: string) => {
    const nextGroup = catalog.groups.find((entry) => entry.id === providerId)
    const nextModel = nextGroup?.models.find((entry) => entry.id === modelId)
    const nextEfforts = nextModel?.reasoning?.efforts ?? []
    const sameModel = selection.provider === providerId && selection.model === modelId
    const preferred = effortId ?? (sameModel ? selection.reasoningEffort : undefined)
    const resolved = resolveSessionReasoningEffort(
      nextEfforts,
      preferred && nextEfforts.some((entry) => entry.id === preferred) ? preferred : undefined,
    )
    closeMenu()
    chipRef.current?.focus()
    onChange({ provider: providerId, model: modelId, reasoningEffort: resolved }, { setDefault })
  }

  const onMenuKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (!open) return
    if (event.nativeEvent.isComposing) return
    if (event.key === 'Escape') {
      event.preventDefault()
      if (searching) {
        setQuery('')
        return
      }
      closeMenu()
      chipRef.current?.focus()
      return
    }
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault()
      const delta = event.key === 'ArrowDown' ? 1 : -1
      setKbIndex((index) => Math.min(flatEntries.length - 1, Math.max(0, index + delta)))
    } else if (event.key === 'Enter') {
      event.preventDefault()
      const hit = flatEntries[kbIndex]
      if (hit) pick(hit.group.id, hit.model.id)
    }
  }

  const toggle = () => {
    if (open) {
      closeMenu()
      return
    }
    setFlip(false)
    setKbIndex(Math.max(0, flatEntries.findIndex((entry) => entry.group.id === selection.provider && entry.model.id === selection.model)))
    setOpen(true)
  }

  return (
    <div className={`${styles.anchor}${open ? ` ${styles.anchorOpen}` : ''}`} ref={rootRef} onKeyDown={onMenuKeyDown}>
      <button
        ref={chipRef}
        type="button"
        className={styles.chip}
        disabled={disabled}
        aria-haspopup="menu"
        aria-expanded={open}
        title={t('sessionModelHint')}
        onClick={toggle}
      >
        <span className={styles.chipLabel}>{model?.name ?? selection.model}</span>
        {(model?.inputModalities ?? []).includes('image') && (
          <span className={styles.chipIcon} title={t('visionColumn')}>
            <IconImage size={11} />
          </span>
        )}
        {model?.contextWindow ? (
          <span className={styles.chipCtx} title={t('contextWindowColumn')}>
            {formatContextWindow(model.contextWindow)}
          </span>
        ) : null}
        {efforts.length > 0 && (
          <>
            <span className={styles.chipDot} aria-hidden="true" />
            <span className={styles.chipEffort}>{effortName}</span>
          </>
        )}
        <span className={styles.chipChevron} aria-hidden="true">
          <IconChevron size={11} />
        </span>
      </button>

      {open && (
        <>
          <div className="menu-backdrop" onClick={closeMenu} />
          <div
            className={`${styles.menu}${flip ? ` ${styles.menuFlip}` : ''}`}
            role="menu"
            aria-label={t('modelProvidersLabel')}
            ref={menuRef}
          >
            <div className={styles.search}>
              <IconSearch size={13} />
              <input
                ref={searchRef}
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder={t('modelSearchPlaceholder')}
                aria-label={t('modelSearchPlaceholder')}
                spellCheck={false}
              />
            </div>

            <div className={styles.scroll}>
              {flatEntries.length === 0 ? (
                <div className={styles.empty}>{t('modelSearchEmpty')}</div>
              ) : (
                groupedView.map((entryGroup) => {
                  const groupCurrent = selection.provider === entryGroup.id
                  const groupModels = searching
                    ? flatEntries
                        .filter((entry) => entry.group.id === entryGroup.id)
                        .map((entry) => entry.model)
                    : entryGroup.models
                  return (
                    <section key={entryGroup.id} className={styles.group}>
                      <div className={styles.groupHead}>
                        <span className={styles.groupGlyph} aria-hidden="true">
                          {entryGroup.name.charAt(0).toUpperCase()}
                        </span>
                        <span className={styles.groupName}>{entryGroup.name}</span>
                        {groupCurrent && (
                          <span className={styles.groupMark} title={t('currentModel')}>
                            <IconCheck size={12} />
                          </span>
                        )}
                      </div>
                      {groupModels.map((entryModel) => {
                        const active =
                          selection.provider === entryGroup.id && selection.model === entryModel.id
                        const flatIndex = flatEntries.findIndex(
                          (entry) => entry.group.id === entryGroup.id && entry.model.id === entryModel.id,
                        )
                        const modelEfforts = entryModel.reasoning?.efforts ?? []
                        const shownEffort = active
                          ? resolveSessionReasoningEffort(modelEfforts, selection.reasoningEffort)
                          : resolveSessionReasoningEffort(modelEfforts, entryModel.reasoning?.defaultEffort)
                        return (
                          <div
                            key={entryModel.id}
                            className={`${styles.modelBlock}${active ? ` ${styles.modelActive}` : ''}`}
                          >
                            <button
                              type="button"
                              role="menuitem"
                              data-kb-idx={flatIndex}
                              className={`${styles.modelRow}${flatIndex === kbIndex ? ` ${styles.modelKb}` : ''}`}
                              onClick={() => pick(entryGroup.id, entryModel.id)}
                            >
                              <span className={styles.modelName}>{entryModel.name}</span>
                              <span className={styles.modelMeta}>
                                {(entryModel.inputModalities ?? []).includes('image') && (
                                  <span className={styles.modelIcon} title={t('visionColumn')}>
                                    <IconImage size={12} />
                                  </span>
                                )}
                                {entryModel.thinkingSupported && (
                                  <span className={styles.modelIcon} title={t('thinkingLabel')}>
                                    <IconThink size={12} />
                                  </span>
                                )}
                                {entryModel.contextWindow ? (
                                  <span className={styles.modelCtx}>
                                    {formatContextWindow(entryModel.contextWindow)}
                                  </span>
                                ) : null}
                                {active && (
                                  <span className={styles.modelCheck} title={t('currentModel')}>
                                    <IconCheck size={13} />
                                  </span>
                                )}
                              </span>
                            </button>
                            {modelEfforts.length > 0 && (
                              <div className={styles.effortRow} role="group" aria-label={t('reasoningLabel')}>
                                <span className={styles.effortLabel}>{t('reasoningLabel')}</span>
                                <div className={styles.effortSeg}>
                                  {modelEfforts.map((entry) => (
                                    <button
                                      key={entry.id}
                                      type="button"
                                      className={`${styles.effortBtn}${shownEffort === entry.id ? ` ${styles.effortBtnActive}` : ''}`}
                                      title={entry.description}
                                      onClick={() => pick(entryGroup.id, entryModel.id, entry.id)}
                                    >
                                      {reasoningEffortLabel(entry.id)}
                                    </button>
                                  ))}
                                </div>
                              </div>
                            )}
                          </div>
                        )
                      })}
                    </section>
                  )
                })
              )}
            </div>

            <div className={styles.footer}>
              <button
                type="button"
                role="checkbox"
                aria-checked={setDefault}
                className={styles.footerCheck}
                onClick={() => setSetDefault((value) => !value)}
              >
                <span className={`${styles.box}${setDefault ? ` ${styles.boxOn}` : ''}`} aria-hidden="true">
                  {setDefault && <IconCheck size={10} />}
                </span>
                {t('llm.picker.applyDefault')}
              </button>
            </div>
          </div>
        </>
      )}
    </div>
  )
}
