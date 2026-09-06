import type { QueuedMessage } from '../types'
import { t } from '../i18n'
import { IconZap, IconPencil, IconTrash } from './icons'

/**
 * 消息队列:AI 运行期间发送的新消息先进入队列,本轮结束后自动发送。
 * 展示在输入框上方(TodoPanel 同级,排列在其上);每条右侧三个按钮:
 * 立即发送(打断当前 AI)/ 编辑(回填输入框)/ 删除。
 */

function QueueIcon() {
  return (
    <svg width={14} height={14} viewBox="0 0 14 14" fill="none" aria-hidden="true">
      <path d="M2 3.5h10M2 7h10M2 10.5h6" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
      <path d="M9.5 12.5v-2.6M9.5 9.9l1.7 1.7M9.5 9.9 7.8 11.6" stroke="currentColor" strokeWidth="1.1" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

export function QueuedMessagePanel({
  messages,
  onSendNow,
  onEdit,
  onDelete,
}: {
  messages: readonly QueuedMessage[]
  onSendNow: (message: QueuedMessage) => void
  onEdit: (message: QueuedMessage) => void
  onDelete: (message: QueuedMessage) => void
}) {
  if (messages.length === 0) return null

  return (
    <section className="queue-panel" data-testid="queue-panel" aria-label={t('queueTitle')}>
      <div className="queue-head">
        <span className="queue-lead" aria-hidden><QueueIcon /></span>
        <span className="queue-title">{t('queueTitle')}</span>
        <span className="queue-count">{t('queueCount', { count: messages.length })}</span>
        <span className="queue-hint">{t('queueSentHint')}</span>
      </div>
      <ul className="queue-list">
        {messages.map((message) => (
          <li key={message.id} className="queue-item">
            <span className="queue-content" title={message.text}>{message.text}</span>
            <span className="queue-actions">
              <button
                type="button"
                className="icon-btn queue-action queue-send-now"
                title={t('queueSendNow')}
                aria-label={t('queueSendNow')}
                onClick={() => onSendNow(message)}
              >
                <IconZap size={13} />
              </button>
              <button
                type="button"
                className="icon-btn queue-action"
                title={t('queueEdit')}
                aria-label={t('queueEdit')}
                onClick={() => onEdit(message)}
              >
                <IconPencil size={13} />
              </button>
              <button
                type="button"
                className="icon-btn queue-action queue-delete"
                title={t('queueDeleteTitle')}
                aria-label={t('queueDeleteTitle')}
                onClick={() => onDelete(message)}
              >
                <IconTrash size={13} />
              </button>
            </span>
          </li>
        ))}
      </ul>
    </section>
  )
}