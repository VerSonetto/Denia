import { t } from './i18n'
import type { SessionSummary, SubagentDescriptor } from './types'

/** Sidebar/header label: AI-generated title, then first-prompt excerpt, then blank. */
export function sessionDisplayTitle(session: SessionSummary): string {
  const title = session.title?.trim()
  if (title) return title
  const excerpt = session.excerpt?.trim()
  if (excerpt) return excerpt
  return t('blankSession')
}

/**
 * 子代理会话的身份摘要（父子两侧共用）：名称、定义、冻结工具数、权限上限、
 * 指令范围与"不能再派遣"。
 *
 * 只读运行快照的事实，不做推论：`effectiveTools` 缺失说明这是旧描述符，
 * 此时明确标成"旧子代理（保守只读授权）"，而不是假装它是新定义。
 */
export function subagentSnapshotText(
  descriptor: SubagentDescriptor | null | undefined,
): string {
  if (!descriptor) return ''
  const legacy = (descriptor.snapshotVersion ?? 0) === 0 || !descriptor.effectiveTools
  const qualified =
    descriptor.profile?.qualifiedId ?? (legacy ? t('runtimeProfileUnknown') : 'inline')
  const name = descriptor.name?.trim() || descriptor.label
  const tools = legacy
    ? t('runtimeLegacyTools')
    : t('subagentsToolsCount').replace('{n}', String(descriptor.effectiveTools?.length ?? 0))
  const ceiling =
    descriptor.permissionCeiling === 'read-only'
      ? t('subagentsCeilingReadOnly')
      : t('subagentsCeilingInherit')
  const scope = descriptor.instructionScope === 'project-only' ? t('runtimeProjectOnly') : ''
  return [name, qualified, tools, ceiling, scope, t('runtimeDelegationDenied')]
    .filter(Boolean)
    .join(' · ')
}
