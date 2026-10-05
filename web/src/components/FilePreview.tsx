/**
 * 文件阅读器的「预览」态：HTML 走 iframe 真实渲染，Markdown 走对话流那套
 * 安全渲染器。
 *
 * # 为什么 HTML 必须用 iframe 而不是 dangerouslySetInnerHTML
 *
 * 预览的就是**工作区里一个用户自己的文件**，但它仍然是任意内容：往仓库里
 * 放一个带 `<script>` 的 HTML，再点开它，那段脚本就会在 Denia 的源上执行
 * ——能读 console 的 localStorage、cookie，能带着会话凭据发任意请求。
 * `dangerouslySetInnerHTML` 就是这个后果，没有中间态。
 *
 * 所以预览一律放进 `<iframe sandbox>`：
 *
 * - **不给 `allow-scripts`** —— 文档里的脚本既不执行，也不发请求；
 * - **不给 `allow-same-origin`** —— 文档拿不到 Denia 的 origin，于是读不到
 *   它的 cookie/localStorage，也碰不到 `window.parent`（同源逃逸是 sandbox
 *   最经典的绕过方式，去掉这项即封死）；
 * - 保留 `allow-popups` 让文档里的 `target="_blank"` 链接仍能点开。
 *
 * 与之配套，服务端对预览资源另有三道独立防线（路径校验 / MIME 白名单 /
 * 严格 CSP 禁脚本，见 `crates/server/src/api/fs.rs`）。前端 sandbox 与服务端
 * CSP 互相独立：任一层配漏，另一层仍在。
 *
 * # 相对引用为什么能工作
 *
 * iframe 的 `src` 是 `/api/fs/preview?path=…&file=…` 这个**路径式 URL**，
 * 文档里的 `./a.css`、`../img/b.png` 由浏览器以它为基址自行解析，再命中
 * 同一个预览端点。`srcDoc` 做不到（它落在 `about:srcdoc`，没有基址）。
 *
 * # Markdown 为什么不用 iframe
 *
 * Markdown 没有「真实渲染」这回事——它必须先变成 DOM。项目已经有一套处理
 * 不可信 Markdown 的渲染器（[MarkdownText](../markdown/MarkdownText.tsx)：
 * 链接走协议白名单、图片只放行绝对 HTTP(S)、裸 HTML 当字面文本、KaTeX 不
 * 开 trusted commands）。直接复用它，不要另写一份 sanitize。
 */

import { useMemo } from 'react'
import { t } from '../i18n'
import * as api from '../api'
import { MarkdownText } from '../markdown/MarkdownText'
import type { MarkdownLabels } from '../markdown/render'
import './FilePreview.css'

/** 一个文件能否有「预览」态。 */
export type PreviewKind = 'html' | 'markdown'

/**
 * 按文件名判定预览类型；不可预览返回 `null`（调用方不显示切换器）。
 * @param name - 文件名（可含路径）。
 * @returns 预览类型，或不可预览。
 */
export function previewKindOf(name: string): PreviewKind | null {
  const lower = name.toLowerCase()
  if (lower.endsWith('.html') || lower.endsWith('.htm')) return 'html'
  if (lower.endsWith('.md') || lower.endsWith('.markdown')) return 'markdown'
  return null
}

export interface FilePreviewProps {
  /** 工作区绝对路径（预览资源的根）。 */
  workspacePath: string
  /** 当前文件的相对路径。 */
  file: string
  /** 文件名（含扩展名），用于判定预览类型。 */
  name: string
  /** 正文（Markdown 预览用；HTML 预览经 iframe 自行拉取）。 */
  content: string
}

export function FilePreview({ workspacePath, file, name, content }: FilePreviewProps) {
  const kind = previewKindOf(name)
  // Markdown 标签必须是引用稳定的对象，否则 MarkdownText 的渲染缓存
  // 会在每次父组件重渲染时被丢弃。
  const labels = useMemo<MarkdownLabels>(
    () => ({ code: { copyLabel: t('copy'), copiedLabel: t('copied') }, footnotes: t('footnotes') }),
    [],
  )
  if (kind === null) return null
  if (kind === 'markdown') {
    return (
      <div className="file-preview file-preview-markdown" data-file-preview="markdown">
        <div className="markdown">
          <MarkdownText text={content} labels={labels} />
        </div>
      </div>
    )
  }
  return (
    <div className="file-preview file-preview-html" data-file-preview="html">
      <iframe
        className="file-preview-frame"
        title={name}
        src={api.workspacePreviewUrl(workspacePath, file)}
        sandbox="allow-popups"
        referrerPolicy="no-referrer"
      />
    </div>
  )
}

export default FilePreview
