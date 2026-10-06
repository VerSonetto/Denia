/**
 * 子代理重构的前端回归网。
 *   node scripts/check-subagents.mjs
 *
 * 两段：
 * A) 纯逻辑行为（编辑器草稿规则：空列表≠全部、复制不共享引用、保存以服务端
 *    返回为准、冲突保留草稿）；
 * B) 契约/清理断言（旧字段彻底退出 UI 与 i18n、新页面接到全部 API、Rust 侧
 *    schema 不再暴露 max_depth、路由与文档一致、生成的 wire 类型包含快照字段）。
 *
 * B 段直接读源码：这些断言的价值在于"回归时立刻失败"，而不是复述实现。
 */
import { readFileSync, mkdtempSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { build } from 'esbuild'

const root = fileURLToPath(new URL('..', import.meta.url))
const repo = fileURLToPath(new URL('../..', import.meta.url))

let failed = 0
function check(label, actual, expected) {
  const a = JSON.stringify(actual)
  const e = JSON.stringify(expected)
  if (a !== e) {
    failed++
    console.log(`FAIL ${label}\n  actual   ${a}\n  expected ${e}`)
  } else {
    console.log(`ok   ${label}`)
  }
}
function assert(label, condition, detail = '') {
  if (condition) console.log(`ok   ${label}`)
  else {
    failed++
    console.log(`FAIL ${label}${detail ? `\n  ${detail}` : ''}`)
  }
}

/* ---------- A) 纯逻辑 ---------- */
const dir = mkdtempSync(join(tmpdir(), 'subagents-'))
const bundle = join(dir, 'subagentDraft.mjs')
await build({
  entryPoints: [join(root, 'src/components/settings/subagentDraft.ts')],
  outfile: bundle,
  bundle: true,
  format: 'esm',
  platform: 'node',
  target: 'node20',
  logLevel: 'silent',
})
const draft = await import(pathToFileURL(bundle).href)

const labels = { inherit: '继承父工具', none: '无工具', count: (n) => `${n} 个工具` }

const blank = draft.blankProfile()
check('新建默认显式空列表（不是 inherit）', blank.tools, { mode: 'allowlist', names: [] })
check('空列表摘要 = 无工具', draft.toolSelectionSummary(blank.tools, labels), '无工具')
check(
  'inherit 摘要 = 继承父工具（与空列表不互相转换）',
  draft.toolSelectionSummary({ mode: 'inherit' }, labels),
  '继承父工具',
)
check('非空列表摘要计数', draft.toolSelectionSummary({ mode: 'allowlist', names: ['a', 'b'] }, labels), '2 个工具')

check('勾选工具稳定排序', draft.toggleToolName(['read_file'], 'bash'), ['bash', 'read_file'])
check('取消勾选', draft.toggleToolName(['bash', 'read_file'], 'bash'), ['read_file'])
check('未知工具不会凭空加入', draft.toggleToolName([], 'nope'), ['nope'])

const view = {
  qualifiedId: 'builtin:explore',
  source: 'builtin',
  revision: 7,
  editable: true,
  overridesBuiltin: false,
  projectWritable: false,
  diagnostics: [],
  profile: {
    schemaVersion: 1,
    id: 'explore',
    name: '只读探索',
    description: '调查',
    instructions: '不修改文件',
    enabled: true,
    tools: { mode: 'allowlist', names: ['read_file'] },
    model: { mode: 'inherit' },
    permissionCeiling: 'read-only',
  },
}

const copy = draft.copyProfile(view)
check('复制得到新 id', copy.id, 'explore-copy')
check('复制得到新名称', copy.name, '只读探索 副本')
check('复制不共享 tools 引用', copy.tools === view.profile.tools, false)
copy.tools.names.push('bash')
check('改副本不影响原定义', view.profile.tools.names, ['read_file'])

const saved = draft.draftAfterSave('user', { ...view, qualifiedId: 'user:explore', source: 'user', revision: 8 })
check('保存后 target 跟随服务端返回', saved.target, 'user:explore')
check('保存后 revision 跟随服务端返回', saved.revision, 8)
check('内置编辑落到用户覆盖层', draft.draftAfterSave('user', view).writeScope, 'user')

const kept = draft.conflictKeepsDraft({ profile: { name: '我的草稿' } }, 409)
check('冲突时草稿原样保留', kept.draft.profile.name, '我的草稿')
check('409 判定为冲突', kept.conflict, true)
check('非 409 不是冲突', draft.conflictKeepsDraft({}, 400).conflict, false)

check('硬禁用行不可选', draft.rowIsDisabled({ hardDenied: true }), true)
check('普通行可选', draft.rowIsDisabled({ hardDenied: false }), false)

const rows = [
  { name: 'read_file', source: 'builtin', category: '读取', effect: '', readOnlyDenied: false, granted: true, grantable: true, hardDenied: false, reason: '' },
  { name: 'bash', source: 'builtin', category: '命令与后台任务', effect: '', readOnlyDenied: true, granted: true, grantable: true, hardDenied: false, reason: '' },
  { name: 'mcp__fs__write', source: 'mcp', category: 'MCP', effect: '', readOnlyDenied: false, granted: null, grantable: true, hardDenied: false, reason: '' },
]
check('未知工具被点名（离线 MCP 不静默丢）', draft.unknownToolNames(['read_file', 'mcp__off__x'], rows), ['mcp__off__x'])
check('全部已知时无未知项', draft.unknownToolNames(['bash'], rows), [])
check('只读上限下写/命令类算冲突', draft.readOnlyConflicts('read-only', ['read_file', 'bash'], rows), ['bash'])
check('继承上限下不算冲突', draft.readOnlyConflicts('inherit', ['bash'], rows), [])
check('未注册工具在只读上限下也按冲突提示', draft.readOnlyConflicts('read-only', ['gone'], rows), ['gone'])
check(
  '只读上限不误伤 send_message（判定只认服务端 readOnlyDenied）',
  draft.readOnlyConflicts('read-only', ['send_message'], [
    ...rows,
    { name: 'send_message', source: 'builtin', category: '代理派遣', effect: '', readOnlyDenied: false, granted: true, grantable: true, hardDenied: false, reason: '' },
  ]),
  [],
)
check('allowlist 取值', draft.allowlistOf({ tools: { mode: 'allowlist', names: ['a'] } }), ['a'])
check('inherit 无 allowlist', draft.allowlistOf({ tools: { mode: 'inherit' } }), null)

/* ---------- A2) 子代理身份摘要（父子两侧共用） ---------- */
const display = await (async () => {
  const out = join(dir, 'sessionDisplay.mjs')
  await build({
    entryPoints: [join(root, 'src/sessionDisplay.ts')],
    outfile: out,
    bundle: true,
    format: 'esm',
    platform: 'node',
    target: 'node20',
    logLevel: 'silent',
  })
  return import(pathToFileURL(out).href)
})()

const modern = display.subagentSnapshotText({
  snapshotVersion: 1,
  label: '开发子代理',
  name: '开发子代理',
  depth: 1,
  mode: 'spawn',
  selection: { provider: 'p', model: 'm' },
  profile: { qualifiedId: 'builtin:develop', revision: 3, inline: false },
  effectiveTools: ['read_file', 'bash', 'write_file'],
  permissionCeiling: 'inherit',
  delegationAllowed: false,
  instructionScope: 'project-only',
})
assert('摘要带名称', modern.includes('开发子代理'), modern)
assert('摘要带定义 qualifiedId', modern.includes('builtin:develop'), modern)
assert('摘要带冻结工具数', modern.includes('3 个工具'), modern)
assert('摘要带权限上限', modern.includes('跟随父权限'), modern)
assert('摘要带仅项目级指令', modern.includes('仅项目级指令'), modern)
assert('摘要写明不能派遣', modern.includes('否（运行时硬规则）'), modern)

const legacy = display.subagentSnapshotText({
  snapshotVersion: 0,
  label: 'worker',
  depth: 1,
  mode: 'default',
  selection: { provider: 'p', model: 'm' },
  permissionCeiling: 'inherit',
  delegationAllowed: false,
})
assert('旧描述符标成保守只读授权', legacy.includes('旧子代理'), legacy)
assert('旧描述符不假装有新定义', !legacy.includes('inline'), legacy)
check('空描述符返回空串', display.subagentSnapshotText(null), '')

/* ---------- B) 契约与清理 ---------- */
const read = (relative) => readFileSync(join(repo, relative), 'utf8')

const runtimeSettings = read('web/src/components/settings/RuntimeSettings.tsx')
assert(
  'RuntimeSettings 的字段表不再含 maxAgents',
  !runtimeSettings.includes("['maxAgents'"),
  '字段表里仍有 maxAgents 输入框',
)
assert(
  'RuntimeSettings 的字段表不再含 maxDepth',
  !runtimeSettings.includes("['maxDepth'"),
  '字段表里仍有 maxDepth 输入框',
)
assert('RuntimeSettings 主动剔除废弃字段', runtimeSettings.includes('delete next.maxAgents'))

const i18n = read('web/src/i18n.ts')
assert('i18n 不再定义 runtimeMaxAgents', !i18n.includes('runtimeMaxAgents'))
assert('i18n 不再定义 runtimeMaxDepth', !i18n.includes('runtimeMaxDepth'))
assert('任务页改名为「任务与运行时」', i18n.includes('runtimeSettings: "任务与运行时"'))
assert('子代理页与调度文案存在', i18n.includes('subagentsTab: "子代理"') && i18n.includes('subagentsMaxConcurrentRuns'))
assert('英文键集同步', i18n.includes('subagentsTab: "Subagents"'))

const settings = read('web/src/components/settings/SubagentSettings.tsx')
for (const fn of [
  'api.listSubagentProfiles',
  'api.listSubagentTools',
  'api.createSubagentProfile',
  'api.updateSubagentProfile',
  'api.deleteSubagentProfile',
  'api.resetSubagentProfile',
  'api.previewSubagentProfile',
]) {
  assert(`新页面接入 ${fn}`, settings.includes(fn))
}
assert('新页面固定说明继承语义', settings.includes("t('subagentsInheritNote')"))
assert('新页面固定说明禁止二次派遣', settings.includes("t('subagentsNoDispatchNote')"))
assert('没有全局 AGENTS.md 开关', !settings.toLowerCase().includes('includeglobal'))
assert('没有深度开关', !settings.includes('maxDepth') && !settings.includes('max_depth'))

const modal = read('web/src/components/SettingsModal.tsx')
assert('设置弹窗挂载子代理页', modal.includes('<SubagentSettings notify={notify} />'))
assert('设置弹窗注册子代理标签', modal.includes("| 'subagents'"))

// Rust 侧：模型参数契约不再暴露 max_depth，且兼容解析给出稳定 code。
const capabilities = read('crates/tools/src/capabilities.rs')
assert('spawn/fork schema 不再暴露 max_depth 字段', !capabilities.includes('"max_depth": {"type"'))
assert('max_depth 命中时给稳定 code', capabilities.includes('subagent/depth-config-removed'))
assert('schema 描述说明可派遣类型与 inline', capabilities.includes('profile_id') && capabilities.includes('inline'))

// Rust 侧：路由与错误语义。
const routes = read('crates/server/src/api/subagents.rs')
for (const path of [
  '/api/subagent-profiles',
  '/api/subagent-profiles/preview',
  '/api/subagent-tools',
  '/subagent-profiles/{qualified_id}',
  '/reset',
]) {
  assert(`路由存在 ${path}`, routes.includes(path))
}
assert('project 作用域不接受任意 filePath', !routes.includes('filePath'))

// 生成的 wire 类型包含运行快照字段（bindings 漂移守卫）。
const generated = read('web/src/generated/core.ts')
for (const name of [
  'SubagentProfile',
  'SubagentProfileView',
  'SubagentInlineSpec',
  'ToolSelection',
  'ModelChoice',
  'PermissionCeiling',
  'ProfileSource',
  'ProfileWriteScope',
  'SubagentProfileRef',
  'SubagentForkProjection',
  'LegacySubagentInfo',
]) {
  assert(`生成类型包含 ${name}`, generated.includes(`export type ${name} `))
}
assert(
  'SubagentDescriptor 携带冻结授权与审计字段',
  generated.includes('effectiveTools?: Array<string>') &&
    generated.includes('instructionScope?: string') &&
    generated.includes('delegationAllowed: boolean'),
)

// 运行观察：父侧面板与子会话横幅必须展示同一份运行快照事实。
const runtimePanel = read('web/src/components/RuntimePanel.tsx')
for (const needle of [
  'profileLabel(',
  'pendingApprovals',
  'pendingAsks',
  'waitingLabel(',
  't(\'runtimeSnapshot\')',
  't(\'runtimeOpenChild\')',
  'agent.result',
  'instructionScope',
  'tools',
]) {
  assert(`RuntimePanel 展示 ${needle}`, runtimePanel.includes(needle))
}
assert('RuntimePanel 不做本地授权推论', runtimePanel.includes('delegationAllowed'))
const sessionsPage = read('web/src/pages/SessionsPage.tsx')
assert('子会话横幅使用共享的身份摘要', sessionsPage.includes('subagentSnapshotText'))
assert('子会话横幅渲染摘要', sessionsPage.includes('child-snapshot'))
assert(
  '列表负载来自服务端快照',
  read('crates/server/src/agent_runtime.rs').includes('"waitingInteraction"'),
)

// 未决交互的加载期收敛（重启后不得留下"可提交但必然失败"的卡片）。
const recovery = read('crates/session/src/recovery.rs')
assert('加载时收敛未决交互', recovery.includes('close_orphaned_interactions'))
assert('收敛写入 unavailable 结局', recovery.includes('AskOutcome::Unavailable'))
assert('收敛覆盖审批', recovery.includes('ApprovalOutcome::Unavailable'))

// 后台命令必须与 job_start 同授权。
const execSource = read('crates/agent-loop/src/exec.rs')
assert('执行器拒绝无授权的后台命令', execSource.includes('bash_requests_background'))
assert('会话有效工具面统一判定', read('crates/agent-loop/src/lib.rs').includes('session_tool_face'))

// 派遣崩溃窗口有 fault-injection 覆盖。
const runtimeSource = read('crates/server/src/agent_runtime.rs')
for (const point of ['"create"', '"snapshot"', '"pre-enqueue"', '"enqueue"', '"post-enqueue"']) {
  assert(`故障注入点 ${point}`, runtimeSource.includes(`test_fault(${point})`))
}

// 缺口 6：工具目录的副作用说明与只读冲突约束。
assert('工具目录带副作用说明', routes.includes('"effect": effect_note('))
assert('工具目录带只读拒绝标记', routes.includes('"readOnlyDenied"'))
assert(
  '只读拒绝判定与执行面同源',
  routes.includes('crate::subagents::resolver::ceiling_denied_tool'),
)
assert(
  '副作用说明按具体工具细化（不拿写文件文案套待办）',
  routes.includes('"todo_write" => "只更新本会话的待办清单'),
)
assert(
  '编辑器删掉按能力猜的只读兼容字段',
  !settings.includes('readOnlyCompatible') && !read('web/src/types.ts').includes('readOnlyCompatible'),
)
assert(
  '编辑器拦住只读冲突而不是偷偷放宽',
  settings.includes('readOnlyConflicts(') && settings.includes('subagentsUseInheritCeiling'),
)
assert('编辑器显式给出两种解法', settings.includes('subagentsDropConflicts'))
assert('离线工具在编辑器里可见可移除', settings.includes('unknownToolNames(') && settings.includes('data-testid="unknown-tools"'))
assert(
  '保存前拦截冲突',
  settings.includes("t('subagentsReadOnlyConflict')") && settings.includes('if (conflicts.length > 0)'),
)
assert(
  '目录预算独立可诊断',
  read('crates/server/src/agent_runtime.rs').includes('subagent_catalog_max_bytes'),
)

// 缺口 7：浏览器 tab 按 child 归属，只清理该 child 自己的资源。
const manager = read('crates/browser/src/manager.rs')
assert('浏览器维护 tab 归属表', manager.includes('tab_owners') && manager.includes('pub struct TabOwners'))
assert('按归属关闭 tab', manager.includes('pub async fn close_owned'))
assert('带归属执行命令', manager.includes('pub async fn execute_owned'))
assert(
  '浏览器工具把会话身份当归属',
  read('crates/tools/src/browser.rs').includes('execute_owned(command, owner.as_deref())'),
)
const runtimeForBrowser = read('crates/server/src/agent_runtime.rs')
assert('停止 child 时清理其 tab', runtimeForBrowser.includes('close_owned_tabs(target)'))
assert('结束 child 时清理其 tab', runtimeForBrowser.includes('hub.close_owned(&current)'))
assert('删除会话时清理其 tab', runtimeForBrowser.includes('close_owned_tabs(&child.id)'))
assert('装配处挂载浏览器中枢', read('crates/server/src/state.rs').includes('attach_browser'))

// 缺口 8：旧 child 的模型历史投影（可继续，且不把全局规则继续送模型）。
const historySource = read('crates/session/src/history.rs')
assert('投影按事件类别决策', historySource.includes('pub fn legacy_subagent_drop_set'))
assert('投影持久化类型', historySource.includes('pub struct HistoryProjection'))
assert('续跑前建立投影', runtimeForBrowser.includes('apply_history_projection'))
assert('诊断改报投影状态', runtimeForBrowser.includes('"projection":'))
assert(
  '模型面走投影视图',
  read('crates/session/src/projection.rs').includes('history_projection.clone()'),
)
assert(
  '注入基线走模型面视图',
  read('crates/agent-loop/src/injections.rs').includes('with_model_events'),
)

// 调度上限的唯一来源是 subagent-policy。
const policy = read('crates/server/src/subagents/policy.rs')
assert('调度命名空间唯一持有并发上限', policy.includes('max_concurrent_runs'))
assert(
  '配置结构里没有深度字段',
  !policy.includes('pub max_depth') && !policy.includes('pub maxDepth'),
  '策略结构仍然声明了深度配置字段',
)
assert('非法深度字段被拒绝（有回归用例）', policy.includes('"maxDepth": 3'))

console.log(failed === 0 ? '\nALL PASS' : `\n${failed} FAILED`)
process.exit(failed === 0 ? 0 : 1)
