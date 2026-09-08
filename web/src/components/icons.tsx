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

/** 眼睛:只读权限(可看不可改)。 */
export function IconEye(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M1.8 8S3.7 4 8 4s6.2 4 6.2 4-1.9 4-6.2 4-6.2-4-6.2-4Z" stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round" />
      <circle cx="8" cy="8" r="1.8" stroke="currentColor" strokeWidth="1.2" />
    </Svg>
  )
}

/** 铅笔:工作区写入权限。 */
export function IconPencil(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M11.3 2a1.9 1.9 0 1 1 2.7 2.7L5 13.7 1.3 14.7 2.3 11l9-9Z" stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round" />
      <path d="m10.2 3.1 2.7 2.7" stroke="currentColor" strokeWidth="1.2" />
    </Svg>
  )
}

/** 闪电:完整权限(不受限)。 */
export function IconZap(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M8.7 1.3 2 9.3h6L7.3 14.7 14 6.7H8l.7-5.4Z" stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round" />
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

/** 撤销/回退:折返箭头(lucide undo-2),小尺寸下比残缺圆环更易辨认。 */
export function IconRewind(props: IconProps) {
  return (
    <Svg {...props} viewBox="0 0 24 24">
      <path
        d="M9 14 4 9l5-5"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path
        d="M4 9h10.5a5.5 5.5 0 0 1 5.5 5.5 5.5 5.5 0 0 1-5.5 5.5H11"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

/** 撤销优化:Ctrl+Z 式弧形箭头(lucide undo),与会话回退折返箭头区分。 */
export function IconUndo(props: IconProps) {
  return (
    <Svg {...props} viewBox="0 0 24 24">
      <path
        d="M3 7v6h6"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path
        d="M21 17a9 9 0 0 0-9-9 9 9 0 0 0-6.7 2.9L3 13"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

export function IconBranch(props: IconProps) {
  return (
    <Svg {...props} viewBox="0 0 18 18">
      <path
        d="M5 5.6v4.2a4 4 0 0 0 4 4h3.4"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <circle cx="5" cy="3.6" r="1.4" stroke="currentColor" strokeWidth="1.4" />
      <circle cx="12.4" cy="13.8" r="1.4" stroke="currentColor" strokeWidth="1.4" />
    </Svg>
  )
}

export function IconDownload(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M8 2.5v8M8 10.5l-3-3M8 10.5l3-3M3 12.5v1a1.5 1.5 0 0 0 1.5 1.5h7a1.5 1.5 0 0 0 1.5-1.5v-1"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
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

/** 普通文件(@ 提及菜单等处的文件条目)。 */
export function IconFile(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M5.5 2.5h4.2L13 6.3v7.2a1.2 1.2 0 0 1-1.2 1.2H5.5a1.2 1.2 0 0 1-1.2-1.2V3.7c0-.66.54-1.2 1.2-1.2Z"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinejoin="round"
      />
      <path
        d="M9.7 2.5v3.8h3.3"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinejoin="round"
        opacity="0.5"
      />
    </Svg>
  )
}

/** 收起侧栏:面板左收(lucide panel-left-close)。 */
export function IconPanelClose(props: IconProps) {
  return (
    <Svg {...props} viewBox="0 0 24 24">
      <rect width="18" height="18" x="3" y="3" rx="2" stroke="currentColor" strokeWidth="2" />
      <path d="M9 3v18" stroke="currentColor" strokeWidth="2" />
      <path
        d="m16 15-3-3 3-3"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

/** 展开侧栏:面板左开(lucide panel-left-open)。 */
export function IconPanelOpen(props: IconProps) {
  return (
    <Svg {...props} viewBox="0 0 24 24">
      <rect width="18" height="18" x="3" y="3" rx="2" stroke="currentColor" strokeWidth="2" />
      <path d="M9 3v18" stroke="currentColor" strokeWidth="2" />
      <path
        d="m14 9 3 3-3 3"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

/** 添加工作区:带加号的文件夹(lucide folder-plus)。 */
export function IconFolderPlus(props: IconProps) {
  return (
    <Svg {...props} viewBox="0 0 24 24">
      <path
        d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinejoin="round"
      />
      <path
        d="M12 10v6M9 13h6"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
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

/** 提示词优化:魔法/星芒,表示“优化”。 */
export function IconSparkles(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M6 2.5 7 5.2l2.7 1-2.7 1-1 2.7-1-2.7-2.7-1 2.7-1L6 2.5Z"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinejoin="round"
      />
      <path
        d="m11.8 9.6.55 1.45 1.45.55-1.45.55-.55 1.45-.55-1.45-1.45-.55 1.45-.55.55-1.45Z"
        fill="currentColor"
      />
    </Svg>
  )
}

/** 优化进行中:旋转弧形 loading 图标。 */
export function IconSpinner(props: IconProps) {
  return (
    <Svg {...props}>
      <circle
        cx="8"
        cy="8"
        r="5.5"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeDasharray="8 20"
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

/** 钥匙:密钥/凭据。 */
export function IconKey(props: IconProps) {
  return (
    <Svg {...props}>
      <circle cx="5.5" cy="5.5" r="2.6" stroke="currentColor" strokeWidth="1.3" />
      <path
        d="m7.5 7.5 5.4 5.4M10.6 10.6l1.6-1.6M12.4 12.4l1.4-1.4"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinecap="round"
      />
    </Svg>
  )
}

/** 心跳折线:连通性测试。 */
export function IconActivity(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M1.8 8.2h3l1.7-4 2.8 8 1.8-4h3.1"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

/** 层叠:模型目录。 */
export function IconStack(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="m8 2.4 5.6 2.8L8 8 2.4 5.2 8 2.4Z"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinejoin="round"
      />
      <path
        d="m2.8 8.4 5.2 2.6 5.2-2.6M2.8 11.4l5.2 2.6 5.2-2.6"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

/** 警告三角:错误提示。 */
export function IconAlert(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M8 2.6 14.2 13H1.8L8 2.6Z"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinejoin="round"
      />
      <path d="M8 6.4v3" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" />
      <circle cx="8" cy="11.2" r="0.75" fill="currentColor" />
    </Svg>
  )
}

/** 环形箭头:重试/重新探测。 */
export function IconRefresh(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M13.2 8a5.2 5.2 0 1 1-1.6-3.75M13.4 1.9v2.6h-2.6"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

/** 返回箭头:抽屉/步骤回退。 */
export function IconArrowLeft(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M13 8H3M6.5 4.5 3 8l3.5 3.5"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}
