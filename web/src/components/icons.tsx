/** Minimal inline-SVG icon set, 16px grid, currentColor. */

import type { ReactNode } from 'react'

type IconProps = { size?: number }

function Svg({
  size = 16,
  children,
  viewBox = '0 0 16 16',
}: IconProps & { children: ReactNode; viewBox?: string }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox={viewBox}
      fill="none"
      aria-hidden="true"
      style={{ flex: 'none', display: 'block' }}
    >
      {children}
    </svg>
  )
}

export function IconSend(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M2 8h9M8 4.5 11.5 8 8 11.5" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" />
    </Svg>
  )
}

export function IconStop(props: IconProps) {
  return (
    <Svg {...props}>
      <rect x="4.5" y="4.5" width="7" height="7" rx="1.5" fill="currentColor" />
    </Svg>
  )
}

export function IconTerminal(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m3 4.5 3 3-3 3M8 11.5h5" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </Svg>
  )
}

export function IconRead(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M8 3.5c-1.6-1-3.8-1.2-5.5-.6v9.6c1.7-.6 3.9-.4 5.5.6 1.6-1 3.8-1.2 5.5-.6V2.9c-1.7-.6-3.9-.4-5.5.6Zm0 0v9.6" stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round" />
    </Svg>
  )
}

export function IconWrite(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m9.8 3.2 3 3L6 13H3v-3l6.8-6.8ZM8.8 4.2l3 3" stroke="currentColor" strokeWidth="1.4" strokeLinejoin="round" />
    </Svg>
  )
}

export function IconThink(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M8 2.5 9.3 6l3.5 1.3-3.5 1.3L8 12.1 6.7 8.6 3.2 7.3 6.7 6 8 2.5Z" stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round" />
      <path d="M12.5 11.5l.6 1.4 1.4.6-1.4.6-.6 1.4-.6-1.4-1.4-.6 1.4-.6.6-1.4Z" fill="currentColor" />
    </Svg>
  )
}

export function IconTool(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M10.5 2.5a3 3 0 0 0-3.9 3.9L2.5 10.5a1.4 1.4 0 0 0 2 2l4.1-4.1a3 3 0 0 0 3.9-3.9l-1.8 1.8-1.9-.6-.6-1.9 1.8-1.8a3 3 0 0 0 .5.5Z" stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round" />
    </Svg>
  )
}

export function IconChevron(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m5 3.5 5 4.5-5 4.5" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </Svg>
  )
}

export function IconPlus(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M8 3v10M3 8h10" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
    </Svg>
  )
}

export function IconTrash(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M3 4.5h10M6.5 4V3h3v1M4.5 4.5l.6 8.5h5.8l.6-8.5M6.7 7v4M9.3 7v4" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round" />
    </Svg>
  )
}

export function IconChat(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M2.5 3.5h11v7h-6l-3 2.5v-2.5h-2v-7Z" stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round" />
    </Svg>
  )
}

export function IconSliders(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M3 5h6M12 5h1M3 11h1M7 11h6" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
      <circle cx="10.5" cy="5" r="1.6" stroke="currentColor" strokeWidth="1.3" />
      <circle cx="5.5" cy="11" r="1.6" stroke="currentColor" strokeWidth="1.3" />
    </Svg>
  )
}

export function IconGear(props: IconProps) {
  return (
    <Svg {...props}>
      <circle cx="8" cy="8" r="2.2" stroke="currentColor" strokeWidth="1.3" />
      <path
        d="M8 2.2v1.6M8 12.2v1.6M2.2 8h1.6M12.2 8h1.6M3.9 3.9l1.1 1.1M11 11l1.1 1.1M12.1 3.9 11 5M5 11l-1.1 1.1"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinecap="round"
      />
    </Svg>
  )
}

export function IconFolder(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M2.5 4h4l1.2 1.5h5.8V12h-11V4Z" stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round" />
    </Svg>
  )
}

/** Brand mark: double chevron in a rounded tile. */
export function BrandMark({ size = 22 }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true" style={{ flex: 'none' }}>
      <rect x="1" y="1" width="22" height="22" rx="7" fill="var(--accent)" />
      <path d="m7 8 4 4-4 4M12.5 8l4 4-4 4" stroke="#fff" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" fill="none" />
    </svg>
  )
}
