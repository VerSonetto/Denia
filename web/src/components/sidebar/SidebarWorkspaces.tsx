import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { bumpCatalogTick } from '../../appStore'
import { t } from '../../i18n'
import {
  readCollapsedChildren,
  readInProgressMarks,
  readInProgressWorkspaces,
  readSidebarFilter,
  readSidebarOrder,
  readSidebarPins,
  restoredSidebarSession,
  writeCollapsedChildren,
  writeInProgressMarks,
  writeInProgressWorkspaces,
  writeSidebarFilter,
  writeSidebarOrder,
  writeSidebarPins,
  type SidebarFilter,
} from '../../sidebarMemory'
import { applyManualOrder, applyPins, moveId, toggleId } from '../../sidebarOrder'
import { sessionAlias, setSessionAlias } from '../../sessionAliases'
import { sessionDisplayTitle } from '../../sessionDisplay'
import type { SessionSummary, WorkspaceRecord } from '../../types'
import { IconCaretRight, IconClose, IconNewSession, IconPencil, IconPin, IconProgressFlag, IconSessionSearch, IconTrash, IconWorkspace } from '../icons'
import { SidebarContextMenu, type SidebarMenuState } from '../SidebarContextMenu'
import { ConfirmDialog } from '../ConfirmDialog'
import type { SidebarDrag, SidebarDropTarget } from './nesting'
import {
  COLLAPSED_LIMIT,
  lineageStats,
  nestByParent,
  placementFromPointer,
  UNGROUPED_KEY,
  WORKSPACE_DRAG_MIME,
} from './nesting'
import { SessionRow } from './SessionRow'

export function SidebarWorkspaces({
  sessions,
  workspaces,
  activeId,
  secondaryId,
  runningIds,
  sessionsLoaded,
  searchOpen,
  onSearchOpenChange,
  expandAllTick,
  onAllExpandedChange,
  onOpenSession,
  onNewSession,
  onDeleteWorkspace,
  onDeleteSession,
  onDeleteUngrouped,
}: {
  sessions: SessionSummary[]
  workspaces: WorkspaceRecord[]
  activeId: string | null
  /** 分屏副窗格的会话 id(弱高亮;null = 未分屏)。 */
  secondaryId?: string | null
  runningIds: Record<string, boolean>
  /** 会话列表是否已从服务端加载完成:未完成前不能清理标记(空列表会误删)。 */
  sessionsLoaded: boolean
  searchOpen: boolean
  onSearchOpenChange: (open: boolean) => void
  expandAllTick: number
  onAllExpandedChange: (allExpanded: boolean) => void
  onOpenSession: (id: string, wsId?: string) => void
  onNewSession: (wsId?: string) => void
  onDeleteWorkspace: (ws: WorkspaceRecord) => void
  onDeleteSession: (session: SessionSummary) => void
  onDeleteUngrouped: (sessions: SessionSummary[]) => void
}) {
  const [expanded, setExpanded] = useState<Record<string, boolean>>({})
  const [showAll, setShowAll] = useState<Record<string, boolean>>({})
  // 血缘折叠:sessionId → 是否收起其后代子树(默认展开)。
  const [collapsedChildren, setCollapsedChildren] = useState(readCollapsedChildren)
  // 手动排序与筛选都是本机侧栏偏好,与折叠状态同走 localStorage。
  const [order, setOrder] = useState(readSidebarOrder)
  const [filter, setFilter] = useState<SidebarFilter>(readSidebarFilter)
  const [pins, setPins] = useState(readSidebarPins)
  // 手动「进行中」标记:与 running 分开持久化,只有移除标记才消失。
  const [inProgress, setInProgress] = useState<ReadonlySet<string>>(
    () => new Set(readInProgressMarks()),
  )
  const [inProgressWorkspaces, setInProgressWorkspaces] = useState<ReadonlySet<string>>(
    () => new Set(readInProgressWorkspaces()),
  )
  // 工作区行右键当前运行会话的“折叠其他会话”:workspaceId → 保留显示的 sessionId。
  const [collapsedWorkspaceSessions, setCollapsedWorkspaceSessions] = useState<Record<string, string>>({})
  const [confirming, setConfirming] = useState<{ kind: 'session' | 'workspace'; id: string } | null>(null)
  const [contextMenu, setContextMenu] = useState<SidebarMenuState | null>(null)
  const [renameTarget, setRenameTarget] = useState<SessionSummary | null>(null)
  const [renameDraft, setRenameDraft] = useState('')
  const [dragState, setDragState] = useState<SidebarDrag | null>(null)
  const dragRef = useRef<SidebarDrag | null>(null)
  const [dropTarget, setDropTarget] = useState<SidebarDropTarget | null>(null)
  const pinnedWorkspaces = useMemo(() => new Set(pins.workspaces), [pins.workspaces])
  const pinnedSessions = useMemo(() => new Set(pins.sessions), [pins.sessions])
  const revealedSessionRef = useRef(restoredSidebarSession(activeId))
  useEffect(() => { writeCollapsedChildren(collapsedChildren) }, [collapsedChildren])

  /**
   * 标记/取消标记「进行中」。
   *
   * 幂等:Set 语义下重复标记不会产生重复项;写回 localStorage 时也去重。
   * 取消标记只影响本账本,不触碰服务端 running 状态。
   */
  const toggleInProgress = useCallback((id: string) => {
    setInProgress((previous) => {
      const next = new Set(previous)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      writeInProgressMarks([...next])
      return next
    })
  }, [])

  const toggleWorkspaceInProgress = useCallback((id: string) => {
    setInProgressWorkspaces((previous) => {
      const next = new Set(previous)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      writeInProgressWorkspaces([...next])
      return next
    })
  }, [])

  const createSessionInWorkspace = useCallback((workspaceId: string) => {
    setCollapsedWorkspaceSessions((previous) => {
      if (!(workspaceId in previous)) return previous
      const next = { ...previous }
      delete next[workspaceId]
      return next
    })
    onNewSession(workspaceId)
  }, [onNewSession])

  /**
   * 会话删除/消失后清理标记。
   *
   * 必须等列表真正加载完成:首屏 sessions 为空,若此时按"不在列表里"清理,
   * 会把重启后刚恢复的标记一次性抹掉。筛选本身也按列表过滤,所以多留几个
   * 孤儿 id 不会显示出来,但清理能让账本不随长期使用无限增长。
   *
   * 空列表同样不能当作"会话都被删了":首次拉取失败时 appStore 也会把
   * sessionsLoaded 置真(见 refreshList 的 catch),此时一次网络故障就会
   * 清空账本。等下一次拿到非空列表再清理;期间孤儿标记不会显示。
   * 一旦本次挂载见过非空列表,后续的空列表就是权威结果(例如删掉了最后
   * 一个会话),仍按"不在列表里"清理。
   */
  const sawSessionsRef = useRef(false)
  useEffect(() => {
    if (!sessionsLoaded) return
    if (sessions.length > 0) sawSessionsRef.current = true
    else if (!sawSessionsRef.current) return
    setInProgress((previous) => {
      if (previous.size === 0) return previous
      const alive = new Set(sessions.map((session) => session.id))
      const next = new Set([...previous].filter((id) => alive.has(id)))
      if (next.size === previous.size) return previous
      writeInProgressMarks([...next])
      return next
    })
  }, [sessions, sessionsLoaded])

  // 行尾相对时间的时钟;60s 一跳足够分钟级分桶。
  const [now, setNow] = useState(() => Date.now())
  const [query, setQuery] = useState('')
  const searchInputRef = useRef<HTMLInputElement>(null)
  const lastExpandTickRef = useRef(0)

  // 10k 会话时避免每次 get/sessionIds 都做 O(n) find;一次建 Map,O(1) 取行。
  const sessionsById = useMemo(
    () => new Map(sessions.map((session) => [session.id, session])),
    [sessions],
  )

  // 工作区路径集合:ungrouped 判定与"是否已有同路径工作区"都改走 Set,
  // 把每次渲染的 O(会话数 × 工作区数) 比较降为 O(会话数)。
  const workspacePaths = useMemo(
    () => new Set(workspaces.map((workspace) => workspace.path)),
    [workspaces],
  )

  // 工作区顺序叠加本机手动排序与置顶;新工作区(服务端 prepend)排在最前。
  const orderedWorkspaces = useMemo(
    () => applyPins(applyManualOrder(workspaces, order.workspaces), pins.workspaces),
    [workspaces, order.workspaces, pins.workspaces],
  )

  const membersOf = useCallback(
    (ws: WorkspaceRecord) =>
      applyPins(
        applyManualOrder(
          ws.sessionIds
            .map((id) => sessionsById.get(id))
            .filter((s): s is SessionSummary => !!s && s.cwd === ws.path),
          order.sessions[ws.id],
        ),
        pins.sessions,
      ),
    [sessionsById, order.sessions, pins.sessions],
  )

  const ungrouped = useMemo(
    () =>
      applyPins(
        applyManualOrder(
          sessions.filter((s) => s.cwd === undefined || !workspacePaths.has(s.cwd)),
          order.ungrouped,
        ),
        pins.sessions,
      ),
    [sessions, workspacePaths, order.ungrouped, pins.sessions],
  )

  const groupKeys = useCallback(() => {
    const keys = orderedWorkspaces.map((ws) => ws.id)
    if (ungrouped.length > 0) keys.push(UNGROUPED_KEY)
    return keys
  }, [orderedWorkspaces, ungrouped.length])

  const isGroupOpen = useCallback(
    (key: string, fallback: boolean) => expanded[key] ?? fallback,
    [expanded],
  )

  useEffect(() => {
    if (!activeId) return
    const ws = orderedWorkspaces.find((item) => item.sessionIds.includes(activeId))
    if (ws) {
      setExpanded((previous) => (previous[ws.id] ? previous : { ...previous, [ws.id]: true }))
      return
    }
    const activeSession = sessions.find((s) => s.id === activeId)
    if (activeSession && !orderedWorkspaces.some((w) => w.path === activeSession.cwd)) {
      setExpanded((previous) =>
        previous[UNGROUPED_KEY] === false ? { ...previous, [UNGROUPED_KEY]: true } : previous,
      )
    }
  }, [activeId, orderedWorkspaces, sessions])

  useEffect(() => {
    const keys = groupKeys()
    if (keys.length === 0) {
      onAllExpandedChange(false)
      return
    }
    const allOpen = keys.every((key) => {
      if (key === UNGROUPED_KEY) return isGroupOpen(key, true)
      const members = membersOf(orderedWorkspaces.find((w) => w.id === key)!)
      return isGroupOpen(key, members.some((s) => s.id === activeId))
    })
    onAllExpandedChange(allOpen)
  }, [expanded, groupKeys, isGroupOpen, membersOf, orderedWorkspaces, activeId, onAllExpandedChange])

  useEffect(() => {
    if (expandAllTick === 0 || expandAllTick === lastExpandTickRef.current) return
    lastExpandTickRef.current = expandAllTick
    const keys = groupKeys()
    const nextExpanded = keys.every((key) => {
      if (key === UNGROUPED_KEY) return isGroupOpen(key, true)
      const members = membersOf(orderedWorkspaces.find((w) => w.id === key)!)
      return isGroupOpen(key, members.some((s) => s.id === activeId))
    })
    const wantExpand = !nextExpanded
    const next: Record<string, boolean> = {}
    for (const key of keys) next[key] = wantExpand
    setExpanded((prev) => ({ ...prev, ...next }))
    // 全部展开时连带展开血缘折叠;收起只收组,不动子树。
    if (wantExpand) setCollapsedChildren({})
  }, [expandAllTick, groupKeys, isGroupOpen, membersOf, orderedWorkspaces, activeId])

  useEffect(() => {
    if (!searchOpen) {
      setQuery('')
      return
    }
    const id = requestAnimationFrame(() => searchInputRef.current?.focus())
    return () => cancelAnimationFrame(id)
  }, [searchOpen])

  useEffect(() => {
    const id = window.setInterval(() => setNow(Date.now()), 60_000)
    return () => window.clearInterval(id)
  }, [])

  // 仅切换会话时展开祖先；刷新恢复与列表推送不覆盖用户的折叠选择。
  useEffect(() => {
    if (!activeId || activeId === revealedSessionRef.current || !sessionsById.has(activeId)) return
    revealedSessionRef.current = activeId
    setCollapsedChildren((prev) => {
      let changed = false
      const next = { ...prev }
      const seen = new Set<string>()
      let cursor = sessionsById.get(activeId)?.parent_session
      while (cursor && !seen.has(cursor)) {
        seen.add(cursor)
        if (next[cursor]) {
          next[cursor] = false
          changed = true
        }
        cursor = sessionsById.get(cursor)?.parent_session
      }
      return changed ? next : prev
    })
  }, [activeId, sessionsById])

  const normalizedQuery = query.trim().toLowerCase()
  const matchesQuery = useCallback(
    (session: SessionSummary) => {
      if (!normalizedQuery) return true
      return sessionDisplayTitle(session).toLowerCase().includes(normalizedQuery)
    },
    [normalizedQuery],
  )
  const searching = normalizedQuery.length > 0
  const filtering = filter === 'running'
  const forceOpen = searching || filtering
  const matchesStatus = useCallback(
    (session: SessionSummary) =>
      !filtering || !!runningIds[session.id] || inProgress.has(session.id) ||
        (session.cwd !== undefined && workspaces.some((workspace) =>
          workspace.path === session.cwd && inProgressWorkspaces.has(workspace.id),
        )),
    [filtering, runningIds, inProgress, inProgressWorkspaces, workspaces],
  )
  const isSessionMarked = useCallback(
    (session: SessionSummary) => inProgress.has(session.id),
    [inProgress],
  )
  const matchesSession = useCallback(
    (session: SessionSummary) => matchesQuery(session) && matchesStatus(session),
    [matchesQuery, matchesStatus],
  )

  const filteredGroups = orderedWorkspaces
    .map((ws) => {
      const allMembers = membersOf(ws)
      const members = allMembers.filter(matchesSession)
      return { ws, allMembers, members }
    })
    .filter((group) => (forceOpen ? group.members.length > 0 : true))

  const filteredUngrouped = ungrouped.filter(matchesSession)
  const empty =
    filteredGroups.length === 0 &&
    filteredUngrouped.length === 0 &&
    (forceOpen || (orderedWorkspaces.length === 0 && ungrouped.length === 0))
  // 计数 = 运行中 ∪ 手动标记:与筛选口径一致,避免出现"列表 3 项、角标 1"。
  const runningCount = sessions.reduce(
    (count, session) =>
      count + (runningIds[session.id] || isSessionMarked(session) ? 1 : 0),
    0,
  )

  const beginDrag = (next: SidebarDrag) => {
    dragRef.current = next
    setDragState(next)
    setDropTarget(null)
  }

  const endDrag = () => {
    dragRef.current = null
    setDragState(null)
    setDropTarget(null)
  }

  /** 离开行/组时收起指示线;真正进入子元素不算离开。 */
  const handleDragLeave = (event: React.DragEvent<HTMLElement>) => {
    const to = event.relatedTarget as Node | null
    if (to && event.currentTarget.contains(to)) return
    setDropTarget(null)
  }

  const beginSessionDrag = (id: string, groupKey: string) => {
    beginDrag({ kind: 'session', id, groupKey })
  }

  const beginWorkspaceDrag = (event: React.DragEvent<HTMLButtonElement>, id: string) => {
    event.dataTransfer.setData(WORKSPACE_DRAG_MIME, id)
    event.dataTransfer.effectAllowed = 'move'
    beginDrag({ kind: 'workspace', id })
  }

  /** 会话的父会话(仅当父也在本组时):只允许同层拖动,避免把子树挪散。 */
  const parentWithin = (memberIds: ReadonlySet<string>, id: string): string | null => {
    const parent = sessionsById.get(id)?.parent_session
    return parent && memberIds.has(parent) ? parent : null
  }

  const persistSessionOrder = (groupKey: string, ids: string[]) => {
    const next = groupKey === UNGROUPED_KEY
      ? { ...order, ungrouped: ids }
      : { ...order, sessions: { ...order.sessions, [groupKey]: ids } }
    writeSidebarOrder(next)
    setOrder(next)
  }

  const handleSessionDragOver = (
    event: React.DragEvent<HTMLDivElement>,
    targetId: string,
    groupKey: string,
    memberIds: ReadonlySet<string>,
  ) => {
    const current = dragRef.current
    if (!current || current.kind !== 'session') return
    if (current.groupKey !== groupKey || current.id === targetId) return
    if (pinnedSessions.has(current.id) !== pinnedSessions.has(targetId)) return
    if (parentWithin(memberIds, current.id) !== parentWithin(memberIds, targetId)) return
    event.preventDefault()
    event.dataTransfer.dropEffect = 'move'
    const placement = placementFromPointer(event)
    setDropTarget((previous) =>
      previous?.kind === 'session' &&
      previous.id === targetId &&
      previous.groupKey === groupKey &&
      previous.placement === placement
        ? previous
        : { kind: 'session', id: targetId, groupKey, placement },
    )
  }

  const handleSessionDrop = (
    event: React.DragEvent<HTMLDivElement>,
    targetId: string,
    groupKey: string,
    memberIds: string[],
  ) => {
    const current = dragRef.current
    const placement = placementFromPointer(event)
    endDrag()
    if (!current || current.kind !== 'session') return
    if (current.groupKey !== groupKey || current.id === targetId) return
    if (pinnedSessions.has(current.id) !== pinnedSessions.has(targetId)) return
    const memberSet = new Set(memberIds)
    if (parentWithin(memberSet, current.id) !== parentWithin(memberSet, targetId)) return
    const next = moveId(memberIds, current.id, targetId, placement)
    if (next === memberIds) return
    event.preventDefault()
    persistSessionOrder(groupKey, next)
  }

  const handleWorkspaceDragOver = (event: React.DragEvent<HTMLDivElement>, targetId: string) => {
    const current = dragRef.current
    if (!current || current.kind !== 'workspace' || current.id === targetId) return
    if (pinnedWorkspaces.has(current.id) !== pinnedWorkspaces.has(targetId)) return
    event.preventDefault()
    event.dataTransfer.dropEffect = 'move'
    const placement = placementFromPointer(event)
    setDropTarget((previous) =>
      previous?.kind === 'workspace' &&
      previous.id === targetId &&
      previous.placement === placement
        ? previous
        : { kind: 'workspace', id: targetId, placement },
    )
  }

  const handleWorkspaceDrop = (event: React.DragEvent<HTMLDivElement>, targetId: string) => {
    const current = dragRef.current
    const placement = placementFromPointer(event)
    endDrag()
    if (!current || current.kind !== 'workspace' || current.id === targetId) return
    if (pinnedWorkspaces.has(current.id) !== pinnedWorkspaces.has(targetId)) return
    const ids = orderedWorkspaces.map((ws) => ws.id)
    const next = moveId(ids, current.id, targetId, placement)
    if (next === ids) return
    event.preventDefault()
    const nextOrder = { ...order, workspaces: next }
    writeSidebarOrder(nextOrder)
    setOrder(nextOrder)
  }

  const closeContextMenu = useCallback(() => setContextMenu(null), [])

  const togglePinned = (kind: 'session' | 'workspace', id: string) => {
    const next = kind === 'session'
      ? { ...pins, sessions: toggleId(pins.sessions, id) }
      : { ...pins, workspaces: toggleId(pins.workspaces, id) }
    writeSidebarPins(next)
    setPins(next)
  }

  /** 删除改成行内二次确认:先请求,确认按钮出现后再真正执行。 */
  const requestDelete = (kind: 'session' | 'workspace', id: string) => {
    closeContextMenu()
    setConfirming({ kind, id })
  }

  const confirmDelete = () => {
    const target = confirming
    setConfirming(null)
    if (!target) return
    if (target.kind === 'session') {
      const session = sessionsById.get(target.id)
      if (session) onDeleteSession(session)
      return
    }
    const workspace = orderedWorkspaces.find((ws) => ws.id === target.id)
    if (workspace) onDeleteWorkspace(workspace)
  }

  // 确认按钮点下时不要被“点到外面”的清场逻辑先撤掉;其余位置/ESC 都取消。
  useEffect(() => {
    if (!confirming) return
    const onPointerDown = (event: PointerEvent) => {
      const target = event.target
      if (target instanceof Element && target.closest('.confirm-delete-btn')) return
      setConfirming(null)
    }
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setConfirming(null)
    }
    window.addEventListener('pointerdown', onPointerDown, true)
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('pointerdown', onPointerDown, true)
      window.removeEventListener('keydown', onKey)
    }
  }, [confirming])

  const beginRename = (session: SessionSummary) => {
    closeContextMenu()
    setRenameTarget(session)
    setRenameDraft(sessionAlias(session.id) ?? sessionDisplayTitle(session))
  }

  const cancelRename = () => {
    setRenameTarget(null)
    setRenameDraft('')
  }

  const commitRename = () => {
    if (!renameTarget) return
    setSessionAlias(renameTarget.id, renameDraft)
    setRenameTarget(null)
    setRenameDraft('')
    bumpCatalogTick()
  }

  const openSessionMenu = (event: React.MouseEvent, session: SessionSummary) => {
    event.preventDefault()
    event.stopPropagation()
    setConfirming(null)
    const pinned = pinnedSessions.has(session.id)
    const workspace = orderedWorkspaces.find((item) => item.path === session.cwd)
    const isCollapsingOthers = workspace !== undefined && collapsedWorkspaceSessions[workspace.id] === session.id
    const canCollapseOthers = workspace !== undefined &&
      inProgressWorkspaces.has(workspace.id) && activeId === session.id
    setContextMenu({
      x: event.clientX,
      y: event.clientY,
      items: [
        {
          key: 'rename',
          label: t('renameSession'),
          icon: <IconPencil size={13} />,
          onSelect: () => beginRename(session),
        },
        {
          key: 'pin',
          label: pinned ? t('unpin') : t('pinToTop'),
          icon: <IconPin size={13} />,
          onSelect: () => togglePinned('session', session.id),
        },
        {
          key: 'in-progress',
          label: inProgress.has(session.id) ? t('unmarkInProgress') : t('markInProgress'),
          icon: <IconCaretRight size={13} />,
          onSelect: () => toggleInProgress(session.id),
        },
        ...(canCollapseOthers ? [{
          key: 'collapse-other-sessions',
          label: isCollapsingOthers ? t('expandOtherSessions') : t('collapseOtherSessions'),
          icon: <IconProgressFlag size={13} />,
          onSelect: () => setCollapsedWorkspaceSessions((previous) => {
            const next = { ...previous }
            if (next[workspace.id] === session.id) delete next[workspace.id]
            else next[workspace.id] = session.id
            return next
          }),
        }] : []),
        {
          key: 'delete',
          label: t('deleteSession'),
          icon: <IconTrash size={13} />,
          danger: true,
          disabled: !!runningIds[session.id],
          onSelect: () => requestDelete('session', session.id),
        },
      ],
    })
  }

  const openWorkspaceMenu = (event: React.MouseEvent, ws: WorkspaceRecord) => {
    event.preventDefault()
    event.stopPropagation()
    setConfirming(null)
    const pinned = pinnedWorkspaces.has(ws.id)
    setContextMenu({
      x: event.clientX,
      y: event.clientY,
      items: [
        {
          key: 'new-session',
          label: t('newSession'),
          icon: <IconNewSession size={13} />,
          onSelect: () => createSessionInWorkspace(ws.id),
        },
        {
          key: 'pin',
          label: pinned ? t('unpin') : t('pinToTop'),
          icon: <IconPin size={13} />,
          onSelect: () => togglePinned('workspace', ws.id),
        },
        {
          key: 'in-progress',
          label: inProgressWorkspaces.has(ws.id) ? t('unmarkInProgress') : t('markInProgress'),
          icon: <IconProgressFlag size={13} />,
          onSelect: () => toggleWorkspaceInProgress(ws.id),
        },
        {
          key: 'delete',
          label: t('deleteWorkspace'),
          icon: <IconTrash size={13} />,
          danger: true,
          onSelect: () => requestDelete('workspace', ws.id),
        },
      ],
    })
  }

  return (
    <div className="sidebar-section">
      <div className="sidebar-filter" role="group" aria-label={t('sidebarFilterLabel')}>
        <button
          type="button"
          className={`sidebar-filter-tab${filter === 'all' ? ' active' : ''}`}
          aria-pressed={filter === 'all'}
          onClick={() => {
            setFilter('all')
            writeSidebarFilter('all')
          }}
        >
          {t('sidebarFilterAll')}
        </button>
        <button
          type="button"
          className={`sidebar-filter-tab${filter === 'running' ? ' active' : ''}`}
          aria-pressed={filter === 'running'}
          onClick={() => {
            setFilter('running')
            writeSidebarFilter('running')
          }}
        >
          {t('sidebarFilterRunning')}
          {runningCount > 0 && <span className="sidebar-filter-count">{runningCount}</span>}
        </button>
      </div>
      {searchOpen && (
        <div className="sidebar-search">
          <IconSessionSearch size={13} />
          <input
            ref={searchInputRef}
            type="search"
            className="sidebar-search-input"
            value={query}
            placeholder={t('searchSessionsPlaceholder')}
            aria-label={t('searchSessions')}
            onChange={(event) => setQuery(event.target.value)}
            onKeyDown={(event) => {
              if (event.key !== 'Escape') return
              setQuery('')
              onSearchOpenChange(false)
            }}
          />
          {query && (
            <button
              type="button"
              className="icon-btn sidebar-search-clear"
              title={t('searchSessionsClear')}
              aria-label={t('searchSessionsClear')}
              onClick={() => setQuery('')}
            >
              <IconClose size={12} />
            </button>
          )}
        </div>
      )}
      <div className="session-list">
        {empty && (
          <div className="empty-hint">
            {searching
              ? t('searchSessionsEmpty')
              : filtering
                ? t('sidebarFilterEmpty')
                : t('emptySessions')}
          </div>
        )}
        {filteredGroups.map(({ ws, allMembers, members }) => {
          const open = searching || isGroupOpen(ws.id, members.some((s) => s.id === activeId))
          const { descendantCount, runningAncestors } = lineageStats(members, runningIds)
          const collapsedSet = searching
            ? undefined
            : new Set(
                Object.keys(collapsedChildren).filter(
                  (id) => collapsedChildren[id] && descendantCount.has(id),
                ),
              )
          const collapsedToSessionId = collapsedWorkspaceSessions[ws.id]
          const nested = nestByParent(members, collapsedSet)
          const visible = collapsedToSessionId
            ? nested.filter(({ session }) => {
                let cursor: string | undefined = session.id
                const seen = new Set<string>()
                while (cursor && !seen.has(cursor)) {
                  if (cursor === collapsedToSessionId) return true
                  seen.add(cursor)
                  cursor = sessionsById.get(cursor)?.parent_session ?? undefined
                }
                return false
              })
            : forceOpen || showAll[ws.id] ? nested : nested.slice(0, COLLAPSED_LIMIT)
          const allIds = allMembers.map((session) => session.id)
          const memberSet = new Set(allIds)
          const dragging = dragState?.kind === 'workspace' && dragState.id === ws.id
          const drop = dropTarget?.kind === 'workspace' && dropTarget.id === ws.id
            ? dropTarget.placement
            : null
          return (
            <div className={`ws-group${open ? ' open' : ''}${dragging ? ' dragging' : ''}`} key={ws.id}>
              <div
                className={`ws-group-head-row${drop ? ` drop-${drop}` : ''}`}
                onDragOver={(event) => handleWorkspaceDragOver(event, ws.id)}
                onDragLeave={handleDragLeave}
                onDrop={(event) => handleWorkspaceDrop(event, ws.id)}
                onContextMenu={(event) => openWorkspaceMenu(event, ws)}
              >
                <button
                  type="button"
                  className={`ws-group-head${members.some((s) => s.id === activeId) ? ' active' : ''}`}
                  title={ws.path}
                  aria-expanded={open}
                  draggable
                  onDragStart={(event) => beginWorkspaceDrag(event, ws.id)}
                  onDragEnd={endDrag}
                  onClick={() => {
                    const fallback = members.some((s) => s.id === activeId)
                    setExpanded((prev) => ({
                      ...prev,
                      [ws.id]: !(prev[ws.id] ?? fallback),
                    }))
                  }}
                >
                  <span className="ws-folder">
                    <IconWorkspace size={14} />
                  </span>
                  <span className="name">{ws.title}</span>
                   {pinnedWorkspaces.has(ws.id) && (
                    <span className="pin-mark" title={t('pinnedLabel')} aria-label={t('pinnedLabel')}>
                      <IconPin size={12} />
                    </span>
                  )}
                  {inProgressWorkspaces.has(ws.id) && (
                    <span className="in-progress-mark" title={t('markedInProgressHint')} aria-label={t('markedInProgressHint')}>
                      <IconProgressFlag size={12} />
                    </span>
                  )}
                  <span className="count">{members.length}</span>
                </button>
                <span className="row-actions">
                  <button
                    type="button"
                    className={`row-action-btn${pinnedWorkspaces.has(ws.id) ? ' active' : ''}`}
                    title={pinnedWorkspaces.has(ws.id) ? t('unpin') : t('pinToTop')}
                    aria-label={pinnedWorkspaces.has(ws.id) ? t('unpin') : t('pinToTop')}
                    aria-pressed={pinnedWorkspaces.has(ws.id)}
                    onClick={() => togglePinned('workspace', ws.id)}
                  >
                    <IconPin size={12} />
                  </button>
                  <button
                    type="button"
                      className="row-action-btn"
                      title={t('newSessionIn', { name: ws.title })}
                      onClick={() => createSessionInWorkspace(ws.id)}
                  >
                    <IconNewSession size={12} />
                  </button>
                  {confirming?.kind === 'workspace' && confirming.id === ws.id ? (
                    <button
                      type="button"
                      className="confirm-delete-btn"
                      onClick={confirmDelete}
                    >
                      {t('confirmDeleteInline')}
                    </button>
                  ) : (
                    <button
                      type="button"
                      className="row-action-btn danger"
                      title={t('deleteWorkspace')}
                      onClick={() => requestDelete('workspace', ws.id)}
                    >
                      <IconTrash size={12} />
                    </button>
                  )}
                </span>
              </div>
              <div className={`ws-group-body${open ? ' open' : ''}`}>
                <div className="ws-group-body-inner">
                  {visible.map(({ session, depth }) => (
                    <SessionRow
                      key={session.id}
                      session={session}
                      depth={depth}
                      active={session.id === activeId}
                      secondaryActive={!!secondaryId && session.id === secondaryId}
                      running={!!runningIds[session.id]}
                      marked={isSessionMarked(session)}
                      now={now}
                      childCount={descendantCount.get(session.id) ?? 0}
                      collapsed={collapsedChildren[session.id] === true}
                      childRunning={runningAncestors.has(session.id)}
                      onToggleCollapse={() => setCollapsedChildren((previous) => ({
                        ...previous,
                        [session.id]: !previous[session.id],
                      }))}
                      onOpen={() => onOpenSession(session.id, ws.id)}
                      pinned={pinnedSessions.has(session.id)}
                      confirmingDelete={confirming?.kind === 'session' && confirming.id === session.id}
                      onRequestDelete={() => requestDelete('session', session.id)}
                      onConfirmDelete={confirmDelete}
                      onTogglePin={() => togglePinned('session', session.id)}
                      onContextMenu={(event) => openSessionMenu(event, session)}
                      drag={{
                        dragging: dragState?.kind === 'session' && dragState.id === session.id,
                        placement:
                          dropTarget?.kind === 'session' &&
                          dropTarget.id === session.id &&
                          dropTarget.groupKey === ws.id
                            ? dropTarget.placement
                            : null,
                        onStart: () => beginSessionDrag(session.id, ws.id),
                        onEnd: endDrag,
                        onOver: (event) => handleSessionDragOver(event, session.id, ws.id, memberSet),
                        onLeave: handleDragLeave,
                        onDrop: (event) => handleSessionDrop(event, session.id, ws.id, allIds),
                      }}
                    />
                  ))}
                  {collapsedToSessionId ? (
                    <button
                      type="button"
                      className="expand-rest"
                      onClick={() => setCollapsedWorkspaceSessions((previous) => {
                        const next = { ...previous }
                        delete next[ws.id]
                        return next
                      })}
                    >
                      {t('expandOtherSessions')}
                    </button>
                  ) : !forceOpen && members.length > COLLAPSED_LIMIT && (
                    <button
                      type="button"
                      className="expand-rest"
                      onClick={() =>
                        setShowAll((prev) => ({ ...prev, [ws.id]: !showAll[ws.id] }))
                      }
                    >
                      {showAll[ws.id]
                        ? t('collapse')
                        : t('expandRest', { n: members.length - COLLAPSED_LIMIT })}
                    </button>
                  )}
                </div>
              </div>
            </div>
          )
        })}
        {filteredUngrouped.length > 0 && (
          <div
            className={`ws-group${searching || isGroupOpen(UNGROUPED_KEY, true) ? ' open' : ''}`}
          >
            <div className="ws-group-head-row">
              <button
                type="button"
                className={`ws-group-head${filteredUngrouped.some((s) => s.id === activeId) ? ' active' : ''}`}
                aria-expanded={searching || isGroupOpen(UNGROUPED_KEY, true)}
                onClick={() =>
                  setExpanded((prev) => ({
                    ...prev,
                    [UNGROUPED_KEY]: !(prev[UNGROUPED_KEY] ?? true),
                  }))
                }
              >
                <span className="ws-folder muted">
                  <IconWorkspace size={14} />
                </span>
                <span className="name">{t('ungrouped')}</span>
                <span className="count">{filteredUngrouped.length}</span>
              </button>
              <span className="ungrouped-actions">
                <button
                  type="button"
                  className="ungrouped-clear"
                  title={t('deleteUngroupedDesc', { n: filteredUngrouped.length })}
                  onClick={() => onDeleteUngrouped(filteredUngrouped)}
                >
                  {t('deleteUngrouped')}
                </button>
              </span>
            </div>
              <div
                className={`ws-group-body${searching || isGroupOpen(UNGROUPED_KEY, true) ? ' open' : ''}`}
              >
                <div className="ws-group-body-inner">
                  {(() => {
                    const { descendantCount, runningAncestors } = lineageStats(
                      filteredUngrouped,
                      runningIds,
                    )
                    const collapsedSet = searching
                      ? undefined
                      : new Set(
                          Object.keys(collapsedChildren).filter(
                            (id) => collapsedChildren[id] && descendantCount.has(id),
                          ),
                        )
                    const allIds = ungrouped.map((session) => session.id)
                    const memberSet = new Set(allIds)
                    return nestByParent(filteredUngrouped, collapsedSet).map(
                      ({ session, depth }) => (
                        <SessionRow
                          key={session.id}
                          session={session}
                          depth={depth}
                          active={session.id === activeId}
                          secondaryActive={!!secondaryId && session.id === secondaryId}
                          running={!!runningIds[session.id]}
                          marked={isSessionMarked(session)}
                          now={now}
                          childCount={descendantCount.get(session.id) ?? 0}
                          collapsed={collapsedChildren[session.id] === true}
                          childRunning={runningAncestors.has(session.id)}
                          onToggleCollapse={() => setCollapsedChildren((previous) => ({
                            ...previous,
                            [session.id]: !previous[session.id],
                          }))}
                          onOpen={() => onOpenSession(session.id)}
                          pinned={pinnedSessions.has(session.id)}
                          confirmingDelete={confirming?.kind === 'session' && confirming.id === session.id}
                          onRequestDelete={() => requestDelete('session', session.id)}
                          onConfirmDelete={confirmDelete}
                          onTogglePin={() => togglePinned('session', session.id)}
                          onContextMenu={(event) => openSessionMenu(event, session)}
                          drag={{
                            dragging: dragState?.kind === 'session' && dragState.id === session.id,
                            placement:
                              dropTarget?.kind === 'session' &&
                              dropTarget.id === session.id &&
                              dropTarget.groupKey === UNGROUPED_KEY
                                ? dropTarget.placement
                                : null,
                            onStart: () => beginSessionDrag(session.id, UNGROUPED_KEY),
                            onEnd: endDrag,
                            onOver: (event) =>
                              handleSessionDragOver(event, session.id, UNGROUPED_KEY, memberSet),
                            onLeave: handleDragLeave,
                            onDrop: (event) =>
                              handleSessionDrop(event, session.id, UNGROUPED_KEY, allIds),
                          }}
                        />
                      ),
                    )
                  })()}
                </div>
              </div>
          </div>
        )}
      </div>
      <SidebarContextMenu menu={contextMenu} onClose={closeContextMenu} />
      <ConfirmDialog
        open={renameTarget !== null}
        title={t('renameSession')}
        desc={t('renameSessionDesc')}
        confirmLabel={t('save')}
        onConfirm={commitRename}
        onCancel={cancelRename}
      >
        <input
          className="confirm-input"
          value={renameDraft}
          maxLength={80}
          autoFocus
          aria-label={t('renameSession')}
          onChange={(event) => setRenameDraft(event.target.value)}
          onFocus={(event) => event.currentTarget.select()}
          onKeyDown={(event) => {
            if (event.key !== 'Enter') return
            event.preventDefault()
            commitRename()
          }}
        />
      </ConfirmDialog>
    </div>
  )
}
