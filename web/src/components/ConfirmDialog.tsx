import { useEffect, type ReactNode } from 'react'
import { t } from '../i18n'

/**
 * 自定义确认对话框(替代 window.confirm)。
 * 遮罩点击/ESC 等同取消;danger 时确认钮用错误色。
 */
export function ConfirmDialog({
  open,
  title,
  desc,
  children,
  confirmLabel,
  danger,
  onConfirm,
  onCancel,
}: {
  open: boolean
  title: string
  desc: string
  children?: ReactNode
  confirmLabel?: string
  danger?: boolean
  onConfirm: () => void
  onCancel: () => void
}) {
  useEffect(() => {
    if (!open) return
    const onKey = (event: KeyboardEvent) => {
      // 已被内层抽屉等拦截的 Esc 不重复当取消处理。
      if (event.key === 'Escape' && !event.defaultPrevented) {
        event.preventDefault()
        onCancel()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [open, onCancel])

  if (!open) return null
  return (
    <div className="confirm-backdrop" onClick={onCancel}>
      <div
        className="confirm-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onClick={(event) => event.stopPropagation()}
      >
        <h2 className="confirm-title">{title}</h2>
        <p className="confirm-desc">{desc}</p>
        {children}
        <div className="confirm-actions">
          <button type="button" className="btn-secondary" onClick={onCancel}>
            {t('cancel')}
          </button>
          <button
            type="button"
            className={`btn-primary${danger ? ' danger' : ''}`}
            onClick={onConfirm}
          >
            {confirmLabel ?? t('confirmAction')}
          </button>
        </div>
      </div>
    </div>
  )
}
