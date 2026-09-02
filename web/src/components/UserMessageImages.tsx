import type { UserMessageImage } from '../types'
import { userImageDataUrl } from '../userImages'
import { CopyMessageButton } from './CopyMessageButton'

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
}: {
  text: string
  images?: UserMessageImage[]
  pending?: boolean
}) {
  const hasImages = !!images && images.length > 0
  const showText = text.trim().length > 0
  if (!showText && !hasImages) return null
  return (
    <div className={`user-message-stack${pending ? ' pending' : ' msg-copy-anchor'}`}>
      {hasImages && <UserMessageImages images={images} />}
      {showText && <div className="msg-user">{text}</div>}
      {!pending && showText && <CopyMessageButton text={text} />}
    </div>
  )
}
