import type { ReactNode } from 'react'
import { t } from '../i18n'
import { IconClose, IconSearch } from './icons'

/**
 * 右侧面板的统一头部。
 *
 * 三个信息层级：
 * 1. 图标 + 标题 + 状态/副标题（当前在看什么）；
 * 2. 右侧动作（刷新、关闭、视图切换等）；
 * 3. 可选工具条（来源切换、筛选、搜索），由调用方放在 `PanelToolbar`。
 *
 * 只负责结构与语义，不绑定具体面板，也不接管任何交互状态。
 */
export function PanelHead({
  icon,
  title,
  subtitle,
  actions,
}: {
  icon: ReactNode
  title: ReactNode
  subtitle?: ReactNode
  actions?: ReactNode
}) {
  return (
    <header className="pane-panel-head">
      <span className="pane-panel-icon" aria-hidden="true">
        {icon}
      </span>
      <div className="pane-panel-titles">
        <h2 className="pane-panel-title">{title}</h2>
        {subtitle ? <p className="pane-panel-subtitle">{subtitle}</p> : null}
      </div>
      {actions ? <div className="pane-panel-actions">{actions}</div> : null}
    </header>
  )
}

/** 面板头下方的工具条：来源切换、筛选框、搜索等。 */
export function PanelToolbar({
  children,
  className = '',
}: {
  children: ReactNode
  className?: string
}) {
  return <div className={`pane-panel-toolbar${className ? ` ${className}` : ''}`}>{children}</div>
}

/**
 * 面板内的统一搜索框(与左侧会话栏的搜索框同一套视觉:细边框 + 浅底 + 放大镜)。
 *
 * 输入框本身带调用方的类名(如 `.file-tree-filter`),既有测试与内部选择器
 * 因此不必改动;这里只负责结构与共享样式钩子。
 */
export function PanelSearch({
  value,
  onChange,
  placeholder,
  ariaLabel,
  className,
  inputClassName,
}: {
  value: string
  onChange(value: string): void
  placeholder: string
  ariaLabel?: string
  className?: string
  inputClassName?: string
}) {
  return (
    <div className={`pane-search${className ? ` ${className}` : ''}`}>
      <IconSearch size={13} className="pane-search-icon" />
      <input
        type="search"
        className={`pane-search-input${inputClassName ? ` ${inputClassName}` : ''}`}
        value={value}
        placeholder={placeholder}
        aria-label={ariaLabel ?? placeholder}
        onChange={(event) => onChange(event.target.value)}
      />
      {value.length > 0 && (
        <button
          type="button"
          className="pane-search-clear"
          title={t('searchSessionsClear')}
          aria-label={t('searchSessionsClear')}
          onClick={() => onChange('')}
        >
          <IconClose size={11} />
        </button>
      )}
    </div>
  )
}

/** 面板内的统一空态。 */
export function PanelEmpty({
  icon,
  title,
  description,
  action,
}: {
  icon?: ReactNode
  title: ReactNode
  description?: ReactNode
  action?: ReactNode
}) {
  return (
    <div className="pane-empty">
      {icon ? <span className="pane-empty-icon" aria-hidden="true">{icon}</span> : null}
      <p className="pane-empty-title">{title}</p>
      {description ? <p className="pane-empty-desc">{description}</p> : null}
      {action ? <div className="pane-empty-action">{action}</div> : null}
    </div>
  )
}
