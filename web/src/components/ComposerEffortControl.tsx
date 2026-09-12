import {
  useEffect,
  useRef,
  useState,
  type CSSProperties,
  type KeyboardEvent as ReactKeyboardEvent,
} from 'react'
import { t } from '../i18n'
import { REASONING_EFFORT_OFF } from '../modelCatalog'
import { reasoningEffortLabel } from '../reasoningEffort'
import type { ReasoningEffortInfo } from '../types'
import { IconThink } from './icons'
/** 滑块连续位置 → 最近档位:拖动时的吸附预览,松手后落到该档位。 */
function snap(position: number, maxIndex: number): number {
  return Math.min(maxIndex, Math.max(0, Math.round(position)))
}

/** 把手半径(px):与 CSS 的 --knob 保持一致,用于把把手夹在轨道内部。 */
const KNOB_RADIUS = 13

/**
 * 思考强度控件:模型选择器右侧的独立 chip,点击展开滑块浮层。
 *
 * 滑块视觉(对照 dsh 社区 reasoning-effort 插件):一条胶囊轨道,
 * 轨道内嵌一个白色圆形把手;原生 input 透明覆盖整条轨道承载全部交互。
 *
 * 交互是「自由滑动 + 松手吸附」:拖动期间位置连续(文案预览最近档位),
 * 松手才吸附到整数档位并回调上层。
 * 性能约定(叶子组件):拖动只改本组件内部状态,一次调节结束才提交,
 * 不把 composer 及以上层拖进每帧重渲染。
 */
export function ComposerEffortControl({
  efforts,
  value,
  disabled,
  onChange,
}: {
  /** 当前模型提供的档位(至少一档;单档时控件退化为纯展示)。 */
  efforts: ReasoningEffortInfo[]
  /** 会话实际档位(已按模型可用档位归一化)。 */
  value: string
  disabled?: boolean
  onChange: (effort: string) => void
}) {
  const [open, setOpen] = useState(false)
  // 滑块位置:拖动中连续(小数),松手吸附成整数档位。
  const [position, setPosition] = useState(0)
  // 拖动中:给把手加放大/高亮反馈。
  const [dragging, setDragging] = useState(false)

  const rootRef = useRef<HTMLDivElement | null>(null)
  const chipRef = useRef<HTMLButtonElement | null>(null)
  const rangeRef = useRef<HTMLInputElement | null>(null)
  // 原生 change 监听只随开关挂载,读值走 ref,避免父组件每次渲染重挂监听。
  const latestRef = useRef({ efforts, value, onChange })
  latestRef.current = { efforts, value, onChange }

  const maxIndex = Math.max(0, efforts.length - 1)
  const index = Math.max(0, efforts.findIndex((effort) => effort.id === value))
  const shownIndex = snap(position, maxIndex)
  const label = reasoningEffortLabel(efforts[shownIndex]?.id ?? '')
  const ratio = maxIndex > 0 ? Math.min(1, Math.max(0, position / maxIndex)) : 1

  // 外部档位变化(切模型 / 会话回放)时同步滑块位置。
  useEffect(() => {
    setPosition(index)
  }, [index])

  // 禁用态(无工作区 / 优化中)收起浮层。
  useEffect(() => {
    if (disabled) setOpen(false)
  }, [disabled])

  // 展开后焦点直接落在滑块上,方向键即可调档。
  useEffect(() => {
    if (!open) return
    rangeRef.current?.focus({ preventScroll: true })
  }, [open])

  // 松手才提交:React 的 onChange 对应原生 input(拖动中持续触发),
  // 原生 change 才表示"一次调节结束" —— 这时把连续位置吸附到档位。
  useEffect(() => {
    if (!open) return
    const element = rangeRef.current
    if (!element) return
    const onNativeChange = () => {
      setDragging(false)
      const latest = latestRef.current
      const at = snap(Number(element.value), Math.max(0, latest.efforts.length - 1))
      setPosition(at)
      const target = latest.efforts[at]
      if (target && target.id !== latest.value) latest.onChange(target.id)
    }
    element.addEventListener('change', onNativeChange)
    return () => element.removeEventListener('change', onNativeChange)
  }, [open])

  /** 按档位提交(键盘 / 点刻度):一步一档,不做自由滑动。 */
  const commit = (next: number) => {
    const at = snap(next, maxIndex)
    setPosition(at)
    const target = efforts[at]
    if (target && target.id !== value) onChange(target.id)
    rangeRef.current?.focus({ preventScroll: true })
  }

  const close = () => {
    setOpen(false)
    setDragging(false)
    // 拖动未落地的位置随关闭收回,保证 chip 文案等于真实档位。
    setPosition(index)
    chipRef.current?.focus({ preventScroll: true })
  }

  const toggle = () => {
    if (open) {
      close()
      return
    }
    setPosition(index)
    setOpen(true)
  }

  const onKeyDown = (event: ReactKeyboardEvent<HTMLDivElement>) => {
    if (!open || event.key !== 'Escape') return
    event.preventDefault()
    // 输入区有自己的 Esc 语义(清空引用 / 关闭候选),不要串上去。
    event.stopPropagation()
    close()
  }

  // 滑块内方向键按档位走:滑块 step 是连续值(自由滑动),键盘自己吸附。
  const onRangeKeyDown = (event: ReactKeyboardEvent<HTMLInputElement>) => {
    let next: number | null = null
    if (event.key === 'ArrowLeft' || event.key === 'ArrowDown') next = shownIndex - 1
    else if (event.key === 'ArrowRight' || event.key === 'ArrowUp') next = shownIndex + 1
    else if (event.key === 'Home' || event.key === 'PageDown') next = 0
    else if (event.key === 'End' || event.key === 'PageUp') next = maxIndex
    if (next === null) return
    event.preventDefault()
    commit(next)
  }

  const off = efforts[shownIndex]?.id === REASONING_EFFORT_OFF
  const description = efforts[shownIndex]?.description?.trim()
  // 档位分级:0=最弱(关/低),1=中间档,2=最高档。
  // 只驱动静态视觉深浅(填充段颜色),不含任何动画/发光。
  const tier: 0 | 1 | 2 = shownIndex >= maxIndex ? 2 : shownIndex === 0 ? 0 : 1

  // 单档:没有可调空间 —— 控件保留占位(位置稳定),但不给展开行为。
  if (maxIndex === 0) {
    return (
      <div className="effort-control" ref={rootRef}>
        <span
          className={`effort-chip-static${off ? ' off' : ''}`}
          title={off ? t('effortControlOffHint') : undefined}
        >
          <span className="effort-chip-icon" aria-hidden="true">
            <IconThink size={12} />
          </span>
          {!off && <span className="effort-chip-label">{label}</span>}
        </span>
      </div>
    )
  }

  // 把手位置:夹在轨道内(留出半径),与 CSS 的 clamp 同口径。
  const knobLeft = `clamp(${KNOB_RADIUS}px, ${ratio * 100}%, calc(100% - ${KNOB_RADIUS}px))`

  return (
    <div className={`effort-control${open ? ' open' : ''}`} ref={rootRef} onKeyDown={onKeyDown}>
      <button
        ref={chipRef}
        type="button"
        className={`effort-chip${open ? ' open' : ''}${off ? ' off' : ''}`}
        disabled={disabled}
        aria-haspopup="dialog"
        aria-expanded={open}
        title={off ? t('effortControlOffHint') : t('effortControlHint')}
        onClick={toggle}
      >
        <span className="effort-chip-icon" aria-hidden="true">
          <IconThink size={12} />
        </span>
        {!off && <span className="effort-chip-label">{label}</span>}
      </button>
      {open && (
        <>
          <div className="menu-backdrop" onClick={close} />
          <div className="effort-popover" role="dialog" aria-label={t('effortControlHint')}>
            <div className="effort-popover-head">
              <span className="effort-popover-title">{t('reasoningLabel')}</span>
              <span className="effort-popover-value">{label}</span>
            </div>
            {/* 胶囊轨道 + 轨道内圆钮;原生 input 透明覆盖,承载拖动/键盘。
                data-tier 只驱动填充段的静态深浅分级,无动画。 */}
            <div
              className={`effort-slider${dragging ? ' dragging' : ''}`}
              data-tier={tier}
              style={{ '--progress': `${ratio * 100}%` } as CSSProperties}
            >
              <div className="effort-track" aria-hidden="true" />
              <input
                ref={rangeRef}
                type="range"
                className="effort-input"
                min={0}
                max={maxIndex}
                step={0.01}
                value={position}
                aria-label={t('effortSliderAria')}
                aria-valuetext={label}
                onChange={(event) => setPosition(Number(event.target.value))}
                onPointerDown={() => setDragging(true)}
                onPointerUp={() => setDragging(false)}
                onPointerCancel={() => setDragging(false)}
                onKeyDown={onRangeKeyDown}
              />
              <span className="effort-knob" style={{ left: knobLeft }} aria-hidden="true" />
            </div>
            {/* 档位刻度:按整条轨道等分对齐,可直接点选 */}
            <div className="effort-ticks">
              {efforts.map((effort, at) => (
                <button
                  key={effort.id}
                  type="button"
                  className={`effort-tick${at === shownIndex ? ' active' : ''}`}
                  style={{ left: `${(at / maxIndex) * 100}%` }}
                  title={effort.description}
                  onClick={() => commit(at)}
                >
                  {reasoningEffortLabel(effort.id)}
                </button>
              ))}
            </div>
            {description && <div className="effort-popover-desc">{description}</div>}
          </div>
        </>
      )}
    </div>
  )
}
