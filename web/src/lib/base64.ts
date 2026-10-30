/**
 * UTF-8 → 标准 base64 的共享内核(btoa 语义)。
 *
 * 不能直接 `btoa(值)`:btoa 只接受 Latin-1,中文工作区路径会抛异常,所以
 * 先走 UTF-8 字节再进 btoa。调用方各自包装(api 的 URL-safe 替换/去 padding、
 * 终端 PTY 的裸 base64),输出与各自原实现逐字节一致。
 */
export function encodeUtf8Base64(text: string): string {
  const bytes = new TextEncoder().encode(text)
  let binary = ''
  for (const byte of bytes) binary += String.fromCharCode(byte)
  return btoa(binary)
}
