/**
 * 设置 → 远程连接:统一的连接管理入口。
 *
 * 形态:选择连接方式 → 生成链接与二维码 → 展示状态 → 断开。
 * 两种方式(局域网 / 公网隧道)共用同一套展示与断开控件,差异只在
 * 提示强度:隧道是公网暴露,必须显眼并给出风险说明。
 */

import { useCallback, useEffect, useState } from 'react'
import * as api from '../../api'
import * as remote from '../../remoteApi'
import { t } from '../../i18n'
import { subscribeServerEvents } from '../../serverEvents'
import { IconCheck, IconCopy, IconGlobe, IconTrash } from '../icons'
import type { RemoteLink, RemoteStatus } from '../../types'
import styles from './RemoteAccessSettings.module.css'

/** 远程命名空间:隧道传输协议要从这里读写(`/api/settings/remote`)。 */
const REMOTE_NS = 'remote'
type TunnelProtocol = 'quic' | 'http2'

type Notify = (kind: 'ok' | 'err', text: string) => void

/** 二维码位图的物理边长(像素)。显示尺寸由 CSS 控制,这里只保证够清晰。 */
const QR_PX = 504

/** 剩余有效期的人读文本。 */
function expiresText(at: number, now: number): string {
  const seconds = Math.round((at - now) / 1000)
  if (seconds <= 0) return t('remoteTicketExpired')
  if (seconds < 60) return t('remoteTicketLeft', { s: seconds })
  return t('remoteTicketLeftMinutes', { m: Math.round(seconds / 60) })
}

/** 二维码:PNG 走 Canvas(手机端"长按保存到相册"要真图),内联 SVG 兜底。
 *
 * 尺寸完全交给 CSS(`.qr` 的 clamp)——这里不写内联宽高:内联样式优先级高于
 * 样式表,一写就把"按容器收缩"锁死,长 URL 随之被挤成溢出。 */
function QrCode({ link }: { link: RemoteLink }) {
  const [pngUrl, setPngUrl] = useState<string | null>(null)

  useEffect(() => {
    if (link.qrWidth <= 0 || link.qrModules.length === 0) {
      setPngUrl(null)
      return
    }
    // 模块矩阵 → Canvas → dataURL。分辨率按模块数放大到 ~500px 物理像素:
    // 显示宽度由 CSS 决定,手机屏幕上 220px 的图被拉伸就会糊到扫不出来。
    const quiet = 4
    const total = link.qrWidth + quiet * 2
    const scale = Math.max(2, Math.round(QR_PX / total))
    const canvas = document.createElement('canvas')
    canvas.width = total * scale
    canvas.height = total * scale
    const context = canvas.getContext('2d')
    if (!context) return
    context.fillStyle = '#ffffff'
    context.fillRect(0, 0, canvas.width, canvas.height)
    context.fillStyle = '#000000'
    for (let y = 0; y < link.qrWidth; y += 1) {
      for (let x = 0; x < link.qrWidth; x += 1) {
        if (!link.qrModules[y * link.qrWidth + x]) continue
        context.fillRect(
          (x + quiet) * scale,
          (y + quiet) * scale,
          scale,
          scale,
        )
      }
    }
    setPngUrl(canvas.toDataURL('image/png'))
  }, [link.qrModules, link.qrWidth])

  if (pngUrl) {
    return (
      <img
        className={styles.qr}
        src={pngUrl}
        alt={t('remoteQrAlt')}
        width={QR_PX}
        height={QR_PX}
      />
    )
  }
  if (link.qrSvg) {
    return (
      <span
        className={styles.qr}
        // 二维码来自本机后端、由固定模板生成,不含外部输入拼接的标签。
        dangerouslySetInnerHTML={{ __html: link.qrSvg }}
      />
    )
  }
  return null
}

export function RemoteAccessSettings({ notify }: { notify: Notify }) {
  const [status, setStatus] = useState<RemoteStatus | null>(null)
  const [busy, setBusy] = useState(false)
  const [copied, setCopied] = useState(false)
  const [now, setNow] = useState(() => Date.now())
  // 隧道传输协议:存在 remote 命名空间里(cloudflared --protocol)。
  const [protocol, setProtocol] = useState<TunnelProtocol>('quic')
  const [revision, setRevision] = useState(0)

  const load = useCallback(async () => {
    try {
      setStatus(await remote.getRemoteStatus())
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }, [notify])

  useEffect(() => {
    void load()
  }, [load])

  // 读一次配置命名空间:协议是持久配置,不随 remote-updated 变,单独取。
  useEffect(() => {
    let disposed = false
    void api
      .getSettings()
      .then((describe) => {
        const ns = describe.namespaces.find((item) => item.ns === REMOTE_NS)
        if (!ns || disposed) return
        const tunnel = (ns.value as { tunnel?: Record<string, unknown> })?.tunnel
        const stored = tunnel?.['transportProtocol']
        setRevision(ns.revision)
        setProtocol(stored === 'http2' ? 'http2' : 'quic')
      })
      .catch(() => {
        /* 读不到配置时保持默认值展示,不弹错:这条只影响一个可选档位 */
      })
    return () => {
      disposed = true
    }
  }, [])

  /** 写协议。改动对**下一条**隧道生效(运行中的 cloudflared 不会中途换协议)。 */
  const saveProtocol = async (next: TunnelProtocol) => {
    const previous = protocol
    setProtocol(next)
    try {
      await api.updateNamespace(REMOTE_NS, { tunnel: { transportProtocol: next } }, revision)
      const ns = (await api.getSettings()).namespaces.find((item) => item.ns === REMOTE_NS)
      if (ns) setRevision(ns.revision)
      notify('ok', t('remoteProtocolSaved'))
    } catch (error) {
      setProtocol(previous)
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }


  useEffect(() => {
    return subscribeServerEvents((type) => {
      if (type === 'remote-updated') void load()
    })
  }, [load])

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(timer)
  }, [])

  /** 所有会改变连接状态的动作共用一层 busy 与错误上报。
   *
   * `busy` 不只是禁用按钮:隧道启停要等 cloudflared 与 Cloudflare 边缘建连
   * (固有 2–5 秒),期间界面必须给出**可见的进行中反馈** —— 否则用户看到
   * 按钮变灰、界面纹丝不动,会以为卡死了(实测反馈就是"响应半天")。
   *
   * 动作的返回值**一律不当作状态用**:后端各端点返回的形状并不一致 ——
   * `GET /status` 是完整 RemoteStatus,而 `lan/start` 只回 `{address,bind,
   * candidates,link,port}`、`lan/stop` 只回 `{ok,stopped}`。此前直接把返回
   * 值 setStatus,会让 `status.sessions` 变成 undefined,渲染
   * `status.sessions.lan` 时整块面板抛错("界面渲染出错")。所以动作完成后
   * 统一 `load()` 取权威状态。 */
  const [busyLabel, setBusyLabel] = useState('')
  const run = async (action: () => Promise<unknown>, okMessage: string, busyText: string) => {
    setBusy(true)
    setBusyLabel(busyText)
    try {
      await action()
      notify('ok', okMessage)
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      await load()
      setBusy(false)
      setBusyLabel('')
    }
  }

  const copyLink = async (url: string) => {
    try {
      await navigator.clipboard.writeText(url)
      setCopied(true)
      window.setTimeout(() => setCopied(false), 1600)
    } catch {
      notify('err', t('remoteCopyFailed'))
    }
  }

  const tunnel = status?.tunnel ?? null
  const lan = status?.lan ?? null

  return (
    <section className="setm-section">
      {/* 进行中横幅:隧道启停要等 cloudflared 建连(固有数秒),没有这条
          用户会以为界面卡死了。 */}
      {busy && busyLabel && (
        <div className={styles.busy} role="status" aria-live="polite">
          <span className={styles.busySpinner} aria-hidden="true" />
          <span>{busyLabel}</span>
        </div>
      )}
      <div className={styles.grid}>
        {/* ---- 局域网 ---- */}
        <div className={styles.card}>
          <div className={styles.cardHead}>
            <div>
              <h4 className={styles.cardTitle}>{t('remoteLanTitle')}</h4>
              <p className={styles.cardDesc}>{t('remoteLanDesc')}</p>
            </div>
            {lan && <span className={styles.badge}>{t('remoteOn')}</span>}
          </div>

          {lan && lan.candidates.length > 0 && (
            <label className={styles.field}>
              <span className={styles.fieldLabel}>{t('remoteLanAddress')}</span>
              <select
                className={styles.select}
                value={lan.address}
                disabled={busy}
                onChange={(event) =>
                  void run(
                    () => remote.selectLanAddress(event.target.value),
                    t('remoteLanAddressSwitched'),
                    t('remoteRefreshing'),
                  )
                }
              >
                {lan.candidates.map((candidate) => (
                  <option key={candidate.address} value={candidate.address}>
                    {candidate.address} · {candidate.interface}
                    {candidate.recommended ? ` · ${t('remoteRecommended')}` : ''}
                  </option>
                ))}
              </select>
            </label>
          )}

          <div className={styles.actions}>
            {lan ? (
              <button
                type="button"
                className={`${styles.btn} ${styles.danger}`}
                disabled={busy}
                onClick={() => void run(() => remote.stopLan(), t('remoteLanStopped'), t('remoteClosing'))}
              >
                <IconTrash size={13} />
                {t('remoteCloseLan')}
              </button>
            ) : (
              <button
                type="button"
                className={styles.btn}
                disabled={busy || status?.enabled === false}
                onClick={() => void run(() => remote.startLan(), t('remoteLanStarted'), t('remoteStarting'))}
              >
                {t('remoteStartLan')}
              </button>
            )}
          </div>

          {lan && <LinkBlock link={lan.link} onCopy={copyLink} copied={copied} now={now} />}
        </div>

        {/* ---- 公网隧道 ---- */}
        <div className={`${styles.card} ${styles.publicCard}`}>
          <div className={styles.cardHead}>
            <div>
              <h4 className={styles.cardTitle}>{t('remoteTunnelTitle')}</h4>
              <p className={styles.cardDesc}>{t('remoteTunnelDesc')}</p>
            </div>
            {tunnel && <span className={`${styles.badge} ${styles.badgeDanger}`}>{t('remoteExposed')}</span>}
          </div>

          {tunnel && (
            <div className={styles.warning} role="alert">
              <strong>{t('remoteTunnelWarningTitle')}</strong>
              <span>{t('remoteTunnelWarningBody')}</span>
            </div>
          )}

          {/* 传输协议:对下一条隧道生效(运行中的 cloudflared 不会中途换协议),
              所以开启前后都能改,但要在文案里说清生效时机。 */}
          <label className={styles.field}>
            <span className={styles.fieldLabel}>{t('remoteProtocolLabel')}</span>
            <select
              className={styles.select}
              value={protocol}
              disabled={busy}
              onChange={(event) =>
                void saveProtocol(event.target.value === 'http2' ? 'http2' : 'quic')
              }
            >
              <option value="quic">{t('remoteProtocolQuic')}</option>
              <option value="http2">{t('remoteProtocolHttp2')}</option>
            </select>
            <span className={styles.meta}>{t('remoteProtocolHint')}</span>
          </label>

          <div className={styles.actions}>
            {tunnel ? (
              <>
                <button
                  type="button"
                  className={`${styles.btn} ${styles.danger}`}
                  disabled={busy}
                  onClick={() => void run(() => remote.stopTunnel(), t('remoteTunnelStopped'), t('remoteClosing'))}
                >
                  <IconTrash size={13} />
                  {t('remoteCloseTunnel')}
                </button>
                <button
                  type="button"
                  className={styles.btn}
                  disabled={busy}
                  onClick={() =>
                    void run(() => remote.refreshTicket(), t('remoteTicketRefreshed'), t('remoteRefreshing'))
                  }
                >
                  {t('remoteRefreshTicket')}
                </button>
              </>
            ) : (
              <button
                type="button"
                className={styles.btn}
                disabled={busy || status?.enabled === false}
                onClick={() => void run(() => remote.startTunnel(), t('remoteTunnelStarted'), t('remoteStarting'))}
              >
                {t('remoteStartTunnel')}
              </button>
            )}
          </div>

          {tunnel && <LinkBlock link={tunnel.link} onCopy={copyLink} copied={copied} now={now} />}
        </div>
      </div>

      {/* ---- 在线会话与审计 ---- */}
      {status && (
        <div className={styles.sessions}>
          <div className={styles.sessionsHead}>
            <h4 className={styles.cardTitle}>{t('remoteSessionsTitle')}</h4>
            <span className={styles.meta}>
              {t('remoteSessionsCount', {
                lan: status.sessions.lan,
                tunnel: status.sessions.tunnel,
              })}
            </span>
          </div>
          {status.sessions.items.length === 0 ? (
            <p className={styles.cardDesc}>{t('remoteSessionsEmpty')}</p>
          ) : (
            <ul className={styles.sessionList}>
              {status.sessions.items.map((session) => (
                <li key={session.id} className={styles.sessionRow}>
                  <span className={styles.sessionVia}>
                    {session.via === 'tunnel' ? t('remoteViaTunnel') : t('remoteViaLan')}
                  </span>
                  <code className={styles.sessionPeer}>{session.peer}</code>
                  <button
                    type="button"
                    className={styles.linkBtn}
                    disabled={busy}
                    onClick={() =>
                      void remote.revokeRemoteSession(session.id).then(load)
                    }
                  >
                    {t('remoteRevokeSession')}
                  </button>
                </li>
              ))}
            </ul>
          )}
          <div className={styles.actions}>
            <button
              type="button"
              className={`${styles.btn} ${styles.danger}`}
              disabled={busy}
              onClick={() =>
                void remote.disconnectAll().then(() => {
                  notify('ok', t('remoteAllDisconnected'))
                  return load()
                })
              }
            >
              {t('remoteDisconnectAll')}
            </button>
          </div>
          <p className={styles.meta}>
            {t('remoteAuditHint', { path: status.auditPath })}
            {status.blockedPeers > 0 && ` · ${t('remoteBlockedPeers', { n: status.blockedPeers })}`}
          </p>
        </div>
      )}
    </section>
  )
}

/** 链接 + 二维码 + 复制 + 有效期。 */
function LinkBlock({
  link,
  onCopy,
  copied,
  now,
}: {
  link: RemoteLink
  onCopy: (url: string) => void
  copied: boolean
  now: number
}) {
  return (
    <div className={styles.linkBlock}>
      <QrCode link={link} />
      <div className={styles.linkMeta}>
        <code className={styles.linkUrl} title={link.ticketUrl}>
          {link.ticketUrl}
        </code>
        <div className={styles.linkActions}>
          <button type="button" className={styles.linkBtn} onClick={() => onCopy(link.ticketUrl)}>
            {copied ? <IconCheck size={13} /> : <IconCopy size={13} />}
            {copied ? t('remoteCopied') : t('remoteCopyLink')}
          </button>
          <a className={styles.linkBtn} href={link.ticketUrl} target="_blank" rel="noreferrer">
            <IconGlobe size={13} />
            {t('remoteOpenLink')}
          </a>
        </div>
        <p className={styles.meta}>
          {expiresText(link.ticketExpiresAt, now)}
          {link.ticketSingleUse ? ` · ${t('remoteTicketOnce')}` : ` · ${t('remoteTicketReusable')}`}
        </p>
        {link.requirePin && link.pin && (
          <p className={styles.pin}>
            {t('remotePinLabel')}
            <code>{link.pin}</code>
          </p>
        )}
        {link.requirePin && !link.pin && <p className={styles.meta}>{t('remotePinRemoteHidden')}</p>}
      </div>
    </div>
  )
}
