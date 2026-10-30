import type { CSSProperties } from 'react'
import { t } from '../../i18n'
import { sessionDisplayTitle } from '../../sessionDisplay'
import { SESSION_DRAG_MIME } from '../../splitView'
import type { DropPlacement } from '../../sidebarOrder'
import type { SessionSummary } from '../../types'
import { IconCaretRight, IconPin, IconTrash } from '../icons'
import { relativeTimeLabel } from './nesting'

export function SessionRow({
  session,
  depth = 0,
  active,
  secondaryActive = false,
  running,
  now,
  childCount = 0,
  collapsed = false,
  childRunning = false,
  onOpen,
  onConfirmDelete,
  onRequestDelete,
  onTogglePin,
  pinned = false,
  confirmingDelete = false,
  onContextMenu,
  onToggleCollapse,
  drag,
}: {
  session: SessionSummary
  /** 血缘嵌套深度:0 = 顶层,>0 = 分支子会话(缩进显示)。 */
  depth?: number
  active: boolean
  /** 分屏副窗格里的会话:弱高亮,与主窗格区分。 */
  secondaryActive?: boolean
  running: boolean
  /** 行尾相对时间的当前时刻(侧栏级 60s 时钟)。 */
  now: number
  /** 后代总数:>0 时渲染折叠钮,折叠时显示 +n 徽标。 */
  childCount?: number
  collapsed?: boolean
  /** 有后代正在运行(折叠时把运行圆点顶到本行)。 */
  childRunning?: boolean
  onOpen: () => void
  /** 行内确认后的真正删除(不再弹对话框)。 */
  onConfirmDelete: () => void
  /** 点击垃圾桶:把图标换成红色“确认删除”按钮。 */
  onRequestDelete: () => void
  onTogglePin: () => void
  pinned?: boolean
  confirmingDelete?: boolean
  onContextMenu: (event: React.MouseEvent<HTMLDivElement>) => void
  onToggleCollapse: () => void
  /** 侧栏拖动排序的落点与回调;未提供时只保留拖到分屏的旧行为。 */
  drag?: {
    dragging: boolean
    placement: DropPlacement | null
    onStart: (event: React.DragEvent<HTMLButtonElement>) => void
    onEnd: () => void
    onOver: (event: React.DragEvent<HTMLDivElement>) => void
    onLeave: (event: React.DragEvent<HTMLDivElement>) => void
    onDrop: (event: React.DragEvent<HTMLDivElement>) => void
  }
}) {
  // 标题前补"(父)"提示,让用户一眼看出这是分支出来的子会话(已嵌套显示,
  // 但同工作区里有多个分支时文字也帮回忆),只对子会话生效,顶会不画。
  const isSubagent = Boolean(session.subagent)
  const isBranch = depth > 0 && !isSubagent
  const isChild = depth > 0
  const showCaret = childCount > 0
  const dotRunning = running || (collapsed && childRunning)
  return (
    <div
      className={`session-row-wrap${active ? ' active' : ''}${!active && secondaryActive ? ' secondary-active' : ''}${isChild ? ' nested' : ''}${drag?.dragging ? ' dragging' : ''}${drag?.placement ? ` drop-${drag.placement}` : ''}`}
      style={isChild ? ({ '--depth': depth } as CSSProperties) : undefined}
      onDragOver={drag?.onOver}
      onDragLeave={drag?.onLeave}
      onDrop={drag?.onDrop}
      onContextMenu={onContextMenu}
    >
      <span className="session-caret-slot">
        {showCaret && (
          <button
            type="button"
            className={`session-caret${collapsed ? '' : ' open'}`}
            title={collapsed ? t('expandChildren', { n: childCount }) : t('collapseChildren')}
            aria-expanded={!collapsed}
            onClick={onToggleCollapse}
          >
            <IconCaretRight size={10} />
          </button>
        )}
      </span>
      <button
        type="button"
        className="session-row"
        onClick={onOpen}
        // 同一个拖拽手势承担两件事:落在侧栏会话行上 = 手动排序(本组件
        // 接管),落在会话区 = 分屏;自定义 MIME 让文件/文本拖拽互不打扰。
        draggable
        onDragStart={(event) => {
          event.dataTransfer.setData(SESSION_DRAG_MIME, session.id)
          event.dataTransfer.effectAllowed = 'copyMove'
          drag?.onStart(event)
        }}
        onDragEnd={drag?.onEnd}
      >
        <span className="lead">
          {session.cwd_alive === false ? (
            <span className="dot err" title={t('deadCwd')} />
          ) : dotRunning ? (
            <span
              className="dot run"
              title={collapsed && !running ? t('childRunningHint') : undefined}
            />
          ) : null}
        </span>
        <span className="excerpt">{sessionDisplayTitle(session)}</span>
        {pinned && (
          <span className="pin-mark" title={t('pinnedLabel')} aria-label={t('pinnedLabel')}>
            <IconPin size={10} />
          </span>
        )}
        {isSubagent && (
          <span className="subagent-tag" title={t('subagentTagHint')} aria-label={t('subagentTagHint')}>
            {t('subagentTag')}
          </span>
        )}
        {isBranch && (
          <span className="branch-tag" title={t('branchTagHint')} aria-label={t('branchTagHint')}>
            {t('branchTag')}
          </span>
        )}
        {showCaret && collapsed && (
          <span className="child-count" title={t('childSessions', { n: childCount })}>
            +{childCount}
          </span>
        )}
      </button>
      {session.created_at > 0 && <span className="session-time">{relativeTimeLabel(session.created_at, now)}</span>}
      <span className="row-actions">
        <button
          type="button"
          className={`row-action-btn${pinned ? ' active' : ''}`}
          title={pinned ? t('unpin') : t('pinToTop')}
          aria-label={pinned ? t('unpin') : t('pinToTop')}
          aria-pressed={pinned}
          onClick={(event) => {
            event.stopPropagation()
            onTogglePin()
          }}
        >
          <IconPin size={12} />
        </button>
        {confirmingDelete ? (
          <button
            type="button"
            className="confirm-delete-btn"
            onClick={(event) => {
              event.stopPropagation()
              onConfirmDelete()
            }}
          >
            {t('confirmDeleteInline')}
          </button>
        ) : (
          <button
            type="button"
            className="row-action-btn danger"
            title={t('deleteSession')}
            disabled={running}
            onClick={(event) => {
              event.stopPropagation()
              onRequestDelete()
            }}
          >
            <IconTrash size={12} />
          </button>
        )}
      </span>
    </div>
  )
}
