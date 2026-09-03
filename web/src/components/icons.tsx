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
      <path
        d="M8 13V3M8 3l3.5 3.5M8 3 4.5 6.5"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

export function IconStop(props: IconProps) {
  return (
    <Svg {...props}>
      <rect
        x="5"
        y="5"
        width="6"
        height="6"
        rx="1.25"
        fill="currentColor"
      />
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

export function IconSearch(props: IconProps) {
  return (
    <Svg {...props}>
      <circle cx="6.5" cy="6.5" r="4" stroke="currentColor" strokeWidth="1.4" />
      <path d="m9.5 9.5 3.5 3.5" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" />
    </Svg>
  )
}

export function IconGlob(props: IconProps) {
  return (
    <Svg {...props}>
      <circle cx="4.5" cy="4.5" r="1.6" stroke="currentColor" strokeWidth="1.2" />
      <circle cx="9.5" cy="4.5" r="1.6" stroke="currentColor" strokeWidth="1.2" />
      <circle cx="4.5" cy="9.5" r="1.6" stroke="currentColor" strokeWidth="1.2" />
      <circle cx="9.5" cy="9.5" r="1.6" stroke="currentColor" strokeWidth="1.2" />
      <path d="M1 12.5h12" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeDasharray="1.6 1.6" />
    </Svg>
  )
}

export function IconEdit(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5 3.2h6.2a1 1 0 0 1 1 1v5.6M11 5.5l2 2v1.2M6.2 12.8H4.2a1 1 0 0 1-1-1V4.2a1 1 0 0 1 1-1" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round" />
      <path d="M8.4 9.6 9 8l1.6-.6.6 1.6-1.6.6-1.2-.0Z" stroke="currentColor" strokeWidth="1" strokeLinejoin="round" fill="none" />
    </Svg>
  )
}

export function IconShield(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M8 2.5 12.8 4v3.4c0 2.9-2 5.2-4.8 6.1-2.8-.9-4.8-3.2-4.8-6.1V4L8 2.5Z" stroke="currentColor" strokeWidth="1.3" strokeLinejoin="round" />
      <path d="M5.8 8.1 7.5 9.7l2.8-3" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round" fill="none" />
    </Svg>
  )
}

/** 回形针:附件/上传文件的通用图标。 */
export function IconPaperclip(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M14.3 7.4l-6.1 6.1a4 4 0 0 1-5.7-5.7l6.1-6.1a2.7 2.7 0 0 1 3.8 3.8l-6.1 6.1a1.3 1.3 0 0 1-1.9-1.9l5.7-5.6"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

export function IconImage(props: IconProps) {
  return (
    <Svg {...props}>
      <rect x="2.5" y="3.5" width="11" height="9" rx="1.2" stroke="currentColor" strokeWidth="1.3" />
      <circle cx="5.6" cy="6.4" r="1" stroke="currentColor" strokeWidth="1" />
      <path d="m3.5 11.5 3-3 2 2 1.8-1.8 2.2 2.2" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round" fill="none" />
    </Svg>
  )
}

export function IconClose(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m5 5 6 6M11 5l-6 6" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
    </Svg>
  )
}

export function IconCopy(props: IconProps) {
  return (
    <Svg {...props} viewBox="0 0 18 18">
      <rect x="5.5" y="5.5" width="8" height="8" rx="1.4" stroke="currentColor" strokeWidth="1.4" />
      <path d="M12.5 3.5h-4A2.2 2.2 0 0 0 6.3 5.7v0" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
      <path d="M9.3 3.5h-1.4A1.8 1.8 0 0 0 6.1 5.3V5.5" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </Svg>
  )
}

export function IconCheck(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m3.5 8.5 3 3 6-7" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </Svg>
  )
}

export function IconRewind(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M3.5 8a4.5 4.5 0 1 0 1.3-3.2M3.5 4.5V8M3.5 8h4" stroke="currentColor" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round" />
    </Svg>
  )
}

export function IconBranch(props: IconProps) {
  return (
    <Svg {...props} viewBox="0 0 18 18">
      <path
        d="M5 5.1v5.8M14 5.1v1.4a5 5 0 0 1-5 5H6.6"
        stroke="currentColor"
        strokeWidth="1.6"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <circle cx="5" cy="3.5" r="1.6" stroke="currentColor" strokeWidth="1.4" />
      <circle cx="5" cy="12.5" r="1.6" stroke="currentColor" strokeWidth="1.4" />
      <circle cx="14" cy="3.5" r="1.6" stroke="currentColor" strokeWidth="1.4" />
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
      {/* 经典六齿齿轮:外轮廓 + 中心孔 */}
      <path
        d="M6.4 1.75h3.2l.28 1.42c.48.13.92.35 1.31.64l1.38-.52 2.26 2.26-.52 1.38c.29.39.51.83.64 1.31L15.25 6.4v3.2l-1.42.28c-.13.48-.35.92-.64 1.31l.52 1.38-2.26 2.26-1.38-.52c-.39.29-.83.51-1.31.64L9.6 15.25H6.4l-.28-1.42a5.1 5.1 0 0 1-1.31-.64l-1.38.52-2.26-2.26.52-1.38a5.1 5.1 0 0 1-.64-1.31L.75 9.6V6.4l1.42-.28c.13-.48.35-.92.64-1.31l-.52-1.38L4.55 1.17l1.38.52c.39-.29.83-.51 1.31-.64L6.4 1.75Z"
        stroke="currentColor"
        strokeWidth="1.15"
        strokeLinejoin="round"
      />
      <circle cx="8" cy="8" r="2.2" stroke="currentColor" strokeWidth="1.2" />
    </Svg>
  )
}

export function IconFolder(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M2.2 5.2c0-.77.62-1.4 1.4-1.4h2.55c.3 0 .58.12.78.34l.72.78c.2.22.48.34.78.34h4.95c.77 0 1.4.63 1.4 1.4v5.74c0 .77-.63 1.4-1.4 1.4H3.6c-.78 0-1.4-.63-1.4-1.4V5.2Z"
        fill="currentColor"
        fillOpacity="0.14"
        stroke="currentColor"
        strokeWidth="1.15"
        strokeLinejoin="round"
      />
      <path
        d="M2.35 7.1h11.3"
        stroke="currentColor"
        strokeWidth="1.1"
        strokeLinecap="round"
        opacity="0.45"
      />
    </Svg>
  )
}

/** 展开全部工作区(多层折线张开)。 */
export function IconExpandAll(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M3.5 6.2 8 2.8l4.5 3.4M3.5 9.8 8 13.2l4.5-3.4"
        stroke="currentColor"
        strokeWidth="1.35"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

/** 收起全部工作区(多层折线合拢)。 */
export function IconCollapseAll(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M3.5 3.8 8 7.2l4.5-3.4M3.5 12.2 8 8.8l4.5 3.4"
        stroke="currentColor"
        strokeWidth="1.35"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

export function IconPrompt(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M4.5 3.5h7v9H8.5l-2.5 2v-2h-1.5v-9Z"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinejoin="round"
      />
      <path d="M6.5 6.5h5M6.5 9h3.5" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
    </Svg>
  )
}

/** 品牌标识:极简方块 + 横线,黑白体系。 */
export function BrandMark({ size = 24 }: IconProps) {
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden="true" style={{ flex: 'none' }}>
      <rect
        x="5"
        y="5"
        width="14"
        height="14"
        rx="3"
        fill="none"
        stroke="currentColor"
        strokeWidth="1.5"
      />
      <path d="M8 12h8" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
    </svg>
  )
}
