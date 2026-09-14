import { t } from '../i18n'
import type { AgentPresetRow } from '../types'
import { IconAgentPreset } from './icons'

/**
 * 会话头部的组装模式标签(只读)。
 *
 * 只读是刻意的:会话一旦开始工作,组装就固定下来(服务端对已产出内容的会话
 * 返回 `agent-preset/locked`)。在头部放控件等于承诺一个 host 会拒绝的开关,
 * 如实说出"这个会话跑的是什么"才是诚实的做法 —— 选择入口留在新会话页那一行。
 *
 * 视觉上并进标题行、不加底色与边框:组装是会话身份的一部分,和标题同源,
 * 读作「标题 · 组装模式」,而不是与标题并列的第二个方块。
 */
export function SessionPresetLabel({
  presetId,
  presets,
}: {
  /** 会话实际运行的组装 id;缺省值已由调用方回落到部署默认值。 */
  presetId: string
  /** 组装名册;名册尚未拉回时为空,此时退化为直接显示 id(仍比留白诚实)。 */
  presets: AgentPresetRow[]
}) {
  const active = presets.find((row) => row.id === presetId)
  // description 允许是空串:兜底一句通用说明,不让 title 变成空白。
  const hint = active?.description.trim() || t('sessionPresetHint')
  return (
    <span className="session-preset" title={hint}>
      <IconAgentPreset size={13} />
      <span className="session-preset-name">{active?.name ?? presetId}</span>
    </span>
  )
}