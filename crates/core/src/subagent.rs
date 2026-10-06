//! 子代理定义（SubagentProfile）与运行快照的共享词汇表。
//!
//! 这里是协议权威：磁盘上的 frontmatter、HTTP API 的 JSON 与派发参数都
//! 由这些类型序列化而来。三条不变量在类型层就固定下来：
//!
//! 1. **定义与运行事实分离**：定义只说"要什么"，实际给了什么由派遣时算出
//!    的 [`ChildExecutionSnapshot`] 记录，编辑定义不热改已有子代理。
//! 2. **收窄而非扩权**：`tools` 的 `inherit` 表示继承父代理可授予集合，
//!    `allowlist` 的空数组表示零业务工具——两者绝不互相解释。
//! 3. **禁止嵌套是硬规则**：快照里 `delegation_allowed` 永远是 `false`，
//!    它只是审计信息，真正的拒绝发生在运行时按会话身份判定。

use serde::{Deserialize, Serialize};

use crate::config::ModelSelection;

/// 定义文件与 API 的格式版本；不认识的版本直接报错，不做宽松解释。
pub const SUBAGENT_SCHEMA_VERSION: u32 = 1;

/// 运行快照格式版本（定义版本与快照版本各自演进）。
pub const SUBAGENT_SNAPSHOT_VERSION: u32 = 1;

/// 角色补充提示的字节上限（UTF-8，64 KiB）。
pub const SUBAGENT_INSTRUCTIONS_MAX_BYTES: usize = 64 * 1024;

/// 显示名长度上限（Unicode 字符）。
pub const SUBAGENT_NAME_MAX_CHARS: usize = 80;

/// 用途描述长度上限（Unicode 字符）。
pub const SUBAGENT_DESCRIPTION_MAX_CHARS: usize = 2000;

/// 定义 id 的语法：小写字母数字起头，后接小写字母数字与连字符。
pub fn is_valid_subagent_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() || first.is_ascii_digit() => {}
        _ => return false,
    }
    id.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// 定义的来源作用域。合并顺序 builtin → user → project，同 id 后者覆盖前者。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "lowercase")]
pub enum SubagentProfileSource {
    Builtin,
    User,
    Project,
}

impl SubagentProfileSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
        }
    }

    /// 该作用域的定义是否可写：内置定义写的是用户覆盖，项目覆盖只写项目目录。
    pub fn editable(&self) -> bool {
        matches!(self, Self::User | Self::Project)
    }

    /// qualifiedId 前缀（`builtin:explore` / `user:x` / `project:x`）。
    pub fn prefix(&self) -> &'static str {
        self.as_str()
    }
}

/// 工具选择：继承父代理可授予集合，或给出显式白名单。
///
/// 空白名单表示"零业务工具"（纯推理任务），**不**解释为全部工具。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ToolChoice {
    Inherit,
    Allowlist { names: Vec<String> },
}

impl ToolChoice {
    /// 解析成具体工具名；`inherit` 交给调用方用父可授予集合代入。
    pub fn allowlist(&self) -> Option<&[String]> {
        match self {
            Self::Inherit => None,
            Self::Allowlist { names } => Some(names),
        }
    }
}

/// 模型选择：继承父代理当前模型，或显式成套指定 provider/model/effort。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ModelChoice {
    Inherit,
    Explicit { selection: ModelSelection },
}

/// 权限上限：只能相对父代理收窄，没有绕过审批的档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum PermissionCeiling {
    /// 继承派遣时父代理的有效权限，并随之收紧。
    #[default]
    Inherit,
    /// 只读：写与命令一律拒绝，且不得被记忆写等特例放行。
    ReadOnly,
}

impl PermissionCeiling {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::ReadOnly => "read-only",
        }
    }

    /// 是否禁止一切写与命令（`ReadOnly` 上限的判定源）。
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly)
    }
}

/// 一条定义或诊断。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SubagentProfile {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub tools: ToolChoice,
    pub model: ModelChoice,
    #[serde(default)]
    pub permission_ceiling: PermissionCeiling,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub color: Option<String>,
}

fn default_true() -> bool {
    true
}

/// 一条诊断：说清哪一项因为什么不能生效。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SubagentDiagnostic {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub field: Option<String>,
    pub reason: String,
}

impl SubagentDiagnostic {
    pub fn new(code: &str, field: Option<&str>, reason: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            field: field.map(str::to_string),
            reason: reason.into(),
        }
    }
}

/// 被禁用的颜色枚举：限定取值，避免前端拿到任意字符串。
pub const SUBAGENT_COLORS: &[&str] = &[
    "slate", "blue", "green", "amber", "red", "purple", "teal", "pink",
];

impl SubagentProfile {
    pub fn validate(&self) -> Vec<SubagentDiagnostic> {
        let mut out: Vec<SubagentDiagnostic> = Vec::new();
        if self.schema_version != SUBAGENT_SCHEMA_VERSION {
            out.push(SubagentDiagnostic::new(
                "subagent/unsupported-schema-version",
                Some("schemaVersion"),
                format!(
                    "不支持的 schemaVersion：{}（本版本只认 {}）",
                    self.schema_version, SUBAGENT_SCHEMA_VERSION
                ),
            ));
        }
        if !is_valid_subagent_id(&self.id) {
            out.push(SubagentDiagnostic::new(
                "subagent/invalid-id",
                Some("id"),
                format!(
                    "非法的定义 id：{}（只允许小写字母、数字与连字符，且以字母数字开头，最长 64）",
                    self.id
                ),
            ));
        }
        let name_len = self.name.chars().count();
        if name_len == 0 || name_len > SUBAGENT_NAME_MAX_CHARS {
            out.push(SubagentDiagnostic::new(
                "subagent/invalid-name",
                Some("name"),
                format!("显示名长度必须在 1–{SUBAGENT_NAME_MAX_CHARS} 个字符之间"),
            ));
        }
        let description_len = self.description.chars().count();
        if description_len == 0 || description_len > SUBAGENT_DESCRIPTION_MAX_CHARS {
            out.push(SubagentDiagnostic::new(
                "subagent/invalid-description",
                Some("description"),
                format!("用途描述长度必须在 1–{SUBAGENT_DESCRIPTION_MAX_CHARS} 个字符之间"),
            ));
        }
        if self.instructions.len() > SUBAGENT_INSTRUCTIONS_MAX_BYTES {
            out.push(SubagentDiagnostic::new(
                "subagent/instructions-too-large",
                Some("instructions"),
                format!(
                    "角色补充提示超过 {} 字节上限",
                    SUBAGENT_INSTRUCTIONS_MAX_BYTES
                ),
            ));
        }
        if let Some(names) = self.tools.allowlist() {
            let mut seen: Vec<&str> = Vec::new();
            for name in names {
                let trimmed = name.trim();
                if trimmed.is_empty() {
                    out.push(SubagentDiagnostic::new(
                        "subagent/invalid-tool-name",
                        Some("tools"),
                        "工具名不能为空",
                    ));
                    continue;
                }
                if trimmed != name {
                    out.push(SubagentDiagnostic::new(
                        "subagent/invalid-tool-name",
                        Some("tools"),
                        format!("工具名前后不能有空白：{name:?}"),
                    ));
                }
                if seen.contains(&trimmed) {
                    out.push(SubagentDiagnostic::new(
                        "subagent/duplicate-tool-name",
                        Some("tools"),
                        format!("工具名重复：{trimmed}"),
                    ));
                } else {
                    seen.push(trimmed);
                }
            }
        }
        if let ModelChoice::Explicit { selection } = &self.model
            && (selection.provider.trim().is_empty() || selection.model.trim().is_empty())
        {
            out.push(SubagentDiagnostic::new(
                "subagent/invalid-model",
                Some("model"),
                "显式模型必须同时给出 provider 与 model",
            ));
        }
        if let Some(color) = &self.color
            && !SUBAGENT_COLORS.contains(&color.as_str())
        {
            out.push(SubagentDiagnostic::new(
                "subagent/invalid-color",
                Some("color"),
                format!("颜色只能是：{}", SUBAGENT_COLORS.join("、")),
            ));
        }
        out
    }

    /// 定义里显式请求的模型；`None` = 继承父代理。
    pub fn explicit_model(&self) -> Option<&ModelSelection> {
        match &self.model {
            ModelChoice::Inherit => None,
            ModelChoice::Explicit { selection } => Some(selection),
        }
    }
}

/// 硬禁项：任何子代理都不能持有这些工具。
///
/// 它们要么会打开"子代理再生子代理"的替代入口，要么会改变会话主控模式
/// 或操作会话目标（那是编排层的职责，不是被派遣者的）。新增 Denia 的
/// agent/workflow 派遣入口时必须登记到这里，不能只靠维护两个字符串。
pub const CHILD_HARD_DENIED_TOOLS: &[&str] = &[
    "spawn_agent",
    "fork_agent",
    "interrupt_agent",
    "list_agents",
    "wait_agent",
    "exit_plan",
    "get_goal",
    "update_goal",
];

/// 宿主管理类能力：定义/preset 的写入面只属于人类配置面，不进子代理。
pub const HOST_ADMINISTRATION_TOOLS: &[&str] = &["create_preset"];

/// 历史（旧日志）里子代理能拿到的只读上限。
///
/// 仅用于读取没有快照的旧子代理：缺授权列表**不等于**继承全工具，按这份
/// 保守集合构造 `LegacySnapshot`。
pub const LEGACY_SUBAGENT_READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "read_tool_output",
    "ls",
    "glob",
    "grep",
    "skill",
    "browser",
    "web_fetch",
];

/// 历史子代理的硬性上限：只读集合 + 记忆沉淀用的写工具。
///
/// 旧日志里显式列出的工具最多与本集合相交，再扣 [`CHILD_HARD_DENIED_TOOLS`]。
pub const LEGACY_SUBAGENT_MAX_TOOLS: &[&str] = &[
    "read_file",
    "read_tool_output",
    "ls",
    "glob",
    "grep",
    "skill",
    "browser",
    "web_fetch",
    "write_file",
    "edit",
];

/// 判断某工具是否对子代理硬禁。
pub fn is_child_hard_denied(name: &str) -> bool {
    CHILD_HARD_DENIED_TOOLS.contains(&name)
        || HOST_ADMINISTRATION_TOOLS.contains(&name)
        || name.starts_with("mcp__denia") // 预留：denia 管理面绝不外露
}

/// 运行快照里对定义来源的引用；inline 定义没有这一段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileRef {
    pub qualified_id: String,
    pub id: String,
    pub source: SubagentProfileSource,
    /// 创建时的定义内容版本；之后编辑定义不会追溯修改这个 child。
    pub revision: String,
}

/// fork 来源投影的审计信息。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ForkProjectionRef {
    pub source_session_id: String,
    /// 截止事件序号（截取到的最后一个闭合轮次）。
    pub cut_seq: u64,
    pub projection_version: u32,
    /// 被丢弃的来源类别（便于审计与排查）。
    pub dropped: Vec<String>,
    pub migration_version: u32,
}

/// 随会话头冻结的子代理运行快照。
///
/// 角色补充提示正文（最大 64 KiB）不进快照：它落在会话目录的
/// `subagent.json` 里，避免每条子代理会话的头行都被 64 KiB 正文撑大、
/// 也避免会话列表接口把它当摘要送回前端。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SubagentSnapshotRef {
    pub version: u32,
    /// `subagent.json` 的内容版本；引用不存在或校验失败必须拒绝启动。
    pub hash: String,
    /// `spawn` | `fork`。
    pub creation_mode: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub profile: Option<ProfileRef>,
    pub resolved_name: String,
    pub resolved_description: String,
    /// 冻结的工具名列表（已扣硬禁项、已与父可授予集合相交）。
    pub effective_tools: Vec<String>,
    pub model: ModelSelection,
    pub permission_ceiling: PermissionCeiling,
    /// 创建时父代理的权限模式（审计；实际判权仍走实时权限引擎）。
    pub permission_mode: String,
    /// 父代理当时的 agent preset（审计；文件消失不回退全量工具）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub parent_preset: Option<String>,
    /// 父基础系统提示快照的 hash。
    pub parent_prompt_hash: String,
    /// 固定 `false`：禁止派遣是运行时硬规则，这个布尔只是审计。
    pub delegation_allowed: bool,
    /// 固定 `project-only`：子代理不自动加载全局 AGENTS.md。
    pub instruction_scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub fork: Option<ForkProjectionRef>,
}

/// 会话目录里落盘的完整快照（含角色正文）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentSnapshotFile {
    pub version: u32,
    pub profile: SubagentSnapshotRef,
    /// 角色补充提示正文；追加到父基础提示之后，不替换父 persona。
    #[serde(default)]
    pub instructions: String,
}

/// 快照文件名（位于会话目录内，与 session.jsonl 平级）。
pub const SNAPSHOT_FILE: &str = "subagent.json";

impl SubagentSnapshotFile {
    /// 内容版本：引用校验与"外部改动即失效"都用它。
    ///
    /// 不把 `profile.hash` 算进输入——它存的就是本函数的结果，否则自引用。
    pub fn hash(&self) -> String {
        let canonical = format!(
            "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            self.version,
            self.profile.resolved_name,
            self.profile.resolved_description,
            self.profile.effective_tools.join(","),
            self.profile.permission_ceiling.as_str(),
            self.profile.instruction_scope,
            self.instructions,
        );
        stable_hash(&canonical)
    }
}

/// 与内容绑定的稳定短哈希（FNV-1a 64；只做版本比对，不做安全承诺）。
pub fn stable_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 三个内置预设的只读探索工具集。
pub const EXPLORE_TOOLS: &[&str] = &[
    "read_file",
    "read_tool_output",
    "ls",
    "glob",
    "grep",
    "skill",
    "web_fetch",
    "send_message",
];

/// 开发执行的只读部分（在上面的基础上加写工具与 bash）。
pub const DEVELOP_TOOLS: &[&str] = &[
    "read_file",
    "read_tool_output",
    "ls",
    "glob",
    "grep",
    "skill",
    "web_fetch",
    "send_message",
    "write_file",
    "edit",
    "bash",
    "todo_write",
];

/// 验证：读取工具 + shell + 待办，没有写文件工具。
pub const VERIFY_TOOLS: &[&str] = &[
    "read_file",
    "read_tool_output",
    "ls",
    "glob",
    "grep",
    "skill",
    "web_fetch",
    "send_message",
    "bash",
    "todo_write",
];

fn allowlist(names: &[&str]) -> ToolChoice {
    ToolChoice::Allowlist {
        names: names.iter().map(|name| (*name).to_string()).collect(),
    }
}

/// 内置定义：随程序提供，可禁用/复制/恢复默认，但不可删除程序资源。
pub fn builtin_profiles() -> Vec<SubagentProfile> {
    vec![
        SubagentProfile {
            schema_version: SUBAGENT_SCHEMA_VERSION,
            id: "explore".to_string(),
            name: "只读探索".to_string(),
            description: "查找实现、理解调用链、定位影响范围；只读调查，不修改项目。"
                .to_string(),
            instructions: EXPLORE_INSTRUCTIONS.to_string(),
            enabled: true,
            tools: allowlist(EXPLORE_TOOLS),
            model: ModelChoice::Inherit,
            permission_ceiling: PermissionCeiling::ReadOnly,
            color: Some("blue".to_string()),
        },
        SubagentProfile {
            schema_version: SUBAGENT_SCHEMA_VERSION,
            id: "develop".to_string(),
            name: "开发执行".to_string(),
            description:
                "实现有明确范围的功能或修复，或按父代理提供／指定的已确认计划逐项落地。"
                    .to_string(),
            instructions: DEVELOP_INSTRUCTIONS.to_string(),
            enabled: true,
            tools: allowlist(DEVELOP_TOOLS),
            model: ModelChoice::Inherit,
            permission_ceiling: PermissionCeiling::Inherit,
            color: Some("green".to_string()),
        },
        SubagentProfile {
            schema_version: SUBAGENT_SCHEMA_VERSION,
            id: "verify".to_string(),
            name: "验证".to_string(),
            description: "审查改动、运行测试与构建、复现错误并核验验收条件；只报告，不自行修复实现。"
                .to_string(),
            instructions: VERIFY_INSTRUCTIONS.to_string(),
            enabled: true,
            tools: allowlist(VERIFY_TOOLS),
            model: ModelChoice::Inherit,
            permission_ceiling: PermissionCeiling::Inherit,
            color: Some("amber".to_string()),
        },
    ]
}

/// 未指定定义时的默认预设 id。
pub const DEFAULT_SUBAGENT_ID: &str = "develop";

const EXPLORE_INSTRUCTIONS: &str = "先定位入口，再沿调用链核对数据流；用 grep/glob 收窄范围，不靠猜测下结论。\n报告证据（文件路径、行号或命令输出）、结论与仍未确认的未知项。发现与预期不符的实现时先如实报告，不要顺手修改文件。";

const DEVELOP_INSTRUCTIONS: &str = "先复述要达成的目标与验收条件，再做最小充分的修改；只改与任务直接相关的文件。\n报告实际改动了哪些文件、跑了哪些验证、结果如何，以及剩余问题与未覆盖范围。遇到阻塞先如实反馈，不擅自扩大修改范围，也不提交计划审批。";

const VERIFY_INSTRUCTIONS: &str = "独立核验改动：读代码找反例，运行相关测试、构建或复现步骤。\n报告实际执行的命令、退出结果与关键输出，明确区分「通过」「失败」与「未覆盖」。不要修改实现来让验证通过。";

/// 临时（inline）子代理规格：主代理调用时现造一个子代理，不要求先保存定义。
///
/// 与 [`SubagentProfile`] 的差别只有三处：没有 `id`（由宿主生成）、没有
/// `schemaVersion`（恒为 1）、没有 `enabled`（inline 一律启用）。字段用
/// camelCase——与外层派遣参数的 snake_case 是两个明确分开的边界。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InlineSubagentSpec {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub instructions: String,
    pub tools: ToolChoice,
    pub model: ModelChoice,
    #[serde(default)]
    pub permission_ceiling: PermissionCeiling,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub color: Option<String>,
}

/// inline 定义的固定 id：不落盘，也就没有与他人重名的风险。
pub const INLINE_SUBAGENT_ID: &str = "inline";

impl InlineSubagentSpec {
    pub fn into_profile(self) -> SubagentProfile {
        SubagentProfile {
            schema_version: SUBAGENT_SCHEMA_VERSION,
            id: INLINE_SUBAGENT_ID.to_string(),
            name: self.name,
            description: self.description,
            instructions: self.instructions,
            enabled: true,
            tools: self.tools,
            model: self.model,
            permission_ceiling: self.permission_ceiling,
            color: self.color,
        }
    }
}

/// 派遣参数：`spawn_agent` / `fork_agent` 的**严格**类型。
///
/// 外层用 snake_case（与既有工具参数一致），`inline` 内层用统一的 camelCase
/// DTO；解析在工具边界一次完成，`Runtime::delegate` 不再零散读 JSON。
///
/// `persona` / `max_depth` / `run_in_background` 是旧调用可能带来的字段：
/// - `persona` 映射成补充 instructions（绝不替换父 persona）；
/// - `run_in_background` 忽略（Denia 的派遣本来就是始终后台）；
/// - `max_depth` 直接失败（深度配置是被删除的产品能力，不能让旧调用以为
///   还能改深度）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct DelegateArgs {
    pub prompt: String,
    #[serde(default)]
    pub description: Option<String>,
    /// 已保存定义的 id（qualifiedId 或逻辑 id）。
    #[serde(default)]
    pub profile_id: Option<String>,
    /// 临时定义；与 `profile_id` 互斥。
    #[serde(default)]
    pub inline: Option<InlineSubagentSpec>,
    /// 对所选定义工具的进一步收窄（只能更小，不能更大）。
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// 旧字段：角色补充提示。
    #[serde(default)]
    pub persona: Option<String>,
    /// 旧字段：出现即拒绝。
    #[serde(default)]
    pub max_depth: Option<serde_json::Value>,
    /// 旧字段：Denia 的派遣本来就是始终后台，忽略其取值。
    #[serde(default)]
    pub run_in_background: Option<bool>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

impl DelegateArgs {
    /// 请求的定义选择：定义 id 与 inline 互斥（同时出现是歧义，不是"后者优先"）。
    pub fn target(&self) -> Result<DelegateTarget, String> {
        if let Some(value) = &self.max_depth {
            return Err(format!(
                "subagent/depth-config-removed: 委派深度不再是可配置项（收到 max_depth={value}）。\
                 子代理不能派遣子代理，请移除该参数后重试。"
            ));
        }
        let profile_id = self
            .profile_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        match (profile_id, self.inline.as_ref()) {
            (Some(_), Some(_)) => Err(
                "subagent/ambiguous-target: profile_id 与 inline 只能给一个（定义 id 与临时定义不能同时指定）"
                    .to_string(),
            ),
            (Some(id), None) => Ok(DelegateTarget::Profile(id.to_string())),
            (None, Some(spec)) => Ok(DelegateTarget::Inline(spec.clone())),
            (None, None) => Ok(DelegateTarget::Default),
        }
    }
}

/// 一次派遣请求指向哪个定义。
#[derive(Debug, Clone, PartialEq)]
pub enum DelegateTarget {
    /// 已保存定义的 qualifiedId 或逻辑 id。
    Profile(String),
    Inline(InlineSubagentSpec),
    /// 都没给时用有效 develop。
    Default,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_profiles_are_valid_and_do_not_reference_hard_denied_tools() {
        for profile in builtin_profiles() {
            assert!(
                profile.validate().is_empty(),
                "内置定义 {} 不合法：{:?}",
                profile.id,
                profile.validate()
            );
            let names = profile.tools.allowlist().expect("内置定义用显式白名单");
            let denied: Vec<&String> = names
                .iter()
                .filter(|name| is_child_hard_denied(name))
                .collect();
            assert!(denied.is_empty(), "内置定义引用了硬禁工具：{denied:?}");
            let mut sorted: Vec<&String> = names.iter().collect();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), names.len(), "工具列表必须去重");
        }
    }

    #[test]
    fn explore_is_read_only_and_has_no_write_tools() {
        let explore = builtin_profiles()
            .into_iter()
            .find(|profile| profile.id == "explore")
            .unwrap();
        assert_eq!(explore.permission_ceiling, PermissionCeiling::ReadOnly);
        let names = explore.tools.allowlist().unwrap();
        for forbidden in ["write_file", "edit", "bash", "job_start", "browser"] {
            assert!(
                !names.iter().any(|name| name == forbidden),
                "探索预设有 {forbidden}"
            );
        }
    }

    #[test]
    fn empty_allowlist_is_not_inherit() {
        let mut profile = builtin_profiles().remove(0);
        profile.tools = ToolChoice::Allowlist { names: Vec::new() };
        assert_eq!(profile.tools.allowlist(), Some(&[][..]));
        assert_ne!(profile.tools, ToolChoice::Inherit);
    }

    #[test]
    fn validation_reports_bad_id_tool_names_and_model() {
        let mut profile = builtin_profiles().remove(0);
        profile.id = "Bad Id".to_string();
        profile.name = String::new();
        profile.tools = ToolChoice::Allowlist {
            names: vec!["read_file".into(), "read_file".into(), "  ".into()],
        };
        profile.model = ModelChoice::Explicit {
            selection: ModelSelection {
                provider: String::new(),
                model: "m".into(),
                reasoning_effort: None,
            },
        };
        let codes: Vec<String> = profile
            .validate()
            .into_iter()
            .map(|diagnostic| diagnostic.code)
            .collect();
        for expected in [
            "subagent/invalid-id",
            "subagent/invalid-name",
            "subagent/duplicate-tool-name",
            "subagent/invalid-tool-name",
            "subagent/invalid-model",
        ] {
            assert!(codes.iter().any(|code| code == expected), "缺少 {expected}: {codes:?}");
        }
    }

    #[test]
    fn unknown_schema_version_is_rejected() {
        let mut profile = builtin_profiles().remove(0);
        profile.schema_version = 2;
        assert!(
            profile
                .validate()
                .iter()
                .any(|d| d.code == "subagent/unsupported-schema-version")
        );
    }

    #[test]
    fn ids_follow_the_slug_grammar() {
        assert!(is_valid_subagent_id("api-reviewer"));
        assert!(is_valid_subagent_id("a1"));
        assert!(!is_valid_subagent_id("Api"));
        assert!(!is_valid_subagent_id("-a"));
        assert!(!is_valid_subagent_id("a b"));
        assert!(!is_valid_subagent_id(&"a".repeat(65)));
    }

    #[test]
    fn child_hard_denied_covers_nesting_and_host_administration() {
        for name in ["spawn_agent", "fork_agent", "exit_plan", "get_goal"] {
            assert!(is_child_hard_denied(name), "{name} 必须是硬禁项");
        }
        assert!(is_child_hard_denied("create_preset"));
        assert!(!is_child_hard_denied("read_file"));
        assert!(!is_child_hard_denied("bash"));
    }
}
