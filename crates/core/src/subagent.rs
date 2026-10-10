//! 子代理定义（subagent profile）词汇：人类可管理的角色声明。
//!
//! 与 [`crate::preset`] 的分工：preset 决定"一个会话是什么 agent"，是主代理
//! 的组装；子代理定义决定"派遣出去的这个子代理是谁"，只描述角色补充、工具
//! 选择、模型与权限上限。子代理定义**不**定义主 persona，也不定义主代理的
//! 功能开关——那些从父会话继承。
//!
//! 本模块只放跨 crate 的协议类型与纯函数校验；文件读写与来源合并属于
//! server 侧的 ProfileStore。

use serde::{Deserialize, Serialize};

/// 定义文件格式版本；不认识的版本必须报错，不得按"缺字段=默认"继续。
pub const SUBAGENT_SCHEMA_VERSION: u32 = 1;

/// 运行快照版本：0 = 旧描述符（本计划之前的日志），1 = 本计划。
pub const SUBAGENT_SNAPSHOT_VERSION: u32 = 1;

/// 角色补充提示的字节上限（UTF-8）。
pub const INSTRUCTIONS_MAX_BYTES: usize = 64 * 1024;
/// 显示名长度上限（Unicode 字符）。
pub const NAME_MAX_CHARS: usize = 80;
/// 用途描述长度上限（Unicode 字符）。
pub const DESCRIPTION_MAX_CHARS: usize = 2000;
/// 定义的物理文件名长度上限（id 的 64 字符 + `.md`）。
pub const ID_MAX_CHARS: usize = 64;

/// 子代理定义 id 语法：小写字母或数字开头，其后是小写字母、数字与单连字符。
/// id 会成为文件名，因此校验必须发生在拼接路径之前。
pub fn is_valid_subagent_id(id: &str) -> bool {
    if id.is_empty() || id.chars().count() > ID_MAX_CHARS {
        return false;
    }
    let bytes = id.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// 定义的来源层级；合并顺序 builtin → user → project，同 id 后者覆盖前者。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum ProfileSource {
    Builtin,
    User,
    Project,
}

impl ProfileSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "builtin" => Some(Self::Builtin),
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            _ => None,
        }
    }
}

/// 限定 id：`builtin:explore` / `user:api-reviewer` / `project:api-reviewer`。
///
/// 项目部分的根由会话推导，调用方只能给出 scope 与 id，不能指定路径。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
pub struct QualifiedProfileId {
    pub source: ProfileSource,
    pub id: String,
}

impl QualifiedProfileId {
    pub fn new(source: ProfileSource, id: impl Into<String>) -> Self {
        Self {
            source,
            id: id.into(),
        }
    }

    /// 解析 `source:id`；两段都做语法校验，路径穿越在这里就失败。
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (source, id) = raw
            .split_once(':')
            .ok_or_else(|| format!("限定 id 必须形如 builtin:explore,实际:{raw}"))?;
        let source = ProfileSource::parse(source)
            .ok_or_else(|| format!("未知的定义来源:{source}(只支持 builtin/user/project)"))?;
        if !is_valid_subagent_id(id) {
            return Err(format!(
                "非法的定义 id:{id}（只允许小写字母、数字与连字符，不能以连字符开头，最长 {ID_MAX_CHARS} 字符）"
            ));
        }
        Ok(Self {
            source,
            id: id.to_string(),
        })
    }
}

impl std::fmt::Display for QualifiedProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.source.as_str(), self.id)
    }
}

/// 工具选择：`inherit` 继承父可授予工具；`allowlist` 显式列表。
///
/// `allowlist` 的空数组表示零业务工具（纯推理任务），**不得**解释为全部工具。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ToolSelection {
    Inherit,
    Allowlist { names: Vec<String> },
}

impl ToolSelection {
    /// 零业务工具（显式空列表）。
    pub fn is_empty_allowlist(&self) -> bool {
        matches!(self, Self::Allowlist { names } if names.is_empty())
    }

    pub fn summarize(&self) -> String {
        match self {
            Self::Inherit => "继承父工具".to_string(),
            Self::Allowlist { names } if names.is_empty() => "无工具".to_string(),
            Self::Allowlist { names } => format!("{} 个工具", names.len()),
        }
    }
}

/// 模型选择：`inherit` 用父模型；`explicit` 用明确选择（provider/model 成套）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(tag = "mode", rename_all = "camelCase")]
pub enum ModelChoice {
    #[default]
    Inherit,
    Explicit {
        selection: crate::config::ModelSelection,
    },
}

impl ModelChoice {
    pub fn selection(&self) -> Option<&crate::config::ModelSelection> {
        match self {
            Self::Inherit => None,
            Self::Explicit { selection } => Some(selection),
        }
    }

    pub fn summarize(&self) -> String {
        match self {
            Self::Inherit => "继承父模型".to_string(),
            Self::Explicit { selection } => format!("{}/{}", selection.provider, selection.model),
        }
    }
}

/// 权限上限：只允许收窄，不提供绕过审批的档位。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum PermissionCeiling {
    /// 跟随父权限模式。
    #[default]
    Inherit,
    /// 强制只读：写工具与命令一律拒绝，MemoryWrite 特例也不例外。
    ReadOnly,
}

impl PermissionCeiling {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inherit => "inherit",
            Self::ReadOnly => "read-only",
        }
    }
}

/// UI 展示用颜色（限定枚举，不参与授权）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum ProfileColor {
    Blue,
    Green,
    Purple,
    Orange,
    Red,
    Gray,
}

impl ProfileColor {
    pub const ALL: [Self; 6] = [
        Self::Blue,
        Self::Green,
        Self::Purple,
        Self::Orange,
        Self::Red,
        Self::Gray,
    ];
}

/// 一份子代理定义（磁盘格式与 wire 格式同一形状）。
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
    pub tools: ToolSelection,
    #[serde(default)]
    pub model: ModelChoice,
    #[serde(default)]
    pub permission_ceiling: PermissionCeiling,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub color: Option<ProfileColor>,
}

fn default_true() -> bool {
    true
}

impl SubagentProfile {
    /// 必填字段与取值的完整校验（不含工具名存在性——那需要部署注册表）。
    pub fn validate(&self) -> Result<(), Vec<ProfileDiagnostic>> {
        let mut issues = Vec::new();
        if self.schema_version != SUBAGENT_SCHEMA_VERSION {
            issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some("schemaVersion"),
                format!(
                    "不支持的定义版本 {}（本版本只认识 {SUBAGENT_SCHEMA_VERSION}）",
                    self.schema_version
                ),
            ));
        }
        if !is_valid_subagent_id(&self.id) {
            issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some("id"),
                format!(
                    "非法的定义 id：{}（只允许小写字母、数字与连字符，最长 {ID_MAX_CHARS} 字符）",
                    self.id
                ),
            ));
        }
        let name_len = self.name.trim().chars().count();
        if name_len == 0 || name_len > NAME_MAX_CHARS {
            issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some("name"),
                format!("name 必须是 1–{NAME_MAX_CHARS} 个字符"),
            ));
        }
        let description_len = self.description.trim().chars().count();
        if description_len == 0 || description_len > DESCRIPTION_MAX_CHARS {
            issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some("description"),
                format!("description 必须是 1–{DESCRIPTION_MAX_CHARS} 个字符"),
            ));
        }
        if self.instructions.len() > INSTRUCTIONS_MAX_BYTES {
            issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some("instructions"),
                format!("instructions 不能超过 {INSTRUCTIONS_MAX_BYTES} 字节"),
            ));
        }
        if let ToolSelection::Allowlist { names } = &self.tools {
            let mut seen = std::collections::HashSet::new();
            for name in names {
                if name.trim().is_empty() {
                    issues.push(ProfileDiagnostic::error(
                        codes::PROFILE_INVALID,
                        Some("tools.names"),
                        "工具名不能为空".to_string(),
                    ));
                } else if !seen.insert(name.clone()) {
                    issues.push(ProfileDiagnostic::error(
                        codes::PROFILE_INVALID,
                        Some("tools.names"),
                        format!("工具名重复：{name}"),
                    ));
                }
            }
        }
        if let ModelChoice::Explicit { selection } = &self.model
            && (selection.provider.trim().is_empty() || selection.model.trim().is_empty())
        {
            issues.push(ProfileDiagnostic::error(
                codes::PROFILE_INVALID,
                Some("model"),
                "显式模型必须同时给出 provider 与 model".to_string(),
            ));
        }
        if issues.is_empty() {
            Ok(())
        } else {
            Err(issues)
        }
    }
}

/// 主代理派遣时临时创建的规格：与定义同形，但没有 id/启用状态/声明版本。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SubagentInlineSpec {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub instructions: String,
    pub tools: ToolSelection,
    #[serde(default)]
    pub model: ModelChoice,
    #[serde(default)]
    pub permission_ceiling: PermissionCeiling,
}

impl SubagentInlineSpec {
    pub fn validate(&self) -> Result<(), Vec<ProfileDiagnostic>> {
        SubagentProfile {
            schema_version: SUBAGENT_SCHEMA_VERSION,
            id: "inline".to_string(),
            name: self.name.clone(),
            description: self.description.clone(),
            instructions: self.instructions.clone(),
            enabled: true,
            tools: self.tools.clone(),
            model: self.model.clone(),
            permission_ceiling: self.permission_ceiling,
            color: None,
        }
        .validate()
    }
}

/// 定义诊断：解析/校验失败的事实，不静默吞掉。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct ProfileDiagnostic {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "bindings", ts(optional))]
    pub field: Option<String>,
    pub message: String,
}

impl ProfileDiagnostic {
    pub fn error(code: &str, field: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            field: field.map(str::to_string),
            message: message.into(),
        }
    }
}

/// 服务端包装层：调用方看到的定义与它的来源元数据。
///
/// `source`/`revision`/`editable` 一律由服务端生成，不接受调用方自报。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SubagentProfileView {
    pub profile: SubagentProfile,
    pub qualified_id: String,
    pub source: ProfileSource,
    pub revision: u64,
    /// 是否可写（内置定义只能写用户覆盖；项目覆盖只能写到项目目录）。
    pub editable: bool,
    /// 是否覆盖了更低优先级的同 id 定义。
    pub overrides_builtin: bool,
    /// 该项目作用域是否可写（无项目根时为 false）。
    pub project_writable: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<ProfileDiagnostic>,
}

/// 输入给 UI 的作用域过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "kebab-case")]
pub enum ProfileWriteScope {
    User,
    Project,
}

/// 派遣目录条目：**只**公布父代理选型需要的信息，不含 instructions 全文。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[serde(rename_all = "camelCase")]
pub struct SubagentCatalogEntry {
    pub qualified_id: String,
    pub name: String,
    pub description: String,
    /// 工具模式摘要（"继承父工具" / "8 个工具" / "无工具"）。
    pub tools: String,
    /// 模型摘要（"继承父模型" / "provider/model"）。
    pub model: String,
}

impl SubagentCatalogEntry {
    pub fn new(qualified_id: String, profile: &SubagentProfile) -> Self {
        Self {
            qualified_id,
            name: profile.name.clone(),
            description: profile.description.clone(),
            tools: profile.tools.summarize(),
            model: profile.model.summarize(),
        }
    }
}

/// 稳定错误码：错误必须能被程序判定，而不是靠中文文案匹配。
pub mod codes {
    pub const PROFILE_NOT_FOUND: &str = "subagent/profile-not-found";
    pub const PROFILE_SHADOWED: &str = "subagent/profile-shadowed";
    pub const PROFILE_INVALID: &str = "subagent/profile-invalid";
    pub const PROFILE_DISABLED: &str = "subagent/profile-disabled";
    pub const PROFILE_EXISTS: &str = "subagent/profile-exists";
    pub const SCOPE_FORBIDDEN: &str = "subagent/scope-forbidden";
    pub const REVISION_CONFLICT: &str = "subagent/revision-conflict";
    pub const TOOL_UNKNOWN: &str = "subagent/tool-unknown";
    pub const TOOL_NOT_GRANTED: &str = "subagent/tool-not-granted";
    pub const TOOL_HARD_DENIED: &str = "subagent/tool-hard-denied";
    pub const DELEGATION_FORBIDDEN: &str = "subagent/delegation-forbidden";
    pub const DEPTH_CONFIG_REMOVED: &str = "subagent/depth-config-removed";
    pub const AMBIGUOUS_SPEC: &str = "subagent/ambiguous-spec";
    pub const CONCURRENCY_EXCEEDED: &str = "subagent/concurrency-exceeded";
    pub const SNAPSHOT_INVALID: &str = "subagent/snapshot-invalid";
    pub const LEGACY_RESUME_UNSUPPORTED: &str = "subagent/legacy-resume-unsupported";
    pub const EMPTY_TOOLS: &str = "subagent/empty-tools";
}

/// 历史只读集合：本计划之前子代理的唯一授权上限。
///
/// **不是**当前策略。只为两条路径保留：
/// 1. 读取旧日志描述符（缺 `effectiveTools` 时按它保守构造授权）；
/// 2. 兼容旧的 `allowed_tools` 参数（显式列表最多与它相交）。
///
/// 新的定义（内置 explore/develop/verify 或用户自定义）各自声明工具，不再
/// 受这个集合约束。
pub const LEGACY_READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "read_tool_output",
    "ls",
    "glob",
    "grep",
    "skill",
    "browser",
    "web_fetch",
];

/// 旧描述符/旧参数的保守授权：绝不等价于 inherit。
///
/// - `explicit` 为 `None` → 历史只读集合（保守上限）；
/// - `explicit` 为 `Some` → 只取与历史只读集合的交集（旧列表不得放开写与命令）；
/// - 两种情况都扣掉 [`child_hard_denied`] 的硬禁项。
pub fn legacy_child_tools(explicit: Option<&[String]>) -> Vec<String> {
    let mut tools: Vec<String> = match explicit {
        Some(names) => names
            .iter()
            .filter(|name| LEGACY_READ_ONLY_TOOLS.contains(&name.as_str()))
            .cloned()
            .collect(),
        None => LEGACY_READ_ONLY_TOOLS
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
    };
    tools.retain(|name| !child_hard_denied(name));
    tools.sort();
    tools.dedup();
    tools
}

/// 子代理**绝对**不能使用的工具（运行时硬规则，UI/配置/模型参数都无法打开）。
/// 分三类：
/// 1. 派遣类——子代理禁止派遣子代理；
/// 2. 会话主控类——`exit_plan` 切换会话主控模式，`get_goal`/`update_goal` 属于
///    主会话目标，不是子代理的职责；
/// 3. 宿主配置类（host-administration）——写定义、写 preset 等人类配置面。
///
/// 未来新增派遣入口必须在本表或 [`ToolCapability`] 分类里登记，不能只维护
/// 两个字符串。
pub const CHILD_HARD_DENIED_TOOLS: &[&str] = &[
    "spawn_agent",
    "fork_agent",
    "interrupt_agent",
    "list_agents",
    "wait_agent",
    "exit_plan",
    "get_goal",
    "update_goal",
    "create_preset",
];

/// 工具的能力分类：派遣准入与硬禁的统一依据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCapability {
    Read,
    Write,
    Shell,
    /// 会创建/控制其他 agent 的入口（派遣能力）。
    Delegation,
    /// 会改动宿主配置或定义的人类配置面。
    HostAdministration,
    /// 会改变会话主控模式或操作主会话目标。
    SessionControl,
    Interaction,
    Jobs,
    Browser,
    Mcp,
    Other,
}

/// 按注册名分类工具能力。
///
/// 只对"需要特殊准入"的名字逐项列举；未知名字归 [`ToolCapability::Other`]，
/// 仍受父级授权与权限引擎约束。
///
/// 任务账本三件套的分工：`get_task` 是读（只读账本），`update_task` 是写
/// （改的是**本会话自己**的账本），`run_checks` 是 `Shell`（它真的启动进程，
/// 与 `bash` 同性质）。三者都**不**归 [`ToolCapability::SessionControl`]：
/// 那个分类被 [`child_hard_denied`] 硬禁，而账本是每个会话自己的工作记录
/// ——子代理写的是它自己的账本，碰不到父会话的目标与收口，所以按父级授权
/// 使用即可，不必硬禁。
pub fn tool_capability(name: &str) -> ToolCapability {
    if name.starts_with("mcp__") || name == "mcp_list" {
        return ToolCapability::Mcp;
    }
    match name {
        "spawn_agent" | "fork_agent" | "interrupt_agent" | "list_agents" | "wait_agent"
        | "send_message" => ToolCapability::Delegation,
        "create_preset" | "create_subagent" | "update_subagent" => {
            ToolCapability::HostAdministration
        }
        "exit_plan" | "get_goal" | "update_goal" => ToolCapability::SessionControl,
        "bash" | "job_start" | "job_output" | "job_kill" | "job_list" | "run_checks" => {
            ToolCapability::Shell
        }
        "write_file" | "edit" | "todo_write" | "update_task" => ToolCapability::Write,
        "ask" => ToolCapability::Interaction,
        "browser" => ToolCapability::Browser,
        "read_file" | "read_tool_output" | "ls" | "glob" | "grep" | "skill" | "web_fetch"
        | "get_task" => ToolCapability::Read,
        _ => ToolCapability::Other,
    }
}

/// 子代理是否被硬禁该工具。
///
/// `send_message` 是例外：它是子代理向直接父会话汇报的唯一通道，属于可选工具
/// （默认包含），不属于派遣控制。因此 [`ToolCapability::Delegation`] 里除
/// `send_message` 外的全部入口都硬禁。
pub fn child_hard_denied(name: &str) -> bool {
    if CHILD_HARD_DENIED_TOOLS.contains(&name) {
        return true;
    }
    match tool_capability(name) {
        ToolCapability::Delegation => name != "send_message",
        ToolCapability::HostAdministration | ToolCapability::SessionControl => true,
        _ => false,
    }
}

/// 三个内置预设：只读探索、开发执行、验证。
///
/// 工具列表由实际 Denia 工具名构成；初始化后去重（见
/// [`builtin_subagent_profiles`]）。这里**不是**全局授权白名单——它只是三个
/// 任务模板的默认值，用户可复制、可改、可禁用。
pub fn builtin_subagent_profiles() -> Vec<SubagentProfile> {
    // 读取工具集合：explore 的工具列表即"读取工具"的定义，develop/verify
    // 在它之上增删。
    let read_tools: &[&str] = &[
        "read_file",
        "read_tool_output",
        "ls",
        "glob",
        "grep",
        "skill",
        "web_fetch",
        "send_message",
    ];
    let mut develop_tools: Vec<String> = read_tools.iter().map(|n| (*n).to_string()).collect();
    develop_tools.extend(
        ["write_file", "edit", "bash", "todo_write"]
            .iter()
            .map(|n| (*n).to_string()),
    );
    let mut verify_tools: Vec<String> = read_tools.iter().map(|n| (*n).to_string()).collect();
    verify_tools.extend(["bash", "todo_write"].iter().map(|n| (*n).to_string()));
    let profiles = vec![
        SubagentProfile {
            schema_version: SUBAGENT_SCHEMA_VERSION,
            id: "explore".to_string(),
            name: "只读探索".to_string(),
            description: "查找实现、理解调用链、定位影响范围；只读调查，不修改项目。".to_string(),
            instructions: "先建立事实：用检索与阅读定位实现、调用链与影响范围。\n\
                           只报告有证据的结论：给出文件路径与关键行，明确标注未知项与推测。\n\
                           不要修改任何文件；需要改动时把建议交回父代理。"
                .to_string(),
            enabled: true,
            tools: ToolSelection::Allowlist {
                names: dedup_tool_names(read_tools.iter().map(|n| (*n).to_string()).collect()),
            },
            model: ModelChoice::Inherit,
            permission_ceiling: PermissionCeiling::ReadOnly,
            color: Some(ProfileColor::Blue),
        },
        SubagentProfile {
            schema_version: SUBAGENT_SCHEMA_VERSION,
            id: "develop".to_string(),
            name: "开发执行".to_string(),
            description:
                "实现有明确范围的功能、修复，或按父代理提供/指定的已确认计划逐项落地。"
                    .to_string(),
            instructions: "先确认范围与验收条件，再动手；只改动任务要求的模块。\n\
                           每完成一项就自检：读回改动、执行相关测试或最小验证。\n\
                           按计划执行时报告完成项、偏离原因与阻塞；不调用 exit_plan，不自行扩展计划。\n\
                           结束前交代：改了什么、怎么验证的、还剩什么问题。"
                .to_string(),
            enabled: true,
            tools: ToolSelection::Allowlist {
                names: dedup_tool_names(develop_tools),
            },
            model: ModelChoice::Inherit,
            permission_ceiling: PermissionCeiling::Inherit,
            color: Some(ProfileColor::Green),
        },
        SubagentProfile {
            schema_version: SUBAGENT_SCHEMA_VERSION,
            id: "verify".to_string(),
            name: "验证".to_string(),
            description: "审查改动、运行测试/构建、复现错误并核验验收条件。".to_string(),
            instructions: "以验收条件为准逐项核验，优先运行真实命令而不是只读代码。\n\
                           如实报告：命令、退出码、关键输出与复现步骤；失败就报失败，不粉饰。\n\
                           发现实现问题只报告证据与最小修复建议，不自行改实现。"
                .to_string(),
            enabled: true,
            tools: ToolSelection::Allowlist {
                names: dedup_tool_names(verify_tools),
            },
            model: ModelChoice::Inherit,
            permission_ceiling: PermissionCeiling::Inherit,
            color: Some(ProfileColor::Purple),
        },
    ];
    profiles
}

/// 去重且保持首现顺序。
pub fn dedup_tool_names(names: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .filter(|name| seen.insert(name.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_grammar_matches_the_documented_shape() {
        assert!(is_valid_subagent_id("explore"));
        assert!(is_valid_subagent_id("api-reviewer"));
        assert!(is_valid_subagent_id("a1"));
        assert!(!is_valid_subagent_id(""));
        assert!(!is_valid_subagent_id("-lead"));
        assert!(!is_valid_subagent_id("Upper"));
        assert!(!is_valid_subagent_id("has_underscore"));
        assert!(!is_valid_subagent_id("dot.dot"));
        assert!(!is_valid_subagent_id("../escape"));
        assert!(!is_valid_subagent_id(&"a".repeat(ID_MAX_CHARS + 1)));
    }

    #[test]
    fn qualified_ids_round_trip_and_reject_traversal() {
        let parsed = QualifiedProfileId::parse("project:api-reviewer").unwrap();
        assert_eq!(parsed.source, ProfileSource::Project);
        assert_eq!(parsed.to_string(), "project:api-reviewer");
        assert!(QualifiedProfileId::parse("nope:explore").is_err());
        assert!(QualifiedProfileId::parse("explore").is_err());
        assert!(QualifiedProfileId::parse("user:../escape").is_err());
        assert!(QualifiedProfileId::parse("user:").is_err());
    }

    #[test]
    fn empty_allowlist_is_zero_tools_not_inherit() {
        let empty = ToolSelection::Allowlist { names: Vec::new() };
        assert!(empty.is_empty_allowlist());
        assert_eq!(empty.summarize(), "无工具");
        assert_ne!(empty, ToolSelection::Inherit);
        let inherited = ToolSelection::Inherit;
        assert!(!inherited.is_empty_allowlist());
        assert_eq!(inherited.summarize(), "继承父工具");
    }

    #[test]
    fn selection_serializes_with_internal_mode_tag() {
        let value = serde_json::to_value(ToolSelection::Allowlist {
            names: vec!["read_file".into()],
        })
        .unwrap();
        assert_eq!(value["mode"], "allowlist");
        assert_eq!(value["names"][0], "read_file");
        let value = serde_json::to_value(ToolSelection::Inherit).unwrap();
        assert_eq!(value["mode"], "inherit");
        // 反序列化必须拒绝未知 mode，不能悄悄退化成 inherit。
        assert!(
            serde_json::from_value::<ToolSelection>(serde_json::json!({"mode": "all"})).is_err()
        );
    }

    #[test]
    fn model_choice_defaults_to_inherit() {
        // 省略整个 model 字段 = inherit（定义文件里的常见写法）。
        let profile: SubagentProfile = serde_json::from_value(serde_json::json!({
            "schemaVersion": 1,
            "id": "x",
            "name": "X",
            "description": "d",
            "tools": {"mode": "inherit"}
        }))
        .unwrap();
        assert_eq!(profile.model, ModelChoice::Inherit);
        // 给了 model 就必须给 mode：含糊的空对象不接受。
        assert!(serde_json::from_value::<ModelChoice>(serde_json::json!({})).is_err());
        let explicit: ModelChoice = serde_json::from_value(serde_json::json!({
            "mode": "explicit",
            "selection": {"provider": "p", "model": "m"}
        }))
        .unwrap();
        assert_eq!(explicit.selection().unwrap().model, "m");
        // provider/model 必须成套：只有一半的显式模型无效。
        assert!(
            SubagentProfile {
                schema_version: SUBAGENT_SCHEMA_VERSION,
                id: "x".into(),
                name: "X".into(),
                description: "d".into(),
                instructions: String::new(),
                enabled: true,
                tools: ToolSelection::Inherit,
                model: ModelChoice::Explicit {
                    selection: crate::config::ModelSelection {
                        provider: "p".into(),
                        model: String::new(),
                        reasoning_effort: None,
                    },
                },
                permission_ceiling: PermissionCeiling::Inherit,
                color: None,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn builtin_profiles_are_valid_and_cover_the_three_roles() {
        let profiles = builtin_subagent_profiles();
        let ids: Vec<&str> = profiles.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, vec!["explore", "develop", "verify"]);
        for profile in &profiles {
            profile.validate().unwrap_or_else(|issues| {
                panic!("内置定义 {} 非法：{issues:?}", profile.id);
            });
            let ToolSelection::Allowlist { names } = &profile.tools else {
                panic!("内置定义必须显式列出工具");
            };
            // 去重。
            let mut sorted = names.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), names.len(), "内置工具列表必须去重");
            // 硬禁项绝不出现在任何内置定义里。
            for name in names {
                assert!(!child_hard_denied(name), "内置定义包含硬禁工具：{name}");
            }
        }
        let explore = &profiles[0];
        assert_eq!(explore.permission_ceiling, PermissionCeiling::ReadOnly);
        let ToolSelection::Allowlist { names } = &explore.tools else {
            unreachable!()
        };
        assert!(!names.contains(&"bash".to_string()));
        assert!(!names.contains(&"write_file".to_string()));
        assert!(!names.contains(&"edit".to_string()));
        // 探索预设初始不授予 browser（可交互扩展不该被标成严格只读）。
        assert!(!names.contains(&"browser".to_string()));
        let develop = &profiles[1];
        let ToolSelection::Allowlist { names } = &develop.tools else {
            unreachable!()
        };
        assert!(names.contains(&"write_file".to_string()));
        assert!(names.contains(&"edit".to_string()));
        assert!(names.contains(&"bash".to_string()));
        // 开发/验证初始不开后台 jobs/browser/MCP/ask。
        for name in names {
            assert!(!name.starts_with("job_"), "develop 不应默认开 jobs：{name}");
            assert_ne!(name, "browser");
            assert_ne!(name, "ask");
        }
        let verify = &profiles[2];
        let ToolSelection::Allowlist { names } = &verify.tools else {
            unreachable!()
        };
        assert!(names.contains(&"bash".to_string()));
        assert!(!names.contains(&"write_file".to_string()));
        assert!(!names.contains(&"edit".to_string()));
    }

    #[test]
    fn hard_denied_covers_delegation_session_control_and_host_admin() {
        for name in [
            "spawn_agent",
            "fork_agent",
            "interrupt_agent",
            "list_agents",
            "wait_agent",
            "exit_plan",
            "get_goal",
            "update_goal",
            "create_preset",
        ] {
            assert!(child_hard_denied(name), "{name} 必须被硬禁");
        }
        // 汇报通道保留。
        assert!(!child_hard_denied("send_message"));
        // 普通读写工具不受硬禁影响。
        for name in [
            "read_file",
            "write_file",
            "edit",
            "bash",
            "todo_write",
            "ask",
        ] {
            assert!(!child_hard_denied(name), "{name} 不该被硬禁");
        }
        // 未来新增的 `mcp__*` 派遣入口按前缀无法判断，但分类函数对已知
        // 派遣名生效——这里断言分类本身覆盖全部硬禁项。
        assert_eq!(tool_capability("spawn_agent"), ToolCapability::Delegation);
        assert_eq!(
            tool_capability("create_preset"),
            ToolCapability::HostAdministration
        );
    }

    #[test]
    fn validate_reports_every_issue_with_stable_codes() {
        let bad = SubagentProfile {
            schema_version: 99,
            id: "Bad Id".into(),
            name: String::new(),
            description: String::new(),
            instructions: String::new(),
            enabled: true,
            tools: ToolSelection::Allowlist {
                names: vec!["read_file".into(), "read_file".into(), String::new()],
            },
            model: ModelChoice::Inherit,
            permission_ceiling: PermissionCeiling::Inherit,
            color: None,
        };
        let issues = bad.validate().unwrap_err();
        let fields: Vec<&str> = issues
            .iter()
            .filter_map(|issue| issue.field.as_deref())
            .collect();
        for expected in ["schemaVersion", "id", "name", "description", "tools.names"] {
            assert!(
                fields.contains(&expected),
                "缺少 {expected} 的诊断:{fields:?}"
            );
        }
        assert!(issues.iter().all(|i| i.code == codes::PROFILE_INVALID));
    }
}
