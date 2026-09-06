/**
 * LLM 面板共享表单原子。
 * Field → 输入控件通过 context 传递错误 id,输入框自动获得
 * aria-invalid / aria-describedby,校验失败就地可见(fail loud)。
 */

import { createContext, useContext, useId, type ReactNode } from 'react'
import styles from './atoms.module.css'

interface FieldErrorInfo {
  label: string
  hintId?: string
  errorId?: string
  invalid: boolean
}

const FieldContext = createContext<FieldErrorInfo>({ label: '', invalid: false })

export function Field({
  label,
  hint,
  error,
  children,
}: {
  label: string
  hint?: string
  error?: string | null
  children: ReactNode
}) {
  const hintId = useId()
  const errorId = useId()
  const value: FieldErrorInfo = {
    label,
    hintId: hint ? hintId : undefined,
    errorId: error ? errorId : undefined,
    invalid: Boolean(error),
  }
  return (
    <div className={styles.field}>
      <span className={styles.fieldLabel}>{label}</span>
      {hint ? (
        <span id={hintId} className={styles.fieldHint}>
          {hint}
        </span>
      ) : null}
      <FieldContext.Provider value={value}>{children}</FieldContext.Provider>
      {error ? (
        <span id={errorId} className={styles.fieldError} role="alert">
          {error}
        </span>
      ) : null}
    </div>
  )
}

/** 从所在 Field 继承校验态与可访问名称;单独使用时不带 aria 标记。 */
function useFieldAria(): {
  'aria-label': string | undefined
  'aria-invalid': boolean | undefined
  'aria-describedby': string | undefined
} {
  const field = useContext(FieldContext)
  const describedBy = [field.hintId, field.errorId].filter(Boolean).join(' ') || undefined
  return {
    'aria-label': field.label || undefined,
    'aria-invalid': field.invalid || undefined,
    'aria-describedby': describedBy,
  }
}

export function TextInput({
  value,
  onChange,
  disabled,
  placeholder,
  mono,
  type = 'text',
  autoComplete,
  spellCheck = false,
  inputMode,
}: {
  value: string
  onChange: (value: string) => void
  disabled?: boolean
  placeholder?: string
  mono?: boolean
  type?: 'text' | 'password'
  autoComplete?: string
  spellCheck?: boolean
  inputMode?: 'text' | 'numeric'
}) {
  const aria = useFieldAria()
  return (
    <input
      type={type}
      className={`${styles.input}${mono ? ` ${styles.mono}` : ''}`}
      value={value}
      disabled={disabled}
      placeholder={placeholder}
      autoComplete={autoComplete}
      spellCheck={spellCheck}
      inputMode={inputMode}
      onChange={(event) => onChange(event.target.value)}
      {...aria}
    />
  )
}

/** 数字输入(去非数字字符;空串回传 undefined)。 */
export function NumberInput({
  value,
  onChange,
  disabled,
  placeholder,
  mono,
}: {
  value: number | undefined
  onChange: (value: number | undefined) => void
  disabled?: boolean
  placeholder?: string
  mono?: boolean
}) {
  const display = value != null && value > 0 ? String(value) : ''
  return (
    <TextInput
      value={display}
      onChange={(raw) => {
        const digits = raw.trim().replace(/\D/g, '')
        onChange(digits ? Number(digits) : undefined)
      }}
      disabled={disabled}
      placeholder={placeholder}
      mono={mono}
      inputMode="numeric"
    />
  )
}

export function ChipRadio<T extends string>({
  value,
  options,
  onChange,
  disabled,
  ariaLabel,
}: {
  value: T
  options: { id: T; label: string; hint?: string }[]
  onChange: (value: T) => void
  disabled?: boolean
  ariaLabel?: string
}) {
  const aria = useFieldAria()
  const groupLabel = ariaLabel ?? aria['aria-label']
  return (
    <div className={styles.chips} role="radiogroup" {...aria} aria-label={groupLabel}>
      {options.map((option) => (
        <button
          key={option.id}
          type="button"
          role="radio"
          aria-checked={value === option.id}
          title={option.hint}
          disabled={disabled}
          className={`${styles.chip}${value === option.id ? ` ${styles.active}` : ''}`}
          onClick={() => onChange(option.id)}
        >
          {option.label}
        </button>
      ))}
    </div>
  )
}

export function Badge({
  tone = 'neutral',
  children,
}: {
  tone?: 'ok' | 'err' | 'neutral'
  children: ReactNode
}) {
  return (
    <span className={`${styles.badge} ${tone}`}>
      <span className={styles.badgeDot} aria-hidden="true" />
      {children}
    </span>
  )
}

export function Button({
  children,
  variant = 'ghost',
  small,
  disabled,
  onClick,
  type = 'button',
  title,
}: {
  children: ReactNode
  variant?: 'primary' | 'ghost' | 'danger'
  small?: boolean
  disabled?: boolean
  onClick?: () => void
  type?: 'button' | 'submit'
  title?: string
}) {
  return (
    <button
      type={type}
      title={title}
      disabled={disabled}
      onClick={onClick}
      className={`${styles.btn} ${variant}${small ? ` ${styles.small}` : ''}`}
    >
      {children}
    </button>
  )
}

export function Skeleton({ width, height = 12 }: { width: number | string; height?: number }) {
  return <span className={styles.skeleton} style={{ width, height, display: 'block' }} />
}
