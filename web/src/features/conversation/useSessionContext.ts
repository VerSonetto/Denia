import { useCallback, useEffect, useRef, useState } from 'react'
import * as api from '../../api'
import { sameBreakdown } from './sessionState'

/** Only one poll per session can be in flight; stale responses never cross sessions. */
export function useSessionContext(activeId: string | null) {
  const [context, setContext] = useState<api.ContextBreakdownResponse | null>(null)
  const refreshRef = useRef<() => void>(() => {})
  useEffect(() => {
    setContext(null)
    const controller = new AbortController()
    let pending = false
    const refresh = () => {
      if (!activeId || document.hidden || pending || controller.signal.aborted) return
      pending = true
      void api.contextBreakdown(activeId, controller.signal).then(next => {
        if (!controller.signal.aborted) {
          setContext(previous => sameBreakdown(previous, next) ? previous : next)
        }
      }).catch(() => {}).finally(() => { pending = false })
    }
    refreshRef.current = refresh
    refresh()
    const timer = window.setInterval(refresh, 1500)
    return () => { controller.abort(); window.clearInterval(timer) }
  }, [activeId])
  const refreshContextBreakdown = useCallback(() => refreshRef.current(), [])
  return { context, refreshContextBreakdown }
}
