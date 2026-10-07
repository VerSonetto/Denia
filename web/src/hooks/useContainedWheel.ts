import { useCallback, useRef } from 'react'

function scrollAxis(root: HTMLElement, target: Element, delta: number, horizontal: boolean, mode: number) {
  if (delta === 0) return
  for (let node: Element | null = target; node && root.contains(node); node = node.parentElement) {
    if (node instanceof HTMLElement) {
      const style = window.getComputedStyle(node)
      const overflow = horizontal ? style.overflowX : style.overflowY
      if (overflow === 'auto' || overflow === 'scroll' || overflow === 'overlay') {
        const size = horizontal ? node.clientWidth : node.clientHeight
        const max = (horizontal ? node.scrollWidth : node.scrollHeight) - size
        const start = horizontal ? node.scrollLeft : node.scrollTop
        if (max > 0 && (delta > 0 ? start < max : start > 0)) {
          const unit = mode === 1 ? parseFloat(style.lineHeight) || 16 : mode === 2 ? size : 1
          const next = Math.max(0, Math.min(max, start + delta * unit))
          if (horizontal) node.scrollLeft = next
          else node.scrollTop = next
          return
        }
      }
    }
    if (node === root) break
  }
}

export function useContainedWheel<T extends HTMLElement>() {
  const detachRef = useRef<(() => void) | null>(null)
  return useCallback((root: T | null) => {
    detachRef.current?.()
    detachRef.current = null
    if (!root) return
    const onWheel = (event: WheelEvent) => {
      // Ctrl+wheel also represents touchpad pinch zoom; leave it to the browser.
      if (event.ctrlKey) return
      event.stopPropagation()
      if (event.defaultPrevented) return
      const target = event.target instanceof Element ? event.target : (event.target as Node | null)?.parentElement
      if (!target) return
      // React wheel listeners are passive. Own the default action here so neither
      // unused delta nor a clipped descendant can scroll the conversation behind it.
      event.preventDefault()
      const shift = event.shiftKey && event.deltaX === 0
      scrollAxis(root, target, shift ? event.deltaY : event.deltaX, true, event.deltaMode)
      scrollAxis(root, target, shift ? 0 : event.deltaY, false, event.deltaMode)
    }
    root.addEventListener('wheel', onWheel, { passive: false })
    detachRef.current = () => root.removeEventListener('wheel', onWheel)
  }, [])
}
