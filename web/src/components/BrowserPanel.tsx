import { useEffect, useRef, useState } from 'react'
import {
  fetchBrowserState,
  browserCommand,
  subscribeBrowserEvents,
} from '../browserApi'
import type { BrowserState, BrowserOutcome, BrowserEventFrame } from '../types'
import { t } from '../i18n'
import {
  IconArrowLeft,
  IconArrowRight,
  IconRefresh,
  IconGlobe,
  IconLock,
  IconAlert,
  IconCamera,
  IconTarget,
  IconKeyboard,
  IconClose,
  IconCopy,
  IconPlus,
  IconPanelClose,
  IconSpinner,
  IconSend,
} from './icons'
import './BrowserPanel.css'

/** elementInfo 命令的 value 结构(见 crates/browser manager.rs)。 */
interface ElementInfoValue {
  tag?: string
  id?: string
  cls?: unknown
  text?: string
  rect?: { x: number; y: number; width: number; height: number }
}

interface PickedElement {
  tag: string
  id: string
  cls: string
  text: string
  rect?: { x: number; y: number; width: number; height: number }
}

type ToastKind = 'error' | 'info'
interface ToastState {
  kind: ToastKind
  text: string
  seq: number
}

/**
 * 内嵌浏览器侧栏:完整浏览器 chrome(tab 条 + 导航栏 + 画面 + 状态栏)。
 * AI 通过 browser 工具与用户操作同一实例;用户在侧栏可导航/点选/滚动/输入、
 * 处理页面弹窗、拾取元素信息、截取画面。
 */
export default function BrowserPanel({
  onClose,
  visible = true,
}: {
  onClose?: () => void
  /**
   * 面板标签是否处于激活态。
   *
   * 切到别的标签时它是 false。用途只有一个:不再消费画面帧 —— 帧是整条
   * 广播通道里最重的负载(每帧一张 JPEG),不可见的标签继续收帧纯属浪费。
   * 这与 ZCode 的 `residency: suspended` 是同一个思路,只是 denia 的浏览器
   * 是单实例,不必真挂起进程,停止消费即可(订阅断开,服务端自然不再推)。
   */
  visible?: boolean
}) {
  const [state, setState] = useState<BrowserState | null>(null)
  const [frame, setFrame] = useState<string | null>(null)
  // 地址栏草稿:跟随活跃 tab 同步,点击后进入编辑态。
  const [urlDraft, setUrlDraft] = useState('')
  const [editingUrl, setEditingUrl] = useState(false)
  // 导航进行中(地址栏导航/刷新/前进后退):画面顶部进度条。
  const [navLoading, setNavLoading] = useState(false)
  const [pickerOn, setPickerOn] = useState(false)
  const [picked, setPicked] = useState<PickedElement | null>(null)
  // 输入条:工具栏切换显示;输入内容回车后打进页面当前聚焦的输入框。
  const [typeOn, setTypeOn] = useState(false)
  const [typeText, setTypeText] = useState('')
  const [dialog, setDialog] = useState<{ message: string } | null>(null)
  const [toast, setToast] = useState<ToastState | null>(null)
  // 状态栏视口尺寸:由 screencastFrame 的 viewport 字段更新(变化才重渲)。
  const [viewport, setViewport] = useState<{ width: number; height: number } | null>(null)

  const imgRef = useRef<HTMLImageElement | null>(null)
  const frameBoxRef = useRef<HTMLDivElement | null>(null)
  const urlInputRef = useRef<HTMLInputElement | null>(null)
  const activeTabRef = useRef<string | null>(null)
  // 页面 CSS 视口尺寸:坐标换算用(与 viewport 状态同源)。
  const pageViewportRef = useRef<{ width: number; height: number } | null>(null)
  const editingRef = useRef(false)
  const navTimerRef = useRef<number | undefined>(undefined)
  const toastTimerRef = useRef<number | undefined>(undefined)
  const toastSeqRef = useRef(0)
  // 滚轮走原生非 passive 监听(React 的 onWheel 在根容器上是 passive,
  // preventDefault 不生效);处理函数每渲染刷新进 ref,避免闭包过期。
  const wheelHandlerRef = useRef<(event: WheelEvent) => void>(() => {})

  /* ---- 提示浮层 ---- */

  const showToast = (kind: ToastKind, text: string) => {
    if (toastTimerRef.current) window.clearTimeout(toastTimerRef.current)
    toastSeqRef.current += 1
    setToast({ kind, text, seq: toastSeqRef.current })
    toastTimerRef.current = window.setTimeout(
      () => setToast(null),
      kind === 'error' ? 8000 : 4000,
    )
  }
  const dismissToast = () => {
    if (toastTimerRef.current) window.clearTimeout(toastTimerRef.current)
    setToast(null)
  }

  /* ---- 导航进度 ---- */

  const beginNav = () => {
    setNavLoading(true)
    if (navTimerRef.current) window.clearTimeout(navTimerRef.current)
    // 兜底:navigated 事件丢失(同页锚点/事件窗口)时进度条不至于挂死。
    navTimerRef.current = window.setTimeout(() => setNavLoading(false), 15000)
  }
  const endNav = () => {
    if (navTimerRef.current) window.clearTimeout(navTimerRef.current)
    navTimerRef.current = undefined
    setNavLoading(false)
  }

  /* ---- 状态订阅(时序纪律:订阅先建立,再开 screencast) ---- */

  // 可见性进 ref:订阅只建立一次,而 visible 会随标签切换变化。
  // 用 ref 而不是把 visible 加进依赖 —— 后者会反复拆建 SSE 订阅,
  // 每次重建都会丢掉重连窗口里的事件。
  const visibleRef = useRef(visible)
  visibleRef.current = visible

  useEffect(() => {
    let disposed = false
    // 面板挂载即重拉一次状态:侧栏是按 AI 触发(visualMode)自动展开的,
    // BrowserPanel 组件此时才挂载;若浏览器已在跑(前面会话留下的实例),
    // 必须立刻把 tabs/地址栏补齐,否则画面有了、tab 栏却是空的。
    const refreshState = () => {
      void fetchBrowserState()
        .then((next) => {
          if (!disposed) {
            setState(next)
            activeTabRef.current = next.activeTabId ?? null
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
            //
            // 但**要按面板可见性过滤**:切到别的标签后画面看不见,继续
            // 每秒重渲几十帧纯属浪费(JPEG 解码 + React setState)。
            // 重新可见时靠下面的 effect 补一帧最新画面。
            if (!visibleRef.current) break
            setFrame(`data:image/jpeg;base64,${event.data}`)
            if (event.viewport) {
              const next = event.viewport
              pageViewportRef.current = next
              setViewport((prev) =>
                prev && prev.width === next.width && prev.height === next.height
                  ? prev
                  : next,
              )
            }
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
            if (event.tabId === activeTabRef.current) {
              if (event.url) setUrlDraft(event.url)
              setPicked(null)
              endNav()
            }
            break
          case 'dialog-opened':
            setDialog({ message: event.message })
            break
          case 'dialog-closed':
            setDialog(null)
            break
          case 'exited':
            setFrame(null)
            setState(null)
            setViewport(null)
            pageViewportRef.current = null
            setDialog(null)
            setPicked(null)
            setPickerOn(false)
            endNav()
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
      endNav()
      if (toastTimerRef.current) window.clearTimeout(toastTimerRef.current)
      // 用保活副本停 screencast:close() 之后再发的命令仍会到达服务端,
      // 但 disposed 标志不再阻塞本组件的其它清理逻辑。
      void fetchBrowserState()
        .then(() => browserCommand({ method: 'stopScreencast' }))
        .catch(() => {})
        .finally(() => close())
    }
  }, [])

  /* ---- 重新可见:补一帧最新画面 ---- */

  // 不可见期间的 frame 事件被丢掉了,切回来时画面停在旧帧。这里主动
  // 重开一次 screencast(幂等),让服务端立刻推当前画面。
  useEffect(() => {
    if (!visible) return
    if (!state?.running) return
    void browserCommand({
      method: 'startScreencast',
      tabId: activeTabRef.current ?? undefined,
    }).catch(() => {})
    // 只在"变为可见"时触发;state.running 变化由订阅路径覆盖。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [visible])

  /* ---- 地址栏跟随活跃 tab(编辑中不打断) ---- */
  const active = state?.tabs.find((tab) => tab.tabId === state?.activeTabId)
  const activeUrl = active?.url ?? ''
  useEffect(() => {
    if (!editingRef.current) setUrlDraft(activeUrl)
  }, [activeUrl])
  useEffect(() => {
    if (editingUrl) urlInputRef.current?.select()
  }, [editingUrl])

  // 拾取模式 Esc 取消。
  useEffect(() => {
    if (!pickerOn) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') setPickerOn(false)
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [pickerOn])

  /* ---- 命令执行 ---- */

  /** 结构性命令:执行后重拉状态(tabs/活跃/URL 变化)。 */
  const run = async (command: Record<string, unknown>): Promise<BrowserOutcome | null> => {
    try {
      const outcome = (await browserCommand(command)) as BrowserOutcome
      const next = await fetchBrowserState()
      setState(next)
      return outcome
    } catch (error) {
      showToast('error', String(error instanceof Error ? error.message : error))
      return null
    }
  }

  /** 画面级命令(点击/滚动/输入):不改变 tabs,跳过状态重拉省一次往返。 */
  const send = async (command: Record<string, unknown>): Promise<BrowserOutcome | null> => {
    try {
      return (await browserCommand(command)) as BrowserOutcome
    } catch (error) {
      showToast('error', String(error instanceof Error ? error.message : error))
      return null
    }
  }

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

  /**
   * 把画面上的显示坐标换算成页面 CSS 像素。
   * 显示区顶部对齐页面(CDP startScreencast 输出即为视口画面),
   * 直接按显示尺寸 ↔ 页面视口尺寸等比映射。
   */
  const toPagePoint = (clientX: number, clientY: number) => {
    const img = imgRef.current
    if (!img) return null
    const rect = img.getBoundingClientRect()
    const vp = pageViewportRef.current
    const pageW = vp?.width || rect.width
    const pageH = vp?.height || rect.height
    return {
      x: (clientX - rect.left) * (pageW / rect.width),
      y: (clientY - rect.top) * (pageH / rect.height),
    }
  }

  /** 元素拾取:在画面上点一下 → elementInfo → 弹出元素信息卡。 */
  const pickElement = async (clientX: number, clientY: number) => {
    const img = imgRef.current
    if (!img) return
    const rect = img.getBoundingClientRect()
    const scaleX = (img.naturalWidth || rect.width) / rect.width
    const scaleY = (img.naturalHeight || rect.height) / rect.height
    const outcome = await run({
      method: 'elementInfo',
      x: (clientX - rect.left) * scaleX,
      y: (clientY - rect.top) * scaleY,
    })
    setPickerOn(false)
    const info = outcome?.value as ElementInfoValue | undefined
    if (!info) return
    setPicked({
      tag: info.tag ?? '?',
      id: info.id ?? '',
      cls: typeof info.cls === 'string' ? info.cls : '',
      text: info.text ?? '',
      rect: info.rect,
    })
  }

  /** 画面点击:通过 CDP 点击页面真实元素(拾取模式才走 elementInfo)。 */
  const clickPage = async (event: React.MouseEvent<HTMLImageElement>) => {
    if (pickerOn) {
      await pickElement(event.clientX, event.clientY)
      return
    }
    if (!activeTabRef.current) return
    const point = toPagePoint(event.clientX, event.clientY)
    if (!point) return
    await send({
      method: 'click',
      x: round1(point.x),
      y: round1(point.y),
      tabId: activeTabRef.current,
    })
  }

  // 滚轮:原生非 passive 监听挂画面容器;浮层内部滚动放行默认行为。
  wheelHandlerRef.current = (event: WheelEvent) => {
    if (!frame) return
    const target = event.target as HTMLElement | null
    if (target?.closest('.brw-overlays')) return
    event.preventDefault()
    const dx = event.deltaX || 0
    const dy = event.deltaY || 0
    if (!activeTabRef.current || pickerOn || (dx === 0 && dy === 0)) return
    const point = toPagePoint(event.clientX, event.clientY)
    if (!point) return
    void send({
      method: 'scroll',
      x: round1(point.x),
      y: round1(point.y),
      deltaX: round1(dx),
      deltaY: round1(dy),
      tabId: activeTabRef.current,
    })
  }
  useEffect(() => {
    const box = frameBoxRef.current
    if (!box) return
    const listener = (event: WheelEvent) => wheelHandlerRef.current(event)
    box.addEventListener('wheel', listener, { passive: false })
    return () => box.removeEventListener('wheel', listener)
  }, [])

  /* ---- 导航栏动作 ---- */

  const normalizeUrl = (raw: string): string => {
    const target = raw.trim()
    if (!target) return ''
    if (/^https?:\/\//.test(target)) return target
    // 本机回环地址约定走 http,其余域名补 https。
    if (/^(localhost|127(?:\.\d+){3})(:\d+)?(\/|$)/i.test(target)) {
      return `http://${target}`
    }
    return `https://${target}`
  }

  const navigate = async () => {
    exitEdit()
    const normalized = normalizeUrl(urlDraft)
    if (!normalized) return
    beginNav()
    const outcome = await run({ method: 'navigate', url: normalized })
    if (!outcome || outcome.ok === false) endNav()
  }

  const beginEdit = () => {
    editingRef.current = true
    setUrlDraft(activeUrl)
    setEditingUrl(true)
  }
  const exitEdit = () => {
    editingRef.current = false
    setEditingUrl(false)
  }

  const historyStep = async (method: 'back' | 'forward') => {
    beginNav()
    // 无历史可退时后端返回 ok:false,属正常,不弹错误提示。
    try {
      const outcome = (await browserCommand({
        method,
        tabId: activeTabRef.current ?? undefined,
      })) as BrowserOutcome | undefined
      if (!outcome || outcome.ok === false) endNav()
    } catch {
      endNav()
    }
    await fetchBrowserState()
      .then((next) => setState(next))
      .catch(() => {})
  }

  const reload = async () => {
    beginNav()
    const outcome = await run({ method: 'reload', tabId: activeTabRef.current ?? undefined })
    if (!outcome || outcome.ok === false) endNav()
  }

  /** 截图:screenshot 命令返回 image 载荷 → 触发下载。 */
  const screenshot = async () => {
    const outcome = await run({
      method: 'screenshot',
      tabId: activeTabRef.current ?? undefined,
    })
    const image = outcome?.image
    if (!image?.base64) {
      showToast('error', outcome?.error?.message || t('browserShotFailed'))
      return
    }
    const link = document.createElement('a')
    link.href = `data:${image.mimeType || 'image/png'};base64,${image.base64}`
    link.download = `denia-browser-${new Date().toISOString().replace(/[:.]/g, '-')}.png`
    link.click()
    showToast('info', t('browserShotSaved'))
  }

  /* ---- tab 条动作 ---- */

  /** 点 tab 切换:activate 后为新活跃 tab 重开 screencast(帧流按 tab 走)。 */
  const switchTab = async (tabId: string) => {
    activeTabRef.current = tabId
    setPicked(null)
    await run({ method: 'activate', tabId })
    await browserCommand({ method: 'startScreencast', tabId }).catch(() => {})
  }

  /** 关闭 tab(tab 条 hover 删除按钮 / 中键)。 */
  const closeTab = async (tabId: string) => {
    await run({ method: 'close', tabId })
    // 关闭后活跃 tab 由服务端迁移;重新对齐 screencast 与画面。
    const next = await fetchBrowserState().catch(() => null)
    if (next?.running && next.activeTabId) {
      activeTabRef.current = next.activeTabId
      await browserCommand({ method: 'startScreencast', tabId: next.activeTabId }).catch(() => {})
    }
  }

  /* ---- 输入条 / 弹窗 / 拾取卡 ---- */

  const commitType = async () => {
    if (!typeText || !activeTabRef.current) return
    await send({ method: 'type', text: typeText, tabId: activeTabRef.current })
    setTypeText('')
  }

  const handleDialog = async (accept: boolean) => {
    setDialog(null)
    await send({ method: 'handleDialog', accept, tabId: activeTabRef.current ?? undefined })
  }

  const copyPickInfo = async () => {
    if (!picked) return
    const parts = [picked.tag]
    if (picked.id) parts.push(`#${picked.id}`)
    if (picked.cls) parts.push(`.${picked.cls.split(/\s+/).join('.')}`)
    if (picked.text) parts.push(`"${picked.text.slice(0, 80)}"`)
    try {
      await navigator.clipboard.writeText(parts.join(' '))
      showToast('info', t('browserPickRefCopied'))
    } catch {
      /* 剪贴板不可用:静默 */
    }
  }

  const tabs = state?.tabs ?? []
  const running = state?.running ?? false
  const urlInfo = prettyUrl(activeUrl)

  return (
    <div className="brw">
      {/* ---- tab 条 ---- */}
      <div className="brw-tabstrip">
        <div className="brw-tabs">
          {tabs.map((tab) => (
            <div
              key={tab.tabId}
              className={`brw-tab${tab.tabId === state?.activeTabId ? ' active' : ''}`}
              title={tab.title || tab.url || t('browserNewTab')}
              onClick={() => void switchTab(tab.tabId)}
              onAuxClick={(event) => {
                if (event.button === 1) void closeTab(tab.tabId)
              }}
            >
              <span className="brw-tab-label">{tab.title || tab.url || t('browserNewTab')}</span>
              <button
                type="button"
                className="brw-tab-close"
                title={t('browserCloseTab')}
                aria-label={t('browserCloseTab')}
                onClick={(event) => {
                  event.stopPropagation()
                  void closeTab(tab.tabId)
                }}
              >
                <IconClose size={9} />
              </button>
            </div>
          ))}
          <button
            type="button"
            className="brw-newtab"
            title={t('browserNewTab')}
            aria-label={t('browserNewTab')}
            onClick={() => void run({ method: 'newTab' })}
          >
            <IconPlus size={13} />
          </button>
        </div>
        {onClose && (
          <button
            type="button"
            className="brw-panel-close"
            title={t('sidebarCollapse')}
            aria-label={t('sidebarCollapse')}
            onClick={onClose}
          >
            <IconPanelClose size={15} />
          </button>
        )}
      </div>

      {/* ---- 导航栏 ---- */}
      <div className="brw-nav">
        <div className="brw-navgroup">
          <button
            type="button"
            className="brw-navbtn"
            title={t('browserBack')}
            aria-label={t('browserBack')}
            disabled={!running}
            onClick={() => void historyStep('back')}
          >
            <IconArrowLeft size={15} />
          </button>
          <button
            type="button"
            className="brw-navbtn"
            title={t('browserForward')}
            aria-label={t('browserForward')}
            disabled={!running}
            onClick={() => void historyStep('forward')}
          >
            <IconArrowRight size={15} />
          </button>
          <button
            type="button"
            className="brw-navbtn"
            title={t('browserReload')}
            aria-label={t('browserReload')}
            disabled={!running}
            onClick={() => void reload()}
          >
            <IconRefresh size={14} />
          </button>
        </div>
        <div className="brw-urlbox">
          <span className="brw-url-icon" data-kind={urlInfo.icon}>
            {urlInfo.icon === 'lock' ? (
              <IconLock size={11} />
            ) : urlInfo.icon === 'alert' ? (
              <IconAlert size={11} />
            ) : (
              <IconGlobe size={11} />
            )}
          </span>
          {editingUrl ? (
            <input
              ref={urlInputRef}
              className="brw-url-input"
              value={urlDraft}
              placeholder={t('browserAddressHint')}
              spellCheck={false}
              onChange={(event) => setUrlDraft(event.target.value)}
              onBlur={exitEdit}
              onKeyDown={(event) => {
                if (event.key === 'Enter') void navigate()
                if (event.key === 'Escape') exitEdit()
              }}
            />
          ) : activeUrl ? (
            <button type="button" className="brw-url-display" onClick={beginEdit}>
              <span className="brw-url-host">{urlInfo.host}</span>
              {urlInfo.rest && <span className="brw-url-rest">{urlInfo.rest}</span>}
            </button>
          ) : (
            <span className="brw-url-placeholder">{t('browserAddressHint')}</span>
          )}
        </div>
        <div className="brw-navgroup">
          <button
            type="button"
            className={`brw-navbtn${pickerOn ? ' active' : ''}`}
            title={t('browserPickElement')}
            aria-label={t('browserPickElement')}
            disabled={!frame}
            onClick={() => {
              setPicked(null)
              setPickerOn((on) => !on)
            }}
          >
            <IconTarget size={14} />
          </button>
          <button
            type="button"
            className={`brw-navbtn${typeOn ? ' active' : ''}`}
            title={t('browserTypeInput')}
            aria-label={t('browserTypeInput')}
            disabled={!running}
            onClick={() => setTypeOn((on) => !on)}
          >
            <IconKeyboard size={14} />
          </button>
          <button
            type="button"
            className="brw-navbtn"
            title={t('browserScreenshot')}
            aria-label={t('browserScreenshot')}
            disabled={!frame}
            onClick={() => void screenshot()}
          >
            <IconCamera size={14} />
          </button>
        </div>
      </div>

      {/* ---- 输入条 ---- */}
      {typeOn && (
        <div className="brw-typebar">
          <input
            autoFocus
            value={typeText}
            placeholder={t('browserTypeHint')}
            onChange={(event) => setTypeText(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter') void commitType()
              if (event.key === 'Escape') setTypeOn(false)
            }}
          />
          <button
            type="button"
            className="brw-typebar-send"
            title={t('browserTypeSend')}
            aria-label={t('browserTypeSend')}
            disabled={!typeText || !running}
            onClick={() => void commitType()}
          >
            <IconSend size={13} />
          </button>
        </div>
      )}

      {/* ---- 画面区 ---- */}
      <div className="brw-frame" ref={frameBoxRef}>
        {frame ? (
          <img
            ref={imgRef}
            src={frame}
            alt="browser"
            draggable={false}
            className={pickerOn ? 'picking' : 'interactive'}
            onClick={(event) => void clickPage(event)}
          />
        ) : running ? (
          // 浏览器在跑但画面还没到(开流后首帧在路上/偶发丢帧):
          // 给出连接中状态与手动重连,而不是误导性的"启动浏览器"按钮。
          <div className="brw-connecting">
            <IconSpinner size={18} />
            <span>{t('browserFrameConnecting')}</span>
            <button
              type="button"
              className="brw-ghostbtn"
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
          <div className="brw-empty">
            <div className="brw-empty-glyph">
              <IconGlobe size={26} />
            </div>
            <div className="brw-empty-title">{t('browserEmptyTitle')}</div>
            <div className="brw-empty-hint">{t('browserEmptyHint')}</div>
            <button type="button" className="brw-startbtn" onClick={() => void startBrowser()}>
              <IconGlobe size={14} />
              {t('browserStart')}
            </button>
          </div>
        )}

        {navLoading && <div className="brw-progress" />}

        {pickerOn && (
          <div className="brw-banner">
            <IconTarget size={12} />
            <span>{t('browserPickActive')}</span>
            <button type="button" onClick={() => setPickerOn(false)}>
              {t('browserPickCancel')}
            </button>
          </div>
        )}

        {(dialog || picked || toast) && (
          <div className="brw-overlays">
            {dialog && (
              <div className="brw-dialog">
                <div className="brw-dialog-head">
                  <IconAlert size={13} />
                  <span>{t('browserDialog')}</span>
                </div>
                {dialog.message && <div className="brw-dialog-msg">{dialog.message}</div>}
                <div className="brw-dialog-hint">{t('browserDialogHint')}</div>
                <div className="brw-dialog-actions">
                  <button type="button" className="brw-ghostbtn" onClick={() => void handleDialog(false)}>
                    {t('browserDialogDismiss')}
                  </button>
                  <button type="button" className="brw-primarybtn" onClick={() => void handleDialog(true)}>
                    {t('browserDialogAccept')}
                  </button>
                </div>
              </div>
            )}
            {picked && (
              <div className="brw-pickcard">
                <div className="brw-pickcard-head">
                  <span className="brw-chip tag">{picked.tag.toLowerCase()}</span>
                  {picked.id && <span className="brw-chip">#{picked.id}</span>}
                  {picked.cls && <span className="brw-chip">.{picked.cls.split(/\s+/)[0]}</span>}
                  <span className="brw-pickcard-spacer" />
                  <button
                    type="button"
                    className="brw-mini"
                    title={t('browserPickCopy')}
                    aria-label={t('browserPickCopy')}
                    onClick={() => void copyPickInfo()}
                  >
                    <IconCopy size={12} />
                  </button>
                  <button
                    type="button"
                    className="brw-mini"
                    title={t('close')}
                    aria-label={t('close')}
                    onClick={() => setPicked(null)}
                  >
                    <IconClose size={11} />
                  </button>
                </div>
                {picked.text && <div className="brw-pickcard-text">{picked.text}</div>}
                {picked.rect && (
                  <div className="brw-pickcard-rect">
                    {Math.round(picked.rect.width)} × {Math.round(picked.rect.height)} @{' '}
                    {Math.round(picked.rect.x)}, {Math.round(picked.rect.y)}
                  </div>
                )}
              </div>
            )}
            {toast && (
              <div className="brw-toast" data-kind={toast.kind}>
                {toast.kind === 'error' && <IconAlert size={13} />}
                <span>{toast.text}</span>
                <button type="button" className="brw-mini" aria-label={t('close')} title={t('close')} onClick={dismissToast}>
                  <IconClose size={11} />
                </button>
              </div>
            )}
          </div>
        )}
      </div>

      {/* ---- 状态栏 ---- */}
      <div className="brw-status">
        {running ? (
          <>
            <span className="brw-status-host">
              {activeUrl ? urlInfo.host || activeUrl : t('browserNewTab')}
            </span>
            <span className="brw-status-sep">·</span>
            <span>{t('browserTabCount', { n: tabs.length })}</span>
          </>
        ) : (
          <span className="brw-status-host">—</span>
        )}
        <span className="brw-status-spacer" />
        {viewport && (
          <>
            <span>
              {t('browserViewportLabel')} {viewport.width}×{viewport.height}
            </span>
            <span className="brw-status-sep">·</span>
          </>
        )}
        <span
          className="brw-status-live"
          data-on={frame ? 'true' : 'false'}
          title={frame ? t('browserLive') : t('browserLiveIdle')}
        />
      </div>
    </div>
  )
}

/** URL 拆解:地址栏展示用。host 为空(about: 等)时整体归 host。 */
function prettyUrl(url: string): { icon: 'lock' | 'alert' | 'globe'; host: string; rest: string } {
  try {
    const parsed = new URL(url)
    if (!parsed.host) {
      return { icon: 'globe', host: url, rest: '' }
    }
    const insecureHttp =
      parsed.protocol === 'http:' &&
      !/^(localhost|127(?:\.\d+){3}|\[::1\])$/.test(parsed.hostname)
    return {
      icon: parsed.protocol === 'https:' ? 'lock' : insecureHttp ? 'alert' : 'globe',
      host: parsed.host,
      rest: `${parsed.pathname}${parsed.search}${parsed.hash}`,
    }
  } catch {
    return { icon: 'globe', host: url, rest: '' }
  }
}

function round1(value: number): number {
  return Math.round(value * 10) / 10
}
