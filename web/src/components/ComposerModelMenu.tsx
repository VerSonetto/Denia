import {
  useEffect,
  useLayoutEffect,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
  type MouseEvent as ReactMouseEvent,
} from 'react'
import { formatContextWindow, resolveSessionReasoningEffort } from '../modelCatalog'
import { reasoningEffortLabel } from '../reasoningEffort'
import { t } from '../i18n'
import type {
  CatalogModel,
  ModelCatalog,
  ModelProviderGroup,
  ModelSelection,
} from '../types'
import { IconCheck, IconChevron, IconSearch, IconThink } from './icons'

/** 二级菜单宽度 + 间距,与 styles.css 的 .model-menu-sub 保持一致(翻转探测用)。 */
const SUB_MENU_WIDTH = 304
const SUB_MENU_GAP = 6
/** 悬浮展开二级的延迟:滑过供应商时不误触。 */
const HOVER_OPEN_DELAY = 180
/** 鼠标离开后的二级宽限:吸收一二级之间穿过缝隙的短暂离开。 */
const SUB_CLOSE_DELAY = 120

/** 搜索命中项:模型连同所属供应商,平铺展示时用。 */
interface SearchHit {
  group: ModelProviderGroup
  model: CatalogModel
}

/**
 * Composer 模型选择:两级级联菜单 + 模型搜索。
 * 一级 = 供应商列表(沿用目录分组顺序),二级 = 该供应商的模型 + 思考强度;
 * 搜索词非空时一级列表切换为命中的模型平铺列表,点击直接选中。
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
  // 二级展开的供应商 id;null 表示只显示一级。
  const [focusedProvider, setFocusedProvider] = useState<string | null>(null)
  // 右侧放不下二级时翻转到左侧。
  const [flip, setFlip] = useState(false)
  // 键盘焦点层:providers 在一级移动,models 在二级移动(→/Enter 进入,←/Esc 返回)。
  const [kbMode, setKbMode] = useState<'providers' | 'models'>('providers')
  const [kbProvider, setKbProvider] = useState(0)
  const [kbModel, setKbModel] = useState(0)
  // 搜索词:非空时一级列表变为命中的模型平铺列表。
  const [query, setQuery] = useState('')

  const rootRef = useRef<HTMLDivElement | null>(null)
  const chipRef = useRef<HTMLButtonElement | null>(null)
  const menuRef = useRef<HTMLDivElement | null>(null)
  const searchRef = useRef<HTMLInputElement | null>(null)
  // 悬浮展开延迟与二级关闭宽限,两个定时器互斥使用。
  const openTimerRef = useRef<number | null>(null)
  const closeTimerRef = useRef<number | null>(null)

  const group = catalog.groups.find((g) => g.id === selection.provider)
  const model = group?.models.find((m) => m.id === selection.model)
  const efforts = model?.reasoning?.efforts ?? []
  const activeEffort = resolveSessionReasoningEffort(efforts, selection.reasoningEffort)
  const effortName = activeEffort ? reasoningEffortLabel(activeEffort) : ''

  /* ---- 搜索派生:按模型名 / ID / 描述 / 供应商名匹配,保持分组顺序平铺 ---- */

  const q = query.trim().toLowerCase()
  const searching = q.length > 0
  const searchHits: SearchHit[] = []
  if (searching) {
    for (const hitGroup of catalog.groups) {
      const groupMatched = hitGroup.name.toLowerCase().includes(q)
      for (const candidate of hitGroup.models) {
        if (
          groupMatched ||
          candidate.name.toLowerCase().includes(q) ||
          candidate.id.toLowerCase().includes(q) ||
          (candidate.description ?? '').toLowerCase().includes(q)
        ) {
          searchHits.push({ group: hitGroup, model: candidate })
        }
      }
    }
  }

  const clearTimers = () => {
    if (openTimerRef.current !== null) {
      window.clearTimeout(openTimerRef.current)
      openTimerRef.current = null
    }
    if (closeTimerRef.current !== null) {
      window.clearTimeout(closeTimerRef.current)
      closeTimerRef.current = null
    }
  }

  const closeMenu = () => {
    clearTimers()
    setOpen(false)
    setFocusedProvider(null)
    setKbMode('providers')
    setQuery('')
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
  }, [open])

  // 搜索词变化时键盘高亮回到命中列表顶部。
  useEffect(() => {
    setKbModel(0)
  }, [query])

  // 打开时:翻转探测 + 一级列尺寸写入 CSS 变量 + 聚焦搜索框(可直接打字)。
  useLayoutEffect(() => {
    if (!open) return
    const menu = menuRef.current
    if (!menu) return
    const rect = menu.getBoundingClientRect()
    // 右侧放得下就放右侧;放不下且左侧更宽才翻到左侧(两侧都窄时保持右侧,避免溢出)。
    const rightSpace = window.innerWidth - rect.right
    const leftSpace = rect.left
    setFlip(rightSpace < SUB_MENU_GAP + SUB_MENU_WIDTH && leftSpace > rightSpace)
    const anchor = menu.parentElement
    // 一级列实测宽/高写入 CSS 变量:二级的 left 依一级右缘(--menu-w)定位,翻转侧用 100%(chip 左缘)。
    anchor?.style.setProperty('--menu-w', `${Math.round(rect.width)}px`)
    anchor?.style.setProperty('--menu-h', `${Math.round(rect.height)}px`)
    searchRef.current?.focus()
  }, [open])

  // 键盘高亮条目跟随滚动(级联模型与搜索平铺共用 data-kb 标记)。
  useEffect(() => {
    if (!open) return
    if (!searching && kbMode !== 'models') return
    rootRef.current?.querySelector('[data-kb="true"]')?.scrollIntoView({ block: 'nearest' })
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, kbMode, kbModel, focusedProvider, query])

  /* ---- 鼠标悬浮:延迟展开 + 宽限关闭,防缝隙闪烁 ---- */

  const hoverProvider = (groupId: string) => {
    if (closeTimerRef.current !== null) {
      window.clearTimeout(closeTimerRef.current)
      closeTimerRef.current = null
    }
    if (focusedProvider === groupId) return
    if (openTimerRef.current !== null) window.clearTimeout(openTimerRef.current)
    openTimerRef.current = window.setTimeout(() => {
      openTimerRef.current = null
      setFocusedProvider(groupId)
    }, HOVER_OPEN_DELAY)
  }

  const scheduleSubClose = () => {
    if (openTimerRef.current !== null) {
      window.clearTimeout(openTimerRef.current)
      openTimerRef.current = null
    }
    if (closeTimerRef.current !== null) window.clearTimeout(closeTimerRef.current)
    closeTimerRef.current = window.setTimeout(() => {
      closeTimerRef.current = null
      setFocusedProvider(null)
    }, SUB_CLOSE_DELAY)
  }

  const cancelSubClose = () => {
    if (closeTimerRef.current !== null) {
      window.clearTimeout(closeTimerRef.current)
      closeTimerRef.current = null
    }
  }

  /* ---- 选择 ---- */

  const pickModel = (providerId: string, modelId: string, effort?: string) => {
    const nextGroup = catalog.groups.find((g) => g.id === providerId)
    const nextModel = nextGroup?.models.find((m) => m.id === modelId)
    const nextEfforts = nextModel?.reasoning?.efforts ?? []
    const sameModel = selection.provider === providerId && selection.model === modelId
    const preferred = effort ?? (sameModel ? selection.reasoningEffort : undefined)
    const resolved = resolveSessionReasoningEffort(
      nextEfforts,
      preferred && nextEfforts.some((entry) => entry.id === preferred) ? preferred : undefined,
    )
    closeMenu()
    chipRef.current?.focus()
    onChange({ provider: providerId, model: modelId, reasoningEffort: resolved })
  }

  /* ---- 键盘导航:焦点可能在 chip 或搜索框,由容器统一分发 ---- */

  const onMenuKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (!open) return
    // 输入法组词阶段的 Enter / Esc 交给 IME,不当作菜单快捷键。
    if (event.nativeEvent.isComposing) return
    const groups = catalog.groups
    if (event.key === 'Escape') {
      event.preventDefault()
      if (searching) {
        setQuery('')
        return
      }
      if (kbMode === 'models') {
        setKbMode('providers')
        setFocusedProvider(null)
        return
      }
      closeMenu()
      return
    }
    if (searching) {
      // 搜索态:↑↓ 在命中模型间移动,Enter 直接选中。
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault()
        const delta = event.key === 'ArrowDown' ? 1 : -1
        setKbModel((prev) => Math.min(searchHits.length - 1, Math.max(0, prev + delta)))
      } else if (event.key === 'Enter') {
        event.preventDefault()
        const hit = searchHits[kbModel]
        if (hit) pickModel(hit.group.id, hit.model.id)
      }
      return
    }
    if (kbMode === 'providers') {
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault()
        const delta = event.key === 'ArrowDown' ? 1 : -1
        const next = Math.min(groups.length - 1, Math.max(0, kbProvider + delta))
        setKbProvider(next)
        // 二级已展开(如鼠标开的)时,高亮移动跟随切换二级内容。
        if (focusedProvider) {
          const target = groups[next]
          clearTimers()
          setFocusedProvider(target && target.models.length > 0 ? target.id : null)
        }
      } else if (event.key === 'ArrowRight' || event.key === 'Enter') {
        event.preventDefault()
        const target = groups[kbProvider]
        if (!target || target.models.length === 0) return
        clearTimers()
        setFocusedProvider(target.id)
        const current = target.models.findIndex(
          (m) => m.id === selection.model && selection.provider === target.id,
        )
        setKbModel(Math.max(0, current))
        setKbMode('models')
      }
    } else {
      const currentGroup = groups[kbProvider]
      const models = currentGroup?.models ?? []
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        event.preventDefault()
        const delta = event.key === 'ArrowDown' ? 1 : -1
        setKbModel(Math.min(models.length - 1, Math.max(0, kbModel + delta)))
      } else if (event.key === 'Enter') {
        event.preventDefault()
        const target = models[kbModel]
        if (currentGroup && target) pickModel(currentGroup.id, target.id)
      } else if (event.key === 'ArrowLeft') {
        event.preventDefault()
        setKbMode('providers')
        setFocusedProvider(null)
      }
    }
  }

  const toggle = () => {
    if (open) {
      closeMenu()
      return
    }
    clearTimers()
    setFlip(false)
    setKbMode('providers')
    const index = catalog.groups.findIndex((g) => g.id === selection.provider)
    setKbProvider(index >= 0 ? index : 0)
    setOpen(true)
  }

  // 点击供应商条目:立即展开二级(不关闭整个菜单)。
  const clickProvider = (event: ReactMouseEvent<HTMLButtonElement>, groupId: string, hasModels: boolean) => {
    event.preventDefault()
    clearTimers()
    if (hasModels) {
      setFocusedProvider(groupId)
      const current = catalog.groups
        .find((g) => g.id === groupId)
        ?.models.findIndex((m) => m.id === selection.model && selection.provider === groupId)
      setKbProvider(Math.max(0, catalog.groups.findIndex((g) => g.id === groupId)))
      setKbModel(Math.max(0, current ?? 0))
    }
  }

  const subGroup = focusedProvider
    ? catalog.groups.find((g) => g.id === focusedProvider)
    : null

  return (
    <div
      className={`model-menu-anchor${open ? ' open' : ''}`}
      ref={rootRef}
      onKeyDown={onMenuKeyDown}
    >
      <button
        ref={chipRef}
        type="button"
        className={`model-chip${open ? ' open' : ''}`}
        disabled={disabled}
        aria-haspopup="menu"
        aria-expanded={open}
        title={t('sessionModelHint')}
        onClick={toggle}
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
          <div className="menu-backdrop" onClick={closeMenu} />
          {/* 一级:搜索框 + (供应商列表 | 搜索命中平铺) */}
          <div className="model-menu" role="menu" aria-label={t('modelProvidersLabel')} ref={menuRef}>
            <div className="model-menu-search">
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
            {searching ? (
              <div className="model-menu-scroll">
                {searchHits.length === 0 ? (
                  <div className="model-menu-empty">{t('modelSearchEmpty')}</div>
                ) : (
                  searchHits.map(({ group: hitGroup, model: hitModel }, index) => {
                    const active =
                      selection.provider === hitGroup.id && selection.model === hitModel.id
                    const kbHere = index === kbModel
                    return (
                      <button
                        key={`${hitGroup.id}-${hitModel.id}`}
                        type="button"
                        role="menuitem"
                        data-kb={kbHere || undefined}
                        className={`model-menu-item${active ? ' active' : ''}${kbHere ? ' kb' : ''}`}
                        onClick={() => pickModel(hitGroup.id, hitModel.id)}
                      >
                        <span className="row1">
                          <span className="name">{hitModel.name}</span>
                          <span className="meta">
                            {hitModel.thinkingSupported && (
                              <span className="think" title={t('thinkingLabel')}>
                                <IconThink size={12} />
                              </span>
                            )}
                            {hitModel.contextWindow ? (
                              <span className="ctx" title={t('contextWindowColumn')}>
                                {formatContextWindow(hitModel.contextWindow)}
                              </span>
                            ) : null}
                            {active && (
                              <span className="check" title={t('currentModel')}>
                                <IconCheck size={13} />
                              </span>
                            )}
                          </span>
                        </span>
                        <span className="hint">{hitGroup.name}</span>
                      </button>
                    )
                  })
                )}
              </div>
            ) : (
              <>
                <div className="model-menu-heading">{t('modelProvidersLabel')}</div>
                <div className="model-menu-scroll" onMouseLeave={scheduleSubClose}>
                  {catalog.groups.map((g, index) => {
                    const hasCurrent = selection.provider === g.id
                    const expanded = focusedProvider === g.id
                    const kbHere = kbMode === 'providers' && index === kbProvider
                    return (
                      <button
                        key={g.id}
                        type="button"
                        role="menuitem"
                        aria-haspopup="menu"
                        aria-expanded={expanded}
                        className={`model-menu-provider${hasCurrent ? ' active' : ''}${
                          expanded ? ' expanded' : ''
                        }${kbHere ? ' kb' : ''}`}
                        onMouseEnter={() => {
                          setKbMode('providers')
                          hoverProvider(g.id)
                        }}
                        onClick={(event) => clickProvider(event, g.id, g.models.length > 0)}
                      >
                        <span className="glyph" aria-hidden="true">
                          {g.name.charAt(0).toUpperCase()}
                        </span>
                        <span className="name">{g.name}</span>
                        {hasCurrent && (
                          <span className="mark" title={t('currentModel')}>
                            <IconCheck size={13} />
                          </span>
                        )}
                        <span className="chev" aria-hidden="true">
                          <IconChevron size={12} />
                        </span>
                      </button>
                    )
                  })}
                </div>
              </>
            )}
          </div>
          {/* 二级:该供应商的模型 + 思考强度(搜索态隐藏) */}
          {!searching && subGroup && subGroup.models.length > 0 && (
            <div
              className={`model-menu-sub${flip ? ' flip' : ''}`}
              role="menu"
              aria-label={subGroup.name}
              onMouseEnter={() => {
                cancelSubClose()
                setKbMode('providers')
              }}
              onMouseLeave={scheduleSubClose}
            >
              <div className="model-menu-heading">{subGroup.name}</div>
              <div className="model-menu-scroll">
                {subGroup.models.map((m, index) => {
                  const active =
                    selection.provider === subGroup.id && selection.model === m.id
                  const kbHere = kbMode === 'models' && index === kbModel
                  const modelEfforts = m.reasoning?.efforts ?? []
                  // 条目内分段控件的高亮:当前模型显示会话实际档位,其余模型显示其默认档。
                  const shownEffort = active
                    ? resolveSessionReasoningEffort(modelEfforts, selection.reasoningEffort)
                    : resolveSessionReasoningEffort(modelEfforts, m.reasoning?.defaultEffort)
                  return (
                    <div
                      key={m.id}
                      className={`model-menu-model${active ? ' active' : ''}`}
                    >
                      <button
                        type="button"
                        role="menuitem"
                        data-kb={kbHere || undefined}
                        className={`model-menu-item${active ? ' active' : ''}${kbHere ? ' kb' : ''}`}
                        onClick={() => pickModel(subGroup.id, m.id)}
                      >
                        <span className="row1">
                          <span className="name">{m.name}</span>
                          <span className="meta">
                            {m.thinkingSupported && (
                              <span className="think" title={t('thinkingLabel')}>
                                <IconThink size={12} />
                              </span>
                            )}
                            {m.contextWindow ? (
                              <span className="ctx" title={t('contextWindowColumn')}>
                                {formatContextWindow(m.contextWindow)}
                              </span>
                            ) : null}
                            {active && (
                              <span className="check" title={t('currentModel')}>
                                <IconCheck size={13} />
                              </span>
                            )}
                          </span>
                        </span>
                        {m.description && <span className="hint">{m.description}</span>}
                      </button>
                      {modelEfforts.length > 0 && (
                        <div className="mm-effort-row" role="group" aria-label={t('reasoningLabel')}>
                          <span className="mm-effort-label">{t('reasoningLabel')}</span>
                          <div className="mm-effort-seg">
                            {modelEfforts.map((effort) => (
                              <button
                                key={effort.id}
                                type="button"
                                className={`mm-effort-btn${
                                  shownEffort === effort.id ? ' active' : ''
                                }`}
                                title={effort.description}
                                onClick={() => pickModel(subGroup.id, m.id, effort.id)}
                              >
                                {reasoningEffortLabel(effort.id)}
                              </button>
                            ))}
                          </div>
                        </div>
                      )}
                    </div>
                  )
                })}
              </div>
            </div>
          )}
        </>
      )}
    </div>
  )
}
