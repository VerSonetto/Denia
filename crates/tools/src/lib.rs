//! Model-facing tools: `bash`, `read_file`, `write_file`.
//!
//! Every failure mode (bad arguments, spawn errors, timeouts, path escapes)
//! normalizes into an `is_error` output — tools never panic and never return
//! a Rust error, so one tool call always yields exactly one model-facing
//! result.

mod ask;
mod bash;
mod browser;
pub mod capabilities;
pub mod coordination;
mod edit;
mod files;
pub mod glob;
mod goal;
pub mod grep;
mod ls;
pub mod mcp;
pub mod output;
pub mod permission;
mod plan;
pub mod prompt;
pub mod read_state;
pub mod runtime_command;
pub mod shell;
pub mod shell_session;
pub mod support;
mod task;
mod todo;
mod web_fetch;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use denia_core::session::{PermissionMode, SessionEvent};
use denia_core::tool::ToolSchema;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

pub use ask::{AskTool, DEFAULT_TIMEOUT_MS as ASK_DEFAULT_TIMEOUT_MS};
pub use bash::BashTool;
pub use browser::{BrowserExecute, BrowserHub, BrowserTool};
pub use edit::EditTool;
pub use files::{ReadFileTool, WriteFileTool};
pub use glob::GlobTool;
pub use goal::{GetGoalTool, UpdateGoalTool};
pub use grep::GrepTool;
pub use ls::LsTool;
pub use mcp::{MCP_LIST_TOOL, McpListTool, McpTool, mcp_list_schema};
pub use plan::{ExitPlanArgs, ExitPlanTool};
pub use prompt::{
    default_shipped, default_shipped_with_browser, default_shipped_with_browser_and_ask,
    register_ask_prompt_section, register_capability_prompt_sections, register_code_style_section,
    register_communication_section, register_context_management_section,
    register_mcp_prompt_section, register_mcp_prompt_section_with_manager,
    register_memory_prompt_section, register_preset_prompt_section, register_risk_honesty_section,
    register_shipped_prompt, register_task_prompt_section, register_working_style_section,
    render_mcp_section, render_memory_section, shipped_with_persona,
    shipped_with_persona_and_browser_and_ask,
};
pub use shell_session::{Captured, PersistentShell, ShellHub};
pub use task::{GetTaskTool, RunChecksTool, TaskLedgerHost, UpdateTaskTool};
pub use todo::TodoWriteTool;
pub use web_fetch::WebFetchTool;

/// Session-event sink handed to tools that emit log-only state: the todo
/// snapshot (`todo_write`), goal operations (`update_goal`) and `bash`'s live
/// output increments (`tool-output-chunk`, 只服务实时观感、不进模型历史).
/// The agent loop wires it to the session append + broadcast; tools never
/// touch the session handle directly.
pub type SessionEventSink = Arc<dyn Fn(SessionEvent) + Send + Sync>;

/// 历史只读集合（旧日志与旧参数路径的保守上限）。
///
/// **不是**当前子代理策略：现在的子代理按定义（内置 explore/develop/verify
/// 或用户自定义）声明自己的工具，只受"父可授予工具 ∩ 定义请求 − 硬禁项"约束。
///
/// 这个常量只服务两条遗留路径：
/// 1. 读旧日志里的 `SubagentDescriptor`（缺 `effectiveTools` 时按它保守构造）；
/// 2. 兼容旧的 `allowed_tools` 参数（显式列表最多与它相交，绝不放开写与命令）。
///
/// 权威定义在 `denia_core::subagent::LEGACY_READ_ONLY_TOOLS`；这里只是别名，
/// 避免两处各写一份导致旧日志的解释漂移。
pub const SUBAGENT_READ_ONLY_TOOLS: &[&str] = denia_core::subagent::LEGACY_READ_ONLY_TOOLS;

/// 提问通道:宿主实现,把 `ask` 工具的提问挂到会话的挂起表并等待用户应答。
///
/// 与 [`ApprovalBridge`](denia_agent_loop::ApprovalBridge) 同构,但语义更宽:
/// 审批是二值决策、由 driver 触发;提问是任意问题组、由模型工具触发,且必须
/// 有超时与结局区分(见 [`denia_core::session::AskOutcome`])。
#[async_trait]
pub trait AskBridge: Send + Sync {
    /// 阻塞等待用户作答;`cancel` 触发时返回 `Cancelled`。
    async fn ask(
        &self,
        session_id: &str,
        request_id: &str,
        call_id: &str,
        questions: &[denia_core::session::AskQuestion],
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> denia_core::session::AskResolution;
}

/// 文件历史后端:写文件类工具在真正落盘前调用,由宿主实现备份原文件。
#[async_trait]
pub trait FileHistoryBackend: Send + Sync {
    /// 记录一次即将发生的写操作;实现应在文件被修改前备份原内容。
    async fn track_before_write(&self, path: &std::path::Path) -> Result<(), String>;
}

/// Execution context handed to every tool call.
#[derive(Clone)]
pub struct ToolContext {
    /// 宿主授予的会话身份，绝不从模型参数接收。
    pub session_id: Option<String>,
    pub selection: Option<denia_core::config::ModelSelection>,
    /// The session's working directory; relative paths anchor here.
    pub cwd: PathBuf,
    /// Cancels the running call when the turn is aborted.
    pub cancel: CancellationToken,
    /// Sandbox: confine file access to `cwd`. Off allows absolute paths
    /// outside the workspace.
    pub confined: bool,
    /// Whether the current model is marked as vision-capable; `read_file`
    /// injects images only when true.
    pub vision_supported: bool,
    /// Log-only event sink; `None` for callers with no owning session.
    pub emit_event: Option<SessionEventSink>,
    /// 文件回退备份后端;`None` 表示当前调用不参与文件快照。
    pub file_history: Option<Arc<dyn FileHistoryBackend>>,
    /// 会话权限模式(四档);写类门控由派发处的策略引擎统一执行,
    /// 工具内部不再自行判权,此值仅供提示词与个别场景参考。
    pub permission_mode: PermissionMode,
    /// 提问通道(`ask` 工具用);`None` 表示该调用无人可问。
    pub ask: Option<Arc<dyn AskBridge>>,
    /// 当前工具调用 id(`ask` 用它把提问卡片挂到对应工具行上)。
    pub call_id: Option<String>,
    /// 会话目标读取器(`get_goal` 用);`None` 表示当前调用无法读取
    /// 会话目标状态。返回 `(当前目标快照, 激活后的 token 用量)`。
    pub goal_reader:
        Option<Arc<dyn Fn() -> Option<(denia_core::session::GoalState, u64)> + Send + Sync>>,
    /// 会话任务账本的宿主接口(`get_task` / `update_task` / `run_checks` 用)。
    ///
    /// `None` = 当前调用没有归属的代理会话(单测、宿主配置面):三个工具会
    /// 以"读不到账本"显式拒绝,而不是静默把操作落到别处。折叠快照与 id
    /// 分配都由宿主给([`TaskLedgerHost`]),工具层不读会话日志。
    pub task_ledger: Option<Arc<dyn TaskLedgerHost>>,
    /// 会话共享的文件读取状态表。
    ///
    /// `read_file` 用它做重复读取去重;`write_file`/`edit` 用它做写前
    /// "读过且新鲜"校验。`None` 表示当前调用不参与去重(独立工具调用、
    /// 测试场景),此时行为与引入该机制之前完全一致。
    pub read_state: Option<read_state::SharedReadState>,
    pub output_store: Option<Arc<output::OutputStore>>,
}

/// One model-facing tool outcome.
///
/// `content` / `is_error` 是**模型面**的事实;`artifact` 与 `report` 是给界面、
/// 日志与诊断用的旁证,不进派生历史。
#[derive(Default)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub artifact: Option<output::OutputArtifact>,
    /// 结构化执行信息;不执行命令的工具(读文件、ls、……)恒为 `None`。
    pub report: Option<ExecutionReport>,
}

impl ToolOutput {
    /// 成功结果。
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            artifact: None,
            report: None,
        }
    }

    /// 错误结果;错误文本应当可执行(原因 + 修复建议)。
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            artifact: None,
            report: None,
        }
    }

    /// 挂上结构化执行信息。
    ///
    /// 只描述**实际发生了什么**(退出码、结束原因),不改变 `is_error`:
    /// 命令正常返回非零是命令自己的结果,不是工具失败。
    pub fn with_report(mut self, report: ExecutionReport) -> Self {
        self.report = Some(report);
        self
    }
}

/// 一次工具调用的结构化执行信息。
///
/// 与 [`ToolOutput::content`] 的分工:`content` 是给模型看的文本,这里是给
/// 界面与诊断看的**事实**。退出码此前只存在于文案首行(见 `bash` 的
/// "退出码: N"),界面要显示它就得反过来解析展示文本 —— 文案一改就断。
/// 这里给它一个不必解析的载体。
///
/// 各字段都可缺省:只有真正跑过命令的工具才报得出退出码,而"不适用"与
/// "取值为 0"必须能分开,所以退出码是 `Option` 而不是哨兵值(`-1` 之类的
/// 哨兵只是文案里的展示约定,不是退出码)。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionReport {
    /// 命令类工具的退出码;`None` = 不适用或未取得。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// 结束原因,取值见 [`end_reason`]。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<String>,
    /// 本次调用影响的文件(相对 `cwd`);推断不出就留空,不要猜。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// 输出是否完整保留;`None` = 没有产物可比对(如未启用输出存储)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_complete: Option<bool>,
}

/// [`ExecutionReport::end_reason`] 的取值。
///
/// 用字符串常量而不是 enum:这个值要原样落进 `meta`,前端、日志与测试按
/// 同一套字面量比对,多一层 serde 映射只会多一处能对不上的地方。
pub mod end_reason {
    /// 命令跑完并拿到了退出码(非零也算 —— 那是命令的结果,不是工具失败)。
    pub const COMPLETED: &str = "completed";
    /// 超过 `timeout_ms` 被终止,没有退出码。
    pub const TIMEOUT: &str = "timeout";
    /// 用户中断,没有退出码。
    pub const CANCELLED: &str = "cancelled";
    /// 命令没能启动,或等待其结束失败。
    pub const SPAWN_FAILED: &str = "spawn_failed";
}

impl ExecutionReport {
    /// 正常结束(拿到或拿不到退出码,都算跑完了)。
    pub fn completed(exit_code: Option<i32>) -> Self {
        Self {
            exit_code,
            end_reason: Some(end_reason::COMPLETED.to_string()),
            ..Default::default()
        }
    }

    /// 未正常结束(超时 / 中断 / 启动或等待失败):没有退出码可报。
    pub fn ended(reason: &str) -> Self {
        Self {
            end_reason: Some(reason.to_string()),
            ..Default::default()
        }
    }

    /// 补上产物的完整性事实。
    pub fn with_evidence_complete(mut self, complete: bool) -> Self {
        self.evidence_complete = Some(complete);
        self
    }
}

/// A model-facing capability.
#[async_trait]
pub trait Tool: Send + Sync {
    /// The declaration sent to the model.
    fn schema(&self) -> &ToolSchema;

    /// Executes one call; `arguments` is the raw JSON string from the model.
    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput;
}

/// The deployment's tool set, in registration order.
#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn replace(&mut self, tool: Arc<dyn Tool>) {
        if let Some(existing) = self
            .tools
            .iter_mut()
            .find(|t| t.schema().name == tool.schema().name)
        {
            *existing = tool;
        } else {
            self.register(tool);
        }
    }
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Model-facing schemas in registration order.
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools
            .iter()
            .map(|tool| tool.schema().clone())
            .collect()
    }

    /// 已注册的工具名(注册顺序)。
    ///
    /// 注册表热替换时用它枚举保留项(并剔除 `mcp__*` 前缀的旧 MCP 工具),
    /// 避免重建把 server 侧的定制实现(如带 runtime 的 bash)冲掉。
    pub fn names(&self) -> Vec<String> {
        self.tools
            .iter()
            .map(|tool| tool.schema().name.clone())
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools
            .iter()
            .find(|tool| tool.schema().name == name)
            .cloned()
    }
}

/// The shipped tool set: bash + read_file + write_file + todo_write + ls + glob + grep + edit + exit_plan + web_fetch.
pub fn default_registry() -> ToolRegistry {
    let mut registry = ToolRegistry::default();
    registry.register(Arc::new(BashTool::default()));
    registry.register(Arc::new(ReadFileTool::default()));
    registry.register(Arc::new(output::ReadToolOutput::default()));
    registry.register(Arc::new(WriteFileTool::default()));
    registry.register(Arc::new(TodoWriteTool::default()));
    registry.register(Arc::new(LsTool::default()));
    registry.register(Arc::new(GlobTool::default()));
    registry.register(Arc::new(GrepTool::default()));
    registry.register(Arc::new(EditTool::default()));
    registry.register(Arc::new(ExitPlanTool));
    registry.register(Arc::new(GetGoalTool));
    registry.register(Arc::new(UpdateGoalTool));
    registry.register(Arc::new(GetTaskTool::default()));
    registry.register(Arc::new(UpdateTaskTool::default()));
    registry.register(Arc::new(RunChecksTool::default()));
    registry.register(Arc::new(WebFetchTool::new()));
    registry
}

/// Shipped tool set + `ask` 工具(有应答通道的部署注册)。
pub fn default_registry_with_ask() -> ToolRegistry {
    let mut registry = default_registry();
    registry.register(Arc::new(AskTool::new()));
    registry
}

/// Shipped tool set + optional browser tool(`hub` 提供时注册 `browser`)。
pub fn default_registry_with_browser(hub: Option<BrowserHub>) -> ToolRegistry {
    let mut registry = default_registry();
    if let Some(hub) = hub {
        registry.register(Arc::new(BrowserTool::new(hub)));
    }
    registry
}

/// 把当前 MCP 快照里的工具追加进注册表。
///
/// 模型可见与可执行必须一致:放进注册表的名字就是快照里的 `mcp__*`
/// 修饰名(快照负责过滤掉禁用/冲突的工具),派发处因此能 `get()` 到。
/// 由 server 在 MCP 配置变更后用新的 `ToolRegistry` 整体替换 driver 的
/// 注册表,保证一个 step 内工具面一致。
pub fn register_mcp_tools(registry: &mut ToolRegistry, manager: &Arc<denia_mcp::McpManager>) {
    let snapshot = manager.snapshot();
    for (name, description, schema) in snapshot.tool_defs() {
        registry.register(Arc::new(mcp::McpTool::new(
            name,
            description,
            schema,
            manager.clone(),
        )));
    }
}

/// Resolves one raw path for a tool call.
///
/// 绝对路径(`C:\…`、`\\server\share\…`、`/…`)与相对路径(`src/main.rs`、
/// `./a/b`)同样接受,这是模型两种写法都能落到正确位置的前提:
/// - 相对路径锚定 `cwd`;绝对路径从根起算,**盘符/UNC 前缀原样保留**;
/// - 两种形式都按组件消解 `.` 与 `..`,弹到根之外即拒绝;
/// - 盘符相对路径(`D:foo`)锚定的是该盘当前目录而非会话工作区,语义含糊,
///   明确拒绝而不是静默落到别处(fail loud);
/// - `confined`(沙箱)开启时解析结果必须落在 `cwd` 内,否则拒绝;
///   Windows 路径大小写不敏感,`d:/work` 与 `D:\work` 视为同一位置。
pub(crate) fn resolve_within(cwd: &Path, raw: &str, confined: bool) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("路径不能为空".to_string());
    }
    let raw_path = Path::new(raw);
    if !raw_path.is_absolute() && matches!(raw_path.components().next(), Some(Component::Prefix(_)))
    {
        return Err(format!(
            "路径 '{raw}' 是盘符相对路径(如 \"D:foo\"),不受支持"
        ));
    }
    // 绝对路径从零起算(首个 Prefix/RootDir 负责落根),相对路径从 cwd 起算。
    let mut out = if raw_path.is_absolute() {
        PathBuf::new()
    } else {
        cwd.to_path_buf()
    };
    for component in raw_path.components() {
        match component {
            // Prefix 与 RootDir 必须 `push` 拼接:`PathBuf::push` 的语义是
            // 「带根无前缀者替换除前缀外的全部」,所以 `C:` + `\` 得到 `C:\`。
            // 整体替换(`out = PathBuf::from(...)`)会把盘符吃掉——`D:\a\b`
            // 解析成 `\a\b`,沙箱内被误判越界、沙箱外静默指向当前盘的同名路径。
            Component::Prefix(_) | Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return Err(format!("路径 '{raw}' 越出了会话工作区"));
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if confined && !within(cwd, &out) {
        return Err(format!("路径 '{raw}' 越出了会话工作区(沙箱开启)"));
    }
    Ok(out)
}

/// 沙箱边界判定:`out` 是否落在 `base` 之内。
///
/// 按组件比较(不是字符串前缀,`/work` 不会误吞 `/workshop`);Windows 上
/// 再按 ASCII 折叠比较一次,免得模型写的小写盘符/目录名被误判为越界。
/// 判错的方向是"多拒绝一次",不会放大权限。
fn within(base: &Path, out: &Path) -> bool {
    if out.starts_with(base) {
        return true;
    }
    #[cfg(windows)]
    {
        let mut out_components = out.components();
        for base_component in base.components() {
            match out_components.next() {
                Some(component)
                    if component
                        .as_os_str()
                        .eq_ignore_ascii_case(base_component.as_os_str()) => {}
                _ => return false,
            }
        }
        // base 组件全部命中即算在内;base 为空路径时不算(避免把任意路径都当"在空边界内")。
        base.components().count() > 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// 宽容参数解析:只取第一个 JSON 值,忽略尾部垃圾。
/// 模型偶尔在参数后吐多余字符,硬失败会浪费一整步。
/// 工具实现请优先用 [`support::parse_tool_args`](数值强转 + 路径别名);
/// crate 外自定义工具(如 server 侧)也可直接用本函数保持同一口径。
pub fn parse_args_lenient<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T, String> {
    let mut iter = serde_json::Deserializer::from_str(raw.trim()).into_iter::<T>();
    match iter.next() {
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => Err(error.to_string()),
        None => Err("参数为空".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_within_anchors_and_rejects_escapes() {
        let cwd = if cfg!(windows) {
            PathBuf::from("C:\\work")
        } else {
            PathBuf::from("/work")
        };
        let inside = resolve_within(&cwd, "src/main.rs", true).unwrap();
        assert!(inside.starts_with(&cwd));
        assert!(resolve_within(&cwd, "../secret", true).is_err());
        assert!(resolve_within(&cwd, "a/../../secret", true).is_err());
        // Unconfined sessions may leave the workspace.
        assert!(resolve_within(&cwd, "../elsewhere", false).is_ok());
        assert!(resolve_within(&cwd, "", true).is_err());
        assert!(resolve_within(&cwd, "   ", true).is_err());
    }

    /// 绝对路径与相对路径必须都能解析:相对锚定 cwd,绝对保留盘符/根,
    /// `.`/`..` 在组件层消解,越界按 confined 放行或拒绝。
    #[test]
    fn resolve_within_handles_absolute_and_relative_forms() {
        let cwd = if cfg!(windows) {
            PathBuf::from("D:\\work")
        } else {
            PathBuf::from("/work")
        };
        // 相对路径 + `.` / `..` 消解。
        assert_eq!(
            resolve_within(&cwd, "src/main.rs", true).unwrap(),
            cwd.join("src/main.rs")
        );
        assert_eq!(
            resolve_within(&cwd, "./src/../lib.rs", true).unwrap(),
            cwd.join("lib.rs")
        );
        assert_eq!(resolve_within(&cwd, ".", true).unwrap(), cwd);

        if cfg!(windows) {
            // 工作区内的绝对路径必须可用(旧实现吃掉盘符后会被误判越界)。
            assert_eq!(
                resolve_within(&cwd, "D:\\work\\src\\main.rs", true).unwrap(),
                PathBuf::from("D:\\work\\src\\main.rs")
            );
            // 正斜杠 + 小写盘符:Windows 大小写不敏感,不该误拒。
            assert_eq!(
                resolve_within(&cwd, "d:/work/src/main.rs", true).unwrap(),
                PathBuf::from("d:/work/src/main.rs")
            );
            // 绝对路径里的 `..` 在边界内消解。
            assert_eq!(
                resolve_within(&cwd, "D:\\work\\src\\..\\lib.rs", true).unwrap(),
                PathBuf::from("D:\\work\\lib.rs")
            );
            // 工作区之外的绝对路径:沙箱拒绝……
            assert!(resolve_within(&cwd, "C:\\Users\\me\\x.txt", true).is_err());
            // ……非沙箱放行,且盘符完好(旧实现会变成 `\Users\me\x.txt`)。
            assert_eq!(
                resolve_within(&cwd, "C:\\Users\\me\\x.txt", false).unwrap(),
                PathBuf::from("C:\\Users\\me\\x.txt")
            );
            // UNC 前缀保留服务器与共享名。
            assert_eq!(
                resolve_within(&cwd, "\\\\srv\\share\\a", false).unwrap(),
                PathBuf::from("\\\\srv\\share\\a")
            );
            // 盘符相对路径语义含糊,直接拒绝而不是静默落到该盘当前目录。
            assert!(resolve_within(&cwd, "D:foo", false).is_err());
        } else {
            assert_eq!(
                resolve_within(&cwd, "/work/src/main.rs", true).unwrap(),
                PathBuf::from("/work/src/main.rs")
            );
            assert!(resolve_within(&cwd, "/etc/passwd", true).is_err());
            assert!(resolve_within(&cwd, "/etc/passwd", false).is_ok());
        }
    }

    #[test]
    fn within_is_component_wise_and_case_insensitive_on_windows() {
        if cfg!(windows) {
            // 组件比较,不是字符串前缀。
            assert!(!within(Path::new("D:\\work"), Path::new("D:\\workshop\\a")));
            assert!(within(Path::new("D:\\work"), Path::new("D:\\work\\a")));
            assert!(within(Path::new("D:\\work"), Path::new("D:\\work")));
            // 大小写折叠。
            assert!(within(
                Path::new("D:\\Code\\Work"),
                Path::new("d:\\code\\work\\a.rs")
            ));
            assert!(!within(Path::new("D:\\work"), Path::new("E:\\work")));
        } else {
            assert!(!within(Path::new("/work"), Path::new("/workshop/a")));
            assert!(within(Path::new("/work"), Path::new("/work/a")));
        }
    }

    #[test]
    fn lenient_parse_ignores_trailing_junk() {
        #[derive(serde::Deserialize)]
        struct Args {
            path: String,
        }
        let parsed: Args = parse_args_lenient(r#"{"path": "app/src"} 谢谢"#).unwrap();
        assert_eq!(parsed.path, "app/src");
        assert!(parse_args_lenient::<Args>("not json").is_err());
    }
}
