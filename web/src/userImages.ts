import type { UserMessageImage } from './types'

/** 内联图片 data URL(用于缩略图/预览)。 */
export function userImageDataUrl(image: UserMessageImage): string {
  return `data:${image.mime};base64,${image.data}`
}

/** 粘贴图常见占位名不展示在 UI 上。 */
export function isGenericImageName(name: string | undefined): boolean {
  if (!name) return true
  const normalized = name.trim().toLowerCase()
  return (
    normalized === '' ||
    normalized === 'image' ||
    normalized === 'image.png' ||
    normalized === 'pasted-image' ||
    normalized.startsWith('pasted-')
  )
}
