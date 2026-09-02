import { useState } from 'react'
import { t } from '../i18n'
import { IconChevron, IconShield } from './icons'

/** 三种权限等级(当前阶段仅前端占位,无后端强制)。 */
export type PermissionLevel = 'read-only' | 'workspace-write' | 'full'

const PERMISSION_KEY = 'denia.permission'

function loadPermission(): PermissionLevel {
  const raw = window.localStorage.getItem(PERMISSION_KEY)
  return raw === 'read-only' || raw === 'workspace-write' || raw === 'full' ? raw : 'full'
}

function permissionName(level: PermissionLevel): string {
  switch (level) {
    case 'read-only':
      return t('permissionReadOnly')
    case 'workspace-write':
      return t('permissionWorkspaceWrite')
    case 'full':
      return t('permissionFull')
  }
}

/**
 * 权限选择器:左下角(模型选择器左侧),三权限单选。
 * 审批发生在运行时 —— AI 调用需要审批的工具时,会在输入框上方弹出
 * 审批弹窗(见 ApprovalDialog),不在这里配置。
 */
export function PermissionSelector({
  value,
  onChange,
  disabled,
}: {
  value: PermissionLevel
  onChange: (level: PermissionLevel) => void
  disabled?: boolean
}) {
  const [open, setOpen] = useState(false)
  const selectPermission = (level: PermissionLevel) => {
    onChange(level)
    window.localStorage.setItem(PERMISSION_KEY, level)
    setOpen(false)
  }

  return (
    <div className="permission-selector">
      <button
        type="button"
        className={`permission-chip${open ? ' open' : ''}`}
        aria-expanded={open}
        disabled={disabled}
        title={t('permissionLabel')}
        onClick={() => setOpen(!open)}
      >
        <IconShield size={13} />
        <span className="permission-name">{permissionName(value)}</span>
        <IconChevron size={10} />
      </button>
      {open && (
        <>
          <div className="menu-backdrop" onClick={() => setOpen(false)} />
          <div className="permission-menu" role="menu">
            <div className="permission-menu-label">{t('permissionLabel')}</div>
            {(
              [
                ['read-only', t('permissionReadOnly'), t('permissionReadOnlyDesc')],
                ['workspace-write', t('permissionWorkspaceWrite'), t('permissionWorkspaceWriteDesc')],
                ['full', t('permissionFull'), t('permissionFullDesc')],
              ] as const
            ).map(([level, label, desc]) => (
              <button
                key={level}
                type="button"
                className={`permission-item${value === level ? ' active' : ''}`}
                onClick={() => selectPermission(level)}
              >
                <span className="permission-item-label">{label}</span>
                <span className="permission-item-desc">{desc}</span>
              </button>
            ))}
          </div>
        </>
      )}
    </div>
  )
}

export { loadPermission }
