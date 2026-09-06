import { useEffect, useState } from 'react'
import { t } from '../i18n'
import { IconCheck, IconChevron, IconEye, IconPencil, IconZap } from './icons'
import type { ComponentType } from 'react'

/** i18n 文案键类型(zh 为源键集)。 */
type MessageKey = Parameters<typeof t>[0]

/** 三种权限等级(与 dsh permission-presets 对齐)。 */
export type PermissionLevel = 'read-only' | 'workspace-write' | 'full'

const PERMISSION_KEY = 'denia.permission'
/** 占位期默认 full 迁移标记:只把旧默认清一次,之后用户显式选 full 保留。 */
const PERMISSION_MIGRATED_KEY = 'denia.permission.migrated'

function loadPermission(): PermissionLevel {
  const raw = window.localStorage.getItem(PERMISSION_KEY)
  if (raw === 'full' && !window.localStorage.getItem(PERMISSION_MIGRATED_KEY)) {
    // 占位期默认 full 且无实际约束;迁移到 dsh 默认 workspace-write。
    try {
      window.localStorage.setItem(PERMISSION_MIGRATED_KEY, '1')
      window.localStorage.setItem(PERMISSION_KEY, 'workspace-write')
    } catch {
      /* storage unavailable */
    }
    return 'workspace-write'
  }
  return raw === 'read-only' || raw === 'workspace-write' || raw === 'full' ? raw : 'workspace-write'
}

/** 三档权限的展示元数据:名称为固定英文(不受 i18n 影响),描述仍走 i18n。 */
const LEVELS: ReadonlyArray<{
  level: PermissionLevel
  icon: ComponentType<{ size?: number }>
  name: string
  descKey: MessageKey
}> = [
  { level: 'read-only', icon: IconEye, name: 'Read-only', descKey: 'permissionReadOnlyDesc' },
  { level: 'workspace-write', icon: IconPencil, name: 'Workspace write', descKey: 'permissionWorkspaceWriteDesc' },
  { level: 'full', icon: IconZap, name: 'Full access', descKey: 'permissionFullDesc' },
]

function levelMeta(level: PermissionLevel) {
  return LEVELS.find((entry) => entry.level === level) ?? LEVELS[1]
}

/**
 * 权限选择器:左下角(模型选择器左侧),三权限单选。
 * 视觉与模型选择器(llm/ModelPicker)对齐:同款透明 chip,
 * 弹层为带图标卡片的菜单,当前项右侧打勾。
 * 审批发生在运行时 —— AI 被策略拒绝后带 sandbox_permissions 重试,
 * 会在输入框上方弹出审批弹窗(见 ApprovalDialog),不在这里配置。
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

  // Esc 关闭弹层(点击外部已由 menu-backdrop 承担)。
  useEffect(() => {
    if (!open) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setOpen(false)
    }
    document.addEventListener('keydown', onKey)
    return () => document.removeEventListener('keydown', onKey)
  }, [open])

  const selectPermission = (level: PermissionLevel) => {
    // 持久化由父级负责:完整权限要先过风险确认,不能在这里提前落盘。
    onChange(level)
    setOpen(false)
  }

  const active = levelMeta(value)
  const ActiveIcon = active.icon

  return (
    <div className={`permission-selector${open ? ' open' : ''}`}>
      <button
        type="button"
        className={`permission-chip${open ? ' open' : ''}`}
        aria-expanded={open}
        aria-haspopup="menu"
        disabled={disabled}
        title={t('permissionLabel')}
        onClick={() => setOpen(!open)}
      >
        <span className="permission-chip-icon">
          <ActiveIcon size={13} />
        </span>
        <span className="permission-chip-name">{active.name}</span>
        <IconChevron size={11} />
      </button>
      {open && (
        <>
          <div className="menu-backdrop" onClick={() => setOpen(false)} />
          <div className="permission-menu" role="menu" aria-label={t('permissionLabel')}>
            <div className="permission-menu-heading">{t('permissionLabel')}</div>
            {LEVELS.map(({ level, icon: Icon, name, descKey }) => {
              const selected = value === level
              return (
                <button
                  key={level}
                  type="button"
                  role="menuitemradio"
                  aria-checked={selected}
                  className={`permission-item${selected ? ' active' : ''}`}
                  onClick={() => selectPermission(level)}
                >
                  <span className="permission-item-icon">
                    <Icon size={14} />
                  </span>
                  <span className="permission-item-text">
                    <span className="permission-item-label">{name}</span>
                    <span className="permission-item-desc">{t(descKey)}</span>
                  </span>
                  {selected && (
                    <span className="permission-item-check">
                      <IconCheck size={13} />
                    </span>
                  )}
                </button>
              )
            })}
          </div>
        </>
      )}
    </div>
  )
}

export { loadPermission }
