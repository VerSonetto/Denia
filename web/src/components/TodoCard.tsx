import { memo, useState } from 'react'
import { t } from '../i18n'
import type { TodoChange, TodoSnapshot, TodoSnapshotItem } from '../toolDisplay'

/**
 * todo_write 的清单卡片。
 *
 * todo_write 是**整表替换**:模型每次重发全量清单,照单全收就是反复刷
 * 同样的条目。所以卡片默认只展开"本次状态发生变化的条目",未变的压成
 * 一行计数,点开才看全量 —— 与 edit 的 diff 同一个取舍。
 */

const CHANGE_CLASS: Record<TodoChange, string> = {
  new: 'new',
  started: 'started',
  done: 'done',
  reopened: 'reopened',
}

const CHANGE_LABEL: Record<TodoChange, () => string> = {
  new: () => t('todoChangeNew'),
  started: () => t('todoChangeStarted'),
  done: () => t('todoChangeDone'),
  reopened: () => t('todoChangeReopened'),
}

/** 完成 ✓ / 进行中 ◐ / 待办 ○:与 composer 上方的 TodoPanel 同一套符号语言。 */
function StatusGlyph({ status }: { status: TodoSnapshotItem['status'] }) {
  return (
    <svg width={13} height={13} viewBox="0 0 14 14" fill="none" aria-hidden="true">
      {status === 'completed' ? (
        <>
          <circle cx="7" cy="7" r="6.2" stroke="currentColor" strokeWidth="1.2" />
          <path
            d="M10.4 5.2L6.2 9.4 3.6 6.8"
            stroke="currentColor"
            strokeWidth="1.3"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </>
      ) : status === 'in_progress' ? (
        <circle
          cx="7"
          cy="7"
          r="6.2"
          stroke="currentColor"
          strokeWidth="1.2"
          strokeDasharray="29 11"
          strokeLinecap="round"
        />
      ) : (
        <circle cx="7" cy="7" r="6.2" stroke="currentColor" strokeWidth="1.2" strokeDasharray="2.4 2.4" />
      )}
    </svg>
  )
}

export const TodoCard = memo(function TodoCard({ snapshot }: { snapshot: TodoSnapshot }) {
  const [showAll, setShowAll] = useState(false)
  const { todos, changes, unchanged, done, total } = snapshot
  // 有变化时默认只列变化项;没有变化(或用户要看全量)才列全表。
  const changed = todos.filter((item) => changes.has(item.content))
  const collapsed = changes.size > 0 && !showAll
  const visible = collapsed ? changed : todos
  if (todos.length === 0) return null

  return (
    <div className="todo-card">
      <div className="todo-card-head">
        <span className="todo-card-title">{t('todoTitle')}</span>
        <span className="todo-card-progress">
          {done}/{total}
        </span>
        {done === total && <span className="todo-card-all-done">{t('todoAllDoneShort', { n: total })}</span>}
      </div>
      <ul className="todo-card-list">
        {visible.map((item) => {
          const change = changes.get(item.content)
          return (
            <li
              key={item.content}
              className={`todo-card-item${change ? ` ${CHANGE_CLASS[change]}` : ''}`}
              data-status={item.status}
            >
              <span className="todo-card-glyph" aria-hidden>
                <StatusGlyph status={item.status} />
              </span>
              <span className="todo-card-content">{item.content}</span>
              {change && <span className="todo-card-change">{CHANGE_LABEL[change]()}</span>}
            </li>
          )
        })}
      </ul>
      {collapsed && unchanged > 0 && (
        <button type="button" className="todo-card-more" onClick={() => setShowAll(true)}>
          {t('todoChangedOnly', { n: unchanged })}
          <span className="todo-card-more-action">{t('todoShowAll', { n: total })}</span>
        </button>
      )}
    </div>
  )
})
