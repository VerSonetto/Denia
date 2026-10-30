import { memo, useEffect, useState } from 'react'
import { t } from '../../i18n'
import { formatDuration } from './formatDuration'

/**
 * 运行中指示行:消息流末尾「工作中… + 计时」。
 * 叶子组件自持 4Hz tick,流式重渲染不牵动;起点取当前 turn 的 turn-start,
 * 切会话往返计时不重置。
 */
export const WorkIndicator = memo(function WorkIndicator({ startedAt }: { startedAt: number }) {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    const interval = window.setInterval(() => setNow(Date.now()), 250)
    return () => window.clearInterval(interval)
  }, [])
  const elapsed = Math.max(0, now - startedAt)
  return (
    <div className="work-indicator" role="status">
      <span>{t('workIndicator')}</span>
      <span className="work-dots" aria-hidden>
        <i />
        <i />
        <i />
      </span>
      <span className="work-elapsed">
        {elapsed < 60_000 ? `${(elapsed / 1000).toFixed(1)}s` : formatDuration(elapsed)}
      </span>
    </div>
  )
})
