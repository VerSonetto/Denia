import { useState } from 'react'
import type { TodoItem } from '../types'
import { t } from '../i18n'

/**
 * The session's todo list, docked above the composer (dsh TodoPanel pattern).
 * Collapsed by default: header shows a per-status progress summary; expanding
 * reveals the full list. Empty list renders nothing.
 */

function CompletedGlyph() {
  return (
    <svg width={14} height={14} viewBox="0 0 14 14" fill="none" aria-hidden="true" className="todo-glyph-completed">
      <circle cx="7" cy="7" r="6.4" stroke="currentColor" strokeWidth="1.2" />
      <path d="M10.5 5.2L6.2 9.5 3.5 6.8" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

function ProgressGlyph() {
  return (
    <svg width={14} height={14} viewBox="0 0 14 14" fill="none" aria-hidden="true" className="todo-glyph-progress">
      <circle cx="7" cy="7" r="6.4" stroke="currentColor" strokeWidth="1.2" strokeDasharray="30 10" strokeLinecap="round" />
    </svg>
  )
}

function PendingGlyph() {
  return (
    <svg width={14} height={14} viewBox="0 0 14 14" fill="none" aria-hidden="true" className="todo-glyph-pending">
      <circle cx="7" cy="7" r="6.4" stroke="currentColor" strokeWidth="1.2" strokeDasharray="2.4 2.4" />
    </svg>
  )
}

function StatusGlyph({ status }: { status: TodoItem['status'] }) {
  switch (status) {
    case 'completed': return <CompletedGlyph />
    case 'in_progress': return <ProgressGlyph />
    case 'pending': return <PendingGlyph />
  }
}

/** Checklist icon for the header lead. */
function ChecklistIcon() {
  return (
    <svg width={14} height={14} viewBox="0 0 14 14" fill="none" aria-hidden="true">
      <path d="M5.2 3.5h7M5.2 7h7M5.2 10.5h7" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
      <path d="M1.4 3l.9.9L4 2.2M1.4 6.5l.9.9 1.7-1.7M1.4 10l.9.9 1.7-1.7" stroke="currentColor" strokeWidth="1.1" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

function ChevronIcon({ up }: { up: boolean }) {
  return (
    <svg width={14} height={14} viewBox="0 0 14 14" fill="none" aria-hidden="true">
      <path d={up ? 'M3.5 8.75L7 5.25l3.5 3.5' : 'M3.5 5.25L7 8.75l3.5-3.5'} stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

/** "·"-joined per-status counts; zero-count segments are omitted. */
function progressLabel(todos: readonly TodoItem[]): string {
  const done = todos.filter((item) => item.status === 'completed').length
  const active = todos.filter((item) => item.status === 'in_progress').length
  const pending = todos.length - done - active
  const parts: string[] = []
  if (done > 0) parts.push(t('todoDone', { done }))
  if (active > 0) parts.push(t('todoActive', { active }))
  if (pending > 0) parts.push(t('todoPending', { pending }))
  return parts.join(' · ')
}

export function TodoPanel({ todos }: { todos: readonly TodoItem[] }) {
  const [collapsed, setCollapsed] = useState(true)
  if (todos.length === 0) return null

  // 折叠时把正在进行的任务晾在头部,不用展开就能看到当前在做什么。
  const active = collapsed ? todos.find((item) => item.status === 'in_progress') : undefined

  return (
    <section className="todo-panel" data-testid="todo-panel" aria-label={t('todoTitle')}>
      <button
        type="button"
        className="todo-header"
        aria-expanded={!collapsed}
        onClick={() => setCollapsed((v) => !v)}
      >
        <span className="todo-lead" aria-hidden><ChecklistIcon /></span>
        <span className="todo-title">{t('todoTitle')}</span>
        {active && (
          <span className="todo-active-now">
            <span className="todo-glyph" aria-hidden><ProgressGlyph /></span>
            <span className="todo-active-text">{active.content}</span>
          </span>
        )}
        <span className="todo-progress">{progressLabel(todos)}</span>
        <span className="todo-chevron" aria-hidden><ChevronIcon up={collapsed} /></span>
      </button>
      {!collapsed && (
        <ul className="todo-list">
          {todos.map((item) => (
            <li key={item.content} className="todo-item" data-status={item.status}>
              <span className="todo-glyph" aria-hidden><StatusGlyph status={item.status} /></span>
              <span className="todo-content">{item.content}</span>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}
