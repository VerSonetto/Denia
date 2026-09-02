import { useEffect, useRef, useState } from 'react'
import { t } from '../i18n'
import { IconCheck, IconCopy } from './icons'

/** 消息底部悬浮复制按钮:仅图标,点击复制整条消息文本,短暂切换对勾反馈。 */
export function CopyMessageButton({ text }: { text: string }) {
  const [state, setState] = useState<'idle' | 'copied' | 'failed'>('idle')
  const timerRef = useRef<number | undefined>(undefined)

  useEffect(() => {
    return () => window.clearTimeout(timerRef.current)
  }, [])

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(text)
      setState('copied')
    } catch {
      setState('failed')
    }
    window.clearTimeout(timerRef.current)
    timerRef.current = window.setTimeout(() => setState('idle'), 1600)
  }

  const label = state === 'copied' ? t('copied') : state === 'failed' ? t('copyFailed') : t('copy')

  return (
    <button
      type="button"
      className={`copy-message-btn${state === 'copied' ? ' copied' : ''}${state === 'failed' ? ' failed' : ''}`}
      onClick={copy}
      title={label}
      aria-label={label}
    >
      {state === 'copied' ? <IconCheck size={16} /> : <IconCopy size={16} />}
    </button>
  )
}