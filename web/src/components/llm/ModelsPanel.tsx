/**
 * 模型目录子页:全部可路由模型的平铺视图。
 * - 搜索(150ms 防抖)/过滤(识图/思考/提供方)/多列排序(主键+模型 id 次键)
 * - 固定行高窗口化,行是 memo 叶子组件;点行打开所属提供方的编辑抽屉
 * - 目录加载失败与 per-provider failure 都 fail loud 展示
 */

import { memo, useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { t } from '../../i18n'
import { formatContextWindow } from '../../modelCatalog'
import { reasoningEffortLabel } from '../../reasoningEffort'
import { Button } from './atoms/form'
import { flattenCatalog, filterRows, sortRows, sanitizeErrorText } from './catalogUtils'
import { useWindowedRange } from './useWindowedRange'
import { DropdownField } from '../ui/controls'
import {
  IconChat,
  IconEdit,
  IconImage,
  IconRefresh,
  IconSearch,
  IconThink,
} from '../icons'
import type { CatalogRow, CatalogSortDir, CatalogSortKey } from './types'
import type { ModelCatalog } from '../../types'
import styles from './ModelsPanel.module.css'

const ROW_HEIGHT = 46

/** 目录行叶子:只接原始值 + 稳定回调,父级 setState 不连坐。 */
const ModelRow = memo(function ModelRow({
  providerName,
  modelId,
  modelName,
  contextWindow,
  vision,
  thinking,
  effortLabels,
  onEdit,
}: {
  providerName: string
  modelId: string
  modelName: string
  contextWindow: string
  vision: boolean
  thinking: boolean
  effortLabels: string
  onEdit: (providerId: string) => void
}) {
  return (
    <button type="button" className={styles.row} onClick={() => onEdit(modelId)} title={modelId}>
      <span className={styles.rowProvider} title={providerName}>
        {providerName}
      </span>
      <span className={styles.rowId}>{modelId}</span>
      <span className={styles.rowName}>{modelName}</span>
      <span className={styles.rowCaps}>
        {vision && (
          <span className={styles.capIcon} title={t('visionColumn')}>
            <IconImage size={13} />
          </span>
        )}
        <span className={styles.capIcon} title={t('llm.models.modalityText')}>
          <IconChat size={13} />
        </span>
        {thinking && (
          <span className={styles.capIcon} title={t('thinkingColumn')}>
            <IconThink size={13} />
          </span>
        )}
        {thinking && effortLabels && <span className={styles.rowEfforts}>{effortLabels}</span>}
      </span>
      <span className={styles.rowCtx}>{contextWindow}</span>
      <span className={styles.rowEdit} aria-hidden="true">
        <IconEdit size={13} />
      </span>
    </button>
  )
})

function SortHeader({
  label,
  sortKey,
  activeKey,
  activeDir,
  onSort,
}: {
  label: string
  sortKey: CatalogSortKey
  activeKey: CatalogSortKey
  activeDir: CatalogSortDir
  onSort: (key: CatalogSortKey) => void
}) {
  const active = activeKey === sortKey
  return (
    <button
      type="button"
      className={`${styles.sortBtn}${active ? ` ${styles.sortActive}` : ''}`}
      onClick={() => onSort(sortKey)}
      aria-label={`${label} · ${t('llm.models.sortAria')}`}
    >
      {label}
      <span className={`${styles.sortArrow}${active && activeDir === 'desc' ? ` ${styles.sortDesc}` : ''}`} aria-hidden="true">
        ▲
      </span>
    </button>
  )
}

export function ModelsPanel({
  catalog,
  catalogError,
  onReloadCatalog,
  onEditProvider,
  onAddProvider,
}: {
  catalog: ModelCatalog | null
  catalogError: string | null
  onReloadCatalog: () => void
  onEditProvider: (route: string) => void
  onAddProvider: () => void
}) {
  const [query, setQuery] = useState('')
  const [debouncedQuery, setDebouncedQuery] = useState('')
  const [visionOnly, setVisionOnly] = useState(false)
  const [thinkingOnly, setThinkingOnly] = useState(false)
  const [providerId, setProviderId] = useState<string>('all')
  const [sortKey, setSortKey] = useState<CatalogSortKey>('provider')
  const [sortDir, setSortDir] = useState<CatalogSortDir>('asc')
  const searchRef = useRef<HTMLInputElement | null>(null)

  // 搜索防抖:键击不直接触发过滤重算。
  useEffect(() => {
    const timer = window.setTimeout(() => setDebouncedQuery(query), 150)
    return () => window.clearTimeout(timer)
  }, [query])

  // `/` 聚焦搜索框(目标不是输入框时)。
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== '/' || event.metaKey || event.ctrlKey || event.altKey) return
      const target = event.target as HTMLElement | null
      if (target && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.isContentEditable)) return
      event.preventDefault()
      searchRef.current?.focus()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const rows = useMemo(() => (catalog ? flattenCatalog(catalog) : []), [catalog])

  const filtered = useMemo(
    () =>
      sortRows(
        filterRows(rows, {
          query: debouncedQuery,
          visionOnly,
          thinkingOnly,
          providerId,
        }),
        sortKey,
        sortDir,
      ),
    [rows, debouncedQuery, visionOnly, thinkingOnly, providerId, sortKey, sortDir],
  )

  const { scrollRef, range, onScroll } = useWindowedRange(filtered.length, ROW_HEIGHT)

  const onSort = useCallback((key: CatalogSortKey) => {
    setSortKey((prevKey) => {
      if (prevKey === key) {
        setSortDir((prevDir) => (prevDir === 'asc' ? 'desc' : 'asc'))
        return prevKey
      }
      setSortDir('asc')
      return key
    })
  }, [])

  const providerOptions = useMemo(() => {
    const options = catalog
      ? catalog.groups.map((group) => ({ id: group.id, label: group.name }))
      : []
    return [{ id: 'all', label: t('llm.models.providerFilterAll') }, ...options]
  }, [catalog])

  const editProvider = useCallback((id: string) => onEditProvider(id), [onEditProvider])

  if (catalogError) {
    return (
      <div className={styles.root}>
        <div className={styles.errorBlock} role="alert">
          <p className={styles.errorText}>
            {t('catalogFailure')}: {sanitizeErrorText(catalogError)}
          </p>
          <Button small variant="ghost" onClick={onReloadCatalog}>
            <IconRefresh size={13} />
            {t('retry')}
          </Button>
        </div>
      </div>
    )
  }

  if (!catalog) {
    return (
      <div className={styles.root}>
        <div className={styles.skeletonBlock} aria-busy="true">
          {Array.from({ length: 8 }, (_, index) => (
            <div key={index} className={styles.skeletonRow} style={{ opacity: 1 - index * 0.09 }} />
          ))}
        </div>
      </div>
    )
  }

  if (catalog.groups.length === 0) {
    return (
      <div className={styles.root}>
        <div className={styles.empty}>
          <h4 className={styles.emptyTitle}>{t('llm.picker.noModels')}</h4>
          <p className={styles.emptyHint}>{t('llm.provider.emptyHint')}</p>
          <Button variant="primary" onClick={onAddProvider}>
            {t('addProvider')}
          </Button>
        </div>
      </div>
    )
  }

  return (
    <div className={styles.root}>
      {catalog.failures.length > 0 && (
        <div className={styles.failureBanner} role="alert">
          <span className={styles.failureTitle}>{t('llm.catalog.failureBanner')}</span>
          {catalog.failures.map((failure) => (
            <span key={failure.id} className={styles.failureItem} title={failure.message}>
              {failure.name}: {sanitizeErrorText(failure.message)}
            </span>
          ))}
        </div>
      )}

      <div className={styles.toolbar}>
        <div className={styles.searchBox}>
          <IconSearch size={13} />
          <input
            ref={searchRef}
            value={query}
            onChange={(event) => setQuery(event.target.value)}
            placeholder={t('llm.models.searchPlaceholder')}
            aria-label={t('llm.models.searchPlaceholder')}
            spellCheck={false}
          />
        </div>
        <div className={styles.toolbarControls}>
          <DropdownField
            label=""
            value={providerId}
            options={providerOptions}
            onChange={setProviderId}
          />
          <button
            type="button"
            className={`${styles.filterChip}${visionOnly ? ` ${styles.filterOn}` : ''}`}
            aria-pressed={visionOnly}
            onClick={() => setVisionOnly((v) => !v)}
          >
            <IconImage size={12} />
            {t('llm.models.filterVision')}
          </button>
          <button
            type="button"
            className={`${styles.filterChip}${thinkingOnly ? ` ${styles.filterOn}` : ''}`}
            aria-pressed={thinkingOnly}
            onClick={() => setThinkingOnly((v) => !v)}
          >
            <IconThink size={12} />
            {t('llm.models.filterThinking')}
          </button>
        </div>
      </div>

      <div className={styles.headRow}>
        <SortHeader
          label={t('llm.models.providerColumn')}
          sortKey="provider"
          activeKey={sortKey}
          activeDir={sortDir}
          onSort={onSort}
        />
        <SortHeader
          label={t('modelIdColumn')}
          sortKey="id"
          activeKey={sortKey}
          activeDir={sortDir}
          onSort={onSort}
        />
        <span className={`${styles.headName} ${styles.headCell}`}>{t('modelNameColumn')}</span>
        <span className={`${styles.headCaps} ${styles.headCell}`}>{t('thinkingColumn')}</span>
        <SortHeader
          label={t('contextWindowColumn')}
          sortKey="context"
          activeKey={sortKey}
          activeDir={sortDir}
          onSort={onSort}
        />
        <span className={`${styles.headEdit} ${styles.headCell}`} />
      </div>

      <div className={styles.scroll} ref={scrollRef} onScroll={onScroll}>
        <div style={{ height: filtered.length * ROW_HEIGHT, position: 'relative' }}>
          {filtered
            .slice(range.start, range.end)
            .map((row: CatalogRow, index) => {
              const absolute = range.start + index
              const efforts = (row.model.reasoning?.efforts ?? []).map((entry) => entry.id)
              return (
                <div
                  key={`${row.providerId}-${row.model.id}`}
                  className={styles.rowSlot}
                  style={{ top: absolute * ROW_HEIGHT, height: ROW_HEIGHT }}
                >
                  <ModelRow
                    providerName={row.providerName}
                    modelId={row.model.id}
                    modelName={row.model.name ?? ''}
                    contextWindow={formatContextWindow(row.model.contextWindow)}
                    vision={(row.model.inputModalities ?? []).includes('image')}
                    thinking={row.model.thinkingSupported === true}
                    effortLabels={efforts.map((id) => reasoningEffortLabel(id)).join('·')}
                    onEdit={editProvider}
                  />
                </div>
              )
            })}
          {filtered.length === 0 && (
            <div className={styles.noMatch}>
              <p className={styles.noMatchTitle}>{t('llm.models.empty')}</p>
              <p className={styles.noMatchHint}>{t('llm.models.emptyHint')}</p>
            </div>
          )}
        </div>
      </div>

      <div className={styles.footer}>
        <span className={styles.count}>{t('llm.models.totalCount', { n: filtered.length })}</span>
      </div>
    </div>
  )
}
