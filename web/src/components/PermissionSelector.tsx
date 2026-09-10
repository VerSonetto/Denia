import { useEffect, useState } from 'react'
import { t } from '../i18n'
import { IconCheck, IconChevron, IconEye, IconPencil, IconPlan, IconZap } from './icons'
import { normalizePermissionMode } from '../types'
import type { PermissionMode } from '../types'
import type { ComponentType } from 'react'

/** i18n 文案键类型(zh 为源键集)。 */
type MessageKey = Parameters<typeof t>[0]

const PERMISSION_KEY = 'denia.permission'
/** 占位期默认 full 迁移标记:只把旧默认清一次,之后用户显式选 full 保留。 */
const PERMISSION_MIGRATED_KEY = 'denia.permission.migrated'

function loadPermission(): PermissionMode {
  const raw = window.localStorage.getItem(PERMISSION_KEY)
  if (raw === 'full' && !window.localStorage.getItem(PERMISSION_MIGRATED_KEY)) {
    // 占位期默认 full 且无实际约束;迁移到默认自动编辑。
    try {
      window.localStorage.setItem(PERMISSION_MIGRATED_KEY, '1')
      window.localStorage.setItem(PERMISSION_KEY, 'auto-edit')
    } catch {
      /* storage unavailable */
    }
    return 'auto-edit'
  }
  // 三档时代的 workspace-write 是 auto-edit 的旧名,直接映射。
  return raw ? normalizePermissionMode(raw) : 'auto-edit'
}

/** 四档权限的展示元数据:名称与描述均走 i18n。 */
export const PERMISSION_LEVELS: ReadonlyArray<{
  level: PermissionMode
  icon: ComponentType<{ size?: number }>
  nameKey: MessageKey
  descKey: MessageKey
}> = [
  { level: 'read-only', icon: IconEye, nameKey: 'permissionReadOnly', descKey: 'permissionReadOnlyDesc' },
  { level: 'auto-edit', icon: IconPencil, nameKey: 'permissionAutoEdit', descKey: 'permissionAutoEditDesc' },
  { level: 'plan', icon: IconPlan, nameKey: 'permissionPlan', descKey: 'permissionPlanDesc' },
  { level: 'full', icon: IconZap, nameKey: 'permissionFull', descKey: 'permissionFullDesc' },
]

function levelMeta(level: PermissionMode) {
  return PERMISSION_LEVELS.find((entry) => entry.level === level) ?? PERMISSION_LEVELS[1]
}

/**
 * 权限选择器:左下角(模型选择器左侧),四档单选。
 * 视觉与模型选择器(ComposerModelMenu)对齐:同款透明 chip,
 * 弹层为带图标卡片的菜单,当前项右侧打勾。
 * 审批发生在运行时 —— 越界写文件或提交计划(计划模式)时会在
 * 输入框位置弹出审批面板,不在这里配置。
 */
export function PermissionSelector({
  value,
  onChange,
  disabled,
}: {
  value: PermissionMode
  onChange: (level: PermissionMode) => void
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

  const selectPermission = (level: PermissionMode) => {
    // 持久化由父级负责:完全访问档要先过风险确认,不能在这里提前落盘。
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
        <span className="permission-chip-name">{t(active.nameKey)}</span>
        <IconChevron size={11} />
      </button>
      {open && (
        <>
          <div className="menu-backdrop" onClick={() => setOpen(false)} />
          <div className="permission-menu" role="menu" aria-label={t('permissionLabel')}>
            <div className="permission-menu-heading">{t('permissionLabel')}</div>
            {PERMISSION_LEVELS.map(({ level, icon: Icon, nameKey, descKey }) => {
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
                    <span className="permission-item-label">{t(nameKey)}</span>
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

/** 是否已由用户显式选过权限档位(决定控制台默认值能否覆盖)。 */
export function hasStoredPermission(): boolean {
  return window.localStorage.getItem(PERMISSION_KEY) !== null
}

/**
 * 落盘权限档位:设置里的默认档位保存后同步写入,让新草稿态的输入框
 * 立刻跟随新默认值(会话内仍以会话日志里的档位为准)。
 */
export function storePermission(level: PermissionMode): void {
  try {
    window.localStorage.setItem(PERMISSION_KEY, level)
    if (level === 'full') window.localStorage.setItem(PERMISSION_MIGRATED_KEY, '1')
  } catch {
    /* storage unavailable */
  }
}

export { loadPermission }
