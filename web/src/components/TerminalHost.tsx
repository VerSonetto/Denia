/**
 * 终端容器:为每个终端标签挂一个 `TerminalPanel`,并负责进程退出后的收尾。
 *
 * # 为什么容器与单个终端分开
 *
 * `TerminalPanel` 只管"一个 PTY 的一生"。但"进程退出后该不该自动关标签"
 * 是**跨标签**的决策(最后一个终端退出 → 连带收起面板,抄 ZCode 的 `lMt`),
 * 放在容器里才看得见全局。
 *
 * # 保活
 *
 * 所有终端**同时挂载**,不可见的用 CSS 隐藏。切标签因此不会卸载 xterm,
 * scrollback 与 WebSocket 都不丢。
 */

import { useCallback } from 'react'
import { TerminalPanel } from './TerminalPanel'
import type { SidePaneTab } from '../sidePane'

export interface TerminalHostProps {
  /** 当前 scope 下的全部终端标签。 */
  tabs: SidePaneTab[]
  /** 当前激活的标签 id。 */
  activeId: string
  /** 终端 cwd(会话工作区)。 */
  cwd: string
  /** 面板整体是否可见(折叠时为 false)。 */
  paneVisible: boolean
  /** shell 解析出来后回填标签标题。 */
  onTitle(tabId: string, title: string): void
  /** 进程退出。 */
  onExit(tabId: string, exitCode: number): void
  /** 新建一个终端(退出条上的按钮用)。 */
  onNewTerminal(): void
}

export function TerminalHost({
  tabs,
  activeId,
  cwd,
  paneVisible,
  onTitle,
  onExit,
  onNewTerminal,
}: TerminalHostProps) {
  const handleTitle = useCallback(
    (tabId: string, title: string) => onTitle(tabId, title),
    [onTitle],
  )

  if (tabs.length === 0) return null

  return (
    <>
      {tabs.map((tab) => (
        <div
          key={tab.id}
          className="terminal-slot"
          data-visible={tab.id === activeId ? 'true' : undefined}
        >
          <TerminalPanel
            cwd={tab.cwd ?? cwd}
            // 只有"面板可见 且 是本标签"时才需要同步尺寸;
            // 隐藏时尺寸为 0,同步会把 PTY 挤成 1 列。
            visible={paneVisible && tab.id === activeId}
            onTitle={(title) => handleTitle(tab.id, title)}
            onExit={(exitCode) => onExit(tab.id, exitCode)}
            onRestart={onNewTerminal}
          />
        </div>
      ))}
    </>
  )
}

export default TerminalHost
