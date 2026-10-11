//! 任务验证账本的工具面:`run_checks`。
//!
//! 账本只剩"有证据的完成判定"这一条链路,而且只有一件工具:
//!
//! - `run_checks` 由宿主真的跑命令:命令、期望退出码、工作目录、超时由模型
//!   给出,退出码与冻结的输入指纹由宿主填写,结论由退出码算出(见
//!   [`ValidationResult::from_check_runs`]),落成 `RecordValidation` 事件。
//!
//! 模型侧没有读账本、写账本的入口:目标、验收项、事实、待办、收口那批记账工具
//! 已经裁掉。模型能做的只有"跑检查";"做完了"这个判断由宿主按最新一条结论裁决
//! (见 [`ValidationResult`] 与 `denia_core::task::TaskState::outcome`)。
//!
//! ## 状态的真值只有一处
//!
//! 工具层不读会话日志、不缓存账本:它通过 [`TaskLedgerHost`] 向宿主问当前
//! 折叠快照与新的 revision 身份。所以工具层既不依赖会话存储 crate,也不会造出
//! 第二份会漂移的状态。
//!
//! ## 检查的输入快照
//!
//! `run_checks` 冻结的输入快照**只覆盖工作目录与文件内容指纹**(复用
//! [`crate::read_state::FileStamp`] 的 `(mtime, size, digest)` 取样口径)。
//! 仓库里没有环境变量快照这个概念:依赖环境变量的检查不受指纹保护,改了
//! 环境而没改文件时旧结论不会被自动推翻 —— 这是已知边界,不假装有。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use denia_core::session::SessionEvent;
use denia_core::task::{
    CheckRun, FileFingerprint, MAX_CHECK_COMMAND_CHARS, RevisionId, TaskOp, TaskState,
    ValidationResult,
};
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::read_state::FileStamp;
use crate::shell::{self, CommandSpec, DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS, StreamSpec};
use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput, end_reason};

/// 检查默认冻结的输入文件条数上限(省缺输入快照取本会话读过的文件)。
const MAX_INPUTS: usize = 32;
/// 结果里最多列出的输入路径条数(完整清单在日志的验证结论里)。
const MAX_LISTED_INPUTS: usize = 8;

/// 宿主侧的账本接口:读当前折叠快照、分配不可复用的 revision。
///
/// 由宿主(会话驱动器)实现并挂到 [`ToolContext::task_ledger`]。工具层只经过
/// 它接触会话,因此"账本长什么样"与"身份从哪来"都留在宿主那一侧。
pub trait TaskLedgerHost: Send + Sync {
    /// 当前折叠出的账本;`None` = 日志里还没有宿主写下的验证结论。
    fn state(&self) -> Option<TaskState>;

    /// 分配 revision id。**必须来自不会复用的来源**:沿用旧 id 会让旧分支的
    /// 验证结论被当成当前版本的结论继承下来(见 [`RevisionId`] 的契约)。
    fn new_revision_id(&self) -> RevisionId;
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 取账本宿主;没有会话归属的调用读不到账本,显式拒绝而不是静默作用于别处。
fn require_host(ctx: &ToolContext) -> Result<&Arc<dyn TaskLedgerHost>, ToolOutput> {
    ctx.task_ledger.as_ref().ok_or_else(|| {
        tool_error(
            "本次调用读不到会话任务账本(没有归属的代理会话)",
            "run_checks 只能在代理会话内使用",
        )
    })
}

/// 取会话事件通道;结论要落盘才算数,写不进去就拒绝。
fn require_sink(ctx: &ToolContext) -> Result<&crate::SessionEventSink, ToolOutput> {
    ctx.emit_event.as_ref().ok_or_else(|| {
        tool_error(
            "本次调用没有会话事件通道,验证结论无法落盘",
            "run_checks 只能在代理会话内使用",
        )
    })
}

/// 检查命令文本:去空白、拒绝空文本与超长文本。
///
/// 超长**拒绝**而不是静默截断:账本只放结论性短句(命令 + 退出码),完整命令
/// 与输出留在回复、会话日志和产物文件里;让折叠层替模型砍掉半条命令,读的人
/// 分不清跑的到底是什么。
fn check_command(raw: &str) -> Result<String, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("检查命令不能为空".to_string());
    }
    if text.chars().count() > MAX_CHECK_COMMAND_CHARS {
        return Err(format!(
            "检查命令超过 {MAX_CHECK_COMMAND_CHARS} 字符上限:账本只放结论性短句,把长脚本写进文件再跑它"
        ));
    }
    Ok(text.to_string())
}

/// 这次取证该绑在哪个 revision 上。
///
/// 有当前身份就沿用(同一轮工作里的多次取证绑同一条 revision,彼此可比);
/// 没有、或当前身份已被 rewind 报废(旧分支的身份,结论不得继承),就换一个
/// **新的** —— 复用报废 id 等于把旧分支的结论算到新分支头上。
fn bind_revision(host: &dyn TaskLedgerHost) -> RevisionId {
    match host.state() {
        Some(state) if !state.revision_retired => state.revision,
        _ => host.new_revision_id(),
    }
}

// ---------------------------------------------------------------------------
// run_checks
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CheckArg {
    command: String,
    /// 通过所需的退出码;缺省 0(非零即失败)。
    #[serde(default)]
    expect_exit_code: Option<i32>,
    /// 运行时的工作目录(相对会话工作区或绝对路径,必须落在工作区内)。
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
struct RunChecksArgs {
    checks: Vec<CheckArg>,
    /// 本次验证要冻结内容的输入文件;缺省时取本会话读过 / 写过的文件。
    #[serde(default)]
    inputs: Vec<String>,
}

/// `run_checks`:跑检查、产结构化结果、把验证结论落成账本事件。
#[derive(Default)]
pub struct RunChecksTool;

impl RunChecksTool {
    fn build_schema() -> ToolSchema {
        ToolSchema {
            name: "run_checks".to_string(),
            description: "按检查定义真的跑命令,并把结论记进任务验证账本(结论由退出码算出,不是可以填的字段)。每条检查:command 必填,expect_exit_code 默认 0(不符即该检查未通过),workdir 默认会话工作区,timeout_ms 默认 120000 / 上限 600000。inputs 声明本次验证要冻结内容的输入文件(相对工作区路径);省略时冻结本会话读过/写过的文件。\n账本上的结局(未验证完成 / 验收通过 / 验收失败)由宿主按**最新一条**有效结论裁决:你**不能**自称验证通过,也不能挑一条通过的就当完成 —— 检查失败就如实报告失败。\n输入快照只覆盖工作目录与文件内容指纹:仓库没有环境变量快照,依赖环境变量的检查不受指纹保护。命令没跑起来(启动失败/超时/被中断)都按**未通过**记,不会算通过。"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "checks": {
                        "type": "array",
                        "minItems": 1,
                        "description": "要跑的检查,按顺序执行;每项是一次命令运行。",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "command": {"type": "string", "description": "检查命令(宿主 shell 方言),在 workdir 里执行。"},
                                "expect_exit_code": {"type": "integer", "description": "通过所需的退出码,默认 0;非零即失败。"},
                                "workdir": {"type": "string", "description": "运行时的工作目录(相对会话工作区或绝对路径,必须落在工作区内);默认会话工作区。"},
                                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": MAX_TIMEOUT_MS, "description": "单条检查的超时(毫秒),默认 120000。"}
                            },
                            "required": ["command"]
                        }
                    },
                    "inputs": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "本次验证冻结内容的输入文件(相对工作区路径):这些字节变了,结论就失效。省略时冻结本会话读过/写过的文件。"
                    }
                },
                "required": ["checks"]
            }),
        }
    }
}

#[async_trait]
impl Tool for RunChecksTool {
    fn schema(&self) -> &ToolSchema {
        static SCHEMA: std::sync::OnceLock<ToolSchema> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(Self::build_schema)
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: RunChecksArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    "参数必须是 JSON 对象,必填 checks(数组,每项含 command)",
                );
            }
        };
        if args.checks.is_empty() {
            return tool_error("checks 不能为空", "至少给一条检查命令");
        }
        let host = match require_host(ctx) {
            Ok(host) => host,
            Err(output) => return output,
        };
        let sink = match require_sink(ctx) {
            Ok(sink) => sink,
            Err(output) => return output,
        };
        let revision = bind_revision(host.as_ref());
        let inputs = match resolve_inputs(&args.inputs, ctx) {
            Ok(inputs) => inputs,
            Err(message) => {
                return tool_error(message, "inputs 里的路径必须存在于会话工作区内");
            }
        };

        let mut runs: Vec<CheckRun> = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        let mut started = 0usize;
        for check in &args.checks {
            let command = match check_command(&check.command) {
                Ok(command) => command,
                Err(message) => return tool_error(message, "补上命令后重试"),
            };
            let expect = check.expect_exit_code.unwrap_or(0);
            let workdir = match resolve_workdir(check.workdir.as_deref(), ctx) {
                Ok(workdir) => workdir,
                Err(message) => return tool_error(message, "workdir 必须是工作区内的路径"),
            };
            let timeout_ms = check
                .timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .min(MAX_TIMEOUT_MS);
            // 输入指纹在**每次运行开始时**冻结:检查自己改了文件时,后续检查
            // 冻的是改后的字节,结论分别绑在各自看到的那份内容上。
            let fingerprints = freeze_inputs(&inputs);
            let run = shell::run_command(
                CommandSpec {
                    command: &command,
                    cwd: &workdir,
                    timeout_ms,
                    output_store: ctx.output_store.as_ref(),
                    stream: Some(StreamSpec {
                        sink: ctx.emit_event.clone(),
                        call_id: ctx.call_id.clone(),
                    }),
                    preview_chars: (1_500, 3_000),
                    timeout_hint: "可拆小这条检查或调大它的 timeout_ms",
                },
                &ctx.cancel,
            )
            .await;
            if run.ended == end_reason::CANCELLED {
                // 被中断的运行不是验收结果:记一条失败会把"用户喊停"说成
                // "验收失败",所以整轮不落结论,如实告诉模型重跑。
                return tool_error(
                    format!("检查被中断,本轮没有记录验证结论(已跑 {} 条)", runs.len()),
                    "重跑 run_checks;中断那次的结果不作数",
                );
            }
            if run.not_started {
                let reason = run
                    .error
                    .clone()
                    .unwrap_or_else(|| "命令没跑起来".to_string());
                lines.push(format!("  - [未启动] {command} —— {reason}"));
                failures.push(format!("$ {command}(未启动:{reason})"));
            } else {
                started += 1;
                let passed = run.exit_code == Some(expect);
                let code = run
                    .exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "无退出码(超时或被中止)".to_string());
                lines.push(format!(
                    "  - [{}] {command}(退出码 {code},期望 {expect})",
                    if passed { "通过" } else { "失败" }
                ));
                if !passed {
                    let mut body = format!("$ {command}\n退出码:{code}\n{}", run.stdout);
                    if !run.stderr.is_empty() {
                        body.push_str(&format!("\n--- stderr ---\n{}", run.stderr));
                    }
                    failures.push(body);
                }
            }
            runs.push(CheckRun {
                command,
                exit_code: run.exit_code,
                expect_exit_code: expect,
                workdir: display_path(&ctx.cwd, &workdir),
                fingerprints,
            });
        }

        let passed = runs.iter().all(CheckRun::passed);
        let Some(result) = ValidationResult::from_check_runs(
            revision.clone(),
            runs,
            ctx.session_id.clone(),
            now_ms(),
        ) else {
            return tool_error("没有可记录的检查运行", "至少给一条检查命令");
        };
        sink(SessionEvent::Task {
            op: TaskOp::RecordValidation { result },
        });

        let mut content = String::new();
        content.push_str(&format!(
            "检查结果:{}({} 条检查)\n",
            if passed {
                "验收通过"
            } else {
                "验收失败"
            },
            lines.len()
        ));
        for line in &lines {
            content.push_str(line);
            content.push('\n');
        }
        content.push_str(&format!(
            "已记入账本:revision {},冻结 {} 个输入指纹。\n",
            revision.as_str(),
            inputs.len()
        ));
        if !inputs.is_empty() {
            content.push_str(&format!(
                "冻结的输入:{}{}\n",
                inputs
                    .iter()
                    .take(MAX_LISTED_INPUTS)
                    .map(|(_, path)| path.clone())
                    .collect::<Vec<String>>()
                    .join("、"),
                if inputs.len() > MAX_LISTED_INPUTS {
                    format!(" 等 {} 个", inputs.len())
                } else {
                    String::new()
                }
            ));
        }
        if passed {
            content
                .push_str("结局由账本按最新一条有效结论裁决:再跑一次失败的检查,这条通过就作废。\n");
        }
        if !failures.is_empty() {
            content.push_str("\n未通过的检查输出:\n");
            for body in &failures {
                content.push_str(&body.replace('\n', "\n  "));
                content.push('\n');
            }
        }
        ToolOutput {
            content,
            // 全部命令都没能启动 = 这个环境根本跑不了检查(工具层面的问题);
            // 只要有一条真的跑过,失败就是检查结果,不是工具错误。
            is_error: started == 0,
            artifact: None,
            report: Some(crate::ExecutionReport {
                exit_code: None,
                end_reason: Some(if started == 0 {
                    end_reason::SPAWN_FAILED.to_string()
                } else {
                    end_reason::COMPLETED.to_string()
                }),
                files: Vec::new(),
                evidence_complete: None,
            }),
        }
    }
}

/// 解析要冻结的输入:显式清单按工作区内的路径解析;缺省时取本会话读过 /
/// 写过的文件(那正是"这次验证依据的输入")。
fn resolve_inputs(raw: &[String], ctx: &ToolContext) -> Result<Vec<(PathBuf, String)>, String> {
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    if raw.is_empty() {
        let Some(shared) = &ctx.read_state else {
            return Ok(out);
        };
        let Ok(state) = shared.lock() else {
            return Ok(out);
        };
        let mut paths: Vec<PathBuf> = state
            .recent_writable(MAX_INPUTS)
            .into_iter()
            .map(|(key, _)| key.path)
            .collect();
        paths.sort();
        paths.dedup();
        for path in paths {
            // 缺省输入只冻结仍然存在的文件:读过的文件后来被删掉不是错误。
            if digest_of(&path).is_some() {
                let display = display_path(&ctx.cwd, &path);
                out.push((path, display));
            }
        }
        out.sort_by(|a, b| a.1.cmp(&b.1));
        return Ok(out);
    }
    for item in raw {
        let path = crate::resolve_within(&ctx.cwd, item, ctx.confined)?;
        // 显式声明的输入必须读得到:声明了却不存在的输入应当当场失败,
        // 而不是冻结一个空指纹、让"这些字节变了"的判定到时候失效。
        if digest_of(&path).is_none() {
            return Err(format!("输入文件不存在或读不到:{item}"));
        }
        let display = display_path(&ctx.cwd, &path);
        if !out.iter().any(|(_, existing)| existing == &display) {
            out.push((path, display));
        }
    }
    Ok(out)
}

/// 冻结输入指纹(逐个读内容算 digest);读不到的文件跳过 —— 冻结不到就不假装
/// 冻结过,后续"输入变了"的判定自然也不会引用它。
fn freeze_inputs(inputs: &[(PathBuf, String)]) -> Vec<FileFingerprint> {
    inputs
        .iter()
        .filter_map(|(path, display)| {
            digest_of(path).map(|digest| FileFingerprint {
                path: display.clone(),
                digest,
            })
        })
        .collect()
}

/// 文件内容指纹:复用读状态表的取样口径([`FileStamp`]),digest 取十六进制。
fn digest_of(path: &Path) -> Option<String> {
    let stamp = FileStamp::of(path)?;
    stamp
        .fingerprint
        .map(|fingerprint| format!("{:016x}", fingerprint.as_u64()))
}

/// 路径口径:工作区内记相对路径(与 `ExecutionReport::files` 同口径,分隔符统一
/// 为 `/`),工作区外记绝对路径。指纹比对([`TaskState::paths_changed_since`])按
/// 这同一口径认路径,两边不一致就认不出"这些字节变了"。
fn display_path(cwd: &Path, path: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().to_string(),
    }
}

/// 检查的工作目录:缺省会话工作区,显式给出时必须是工作区内的路径。
fn resolve_workdir(raw: Option<&str>, ctx: &ToolContext) -> Result<PathBuf, String> {
    match raw.map(str::trim).filter(|raw| !raw.is_empty()) {
        Some(raw) => crate::resolve_within(&ctx.cwd, raw, ctx.confined),
        None => Ok(ctx.cwd.clone()),
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------
//
// 这一层要证明的是**工具面本身的边界**,以及它写出的记录在裁决层怎么算:
//
// 1. schema 是闭合的,参数面里没有"填结论"的位置;
// 2. 结论由真实命令的退出码算出(不 mock 执行链),没跑起来的命令按未通过记;
// 3. 写出的结论绑着当时的 revision 与输入指纹;当前身份被 rewind 报废时换新的;
// 4. 每次调用只落一条宿主写的 `RecordValidation` 事件。
//
// 其中几条走**真实命令**(`exit 0` / `exit 3` / 不存在的可执行文件 / 不存在的工作目
// 录),不 mock 执行链 —— 与 `bash.rs` 的既有测试同一路数(一次性进程、stdin 为
// null)。它们依赖宿主有可用的 shell;若某个环境没有,替代品是绕过工具、直接拿
// `CheckRun` + `ValidationResult::from_check_runs` 断言裁决,但那样就测不到
// "工具真的跑了命令、按退出码算出结论"这层。
//
// 关于覆盖面的一句实话:结局裁决住在 `denia_core::task::TaskState::outcome` /
// `verification` 里,`crates/session` 的 `project_task_outcome` 只是
// `project_task(events).map(|state| state.outcome())`。tools crate 的依赖里
// **没有** denia-session(也没有 dev-dependency),所以这里对着同一份核心裁决
// 函数断言,而不是重新实现一遍折叠 —— 重新实现只会测出另一份语义。

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolContext;
    use denia_core::session::{PermissionMode, SessionEvent};
    use denia_core::task::{
        LedgerOverflow, StaleReason, TaskOutcome, ValidationVerdict, VerificationState,
    };
    use std::sync::Mutex;
    use tokio_util::sync::CancellationToken;

    /// 假宿主:只提供工具层真正问的两件事(折叠快照、新 revision 身份)。
    /// 它不折叠事件 —— 落不落盘看传给工具的事件 sink,那正是"模型有没有写出
    /// 结论"的判据。
    struct FakeHost {
        state: Option<TaskState>,
        allocated: Mutex<u32>,
    }

    impl FakeHost {
        fn new(state: Option<TaskState>) -> Arc<Self> {
            Arc::new(Self {
                state,
                allocated: Mutex::new(0),
            })
        }
    }

    impl TaskLedgerHost for FakeHost {
        fn state(&self) -> Option<TaskState> {
            self.state.clone()
        }

        fn new_revision_id(&self) -> RevisionId {
            let mut count = self.allocated.lock().unwrap();
            *count += 1;
            RevisionId::new(format!("rev-{}", *count))
        }
    }

    fn ctx_with(
        dir: &Path,
        ledger: Option<Arc<FakeHost>>,
        events: &Arc<Mutex<Vec<SessionEvent>>>,
    ) -> ToolContext {
        let sink: crate::SessionEventSink = {
            let events = events.clone();
            Arc::new(move |event| events.lock().unwrap().push(event))
        };
        ToolContext {
            output_store: None,
            session_id: None,
            selection: None,
            cwd: dir.to_path_buf(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: Some(sink),
            file_history: None,
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
            task_ledger: ledger.map(|host| host as Arc<dyn TaskLedgerHost>),
            read_state: None,
        }
    }

    /// 一个已经折出来的账本:当前 revision + 一条冻结指纹(无验证结论)。
    fn ledger_state(revision: &str) -> TaskState {
        TaskState {
            revision: RevisionId::new(revision),
            revision_retired: false,
            session: None,
            validations: Vec::new(),
            fingerprints: Vec::new(),
            overflow: LedgerOverflow::default(),
            created_at: 1,
            updated_at: 1,
        }
    }

    fn task_ops(events: &Arc<Mutex<Vec<SessionEvent>>>) -> Vec<TaskOp> {
        events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                SessionEvent::Task { op } => Some(op.clone()),
                _ => None,
            })
            .collect()
    }

    fn validation_results(events: &Arc<Mutex<Vec<SessionEvent>>>) -> Vec<ValidationResult> {
        task_ops(events)
            .into_iter()
            .filter_map(|op| match op {
                TaskOp::RecordValidation { result } => Some(result),
                TaskOp::Legacy => None,
            })
            .collect()
    }

    /// 跑一次 `run_checks`,返回工具输出与它落进账本的全部验证结论。
    async fn run_checks_with(
        host: &Arc<FakeHost>,
        dir: &Path,
        arguments: serde_json::Value,
    ) -> (ToolOutput, Vec<ValidationResult>) {
        let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::default();
        let ctx = ctx_with(dir, Some(host.clone()), &events);
        let out = RunChecksTool.execute(&arguments.to_string(), &ctx).await;
        (out, validation_results(&events))
    }

    /// 以指定退出码结束的短命令:Windows 走 `pwsh -Command`、Unix 走 `bash -c`,
    /// 两边 `exit N` 都是"以 N 退出"。
    ///
    /// 刻意不用 `cmd /C exit N`:实测经 pwsh 转发后拿到的是 1 而不是 N,拿它当
    /// 退出码来源会把"退出码决定结论"这条测试测成另一个东西。
    fn exit_with(code: i32) -> String {
        format!("exit {code}")
    }

    /// schema 是闭合的,且没有任何地方能填结论。
    #[test]
    fn schema_is_closed_and_holds_no_verdict_field() {
        let checks = RunChecksTool.schema();
        assert_eq!(checks.name, "run_checks");
        assert_eq!(checks.parameters["type"], serde_json::json!("object"));
        assert_eq!(
            checks.parameters["additionalProperties"],
            serde_json::json!(false),
            "run_checks 必须 additionalProperties: false"
        );
        // 参数面逐字段点名:只有"跑什么、冻结什么",没有 verdict/result 这类
        // "写结论"的位置。
        let mut declared: Vec<&str> = checks.parameters["properties"]
            .as_object()
            .expect("run_checks 的 properties")
            .keys()
            .map(String::as_str)
            .collect();
        declared.sort();
        assert_eq!(declared, vec!["checks", "inputs"]);
        let mut one_check: Vec<&str> =
            checks.parameters["properties"]["checks"]["items"]["properties"]
                .as_object()
                .expect("checks 项的 properties")
                .keys()
                .map(String::as_str)
                .collect();
        one_check.sort();
        assert_eq!(
            one_check,
            vec!["command", "expect_exit_code", "timeout_ms", "workdir"]
        );
        assert_eq!(
            checks.parameters["properties"]["checks"]["items"]["additionalProperties"],
            serde_json::json!(false)
        );
    }

    /// 结论由退出码算出:相等即通过,不等即失败(包括非零的期望码)。
    #[tokio::test]
    async fn run_checks_derives_the_verdict_from_the_exit_code() {
        let host = FakeHost::new(Some(ledger_state("rev-1")));

        // 期望 3、实际 3 → 通过:通过与否只看"实际退出码 == 期望退出码"。
        let (out, results) = run_checks_with(
            &host,
            &std::env::temp_dir(),
            serde_json::json!({
                "checks": [{"command": exit_with(3), "expect_exit_code": 3}],
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("验收通过"), "{}", out.content);
        assert_eq!(results.len(), 1, "一次调用落一条验证结论");
        assert_eq!(results[0].runs().len(), 1);
        assert_eq!(results[0].runs()[0].exit_code, Some(3));
        assert_eq!(results[0].runs()[0].expect_exit_code, 3);
        assert!(results[0].runs()[0].passed());
        assert_eq!(results[0].verdict(), Some(ValidationVerdict::Passed));

        // 期望 0、实际 3 → 失败。工具本身不算出错(命令确实跑完了),失败的是检查。
        let (out, results) = run_checks_with(
            &host,
            &std::env::temp_dir(),
            serde_json::json!({
                "checks": [{"command": exit_with(3)}],
            }),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("验收失败"), "{}", out.content);
        assert_eq!(results[0].runs()[0].exit_code, Some(3));
        assert_eq!(results[0].runs()[0].expect_exit_code, 0);
        assert!(!results[0].runs()[0].passed());
        assert_eq!(results[0].verdict(), Some(ValidationVerdict::Failed));
    }

    /// 命令没跑起来,绝不能算通过。
    ///
    /// 两条"没跑起来"的路:命令名不存在(shell 起来了、那条命令没有),以及工作目录
    /// 不存在(进程根本没启动,拿到的是没有退出码的 `not_started`)。两条都按未通过记。
    #[tokio::test]
    async fn run_checks_never_passes_a_command_that_did_not_run() {
        let host = FakeHost::new(Some(ledger_state("rev-1")));

        let (out, results) = run_checks_with(
            &host,
            &std::env::temp_dir(),
            serde_json::json!({
                "checks": [{"command": "denia-no-such-binary-9f3a --version"}],
            }),
        )
        .await;
        assert_eq!(results.len(), 1, "没跑起来也要落号:按未通过记");
        let run = &results[0].runs()[0];
        assert_ne!(run.exit_code, Some(0), "找不到的命令不可能拿到 0");
        assert!(!run.passed());
        assert_eq!(results[0].verdict(), Some(ValidationVerdict::Failed));
        assert!(out.content.contains("验收失败"), "{}", out.content);

        // 工作目录不存在 → 进程起不来:`not_started`,记录里没有退出码。
        let (out, results) = run_checks_with(
            &host,
            &std::env::temp_dir(),
            serde_json::json!({
                "checks": [{"command": exit_with(0), "workdir": "denia-missing-dir-9f3a"}],
            }),
        )
        .await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].runs()[0].exit_code, None);
        assert!(!results[0].runs()[0].passed());
        assert_eq!(results[0].verdict(), Some(ValidationVerdict::Failed));
        assert!(
            out.is_error,
            "一条都没跑起来 = 这个环境跑不了检查:{}",
            out.content
        );
        assert!(out.content.contains("[未启动]"), "{}", out.content);
        assert_eq!(
            out.report
                .as_ref()
                .and_then(|report| report.end_reason.as_deref()),
            Some(end_reason::SPAWN_FAILED)
        );
    }

    /// 结论绑住当时的 revision 与输入字节:改了输入,新记录算出的指纹就该变;
    /// 折叠层据此把旧结论判成"输入已变",不再当可用结论。
    #[tokio::test]
    async fn run_checks_freezes_the_revision_and_the_input_bytes() {
        let dir = std::env::temp_dir().join(format!("denia-task-inputs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("input.txt");
        std::fs::write(&input, "v1").unwrap();

        let host = FakeHost::new(Some(ledger_state("rev-1")));
        let arguments = serde_json::json!({
            "checks": [{"command": exit_with(0)}],
            "inputs": ["input.txt"],
        });

        let (out, first) = run_checks_with(&host, &dir, arguments.clone()).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].revision().as_str(), "rev-1", "结论绑当前 revision");
        let frozen = first[0].fingerprints();
        assert_eq!(frozen.len(), 1, "声明的输入要被冻进结论");
        assert_eq!(frozen[0].path, "input.txt", "路径按工作区内的相对口径记");
        let before = frozen[0].digest.clone();
        assert!(!before.is_empty());

        std::fs::write(&input, "v2").unwrap();
        let (out, second) = run_checks_with(&host, &dir, arguments).await;
        assert!(!out.is_error, "{}", out.content);
        let after = second[0].fingerprints()[0].digest.clone();
        assert_ne!(after, before, "改过的输入必须算出不同指纹");
        assert_eq!(second[0].fingerprints()[0].path, "input.txt");

        // 折叠侧:指纹基线被后续变更改到 v2 之后,绑 v1 的旧结论不再是可用结论。
        let mut state = ledger_state("rev-1");
        state.validations = vec![first[0].clone()];
        state.fingerprints = vec![FileFingerprint {
            path: "input.txt".to_string(),
            digest: after.clone(),
        }];
        assert_eq!(
            state.verification(),
            VerificationState::Unverified {
                reason: StaleReason::InputsChanged {
                    paths: vec!["input.txt".to_string()],
                },
            }
        );
        assert_eq!(state.outcome(), TaskOutcome::Unverified);

        // 基线仍是验证时那份字节时,同一条结论才是可用的通过结论。
        state.fingerprints = vec![FileFingerprint {
            path: "input.txt".to_string(),
            digest: before,
        }];
        assert_eq!(state.outcome(), TaskOutcome::Passed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 身份规则:有当前 revision 就沿用,已经被 rewind 报废就换一个新的 ——
    /// 复用报废 id 等于把旧分支的结论算到新分支头上。
    #[tokio::test]
    async fn run_checks_reuses_the_live_revision_and_replaces_a_retired_one() {
        let arguments = serde_json::json!({"checks": [{"command": exit_with(0)}]});

        let live = FakeHost::new(Some(ledger_state("rev-live")));
        let (_, results) = run_checks_with(&live, &std::env::temp_dir(), arguments.clone()).await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].revision().as_str(),
            "rev-live",
            "同一轮工作里的取证绑同一条 revision"
        );

        let mut retired = ledger_state("rev-old");
        retired.revision_retired = true;
        let host = FakeHost::new(Some(retired));
        let (out, results) = run_checks_with(&host, &std::env::temp_dir(), arguments).await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(results.len(), 1);
        assert_ne!(
            results[0].revision().as_str(),
            "rev-old",
            "报废身份不得复用"
        );
        assert_eq!(
            results[0].revision().as_str(),
            "rev-1",
            "换成宿主新分配的身份"
        );

        // 没有账本(第一次取证):同样由宿主分配新身份。
        let fresh = FakeHost::new(None);
        let (out, results) = run_checks_with(
            &fresh,
            &std::env::temp_dir(),
            serde_json::json!({"checks": [{"command": exit_with(0)}]}),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].revision().as_str(), "rev-1");
    }

    /// 工具层只写得动宿主那条事件:整轮下来账本上只有 `RecordValidation`。
    #[tokio::test]
    async fn run_checks_writes_only_the_host_validation_event() {
        let events: Arc<Mutex<Vec<SessionEvent>>> = Arc::default();
        let host = FakeHost::new(Some(ledger_state("rev-1")));
        let ctx = ctx_with(&std::env::temp_dir(), Some(host), &events);
        let out = RunChecksTool
            .execute(
                &serde_json::json!({"checks": [{"command": exit_with(0)}]}).to_string(),
                &ctx,
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let ops = task_ops(&events);
        assert_eq!(ops.len(), 1, "一次调用落一条任务事件:{ops:?}");
        assert!(matches!(ops[0], TaskOp::RecordValidation { .. }));
    }
}
