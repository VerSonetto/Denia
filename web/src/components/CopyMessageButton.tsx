import { useEffect, useRef, useState } from 'react'
import { t } from '../i18n'
import { IconBranch, IconCheck, IconCopy } from './icons'

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

/**
 * 轮次收尾消息的分支按钮(抄 dsh MessageIconActions 的 branch 位):
 * 不可分支(非 transcript 当前尾部)时保持可见但禁用,tooltip 说明原因。
 */
export function BranchMessageButton({
  available,
  onBranch,
}: {
  available: boolean
  onBranch: () => void
}) {
  const label = available ? t('messageBranch') : t('messageBranchUnavailable')
  return (
    <button
      type="button"
      className="copy-message-btn branch"
      title={label}
      aria-label={label}
      aria-disabled={available ? undefined : true}
      data-unavailable={available ? undefined : ''}
      onClick={available ? onBranch : undefined}
    >
      <IconBranch size={16} />
    </button>
  )
}