//! The session driver: turn/step loop over the llm registry and tools.
//!
//! A step is one model request plus the tools it called; a turn keeps
//! stepping while the assistant message carries tool-call blocks and closes
//! on a tool-call-free message. Every event is appended to the session log
//! first and only then mirrored to `emit` — the log is the ordering
//! authority. A turn whose appends fail mid-flight is left open; session
//! load closes it with a synthetic aborted `turn-end`.

mod compact;
mod runtime_context;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use denia_core::config::{LlmCallConfig, ModelSelection};
use denia_core::error::{LlmFailure, codes};
use denia_core::message::{ChatMessage, ToolCallRef};
use denia_core::session::{
    AbortCause, ApprovalOutcome, PermissionMode, RequestHeaderReason, RequestHeaderSnapshot,
    SessionEnvelope, SessionEvent, TurnEndReason,
};
use denia_core::stream::{ContentBlock, FinishReason, StreamChunk, TokenUsage};
use denia_llm::{GenerateRequest, LlmRegistry};
use denia_session::Session;
use denia_system_prompt::{
    AssembleContext, SystemPrompt, frame_system_prompt_for_model, render_prompt,
    render_prompt_for_user,
};
use denia_token_meter::{estimate_system_tokens, estimate_tools_tokens};
use denia_tools::permission::{
    is_strictly_wider, parse_permission_mode, validate_escalation_args,
};
use denia_tools::{FileHistoryBackend, ToolContext, ToolRegistry};
use futures::StreamExt;
use runtime_context::RuntimeContextProjection;
use tokio_util::sync::CancellationToken;

pub use compact::{CompactOutcome, CompactionSettings};
use compact::{
    build_summary_messages, format_summary, prune_projection, rough_tokens, select_keep_start,
    should_compact, truncate_head,
};

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

/// 审批通道:driver 在遇到工具升权请求时,向宿主请求一次用户决策。
/// 实现方负责把请求挂到 `LiveSession` 的 pending 表并等待 REST 应答;
/// 取消 token 发生时实现方应返回 `Cancelled`。
#[async_trait]
pub trait ApprovalBridge: Send + Sync {
    async fn request(
        &self,
        session_id: &str,
        request_id: &str,
        cancel: CancellationToken,
    ) -> ApprovalOutcome;
}

/// 请求失败时回注给模型的纠错提示(抄 dsh inject 上下文思路):
/// 不中断,让模型看见拒绝原因自己纠正;每轮最多 MAX_FEEDBACK 次防死循环。
const MAX_FEEDBACK: u32 = 2;

/// 反馈注入的适用范围(模型输出/请求形态问题,模型自纠有意义):
/// 提供方抖动(TRANSPORT/TIMEOUT/SERVER/RATE_LIMIT 等)与配置/凭据问题
/// (AUTH/MISSING_CREDENTIAL 等)不在此列——前者由退避重试处理,后者
/// 直接 error 终止(dsh 语义),都不浪费注入配额。
fn feedback_eligible(code: &str) -> bool {
    matches!(
        code,
        codes::INVALID_REQUEST
            | codes::MALFORMED_RESPONSE
            | codes::UNSUPPORTED_CONTENT
            | codes::UNSUPPORTED_REASONING_EFFORT
    )
}

fn feedback_text(failure: &LlmFailure) -> String {
    format!(
        "[harness] 上一次模型请求被提供方拒绝({code}:{message})。         请检查并纠正上一条输出(尤其是工具参数格式)后继续,不要原样重复。",
        code = failure.code,
        message = failure.message,
    )
}

/// 从正文提取"文本伪工具调用"中声明的函数名列表。
///
/// 部分缺少原生函数调用支持的模型/网关会把工具调用渲染成正文标签
/// (<tool_call><function=name>…),而 wire 响应没有 tool_calls 字段——
/// 这是实测 qwen3.8-flash(cat 网关)的故障形态。仅当文本同时出现
/// `<tool_call` 标签与 `<function=` 赋值时才判定为伪调用,降低对普通
/// 正文(如讲解标签格式)的误报。
fn fake_tool_call_names(text: &str) -> Vec<String> {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("<tool_call") {
        return Vec::new();
    }
    let mut names = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("<function=") {
        let start = from + rel + "<function=".len();
        let mut end = start;
        while end < lower.len()
            && (lower.as_bytes()[end].is_ascii_alphanumeric()
                || matches!(lower.as_bytes()[end], b'_' | b'-' | b'.'))
        {
            end += 1;
        }
        let name = &lower[start..end];
        if !name.is_empty() && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
        match end.checked_add(1) {
            Some(next) if next <= lower.len() => from = next,
            _ => break,
        }
    }
    names
}

/// 文本伪工具调用的自纠提示:与提供方拒绝共用一个反馈通道,让模型
/// 看见"调用没被执行"的原因后改用原生 tool_calls 字段。
fn fake_tool_call_feedback(names: &[String]) -> String {
    let listed = names
        .iter()
        .map(|name| format!("{name}()"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "[harness] 你上一条输出把工具调用写成了正文标签(<tool_call>/<function=…>),这些调用没有被执行:{listed}。\n请改用响应的原生 tool_calls 字段发起工具调用,不要输出 <tool_call> 之类的文本标签;若确实不需要调用工具,直接给出文字回答。"
    )
}

/// 从正文标签里救援出来的一个工具调用。
#[derive(Debug, Clone, PartialEq)]
struct RescuedCall {
    name: String,
    arguments: serde_json::Value,
}

/// 尝试把"文本伪工具调用"(<tool_call><function=…><parameter=…>)解析成
/// 可执行的调用列表。任一块格式解析失败返回 None(整体放弃,退回自纠
/// 注入,避免半吊子执行);整段没有伪调用也返回 None。参数值优先按
/// JSON 解析,失败按字符串处理——bash 的 command 这类自由文本参数
/// 正好落入后者。`.to_ascii_lowercase()` 不改变字节长度,lower 上算出的
/// 索引可直接切原文。
fn rescue_fake_tool_calls(text: &str) -> Option<Vec<RescuedCall>> {
    let lower = text.to_ascii_lowercase();
    let mut calls = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("<tool_call") {
        let block_start = from + rel;
        let tail_start = block_start + "<tool_call".len();
        let Some(rrel) = lower[tail_start..].find("</tool_call>") else {
            // 尾部不完整块(截断/格式烂):整体放弃,交由自纠注入。
            return None;
        };
        let block_end = tail_start + rrel;
        let body = &lower[block_start..block_end];
        // <function=name>
        let Some(frel) = body.find("<function=") else {
            return None;
        };
        let name_start = frel + "<function=".len();
        let mut name_end = name_start;
        while name_end < body.len()
            && (body.as_bytes()[name_end].is_ascii_alphanumeric()
                || matches!(body.as_bytes()[name_end], b'_' | b'-' | b'.'))
        {
            name_end += 1;
        }
        if name_end == name_start {
            return None;
        }
        // <parameter=key>value</parameter> 反复提取。
        let mut args = serde_json::Map::new();
        let mut psearch = 0usize;
        loop {
            let Some(prel) = body[psearch..].find("<parameter=") else {
                break;
            };
            let pkey_start = psearch + prel + "<parameter=".len();
            let mut pkey_end = pkey_start;
            while pkey_end < body.len()
                && (body.as_bytes()[pkey_end].is_ascii_alphanumeric()
                    || matches!(body.as_bytes()[pkey_end], b'_' | b'-' | b'.'))
            {
                pkey_end += 1;
            }
            if pkey_end == pkey_start {
                return None;
            }
            // <parameter=key> 的 key 结束于 '>' 之前;值从 '>' 之后开始。
            let Some(gt_rel) = body[pkey_end..].find('>') else {
                return None;
            };
            let value_start = pkey_end + gt_rel + 1;
            let Some(rrrel) = body[value_start..].find("</parameter>") else {
                return None; // 参数块未闭合。
            };
            let raw = text[block_start + value_start..block_start + value_start + rrrel].trim();
            let value = serde_json::from_str::<serde_json::Value>(raw)
                .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()));
            args.insert(body[pkey_start..pkey_end].to_string(), value);
            psearch = value_start + rrrel + "</parameter>".len();
        }
        calls.push(RescuedCall {
            name: body[name_start..name_end].to_string(),
            arguments: serde_json::Value::Object(args),
        });
        from = block_end + "</tool_call>".len();
    }
    if calls.is_empty() {
        None
    } else {
        Some(calls)
    }
}

/// Drives user turns on one session at a time.
pub struct SessionDriver {
    registry: Arc<LlmRegistry>,
    tools: Arc<ToolRegistry>,
    system_prompt: Arc<ArcSwap<SystemPrompt>>,
    file_history: Option<Arc<dyn FileHistoryProvider>>,
    approval: Option<Arc<dyn ApprovalBridge>>,
    /// 层叠上下文管理:投影剪枝闸门 + LLM 总结压缩(学 dsh / Claude Code)。
    compaction: CompactionSettings,
    /// 连续压缩失败计数(熔断,学 Claude Code `MAX_CONSECUTIVE_AUTOCOMPACT_FAILURES`)。
    compact_failures: AtomicU32,
}

fn should_log_system_prompt(session: &Session, step: u32, text: &str) -> bool {
    if step == 1 {
        return true;
    }
    // O(1):Session 在 append 时维护最近一次系统提示词,不再遍历日志。
    // fold 留 UI 副本(无框架);meter 由 driver 单独喂 framed 版本。
    session.last_system_prompt().as_deref() != Some(text)
}

impl SessionDriver {
    pub fn new(
        registry: Arc<LlmRegistry>,
        tools: Arc<ToolRegistry>,
        system_prompt: Arc<ArcSwap<SystemPrompt>>,
    ) -> Self {
        Self {
            registry,
            tools,
            system_prompt,
            file_history: None,
            approval: None,
            compaction: CompactionSettings::default(),
            compact_failures: AtomicU32::new(0),
        }
    }

    /// 覆盖层叠上下文管理配置(投影闸门 + LLM 压缩;默认值见
    /// [`CompactionSettings::default`])。
    pub fn with_compaction(mut self, settings: CompactionSettings) -> Self {
        self.compaction = settings;
        self
    }

    /// 当前层叠上下文管理配置(server 热更新用)。
    pub fn compaction_settings(&self) -> CompactionSettings {
        self.compaction.clone()
    }

    /// LLM 总结压缩(学 Claude Code compact):把 surface 中旧事件区间
    /// 折叠成一条摘要。摘要请求复用主请求的 system + tools + 消息前缀
    /// (学 `tengu_compact_cache_prefix` / `runForkedAgent`:前缀不变则
    /// provider 缓存命中,压缩成本几乎只是增量)。
    ///
    /// 失败(PTL 等)时按 Claude Code `truncateHeadForPTLRetry` 丢最老约
    /// 20% 消息重试,最多 `max_attempts` 次;仍失败返回 `Err`(调用方熔断)。
    async fn compact_context(
        &self,
        session: &Arc<Session>,
        selection: &ModelSelection,
        framed_system: &str,
        tools: &[denia_core::tool::ToolSchema],
        turn: u32,
        step: u32,
        cancel: &CancellationToken,
    ) -> Result<Option<CompactOutcome>, LlmFailure> {
        let settings = self.compaction.clone();
        let events = session.events();
        // 压缩输入走投影口径:被压缩的历史里,超预算工具结果本来就是
        // head/marker/tail(与闸门下的主请求同视角),摘要更省 token。
        let surface = denia_core::session::derive_surface(&events, Some(&settings.prune));
        let Some(keep_start) = select_keep_start(&surface, &settings) else {
            return Ok(None);
        };
        let (compressed, kept) = surface.split_at(keep_start);
        let pre_tokens = compressed
            .iter()
            .fold(0u64, |acc, item| acc.saturating_add(rough_tokens(&item.message)));
        let post_tokens = kept
            .iter()
            .fold(0u64, |acc, item| acc.saturating_add(rough_tokens(&item.message)));

        let mut attempt = 0u32;
        let mut messages = build_summary_messages(compressed, "");
        let mut summary: Option<String> = None;
        while attempt < settings.max_attempts {
            attempt += 1;
            if cancel.is_cancelled() {
                return Err(LlmFailure::new(
                    codes::ABORTED,
                    "compaction cancelled by user",
                ));
            }
            let request = GenerateRequest {
                model: selection.model.clone(),
                reasoning_effort: selection.reasoning_effort.clone(),
                messages: messages.clone(),
                system: Some(framed_system.to_string()),
                tools: tools.to_vec(),
                temperature: Some(0.0),
                max_tokens: Some(settings.summary_max_tokens),
                stop: Vec::new(),
            };
            let stream = match self
                .registry
                .stream(&selection.provider, &request, None)
                .await
            {
                Ok(stream) => stream,
                Err(error) => {
                    tracing::warn!(
                        session_id = session.id(),
                        error_code = %error.failure.code,
                        attempt,
                        "compaction summary request setup failed"
                    );
                    if attempt >= settings.max_attempts {
                        return Err(error.failure);
                    }
                    messages = truncate_head(&messages, attempt);
                    continue;
                }
            };
            let mut stream = stream;
            let mut text = String::new();
            let mut stream_error: Option<LlmFailure> = None;
            loop {
                let next = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        return Err(LlmFailure::new(
                            codes::ABORTED,
                            "compaction cancelled by user",
                        ));
                    }
                    item = stream.next() => item,
                };
                match next {
                    Some(Ok(StreamChunk::TextDelta { text: delta, .. })) => {
                        text.push_str(&delta);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(failure)) => {
                        stream_error = Some(failure);
                        break;
                    }
                    None => break,
                }
            }
            if let Some(failure) = stream_error {
                tracing::warn!(
                    session_id = session.id(),
                    error_code = %failure.code,
                    error_message = %failure.message,
                    attempt,
                    "compaction summary stream failed"
                );
                if attempt >= settings.max_attempts || cancel.is_cancelled() {
                    return Err(failure);
                }
                messages = truncate_head(&messages, attempt);
                continue;
            }
            let formatted = format_summary(&text);
            if formatted.is_empty() {
                tracing::warn!(
                    session_id = session.id(),
                    attempt,
                    "compaction summary produced no text"
                );
                if attempt >= settings.max_attempts {
                    return Err(LlmFailure::new(
                        codes::MALFORMED_RESPONSE,
                        "compaction summary produced no text".to_string(),
                    ));
                }
                messages = truncate_head(&messages, attempt);
                continue;
            }
            summary = Some(formatted);
            break;
        }

        let Some(summary) = summary else {
            return Err(LlmFailure::new(
                codes::UNKNOWN,
                "compaction exhausted retries without a summary".to_string(),
            ));
        };
        Ok(Some(CompactOutcome {
            summary: summary.clone(),
            replaces_from: compressed[0].seq,
            replaces_to: compressed[compressed.len() - 1].seq,
            keep_from: kept[0].seq,
            pre_tokens,
            // 摘要消息本身的开销按角色框 + 文本估算,并入压缩后占用。
            post_tokens: post_tokens.saturating_add(rough_tokens(&ChatMessage::user(&summary))),
        }))
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
        let system = render_prompt(&assembly);
        let framed = frame_system_prompt_for_model(&system);
        let tools = serde_json::to_string(&assembly.tools).map_err(|error| error.to_string())?;
        Ok((system.len() + framed.len(), tools.len()))
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
        match self
            .run_turn_inner(session, selection, prompt, images, files, quoted, vision_supported, cancel, emit)
            .await
        {
            Ok(reason) => reason,
            // Append or dispatch failure: the turn stays open in the log and
            // session load closes it with a synthetic aborted turn-end.
            Err(failure) => TurnEndReason::Error { failure },
        }
    }

    async fn run_turn_inner(
        &self,
        session: &Arc<Session>,
        selection: &ModelSelection,
        prompt: &str,
        images: Vec<denia_core::message::ImageData>,
        files: Vec<String>,
        quoted: Vec<(String, String)>,
        vision_supported: bool,
        cancel: CancellationToken,
        emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
    ) -> Result<TurnEndReason, LlmFailure> {
        let turn = session.next_turn_number();
        let cwd = PathBuf::from(session.header().cwd.clone());
        let file_history = match &self.file_history {
            Some(provider) => provider.backend(session.id(), &cwd).await,
            None => None,
        };
        if !files.is_empty() {
            let list = files
                .iter()
                .map(|path| format!("- {path}"))
                .collect::<Vec<_>>()
                .join("\n");
            append(
                session,
                &emit,
                SessionEvent::UserMessage {
                    text: format!("[harness] 用户上传了文件:\n{list}\n这些文件已保存,可随时用工具读取。"),
                    injected: true,
                    images: Vec::new(),
                },
            )?;
        }
        if !quoted.is_empty() {
            // 轨迹引用:与文件通知同一注入通道,真实用户消息之前落库,
            // 模型在看到提问前先看到引用内容。
            let body = quoted
                .iter()
                .map(|(title, text)| format!("---\n[{title}]\n{text}\n"))
                .collect::<Vec<_>>()
                .join("\n");
            append(
                session,
                &emit,
                SessionEvent::UserMessage {
                    text: format!(
                        "[harness] 用户从轨迹视图引用了以下记录,请结合这些内容回答:\n\n{body}"
                    ),
                    injected: true,
                    images: Vec::new(),
                },
            )?;
        }
        let user_envelope = append(
            session,
            &emit,
            SessionEvent::UserMessage {
                text: prompt.to_string(),
                injected: false,
                images,
            },
        )?;
        if let Some(provider) = &self.file_history {
            if let Err(error) = provider
                .snapshot(session.id(), &cwd, user_envelope.seq)
                .await
            {
                // 快照失败不阻断本轮对话;但该回退点会缺失文件历史,记录日志便于排查。
                tracing::warn!(
                    session_id = session.id(),
                    seq = user_envelope.seq,
                    error = %error,
                    "file history snapshot failed"
                );
            }
        }
        append(session, &emit, SessionEvent::TurnStart { turn })?;

        let mut step: u32 = 0;
        let mut feedback: u32 = 0;
        let mut runtime_projection = RuntimeContextProjection::restore(session);
        // dsh 对齐:请求头/路由元数据按需落盘(request-header / request-context)。
        // 头相同的连续请求不重复写(最近快照即重建);context 仅在路由或容量变化时写。
        let has_request_header = session
            .events()
            .iter()
            .any(|envelope| matches!(envelope.event, SessionEvent::RequestHeader { .. }));
        let mut last_header: Option<RequestHeaderSnapshot> = None;
        let mut last_context: Option<(String, String, Option<u64>)> = None;
        // 解析一次路由容量;失败不影响主流程(日志记录尽力而为)。
        let context_window = self
            .registry
            .resolve_call(&selection.provider, &selection.model, selection.reasoning_effort.as_deref())
            .await
            .ok()
            .and_then(|resolved| resolved.context_window);
        'step_loop: loop {
            step += 1;
            append(session, &emit, SessionEvent::StepStart { turn, step })?;

            let cwd = session.header().cwd.clone();
            let assembly = match self
                .system_prompt
                .load()
                .assemble(&AssembleContext {
                    cwd: Some(cwd.clone()),
                    model: Some(selection.model.clone()),
                    provider: Some(selection.provider.clone()),
                    permission_mode: Some(session.permission_mode().as_str().to_string()),
                    approval_policy: Some(session.approval_policy().as_str().to_string()),
                }) {
                Ok(assembly) => assembly,
                Err(error) => {
                    // 日志平衡:step 已开,补 step-end + turn-end(对齐 dsh
                    // 的 finally 配对语义;不再让轮次裸开等 load 合成)。
                    let failure = LlmFailure::new(codes::UNKNOWN, error);
                    append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                    let reason = TurnEndReason::Error { failure: failure.clone() };
                    append(session, &emit, SessionEvent::TurnEnd { turn, reason })?;
                    return Ok(TurnEndReason::Error { failure });
                }
            };
            let tools_tokens = serde_json::to_string(&assembly.tools)
                .map(|json| estimate_tools_tokens(&json))
                .unwrap_or(0);
            session.set_tools_tokens(tools_tokens);

            if let Some(snapshot) = runtime_projection.project(&assembly) {
                append(
                    session,
                    &emit,
                    SessionEvent::UserMessage { text: snapshot, injected: true, images: Vec::new() },
                )?;
            }

            // UI 副本只展示 User audience 的 sections(身份 + persona),
            // 工具纪律/工具使用说明等 Model audience 段不入日志副本。
            let prompt_body = render_prompt_for_user(&assembly);
            if should_log_system_prompt(session, step, &prompt_body) {
                append(
                    session,
                    &emit,
                    SessionEvent::SystemPrompt {
                        turn,
                        step,
                        text: prompt_body.clone(),
                    },
                )?;
            }
            // model 实际收到的是完整 prompt(全部 audience) + 框架。
            let model_prompt = render_prompt(&assembly);
            let framed_system = frame_system_prompt_for_model(&model_prompt);
            session.set_system_tokens(estimate_system_tokens(&framed_system));

            // request-header / request-context(对齐 dsh):请求 dispatch 前落盘。
            let snapshot = RequestHeaderSnapshot {
                config: LlmCallConfig {
                    provider: selection.provider.clone(),
                    model: selection.model.clone(),
                    reasoning_effort: selection.reasoning_effort.clone(),
                    temperature: None,
                    max_tokens: None,
                    stop: Vec::new(),
                },
                system: Some(framed_system.clone()),
                tools: assembly.tools.clone(),
            };
            if last_header.as_ref() != Some(&snapshot) {
                let reason = match &last_header {
                    None if !has_request_header => RequestHeaderReason::Initial,
                    None => RequestHeaderReason::Resume,
                    Some(_) => RequestHeaderReason::Change,
                };
                append(
                    session,
                    &emit,
                    SessionEvent::RequestHeader {
                        turn,
                        step,
                        header: snapshot.clone(),
                        reason,
                        starts_series: reason == RequestHeaderReason::Change,
                    },
                )?;
                last_header = Some(snapshot);
            }
            let context = (
                selection.provider.clone(),
                selection.model.clone(),
                context_window,
            );
            if last_context.as_ref() != Some(&context) {
                append(
                    session,
                    &emit,
                    SessionEvent::RequestContext {
                        turn,
                        step,
                        provider: context.0.clone(),
                        model: context.1.clone(),
                        context_window: context.2,
                    },
                )?;
                last_context = Some(context);
            }

            // —— 层叠上下文管理(学 dsh 压力驱动剪枝 + Claude Code compact)——
            // 1. 高压力:LLM 总结压缩,把旧事件区间折叠成摘要(落盘
            //    compaction-summary,日志保持 append-only)。
            // 2. 中压力:投影剪枝,派生历史时对超预算工具结果做
            //    head/marker/tail(不落盘,请求前缀才稳定)。
            // 3. 低压力:什么都不做 —— 请求内容与日志逐字一致,provider
            //    前缀缓存持续命中。
            let pressure = session.context_pressure();
            if should_compact(&pressure, &self.compaction)
                && self.compact_failures.load(Ordering::SeqCst) < self.compaction.max_attempts
            {
                match self
                    .compact_context(&session, &selection, &framed_system, &assembly.tools, turn, step, &cancel)
                    .await
                {
                    Ok(Some(outcome)) => {
                        // 压缩成功:熔断清零,落盘事件由 append 广播给前端。
                        self.compact_failures.store(0, Ordering::SeqCst);
                        append(
                            session,
                            &emit,
                            SessionEvent::CompactionSummary {
                                turn,
                                step,
                                summary: outcome.summary,
                                replaces_from: outcome.replaces_from,
                                replaces_to: outcome.replaces_to,
                                keep_from: outcome.keep_from,
                                pre_tokens: outcome.pre_tokens,
                                post_tokens: outcome.post_tokens,
                            },
                        )?;
                    }
                    Ok(None) => {
                        // 无可压缩区间(历史太短/窗口选择失败):不计数。
                    }
                    Err(error) => {
                        self.compact_failures.fetch_add(1, Ordering::SeqCst);
                        tracing::warn!(
                            session_id = session.id(),
                            error_code = %error.code,
                            error_message = %error.message,
                            failures = self.compact_failures.load(Ordering::SeqCst),
                            "llm compaction failed; skipping and continuing with full history"
                        );
                    }
                }
            }
            // 压缩成功后压力已下降(compaction-summary 折叠进 meter),重新
            // 决策投影闸门;低压力即原文直出。
            let pressure = session.context_pressure();
            let projection = prune_projection(&pressure, &self.compaction);

            let request = GenerateRequest {
                model: selection.model.clone(),
                reasoning_effort: selection.reasoning_effort.clone(),
                messages: session.derive_messages_projected(projection.as_ref()),
                system: Some(framed_system.clone()),
                tools: assembly.tools,
                temperature: None,
                max_tokens: None,
                stop: Vec::new(),
            };
            let retry_sink: Option<denia_llm::RetrySink> = Some(Arc::new({
                let session = session.clone();
                let emit = emit.clone();
                move |attempt: &denia_llm::RetryAttempt| {
                    // 重试轨迹落盘(对齐 dsh llm-retry 事件化):失败不阻断
                    // 主流程,只记录。
                    if let Err(error) = append(
                        &session,
                        &emit,
                        SessionEvent::RetryAttempt {
                            turn,
                            step,
                            attempt: attempt.attempt,
                            code: attempt.code.clone(),
                            message: attempt.message.clone(),
                            delay_ms: attempt.delay_ms,
                        },
                    ) {
                        tracing::warn!(
                            session_id = session.id(),
                            error_code = %error.code,
                            error_message = %error.message,
                            "retry attempt append failed"
                        );
                    }
                }
            }));
            // —— 请求派发:dsh 对齐的 step 内 attempt 循环 ——
            // setup 失败:registry 内部已按 RetryPolicy 退避重试(maxRetries=5),
            // 耗尽后分流——模型输出问题(INVALID_REQUEST/MALFORMED 等)注入
            // 自纠反馈(denia 保留特性);提供方/配置问题直接 error 终止
            // (dsh 语义:setup 失败不进重试环)。
            // finish 错误(可重试码、预算内、未取消)→ 同 step 内退避重试
            // (对应 dsh 的 step while-loop + llm-retry,失败尝试的 chunk 保留
            // 在日志但排除在最终 source_event_seqs 之外);无 chunk 的流错误
            // 也重试(优化:结果未知时重放安全)。
            let retry_policy = denia_llm::RetryPolicy::default();
            let mut step_retries: u32 = 0;
            // 流消费状态:attempt 循环外声明(成功路径读取最后一次尝试的值),
            // 每次 attempt 开头重置;首个 attempt 必先赋值再读取。
            let mut blocks: Vec<ContentBlock>;
            let mut source_event_seqs: Vec<u64>;
            let mut usage: Option<TokenUsage>;
            let mut finish: Option<FinishReason>;
            let mut stream_error: Option<LlmFailure>;
            let mut interrupted: bool;
            'attempts: loop {
                // 请求前检查点(对齐 dsh checkpoint-policy):请求前缀刷盘
                // 成功才派发;失败 fail-closed(不发出请求)。
                session
                    .flush()
                    .map_err(|error| LlmFailure::new(codes::UNKNOWN, format!("log flush before dispatch failed: {error}")))?;
                // 请求建立期同样响应取消:网关/代理黑洞挂起(连接已发出、
                // 响应头迟迟不来,或 setup 重试退避中)时,适配器内部总超时
                // 要 60-120s 才报错,期间点停止必须立刻能断。select 放弃
                // 卡死的建立 future,直接闭合为 aborted。
                let stream_setup = self.registry.stream(&selection.provider, &request, retry_sink.clone());
                tokio::pin!(stream_setup);
                let mut stream = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                        let reason = TurnEndReason::Aborted {
                            cause: Some(AbortCause::User),
                        };
                        append(session, &emit, SessionEvent::TurnEnd { turn, reason: reason.clone() })?;
                        return Ok(reason);
                    }
                    result = &mut stream_setup => match result {
                        Ok(stream) => stream,
                        Err(error) => {
                            let failure = error.failure.clone();
                            append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                            if feedback_eligible(&failure.code) && feedback < MAX_FEEDBACK {
                                feedback += 1;
                                append(
                                    session,
                                    &emit,
                                    SessionEvent::UserMessage { text: feedback_text(&failure), injected: true, images: Vec::new() },
                                )?;
                                continue 'step_loop;
                            }
                            let reason = TurnEndReason::Error { failure };
                            append(session, &emit, SessionEvent::TurnEnd { turn, reason: reason.clone() })?;
                            return Ok(reason);
                        }
                    },
                };

                blocks = Vec::new();
                source_event_seqs = Vec::new();
                usage = None;
                finish = None;
                stream_error = None;
                interrupted = false;
                loop {
                    let next = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            interrupted = true;
                            break;
                        }
                        item = stream.next() => item,
                    };
                    match next {
                        Some(Ok(chunk)) => {
                            let envelope = append(
                                session,
                                &emit,
                                SessionEvent::AssistantChunk {
                                    turn,
                                    step,
                                    chunk: chunk.clone(),
                                },
                            )?;
                            source_event_seqs.push(envelope.seq);
                            match chunk {
                                StreamChunk::BlockEnd { block, .. } => blocks.push(block),
                                StreamChunk::Usage { usage: next_usage } => usage = Some(next_usage),
                                StreamChunk::Finish { reason } => finish = Some(reason),
                                _ => {}
                            }
                        }
                        Some(Err(failure)) => {
                            stream_error = Some(failure);
                            break;
                        }
                        None => break,
                    }
                }

                if interrupted {
                    append(
                        session,
                        &emit,
                        SessionEvent::AssistantMessage {
                            turn,
                            step,
                            blocks: blocks.clone(),
                            usage,
                            interrupted: true,
                            source_event_seqs: source_event_seqs.clone(),
                        },
                    )?;
                    append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                    let reason = TurnEndReason::Aborted {
                        cause: Some(AbortCause::User),
                    };
                    append(session, &emit, SessionEvent::TurnEnd { turn, reason })?;
                    return Ok(TurnEndReason::Aborted {
                        cause: Some(AbortCause::User),
                    });
                }

                // 错误统一分流:流错误 或 finish { Error | Aborted }。
                let failure = stream_error.clone().or_else(|| match &finish {
                    Some(FinishReason::Error { failure }) | Some(FinishReason::Aborted { failure }) => {
                        Some(failure.clone())
                    }
                    _ => None,
                });
                if let Some(failure) = failure {
                    let has_chunks = !source_event_seqs.is_empty();
                    let retryable = retry_policy.is_retryable(&failure.code)
                        && step_retries < retry_policy.max_retries
                        && !cancel.is_cancelled();
                    // 可重试:finish 错误与流错误都重试(dsh:llm-retry 丢弃
                    // 失败尝试的 chunk)。重放 LLM 请求无副作用:半成品 chunk
                    // 留在日志但不进派生历史(derive 只投影最终
                    // assistant-message),工具尚未执行;失败尝试中若模型已
                    // 输出部分内容,重试后由新 attempt 的 blocks 整体取代。
                    // 曾限制"仅无输出时重试",实测被网关断流掐死的收尾步
                    // 白白死亡——有输出重放是安全的,故取消该限制。
                    if retryable {
                        step_retries += 1;
                        let delay = retry_policy
                            .delay_ms(step_retries, failure.provider_retry_after_ms)
                            .unwrap_or(0);
                        append(
                            session,
                            &emit,
                            SessionEvent::RetryAttempt {
                                turn,
                                step,
                                attempt: step_retries,
                                code: failure.code.clone(),
                                message: failure.message.clone(),
                                delay_ms: delay,
                            },
                        )?;
                        let sleep = tokio::time::sleep(std::time::Duration::from_millis(delay));
                        tokio::select! {
                            biased;
                            // 取消压倒重试(dsh:signal.abort 优先于 retry)。
                            _ = cancel.cancelled() => {}
                            _ = sleep => {}
                        }
                        if cancel.is_cancelled() {
                            if has_chunks {
                                append(
                                    session,
                                    &emit,
                                    SessionEvent::AssistantMessage {
                                        turn,
                                        step,
                                        blocks: blocks.clone(),
                                        usage,
                                        interrupted: true,
                                        source_event_seqs: source_event_seqs.clone(),
                                    },
                                )?;
                            }
                            append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                            let reason = TurnEndReason::Aborted {
                                cause: Some(AbortCause::User),
                            };
                            append(session, &emit, SessionEvent::TurnEnd { turn, reason })?;
                            return Ok(TurnEndReason::Aborted {
                                cause: Some(AbortCause::User),
                            });
                        }
                        continue 'attempts;
                    }
                    // 不可重试/预算耗尽:失败尝试不产出终稿消息(dsh:流错误
                    // rethrow 不终稿;已落盘的 chunk 保留在日志),分流终止。
                    append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                    if feedback_eligible(&failure.code) && feedback < MAX_FEEDBACK {
                        feedback += 1;
                        append(
                            session,
                            &emit,
                            SessionEvent::UserMessage { text: feedback_text(&failure), injected: true, images: Vec::new() },
                        )?;
                        continue 'step_loop;
                    }
                    let reason = TurnEndReason::Error { failure };
                    append(session, &emit, SessionEvent::TurnEnd { turn, reason: reason.clone() })?;
                    return Ok(reason);
                }
                break 'attempts;
            }

            append(
                session,
                &emit,
                SessionEvent::AssistantMessage {
                    turn,
                    step,
                    blocks: blocks.clone(),
                    usage,
                    interrupted: false,
                    source_event_seqs,
                },
            )?;

            let mut calls: Vec<ToolCallRef> = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::ToolCall {
                        id,
                        name,
                        arguments,
                    } => Some(ToolCallRef {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    }),
                    _ => None,
                })
                .collect();
            let hit_max_tokens = matches!(finish, Some(FinishReason::MaxTokens));

            if calls.is_empty() || hit_max_tokens {
                // —— 文本伪调用救援 ——
                // 模型把工具调用渲染成了正文标签且不会走 wire tool_calls
                // (实测 qwen3.8-flash/cat 网关的故障形态)。标签可完整解析
                // 时直接代为执行(走下方同一条 dispatch,权限/沙箱/审批
                // 一视同仁);解析失败退回注入自纠;配额耗尽按原样收尾。
                if !hit_max_tokens && calls.is_empty() {
                    let mut combined: Vec<RescuedCall> = Vec::new();
                    let mut parse_ok = true;
                    for block in &blocks {
                        if let ContentBlock::Text { text } = block {
                            match rescue_fake_tool_calls(text) {
                                Some(rescued) => combined.extend(rescued),
                                None => parse_ok = false,
                            }
                        }
                    }
                    if parse_ok && !combined.is_empty() {
                        tracing::info!(
                            names = ?combined.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
                            "rescued text-rendered tool calls"
                        );
                        calls = combined
                            .into_iter()
                            .map(|c| ToolCallRef {
                                id: uuid::Uuid::new_v4().to_string(),
                                name: c.name,
                                arguments: serde_json::to_string(&c.arguments)
                                    .unwrap_or_else(|_| "{}".to_string()),
                            })
                            .collect();
                    } else if feedback < MAX_FEEDBACK {
                        let fake_names: Vec<String> = blocks
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .flat_map(fake_tool_call_names)
                            .collect();
                        if !fake_names.is_empty() {
                            feedback += 1;
                            append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                            append(
                                session,
                                &emit,
                                SessionEvent::UserMessage {
                                    text: fake_tool_call_feedback(&fake_names),
                                    injected: true,
                                    images: Vec::new(),
                                },
                            )?;
                            continue 'step_loop;
                        }
                    }
                }
                if calls.is_empty() || hit_max_tokens {
                    append(session, &emit, SessionEvent::StepEnd { turn, step })?;
                    let reason = if hit_max_tokens {
                        TurnEndReason::MaxTokens
                    } else {
                        TurnEndReason::Completed
                    };
                    append(session, &emit, SessionEvent::TurnEnd { turn, reason: reason.clone() })?;
                    return Ok(reason);
                }
            }

            // Sequential dispatch; every logged call gets exactly one result,
            // including calls caught by a mid-dispatch abort.
            let cwd = PathBuf::from(session.header().cwd.clone());
            for call in &calls {
                append(
                    session,
                    &emit,
                    SessionEvent::ToolCall {
                        turn,
                        step,
                        call_id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    },
                )?;
                let output = if cancel.is_cancelled() {
                    denia_tools::ToolOutput {
                        content: "aborted before dispatch".to_string(),
                        is_error: true,
                    }
                } else if let Some(tool) = self.tools.get(&call.name) {
                    let current_mode = session.permission_mode();
                    let mut permission_override = None;
                    let mut approval_failure = None;
                    let can_escalate = matches!(
                        call.name.as_str(),
                        "bash" | "write_file" | "edit"
                    );
                    if can_escalate {
                        if let Some((requested_raw, justification)) =
                            escalation_fields(&call.arguments)
                        {
                            match resolve_escalation(
                                self,
                                session,
                                &emit,
                                call,
                                current_mode,
                                &requested_raw,
                                &justification,
                                escalation_subject(&call.name),
                                cancel.clone(),
                            )
                            .await
                            {
                                Ok(mode) => permission_override = Some(mode),
                                Err(message) => approval_failure = Some(message),
                            }
                        }
                    }
                    if let Some(message) = approval_failure {
                        denia_tools::ToolOutput {
                            content: message,
                            is_error: true,
                        }
                    } else {
                        // Log-only event sink for tools like todo_write: appends
                        // through the same session and echoes to SSE followers.
                        let sink_session = session.clone();
                        let sink_emit = emit.clone();
                        let context = ToolContext {
                            cwd: cwd.clone(),
                            cancel: cancel.child_token(),
                            // 完整权限关闭路径沙箱;其他档位沿用会话头 sandbox。
                            confined: !current_mode.is_full() && session.header().sandbox,
                            vision_supported,
                            emit_event: Some(Arc::new(move |event: SessionEvent| {
                                if let Ok(envelope) = sink_session.append(event) {
                                    sink_emit(&envelope);
                                }
                            })),
                            file_history: file_history.clone(),
                            permission_mode: current_mode,
                            permission_override,
                        };
                        // 工具执行必须可中断:glob/grep/bash 内部已响应 ctx.cancel,
                        // 但 files/edit/browser/recon 等无取消意识的实现可能在慢盘、
                        // CDP、网络调用上无限挂起。select 取消令牌兜底:放弃卡死的
                        // 执行 future(其内部 await 随 drop 中止,spawn_blocking 句柄
                        // 不再阻塞轮次),保证任何情况下点停止都能立即结束轮次。
                        let execute = tool.execute(&call.arguments, &context);
                        tokio::pin!(execute);
                        tokio::select! {
                            biased;
                            _ = cancel.cancelled() => denia_tools::ToolOutput {
                                content: "工具执行被中断".to_string(),
                                is_error: true,
                            },
                            output = &mut execute => output,
                        }
                    }
                } else {
                    denia_tools::ToolOutput {
                        content: format!("unknown tool: {}", call.name),
                        is_error: true,
                    }
                };
                append(
                    session,
                    &emit,
                    SessionEvent::ToolResult {
                        turn,
                        step,
                        call_id: call.id.clone(),
                        content: output.content,
                        is_error: output.is_error,
                        error: None,
                        error_identity: None,
                        meta: None,
                        replaces: None,
                    },
                )?;
            }
            append(session, &emit, SessionEvent::StepEnd { turn, step })?;
        }
    }
}

/// 从工具原始参数里提取升权请求字段(宽松解析:取不到就视为无升权)。
fn escalation_fields(raw: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    let permissions = value.get("sandbox_permissions")?.as_str()?.to_string();
    let justification = value.get("justification")?.as_str()?.to_string();
    Some((permissions, justification))
}

/// 处理一次工具升权请求:校验 → 落 approval/asked → 等用户决策 →
/// 落 approval/decided → 把结果映射为允许模式或错误文本。
///
/// 仅在模型显式携带 `sandbox_permissions` + `justification` 时调用;
/// 普通被拒调用由工具自身返回 `[sandbox: …]` 标记,模型再据此重试。
#[allow(clippy::too_many_arguments)]
async fn resolve_escalation(
    driver: &SessionDriver,
    session: &Arc<Session>,
    emit: &Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
    call: &ToolCallRef,
    current_mode: PermissionMode,
    requested_raw: &str,
    justification: &str,
    subject: &str,
    cancel: CancellationToken,
) -> Result<PermissionMode, String> {
    if let Err(message) = validate_escalation_args(Some(requested_raw), Some(justification)) {
        return Err(message);
    }
    let requested = parse_permission_mode(requested_raw)
        .ok_or_else(|| format!("unknown sandbox mode \"{requested_raw}\""))?;
    if !is_strictly_wider(current_mode, requested) {
        return Err(format!(
            "sandbox escalation to \"{}\" is not strictly wider than this call's current \"{}\" mode",
            requested.as_str(),
            current_mode.as_str(),
        ));
    }
    let Some(approval) = &driver.approval else {
        return Err(format!(
            "sandbox escalation to \"{}\" requires approval, but no approval channel is available",
            requested.as_str()
        ));
    };
    let request_id = uuid::Uuid::new_v4().to_string();
    let reason = format!(
        "escalate sandbox to {}: {}",
        requested.as_str(),
        justification.trim()
    );
    append(
        session,
        emit,
        SessionEvent::ApprovalAsked {
            request_id: request_id.clone(),
            call_id: call.id.clone(),
            tool: call.name.clone(),
            args_preview: call.arguments.clone(),
            reason: Some(reason.clone()),
        },
    )
    .map_err(|error| format!("[{}] {}", error.code, error.message))?;
    let outcome = approval.request(session.id(), &request_id, cancel).await;
    append(
        session,
        emit,
        SessionEvent::ApprovalDecided {
            request_id: request_id.clone(),
            outcome,
        },
    )
    .map_err(|error| format!("[{}] {}", error.code, error.message))?;
    match outcome {
        ApprovalOutcome::AllowedOnce => Ok(requested),
        ApprovalOutcome::Rejected => Err(format!(
            "the user rejected escalating this {subject} to \"{}\"",
            requested.as_str()
        )),
        ApprovalOutcome::Cancelled => Err(format!(
            "approval for escalating to \"{}\" was cancelled",
            requested.as_str()
        )),
        ApprovalOutcome::Unavailable => Err(format!(
            "sandbox escalation to \"{}\" requires approval, but no approval channel is available",
            requested.as_str()
        )),
    }
}

/// 工具升权审批的模型侧主题(与 dsh escalation hint 一致)。
fn escalation_subject(tool_name: &str) -> &'static str {
    match tool_name {
        "bash" => "command",
        "write_file" | "edit" => "operation",
        _ => "operation",
    }
}

fn append(
    session: &Session,
    emit: &Arc<dyn Fn(&SessionEnvelope) + Send + Sync>,
    event: SessionEvent,
) -> Result<SessionEnvelope, LlmFailure> {
    let envelope = session
        .append(event)
        .map_err(|error| LlmFailure::new(codes::UNKNOWN, error.to_string()))?;
    emit(&envelope);
    Ok(envelope)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_frames_runtime_authority() {
        let (prompt, _) = denia_tools::default_shipped();
        let assembly = prompt
            .assemble(&denia_system_prompt::AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                model: Some("mock".to_string()),
                provider: Some("mock".to_string()),
                ..Default::default()
            })
            .unwrap();
        let body = denia_system_prompt::render_prompt(&assembly);
        assert!(!body.contains("最高优先级"));
        assert!(body.contains("/tmp/ws"));

        let model = denia_system_prompt::frame_system_prompt_for_model(&body);
        assert!(model.contains("最高优先级"));
        assert!(model.contains("再次确认"));
        assert!(model.contains("/tmp/ws"));
    }

    use async_trait::async_trait;
    use denia_core::stream::{BlockType, FinishReason};
    use denia_core::tool::ToolSchema;
    use denia_core::error::LlmError;
    use denia_llm::{ChunkStream, LlmAdapter, LlmModelInfo, LlmResolvedModelInfo, ProviderInfo};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Scripted adapter: each `stream` call pops the next queued chunk run.
    enum MockScript {
        Chunks(Vec<StreamChunk>),
        Fail(LlmFailure),
        /// 先产出 chunk,再以流错误终止(模拟 [DONE] 前断流)。
        ChunksThenFail(Vec<StreamChunk>, LlmFailure),
    }

    struct MockAdapter {
        scripts: Mutex<VecDeque<MockScript>>,
    }

    #[async_trait]
    impl LlmAdapter for MockAdapter {
        fn provider_info(&self, provider: &str) -> ProviderInfo {
            ProviderInfo {
                id: provider.to_string(),
                name: "mock".to_string(),
            }
        }

        async fn list_models(&self, provider: &str) -> Result<Vec<LlmModelInfo>, LlmError> {
            Ok(vec![LlmModelInfo {
                provider: provider.to_string(),
                id: "mock-1".to_string(),
                name: "Mock One".to_string(),
                description: None,
                input_modalities: vec!["text".to_string()],
            }])
        }

        async fn resolve_model(
            &self,
            provider: &str,
            model: &str,
        ) -> Result<LlmResolvedModelInfo, LlmError> {
            Ok(LlmResolvedModelInfo {
                info: LlmModelInfo {
                    provider: provider.to_string(),
                    id: model.to_string(),
                    name: model.to_string(),
                    description: None,
                    input_modalities: vec!["text".to_string()],
                },
                context_window: Some(100_000),
                default_max_tokens: Some(4_000),
                reasoning: None,
            })
        }

        async fn stream(
            &self,
            _provider: &str,
            _request: &GenerateRequest,
        ) -> Result<ChunkStream, LlmError> {
            match self.scripts.lock().unwrap().pop_front() {
                Some(MockScript::Fail(failure)) => {
                    Err(denia_core::error::LlmError::from_failure(failure))
                }
                Some(MockScript::Chunks(chunks)) => Ok(Box::pin(
                    futures::stream::iter(chunks.into_iter().map(Ok)),
                )),
                Some(MockScript::ChunksThenFail(chunks, failure)) => Ok(Box::pin(
                    futures::stream::iter(chunks.into_iter().map(Ok)).chain(
                        futures::stream::once(async move { Err::<StreamChunk, LlmFailure>(failure) }),
                    ),
                )),
                None => Ok(Box::pin(futures::stream::empty())),
            }
        }
    }

    struct EchoTool;

    #[async_trait]
    impl denia_tools::Tool for EchoTool {
        fn schema(&self) -> &ToolSchema {
            // Leaked once for the &'static contract of the test trait object.
            Box::leak(Box::new(ToolSchema {
                name: "echo".to_string(),
                description: "echoes".to_string(),
                parameters: serde_json::json!({ "type": "object" }),
            }))
        }

        async fn execute(&self, arguments: &str, _ctx: &ToolContext) -> denia_tools::ToolOutput {
            denia_tools::ToolOutput {
                content: format!("echo:{arguments}"),
                is_error: false,
            }
        }
    }

    /// 一条 finish-error 流:模拟网关空响应(EMPTY_RESPONSE 以 finish 错误产出)。
fn error_finish_script() -> Vec<StreamChunk> {
    vec![StreamChunk::Finish {
        reason: FinishReason::Error {
            failure: LlmFailure::new(denia_core::error::codes::EMPTY_RESPONSE, "empty response"),
        },
    }]
}

fn text_script(text: &str) -> Vec<StreamChunk> {
        vec![
            StreamChunk::BlockStart {
                index: 0,
                block_type: BlockType::Text,
            },
            StreamChunk::TextDelta {
                index: 0,
                text: text.to_string(),
            },
            StreamChunk::BlockEnd {
                index: 0,
                block: ContentBlock::Text {
                    text: text.to_string(),
                },
            },
            StreamChunk::Usage {
                usage: TokenUsage {
                    input_tokens: 5,
                    output_tokens: 2,
                    cache_read_tokens: None,
                    reasoning_tokens: None,
                },
            },
            StreamChunk::Finish {
                reason: FinishReason::Stop,
            },
        ]
    }

    fn tool_script() -> Vec<StreamChunk> {
        vec![
            StreamChunk::BlockStart {
                index: 0,
                block_type: BlockType::ToolCall,
            },
            StreamChunk::ToolCallDelta {
                index: 0,
                id: "call_1".to_string(),
                name: Some("echo".to_string()),
                arguments_delta: "{\"text\":\"hi\"}".to_string(),
            },
            StreamChunk::BlockEnd {
                index: 0,
                block: ContentBlock::ToolCall {
                    id: "call_1".to_string(),
                    name: "echo".to_string(),
                    arguments: "{\"text\":\"hi\"}".to_string(),
                },
            },
            StreamChunk::Finish {
                reason: FinishReason::ToolCalls,
            },
        ]
    }

    fn driver(scripts: Vec<MockScript>) -> (SessionDriver, Arc<LlmRegistry>) {
        let registry = Arc::new(LlmRegistry::new());
        registry
            .register(
                &["mock".to_string()],
                Arc::new(MockAdapter {
                    scripts: Mutex::new(VecDeque::from(scripts)),
                }),
                denia_llm::RetryPolicy::default(),
            )
            .unwrap();
        let mut tools = ToolRegistry::default();
        tools.register(Arc::new(EchoTool));
        let mut prompt = SystemPrompt::new(denia_system_prompt::SystemPromptConfig {
            include_runtime_context: false,
            ..Default::default()
        });
        let schemas = tools.schemas();
        prompt.tools(move |_| denia_system_prompt::ToolProviderResult {
            schemas: schemas.clone(),
            known_names: None,
        });
        prompt.variable("cwd", |context| context.cwd.clone()).unwrap();
        prompt.variable("model", |context| context.model.clone()).unwrap();
        prompt.variable("provider", |context| context.provider.clone()).unwrap();
        (
            SessionDriver::new(
                registry.clone(),
                Arc::new(tools),
                Arc::new(ArcSwap::from_pointee(prompt)),
            ),
            registry,
        )
    }

    fn temp_session() -> Arc<Session> {
        let dir = std::env::temp_dir().join(format!(
            "denia-loop-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Arc::new(Session::create(&dir, uuid::Uuid::new_v4().to_string(), &dir, true, None).unwrap())
    }

    fn selection() -> ModelSelection {
        ModelSelection {
            provider: "mock".to_string(),
            model: "mock-1".to_string(),
            reasoning_effort: None,
        }
    }

    fn noop_emit() -> Arc<dyn Fn(&SessionEnvelope) + Send + Sync> {
        Arc::new(|_| {})
    }

    #[tokio::test]
    async fn plain_text_turn_completes() {
        let (driver, _registry) = driver(vec![MockScript::Chunks(text_script("done!"))]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "hello", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);

        let kinds: Vec<&str> = session
            .events()
            .iter()
            .map(|envelope| match &envelope.event {
                SessionEvent::UserMessage { .. } => "user",
                SessionEvent::TurnStart { .. } => "turn-start",
                SessionEvent::StepStart { .. } => "step-start",
                SessionEvent::SystemPrompt { .. } => "system-prompt",
                SessionEvent::AssistantMessage { .. } => "assistant",
                SessionEvent::StepEnd { .. } => "step-end",
                SessionEvent::TurnEnd { .. } => "turn-end",
                _ => "chunk",
            })
            .filter(|kind| *kind != "chunk")
            .collect();
        assert_eq!(
            kinds,
            vec![
                "user",
                "turn-start",
                "step-start",
                "system-prompt",
                "assistant",
                "step-end",
                "turn-end"
            ]
        );
        let messages = session.derive_messages();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content, "done!");
    }

    #[tokio::test]
    async fn tool_call_continues_to_second_step() {
        let (driver, _registry) = driver(vec![MockScript::Chunks(tool_script()), MockScript::Chunks(text_script("after tool"))]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "use the tool", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);

        let flat: Vec<String> = session
            .events()
            .iter()
            .map(|envelope| match &envelope.event {
                SessionEvent::ToolCall { .. } => "call".to_string(),
                SessionEvent::ToolResult { is_error, .. } => format!("result:{is_error}"),
                SessionEvent::StepStart { step, .. } => format!("step:{step}"),
                _ => "-".to_string(),
            })
            .collect();
        let call_pos = flat.iter().position(|k| k == "call").unwrap();
        let result_pos = flat.iter().position(|k| k == "result:false").unwrap();
        assert!(call_pos < result_pos, "tool/call must precede tool/result");
        assert!(flat.iter().any(|k| k == "step:2"), "expected a second step");

        // The second request saw the tool result in derived history.
        let messages = session.derive_messages();
        assert!(messages.iter().any(|m| m.role == denia_core::message::ChatRole::Tool));
    }

    #[tokio::test]
    async fn unknown_tool_yields_error_result_and_continues() {
        let mut unknown = tool_script();
        if let StreamChunk::BlockEnd { block, .. } = &mut unknown[2] {
            *block = ContentBlock::ToolCall {
                id: "call_x".to_string(),
                name: "nope".to_string(),
                arguments: "{}".to_string(),
            };
        }
        let (driver, _registry) = driver(vec![MockScript::Chunks(unknown), MockScript::Chunks(text_script("ok"))]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        let result = session.events().iter().find_map(|envelope| match &envelope.event {
            SessionEvent::ToolResult { content, is_error, .. } => {
                Some((content.clone(), *is_error))
            }
            _ => None,
        });
        let (content, is_error) = result.unwrap();
        assert!(is_error);
        assert!(content.contains("unknown tool: nope"));
    }

    #[tokio::test]
    async fn pending_stream_cancel_aborts() {
        struct PendingAdapter;
        #[async_trait]
        impl LlmAdapter for PendingAdapter {
            fn provider_info(&self, provider: &str) -> ProviderInfo {
                ProviderInfo {
                    id: provider.to_string(),
                    name: "pending".to_string(),
                }
            }
            async fn list_models(&self, _p: &str) -> Result<Vec<LlmModelInfo>, LlmError> {
                Ok(Vec::new())
            }
            async fn resolve_model(&self, p: &str, m: &str) -> Result<LlmResolvedModelInfo, LlmError> {
                Ok(LlmResolvedModelInfo {
                    info: LlmModelInfo {
                        provider: p.to_string(),
                        id: m.to_string(),
                        name: m.to_string(),
                        description: None,
                        input_modalities: vec![],
                    },
                    context_window: None,
                    default_max_tokens: None,
                    reasoning: None,
                })
            }
            async fn stream(&self, _p: &str, _r: &GenerateRequest) -> Result<ChunkStream, LlmError> {
                Ok(Box::pin(futures::stream::pending()))
            }
        }
        let registry = Arc::new(LlmRegistry::new());
        registry
            .register(&["mock".to_string()], Arc::new(PendingAdapter), denia_llm::RetryPolicy::default())
            .unwrap();
        let mut prompt = SystemPrompt::new(denia_system_prompt::SystemPromptConfig {
            include_runtime_context: false,
            ..Default::default()
        });
        prompt.variable("cwd", |context| context.cwd.clone()).unwrap();
        prompt.variable("model", |context| context.model.clone()).unwrap();
        prompt.variable("provider", |context| context.provider.clone()).unwrap();
        let driver = SessionDriver::new(
            registry,
            Arc::new(ToolRegistry::default()),
            Arc::new(ArcSwap::from_pointee(prompt)),
        );
        let session = temp_session();
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), Vec::new(), true, cancel, noop_emit())
            .await;
        assert_eq!(
            reason,
            TurnEndReason::Aborted {
                cause: Some(AbortCause::User)
            }
        );
        let has_interrupted = session.events().iter().any(|envelope| {
            matches!(
                &envelope.event,
                SessionEvent::AssistantMessage { interrupted: true, .. }
            )
        });
        assert!(has_interrupted);
    }

    #[test]
    fn rescue_parses_tag_blocks_into_calls() {
        // 完整模板:函数名 + 多参数;自由文本参数按字符串,JSON 参数原样。
        let text = "\n<tool_call>\n<function=edit>\n<parameter=path>\nsrc/lib.rs\n</parameter>\n<parameter=old_string>\n{\"a\": 1}\n</parameter>\n</function>\n</tool_call>\n<tool_call>\n<function=bash>\n<parameter=command>\nGet-ChildItem\n</parameter>\n</function>\n</tool_call>";
        let calls = rescue_fake_tool_calls(text).expect("blocks must parse");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "edit");
        assert_eq!(calls[0].arguments["path"], "src/lib.rs");
        // 值本身是合法 JSON(对象)时原样保留,不是一律字符串。
        assert_eq!(calls[0].arguments["old_string"], serde_json::json!({"a": 1}));
        assert_eq!(calls[1].name, "bash");
        assert_eq!(calls[1].arguments["command"], "Get-ChildItem");
        // 大小写不敏感,函数名归一为小写。
        let upper = rescue_fake_tool_calls("<TOOL_CALL><FUNCTION=Echo><PARAMETER=text>x</PARAMETER></FUNCTION></TOOL_CALL>")
            .expect("upper-case tags must parse");
        assert_eq!(upper[0].name, "echo");
        // 纯正文 → None。
        assert!(rescue_fake_tool_calls("普通文本,没有工具标签").is_none());
        // 未闭合的 parameter 块 → 整体放弃。
        assert!(rescue_fake_tool_calls("<tool_call><function=bash><parameter=command>x</tool_call>").is_none());
        // 无参数调用也能救(空对象)。
        let bare = rescue_fake_tool_calls("<tool_call><function=baseline></tool_call>").expect("bare call must parse");
        assert_eq!(bare[0].arguments, serde_json::json!({}));
    }

    #[tokio::test]
    async fn text_rendered_call_is_rescued_and_executed() {
        // 模型输出文本标签的 bash 调用:harness 代为解析执行,工具结果
        // 回给模型,下一步模型基于真实结果继续——qwen3.8-flash 这类无
        // 原生 FC 能力的模型也能真正干活。
        let fake = "我先看一下\n\n<tool_call>\n<function=echo>\n<parameter=text>\nhello\n</parameter>\n</function>\n</tool_call>";
        let (driver, _registry) = driver(vec![
            MockScript::Chunks(text_script(fake)),
            MockScript::Chunks(text_script("done")),
        ]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "分析项目", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        // 工具被执行,且结果进了派生历史(模型下一步看得到)。
        let result = session.events().iter().find_map(|e| match &e.event {
            SessionEvent::ToolResult { content, is_error, .. } => Some((content.clone(), *is_error)),
            _ => None,
        });
        let (content, is_error) = result.expect("rescued call must be dispatched");
        assert!(!is_error);
        assert!(content.contains("hello"));
        let messages = session.derive_messages();
        assert!(messages.iter().any(|m| m.role == denia_core::message::ChatRole::Tool));
        // 自纠注入未发生(救援成功,无需反馈)。
        let injected = session.events().iter().filter_map(|e| match &e.event {
            SessionEvent::UserMessage { text, injected: true, .. } => Some(text.clone()),
            _ => None,
        }).collect::<Vec<_>>();
        assert!(injected.is_empty(), "no feedback needed when rescue succeeded");
    }

    #[tokio::test]
    async fn rescued_unknown_tool_yields_error_result() {
        // 救援不校验工具存在性:未知工具照常走 dispatch,is_error 结果
        // 回给模型自纠(与原生 tool_calls 的行为一致)。
        let fake = "<tool_call>\n<function=nope>\n<parameter=text>\nx\n</parameter>\n</function>\n</tool_call>";
        let (driver, _registry) = driver(vec![
            MockScript::Chunks(text_script(fake)),
            MockScript::Chunks(text_script("ok")),
        ]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        let result = session.events().iter().find_map(|e| match &e.event {
            SessionEvent::ToolResult { content, is_error, .. } => {
                Some((content.clone(), *is_error))
            }
            _ => None,
        });
        let (content, is_error) = result.unwrap();
        assert!(is_error);
        assert!(content.contains("unknown tool: nope"));
    }

    #[test]
    fn fake_tool_call_names_detects_tags() {
        // Qwen 系"文本模拟工具调用"模板:识别并提取函数名。
        let text = "先看目录\n<tool_call>\n<function=bash>\n<parameter=command>\nGet-ChildItem\n</parameter>\n</function>\n</tool_call>\n<tool_call>\n<function=read_file>\n</function>\n</tool_call>";
        assert_eq!(
            fake_tool_call_names(text),
            vec!["bash".to_string(), "read_file".to_string()]
        );
        // 大小写不敏感,函数名归一为原文。
        assert_eq!(
            fake_tool_call_names("<TOOL_CALL><FUNCTION=Echo>"),
            vec!["echo".to_string()]
        );
        // 纯正文没有标签 → 空。
        assert!(fake_tool_call_names("普通文本,没有工具标签").is_empty());
        // 有 <tool_call> 但无 <function= → 空(避免误报)。
        assert!(fake_tool_call_names("提到了 <tool_call> 但没函数").is_empty());
        // 同一函数重复声明只列一次。
        assert_eq!(
            fake_tool_call_names(
                "<tool_call><function=bash></tool_call><tool_call><function=bash></tool_call>"
            ),
            vec!["bash".to_string()]
        );
        // 无名字的裸 <function=> 不 panic。
        assert!(fake_tool_call_names("<tool_call><function=></tool_call>").is_empty());
    }

    #[tokio::test]
    async fn text_fake_tool_call_is_fed_back_and_recovered() {
        // 伪调用格式烂到无法救援(参数块未闭合)→ 注入自纠,重启一步后
        // 模型改用原生 tool_calls,工具真正被执行。
        let fake = "我先看一下\n\n<tool_call>\n<function=echo>\n<parameter=text>\nhi\n</tool_call>";
        let (driver, _registry) = driver(vec![
            MockScript::Chunks(text_script(fake)),
            MockScript::Chunks(tool_script()),
            MockScript::Chunks(text_script("done")),
        ]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "优化模型选择框", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        // 注入的纠错提示恰一条,且点名了伪调用函数。
        let injected = session.events().iter().filter_map(|e| match &e.event {
            SessionEvent::UserMessage { text, injected: true, .. } => Some(text.clone()),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(injected.len(), 1, "one self-correction injection expected");
        assert!(injected[0].contains("原生 tool_calls"));
        assert!(injected[0].contains("echo()"));
        // 模型随后真的发了原生调用,工具被执行。
        let has_call = session.events().iter().any(|e| {
            matches!(&e.event, SessionEvent::ToolCall { name, .. } if name == "echo")
        });
        assert!(has_call, "recovered step must dispatch the native tool call");
    }

    #[tokio::test]
    async fn fake_tool_call_quota_exhausts_then_completes() {
        // 模型连续输出无法救援的伪调用(参数块未闭合):注入 2 次后配额
        // 耗尽,第三次直接以 Completed 收尾(伪调用文本对用户可见),不死循环。
        let fake = "<tool_call><function=bash><parameter=command>ls</tool_call>";
        let (driver, _registry) = driver(vec![
            MockScript::Chunks(text_script(fake)),
            MockScript::Chunks(text_script(fake)),
            MockScript::Chunks(text_script(fake)),
        ]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        let injected = session.events().iter().filter_map(|e| match &e.event {
            SessionEvent::UserMessage { text, injected: true, .. } => Some(text.clone()),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(injected.len(), 2, "feedback quota must cap at MAX_FEEDBACK");
        let calls = session.events().iter().filter(|e| matches!(e.event, SessionEvent::ToolCall { .. })).count();
        assert_eq!(calls, 0, "no native tool call ever arrived");
        // 日志平衡:step 数与 step-end 数一致。
        let step_starts = session.events().iter().filter(|e| matches!(e.event, SessionEvent::StepStart { .. })).count();
        let step_ends = session.events().iter().filter(|e| matches!(e.event, SessionEvent::StepEnd { .. })).count();
        assert_eq!(step_starts, step_ends);
    }

    #[tokio::test]
    async fn request_failure_is_fed_back_for_self_correction() {
        let (driver, _registry) = driver(vec![
            // MALFORMED_RESPONSE:feedback_eligible 且不在可重试集 → 注入自纠。
            MockScript::Fail(LlmFailure::new("MALFORMED_RESPONSE", "bad payload")),
            MockScript::Chunks(text_script("fixed")),
        ]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        // 纠错提示以 injected 用户消息落日志,模型看得见。
        let injected = session.events().iter().filter_map(|e| match &e.event {
            SessionEvent::UserMessage { text, injected, .. } if *injected => Some(text.clone()),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(injected.len(), 1, "feedback-injected message must exist");
        assert!(injected[0].contains("MALFORMED_RESPONSE"));
    }

    #[tokio::test]
    async fn provider_failure_terminates_without_feedback_waste() {
        // AUTH:不可重试、不可自纠 → 直接 error 终止,不注入反馈(不烧配额)。
        let (driver, _registry) = driver(vec![MockScript::Fail(LlmFailure::new(
            "AUTH",
            "bad key",
        ))]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(
            reason,
            TurnEndReason::Error {
                failure: LlmFailure::new("AUTH", "bad key")
            }
        );
        let injected = session.events().iter().filter_map(|e| match &e.event {
            SessionEvent::UserMessage { text, injected, .. } if *injected => Some(text.clone()),
            _ => None,
        }).collect::<Vec<_>>();
        assert!(injected.is_empty(), "AUTH must not waste the feedback quota");
        // 日志平衡:step-end 与 turn-end 都已落盘。
        let step_starts = session.events().iter().filter(|e| matches!(e.event, SessionEvent::StepStart { .. })).count();
        let step_ends = session.events().iter().filter(|e| matches!(e.event, SessionEvent::StepEnd { .. })).count();
        assert_eq!(step_starts, step_ends);
    }

    #[tokio::test]
    async fn retryable_setup_failure_retries_and_records_attempts() {
        // INVALID_REQUEST 在可重试集:registry 内部退避重试,重试轨迹落盘。
        let (driver, _registry) = driver(vec![
            MockScript::Fail(LlmFailure::new("INVALID_REQUEST", "openai_error").with_status(404)),
            MockScript::Fail(LlmFailure::new("INVALID_REQUEST", "openai_error").with_status(404)),
            MockScript::Chunks(text_script("ok")),
        ]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        // 两次失败 → 两次 retry-attempt 落盘(带退避与错误信息)。
        let attempts = session.events().iter().filter_map(|e| match &e.event {
            SessionEvent::RetryAttempt { attempt, code, delay_ms, .. } => {
                Some((*attempt, code.clone(), *delay_ms))
            }
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0].0, 1);
        assert_eq!(attempts[1].0, 2);
        assert!(attempts.iter().all(|(_, code, _)| code == "INVALID_REQUEST"));
        assert!(attempts[0].2 >= 400 && attempts[1].2 >= 800, "backoff must grow");
    }

    #[tokio::test]
    async fn finish_error_is_not_swallowed_as_completed() {
        // finish 带 Error{…}(如 EMPTY_RESPONSE):不再被吞成 Completed;
        // 可重试码空流重试一次后成功(step 内 attempt 循环)。
        let (driver, _registry) = driver(vec![
            MockScript::Chunks(error_finish_script()),
            MockScript::Chunks(text_script("recovered")),
        ]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        // step 内重试轨迹:1 条 retry-attempt(EMPTY_RESPONSE)。
        let attempts = session.events().iter().filter_map(|e| match &e.event {
            SessionEvent::RetryAttempt { code, .. } => Some(code.clone()),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(attempts, vec!["EMPTY_RESPONSE".to_string()]);
    }

    #[tokio::test]
    async fn stream_closed_with_partial_output_retries_and_recovers() {
        // 网关在 [DONE] 前断流(已有部分输出):step 内重试一次后成功。
        // 曾限制"仅无输出时重试",导致收尾步被断流白白掐死——现在有输出
        // 也重放(重放无副作用:半成品 chunk 不进派生历史)。
        let (driver, _registry) = driver(vec![
            MockScript::ChunksThenFail(
                vec![
                    StreamChunk::BlockStart {
                        index: 0,
                        block_type: BlockType::Text,
                    },
                    StreamChunk::TextDelta {
                        index: 0,
                        text: "partial".to_string(),
                    },
                ],
                LlmFailure::new(denia_core::error::codes::STREAM_CLOSED, "stream ended before the [DONE] marker"),
            ),
            MockScript::Chunks(text_script("recovered")),
        ]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);
        // 重试轨迹:1 条 STREAM_CLOSED。
        let attempts = session.events().iter().filter_map(|e| match &e.event {
            SessionEvent::RetryAttempt { code, .. } => Some(code.clone()),
            _ => None,
        }).collect::<Vec<_>>();
        assert_eq!(attempts, vec!["STREAM_CLOSED".to_string()]);
        // 最终 assistant-message 的 source_event_seqs 只引用第二次尝试的 chunk
        // (partial 尝试的 chunk 不在序列内)——重放未污染派生历史。
        let final_msg = session.events().iter().find_map(|e| match &e.event {
            SessionEvent::AssistantMessage { interrupted, source_event_seqs, .. } if !interrupted => {
                Some(source_event_seqs.clone())
            }
            _ => None,
        });
        let seqs = final_msg.expect("final assistant-message must exist");
        assert_eq!(seqs.len(), 5, "source seqs must cover only the recovered attempt");
    }

    #[tokio::test]
    async fn request_header_and_context_are_logged() {
        let (driver, _registry) = driver(vec![MockScript::Chunks(text_script("hi"))]);
        let session = temp_session();
        let reason = driver
            .run_turn(&session, &selection(), "go", Vec::new(), Vec::new(), Vec::new(), true, CancellationToken::new(), noop_emit())
            .await;
        assert_eq!(reason, TurnEndReason::Completed);

        let events = session.events();
        // request-header:initial 快照,带 provider/model/system/tools。
        let (snapshot, header_reason) = events
            .iter()
            .find_map(|envelope| match &envelope.event {
                SessionEvent::RequestHeader { header, reason, .. } => Some((header, reason)),
                _ => None,
            })
            .expect("request-header must be logged on the first request");
        assert_eq!(*header_reason, RequestHeaderReason::Initial);
        assert_eq!(snapshot.config.provider, "mock");
        assert_eq!(snapshot.config.model, "mock-1");
        assert!(snapshot
            .system
            .as_deref()
            .unwrap_or("")
            .contains("你是由 denia 驱动的"));
        assert!(!snapshot.tools.is_empty());
        // 注册路由后只写一次:同一 step 循环的下一请求不会重复落盘(snapshot 相同)。
        let header_count = events
            .iter()
            .filter(|envelope| matches!(envelope.event, SessionEvent::RequestHeader { .. }))
            .count();
        assert_eq!(header_count, 1);

        // request-context:provider/model/context_window(来自 resolve_call)。
        events
            .iter()
            .find_map(|envelope| match &envelope.event {
                SessionEvent::RequestContext { provider, model, context_window, .. } => {
                    Some((provider, model, context_window))
                }
                _ => None,
            })
            .map(|(provider, model, window)| {
                assert_eq!(provider, "mock");
                assert_eq!(model, "mock-1");
                assert_eq!(*window, Some(100_000));
            })
            .expect("request-context must be logged");

        // assistant-message 的 source_event_seqs 引用全部 chunk seq(5 个)。
        let seqs = events
            .iter()
            .find_map(|envelope| match &envelope.event {
                SessionEvent::AssistantMessage { source_event_seqs, .. } => Some(source_event_seqs),
                _ => None,
            })
            .expect("assistant-message must exist");
        assert_eq!(seqs.len(), 5);
        // 引用的 seq 都是 chunk 事件(顺带验证方向正确)。
        let chunk_seqs = events
            .iter()
            .filter(|envelope| matches!(envelope.event, SessionEvent::AssistantChunk { .. }))
            .map(|envelope| envelope.seq)
            .collect::<Vec<_>>();
        assert_eq!(chunk_seqs, *seqs);
    }
}

