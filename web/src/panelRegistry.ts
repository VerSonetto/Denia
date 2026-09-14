/**
 * 面板注册表:一处定义「有哪些面板」,标签栏的 `+` 菜单与空态引导页共用。
 *
 * 抄 ZCode 的 `nt` / `Pjt()` 结构:菜单项**按可用性动态生成**,而不是写死
 * 一个列表再逐个判断显示。差别在于 ZCode 的判定逻辑散在组件里(`Ke`/`Ge`/`G`
 * 几个布尔),这里收成 `available(ctx)` 一个纯函数,好测也好读。
 *
 * 关键约束(与 ZCode 一致):
 * - **审查是单例**:已经开着就不在菜单里出现(避免点两次得到两个一样的面板);
 * - **终端与浏览器可多开**:菜单项永远在(只要能力支持)。
 */

import type { SidePaneTabType } from './sidePane'

export interface PanelContext {
  /** 当前会话绑定的工作区路径;没有则为 null(空白态)。 */
  workspacePath: string | null
  /** 该面板类型是否已经开着(单例判定用)。 */
  isOpen(type: SidePaneTabType): boolean
  /** 内嵌浏览器是否可用(远程/无 Playwright 时不可用)。 */
  supportsBrowser: boolean
}

export interface PanelDescriptor {
  type: SidePaneTabType
  /** i18n 键;标签栏与菜单共用同一份文案。 */
  labelKey: 'sidePaneReview' | 'sidePaneTerminal' | 'sidePaneBrowser'
  /** 该面板此刻能否打开。 */
  available(ctx: PanelContext): boolean
  /** 单例面板已开时是否从菜单隐藏。 */
  singleton: boolean
}

export const PANELS: PanelDescriptor[] = [
  {
    type: 'review',
    labelKey: 'sidePaneReview',
    // 单例:已经开着就不给入口(ZCode 的 `Ge ? null : <Item/>`)。
    available: (ctx) => !ctx.isOpen('review'),
    singleton: true,
  },
  {
    type: 'terminal',
    labelKey: 'sidePaneTerminal',
    // 终端不依赖工作区:没有工作区时落到 home(后端兜底),
    // 所以永远可用。这是有意的 —— 用户可能只想开个终端看看环境。
    available: () => true,
    singleton: false,
  },
  {
    type: 'browser',
    labelKey: 'sidePaneBrowser',
    available: (ctx) => ctx.supportsBrowser,
    singleton: false,
  },
]

/** 当前可打开的菜单项(顺序即菜单顺序)。 */
export function availablePanels(ctx: PanelContext): PanelDescriptor[] {
  return PANELS.filter((panel) => panel.available(ctx))
}

/**
 * 空态引导页的按钮列表。
 *
 * 与 `+` 菜单**共用同一份过滤**,只有一处例外:审查已经开着时,空态页
 * 不可能出现(有标签就不是空态),所以这里不需要 `singleton` 的排除逻辑 ——
 * 直接复用同一函数即可,不必写第二套判定。
 */
export function openablePanels(ctx: PanelContext): PanelDescriptor[] {
  return availablePanels(ctx)
}
