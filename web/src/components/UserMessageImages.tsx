import { useCallback, useEffect, useRef, useState } from 'react'
import { createPortal } from 'react-dom'
import type { UserMessageImage } from '../types'
import { userImageDataUrl } from '../userImages'
import { t } from '../i18n'
import { CopyMessageButton } from './CopyMessageButton'
import { IconArrowLeft, IconArrowRight, IconClose, IconRewind } from './icons'

/** 用户消息中的内联图片缩略图网格;点击缩略图打开大图预览。 */
export function UserMessageImages({
  images,
}: {
  images: UserMessageImage[]
}) {
  const [previewIndex, setPreviewIndex] = useState<number | null>(null)
  const closeRef = useRef<HTMLButtonElement>(null)
  const triggerRef = useRef<HTMLButtonElement | null>(null)

  const close = useCallback(() => {
    setPreviewIndex(null)
    // 关闭后回到刚才点开的缩略图,键盘/读屏焦点不丢失。
    triggerRef.current?.focus({ preventScroll: true })
  }, [])

  const step = useCallback((delta: number) => {
    setPreviewIndex((current) => {
      if (current === null || images.length === 0) return current
      return (current + delta + images.length) % images.length
    })
  }, [images.length])

  useEffect(() => {
    if (previewIndex === null) return
    closeRef.current?.focus({ preventScroll: true })
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault()
        close()
      } else if (event.key === 'ArrowLeft') {
        event.preventDefault()
        step(-1)
      } else if (event.key === 'ArrowRight') {
        event.preventDefault()
        step(1)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [previewIndex, close, step])

  if (images.length === 0) return null

  const preview = previewIndex === null ? null : images[previewIndex] ?? null

  return (
    <>
      <div className="msg-user-images">
        {images.map((image, index) => (
          <button
            key={index}
            type="button"
            className="msg-user-image"
            title={t('imagePreview')}
            aria-label={t('imagePreview')}
            onClick={(event) => {
              event.stopPropagation()
              triggerRef.current = event.currentTarget
              setPreviewIndex(index)
            }}
          >
            <img src={userImageDataUrl(image)} alt="" loading="lazy" draggable={false} />
          </button>
        ))}
      </div>
      {preview && createPortal(
        <div
          className="image-lightbox"
          role="dialog"
          aria-modal="true"
          aria-label={t('imagePreview')}
          onClick={close}
        >
          <button
            ref={closeRef}
            type="button"
            className="image-lightbox-close"
            title={t('close')}
            aria-label={t('close')}
            onClick={(event) => {
              event.stopPropagation()
              close()
            }}
          >
            <IconClose size={18} />
          </button>
          {images.length > 1 && (
            <>
              <button
                type="button"
                className="image-lightbox-nav prev"
                title={t('imagePreviewPrev')}
                aria-label={t('imagePreviewPrev')}
                onClick={(event) => {
                  event.stopPropagation()
                  step(-1)
                }}
              >
                <IconArrowLeft size={20} />
              </button>
              <button
                type="button"
                className="image-lightbox-nav next"
                title={t('imagePreviewNext')}
                aria-label={t('imagePreviewNext')}
                onClick={(event) => {
                  event.stopPropagation()
                  step(1)
                }}
              >
                <IconArrowRight size={20} />
              </button>
              <span className="image-lightbox-count">
                {(previewIndex ?? 0) + 1} / {images.length}
              </span>
            </>
          )}
          <img
            className="image-lightbox-img"
            src={userImageDataUrl(preview)}
            alt=""
            draggable={false}
            onClick={(event) => event.stopPropagation()}
          />
        </div>,
        document.body,
      )}
    </>
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
