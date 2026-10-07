/** Minimal inline-SVG icon set, 16px grid, currentColor. */

import type { ReactNode } from 'react'

import { COMMAND_GLYPHS } from '../lib/slashGlyphs'

type IconProps = { size?: number; className?: string }

function Svg({
  size = 16,
  children,
  viewBox = '0 0 16 16',
  className,
}: IconProps & { children: ReactNode; viewBox?: string }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox={viewBox}
      fill="none"
      aria-hidden="true"
      className={className}
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
        x="3.5"
        y="3.5"
        width="9"
        height="9"
        rx="1.75"
        fill="currentColor"
      />
    </Svg>
  )
}

/** 工具与导航图标共用 16px 网格、1.25px 圆角描边。 */
function StrokeSvg({ children, ...props }: IconProps & { children: ReactNode }) {
  return (
    <Svg {...props}>
      <g stroke="currentColor" strokeWidth="1.25" strokeLinecap="round" strokeLinejoin="round">
        {children}
      </g>
    </Svg>
  )
}

export function IconTerminal(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <rect x="2" y="3" width="12" height="10" rx="1.75" />
      <path d="m4.5 6 2 2-2 2M9 10h2.5" />
    </StrokeSvg>
  )
}

/**
 * slash 内置命令图标:按名字取专属 glyph(plan/compact 各有语义),未登记
 * 的命令回退终端符。数据源与输入框卡片共用 lib/slashGlyphs,不在这里另画。
 */
export function IconSlashCommand({ name, size = 13 }: IconProps & { name: string }) {
  const glyph = COMMAND_GLYPHS[name]
  if (!glyph) return <IconTerminal size={size} />
  return (
    <Svg size={size}>
      {glyph.paths.map((d) => (
        <path
          key={d}
          d={d}
          stroke="currentColor"
          strokeWidth={glyph.stroke}
          strokeLinecap="round"
          strokeLinejoin="round"
        />
      ))}
    </Svg>
  )
}

export function IconRead(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M8 4c-1.6-1.1-3.7-1.5-6-1v9c2.3-.5 4.4-.1 6 1 1.6-1.1 3.7-1.5 6-1V3c-2.3-.5-4.4-.1-6 1Zm0 0v9M4.25 6h1.25M10.5 6h1.25" />
    </StrokeSvg>
  )
}

export function IconWrite(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M9 2H4a1.5 1.5 0 0 0-1.5 1.5v9A1.5 1.5 0 0 0 4 14h8a1.5 1.5 0 0 0 1.5-1.5V6L9 2Zm0 0v4h4.5M8 8v4M6 10h4" />
    </StrokeSvg>
  )
}

/** 思考：对称脑形轮廓与两处内折，减少小尺寸下的零碎细节。 */
export function IconThink(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M8 4a2 2 0 0 0-3.9-.6 2.5 2.5 0 0 0-1.8 3.9 2.75 2.75 0 0 0 1.2 4.8A2.25 2.25 0 0 0 8 12V4Zm0 0a2 2 0 0 1 3.9-.6 2.5 2.5 0 0 1 1.8 3.9 2.75 2.75 0 0 1-1.2 4.8A2.25 2.25 0 0 1 8 12" />
      <path d="M4 6.5c1.1 0 1.75.65 1.75 1.75M12 6.5c-1.1 0-1.75.65-1.75 1.75" />
    </StrokeSvg>
  )
}

/** 计划模式专属：剪贴板清单。 */
export function IconPlan(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5.5 2.5h-1A1.5 1.5 0 0 0 3 4v9a1.5 1.5 0 0 0 1.5 1.5h7A1.5 1.5 0 0 0 13 13V4a1.5 1.5 0 0 0-1.5-1.5h-1" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
      <rect x="5.5" y="1.25" width="5" height="2.5" rx="0.75" stroke="currentColor" strokeWidth="1.2" />
      <path d="M5.75 7.25h4.5M5.75 9.75h4.5M5.75 12.25h2.75" stroke="currentColor" strokeWidth="1.1" strokeLinecap="round" />
    </Svg>
  )
}

export function IconTool(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M9 2.5a3.5 3.5 0 0 0-3.5 4.6L2.7 9.9a2.4 2.4 0 0 0 3.4 3.4l2.8-2.8A3.5 3.5 0 0 0 13.5 7L11 8l-2-1-1-2 1-2.5Z" />
    </StrokeSvg>
  )
}

export function IconGrep(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <circle cx="6.75" cy="6.75" r="4.25" />
      <path d="m10 10 3.5 3.5M5 6.75h3.5" />
    </StrokeSvg>
  )
}

export function IconGlob(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M8 2H4a1.5 1.5 0 0 0-1.5 1.5v9A1.5 1.5 0 0 0 4 14h7.5M10.75 3.5v7M7.72 5.25l6.06 3.5M7.72 8.75l6.06-3.5" />
    </StrokeSvg>
  )
}

/** 文件编辑：留白的文档轮廓与斜向铅笔，保证 14px 下笔尖仍清晰。 */
export function IconEdit(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M8.5 2.5H3.75A1.25 1.25 0 0 0 2.5 3.75v8.5c0 .69.56 1.25 1.25 1.25H8.5"
        stroke="currentColor"
        strokeWidth="1.25"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path d="M5 5.25h2.5M5 7.75h1" stroke="currentColor" strokeWidth="1.25" strokeLinecap="round" />
      <path
        d="m8 11 .5-2.5 4.75-4.75a.71.71 0 0 1 1 0l1 1a.71.71 0 0 1 0 1L10.5 10.5 8 11Z"
        stroke="currentColor"
        strokeWidth="1.25"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
      <path d="m12 5 2 2" stroke="currentColor" strokeWidth="1.25" strokeLinecap="round" />
    </Svg>
  )
}

/** 清单：已完成的勾选与两项待办。 */
export function IconTodo(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="m2 4 1 1 2-2M7.5 4h6M7.5 8h6M7.5 12h6" />
      <rect x="2.25" y="7.25" width="1.5" height="1.5" rx=".4" />
      <rect x="2.25" y="11.25" width="1.5" height="1.5" rx=".4" />
    </StrokeSvg>
  )
}

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
    <StrokeSvg {...props}>
      <path d="m4.5 4.5 7 7m0-7-7 7" />
    </StrokeSvg>
  )
}

export function IconWindowMinimize(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M3 11.5h10" stroke="currentColor" strokeWidth="1.25" strokeLinecap="round" />
    </Svg>
  )
}

export function IconWindowMaximize(props: IconProps) {
  return (
    <Svg {...props}>
      <rect x="3.4" y="3.4" width="9.2" height="9.2" rx="1" stroke="currentColor" strokeWidth="1.2" />
    </Svg>
  )
}

export function IconWindowRestore(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="M5.4 5.4V3.8c0-.44.36-.8.8-.8h6c.44 0 .8.36.8.8v6c0 .44-.36.8-.8.8h-1.6" stroke="currentColor" strokeWidth="1.15" strokeLinejoin="round" />
      <rect x="3" y="5.4" width="7.6" height="7.6" rx=".9" stroke="currentColor" strokeWidth="1.2" />
    </Svg>
  )
}

export function IconMenu(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M3 4.5h10M3 8h10M3 11.5h7" />
    </StrokeSvg>
  )
}

/** 复制：错位叠页，后页只保留外露边缘，避免重叠描边。 */
export function IconCopy(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <rect x="5.5" y="5.5" width="8" height="8" rx="1.5" />
      <path d="M10.5 3V2.5A1 1 0 0 0 9.5 1.5h-7a1 1 0 0 0-1 1v7a1 1 0 0 0 1 1H3" />
    </StrokeSvg>
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

/** 分叉：连续主线与弧形支路，三个端点表达一条对话分为两路。 */
export function IconBranch(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M4 5v6M4 9h4a4 4 0 0 0 4-4" />
      <circle cx="4" cy="3.5" r="1.5" />
      <circle cx="4" cy="12.5" r="1.5" />
      <circle cx="12" cy="3.5" r="1.5" />
    </StrokeSvg>
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

/** 下拉触发器用:默认朝下,展开时由 CSS 旋转朝上。 */
export function IconChevronDown(props: IconProps) {
  return (
    <Svg {...props}>
      <path d="m4 6.2 4 3.9 4-3.9" stroke="currentColor" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round" />
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
    <StrokeSvg {...props}>
      <path d="M2.75 4.5h10.5M6 4.5V3.25c0-.41.34-.75.75-.75h2.5c.41 0 .75.34.75.75V4.5M4 4.5l.5 8c.04.56.48 1 1.04 1h4.92c.56 0 1-.44 1.04-1l.5-8M6.5 7.25v3.5M9.5 7.25v3.5" />
    </StrokeSvg>
  )
}

/** 树形折叠指示:默认朝右(收起),展开时由 CSS 旋转朝下。 */
export function IconCaretRight(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="m6 4 4 4-4 4" />
    </StrokeSvg>
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
    <StrokeSvg {...props}>
      <path d="m6.5 2-.4 1.6-1.6.9-1.6-.4-1.4 2.4 1.2 1.1v1.8l-1.2 1.1 1.4 2.4 1.6-.4 1.6.9.4 1.6h3l.4-1.6 1.6-.9 1.6.4 1.4-2.4-1.2-1.1V7.6l1.2-1.1-1.4-2.4-1.6.4-1.6-.9L9.5 2h-3Z" /><circle cx="8" cy="8" r="2.1" />
    </StrokeSvg>
  )
}

export function IconFolder(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M2 6V4.25C2 3.56 2.56 3 3.25 3h2.5l1.5 1.5h5.5c.69 0 1.25.56 1.25 1.25V6M2 6h12v5.75c0 .69-.56 1.25-1.25 1.25h-9.5C2.56 13 2 12.44 2 11.75V6Z" />
    </StrokeSvg>
  )
}

/** 工作区文件(文件系统树,与 Git 无关)。 */
export function IconFiles(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M2.2 5.2c0-.77.62-1.4 1.4-1.4h2.55c.3 0 .58.12.78.34l.72.78c.2.22.48.34.78.34h4.95c.77 0 1.4.63 1.4 1.4v5.74c0 .77-.63 1.4-1.4 1.4H3.6c-.78 0-1.4-.63-1.4-1.4V5.2Z"
        stroke="currentColor"
        strokeWidth="1.15"
        strokeLinejoin="round"
      />
      <path
        d="M5.6 7.3h4.8M5.6 9.5h3.2"
        stroke="currentColor"
        strokeWidth="1.1"
        strokeLinecap="round"
        opacity="0.55"
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

/** 代码文件(产物 chip 等处的代码条目)。 */
export function IconCode(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="m5.2 4.7-3.4 3.3 3.4 3.3"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinecap="round"
        strokeLinejoin="round"
        fill="none"
      />
      <path
        d="m10.8 4.7 3.4 3.3-3.4 3.3"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinecap="round"
        strokeLinejoin="round"
        fill="none"
      />
      <path d="M9.1 3.6 6.9 12.4" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
    </Svg>
  )
}

/** 收起侧栏：分栏面板与左向箭头。 */
export function IconPanelClose(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <rect x="2" y="2.5" width="12" height="11" rx="1.75" /><path d="M6 2.5v11m5.5-8-2 2.5 2 2.5" />
    </StrokeSvg>
  )
}

/** 展开侧栏：分栏面板与右向箭头。 */
export function IconPanelOpen(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <rect x="2" y="2.5" width="12" height="11" rx="1.75" /><path d="M6 2.5v11M9 5.5l2 2.5-2 2.5" />
    </StrokeSvg>
  )
}

/** 添加工作区：文件夹与独立加号。 */
export function IconFolderPlus(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M14 8V5.75c0-.69-.56-1.25-1.25-1.25h-5.5L5.75 3h-2.5C2.56 3 2 3.56 2 4.25v7.5c0 .69.56 1.25 1.25 1.25h4M2 6h12M11.5 9v5M9 11.5h5" />
    </StrokeSvg>
  )
}

/** 展开全部工作区(多层折线张开)。 */
export function IconExpandAll(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="m4.5 5.5 3.5-3 3.5 3M4.5 10.5l3.5 3 3.5-3M5 8h6" />
    </StrokeSvg>
  )
}

/** 收起全部工作区(多层折线合拢)。 */
export function IconCollapseAll(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="m4.5 2.5 3.5 3 3.5-3M4.5 13.5l3.5-3 3.5 3M5 8h6" />
    </StrokeSvg>
  )
}

/** agent preset(组装模式):三块咬合成一体 —— 一块居中压在两块的接缝上,
    读作"这个会话由哪些部件装成"。刻意不复用星芒(那是提示词优化,见
    IconSparkles),也不复用齿轮/滑块(设置与模型);三者会同屏出现在新会话页。 */
export function IconAgentPreset(props: IconProps) {
  return (
    <Svg {...props}>
      {/* 底座两块:留出 1.2 的中缝,让上面那块有位置 */}
      <rect x="2.6" y="8.5" width="4.8" height="4.4" rx="1.3" stroke="currentColor" strokeWidth="1.2" />
      <rect x="8.6" y="8.5" width="4.8" height="4.4" rx="1.3" stroke="currentColor" strokeWidth="1.2" />
      {/* 顶上那块:跨在接缝上方,间距 1.0(收紧到一个整体,不散成三颗点) */}
      <rect x="5.6" y="3.1" width="4.8" height="4.4" rx="1.3" stroke="currentColor" strokeWidth="1.2" />
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

/** 前进箭头:浏览器工具栏。 */
export function IconArrowRight(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M3 8h10M9.5 4.5 13 8l-3.5 3.5"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

/** 回到底部:竖直向下的箭头(回底按钮用)。 */
export function IconArrowDown(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M8 3.6v8.8M4.6 9.1 8 12.5l3.4-3.4"
        stroke="currentColor"
        strokeWidth="1.4"
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </Svg>
  )
}

/** 地球:浏览器空态/无安全信息的站点。 */
export function IconGlobe(props: IconProps) {
  return (
    <Svg {...props}>
      <circle cx="8" cy="8" r="5.7" stroke="currentColor" strokeWidth="1.2" />
      <path
        d="M2.3 8h11.4M8 2.3c1.7 1.6 2.6 3.5 2.6 5.7S9.7 12.1 8 13.7C6.3 12.1 5.4 10.2 5.4 8S6.3 3.9 8 2.3Z"
        stroke="currentColor"
        strokeWidth="1.1"
      />
    </Svg>
  )
}

/** 锁:HTTPS 安全标识。 */
export function IconLock(props: IconProps) {
  return (
    <Svg {...props}>
      <rect
        x="3.6"
        y="7.2"
        width="8.8"
        height="5.8"
        rx="1.4"
        stroke="currentColor"
        strokeWidth="1.3"
      />
      <path
        d="M5.7 7.2V5.5a2.3 2.3 0 0 1 4.6 0v1.7"
        stroke="currentColor"
        strokeWidth="1.3"
        strokeLinecap="round"
      />
    </Svg>
  )
}

/** 相机:页面截图。 */
export function IconCamera(props: IconProps) {
  return (
    <Svg {...props}>
      <path
        d="M6 4.2 6.8 3h2.4l.8 1.2h2c.66 0 1.2.54 1.2 1.2v5.4c0 .66-.54 1.2-1.2 1.2H3.9c-.66 0-1.2-.54-1.2-1.2V5.4c0-.66.54-1.2 1.2-1.2H6Z"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinejoin="round"
      />
      <circle cx="8" cy="8" r="2" stroke="currentColor" strokeWidth="1.2" />
    </Svg>
  )
}

/** 准星:元素拾取。 */
export function IconTarget(props: IconProps) {
  return (
    <Svg {...props}>
      <circle cx="8" cy="8" r="4.6" stroke="currentColor" strokeWidth="1.2" />
      <circle cx="8" cy="8" r="1.1" fill="currentColor" />
      <path
        d="M8 1.6v2.2M8 12.2v2.2M1.6 8h2.2M12.2 8h2.2"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinecap="round"
      />
    </Svg>
  )
}

/** 键盘:文本输入条开关。 */
export function IconKeyboard(props: IconProps) {
  return (
    <Svg {...props}>
      <rect
        x="1.8"
        y="4.2"
        width="12.4"
        height="7.6"
        rx="1.4"
        stroke="currentColor"
        strokeWidth="1.2"
      />
      <path
        d="M3.6 6.6h.01M5.8 6.6h.01M8 6.6h.01M10.2 6.6h.01M12.4 6.6h.01M5.4 9.4h5.2"
        stroke="currentColor"
        strokeWidth="1.2"
        strokeLinecap="round"
      />
    </Svg>
  )
}


export function IconAgentSpawn(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <circle cx="6" cy="5" r="2.5" />
      <path d="M1.75 13v-1a4.25 4.25 0 0 1 8.5 0v1M12.5 4v5M10 6.5h5" />
    </StrokeSvg>
  )
}


export function IconAgentFork(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M4 11V5M5.5 11H8a4 4 0 0 0 4-4V5" />
      <circle cx="4" cy="3.5" r="1.5" />
      <circle cx="12" cy="3.5" r="1.5" />
      <circle cx="4" cy="12.5" r="1.5" />
    </StrokeSvg>
  )
}


export function IconAgentList(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <circle cx="5.75" cy="5" r="2.25" />
      <path d="M1.75 13v-1a4 4 0 0 1 8 0v1M10.5 2.75a2.25 2.25 0 0 1 0 4.5M11.5 9a3.25 3.25 0 0 1 2.75 3.25V13" />
    </StrokeSvg>
  )
}


export function IconToolMessage(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M12.5 2.5h-9A1.5 1.5 0 0 0 2 4v6a1.5 1.5 0 0 0 1.5 1.5H5V14l3-2.5h4.5A1.5 1.5 0 0 0 14 10V4a1.5 1.5 0 0 0-1.5-1.5ZM5 7h6M8.5 4.5 11 7 8.5 9.5" />
    </StrokeSvg>
  )
}


export function IconToolInterrupt(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <rect x="2.5" y="2.5" width="11" height="11" rx="3" />
      <path d="M6.25 5.5v5M9.75 5.5v5" />
    </StrokeSvg>
  )
}


export function IconToolWait(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <circle cx="8" cy="8" r="5.5" />
      <path d="M8 4.5V8l2.5 1.5" />
    </StrokeSvg>
  )
}


export function IconJobStart(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <rect x="2" y="3" width="12" height="10" rx="1.75" />
      <path d="m6.5 5.5 4 2.5-4 2.5v-5Z" />
    </StrokeSvg>
  )
}


export function IconJobList(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <rect x="2" y="3" width="12" height="10" rx="1.75" />
      <path d="M7 6.5h4M7 9.5h4M4.5 6.5h.01M4.5 9.5h.01" />
    </StrokeSvg>
  )
}


export function IconJobOutput(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M3 2.5v9A1.5 1.5 0 0 0 4.5 13H14M6 4h7M6 7h5M9 10h4m-2-2 2 2-2 2" />
    </StrokeSvg>
  )
}


export function IconToolSkill(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M8 2 2.5 5v6L8 14l5.5-3V5L8 2Zm0 0v6m-5.5 3L8 8l5.5 3" />
    </StrokeSvg>
  )
}


export function IconToolAsk(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M12.5 2.5h-9A1.5 1.5 0 0 0 2 4v6a1.5 1.5 0 0 0 1.5 1.5H5V14l3-2.5h4.5A1.5 1.5 0 0 0 14 10V4a1.5 1.5 0 0 0-1.5-1.5ZM6.25 5.5a1.75 1.75 0 0 1 3.5 0C9.75 6.75 8 6.75 8 8" />
      <circle cx="8" cy="9.5" r=".65" fill="currentColor" stroke="none" />
    </StrokeSvg>
  )
}


export function IconSearch(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <circle cx="6.75" cy="6.75" r="4.25" /><path d="m10 10 3.5 3.5" />
    </StrokeSvg>
  )
}


export function IconSessionSearch(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M8.5 2.5h-5A1.5 1.5 0 0 0 2 4v6a1.5 1.5 0 0 0 1.5 1.5H5V14l3-2.5" /><circle cx="11" cy="5.5" r="2.5" /><path d="m12.8 7.3 2 2M4.5 6h1.25" />
    </StrokeSvg>
  )
}


export function IconNewSession(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M7.5 2.5h-4A1.5 1.5 0 0 0 2 4v6a1.5 1.5 0 0 0 1.5 1.5H5V14l3-2.5h4.5A1.5 1.5 0 0 0 14 10V8.5M12 2v5M9.5 4.5h5" />
    </StrokeSvg>
  )
}


export function IconWorkspace(props: IconProps) {
  return (
    <StrokeSvg {...props}>
      <path d="M2 6V4.25C2 3.56 2.56 3 3.25 3h2.5l1.5 1.5h5.5c.69 0 1.25.56 1.25 1.25V6M2 6h12v5.75c0 .69-.56 1.25-1.25 1.25h-9.5C2.56 13 2 12.44 2 11.75V6ZM6 9h4" />
    </StrokeSvg>
  )
}
