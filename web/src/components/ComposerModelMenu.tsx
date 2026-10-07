import {
  Fragment,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent as ReactKeyboardEvent,
} from 'react'
import { formatContextWindow, resolveSessionReasoningEffort } from '../modelCatalog'
import { t } from '../i18n'
import type {
  CatalogModel,
  ModelCatalog,
  ModelProviderGroup,
  ModelSelection,
} from '../types'
import { IconCheck, IconChevronDown, IconClose, IconSearch, IconThink } from './icons'

/**
 * 最近使用:本机记最近选过的几个模型。「模型极多」的目录里,常用项永远在
 * 首屏 —— 这也是选择器唯一能让"每天只用两个模型"的人免于搜索/滚动的办法。
 */
const RECENT_KEY = 'denia.model.recent.v1'
const RECENT_MAX = 5

interface ModelRef {
  provider: string
  model: string
}

function loadRecents(): ModelRef[] {
  try {
    const raw = window.localStorage.getItem(RECENT_KEY)
    if (!raw) return []
    const parsed: unknown = JSON.parse(raw)
    if (!Array.isArray(parsed)) return []
    return parsed
      .filter(
        (entry): entry is ModelRef =>
          typeof entry === 'object' &&
          entry !== null &&
          typeof (entry as ModelRef).provider === 'string' &&
          typeof (entry as ModelRef).model === 'string',
      )
      .slice(0, RECENT_MAX)
  } catch {
    /* storage unavailable:最近使用只是便利项,丢了不影响选择 */
    return []
  }
}

function rememberRecent(entry: ModelRef): ModelRef[] {
  const next = [
    entry,
    ...loadRecents().filter((r) => r.provider !== entry.provider || r.model !== entry.model),
  ].slice(0, RECENT_MAX)
  try {
    window.localStorage.setItem(RECENT_KEY, JSON.stringify(next))
  } catch {
    /* storage unavailable */
  }
  return next
}

/** 列表行(渲染与键盘索引共用同一份编号,方向键才能跨区块连续移动)。 */
interface Row {
  group: ModelProviderGroup
  model: CatalogModel
  index: number
}

/** 区块:标题可选(单供应商、单搜索块时不写标题,省掉一层噪声)。 */
interface Section {
  key: string
  title: string | null
  rows: Row[]
}

/**
 * Composer 模型选择:单面板 + 搜索优先。
 *
 * # 为什么从「两级级联」改成「单面板」
 *
 * 旧实现是一级供应商列 + 悬浮展开的二级模型列。两个极端都不好:
 *   - 模型极少(1 个供应商 2 个模型):也要先扫一级、再展开二级,两次定位;
 *   - 模型极多:得先知道模型属于哪个供应商,**搜索还只在一级列里**,
 *     键盘要在"一级/二级/搜索"三种模式间切换(←/→ 进出)。
 *
 * 现在只有一屏:常驻搜索框 + 供应商筛选胶囊 + 一个连续列表。
 *   - 极多 → 打字两三个字符即命中(跨全部供应商、名称/ID/描述),Enter 选中;
 *   - 常用 → "最近使用"永远排在最前;
 *   - 极少 → 供应商筛选不渲染、分组标题也不渲染,列表就是那两行;
 *   - 键盘 → ↑↓ 在**渲染顺序**上连续移动(最近使用 → 各供应商),Enter 选中,
 *     Esc 逐层退(清搜索 → 取消筛选 → 关闭),没有模式切换。
 *
 * 思考强度已拆为独立控件(ComposerEffortControl),此处只管模型本身。
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
  const [query, setQuery] = useState('')
  // 供应商筛选:null = 全部。与搜索叠加(在选中的供应商里搜)。
  const [providerId, setProviderId] = useState<string | null>(null)
  // 键盘光标:平铺列表(含"最近使用")里的绝对下标。
  const [cursor, setCursor] = useState(0)
  const [recents, setRecents] = useState<ModelRef[]>([])

  const rootRef = useRef<HTMLDivElement | null>(null)
  const chipRef = useRef<HTMLButtonElement | null>(null)
  const searchRef = useRef<HTMLInputElement | null>(null)
  const filtersRef = useRef<HTMLDivElement | null>(null)

  const groups = catalog.groups
  const group = groups.find((g) => g.id === selection.provider)
  const model = group?.models.find((m) => m.id === selection.model)
  // 网关模型名常带厂商标注后缀(如 "qwen3.8-flash (ali)");
  // 输入框统一显示「模型名 (提供商 id)」,括号内固定是路由 id。
  const chipLabel = `${(model?.name ?? selection.model).replace(/\s*\([^)]*\)\s*$/, '')} (${selection.provider})`

  const q = query.trim().toLowerCase()
  const searching = q.length > 0
  const filtering = providerId !== null
  const visibleGroups = filtering ? groups.filter((g) => g.id === providerId) : groups

  /* ---- 列表派生:区块(渲染)→ 行(键盘) ---- */

  const sections = useMemo<Section[]>(() => {
    const out: Section[] = []
    let index = 0
    const push = (
      key: string,
      title: string | null,
      pairs: Array<[ModelProviderGroup, CatalogModel]>,
    ) => {
      if (pairs.length === 0) return
      out.push({
        key,
        title,
        rows: pairs.map(([pairGroup, pairModel]) => ({
          group: pairGroup,
          model: pairModel,
          index: index++,
        })),
      })
    }

    if (searching) {
      const hits: Array<[ModelProviderGroup, CatalogModel]> = []
      for (const candidateGroup of visibleGroups) {
        const groupMatched = candidateGroup.name.toLowerCase().includes(q)
        for (const candidate of candidateGroup.models) {
          if (
            groupMatched ||
            candidate.name.toLowerCase().includes(q) ||
            candidate.id.toLowerCase().includes(q) ||
            (candidate.description ?? '').toLowerCase().includes(q)
          ) {
            hits.push([candidateGroup, candidate])
          }
        }
      }
      // 搜索命中本就稀疏:平铺成一整块,不再按供应商切分。
      push('results', null, hits)
      return out
    }

    if (!filtering) {
      const pairs: Array<[ModelProviderGroup, CatalogModel]> = []
      for (const recent of recents) {
        const recentGroup = groups.find((g) => g.id === recent.provider)
        const recentModel = recentGroup?.models.find((m) => m.id === recent.model)
        // 目录可能已换(供应商下线/模型改名):找不到就静默丢掉,不显示死项。
        if (recentGroup && recentModel) pairs.push([recentGroup, recentModel])
      }
      push('recent', t('modelRecentLabel'), pairs)
    }

    for (const candidateGroup of visibleGroups) {
      push(
        candidateGroup.id,
        // 只有一个供应商时分组标题是废话(极简目录的极端情况)。
        visibleGroups.length > 1 ? candidateGroup.name : null,
        candidateGroup.models.map((entry) => [candidateGroup, entry] as [ModelProviderGroup, CatalogModel]),
      )
    }
    return out
  }, [filtering, groups, q, recents, searching, visibleGroups])

  const rows = useMemo(() => sections.flatMap((section) => section.rows), [sections])

  // 打开时把光标落在当前模型上:按 ↓/Enter 的直觉是"从这里继续"。
  // 查询与筛选变化后回到列表头(用户刚收窄了范围,视线在第一条)。
  useEffect(() => {
    if (!open) return
    const current = rows.findIndex(
      (row) => row.group.id === selection.provider && row.model.id === selection.model,
    )
    setCursor(current >= 0 ? current : 0)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open])

  useEffect(() => {
    setCursor(0)
  }, [query, providerId])

  useEffect(() => {
    setCursor((previous) => Math.min(previous, Math.max(0, rows.length - 1)))
  }, [rows.length])

  // 光标跟随滚动(关闭时 rows 为空,不必处理)。
  useEffect(() => {
    if (!open) return
    rootRef.current?.querySelector('[data-kb="true"]')?.scrollIntoView({ block: 'nearest' })
  }, [open, cursor])

  const closeMenu = () => {
    setOpen(false)
    setQuery('')
    setProviderId(null)
  }

  useEffect(() => {
    if (!open) return
    const onDoc = (event: MouseEvent) => {
      if (rootRef.current?.contains(event.target as Node)) return
      closeMenu()
    }
    document.addEventListener('mousedown', onDoc)
    return () => document.removeEventListener('mousedown', onDoc)
  }, [open])

  // 打开即聚焦搜索框:选择器的第一动作永远是"打字"。
  useLayoutEffect(() => {
    if (!open) return
    setRecents(loadRecents())
    searchRef.current?.focus()
  }, [open])

  // 供应商筛选栏:纵向滚轮转成横向滚动。
  // 滚动条被刻意藏起(26px 的胶囊行挂不下一条槽),于是滚轮成了唯一的滚动
  // 手段 —— 而纵向滚轮在横向溢出容器上默认什么也不做,会被面板背后的会话
  // 吃掉,表现得像"这栏根本不能滚"。这里直接用原生非被动监听:
  // React 的 onWheel 挂在根节点上是被动的,preventDefault 不生效。
  useEffect(() => {
    const row = filtersRef.current
    if (!row || !open) return
    const onWheel = (event: WheelEvent) => {
      if (row.scrollWidth <= row.clientWidth) return
      const delta = event.deltaY !== 0 ? event.deltaY : event.deltaX
      if (delta === 0) return
      event.preventDefault()
      row.scrollLeft += delta
    }
    row.addEventListener('wheel', onWheel, { passive: false })
    return () => row.removeEventListener('wheel', onWheel)
  }, [open])

  /* ---- 选择 ---- */

  const pickModel = (provider: string, modelId: string) => {
    const nextGroup = groups.find((g) => g.id === provider)
    const nextModel = nextGroup?.models.find((m) => m.id === modelId)
    const nextEfforts = nextModel?.reasoning?.efforts ?? []
    // 换模型时:同模型保留当前档位,跨模型交给档位归一化(默认最高档)。
    const sameModel = selection.provider === provider && selection.model === modelId
    const preferred = sameModel ? selection.reasoningEffort : undefined
    const resolved = resolveSessionReasoningEffort(
      nextEfforts,
      preferred && nextEfforts.some((entry) => entry.id === preferred) ? preferred : undefined,
    )
    setRecents(rememberRecent({ provider, model: modelId }))
    closeMenu()
    chipRef.current?.focus()
    onChange({ provider, model: modelId, reasoningEffort: resolved })
  }

  /* ---- 键盘:Escape 逐层退,↑↓ 全场连续,Enter 选中 ---- */

  const onKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (!open) return
    // 输入法组词阶段的 Enter / Esc 交给 IME,不当作菜单快捷键。
    if (event.nativeEvent.isComposing) return
    if (event.key === 'Escape') {
      event.preventDefault()
      if (searching) {
        setQuery('')
        return
      }
      if (filtering) {
        setProviderId(null)
        return
      }
      closeMenu()
      chipRef.current?.focus()
      return
    }
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault()
      const delta = event.key === 'ArrowDown' ? 1 : -1
      setCursor((previous) => Math.min(rows.length - 1, Math.max(0, previous + delta)))
      return
    }
    if (event.key === 'Enter') {
      event.preventDefault()
      const row = rows[cursor]
      if (row) pickModel(row.group.id, row.model.id)
    }
  }

  const toggle = () => {
    if (open) {
      closeMenu()
      return
    }
    setQuery('')
    setProviderId(null)
    setOpen(true)
  }

  return (
    <div className={`model-menu-anchor${open ? ' open' : ''}`} ref={rootRef} onKeyDown={onKeyDown}>
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
        <span className="model-chip-label">{chipLabel}</span>
        <IconChevronDown size={10} />
      </button>
      {open && (
        <>
          <div className="menu-backdrop" onClick={closeMenu} />
          <div className="model-menu" role="menu" aria-label={t('modelPickerLabel')}>
            <div className="model-menu-search">
              <IconSearch size={13} />
              {/* 不给 type:一旦写成 type="text",就会命中 controls.css 里
                  `input[type='text']` 的全局皮肤(底色 + 1px 边框 + 12px 圆角),
                  搜索行立刻多出一个"框"。这里要的是完全无边框。 */}
              <input
                ref={searchRef}
                value={query}
                onChange={(event) => setQuery(event.target.value)}
                placeholder={t('modelSearchPlaceholder')}
                aria-label={t('modelSearchPlaceholder')}
                spellCheck={false}
              />
              {searching && (
                <button
                  type="button"
                  className="model-menu-clear"
                  title={t('searchSessionsClear')}
                  aria-label={t('searchSessionsClear')}
                  onClick={() => {
                    setQuery('')
                    searchRef.current?.focus()
                  }}
                >
                  <IconClose size={11} />
                </button>
              )}
            </div>
            {/* 供应商筛选:只有多于一个供应商时才值得占一行(极简目录不渲染)。 */}
            {groups.length > 1 && (
              <div className="model-menu-filters" ref={filtersRef}>
                <button
                  type="button"
                  className={`model-filter-chip${filtering ? '' : ' active'}`}
                  onClick={() => setProviderId(null)}
                >
                  {t('modelFilterAll')}
                </button>
                {groups.map((filterGroup) => (
                  <button
                    key={filterGroup.id}
                    type="button"
                    className={`model-filter-chip${providerId === filterGroup.id ? ' active' : ''}`}
                    onClick={() => setProviderId(filterGroup.id)}
                  >
                    {filterGroup.name}
                  </button>
                ))}
              </div>
            )}
            <div className="model-menu-scroll">
              {rows.length === 0 ? (
                <div className="model-menu-empty">{t('modelSearchEmpty')}</div>
              ) : (
                sections.map((section) => (
                  <Fragment key={section.key}>
                    {section.title && <div className="model-menu-heading">{section.title}</div>}
                    {section.rows.map((row) => {
                      const active =
                        selection.provider === row.group.id && selection.model === row.model.id
                      const kb = cursor === row.index
                      // 分组标题被省略时(单供应商、平铺搜索、最近使用)供应商
                      // 信息只剩这里能说,补一行副标题。
                      const showProvider =
                        searching || section.key === 'recent' || visibleGroups.length === 1
                      return (
                        <button
                          key={`${section.key}-${row.group.id}-${row.model.id}`}
                          type="button"
                          role="menuitemradio"
                          aria-checked={active}
                          data-kb={kb || undefined}
                          className={`model-menu-item${active ? ' active' : ''}${kb ? ' kb' : ''}`}
                          onMouseMove={() => setCursor(row.index)}
                          onClick={() => pickModel(row.group.id, row.model.id)}
                        >
                          <span className="row1">
                            <span className="name" title={row.model.name}>
                              {row.model.name}
                            </span>
                            <span className="meta">
                              {row.model.thinkingSupported && (
                                <span className="think" title={t('thinkingLabel')}>
                                  <IconThink size={12} />
                                </span>
                              )}
                              {row.model.contextWindow ? (
                                <span className="ctx" title={t('contextWindowColumn')}>
                                  {formatContextWindow(row.model.contextWindow)}
                                </span>
                              ) : null}
                              {active && (
                                <span className="check" title={t('currentModel')}>
                                  <IconCheck size={13} />
                                </span>
                              )}
                            </span>
                          </span>
                          {showProvider ? (
                            <span className="hint" title={row.group.name}>
                              {row.group.name}
                            </span>
                          ) : row.model.description ? (
                            <span className="hint" title={row.model.description}>
                              {row.model.description}
                            </span>
                          ) : null}
                        </button>
                      )
                    })}
                  </Fragment>
                ))
              )}
            </div>
          </div>
        </>
      )}
    </div>
  )
}
