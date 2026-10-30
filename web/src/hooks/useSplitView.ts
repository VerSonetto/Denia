import { useCallback, useEffect, useRef, useState } from 'react'
import { setActiveId } from '../appStore'
import type { ExtensionTab } from '../extensionsRoute'
import {
  SESSION_DRAG_MIME,
  clampSplitRatio,
  dropSessionOnSide,
  ratioFromPointer,
  readSplitRatio,
  reconcileSplit,
  writeSplitRatio,
  type PaneSlot,
  type SplitSide,
  type SplitState,
  type SplitView,
} from '../splitView'
import type { SessionSummary, WorkspaceRecord } from '../types'

/**
 * 分屏:拖动侧栏会话到会话窗口。
 * 副窗格会话与它相对主窗格的位置;null = 单窗格。侧栏高亮、URL、右侧
 * 面板仍只看 activeId(主窗格),副窗格是"参考视图"。
 */
export function useSplitView(options: {
  activeId: string | null
  sessions: SessionSummary[]
  sessionsLoaded: boolean
  workspaces: WorkspaceRecord[]
  isMobile: boolean
  extensionTab: ExtensionTab | null
}) {
  const { activeId, sessions, sessionsLoaded, workspaces, isMobile, extensionTab } = options
  const [split, setSplit] = useState<SplitState | null>(null)
  const [splitRatio, setSplitRatio] = useState(() => readSplitRatio())
  // 会话拖入时的落点提示(left/right),松开或离开即清除。
  const [dragHint, setDragHint] = useState<SplitSide | null>(null)
  const splitViewRef = useRef<HTMLDivElement | null>(null)
  const splitDragRef = useRef<number | null>(null)
  const liveSplitRatioRef = useRef(splitRatio)

  const splitActive = split !== null && activeId !== null

  // 窗格里的会话不存在了(被删除/被其他实例删掉)就退回单窗格:
  // 留一个指向空气的窗格只会让人以为会话还在。
  useEffect(() => {
    if (!split || !sessionsLoaded) return
    const alive = new Set(sessions.map((s) => s.id))
    setSplit((prev) =>
      prev
        ? reconcileSplit({ primaryId: activeId, split: prev }, (id) => alive.has(id)).split
        : prev,
    )
  }, [split, sessions, sessionsLoaded, activeId])

  // 手机屏放不下两个会话;扩展页占满主列。两者与分屏互斥。
  useEffect(() => {
    if (isMobile || extensionTab) setSplit(null)
  }, [isMobile, extensionTab])

  // 拖拽在窗口外松开时没有 drop 事件:靠 dragend 兜底清掉落点提示。
  useEffect(() => {
    const clear = () => setDragHint(null)
    window.addEventListener('dragend', clear)
    return () => window.removeEventListener('dragend', clear)
  }, [])

  /**
   * 把一次"打开会话"的结果套到全局状态上。
   * 主窗格会话变化走 setActiveId,副窗格变化只落在 split 上。
   */
  const applySplitView = useCallback(
    (next: SplitView, wsId?: string | null) => {
      if (next.primaryId !== null && next.primaryId !== activeId) {
        setActiveId(next.primaryId, wsId ?? null)
      }
      setSplit(next.split)
    },
    [activeId],
  )

  /** 会话所属工作区 id(drop 时与会话头一致地更新 pendingWsId)。 */
  const workspaceIdForSession = useCallback(
    (id: string): string | null => {
      const session = sessions.find((s) => s.id === id)
      if (!session) return null
      return workspaces.find((w) => w.path === session.cwd)?.id ?? null
    },
    [sessions, workspaces],
  )

  /** 指针落在容器的哪一侧(拖放落点)。 */
  const sideFromPointer = (event: React.DragEvent<HTMLElement>): SplitSide => {
    const rect = event.currentTarget.getBoundingClientRect()
    return event.clientX - rect.left < rect.width / 2 ? 'left' : 'right'
  }

  /** 拖动自家侧栏的会话经过会话区:亮出落点提示。 */
  const onPaneDragOver = (event: React.DragEvent<HTMLDivElement>) => {
    if (!event.dataTransfer.types.includes(SESSION_DRAG_MIME)) return
    event.preventDefault()
    event.dataTransfer.dropEffect = 'copy'
    setDragHint(sideFromPointer(event))
  }

  const onPaneDragLeave = (event: React.DragEvent<HTMLDivElement>) => {
    const to = event.relatedTarget as Node | null
    if (to && event.currentTarget.contains(to)) return
    setDragHint(null)
  }

  /** 松开:会话落到拖入的那一侧(已在屏上的会话则互换窗格)。 */
  const onPaneDrop = (event: React.DragEvent<HTMLDivElement>) => {
    const id = event.dataTransfer.getData(SESSION_DRAG_MIME)
    setDragHint(null)
    // 不是自家会话(文件/文本)不接管。
    if (!id) return
    event.preventDefault()
    const next = dropSessionOnSide({ primaryId: activeId, split }, id, sideFromPointer(event))
    applySplitView(next, workspaceIdForSession(id))
  }

  /* 分隔条:拖动调宽 + 退出分屏。 */

  const beginSplitDrag = (event: React.PointerEvent<HTMLDivElement>) => {
    if (event.button !== 0) return
    event.preventDefault()
    splitDragRef.current = event.pointerId
    try {
      event.currentTarget.setPointerCapture(event.pointerId)
    } catch {
      /* 合成/已失效的指针:没有 capture 也能靠 grip 上的 move 拖动 */
    }
    splitViewRef.current?.classList.add('dragging')
  }

  const moveSplitDrag = (event: React.PointerEvent<HTMLDivElement>) => {
    const host = splitViewRef.current
    if (splitDragRef.current === null || !host) return
    const rect = host.getBoundingClientRect()
    const ratio = ratioFromPointer(event.clientX, rect.left, rect.width)
    // 拖动期间直接改 DOM 变量、不落 state:两个会话子树都很大,每帧
    // setState 会把整页重渲染;松手时一次性落库。
    liveSplitRatioRef.current = ratio
    host.style.setProperty('--split-left', `${ratio * 100}%`)
  }

  const endSplitDrag = (event: React.PointerEvent<HTMLDivElement>) => {
    if (splitDragRef.current === null) return
    splitDragRef.current = null
    splitViewRef.current?.classList.remove('dragging')
    try {
      event.currentTarget.releasePointerCapture(event.pointerId)
    } catch {
      /* 指针已释放:忽略 */
    }
    setSplitRatio(liveSplitRatioRef.current)
    writeSplitRatio(liveSplitRatioRef.current)
  }

  const keySplitDrag = (event: React.KeyboardEvent<HTMLDivElement>) => {
    if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return
    event.preventDefault()
    const step = event.shiftKey ? 0.1 : 0.03
    const ratio = clampSplitRatio(splitRatio + (event.key === 'ArrowLeft' ? -step : step))
    setSplitRatio(ratio)
    writeSplitRatio(ratio)
  }

  /** 点击窗格把焦点移过去:侧栏点击与拖放的落点都以焦点窗格为准。 */
  const focusPane = useCallback((slot: PaneSlot) => {
    setSplit((prev) => {
      if (!prev || prev.focus === slot) return prev
      return { ...prev, focus: slot }
    })
  }, [])

  /** 副窗格的会话切换出口:约束在副窗格;会话消失就退回单窗格。 */
  const navigateSecondary = useCallback((id: string | null) => {
    setSplit((prev) => {
      if (!prev) return prev
      if (id === null) return null
      return { ...prev, secondaryId: id, focus: 'secondary' }
    })
  }, [])

  return {
    split,
    setSplit,
    splitRatio,
    dragHint,
    splitViewRef,
    splitActive,
    applySplitView,
    onPaneDragOver,
    onPaneDragLeave,
    onPaneDrop,
    beginSplitDrag,
    moveSplitDrag,
    endSplitDrag,
    keySplitDrag,
    focusPane,
    navigateSecondary,
  }
}
