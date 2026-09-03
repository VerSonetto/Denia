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
import type { ModelCatalog, ModelSelection } from '../types'
import { IconCheck, IconChevron, IconThink } from './icons'

/** 二级菜单宽度 + 间距,与 styles.css 的 .model-menu-sub 保持一致(翻转探测用)。 */
const SUB_MENU_WIDTH = 304
const SUB_MENU_GAP = 6
/** 悬浮展开二级的延迟:滑过供应商时不误触。 */
const HOVER_OPEN_DELAY = 180
/** 鼠标离开后的二级宽限:吸收一二级之间穿过缝隙的短暂离开。 */
const SUB_CLOSE_DELAY = 120

/**
 * Composer 模型选择:两级级联菜单。
 * 一级 = 供应商列表(沿用目录分组顺序),二级 = 该供应商的模型 + 思考强度。
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

  const rootRef = useRef<HTMLDivElement | null>(null)
  const chipRef = useRef<HTMLButtonElement | null>(null)
  const menuRef = useRef<HTMLDivElement | null>(null)
  const subRef = useRef<HTMLDivElement | null>(null)
  // 悬浮展开延迟与二级关闭宽限,两个定时器互斥使用。
  const openTimerRef = useRef<number | null>(null)
  const closeTimerRef = useRef<number | null>(null)

  const group = catalog.groups.find((g) => g.id === selection.provider)
  const model = group?.models.find((m) => m.id === selection.model)
  const efforts = model?.reasoning?.efforts ?? []
  const activeEffort = resolveSessionReasoningEffort(efforts, selection.reasoningEffort)
  const effortName = activeEffort ? reasoningEffortLabel(activeEffort) : ''

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

  // 打开时:探测二级该放右侧还是左侧,并把一级列总高写入 CSS 变量,二级锁同高顶对齐。
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
  }, [open])

  // 键盘高亮的模型条目跟随滚动。
  useEffect(() => {
    if (!open || kbMode !== 'models') return
    subRef.current?.querySelector('[data-kb="true"]')?.scrollIntoView({ block: 'nearest' })
  }, [open, kbMode, kbModel, focusedProvider])

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

  /* ---- 键盘导航:焦点保持在 chip 上,由 chip 统一分发 ---- */

  const onChipKeyDown = (event: ReactKeyboardEvent<HTMLButtonElement>) => {
    if (!open) return
    const groups = catalog.groups
    if (event.key === 'Escape') {
      event.preventDefault()
      // 二级里 Esc 返回一级;一级里 Esc 关闭整个菜单。
      if (kbMode === 'models') {
        setKbMode('providers')
        setFocusedProvider(null)
      } else {
        closeMenu()
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
    <div className={`model-menu-anchor${open ? ' open' : ''}`} ref={rootRef}>
      <button
        ref={chipRef}
        type="button"
        className={`model-chip${open ? ' open' : ''}`}
        disabled={disabled}
        aria-haspopup="menu"
        aria-expanded={open}
        title={t('sessionModelHint')}
        onClick={toggle}
        onKeyDown={onChipKeyDown}
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
          {/* 一级:供应商列表 */}
          <div className="model-menu" role="menu" aria-label={t('modelProvidersLabel')} ref={menuRef}>
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
          </div>
          {/* 二级:该供应商的模型 + 思考强度 */}
          {subGroup && subGroup.models.length > 0 && (
            <div
              className={`model-menu-sub${flip ? ' flip' : ''}`}
              role="menu"
              aria-label={subGroup.name}
              ref={subRef}
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
