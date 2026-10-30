import type { QueuedMessage } from '../types'
import { t } from '../i18n'
import {
  IconZap,
  IconPencil,
  IconTrash,
  IconBranch,
  IconToolWait,
  IconFile,
  IconSpinner,
} from './icons'

/**
 * 消息队列:AI 运行期间发送的新消息先进入队列,本轮结束后自动发送。
 * 每条右侧三个动作:发送方式左右选择器(插话 / 引导)、编辑(回填输入框)、删除。
 *
 * 两种发送方式都是"立刻把消息发出去",区别只在模型什么时候看到:
 * - 插话:取消当前轮次,再作为新消息发送;
 * - 引导:不打断当前输出/工具调用,由服务端在当前动作结束后的下一个 step
 *   边界注入。
 *
 * 载荷与输入框同构:文本之外还可以带粘贴图片、附件与轨迹引用 —— 入队时
 * 整体搬进来,这里把图片缩略图与附件/引用芯片一并画出来,避免"排队时看得
 * 见、进队列就消失"的错觉。
 */

function QueueIcon() {
  return (
    <svg width={14} height={14} viewBox="0 0 14 14" fill="none" aria-hidden="true">
      <path d="M2 3.5h10M2 7h10M2 10.5h6" stroke="currentColor" strokeWidth="1.2" strokeLinecap="round" />
      <path d="M9.5 12.5v-2.6M9.5 9.9l1.7 1.7M9.5 9.9 7.8 11.6" stroke="currentColor" strokeWidth="1.1" strokeLinecap="round" strokeLinejoin="round" />
    </svg>
  )
}

/**
 * 队列条目的发送方式左右选择器：左边插话、右边引导。
 *
 * 不做下拉浮层：两种方式都是"立即发送"，直接并排放在条目动作区，点击即
 * 执行。title 里保留两者的时机差异；已交给模型的引导显示为等待态。
 */
function QueueSendSelector({
  message,
  onInterject,
  onGuide,
}: {
  message: QueuedMessage
  onInterject: (message: QueuedMessage) => void
  onGuide: (message: QueuedMessage) => void
}) {
  if (message.state === 'steering') {
    // 等待态由行内的状态徽标统一表达，动作区不再重复放一个禁用选择器。
    return null
  }

  return (
    <span className="queue-send-picker" role="group" aria-label={t('queueSendChoose')}>
      <button
        type="button"
        className="queue-send-choice queue-interject"
        title={t('queueInterjectDesc')}
        aria-label={t('queueModeInterject')}
        onClick={() => onInterject(message)}
      >
        <IconZap size={11} />
        <span>{t('queueModeInterject')}</span>
      </button>
      <button
        type="button"
        className="queue-send-choice queue-guide"
        title={t('queueGuideDesc')}
        aria-label={t('queueModeGuide')}
        onClick={() => onGuide(message)}
      >
        <IconToolWait size={11} />
        <span>{t('queueModeGuide')}</span>
      </button>
    </span>
  )
}

export function QueuedMessagePanel({
  messages,
  onInterject,
  onGuide,
  onEdit,
  onDelete,
}: {
  messages: readonly QueuedMessage[]
  onInterject: (message: QueuedMessage) => void
  onGuide: (message: QueuedMessage) => void
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
        <span className="queue-hint">{t('queueHeadHint')}</span>
      </div>
      <ul className="queue-list">
        {messages.map((message, index) => {
          const images = message.images ?? []
          const attachments = message.attachments ?? []
          const quotes = message.quotes ?? []
          const steering = message.state === 'steering'
          return (
            <li key={message.id} className={`queue-item${steering ? ' steering' : ''}`}>
              <span className="queue-index" aria-hidden="true">{index + 1}</span>
              <span className="queue-main">
                <span className="queue-line">
                  <span className="queue-text" title={message.text}>
                    {message.text || t('queueEmptyText')}
                  </span>
                  <span
                    className={`queue-mode${steering ? ' steering queue-steering-state' : ''}`}
                    title={steering ? t('queueSteeringHint') : undefined}
                  >
                    {steering && <IconSpinner size={10} />}
                    {steering ? t('queueStateSteering') : t('queueStateQueued')}
                  </span>
                </span>
                {(images.length > 0 || attachments.length > 0 || quotes.length > 0) && (
                  <span className="queue-media">
                    {quotes.map((quote) => (
                      <span className="queue-chip" key={quote.id} title={quote.title}>
                        <IconBranch size={11} />
                        <span className="queue-chip-name">{quote.title}</span>
                      </span>
                    ))}
                    {images.map((image, imageIndex) => (
                      <span className="queue-thumb" key={`img-${imageIndex}`} title={image.name}>
                        <img src={image.preview} alt="" />
                      </span>
                    ))}
                    {attachments.map((attachment, attachmentIndex) => (
                      <span className="queue-chip" key={`att-${attachmentIndex}`} title={attachment.name}>
                        <IconFile size={11} />
                        <span className="queue-chip-name">{attachment.name}</span>
                      </span>
                    ))}
                  </span>
                )}
              </span>
              <span className="queue-actions">
                <QueueSendSelector
                  message={message}
                  onInterject={onInterject}
                  onGuide={onGuide}
                />
                <button
                  type="button"
                  className="icon-btn queue-action"
                  title={steering ? t('queueSteeringHint') : t('queueEdit')}
                  aria-label={steering ? t('queueSteeringHint') : t('queueEdit')}
                  disabled={steering}
                  onClick={() => onEdit(message)}
                >
                  <IconPencil size={13} />
                </button>
                <button
                  type="button"
                  className="icon-btn queue-action queue-delete"
                  title={steering ? t('queueSteeringHint') : t('queueDeleteTitle')}
                  aria-label={steering ? t('queueSteeringHint') : t('queueDeleteTitle')}
                  disabled={steering}
                  onClick={() => onDelete(message)}
                >
                  <IconTrash size={13} />
                </button>
              </span>
            </li>
          )
        })}
      </ul>
    </section>
  )
}
