import { useEffect, useState } from 'react'
import { t } from '../../i18n'

/**
 * 压缩进行中的占位行:与 CompactionRow 同一视觉家族(同边框/圆角/底色),
 * 但**不可点** —— 没有内容可展开,点了只会让人以为卡住了。
 *
 * 后端是一次 await、无中间进度事件,所以这里只表达"进行中":一个 spinner
 * 加一句正在做什么,外加已经等了多久(长任务没有时间感最容易让人以为
 * 死了)。不画假进度条 —— 不知道百分比就不该装作知道。
 */
export function CompactingRow({ startedAt }: { startedAt: number }) {
  const [elapsed, setElapsed] = useState(() => Math.max(0, Date.now() - startedAt))
  useEffect(() => {
    const timer = window.setInterval(() => {
      setElapsed(Math.max(0, Date.now() - startedAt))
    }, 1000)
    return () => window.clearInterval(timer)
  }, [startedAt])
  return (
    <div className="compaction-row compacting-row" role="status" aria-live="polite">
      <div className="compaction-header compacting-header">
        <span className="compacting-spinner" aria-hidden />
        <span className="compacting-label">{t('contextCompacting')}</span>
        <span className="compaction-meta compacting-elapsed">
          {t('contextCompactingElapsed', { seconds: Math.max(1, Math.round(elapsed / 1000)) })}
        </span>
      </div>
    </div>
  )
}
