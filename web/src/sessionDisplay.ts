import { t } from './i18n'
import { reasoningEffortLabel } from './reasoningEffort'
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
 * 派遣快照的读法（清单与一行摘要共用一份解释）。
 *
 * 只读快照里的事实，不做推论：`effectiveTools` 缺失说明这是旧描述符，
 * 此时明确标成"旧子代理（保守只读授权）"，而不是假装它是新定义。
 * 模型取 `selection`——派遣时冻结的那一个，与父会话当前档位无关。
 */
function readSnapshot(descriptor: SubagentDescriptor) {
  const legacy = (descriptor.snapshotVersion ?? 0) === 0 || !descriptor.effectiveTools
  const selection = descriptor.selection
  const model = [selection?.provider, selection?.model].filter(Boolean).join('/')
  return {
    legacy,
    name: subagentDisplayName(descriptor),
    profile: descriptor.profile?.qualifiedId ?? (legacy ? t('runtimeProfileUnknown') : 'inline'),
    inline: descriptor.profile?.inline === true,
    model,
    modelEffort: selection?.reasoningEffort ? reasoningEffortLabel(selection.reasoningEffort) : '',
    tools: legacy
      ? t('runtimeLegacyTools')
      : t('subagentsToolsCount').replace('{n}', String(descriptor.effectiveTools?.length ?? 0)),
    toolNames: legacy ? '' : (descriptor.effectiveTools ?? []).join('、'),
    ceiling:
      descriptor.permissionCeiling === 'read-only'
        ? t('subagentsCeilingReadOnly')
        : t('subagentsCeilingInherit'),
    createdMode: descriptor.createdPermissionMode ?? '',
    scope: descriptor.instructionScope === 'project-only' ? t('runtimeProjectOnly') : '',
  }
}

/** 子代理会话的显示名（横幅标题与摘要首项都是它）。 */
export function subagentDisplayName(descriptor: SubagentDescriptor | null | undefined): string {
  if (!descriptor) return ''
  return descriptor.name?.trim() || descriptor.label
}

/**
 * 派遣时冻结的模型（`provider/model`）——横幅标题上"用了哪个模型"的取值。
 *
 * 取快照而不是父会话当前档位：子代理跑的是它自己那份冻结的选择。
 */
export function subagentModelLabel(
  descriptor: SubagentDescriptor | null | undefined,
): string {
  if (!descriptor) return ''
  const selection = descriptor.selection
  return [selection?.provider, selection?.model].filter(Boolean).join('/')
}

/**
 * 横幅上的一项元信息。
 *
 * `value` 必须是**自解释的短句**（"跟随父权限"、"12 个工具"），横幅里不画
 * "标签：值"的前缀——会话界面读的是一句人话，不是表单。`label` 只留在
 * 无障碍名与悬停细节里，`hint` 承载放不下的细节（工具名单、创建时权限、
 * 推理强度）。
 */
export interface SubagentFact {
  key: 'tools' | 'ceiling' | 'profile' | 'scope'
  label: string
  value: string
  hint?: string
}

/**
 * 子代理会话横幅的元信息清单（名称走标题，不重复列）。
 *
 * 按"最想知道"排序：能干什么（工具与权限）在前，身份与指令范围在后。
 * 模型不进清单——它已经在横幅标题上（"子智能体：provider/model"），
 * 重复一遍只会把一行能说完的事拆成两处。"不能再派遣"这类审计事实也
 * 不进清单（它读起来必然带标签），留在 [`subagentSnapshotText`] 里。
 */
export function subagentSnapshotFacts(
  descriptor: SubagentDescriptor | null | undefined,
): SubagentFact[] {
  if (!descriptor) return []
  const snapshot = readSnapshot(descriptor)
  const facts: SubagentFact[] = [
    {
      key: 'tools',
      label: t('childTools'),
      value: snapshot.tools,
      hint: snapshot.toolNames || undefined,
    },
    {
      key: 'ceiling',
      label: t('subagentsCeilingTitle'),
      value: snapshot.ceiling,
      hint: snapshot.createdMode ? `${t('runtimeCreatedMode')} ${snapshot.createdMode}` : undefined,
    },
    {
      key: 'profile',
      label: t('runtimeProfile'),
      value: snapshot.inline
        ? `${snapshot.profile} · ${t('runtimeProfileInline')}`
        : snapshot.profile,
    },
  ]
  if (snapshot.scope) {
    facts.push({ key: 'scope', label: t('runtimeInstructionScope'), value: snapshot.scope })
  }
  return facts
}

/**
 * 子代理会话的一行身份摘要（父子两侧共用；横幅拿它当悬停全文）。
 *
 * 与 [`subagentSnapshotFacts`] 同源：横幅只摆自解释的短句，这里保留
 * 完整事实（含推理强度与"不能再派遣"）。
 */
export function subagentSnapshotText(
  descriptor: SubagentDescriptor | null | undefined,
): string {
  if (!descriptor) return ''
  const snapshot = readSnapshot(descriptor)
  return [
    snapshot.name,
    snapshot.profile,
    [snapshot.model, snapshot.modelEffort].filter(Boolean).join(' · '),
    snapshot.tools,
    snapshot.ceiling,
    snapshot.scope,
    t('runtimeDelegationDenied'),
  ]
    .filter(Boolean)
    .join(' · ')
}
