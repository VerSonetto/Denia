//! The session driver: turn/step loop over the llm registry and tools.
//!
//! A step is one model request plus the tools it called; a turn keeps
//! stepping while the assistant message carries tool-call blocks and closes
//! on a tool-call-free message. Every event is appended to the session log
//! first and only then mirrored to `emit` — the log is the ordering
//! authority. A turn whose appends fail mid-flight is left open; session
//! load closes it with a synthetic aborted `turn-end`.
//!
//! 模块划分(按职责,`impl SessionDriver` 分散在各文件):
//! - [`turn`] — turn/step 主编排(终止判定、死循环检测接线);
//! - [`injections`] — 每 step 的背景注入管线(通道幂等);
//! - [`request`] — 请求构造 + attempt 重试环 + 失败分流;
//! - [`exec`] — 工具并行执行池 + 升权审批 + 结果兜底截断;
//! - [`errors`] — 失败分类处置与用户可读的中文报错摘要;
//! - [`loop_guard`] — 死循环检测(文本/工具两条指纹线);
//! - [`rescue`] — 文本伪工具调用救援;
//! - [`compact`] — LLM 总结压缩(压力驱动);
//! - [`microcompact`] — 工具结果微压缩(占位符替换,摘要前的廉价清理)。

mod compact;
mod errors;
mod exec;
mod injections;
mod loop_guard;
pub mod microcompact;
pub mod preset;
mod request;
mod rescue;
mod runtime_context;
mod turn;
mod workspace_instructions;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use denia_core::config::{LlmCallConfig, ModelSelection};
use denia_core::error::{LlmFailure, codes};
use denia_core::session::{
    RequestHeaderSnapshot, SessionEnvelope, SessionEvent, TurnEndReason,
};
use denia_llm::LlmRegistry;
use denia_session::Session;
use denia_system_prompt::SystemPrompt;
use denia_tools::{FileHistoryBackend, ToolRegistry};
use tokio_util::sync::CancellationToken;

pub use compact::{CompactOutcome, CompactionSettings};
pub use injections::GOAL_CHANNEL;
pub use loop_guard::LOOP_THRESHOLD;
pub use preset::AgentPresetSource;

/// 工具并行执行配置(学 codex `ToolCallRuntime` + dsh `maxParallelToolCalls`)。
///
/// 一次 step 内模型返回的多个工具调用并发执行,受滚动池上限约束;结果仍按
/// model-order 提交,保证每个 call 恰好一条 `ToolResult` 事件(事件源不变量)。
/// 需要审批的调用(升权)串行化——审批是交互式阻塞,并发弹窗会交错事件。
#[derive(Debug, Clone, PartialEq)]
pub struct ParallelSettings {
    /// 同时执行的工具调用上限(对齐 dsh `maxParallelToolCalls=10`)。
    pub max_parallel_tool_calls: usize,
}

impl Default for ParallelSettings {
    fn default() -> Self {
        Self {
            max_parallel_tool_calls: 10,
        }
    }
}

/// 文件历史提供者:server 侧实现,按会话提供备份句柄并在用户消息落库后
/// 固化快照。`None` 表示该部署不启用文件回退。
#[async_trait]
pub trait FileHistoryProvider: Send + Sync {
    /// 返回当前会话的工具备份句柄。
    async fn backend(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
    ) -> Option<Arc<dyn FileHistoryBackend>>;
    /// 用户消息落库后调用,固化该消息对应的文件快照。
    async fn snapshot(
        &self,
        session_id: &str,
        cwd: &std::path::Path,
        message_seq: u64,
    ) -> Result<(), String>;
}

/// 审批通道:driver 在遇到策略 Ask 决策(越界写文件、计划提交)时,向
/// 宿主请求一次用户决策。实现方负责把请求挂到 `LiveSession` 的 pending
/// 表并等待 REST 应答;取消 token 发生时实现方应返回 `Cancelled` 结局。
/// 计划批准的决策可携带执行档位/模型(由 driver 落会话事件与 TurnState)。
#[async_trait]
pub trait ApprovalBridge: Send + Sync {
    async fn request(
        &self,
        session_id: &str,
        request_id: &str,
        cancel: CancellationToken,
    ) -> denia_core::session::PlanReviewDecision;
}

/// Drives user turns on one session at a time.
pub struct SessionDriver {
    runtime: Option<Arc<dyn denia_tools::capabilities::AgentRuntime>>,
    registry: Arc<LlmRegistry>,
    /// 工具注册表。
    ///
    /// 用 `ArcSwap` 而不是裸 `Arc`:MCP 服务器配置变更会增删工具,整份
    /// 注册表整体替换而不是原地增删——读到的永远是一份自洽的工具面,
    /// 不会出现"提示词里有这个工具、注册表里却没了"的窗口期。
    tools: Arc<ArcSwap<ToolRegistry>>,
    /// 工具面变更时对外广播(供 server 侧刷新系统提示词的 MCP schema)。
    system_prompt: Arc<ArcSwap<SystemPrompt>>,
    file_history: Option<Arc<dyn FileHistoryProvider>>,
    approval: Option<Arc<dyn ApprovalBridge>>,
    /// 提问通道(`ask` 工具用);`None` 时该工具按 unavailable 结算。
    ask: Option<Arc<dyn denia_tools::AskBridge>>,
    /// agent preset 名册:按会话记录中的 preset 收窄工具面与 persona。
    /// `None` 时所有会话都运行出厂全量组装(无 preset 的部署)。
    presets: Option<Arc<dyn AgentPresetSource>>,
    /// 层叠上下文管理:LLM 总结压缩(学 dsh 压力驱动 + Claude Code compact)。
    compaction: CompactionSettings,
    /// 工具结果微压缩:摘要之前的廉价清理。
    microcompact: microcompact::MicrocompactSettings,
    /// 连续压缩失败计数(熔断,学 Claude Code `MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES`)。
    compact_failures: AtomicU32,
    /// 连续"压缩后快速回填"计数(rapid-refill 断路器)。
    rapid_refills: AtomicU32,
    /// 上次压缩后经过的工具轮次(用于判定回填是否过快)。
    tool_turns_since_compact: AtomicU32,
    /// 工具并行执行配置(学 codex `ToolCallRuntime`)。
    parallel: ParallelSettings,
    /// 死循环检测状态(按会话):跨 turn 记忆——"用户手动继续后模型又
    /// 重复同样内容"的场景只能靠跨轮记忆抓住。同一会话的轮次由 server
    /// 的 running 位串行化,锁只保护跨会话的并发访问。
    loop_guards: std::sync::Mutex<std::collections::HashMap<String, loop_guard::LoopGuard>>,
    /// 文件读取状态(按会话):重复读取去重 + 写前新鲜度校验。
    /// 同一会话的工具调用共享一张表。
    read_states: std::sync::Mutex<
        std::collections::HashMap<String, denia_tools::read_state::SharedReadState>,
    >,
    /// 会话级审批放行表(按会话 × 操作类别):自动编辑档下用户对某类
    /// 审批(bash 写/删除/区外写)选"本窗口放行"后,同类操作在本会话
    /// 内直接放行不再询问。运行时内存态,不落盘,进程重启后自然失效。
    ask_grants: std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<denia_tools::permission::ActionClass>>>,
    /// 已装载完整参数定义的 MCP 工具(按会话):两段式工具面的第二段。
    /// 未装载的 `mcp__*` 工具在请求里只带名称与描述,首次调用拦截返回
    /// 参数定义并登记到此表,之后 schema 全量进请求、调用正常执行。
    /// 运行时内存态:进程重启或会话淘汰后回到轻量态,重新装载即可。
    mcp_loads: std::sync::Mutex<std::collections::HashMap<String, std::collections::HashSet<String>>>,
}

impl SessionDriver {
    pub fn new(
        registry: Arc<LlmRegistry>,
        tools: Arc<ToolRegistry>,
        system_prompt: Arc<ArcSwap<SystemPrompt>>,
    ) -> Self {
        Self {
            registry,
            tools: Arc::new(ArcSwap::from_pointee((*tools).clone())),
            system_prompt,
            runtime: None,
            file_history: None,
            approval: None,
            ask: None,
            presets: None,
            compaction: CompactionSettings::default(),
            microcompact: microcompact::MicrocompactSettings::default(),
            compact_failures: AtomicU32::new(0),
            rapid_refills: AtomicU32::new(0),
            tool_turns_since_compact: AtomicU32::new(0),
            parallel: ParallelSettings::default(),
            loop_guards: std::sync::Mutex::new(std::collections::HashMap::new()),
            read_states: std::sync::Mutex::new(std::collections::HashMap::new()),
            ask_grants: std::sync::Mutex::new(std::collections::HashMap::new()),
            mcp_loads: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn with_runtime(
        mut self,
        runtime: Arc<dyn denia_tools::capabilities::AgentRuntime>,
    ) -> Self {
        self.runtime = Some(runtime);
        self
    }

    /// 覆盖层叠上下文管理配置(LLM 压缩;默认值见
    /// [`CompactionSettings::default`])。
    pub fn with_compaction(mut self, settings: CompactionSettings) -> Self {
        self.compaction = settings;
        self
    }

    /// 覆盖工具结果微压缩配置(默认值见
    /// [`MicrocompactSettings::default`])。
    pub fn with_microcompact(mut self, settings: microcompact::MicrocompactSettings) -> Self {
        self.microcompact = settings;
        self
    }

    /// 当前微压缩配置(server 侧构造 driver 时同步设置)。
    pub fn microcompact_settings(&self) -> microcompact::MicrocompactSettings {
        self.microcompact.clone()
    }

    /// 摘要阈值 token 数(供微压缩取 90% 作为自己的触发线)。
    ///
    /// 与 [`compact::should_compact`] 同口径:有效窗口 = 窗口 - 输出预留,
    /// 阈值 = 有效窗口 - buffer。
    pub(crate) fn compaction_threshold_tokens(
        &self,
        pressure: &denia_token_meter::ContextPressure,
    ) -> Option<u64> {
        let window = pressure.context_window?;
        if window == 0 {
            return None;
        }
        let reserve = self.compaction.summary_max_tokens.min(window);
        let effective = window.saturating_sub(reserve);
        Some(effective.saturating_sub(13_000))
    }

    /// 覆盖工具并行执行配置(默认值见 [`ParallelSettings::default`])。
    pub fn with_parallel(mut self, settings: ParallelSettings) -> Self {
        self.parallel = settings;
        self
    }

    /// 启用文件历史:回退功能依赖此提供者。
    pub fn with_file_history(mut self, provider: Arc<dyn FileHistoryProvider>) -> Self {
        self.file_history = Some(provider);
        self
    }

    /// 启用审批通道(抄 dsh ctx.approval);无通道时升权请求 fail-closed。
    pub fn with_approval(mut self, bridge: Arc<dyn ApprovalBridge>) -> Self {
        self.approval = Some(bridge);
        self
    }

    /// 启用提问通道(`ask` 工具);无通道时该工具按 `unavailable` 结算,
    /// 模型仍能自行决策继续(不像 dsh 那样把整次调用变成硬错误)。
    pub fn with_ask(mut self, bridge: Arc<dyn denia_tools::AskBridge>) -> Self {
        self.ask = Some(bridge);
        self
    }

    /// 启用 agent preset 名册:会话按自己日志里的 preset 组装工具面与 persona。
    pub fn with_presets(mut self, presets: Arc<dyn AgentPresetSource>) -> Self {
        self.presets = Some(presets);
        self
    }

    /// 解析某会话应当运行的组装。
    ///
    /// 会话未指定 preset 时用部署默认;日志里的 id 已从名册消失(用户删掉
    /// 它、或文件损坏)时同样回退到默认组装——运行中的会话不该因为一个坏
    /// 文件就连请求都发不出去,名册会把这个事实以 broken 行呈现给用户。
    pub fn preset_for(
        &self,
        session_preset: Option<&str>,
    ) -> Option<denia_core::preset::AgentPreset> {
        let source = self.presets.as_ref()?;
        let requested = session_preset
            .map(str::to_string)
            .unwrap_or_else(|| source.default_id());
        source
            .resolve(&requested)
            .or_else(|| source.resolve(&source.default_id()))
    }

    /// 会话生效 preset 的功能开关(无名册部署 = 全开)。注入通道、压缩闸门
    /// 与 goal 续跑等"装配之外"的联动统一从这里取开关,保证与工具面收窄
    /// 读到的是同一份声明。
    pub fn preset_features(
        &self,
        session_preset: Option<&str>,
    ) -> denia_core::preset::PresetFeatures {
        self.preset_for(session_preset)
            .map(|preset| preset.features)
            .unwrap_or_default()
    }

    /// 当前工具注册表(读路径无锁,拿到的是一份自洽快照)。
    pub fn tools(&self) -> Arc<ToolRegistry> {
        Arc::clone(&self.tools.load())
    }

    /// 整体替换工具注册表(MCP 服务器配置变更后由 server 调用)。
    ///
    /// 已在飞的工具调用持有旧注册表的 `Arc`,不受影响;新 step 的装配与
    /// 派发都用新表,模型可见与可执行在同一次替换里对齐。
    pub fn replace_tools(&self, registry: ToolRegistry) {
        self.tools.store(Arc::new(registry));
    }

    /// 当前层叠上下文管理配置(server 热更新用)。
    pub fn compaction_settings(&self) -> CompactionSettings {
        self.compaction.clone()
    }

    /// 供 server 端热加载写入同一 `ArcSwap`。
    pub fn system_prompt_handle(&self) -> Arc<ArcSwap<SystemPrompt>> {
        self.system_prompt.clone()
    }

    /// 估算当前提示词的组成部分大小(字节):系统提示词(含模型框架)与
    /// 工具声明。供前端"上下文窗口占用"面板展示占比。
    pub fn prompt_parts(
        &self,
        cwd: &str,
        selection: &ModelSelection,
    ) -> Result<(usize, usize), String> {
        use denia_system_prompt::AssembleContext;
        let assembly = self
            .system_prompt
            .load()
            .assemble(&AssembleContext {
                cwd: Some(cwd.to_string()),
                model: Some(selection.model.clone()),
                provider: Some(selection.provider.clone()),
                ..Default::default()
            })
            .map_err(|error| error.to_string())?;
        let system = denia_system_prompt::render_prompt(&assembly);
        let framed = denia_system_prompt::frame_system_prompt_for_model(&system);
        let tools = serde_json::to_string(&assembly.tools).map_err(|error| error.to_string())?;
        Ok((system.len() + framed.len(), tools.len()))
    }

    /// 手动压缩(上下文面板按钮):从日志最近一次请求头部恢复请求形态
    /// (模型 / 系统提示 / 工具集),执行与自动路径完全相同的总结压缩。
    /// 绕过压力闸门 —— 用户主动触发,不要求压力 ≥ `compact_ratio`。
    /// 会话组装关闭了压缩功能时拒绝(与自动闸门同一开关)。
    ///
    /// 调用方负责:运行位占用与 cancel 生命周期、提前拒绝无请求历史的
    /// 会话、以及 `CompactionSummary` 事件落盘与广播。
    pub async fn compact_manually(
        &self,
        session: &Arc<Session>,
        cancel: CancellationToken,
    ) -> Result<Option<CompactOutcome>, LlmFailure> {
        if !self
            .preset_features(session.agent_preset().as_deref())
            .compaction
        {
            return Err(LlmFailure::new(
                codes::UNKNOWN,
                "本会话的 agent 组装未启用上下文压缩".to_string(),
            ));
        }
        let (header, turn) = session.with_events(|events| {
            let header = events.iter().rev().find_map(|item| match &item.event {
                SessionEvent::RequestHeader { header, .. } => Some(header.clone()),
                _ => None,
            });
            let turn = events
                .iter()
                .rev()
                .find_map(|item| match item.event {
                    SessionEvent::TurnStart { turn } => Some(turn),
                    _ => None,
                })
                .unwrap_or(0);
            (header, turn)
        });
        let header = header.ok_or_else(|| {
            LlmFailure::new(
                codes::UNKNOWN,
                "session has no request header to restore".to_string(),
            )
        })?;
        let selection = ModelSelection {
            provider: header.config.provider.clone(),
            model: header.config.model.clone(),
            reasoning_effort: header.config.reasoning_effort.clone(),
        };
        let framed_system = header.system.clone().unwrap_or_default();
        // 压缩的是已发生的历史:事件归属于日志中最后一次出现的轮次,step 0
        // 表示轮次外的合成步骤(与自动压缩一样仅作元数据)。
        self.compact_context(
            session,
            &selection,
            &framed_system,
            &header.tools,
            turn,
            0,
            &cancel,
        )
        .await
    }

    /// Runs one user turn to completion and returns the reason it ended.
    ///
    /// `session` is shared (`Arc`) because tools like `todo_write` append
    /// log-only events through a `'static` sink wired back to the same log.
    /// `images` are inline pasted images (vision models); `files` are paths of
    /// uploaded files; `quoted` are trajectory references (title, text),
    /// injected as a context message before the prompt — same channel as the
    /// file notice.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_turn(
        &self,
        session: &Arc<Session>,
        selection: &ModelSelection,
        prompt: &str,
        images: Vec<denia_core::message::ImageData>,
        files: Vec<String>,
        quoted: Vec<(String, String)>,
        // 当前模型是否标记为可识图(read_file 图片注入与图片发送的依据)。
        vision_supported: bool,
        cancel: CancellationToken,
        emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
    ) -> TurnEndReason {
        // 死循环检测记忆跨 turn 保留:轮次开始借出,结束(含错误路径)归还。
        let loop_guard = self
            .loop_guards
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session.id())
            .unwrap_or_default();
        // 文件读取状态同样按会话持有:跨 turn 保留(同一会话的下一轮里,
        // "上一轮读过的文件"依然算读过,不该要求重读)。
        let read_state = self.read_state_for(session.id());
        let mut state = TurnState::new(
            session.clone(),
            emit,
            selection.clone(),
            vision_supported,
            cancel,
            loop_guard,
            read_state,
        );
        let result = turn::run_turn_inner(self, &mut state, prompt, images, files, quoted).await;
        let guard = std::mem::take(&mut state.loop_guard);
        self.loop_guards
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(session.id().to_string(), guard);
        match result {
            Ok(reason) => reason,
            // Append or dispatch failure: the turn stays open in the log and
            // session load closes it with a synthetic aborted turn-end.
            Err(failure) => TurnEndReason::Error { failure },
        }
    }

    /// 取(或首次创建)某会话的文件读取状态表。
    ///
    /// 按会话持有:同一会话的所有工具调用共享一张表,不同会话互不干扰。
    /// 表本身是 `Arc<Mutex<..>>`,借出的是克隆的句柄。
    pub fn read_state_for(
        &self,
        session_id: &str,
    ) -> denia_tools::read_state::SharedReadState {
        let mut map = self
            .read_states
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        map.entry(session_id.to_string())
            .or_insert_with(denia_tools::read_state::shared)
            .clone()
    }

    /// 清空某会话的文件读取状态(压缩后调用)。
    ///
    /// 压缩摘要已接管旧内容的记忆职责;清空后模型若需要文件内容会重新读取,
    /// 而不是依赖"读过"的标记去写一个自己已经看不到内容的文件。
    ///
    /// 整条移除而不是留空表:表按会话 id 累积,长期运行下"开过的每个会话"
    /// 都会留下一个条目;清空时顺手删掉,让不再用的表能被回收。
    pub fn clear_read_state(&self, session_id: &str) {
        let mut map = self
            .read_states
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(state) = map.remove(session_id)
            && let Ok(mut state) = state.lock()
        {
            state.clear();
        }
    }

    /// 会话是否已放行某类审批操作(自动编辑档"本窗口放行"的查询口)。
    pub fn ask_granted(&self, session_id: &str, class: denia_tools::permission::ActionClass) -> bool {
        self.ask_grants
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(session_id)
            .is_some_and(|set| set.contains(&class))
    }

    /// 记录一次"本窗口放行":该会话内同类审批操作不再询问。
    pub fn grant_ask_class(&self, session_id: &str, class: denia_tools::permission::ActionClass) {
        self.ask_grants
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(session_id.to_string())
            .or_default()
            .insert(class);
    }

    /// 清空某会话的审批放行表(会话删除时调用,防止句柄泄漏式累积)。
    pub fn clear_ask_grants(&self, session_id: &str) {
        self.ask_grants
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
    }

    /// 某会话是否已装载某 MCP 工具的完整参数定义。
    pub fn mcp_loaded(&self, session_id: &str, qualified: &str) -> bool {
        self.mcp_loads
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(session_id)
            .is_some_and(|set| set.contains(qualified))
    }

    /// 登记一次 MCP 工具装载:之后该工具的完整 schema 随请求下发,
    /// 调用不再被拦截。重复登记无害(集合语义)。
    pub fn load_mcp_tool(&self, session_id: &str, qualified: &str) {
        self.mcp_loads
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .entry(session_id.to_string())
            .or_default()
            .insert(qualified.to_string());
    }

    /// 清空某会话的 MCP 装载表(会话淘汰时调用,防句柄累积)。
    /// 清空后回到轻量态:工具只剩名称与描述,再次调用会重新装载。
    pub fn clear_mcp_loads(&self, session_id: &str) {
        self.mcp_loads
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
    }
}

/// Append one event to the log, then mirror it to the live subscribers.
/// The log is the ordering authority; emit failures are ignored (subscribers
/// re-sync from the log snapshot on reconnect).
pub(crate) fn append(
    session: &Session,
    emit: &Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
    event: SessionEvent,
) -> Result<SessionEnvelope, LlmFailure> {
    let envelope = session.append(event).map_err(|error| {
        LlmFailure::new(
            codes::STORAGE,
            format!("会话日志写入失败:{error}。磁盘可能已满或目录不可写。"),
        )
    })?;
    emit(&envelope);
    Ok(envelope)
}

/// Should the user-audience system-prompt copy be logged for this step?
/// O(1): the session maintains the last logged copy at append time.
fn should_log_system_prompt(session: &Session, step: u32, text: &str) -> bool {
    if step == 1 {
        return true;
    }
    session.last_system_prompt().as_deref() != Some(text)
}

/// 请求前的日志刷盘检查点(对齐 dsh checkpoint-policy):请求前缀落盘
/// 成功才派发;失败 fail-closed(不发出请求)。
pub(crate) fn flush_before_dispatch(session: &Session) -> Result<(), LlmFailure> {
    session.flush().map_err(|error| {
        LlmFailure::new(
            codes::STORAGE,
            format!("会话日志刷盘失败:{error}。请求未派发,避免崩溃后请求上下文不完整。"),
        )
    })
}

/// Build a [`RequestHeaderSnapshot`] for the upcoming request.
pub(crate) fn build_header_snapshot(
    selection: &ModelSelection,
    framed_system: &str,
    tools: &[denia_core::tool::ToolSchema],
) -> RequestHeaderSnapshot {
    RequestHeaderSnapshot {
        config: LlmCallConfig {
            provider: selection.provider.clone(),
            model: selection.model.clone(),
            reasoning_effort: selection.reasoning_effort.clone(),
            temperature: None,
            max_tokens: None,
            stop: Vec::new(),
        },
        system: Some(framed_system.to_string()),
        tools: tools.to_vec(),
    }
}

/// One user turn's shared, mutable state (event wiring + loop counters).
pub(crate) struct TurnState {
    pub session: Arc<Session>,
    pub emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
    pub selection: ModelSelection,
    pub vision_supported: bool,
    pub cancel: CancellationToken,
    pub file_history: Option<Arc<dyn FileHistoryBackend>>,
    /// 本轮 turn 号(从 1 开始)。
    pub turn: u32,
    /// 自纠反馈注入计数(每轮上限 [`errors::MAX_FEEDBACK`])。
    pub feedback: u32,
    /// 死循环检测器(driver 按会话持有,跨 turn 记忆)。
    pub loop_guard: loop_guard::LoopGuard,
    /// 文件读取状态表(按会话共享):重复读取去重 + 写前新鲜度校验。
    pub read_state: denia_tools::read_state::SharedReadState,
    /// dsh 对齐:请求头按需落盘的基准(最近快照即重建)。
    pub last_header: Option<RequestHeaderSnapshot>,
    /// 路由元数据(仅在变化时写 request-context)。
    pub last_context: Option<(String, String, Option<u64>)>,
    /// 日志里是否已有任何 request-header(resume/initial 判定)。
    pub has_request_header: bool,
    /// 路由通告的上下文窗口(解析失败为 None,不影响主流程)。
    pub context_window: Option<u64>,
    /// 每步已自动重试的次数(报错摘要用;attempt 环内更新)。
    pub step_retries: u32,
}

impl TurnState {
    #[allow(clippy::too_many_arguments)]
    fn new(
        session: Arc<Session>,
        emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
        selection: ModelSelection,
        vision_supported: bool,
        cancel: CancellationToken,
        loop_guard: loop_guard::LoopGuard,
        read_state: denia_tools::read_state::SharedReadState,
    ) -> Self {
        Self {
            session,
            emit,
            selection,
            vision_supported,
            cancel,
            file_history: None,
            turn: 0,
            feedback: 0,
            loop_guard,
            read_state,
            last_header: None,
            last_context: None,
            has_request_header: false,
            context_window: None,
            step_retries: 0,
        }
    }

    pub(crate) fn cwd(&self) -> PathBuf {
        PathBuf::from(self.session.header().cwd.clone())
    }

    /// 当前会话权限模式(O(1) 投影)。
    pub(crate) fn permission_mode(&self) -> denia_core::session::PermissionMode {
        self.session.permission_mode()
    }
}

#[cfg(test)]
mod tests;
