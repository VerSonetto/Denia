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
 * 交互模型与 ZCode 一致:AI 通过 browser 工具操作同一实例;
 * 用户在面板里也能手动导航/点选元素(elementInfo)。
 */
export default function BrowserPanel() {
  const [state, setState] = useState<BrowserState | null>(null)
  const [frame, setFrame] = useState<string | null>(null)
  const [urlInput, setUrlInput] = useState('')
  const [notice, setNotice] = useState<string | null>(null)
  const [pickerOn, setPickerOn] = useState(false)
  const imgRef = useRef<HTMLImageElement | null>(null)
  const activeTabRef = useRef<string | null>(null)

  useEffect(() => {
    let disposed = false
    const refreshState = () => {
      void fetchBrowserState()
        .then((next) => {
          if (!disposed) {
            setState(next)
            activeTabRef.current = next.activeTabId ?? null
            const current = next.tabs.find((tab) => tab.tabId === next.activeTabId)
            if (current?.url) setUrlInput((input) => input || current.url)
          }
        })
        .catch(() => {})
    }
    // 订阅先建立,再开 screencast(否则首帧前的事件会丢)。
    const close = subscribeBrowserEvents((event: BrowserEventFrame) => {
      switch (event.type) {
        case 'frame':
          // 单实例浏览器:活跃 tab 的帧直接显示,不按 tabId 过滤
          // (首帧可能早于状态刷新,过滤会永久丢帧)。
          setFrame(`data:image/jpeg;base64,${event.data}`)
          break
        case 'tabs-changed':
          refreshState()
          // AI 刚启动浏览器(面板先开/后开都可能):补开 screencast,幂等。
          void browserCommand({ method: 'startScreencast' }).catch(() => {})
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
          refreshState()
          break
        default:
          break
      }
    })
    // 先拉状态:仅当浏览器已在跑才开 screencast,不因打开面板而自发拉起进程。
    void (async () => {
      try {
        const next = await fetchBrowserState()
        if (disposed) return
        setState(next)
        activeTabRef.current = next.activeTabId ?? null
        const current = next.tabs.find((tab) => tab.tabId === next.activeTabId)
        if (current?.url) setUrlInput((input) => input || current.url)
        if (next.running) await browserCommand({ method: 'startScreencast' })
      } catch {
        /* 服务不可达:保持空态 */
      }
    })()
    return () => {
      disposed = true
      void browserCommand({ method: 'stopScreencast' }).catch(() => {})
      close()
    }
  }, [])

  /** 空态按钮:显式拉起浏览器并开画面。 */
  const startBrowser = async () => {
    await run({ method: 'getState' }) // 首条命令会拉起实例
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

  const navigate = async () => {
    const target = urlInput.trim()
    if (!target) return
    const normalized = /^https?:\/\//.test(target) ? target : `https://${target}`
    setNotice(t('browserLoading'))
    await run({ method: 'navigate', url: normalized })
    setNotice(null)
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

  const active = state?.tabs.find((tab) => tab.tabId === state?.activeTabId)

  return (
    <div className="browser-panel">
      <div className="browser-toolbar">
        <div className="browser-tabs">
          {state?.tabs.map((tab) => (
            <button
              key={tab.tabId}
              type="button"
              className={`browser-tab${tab.tabId === state.activeTabId ? ' active' : ''}`}
              title={tab.title || tab.url}
              onClick={() => void run({ method: 'activate', tabId: tab.tabId })}
            >
              {tab.title || tab.url || t('browserNewTab')}
            </button>
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
        </div>
      </div>
      <div className="browser-frame">
        {frame ? (
          <img
            ref={imgRef}
            src={frame}
            alt="browser"
            className={pickerOn ? 'picking' : ''}
            onClick={(event) => void pickElement(event)}
          />
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
