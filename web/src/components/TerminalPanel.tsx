/**
 * 终端面板:一个 xterm 实例 + 与后端 PTY 的 WebSocket 连接。
 *
 * # 关键设计(逐条对齐 ZCode 的实测结论)
 *
 * 1. **创建前先 fit**:xterm 挂进 DOM 后立刻 `fit()` 拿到真实 `cols/rows`,
 *    再用它调 `createTerminal`。PTY 一开始就是正确宽度 —— 否则 shell 会
 *    先按 80 列输出提示符,纠正宽度后首屏留下折行残影。
 * 2. **实例与面板解耦**:本组件只负责"一个终端",标签条与面板外壳在
 *    `SidePane.tsx`。这样切标签时组件不卸载(见下条)。
 * 3. **不因隐藏而卸载**:面板折叠 / 切标签时,本组件保持在 DOM 里,只用
 *    CSS 隐藏。xterm 的 scrollback 与 WebSocket 连接因此不丢,切回来即所见。
 * 4. **ResizeObserver + rAF 节流**:拖拽面板宽度会连续触发 resize,每帧
 *    都发一次尺寸同步会把 PTY 刷爆。用 rAF 合并成"每帧最多一次"。
 * 5. **终端配色读 CSS 变量**:主题切换后重新取色,不重建实例。
 */

import { useEffect, useRef, useState } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { WebLinksAddon } from '@xterm/addon-web-links'
import '@xterm/xterm/css/xterm.css'
import {
  closeTerminal,
  connectTerminal,
  createTerminal,
  resizeTerminal,
  type TerminalConnection,
  type TerminalInfo,
} from '../terminalApi'
import { t } from '../i18n'
import './TerminalPanel.css'

/** 终端字体:与 ZCode 同一套候选,优先等宽且含 Nerd Font 回退。 */
const TERMINAL_FONT =
  "'SF Mono', 'JetBrains Mono', 'Cascadia Code', Consolas, 'MesloLGS NF', 'Hack Nerd Font', monospace"

/**
 * 从当前主题读终端配色。
 *
 * 读 CSS 变量而不是写死两套颜色:denia 的主题是 `[data-theme]` 驱动的一整套
 * 语义 token,写死颜色在切换主题时会与面板其余部分脱节。
 */
function readTheme(): Record<string, string> {
  const style = getComputedStyle(document.documentElement)
  const pick = (name: string, fallback: string) => {
    const value = style.getPropertyValue(name).trim()
    return value.length > 0 ? value : fallback
  }
  return {
    background: pick('--code-block', '#0d0d0d'),
    foreground: pick('--label-primary', '#f5f5f5'),
    cursor: pick('--label-primary', '#f5f5f5'),
    cursorAccent: pick('--code-block', '#0d0d0d'),
    selectionBackground: pick('--active-fill', 'rgba(255,255,255,0.16)'),
    black: '#1f2937',
    red: '#ef4444',
    green: '#22c55e',
    yellow: '#eab308',
    blue: '#3b82f6',
    magenta: '#a855f7',
    cyan: '#06b6d4',
    white: '#e5e7eb',
    brightBlack: '#6b7280',
    brightRed: '#f87171',
    brightGreen: '#4ade80',
    brightYellow: '#facc15',
    brightBlue: '#60a5fa',
    brightMagenta: '#c084fc',
    brightCyan: '#22d3ee',
    brightWhite: '#f9fafb',
  }
}

/** 尺寸合法区间:与后端 `MIN_COLS`/`MAX_COLS` 保持一致。 */
const clampCols = (value: number) => Math.max(2, Math.min(1000, Math.round(value)))
const clampRows = (value: number) => Math.max(1, Math.min(500, Math.round(value)))

export interface TerminalPanelProps {
  /** 工作目录(会话 cwd;空白态用工作区路径)。 */
  cwd: string
  /** 面板当前是否可见:不可见时不同步尺寸(尺寸为 0 会把 PTY 压成 1 列)。 */
  visible: boolean
  /** 标签标题回调(shell 解析出来后回填标签条)。 */
  onTitle?: (title: string) => void
  /** 进程退出回调(前端据此决定是否自动关标签)。 */
  onExit?: (exitCode: number) => void
  /** 终端被服务端回收时回调(超限/关闭)。 */
  onClosed?: (reason: string) => void
  /** 进程已退出时,面板上「新建终端」按钮的动作(由父组件开新标签)。 */
  onRestart?: () => void
}

export function TerminalPanel({
  cwd,
  visible,
  onTitle,
  onExit,
  onClosed,
  onRestart,
}: TerminalPanelProps) {
  const hostRef = useRef<HTMLDivElement | null>(null)
  const termRef = useRef<Terminal | null>(null)
  const fitRef = useRef<FitAddon | null>(null)
  const connectionRef = useRef<TerminalConnection | null>(null)
  const infoRef = useRef<TerminalInfo | null>(null)
  /** 已发出的尺寸:避免重复下发同一个 cols/rows。 */
  const lastSizeRef = useRef<{ cols: number; rows: number } | null>(null)
  const rafRef = useRef<number | null>(null)
  /** 回调进 ref:避免它们变化导致终端重建(重建会丢 scrollback)。 */
  const titleRef = useRef(onTitle)
  const exitRef = useRef(onExit)
  const closedRef = useRef(onClosed)
  const visibleRef = useRef(visible)
  titleRef.current = onTitle
  exitRef.current = onExit
  closedRef.current = onClosed
  visibleRef.current = visible
  const [error, setError] = useState('')
  const [exited, setExited] = useState<number | null>(null)

  /* ---- 建立终端(只做一次) ---- */

  useEffect(() => {
    const host = hostRef.current
    if (!host) return
    let disposed = false
    let resizeObserver: ResizeObserver | null = null
    let themeObserver: MutationObserver | null = null

    const term = new Terminal({
      fontFamily: TERMINAL_FONT,
      fontSize: 13,
      cursorBlink: true,
      // 服务端原样转发 PTY 字节流,换行交给 shell —— 不能让 xterm 再补 CR,
      // 否则每行会多一个回车。
      convertEol: false,
      scrollback: 10_000,
      theme: readTheme(),
      allowProposedApi: true,
    })
    const fit = new FitAddon()
    term.loadAddon(fit)
    // 链接交给宿主打开:面板里点链接不该把 Electron 主窗口导航走。
    term.loadAddon(
      new WebLinksAddon((event, uri) => {
        event.preventDefault()
        window.open(uri, '_blank', 'noopener,noreferrer')
      }),
    )
    term.open(host)
    termRef.current = term
    fitRef.current = fit

    /** 量出真实尺寸并下发;`force` 用于首次(还没记过尺寸)。 */
    const syncSize = () => {
      rafRef.current = null
      if (disposed) return
      const element = hostRef.current
      // 隐藏时元素尺寸为 0:此时 fit 会把 cols/rows 压到最小值,
      // 下发会把 PTY 挤坏。所以不可见时只记不报,等可见了再同步。
      if (!element || element.clientWidth <= 0 || element.clientHeight <= 0) return
      try {
        fit.fit()
      } catch {
        return
      }
      const cols = clampCols(term.cols)
      const rows = clampRows(term.rows)
      const last = lastSizeRef.current
      if (last && last.cols === cols && last.rows === rows) return
      lastSizeRef.current = { cols, rows }
      const connection = connectionRef.current
      if (connection) connection.resize(cols, rows)
      else if (infoRef.current) void resizeTerminal(infoRef.current.id, cols, rows).catch(() => {})
    }

    const scheduleSize = () => {
      if (rafRef.current !== null) return
      rafRef.current = requestAnimationFrame(syncSize)
    }

    // 首次 fit:必须在 create 之前,PTY 才能一开始就是正确宽度。
    syncSize()

    const size = lastSizeRef.current ?? { cols: 80, rows: 24 }
    createTerminal({ cwd, cols: size.cols, rows: size.rows })
      .then(({ terminal }) => {
        if (disposed) {
          // 组件已卸载:把刚建出来的进程关掉,不留孤儿。
          void closeTerminal(terminal.id).catch(() => {})
          return
        }
        infoRef.current = terminal
        titleRef.current?.(terminal.shell)
        const connection = connectTerminal(terminal.id, {
          onData(bytes) {
            // 直接喂 Uint8Array:xterm 自己解码,避免我们在这里做
            // "半个多字节字符"的拼接(PTY 分块不保证切在字符边界)。
            term.write(bytes)
          },
          onExit(code) {
            setExited(code)
            exitRef.current?.(code)
          },
          onError(message) {
            setError(message)
          },
        })
        connectionRef.current = connection
        // 连上后把当前尺寸同步一次(创建时的尺寸可能已经过期)。
        if (lastSizeRef.current) {
          connection.resize(lastSizeRef.current.cols, lastSizeRef.current.rows)
        }
        term.focus()
      })
      .catch((cause: unknown) => {
        if (disposed) return
        const message = cause instanceof Error ? cause.message : String(cause)
        setError(message)
        term.write(`\r\n\x1b[31m${t('terminalStartFailed')}:${message}\x1b[0m\r\n`)
      })

    term.onData((data) => {
      connectionRef.current?.send(data)
    })

    // 尺寸跟随:面板拖拽/窗口缩放都走这里,rAF 合并成每帧最多一次。
    if (typeof ResizeObserver !== 'undefined') {
      resizeObserver = new ResizeObserver(scheduleSize)
      resizeObserver.observe(host)
    }
    window.addEventListener('resize', scheduleSize)

    // 主题切换:重取配色,不重建实例(重建会丢 scrollback)。
    themeObserver = new MutationObserver(() => {
      term.options.theme = readTheme()
    })
    themeObserver.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['data-theme'],
    })

    return () => {
      disposed = true
      if (rafRef.current !== null) cancelAnimationFrame(rafRef.current)
      rafRef.current = null
      resizeObserver?.disconnect()
      themeObserver?.disconnect()
      window.removeEventListener('resize', scheduleSize)
      connectionRef.current?.dispose()
      connectionRef.current = null
      const info = infoRef.current
      infoRef.current = null
      if (info) void closeTerminal(info.id).catch(() => {})
      term.dispose()
      termRef.current = null
      fitRef.current = null
      lastSizeRef.current = null
    }
    // 只依赖 cwd:换目录才需要新终端;其余依赖都走 ref。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [cwd])

  /* ---- 可见性变化:重新 fit 并同步尺寸 ---- */

  useEffect(() => {
    if (!visible) return
    const host = hostRef.current
    const fit = fitRef.current
    const term = termRef.current
    if (!host || !fit || !term) return
    // 等一帧:面板展开动画期间尺寸还在变,立刻 fit 会量到中间值。
    const handle = requestAnimationFrame(() => {
      if (host.clientWidth <= 0 || host.clientHeight <= 0) return
      try {
        fit.fit()
      } catch {
        return
      }
      const cols = clampCols(term.cols)
      const rows = clampRows(term.rows)
      const last = lastSizeRef.current
      if (last && last.cols === cols && last.rows === rows) return
      lastSizeRef.current = { cols, rows }
      connectionRef.current?.resize(cols, rows)
    })
    return () => cancelAnimationFrame(handle)
  }, [visible])

  /* ---- 进程退出:保留画面,让用户决定新建还是关标签 ---- */

  return (
    <div className="terminal-panel" data-exited={exited !== null ? 'true' : undefined}>
      <div className="terminal-host" ref={hostRef} />
      {error && (
        <div className="terminal-error" role="alert">
          {error}
        </div>
      )}
      {exited !== null && (
        <div className="terminal-exited" role="status">
          <span>
            {t('terminalExited')}
            {exited !== 0 ? ` · ${t('terminalExitCode')} ${exited}` : ''}
          </span>
          {onRestart && (
            <button type="button" className="terminal-exited-action" onClick={onRestart}>
              {t('terminalNew')}
            </button>
          )}
        </div>
      )}
    </div>
  )
}

export default TerminalPanel
