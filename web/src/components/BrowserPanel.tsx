import { useEffect, useRef, useState } from 'react'
import {
  fetchBrowserState,
  browserCommand,
  subscribeBrowserEvents,
} from '../browserApi'
import type { BrowserState, BrowserOutcome, BrowserEventFrame } from '../types'
import { t } from '../i18n'

/**
 * 内嵌浏览器面板:实时画面(screencast 帧)+ tab 栏 + 地址栏。
 * AI 通过 browser 工具与用户操作同一实例;
 * 用户在面板里也能手动导航/点选/点击/滚轮/输入。
 */
export default function BrowserPanel() {
  const [state, setState] = useState<BrowserState | null>(null)
  const [frame, setFrame] = useState<string | null>(null)
  const [urlInput, setUrlInput] = useState('')
  const [notice, setNotice] = useState<string | null>(null)
  const [pickerOn, setPickerOn] = useState(false)
  // 输入条:工具栏切换显示;输入内容回车后粘贴进页面(配合先点击页面输入框)。
  const [typeInputOn, setTypeInputOn] = useState(false)
  const [typeText, setTypeText] = useState('')
  const imgRef = useRef<HTMLImageElement | null>(null)
  const activeTabRef = useRef<string | null>(null)
  // 页面 CSS 视口尺寸:由 screencastFrame 的 viewport 字段更新,坐标换算用。
  const pageViewportRef = useRef<{ width: number; height: number } | null>(null)

  useEffect(() => {
    let disposed = false
    // 面板挂载即重拉一次状态:侧栏是按 AI 触发(browserOpen)自动展开的,
    // BrowserPanel 组件此时才挂载;若浏览器已在跑(前面会话留下的实例),
    // 必须立刻把 tabs/地址栏补齐,否则画面有了、tab 栏却是空的。
    const refreshState = () => {
      void fetchBrowserState()
        .then((next) => {
          if (!disposed) {
            setState(next)
            activeTabRef.current = next.activeTabId ?? null
            const current = next.tabs.find((tab) => tab.tabId === next.activeTabId)
            if (current?.url) setUrlInput((input) => input || current.url)
            // 状态拉到且浏览器在跑 → 确保 screencast 开着(幂等);
            // 覆盖"订阅建好前浏览器刚启动,首帧事件被错过"的窗口。
            if (next.running) {
              void browserCommand({
                method: 'startScreencast',
                tabId: next.activeTabId ?? undefined,
              }).catch(() => {})
            }
          }
        })
        .catch(() => {})
    }
    // 订阅先建立,再开 screencast(否则首帧前的事件会丢)。
    const close = subscribeBrowserEvents(
      (event: BrowserEventFrame) => {
        switch (event.type) {
          case 'frame':
            // 后端单流保证只推当前 screencast 归属 tab 的帧(切 tab 时
            // StartScreencast 自动迁移流),**不要按 tabId 过滤**:面板刚挂载
            // 时 activeTabRef 尚为 null,首帧早于状态刷新,过滤会永久丢帧,
            // 画面停在"启动浏览器"空态。
            setFrame(`data:image/jpeg;base64,${event.data}`)
            if (event.viewport) pageViewportRef.current = event.viewport
            break
          case 'tabs-changed':
            refreshState()
            // AI 刚启动浏览器(面板先开/后开都可能):补开 screencast,幂等。
            void browserCommand({
              method: 'startScreencast',
              tabId: activeTabRef.current ?? undefined,
            }).catch(() => {})
            break
          case 'navigated':
            refreshState()
            if (event.tabId === activeTabRef.current && event.url) setUrlInput(event.url)
            break
          case 'dialog-opened':
            setNotice(`${t('browserDialog')}:${event.message}`)
            break
          case 'dialog-closed':
            setNotice(null)
            break
          case 'exited':
            setFrame(null)
            setState(null)
            refreshState()
            break
          default:
            break
        }
      },
      () => {
        // SSE 断线(断线窗口内 frame/tabs-changed 可能丢):重连后立即补一次
        // 状态对齐 + 重开 screencast,不依赖服务端是否恰好再推一条事件。
        refreshState()
      },
    )
    refreshState()
    return () => {
      disposed = true
      // 用保活副本停 screencast:close() 之后再发的命令仍会到达服务端,
      // 但 disposed 标志不再阻塞本组件的其它清理逻辑。
      void fetchBrowserState()
        .then(() => browserCommand({ method: 'stopScreencast' }))
        .catch(() => {})
        .finally(() => close())
    }
  }, [])

  /** 空态按钮:显式拉起浏览器并开画面。 */
  const startBrowser = async () => {
    await run({ method: 'getState' }) // 探活入口:没跑就拉起实例
    // 拉起后立刻确保 active tab 存在(启动路径会建首个 tab;
    // 若上一实例把 tab 全关了,这里补一个空白 tab)。
    const after = await fetchBrowserState().catch(() => null)
    if (after && after.running && after.tabs.length === 0) {
      await run({ method: 'newTab' })
    }
    await browserCommand({ method: 'startScreencast' }).catch(() => {})
    const next = await fetchBrowserState().catch(() => null)
    if (next) setState(next)
  }
  const run = async (command: Record<string, unknown>): Promise<BrowserOutcome | null> => {
    try {
      const outcome = await browserCommand(command) as BrowserOutcome
      const next = await fetchBrowserState()
      setState(next)
      return outcome
    } catch (error) {
      setNotice(String(error instanceof Error ? error.message : error))
      return null
    }
  }

  /**
   * 把画面上的显示坐标换算成页面 CSS 像素。
   * 显示区顶部对齐页面(CDP startScreencast 输出即为视口画面),
   * 直接按显示尺寸 ↔ 页面视口尺寸等比映射。
   */
  const toPagePoint = (
    event: { clientX: number; clientY: number },
  ): { x: number; y: number } | null => {
    if (!imgRef.current) return null
    const rect = imgRef.current.getBoundingClientRect()
    const vp = pageViewportRef.current
    const pageW = vp?.width || rect.width
    const pageH = vp?.height || rect.height
    const scaleX = pageW / rect.width
    const scaleY = pageH / rect.height
    return {
      x: (event.clientX - rect.left) * scaleX,
      y: (event.clientY - rect.top) * scaleY,
    }
  }

  /** 元素拾取:在画面上点一下 → elementInfo{x,y} → 弹出元素信息。 */
  const pickElement = async (event: React.MouseEvent<HTMLImageElement>) => {
    if (!pickerOn || !imgRef.current) return
    const rect = imgRef.current.getBoundingClientRect()
    const scaleX = (imgRef.current.naturalWidth || rect.width) / rect.width
    const scaleY = (imgRef.current.naturalHeight || rect.height) / rect.height
    const x = (event.clientX - rect.left) * scaleX
    const y = (event.clientY - rect.top) * scaleY
    const outcome = await run({ method: 'elementInfo', x, y })
    if (outcome?.value) {
      const info = outcome.value as { ref?: string; tag?: string; text?: string }
      setNotice(`${info.tag ?? ''} ${info.ref ? `[${info.ref}]` : ''} ${(info.text ?? '').slice(0, 60)}`)
    }
    setPickerOn(false)
  }

  /** 画面点击:通过 CDP 点击页面真实元素(拾取模式才走 elementInfo)。 */
  const clickPage = async (event: React.MouseEvent<HTMLImageElement>) => {
    if (pickerOn) {
      await pickElement(event)
      return
    }
    if (!activeTabRef.current) return
    const point = toPagePoint(event)
    if (!point) return
    setNotice(null)
    await run({
      method: 'click',
      x: round1(point.x),
      y: round1(point.y),
      tabId: activeTabRef.current,
    })
  }

  /** 画面滚轮:滚动页面。 */
  const wheelPage = async (event: React.WheelEvent<HTMLImageElement>) => {
    if (!activeTabRef.current || pickerOn) return
    const dx = event.deltaX || 0
    const dy = event.deltaY || 0
    // 无滚动量(trackpad 惯性结束等)不发请求。
    if (dx === 0 && dy === 0) return
    const point = toPagePoint(event)
    if (!point) return
    setNotice(null)
    await run({
      method: 'scroll',
      x: round1(point.x),
      y: round1(point.y),
      deltaX: round1(dx),
      deltaY: round1(dy),
      tabId: activeTabRef.current,
    })
    event.preventDefault()
  }

  /** 输入条:把文本粘贴进页面(先点击页面里的输入框聚焦)。 */
  const commitType = async () => {
    if (!typeText || !activeTabRef.current) return
    await run({ method: 'type', text: typeText, tabId: activeTabRef.current })
    setTypeText('')
    setTypeInputOn(false)
  }

  const navigate = async () => {
    const target = urlInput.trim()
    if (!target) return
    const normalized = /^https?:\/\//.test(target) ? target : `https://${target}`
    setNotice(t('browserLoading'))
    await run({ method: 'navigate', url: normalized })
    setNotice(null)
  }

  const active = state?.tabs.find((tab) => tab.tabId === state?.activeTabId)

  /** 点 tab 切换:activate 后为新活跃 tab 重开 screencast(帧流按 tab 走)。 */
  const switchTab = async (tabId: string) => {
    setNotice(null)
    activeTabRef.current = tabId
    await run({ method: 'activate', tabId })
    await browserCommand({ method: 'startScreencast', tabId }).catch(() => {})
  }

  /** 关闭 tab(tab 栏 hover 删除按钮)。 */
  const closeTab = async (tabId: string) => {
    setNotice(null)
    await run({ method: 'close', tabId })
    // 关闭后活跃 tab 由服务端迁移;重新对齐 screencast 与画面。
    const next = await fetchBrowserState().catch(() => null)
    if (next?.running && next.activeTabId) {
      activeTabRef.current = next.activeTabId
      await browserCommand({ method: 'startScreencast', tabId: next.activeTabId }).catch(() => {})
    }
  }

  return (
    <div className="browser-panel">
      <div className="browser-toolbar">
        <div className="browser-tabs">
          {state?.tabs.map((tab) => (
            <div
              key={tab.tabId}
              className={`browser-tab${tab.tabId === state.activeTabId ? ' active' : ''}`}
              title={tab.title || tab.url}
              onClick={() => void switchTab(tab.tabId)}
            >
              <span className="browser-tab-label">
                {tab.title || tab.url || t('browserNewTab')}
              </span>
              <button
                type="button"
                className="browser-tab-close"
                title={t('browserCloseTab')}
                aria-label={t('browserCloseTab')}
                onClick={(event) => {
                  event.stopPropagation()
                  void closeTab(tab.tabId)
                }}
              >
                ×
              </button>
            </div>
          ))}
          <button
            type="button"
            className="browser-tab new"
            title={t('browserNewTab')}
            onClick={() => void run({ method: 'newTab' })}
          >
            +
          </button>
        </div>
        <div className="browser-address">
          <input
            value={urlInput}
            placeholder={active?.url || t('browserAddressHint')}
            onChange={(event) => setUrlInput(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter') void navigate()
            }}
          />
          <button type="button" className="icon-btn" title={t('browserReload')} onClick={() => void run({ method: 'reload' })}>
            ⟳
          </button>
          <button
            type="button"
            className={`icon-btn${pickerOn ? ' active' : ''}`}
            title={t('browserPickElement')}
            onClick={() => setPickerOn((on) => !on)}
          >
            ⌖
          </button>
          <button
            type="button"
            className={`icon-btn${typeInputOn ? ' active' : ''}`}
            title={t('browserTypeInput')}
            onClick={() => setTypeInputOn((on) => !on)}
          >
            ⌨
          </button>
        </div>
        {typeInputOn && (
          <div className="browser-typebar">
            <input
              autoFocus
              value={typeText}
              placeholder={t('browserTypeHint')}
              onChange={(event) => setTypeText(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter') void commitType()
                if (event.key === 'Escape') setTypeInputOn(false)
              }}
            />
            <button type="button" className="icon-btn" title={t('browserTypeSend')} onClick={() => void commitType()}>
              ↵
            </button>
          </div>
        )}
      </div>
      <div className="browser-frame">
        {frame ? (
          <img
            ref={imgRef}
            src={frame}
            alt="browser"
            className={pickerOn ? 'picking' : 'interactive'}
            onClick={(event) => void clickPage(event)}
            onWheel={(event) => void wheelPage(event)}
          />
        ) : state?.running ? (
          // 浏览器在跑但画面还没到(开流后首帧在路上/偶发丢帧):
          // 给出连接中状态与手动重连,而不是误导性的"启动浏览器"按钮。
          <div className="browser-empty">
            <span className="empty-hint">{t('browserFrameConnecting')}</span>
            <button
              type="button"
              onClick={() =>
                void browserCommand({
                  method: 'startScreencast',
                  tabId: activeTabRef.current ?? undefined,
                }).catch(() => {})
              }
            >
              {t('browserFrameRetry')}
            </button>
          </div>
        ) : (
          <div className="browser-empty">
            <button type="button" onClick={() => void startBrowser()}>
              {t('browserStart')}
            </button>
          </div>
        )}
        {notice && <div className="browser-notice">{notice}</div>}
      </div>
    </div>
  )
}

function round1(value: number): number {
  return Math.round(value * 10) / 10
}