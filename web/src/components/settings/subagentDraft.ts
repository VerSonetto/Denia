/**
 * 子代理定义编辑器里**可测的纯逻辑**。
 *
 * 组件只负责渲染与请求；下面这些决定（空列表不是"全部"、复制不共享引用、
 * 保存后以服务端返回为准、工具勾选稳定排序）抽出来单独测，避免把这些
 * 约定埋进 JSX 里靠人眼审查。
 */
import type {
  ProfileWriteScope,
  SubagentProfile,
  SubagentProfileView,
  SubagentToolRow,
  ToolSelection,
} from '../../types'

/** 新建草稿：默认显式空列表（零业务工具），不会意外继承父的全部能力。 */
export function blankProfile(): SubagentProfile {
  return {
    schemaVersion: 1,
    id: '',
    name: '',
    description: '',
    instructions: '',
    enabled: true,
    tools: { mode: 'allowlist', names: [] },
    model: { mode: 'inherit' },
    permissionCeiling: 'inherit',
  }
}

/** 深拷贝一份定义（复制/编辑都不与原对象共享 tools/model 引用）。 */
export function cloneProfile(view: SubagentProfileView): SubagentProfile {
  return {
    ...view.profile,
    tools: JSON.parse(JSON.stringify(view.profile.tools)) as ToolSelection,
    model: JSON.parse(JSON.stringify(view.profile.model)),
  }
}

/**
 * 复制为一份新定义：工具的显式集合一致，但**身份是新 id**。
 * 空 id 保持空（由用户填），避免生成非法 id。
 */
export function copyProfile(view: SubagentProfileView): SubagentProfile {
  const profile = cloneProfile(view)
  profile.id = profile.id ? `${profile.id}-copy` : ''
  profile.name = `${profile.name} 副本`
  return profile
}

/** 勾选/取消一个工具：稳定排序，便于保存后与 revision 比较一致。 */
export function toggleToolName(names: string[], name: string): string[] {
  const next = names.includes(name) ? names.filter((item) => item !== name) : [...names, name]
  next.sort()
  return next
}

/**
 * 工具摘要（渲染与断言共用）。
 * `inherit` 与"空列表"必须给出**不同**的文案：前者继承父能力，后者是零工具。
 */
export function toolSelectionSummary(
  tools: ToolSelection,
  labels: { inherit: string; none: string; count: (n: number) => string },
): string {
  if (tools.mode === 'inherit') return labels.inherit
  return tools.names.length === 0 ? labels.none : labels.count(tools.names.length)
}

/** 工具行是否不可选（子代理硬禁用）。 */
export function rowIsDisabled(row: SubagentToolRow): boolean {
  return row.hardDenied === true
}

/** 勾选的 allowlist（`inherit` 时为 null）。 */
export function allowlistOf(profile: SubagentProfile): string[] | null {
  return profile.tools.mode === 'allowlist' ? profile.tools.names : null
}

/**
 * 定义里有、但当前部署注册表里没有的工具名。
 *
 * 这些名字**不能**被静默丢掉（多半是离线 MCP 或已卸载的工具）：保留原值并
 * 明确诊断，由用户决定移除还是等服务器恢复。
 */
export function unknownToolNames(names: string[], rows: SubagentToolRow[]): string[] {
  const known = new Set(rows.map((row) => row.name))
  return names.filter((name) => !known.has(name))
}

/**
 * 只读上限与工具选择的冲突项。
 *
 * 判定与执行面同源：服务端按 `ceiling_denied_tool` 给出 `readOnlyDenied`
 * （只读档拒绝 `write_file`/`edit`/`bash`/`job_start`/`todo_write`），UI 只做
 * 展示与拦截，不自己按能力分类猜——否则会拦下只读档本来可用的
 * `send_message`/`ask`。
 *
 * 冲突不由系统自动修：要么显式改为 `inherit` 上限，要么移除这些工具
 * （计划 7：不偷偷解除只读上限）。
 */
export function readOnlyConflicts(
  ceiling: SubagentProfile['permissionCeiling'],
  names: string[],
  rows: SubagentToolRow[],
): string[] {
  if (ceiling !== 'read-only') return []
  const byName = new Map(rows.map((row) => [row.name, row]))
  return names.filter((name) => {
    const row = byName.get(name)
    // 未注册的工具在只读上限下也无法验证副作用，一并按冲突提示。
    return row === undefined || row.readOnlyDenied === true
  })
}

/** 保存后的草稿：revision 与服务端返回对齐，target 指向有效限定 id。 */
export function draftAfterSave(
  writeScope: ProfileWriteScope,
  view: SubagentProfileView,
): { target: string; writeScope: ProfileWriteScope; revision: number; profile: SubagentProfile } {
  return {
    target: view.qualifiedId,
    // 内置定义编辑后落到用户覆盖层：写入作用域跟服务端返回的 source 走。
    writeScope: view.source === 'project' ? 'project' : writeScope === 'project' ? 'project' : 'user',
    revision: view.revision,
    profile: cloneProfile(view),
  }
}

/**
 * 保存冲突时的处理：**保留草稿**，只把冲突事实交回界面。
 * 返回值不是新草稿——调用方必须原样保留用户已输入的内容。
 */
export function conflictKeepsDraft<T>(draft: T, status: number): { draft: T; conflict: boolean } {
  return { draft, conflict: status === 409 }
}
