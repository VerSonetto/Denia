/**
 * 设置面板共享的版式原件（卡片 / 选择瓦片 / 动作行）。
 *
 * 这些结构原先私有在 `SettingsModal.tsx` 里，只服务「系统提示词 / 外观」两块；
 * 子代理页要长得和它们一模一样，就必须共用同一份 DOM 结构，而不是各写一套
 * 近似样式（否则同一次改版会让两个页面慢慢漂开）。
 *
 * 样式全部来自全局 `src/styles/settings.css` 的 `setm-*` 类，这里只负责
 * 结构与可访问性（`aria-expanded`/`aria-controls`/`role=radiogroup`）。
 */
import type { ReactNode } from 'react'
import { IconChevron } from '../icons'

export function SetmActions({ children }: { children: ReactNode }) {
  return <div className="setm-actions">{children}</div>
}

export function SetmBtn({
  children,
  variant = 'primary',
  small,
  disabled,
  title,
  onClick,
}: {
  children: ReactNode
  variant?: 'primary' | 'ghost' | 'danger'
  /** 列表行内的小号动作（32px 高）。 */
  small?: boolean
  disabled?: boolean
  title?: string
  onClick?: () => void
}) {
  return (
    <button
      type="button"
      title={title}
      className={`setm-btn ${variant}${small ? ' small' : ''}`}
      disabled={disabled}
      onClick={onClick}
    >
      {children}
    </button>
  )
}

/**
 * 面板内的卡片(标题 + 可选说明 + 可折叠正文)。
 *
 * 一个 pane 里可能堆多块内容,默认折叠只留标题行 + 状态摘要,展开才占面积
 * —— 块多了也不会把面板顶成长条。
 */
export function SetmCard({
  title,
  hint,
  summary,
  open,
  onToggle,
  children,
}: {
  title: string
  hint?: string
  /** 折叠时标题行右侧的一句话状态(如"未设置"/"已自定义")。 */
  summary?: string
  open: boolean
  onToggle: () => void
  children: ReactNode
}) {
  const bodyId = `setm-card-${title}`
  return (
    <section className={`setm-card${open ? ' open' : ''}`}>
      <button
        type="button"
        className="setm-card-head"
        aria-expanded={open}
        aria-controls={bodyId}
        onClick={onToggle}
      >
        <span className="setm-card-copy">
          <span className="setm-card-title">{title}</span>
          {hint && !open && <span className="setm-card-hint">{hint}</span>}
        </span>
        {summary && !open && <span className="setm-card-summary">{summary}</span>}
        <span className="setm-card-chevron">
          <IconChevron size={13} />
        </span>
      </button>
      {open && (
        <div className="setm-card-body" id={bodyId}>
          {hint && <p className="setm-card-desc">{hint}</p>}
          {children}
        </div>
      )}
    </section>
  )
}

export function SetmTiles<T extends string>({
  value,
  options,
  onChange,
}: {
  value: T
  options: { id: T; label: string }[]
  onChange: (next: T) => void
}) {
  return (
    <div className="setm-tiles" role="radiogroup">
      {options.map((option) => (
        <button
          key={option.id}
          type="button"
          role="radio"
          aria-checked={value === option.id}
          className={`setm-tile${value === option.id ? ' active' : ''}`}
          onClick={() => onChange(option.id)}
        >
          {option.label}
        </button>
      ))}
    </div>
  )
}
