import { t } from '../../i18n'
import type { UserMessageImage } from '../../types'
import { CopyMessageButton } from '../CopyMessageButton'
import { UserMessageImages } from '../UserMessageImages'
import { IconAgentList, IconImage, IconTool } from '../icons'
import { DisclosureRow } from './DisclosureRow'

/** 注入消息的 UI 投影:对话流上"老老实实显示是什么"——识别通道、给出诚实
 * 标题并剥掉给模型看的 `<system-reminder>` 框架与通道头行;未识别的文本保持
 * 原样。模型可见历史不受影响:框架只在后端日志里,前端只做展示投影。 */
const REMINDER_OPEN = '<system-reminder>\n'
const REMINDER_CLOSE = '</system-reminder>'

/** 视觉注入(浏览器截图 / read_file 读图)的标题与详情:图片本身直接可见,
 * 标题只说明"模型看到了什么",详情给出文件路径或来源。 */
function visualInjectionDisplay(
  text: string,
  images: UserMessageImage[],
): { title: string; detail: string } {
  const textPath =
    /^\[harness\] 读取图片:(.+?)\([^)]*\)$/.exec(text.trim())?.[1] ?? ''
  const filePath = textPath || images.find((image) => image.path)?.path || ''
  return {
    title: t('injectionVisualInput'),
    detail: filePath || t('injectionBrowserScreenshot'),
  }
}

function injectionDisplay(text: string): { title: string; body: string } {
  // 子代理/后台任务通知沿用首行原样展示。
  if (
    text.startsWith('[子代理') ||
    text.startsWith('[代理 ') ||
    text.startsWith('[后台任务')
  ) {
    return { title: text.split('\n')[0], body: text }
  }
  if (text.startsWith('[技能正文 ')) {
    const head = text.split('\n')[0] ?? ''
    const name = head.slice('[技能正文 '.length).split('（')[0]?.trim() || '…'
    return {
      title: t('injectionSkillBody', { name }),
      body: text.slice(head.length + 1),
    }
  }
  if (text.startsWith('[denia 能力上下文]')) {
    return {
      title: t('injectionCapability'),
      body: text.split('\n').slice(1).join('\n'),
    }
  }
  if (text.startsWith('Current runtime context.')) {
    return {
      title: t('injectionRuntimeSnapshot'),
      body: text.split('\n').slice(1).join('\n'),
    }
  }
  if (text.startsWith(REMINDER_OPEN)) {
    const inner = text.endsWith(REMINDER_CLOSE)
      ? text.slice(REMINDER_OPEN.length, -REMINDER_CLOSE.length - 1)
      : text.slice(REMINDER_OPEN.length)
    const nl = inner.indexOf('\n')
    const head = nl < 0 ? inner : inner.slice(0, nl)
    const body = nl < 0 ? '' : inner.slice(nl + 1).replace(/^\n+/, '')
    if (head.startsWith('工作区指令:')) {
      return {
        title: t(head.includes('取代') ? 'injectionWorkspaceUpdate' : 'injectionWorkspace'),
        body,
      }
    }
    if (head.startsWith('技能目录:')) {
      // <available_skills> 帧是给模型的;对话流里直接展示技能行列表。
      const lines = body
        .replace('<available_skills>\n', '')
        .replace('\n</available_skills>', '')
      return {
        title: t(head.includes('取代') ? 'injectionSkillCatalogUpdate' : 'injectionSkillCatalog'),
        body: lines,
      }
    }
    if (head.startsWith('系统提示更新:')) {
      // system 字节冻结后,变化的新提示词全文经此通道追加(缓存前缀不变)。
      return { title: t('injectionSystemPromptUpdate'), body }
    }
    return { title: t('contextInjectionTitle'), body: inner }
  }
  return { title: t('contextInjectionTitle'), body: text }
}

/**
 * agent-delivery 的两种方向在 UI 上截然不同:
 *   - 主代理 → 本会话(父代理委派任务 / 发来消息):这是**对话**,
 *     渲染成右对齐的用户消息气泡(与用户自己说的话同一套组件/样式),
 *     角标标明来自主代理。
 *   - 子代理 → 主代理(执行结束汇报 / 后台任务完成 / 其它代理来信):
 *     那是**系统反馈**,继续走折叠行。
 *
 * 判据是 fold 带过来的 `fromParentAgent`(由 source 与会话头父 id 比对得出,
 * 见 fold.ts 的 deliveryProjection),不在这里重新猜正文。
 */
/**
 * 父代理来信的首行(通道头):`[父代理 <会话 id> 委派任务]` /
 * `[代理 <会话 id> 发来消息]`。这一行是给模型看的 —— 会话 id 对读日志的人
 * 毫无意义,界面上由"来自主代理"角标承担,正文从第二行开始。
 *
 * 只在方向已判定为父代理来信时调用;其余通道的首行照旧当标题用。
 */
export function parentAgentBody(text: string): string {
  const nl = text.indexOf('\n')
  const head = nl < 0 ? text : text.slice(0, nl)
  if (!head.startsWith('[父代理') && !head.startsWith('[代理 ')) return text
  return nl < 0 ? '' : text.slice(nl + 1).replace(/^\n+/, '')
}

export function ContextInjectionRow({
  text,
  images,
  fromParentAgent,
}: {
  text: string
  images?: UserMessageImage[]
  fromParentAgent?: boolean
}) {
  if (fromParentAgent) {
    const body = parentAgentBody(text)
    return (
      <div className="user-row agent-parent-row">
        <div className="user-message-stack msg-copy-anchor">
          <div className="agent-parent-head">
            <IconAgentList size={13} />
            <span>{t('injectionFromParentAgent')}</span>
          </div>
          <div className="msg-user">{body}</div>
          <div className="message-actions">
            <CopyMessageButton text={body} />
          </div>
        </div>
      </div>
    )
  }
  if (images && images.length > 0) {
    const display = visualInjectionDisplay(text, images)
    return (
      <div className="disc-row inject-row visual-inject-row">
        <div className="disc-head visual-inject-head">
          <span className="glyph">
            <IconImage size={14} />
          </span>
          <span className="title">{display.title}</span>
          {display.detail && (
            <>
              <span className="sep" />
              <span className="summary inject-summary">{display.detail}</span>
            </>
          )}
        </div>
        <div className="disc-body visual-inject-body">
          <UserMessageImages images={images} variant="visual" />
        </div>
      </div>
    )
  }
  const display = injectionDisplay(text)
  return <DisclosureRow title={display.title} icon={<IconTool size={14} />} text={display.body} />
}
