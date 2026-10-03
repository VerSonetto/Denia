import { useLayoutEffect, useRef, useState, type ReactNode } from 'react'

interface RowObserver {
  observer: IntersectionObserver
  listeners: Map<Element, (visible: boolean) => void>
}

const observers = new WeakMap<Element, RowObserver>()

function observeRow(root: Element, target: Element, listener: (visible: boolean) => void) {
  let shared = observers.get(root)
  if (!shared) {
    const listeners = new Map<Element, (visible: boolean) => void>()
    const observer = new IntersectionObserver((entries) => {
      for (const entry of entries) listeners.get(entry.target)?.(entry.isIntersecting)
    }, { root, rootMargin: '800px 0px' })
    shared = { observer, listeners }
    observers.set(root, shared)
  }
  const current = shared
  current.listeners.set(target, listener)
  current.observer.observe(target)
  return () => {
    current.observer.unobserve(target)
    current.listeners.delete(target)
    if (current.listeners.size === 0) {
      current.observer.disconnect()
      observers.delete(root)
    }
  }
}

export function ViewportRow({ children, eager = false }: { children: ReactNode; eager?: boolean }) {
  const rootRef = useRef<HTMLDivElement>(null)
  const heightRef = useRef<number | null>(null)
  const [visible, setVisible] = useState(eager || typeof IntersectionObserver === 'undefined')
  useLayoutEffect(() => {
    const target = rootRef.current
    const root = target?.closest('.conversation-scroll')
    if (!target || !root || typeof IntersectionObserver === 'undefined') {
      setVisible(true)
      return
    }
    return observeRow(root, target, (intersects) => {
      const selection = document.getSelection()
      if (!intersects && (target.contains(document.activeElement) ||
        (!selection?.isCollapsed && (target.contains(selection?.anchorNode ?? null) ||
          target.contains(selection?.focusNode ?? null))) ||
        target.querySelector('.process-body, .disc-body, .ask-card, [aria-expanded="true"]'))) return
      if (!intersects) heightRef.current = target.getBoundingClientRect().height
      setVisible(intersects)
    })
  }, [])
  useLayoutEffect(() => {
    const target = rootRef.current
    if (!visible || !target) return
    const measure = () => {
      heightRef.current = target.getBoundingClientRect().height
    }
    measure()
    if (typeof ResizeObserver === 'undefined') return
    const observer = new ResizeObserver(measure)
    observer.observe(target)
    return () => observer.disconnect()
  }, [visible])
  return <div ref={rootRef} className="transcript-viewport-row"
    style={visible ? undefined : { height: heightRef.current ?? 96 }}>
    {visible ? children : null}
  </div>
}
