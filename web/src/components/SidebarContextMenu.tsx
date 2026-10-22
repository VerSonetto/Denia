import { useEffect, useRef, type ReactNode } from 'react'
import { createPortal } from 'react-dom'

/** 一条自定义右键菜单项。 */
export interface SidebarMenuEntry {
  key: string
  label: string
  icon?: ReactNode
  danger?: boolean
  disabled?: boolean
  onSelect(): void
}

/** 打开中的菜单:视口坐标 + 当前动作集合。 */
export interface SidebarMenuState {
  x: number
  y: number
  items: SidebarMenuEntry[]
}

const MENU_WIDTH = 190
const MENU_ITEM_HEIGHT = 30
const MENU_PADDING = 8

/**
 * 侧栏自定义右键菜单(替代浏览器默认菜单)。
 *
 * 挂在 body 上而不是侧栏内:侧栏有 overflow 裁剪,固定定位也仍会被祖先的
 * transform 影响;portal 到 body 才能稳定贴着指针显示。
 */
export function SidebarContextMenu({
  menu,
  onClose,
}: {
  menu: SidebarMenuState | null
  onClose: () => void
}) {
  const menuRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!menu) return
    menuRef.current?.focus({ preventScroll: true })

    const onPointerDown = (event: PointerEvent) => {
      if (menuRef.current?.contains(event.target as Node)) return
      onClose()
    }
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return
      event.preventDefault()
      onClose()
    }
    const close = () => onClose()
    window.addEventListener('pointerdown', onPointerDown, true)
    window.addEventListener('keydown', onKey)
    window.addEventListener('scroll', close, true)
    window.addEventListener('resize', close)
    window.addEventListener('blur', close)
    return () => {
      window.removeEventListener('pointerdown', onPointerDown, true)
      window.removeEventListener('keydown', onKey)
      window.removeEventListener('scroll', close, true)
      window.removeEventListener('resize', close)
      window.removeEventListener('blur', close)
    }
  }, [menu, onClose])

  if (!menu) return null

  // 先按估算尺寸夹住视口;条目高度固定,不需要等渲染后再测量。
  const height = menu.items.length * MENU_ITEM_HEIGHT + MENU_PADDING
  const left = Math.max(8, Math.min(menu.x, window.innerWidth - MENU_WIDTH - 8))
  const top = Math.max(8, Math.min(menu.y, window.innerHeight - height - 8))

  return createPortal(
    <div
      ref={menuRef}
      className="sidebar-context-menu"
      role="menu"
      tabIndex={-1}
      style={{ left, top, width: MENU_WIDTH }}
      onContextMenu={(event) => event.preventDefault()}
    >
      {menu.items.map((item) => (
        <button
          key={item.key}
          type="button"
          role="menuitem"
          className={`sidebar-menu-item${item.danger ? ' danger' : ''}`}
          disabled={item.disabled}
          onClick={() => {
            if (item.disabled) return
            onClose()
            item.onSelect()
          }}
        >
          {item.icon && <span className="sidebar-menu-icon">{item.icon}</span>}
          <span className="sidebar-menu-label">{item.label}</span>
        </button>
      ))}
    </div>,
    document.body,
  )
}
