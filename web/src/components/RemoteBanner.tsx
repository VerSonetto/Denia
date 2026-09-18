/**
 * 「公网隧道已开启」警示条。
 *
 * 两种客户端都要看到它,但动作不同:
 * - 本机控制台:显示链接与剩余时间,给「关闭隧道」按钮(唯一的关闭入口
 *   之一,另一个是终端里的 Ctrl+C);
 * - 远程客户端:只提示"当前处于公网暴露状态",**不给关闭按钮** ——
 *   关隧道是主机持有者的权限,不该交给被临时授权的一方。
 *
 * 订阅 `remote-updated` 推送:隧道可能被终端里 Ctrl+C、被另一台设备关掉,
 * 本页不能靠轮询才知道。
 */

import { useCallback, useEffect, useState } from 'react'
import * as api from '../remoteApi'
import { t } from '../i18n'
import type { RemoteStatus } from '../types'
import { IconClose } from './icons'
import { subscribeServerEvents } from '../serverEvents'

/** 每 30 秒刷一次剩余时间显示(票据倒计时)。 */
const TICK_MS = 30_000

export function RemoteBanner({ local }: { local: boolean }) {
  const [status, setStatus] = useState<RemoteStatus | null>(null)
  const [busy, setBusy] = useState(false)
  const [now, setNow] = useState(() => Date.now())
  // 后端是权威来源:`local` 由 props 给的是"这个页面以为的",而实际权限
  // 以服务端返回的为准(比如会话过期后本机判定会变)。
  const isLocal = status?.local ?? local

  const load = useCallback(async () => {
    try {
      setStatus(await api.getRemoteStatus())
    } catch {
      // 远程客户端可能因会话过期拿不到状态:横幅静默消失比弹错更合适,
      // 真正的失效提示由远程门页负责。
      setStatus(null)
    }
  }, [])

  useEffect(() => {
    void load()
  }, [load])

  useEffect(() => {
    return subscribeServerEvents((type) => {
      if (type === 'remote-updated') void load()
    })
  }, [load])

  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), TICK_MS)
    return () => window.clearInterval(timer)
  }, [])

  const tunnel = status?.tunnel ?? null
  const lan = status?.lan ?? null
  if (!status || (!tunnel && !lan)) return null

  const closeTunnel = async () => {
    setBusy(true)
    try {
      await api.stopTunnel()
      await load()
    } finally {
      setBusy(false)
    }
  }

  // 隧道存在时是最需要提醒的状态:公网入口开着。
  if (tunnel) {
    const expiresIn = Math.max(0, Math.round((tunnel.link.ticketExpiresAt - now) / 1000))
    return (
      <div className="rbanner danger" role="status">
        <span className="rbanner-dot" aria-hidden="true" />
        <span className="rbanner-text">
          <strong>{t('remoteTunnelOn')}</strong>
          <span className="rbanner-sub">{t('remoteTunnelExposure')}</span>
        </span>
        {isLocal ? (
          <>
            <code className="rbanner-url" title={tunnel.url}>
              {tunnel.host}
            </code>
            {expiresIn > 0 && (
              <span className="rbanner-meta">{t('remoteTicketLeft', { s: expiresIn })}</span>
            )}
            <button
              type="button"
              className="rbanner-btn"
              disabled={busy}
              onClick={() => void closeTunnel()}
            >
              <IconClose size={12} />
              {t('remoteCloseTunnel')}
            </button>
          </>
        ) : (
          <span className="rbanner-meta">{t('remoteTunnelRemoteHint')}</span>
        )}
      </div>
    )
  }

  // 只有局域网:轻提示,不制造紧张感(局域网本身是可信网段的边界)。
  const address = lan ? `${lan.address}:${lan.port}` : ''
  const sessions = status.sessions.lan
  return (
    <div className="rbanner" role="status">
      <span className="rbanner-dot ok" aria-hidden="true" />
      <span className="rbanner-text">
        <strong>{t('remoteLanOn')}</strong>
        <span className="rbanner-sub">{t('remoteLanSessions', { n: sessions })}</span>
      </span>
      {isLocal && <code className="rbanner-url">{address}</code>}
      {isLocal && (
        <button
          type="button"
          className="rbanner-btn ghost"
          disabled={busy}
          onClick={() => {
            setBusy(true)
            void api
              .stopLan()
              .then(load)
              .finally(() => setBusy(false))
          }}
        >
          <IconClose size={12} />
          {t('remoteCloseLan')}
        </button>
      )}
    </div>
  )
}
