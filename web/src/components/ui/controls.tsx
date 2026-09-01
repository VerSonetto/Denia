import { useEffect, useRef, useState } from 'react'
import { IconChevron } from '../icons'

export interface SelectOption<T extends string = string> {
  id: T
  label: string
  hint?: string
}

function ToggleTrack({ checked }: { checked: boolean }) {
  return (
    <span className={`ui-toggle-track${checked ? ' on' : ''}`} aria-hidden="true">
      <span className="ui-toggle-thumb" />
    </span>
  )
}

/** 自定义开关,替代原生 checkbox。 */
export function Toggle({
  checked,
  onChange,
  disabled,
  label,
  title,
}: {
  checked: boolean
  onChange: (checked: boolean) => void
  disabled?: boolean
  label?: string
  title?: string
}) {
  return (
    <button
      type="button"
      className="ui-toggle"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      title={title ?? label}
      disabled={disabled}
      onClick={() => onChange(!checked)}
    >
      <ToggleTrack checked={checked} />
      {label ? <span className="ui-toggle-label">{label}</span> : null}
    </button>
  )
}

/** 分段单选,替代原生 select(选项较少时)。 */
export function SegmentControl<T extends string>({
  value,
  options,
  onChange,
  disabled,
}: {
  value: T
  options: SelectOption<T>[]
  onChange: (value: T) => void
  disabled?: boolean
}) {
  return (
    <div className="ui-segment" role="radiogroup">
      {options.map((option) => (
        <button
          key={option.id || '__default__'}
          type="button"
          role="radio"
          aria-checked={value === option.id}
          className={`ui-segment-item${value === option.id ? ' active' : ''}`}
          disabled={disabled}
          title={option.hint}
          onClick={() => onChange(option.id)}
        >
          {option.label}
        </button>
      ))}
    </div>
  )
}

/** 下拉菜单单选,替代原生 select(选项较多时)。 */
export function DropdownField<T extends string>({
  label,
  value,
  options,
  onChange,
  disabled,
  placeholder,
}: {
  label: string
  value: T
  options: SelectOption<T>[]
  onChange: (value: T) => void
  disabled?: boolean
  placeholder?: string
}) {
  const [open, setOpen] = useState(false)
  const rootRef = useRef<HTMLDivElement | null>(null)
  const current = options.find((option) => option.id === value)

  useEffect(() => {
    if (!open) return
    const onDoc = (event: MouseEvent) => {
      if (rootRef.current?.contains(event.target as Node)) return
      setOpen(false)
    }
    document.addEventListener('mousedown', onDoc)
    return () => document.removeEventListener('mousedown', onDoc)
  }, [open])

  return (
    <div className="field">
      <label>{label}</label>
      <div className={`ui-dropdown${open ? ' open' : ''}`} ref={rootRef}>
        <button
          type="button"
          className="ui-dropdown-trigger"
          disabled={disabled || options.length === 0}
          aria-expanded={open}
          aria-haspopup="listbox"
          onClick={() => setOpen((state) => !state)}
        >
          <span className="ui-dropdown-value">
            {current?.label ?? placeholder ?? '—'}
          </span>
          <IconChevron size={11} />
        </button>
        {open && (
          <>
            <div className="menu-backdrop" onClick={() => setOpen(false)} />
            <div className="ui-dropdown-menu" role="listbox" aria-label={label}>
              {options.map((option) => (
                <button
                  key={option.id || '__default__'}
                  type="button"
                  role="option"
                  aria-selected={value === option.id}
                  className={`ui-dropdown-item${value === option.id ? ' active' : ''}`}
                  onClick={() => {
                    onChange(option.id)
                    setOpen(false)
                  }}
                >
                  <span className="name">{option.label}</span>
                  {option.hint ? <span className="hint">{option.hint}</span> : null}
                </button>
              ))}
            </div>
          </>
        )}
      </div>
    </div>
  )
}

/** 自定义文本输入,替代原生 input 默认外观。 */
export function TextField({
  value,
  onChange,
  disabled,
  placeholder,
  mono,
  narrow,
  inputMode,
  autoComplete,
  type = 'text',
}: {
  value: string
  onChange: (value: string) => void
  disabled?: boolean
  placeholder?: string
  mono?: boolean
  narrow?: boolean
  inputMode?: 'text' | 'numeric' | 'decimal'
  autoComplete?: string
  type?: 'text' | 'password'
}) {
  const className = ['ui-text-input', mono ? 'mono' : '', narrow ? 'narrow' : '']
    .filter(Boolean)
    .join(' ')
  return (
    <input
      type={type}
      className={className}
      value={value}
      disabled={disabled}
      placeholder={placeholder}
      inputMode={inputMode}
      autoComplete={autoComplete}
      onChange={(event) => onChange(event.target.value)}
    />
  )
}

/** 数字输入(无原生 number 步进器),用于上下文窗口等字段。 */
export function NumberField({
  value,
  onChange,
  disabled,
  placeholder,
  mono,
  narrow,
}: {
  value: number | undefined
  onChange: (value: number | undefined) => void
  disabled?: boolean
  placeholder?: string
  mono?: boolean
  narrow?: boolean
}) {
  const display = value != null && value > 0 ? String(value) : ''
  return (
    <TextField
      value={display}
      mono={mono}
      narrow={narrow}
      disabled={disabled}
      placeholder={placeholder}
      inputMode="numeric"
      onChange={(raw) => {
        const trimmed = raw.trim()
        if (!trimmed) {
          onChange(undefined)
          return
        }
        const digits = trimmed.replace(/\D/g, '')
        if (!digits) {
          onChange(undefined)
          return
        }
        onChange(Number(digits))
      }}
    />
  )
}

/** 表格内紧凑开关,无文字标签。 */
export function ToggleCell({
  checked,
  onChange,
  disabled,
  title,
}: {
  checked: boolean
  onChange: (checked: boolean) => void
  disabled?: boolean
  title?: string
}) {
  return (
    <button
      type="button"
      className="ui-toggle compact"
      role="switch"
      aria-checked={checked}
      title={title}
      disabled={disabled}
      onClick={() => onChange(!checked)}
    >
      <ToggleTrack checked={checked} />
    </button>
  )
}
