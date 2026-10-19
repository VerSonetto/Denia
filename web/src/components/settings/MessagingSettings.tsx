/**
 * 设置 → 消息平台:把 denia 接到聊天工具里。
 *
 * 面板按平台列卡片;目前只有微信,后续接入 QQ/飞书等只需在这里追加一张卡。
 * 微信卡片负责扫码登录、状态展示与断开 —— 会话选择、模型与权限切换都在
 * 聊天里用 /workspace、/switch、/model、/permission 完成。
 */

import { useCallback, useEffect, useState } from 'react'
import * as api from '../../api'
import { t } from '../../i18n'
import { IconCheck, IconTrash } from '../icons'
import type { WeixinChannelStatus } from '../../types'
import styles from './MessagingSettings.module.css'

type Notify = (kind: 'ok' | 'err', text: string) => void

/** 登录状态轮询间隔:二维码状态机每秒推进,前端不必更勤。 */
const POLL_MS = 1500

function messages(login: NonNullable<WeixinChannelStatus['login']>): string {
  if (login.message) return login.message
  switch (login.status) {
    case 'wait':
      return t('weixinWaitScan')
    case 'scaned':
      return t('weixinScaned')
    case 'confirmed':
      return t('weixinConfirmed')
    case 'failed':
      return t('weixinLoginFailed')
    default:
      return login.status
  }
}

export function MessagingSettings({ notify }: { notify: Notify }) {
  return (
    <div className={styles.root}>
      <p className={styles.intro}>{t('channelsIntro')}</p>
      <WeChatCard notify={notify} />
    </div>
  )
}

function WeChatCard({ notify }: { notify: Notify }) {
  const [status, setStatus] = useState<WeixinChannelStatus | null>(null)
  const [busy, setBusy] = useState(false)

  const load = useCallback(async () => {
    try {
      setStatus(await api.getWeixinChannel())
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    }
  }, [notify])

  useEffect(() => {
    void load()
  }, [load])

  // 登录中轮询:confirmed / failed 是终态,停止。
  useEffect(() => {
    const login = status?.login
    if (!login || login.status === 'confirmed' || login.status === 'failed') return
    const handle = window.setTimeout(() => {
      void load()
    }, POLL_MS)
    return () => window.clearTimeout(handle)
  }, [status, load])

  const start = async () => {
    setBusy(true)
    try {
      setStatus(await api.startWeixinLogin())
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      setBusy(false)
    }
  }

  const logout = async () => {
    setBusy(true)
    try {
      setStatus(await api.logoutWeixin())
      notify('ok', t('weixinLoggedOut'))
    } catch (error) {
      notify('err', error instanceof Error ? error.message : String(error))
    } finally {
      setBusy(false)
    }
  }

  const login = status?.login
  const loginFailed = login?.status === 'failed'

  return (
    <section className={styles.card}>
      <div className={styles.head}>
        <div className={styles.titleRow}>
          <span className={styles.title}>{t('weixinTitle')}</span>
          <span
            className={`${styles.badge} ${status?.connected ? styles.badgeOn : ''}`}
          >
            {status?.connected ? t('weixinConnected') : t('weixinDisconnected')}
          </span>
        </div>
        {status?.connected ? (
          <button
            type="button"
            className={styles.dangerBtn}
            disabled={busy}
            onClick={() => void logout()}
          >
            <IconTrash size={14} />
            {t('weixinLogout')}
          </button>
        ) : (
          <button
            type="button"
            className={styles.primaryBtn}
            disabled={busy}
            onClick={() => void start()}
          >
            <IconCheck size={14} />
            {loginFailed ? t('weixinRetry') : t('weixinConnect')}
          </button>
        )}
      </div>

      <p className={styles.hint}>{t('weixinHint')}</p>

      {status?.connected && status.account && (
        <div className={styles.meta}>
          <span>{t('weixinAccount')}</span>
          <code>{status.account.accountId}</code>
        </div>
      )}

      {!status?.connected && login && (
        <div className={styles.login}>
          {login.qrSvg && (
            <span
              className={styles.qr}
              // 二维码来自本机后端、由固定模板生成,不含外部输入拼接的标签。
              dangerouslySetInnerHTML={{ __html: login.qrSvg }}
            />
          )}
          <div className={styles.loginCopy}>
            <p className={styles.statusText}>{messages(login)}</p>
            {login.status === 'wait' && (
              <p className={styles.hint}>{t('weixinScanHint')}</p>
            )}
          </div>
        </div>
      )}

      {status?.lastError && (
        <p className={styles.error}>
          {t('weixinLastError')}: {status.lastError}
        </p>
      )}

      <p className={styles.diagnostic}>
        {status?.lastInboundAt
          ? `${t('weixinLastInbound')}: ${new Date(status.lastInboundAt).toLocaleTimeString()}（${status.lastInboundChars} ${t('weixinChars')}）${
              status.lastAction ? ` → ${status.lastAction}` : ''
            }`
          : t('weixinNoInbound')}
      </p>

      <div className={styles.commands}>
        <p className={styles.commandsTitle}>{t('weixinCommandsTitle')}</p>
        <p className={styles.hint}>{t('weixinCommands')}</p>
      </div>
    </section>
  )
}
