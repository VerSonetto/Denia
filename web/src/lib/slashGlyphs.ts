/**
 * slash 内置命令的专属图标(单一事实源)。
 *
 * 候选菜单(React 组件)和输入框卡片(纯 DOM 字符串)都从这里取,避免两处
 * 各画一份导致漂移——卡片改图标而菜单没改,是最容易漏的那种不一致。
 *
 * 坐标一律 16 网格,绘制约定:stroke = currentColor、圆端头圆角接。
 */

export interface SlashGlyph {
  /** 每条 path 的 `d`。 */
  paths: string[]
  stroke: number
}

export const COMMAND_GLYPHS: Record<string, SlashGlyph> = {
  // 计划:清单 + 勾(计划是一组待办,不是一条 shell 命令)
  plan: {
    stroke: 1.3,
    paths: ['M2.8 4.4h10.4', 'M2.8 7.8h10.4', 'M2.8 11.2h4.4', 'm9.4 11 1.4 1.5 2.9-3.3'],
  },
  // 压缩:上下箭头向内收
  compact: {
    stroke: 1.3,
    paths: ['m4.2 6.2 3.8-3.4 3.8 3.4', 'm4.2 9.8 3.8 3.4 3.8-3.4'],
  },
}

/** 卡片用:把 glyph 拼成完整 SVG 字符串(纯 DOM 层不引 React)。 */
export function glyphSvg(glyph: SlashGlyph, size: number): string {
  const paths = glyph.paths
    .map(
      (d) =>
        `<path d="${d}" stroke="currentColor" stroke-width="${glyph.stroke}" stroke-linecap="round" stroke-linejoin="round"/>`,
    )
    .join('')
  return `<svg viewBox="0 0 16 16" width="${size}" height="${size}" fill="none" aria-hidden="true">${paths}</svg>`
}
