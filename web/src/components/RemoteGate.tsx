/**
 * 远程门页:扫码后在手机浏览器上先看到的那一屏。
 *
 * 流程:URL 带 `?ticket=…` → 自动兑换 → 需要 PIN 就出输入框 → 拿到会话
 * cookie 后刷新进入控制台。任何一步失败都停在可读的说明上,而不是白屏。
 *
 * 兑换成功后立刻 `history.replaceState` 抹掉 URL 里的票据:票据是一次性
 * 凭据,留在地址栏会被截屏、被分享、被写进浏览器历史。
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import * as api from '../remoteApi'
import { t } from '../i18n'

type Phase = 'exchanging' | 'pin' | 'done' | 'failed'

export function RemoteGate() {
  const [phase, setPhase] = useState<Phase>('exchanging')
  const [message, setMessage] = useState('')
  const [challenge, setChallenge] = useState('')
  const [attempts, setAttempts] = useState(0)
  const [pin, setPin] = useState('')
  const [busy, setBusy] = useState(false)
  const started = useRef(false)

  const stripTicket = useCallback(() => {
    const url = new URL(window.location.href)
    url.searchParams.delete('ticket')
    window.history.replaceState(null, '', url.pathname + url.search + url.hash)
  }, [])

  const exchange = useCallback(
    async (ticket: string) => {
      setPhase('exchanging')
      setBusy(true)
      try {
        const result = await api.exchangeTicket(ticket)
        if (result.status === 'pin-required') {
          setChallenge(result.challenge)
          setAttempts(result.attempts)
          setPhase('pin')
          return
        }
        setPhase('done')
        window.location.replace(window.location.pathname + window.location.hash)
      } catch (error) {
        setPhase('failed')
        setMessage(error instanceof Error ? error.message : String(error))
      } finally {
        setBusy(false)
      }
    },
    [],
  )

  useEffect(() => {
    if (started.current) return
    started.current = true
    const ticket = new URL(window.location.href).searchParams.get('ticket')
    if (!ticket) {
      setPhase('failed')
      setMessage(t('remoteGateNoTicket'))
      return
    }
    // 兑换请求发出后票据就已进请求体,地址栏可以立刻清干净。
    stripTicket()
    void exchange(ticket)
  }, [exchange, stripTicket])

  const submit = async () => {
    if (pin.trim().length === 0) return
    setBusy(true)
    try {
      await api.submitPin(challenge, pin.trim())
      setPhase('done')
      window.location.replace(window.location.pathname + window.location.hash)
    } catch (error) {
      setMessage(error instanceof Error ? error.message : String(error))
      setPin('')
      // 挑战作废后必须重新扫码,不能继续在同一个 challenge 上试。
      if (error instanceof Error && /扫码/.test(error.message)) {
        setPhase('failed')
      }
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="rgate">
      <div className="rgate-card">
        <div className="rgate-brand">Denia</div>
        {phase === 'exchanging' && (
          <>
            <h1>{t('remoteGateChecking')}</h1>
            <p className="rgate-hint">{t('remoteGateCheckingHint')}</p>
            <div className="rgate-spinner" aria-hidden="true" />
          </>
        )}

        {phase === 'done' && (
          <>
            <h1>{t('remoteGateReady')}</h1>
            <p className="rgate-hint">{t('remoteGateReadyHint')}</p>
          </>
        )}

        {phase === 'pin' && (
          <>
            <h1>{t('remoteGatePinTitle')}</h1>
            <p className="rgate-hint">{t('remoteGatePinHint')}</p>
            <input
              className="rgate-pin"
              value={pin}
              onChange={(event) => setPin(event.target.value.replace(/\D/g, '').slice(0, 6))}
              onKeyDown={(event) => {
                if (event.key === 'Enter') void submit()
              }}
              inputMode="numeric"
              autoComplete="one-time-code"
              placeholder="••••••"
              aria-label={t('remoteGatePinTitle')}
              autoFocus
            />
            {message && <p className="rgate-error">{message}</p>}
            {attempts > 0 && (
              <p className="rgate-hint">
                {t('remoteGatePinAttempts', { n: attempts })}
              </p>
            )}
            <button
              type="button"
              className="rgate-btn"
              disabled={busy || pin.length === 0}
              onClick={() => void submit()}
            >
              {busy ? t('remoteGateVerifying') : t('remoteGateConfirm')}
            </button>
          </>
        )}

        {phase === 'failed' && (
          <>
            <h1>{t('remoteGateFailed')}</h1>
            <p className="rgate-error">{message}</p>
            <p className="rgate-hint">{t('remoteGateFailedHint')}</p>
          </>
        )}
      </div>
    </div>
  )
}

/**
 * 当前页面是不是"扫码落地页"。
 *
 * 只在 URL 里带 ticket、或本机正处于远程监听端口上时才接管整页 ——
 * 桌面控制台自己打开时不该看到这一屏。
 */
export function shouldShowGate(): boolean {
  return new URL(window.location.href).searchParams.has('ticket')
}
