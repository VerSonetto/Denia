/**
 * 字节数的人类可读格式化(单一事实源)。
 *
 * 文件读取面板、记忆面板与更新下载进度各自画过一份 KB/MB 换算,阈值或
 * 小数位一漂移,同一份字节在界面上就会出现两种尺寸;这里统一 1024 进制
 * 阶梯(最高到 TB),渲染风格由调用方按各自口径决定。
 */

export type ByteUnit = 'B' | 'KB' | 'MB' | 'GB' | 'TB'

const BYTE_UNITS: readonly ByteUnit[] = ['B', 'KB', 'MB', 'GB', 'TB']

export interface ByteSize {
  value: number
  unit: ByteUnit
}

/** 1024 进制分桶:1024 以下为 B,每满 1024 升一级,封顶 TB。 */
export function splitByteSize(bytes: number): ByteSize {
  let value = bytes
  let index = 0
  while (index < BYTE_UNITS.length - 1 && value >= 1024) {
    value /= 1024
    index += 1
  }
  return { value, unit: BYTE_UNITS[index] }
}

/** 人类可读的字节数(与文件树/产物行同一套口径)。 */
export function formatBytesHuman(bytes: number): string {
  const { value, unit } = splitByteSize(bytes)
  if (unit === 'B') return `${value} B`
  return `${value.toFixed(1)} ${unit}`
}
