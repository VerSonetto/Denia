import { useEffect, useState } from 'react'
import { isTauri } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { t } from '../i18n'
import {
  IconClose,
  IconWindowMaximize,
  IconWindowMinimize,
  IconWindowRestore,
} from './icons'

/**
 * Tauri 桌面壳的窗口条。
 *
 * ## 为什么它不是"通栏标题栏"
 *
 * 最初这一条是通栏的(左侧品牌 + 右侧窗口按钮),结果是**两个 Denia**:
 * 侧栏左上角已经有一个品牌,标题栏再来一个,重复且割裂。
 *
 * 现在把它整条移进**主列内部**(见 `.main > .desktop-titlebar`),于是:
 *
 * - 侧栏从窗口最顶端一直贯到底,成为一条完整的竖带,品牌只有一处(侧栏顶部);
 * - 窗口按钮落在主列上沿,与主列左边界对齐,不跨到侧栏上方去;
 * - 窗口拖动区只覆盖主列顶部,侧栏顶部的品牌区不参与拖动 —— 那里要能点。
 *
 * 浏览器形态(含远程手机端)不渲染它,避免把桌面窗口语义带过去。
 */
export function DesktopTitleBar() {
  const [desktop] = useState(() => isTauri())
  const [maximized, setMaximized] = useState(false)

  useEffect(() => {
    if (!desktop) return

    const window = getCurrentWindow()
    let disposed = false
    let unlisten: (() => void) | undefined

    const syncMaximized = () => {
      void window
        .isMaximized()
        .then((value) => {
          if (!disposed) setMaximized(value)
        })
        .catch(() => {})
    }

    syncMaximized()
    void window
      .onResized(syncMaximized)
      .then((stop) => {
        if (disposed) stop()
        else unlisten = stop
      })
      .catch(() => {})

    return () => {
      disposed = true
      unlisten?.()
    }
  }, [desktop])

  if (!desktop) return null

  const window = getCurrentWindow()
  const toggleMaximize = () => {
    void window.toggleMaximize().catch(() => {})
  }

  return (
    <header className="desktop-titlebar" aria-label={t('titleBarLabel')}>
      {/* 拖动区:主列顶部整条。`data-tauri-drag-region="deep"` 而不是裸属性 ——
          Tauri 2.11 的 drag.js 对裸属性只认「直接点在自己身上」
          (`el === composedPath[0]`),点在子元素上不拖。`deep` 让整棵子树可拖。

          双击最大化**不在这里挂 onDoubleClick**:drag.js 自己会以
          `internal_toggle_maximize` 响应 detail===2 的 mousedown,两边都挂就会
          双击一次切换两次、净效果为零(看起来就是双击没反应)。 */}
      <div className="desktop-titlebar-drag" data-tauri-drag-region="deep" />
      <div className="desktop-titlebar-actions">
        <button
          type="button"
          className="desktop-window-btn"
          aria-label={t('titleBarMinimize')}
          title={t('titleBarMinimize')}
          onClick={() => void window.minimize().catch(() => {})}
        >
          <IconWindowMinimize size={15} />
        </button>
        <button
          type="button"
          className="desktop-window-btn"
          aria-label={maximized ? t('titleBarRestore') : t('titleBarMaximize')}
          title={maximized ? t('titleBarRestore') : t('titleBarMaximize')}
          onClick={toggleMaximize}
        >
          {maximized ? <IconWindowRestore size={15} /> : <IconWindowMaximize size={15} />}
        </button>
        <button
          type="button"
          className="desktop-window-btn close"
          aria-label={t('titleBarClose')}
          title={t('titleBarClose')}
          onClick={() => void window.close().catch(() => {})}
        >
          <IconClose size={16} />
        </button>
      </div>
    </header>
  )
}
