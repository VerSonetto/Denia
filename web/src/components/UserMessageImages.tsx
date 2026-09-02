import type { UserMessageImage } from '../types'
import { userImageDataUrl } from '../userImages'
import { t } from '../i18n'
import { CopyMessageButton } from './CopyMessageButton'
import { IconRewind } from './icons'

/** 用户消息中的内联图片缩略图网格。 */
export function UserMessageImages({
  images,
}: {
  images: UserMessageImage[]
}) {
  if (images.length === 0) return null
  return (
    <div className="msg-user-images">
      {images.map((image, index) => (
        <a
          key={index}
          className="msg-user-image"
          href={userImageDataUrl(image)}
          target="_blank"
          rel="noopener noreferrer"
          onClick={(event) => event.stopPropagation()}
        >
          <img src={userImageDataUrl(image)} alt="" loading="lazy" />
        </a>
      ))}
    </div>
  )
}

/** 用户消息:图片悬浮在气泡上方,文本在气泡内。 */
export function UserMessageBubble({
  text,
  images,
  pending = false,
  seq,
  onRewind,
}: {
  text: string
  images?: UserMessageImage[]
  pending?: boolean
  /** 用户消息的 seq;回退按钮需要它定位 checkpoint。 */
  seq?: number
  /** 点击回退按钮时回调(由页面层弹确认框)。 */
  onRewind?: (seq: number) => void
}) {
  const hasImages = !!images && images.length > 0
  const showText = text.trim().length > 0
  if (!showText && !hasImages) return null
  return (
    <div className={`user-message-stack${pending ? ' pending' : ' msg-copy-anchor'}`}>
      {hasImages && <UserMessageImages images={images} />}
      {showText && <div className="msg-user">{text}</div>}
      {!pending && showText && (
        <div className="message-actions">
          <CopyMessageButton text={text} />
          {onRewind && seq !== undefined && (
            <button
              type="button"
              className="rewind-message-btn"
              title={t('rewind')}
              aria-label={t('rewind')}
              onClick={(event) => {
                event.stopPropagation()
                onRewind(seq)
              }}
            >
              <IconRewind size={16} />
            </button>
          )}
        </div>
      )}
    </div>
  )
}
