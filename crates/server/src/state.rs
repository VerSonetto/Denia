//! Composition root: opens stores, registers namespaces and adapters, and
//! keeps OpenAI-compatible routes in sync with settings.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use denia_agent_loop::{ApprovalBridge, SessionDriver};
use denia_core::session::ApprovalOutcome;
use denia_credentials::{CredentialEvent, CredentialStore};
use denia_llm::{LlmRegistry, OPENAI_SETTINGS_NS, OpenAiCompatAdapter, OpenAiSection, RetryPolicy};
use denia_session::{Session, SessionError, SessionStore};
use denia_settings::{Applies, NamespaceSpec, SettingsEvent, SettingsStore};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

pub use crate::configuration::*;

/// Process-wide shared state handed to every handler.
pub struct AppState {
    pub runtime: Arc<crate::agent_runtime::Runtime>,
    pub home: std::path::PathBuf,
    pub settings: Arc<SettingsStore>,
    pub credentials: Arc<CredentialStore>,
    pub registry: Arc<LlmRegistry>,
    pub events: broadcast::Sender<ServerEvent>,
    /// `events` 的可轮询副本(带序号环缓冲),供 `/api/events/poll` 用。
    ///
    /// 存在的原因是实测发现 SSE 经 cloudflared 隧道**完全不透传帧**,而长轮询
    /// 能穿透 —— 手机经隧道时推送只能走轮询。见 `crate::event_pulse`。
    pub pulse: Arc<crate::event_pulse::EventPulse>,
    pub http: reqwest::Client,
    pub sessions: Arc<SessionStore>,
    pub live: Arc<LiveSessions>,
    pub driver: Arc<SessionDriver>,
    pub workspaces: Arc<crate::workspace::WorkspaceRegistry>,
    pub system_prompt: Arc<crate::system_prompt_store::SystemPromptState>,
    /// agent preset 名册:随附组装 + 用户自定义组装(会话按它组装工具面)。
    pub agent_presets: Arc<crate::agent_presets::PresetStore>,
    /// 子代理定义仓库:文件、覆盖层、revision 与诊断的唯一所有者。
    pub subagent_profiles: Arc<crate::subagents::ProfileStore>,
    pub file_history: Arc<crate::file_history::FileHistoryStore>,
    /// 内嵌浏览器中枢(工具与 REST API 共用)。
    pub browser: Arc<denia_browser::BrowserManager>,
    /// 面板终端中枢:交互式 PTY 会话(右侧「终端」标签用)。
    pub terminals: Arc<denia_terminal::TerminalManager>,
    /// MCP 运行时:外部 MCP 服务器的连接与工具面同步。
    pub mcp: Arc<crate::mcp_runtime::McpRuntime>,
    /// 会话头部「用应用打开」:host 侧应用探测、图标与启动。
    pub open_in_app: Arc<crate::open_in_app::OpenInAppState>,
    /// 远程连接中枢:局域网直连 + Cloudflare 临时隧道的统一入口。
    pub remote: Arc<crate::remote::RemoteManager>,
    /// 绑定地址非回环 ⇒ 远程浏览器 ⇒ 目录选择器走 browse。
    pub bound_remote: bool,
}

impl AppState {
    pub fn session_commands(&self) -> crate::application::sessions::SessionCommands<'_> {
        use crate::application::sessions::{SessionCommands, SessionServices};
        SessionCommands::new(SessionServices {
            home: &self.home,
            sessions: &self.sessions,
            live: &self.live,
            registry: self.registry.clone(),
            driver: self.driver.clone(),
            runtime: self.runtime.clone(),
            events: self.events.clone(),
        })
    }

    /// 广播 MCP 状态变化(前端刷新面板)。
    pub fn announce_mcp_updated(&self) {
        let _ = self.events.send(ServerEvent::McpUpdated);
    }
}

/// Wire push events for the console.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ServerEvent {
    SettingsUpdated,
    CredentialsUpdated,
    LlmUpdated,
    SessionsUpdated,
    /// 会话运行状态变化(发消息/轮次结束/删除);前端据此维护 running 集合,
    /// 无需为后台会话各维持一条 SSE 长连接。
    RunningChanged {
        id: String,
        running: bool,
    },
    /// `/api/events` 连接时的运行中会话快照(只发给该连接,不走广播)。
    /// 刷新/重连后前端用它还原中断按钮,不必等下一次增量。
    RunningSnapshot {
        ids: Vec<String>,
    },
    /// 自定义系统提示词文件变更后广播(前端可选订阅)。
    SystemPromptChanged,
    /// agent preset 名册变化(用户复制/删除,或在编辑器里改了 preset.yml);
    /// 前端据此刷新选择器与设置页。
    AgentPresetsUpdated,
    /// MCP 服务器配置或连接状态发生变化;前端据此刷新 MCP 面板。
    McpUpdated,
    /// 远程连接状态变化(开启/关闭局域网或隧道、会话增减);前端据此刷新
    /// 远程面板与顶部警示条。
    RemoteUpdated,
    /// 手动压缩已结束但**没有**摘要产出:无可压缩区间(`nothing-to-compact`)
    /// 或摘要调用失败(附可读原因)。压缩是后台任务(202),结果只能走广播;
    /// 成功路径不发这个 —— 成功有 append-only 的 compaction-summary 事件。
    CompactionFailed {
        id: String,
        reason: String,
    },
}

/// One materialized session: durable log plus live fan-out. `session` is an
/// internally synchronized log, so readers never wait behind a running turn.
pub struct LiveSession {
    pub session: Arc<Session>,
    pub followers: broadcast::Sender<denia_core::session::SessionEnvelope>,
    pub running: AtomicBool,
    /// 最近一次被访问(挂载/发消息/follow)的 epoch ms;空闲淘汰依据。
    pub last_touch: AtomicU64,
    pub cancel: std::sync::Mutex<Option<CancellationToken>>,
    /// 等待用户决策的审批请求(request_id → oneshot)。计划审批的决策
    /// 可携带执行档位/模型/补充建议(PlanReviewDecision)。
    pub pending_approvals: std::sync::Mutex<
        HashMap<String, tokio::sync::oneshot::Sender<denia_core::session::PlanReviewDecision>>,
    >,
    /// 等待用户作答的提问请求(request_id → oneshot)。
    ///
    /// 与审批分表:dsh 用同一 id 空间承担两种交互,重试/多问题批次时
    /// 应答可能误配;这里按 request_id 严格隔离,且答题是一次性原子结算。
    pub pending_asks: std::sync::Mutex<
        HashMap<String, tokio::sync::oneshot::Sender<denia_core::session::AskResolution>>,
    >,
}

/// 驻留会话数上限。超出后按 LRU 淘汰非运行、无订阅者会话。
const LIVE_MAX_RESIDENT: usize = 8;
/// 驻留会话的**真实内存**字节预算(见 [`Session::resident_bytes`])。
///
/// 早先这里用日志文件大小当代理量,而日志里约九成是轮次闭合即清扫的
/// chunk —— 实测 8 个百 MB 级会话的驻留事件只有 60 MB 上下,按文件大小
/// 记账(512 MB 预算)等于永不触发,常驻内存因此随打开过的会话线性上涨。
///
/// 现在按真实驻留字节约束,取 32 MB:实测"实际内存 ≈ 记账值 × 1.6"
/// (结构体 + 分配器开销),32 MB 对应约 50 MB 会话内存,加上冷启动基线
/// (~20 MB,含嵌入控制台与运行时),总常驻稳定在 100 MB 以内。
///
/// 被淘汰的只是**空闲且无人订阅**的会话:运行中的会话永不淘汰,正在看的
/// 会话有 SSE 订阅者也不淘汰。重新加载一个百 MB 级会话实测 0.5~1.7s,
/// 换来的内存上界远比这点延迟值钱。
const LIVE_MAX_RESIDENT_BYTES: u64 = 32 * 1024 * 1024;

/// Live sessions keyed by id; loads (and repairs) on first touch.
///
/// ## 生命周期(内存上限)
///
/// - 每次 `get_or_load` / `touch` 都会刷新 `last_touch`。
/// - 后台淘汰任务(见 [`spawn_live_evictor`])周期清理:非运行中、
///   无 SSE 订阅者、且超过 `idle_after` 未使用的会话被卸载,事件内存
///   随之释放;下次访问按日志重新加载。运行中的会话永不淘汰。
/// - **驻留上限双约束**:会话数(`LIVE_MAX_RESIDENT`)+ 日志字节预算
///   (`LIVE_MAX_RESIDENT_BYTES`),加载新会话触顶时按 LRU 淘汰非运行、
///   无订阅者的会话,防止连续打开长会话把常驻内存撑到 O(全部历史)。
pub struct LiveSessions {
    inner: std::sync::Mutex<HashMap<String, Arc<LiveSession>>>,
    /// 最多同时驻留的完整会话数。超出后按 LRU 淘汰非运行、无订阅者会话,
    /// 防止连续打开长会话把后端常驻内存撑到 O(全部历史)。
    max_resident: usize,
    /// 驻留会话的日志字节总预算(代理量)。
    max_resident_bytes: u64,
}

impl Default for LiveSessions {
    fn default() -> Self {
        Self::new(LIVE_MAX_RESIDENT, LIVE_MAX_RESIDENT_BYTES)
    }
}

impl LiveSessions {
    pub fn new(max_resident: usize, max_resident_bytes: u64) -> Self {
        Self {
            inner: std::sync::Mutex::new(HashMap::new()),
            max_resident: max_resident.max(1),
            max_resident_bytes,
        }
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl LiveSessions {
    pub fn get_or_load(
        &self,
        store: &SessionStore,
        id: &str,
    ) -> Result<Arc<LiveSession>, SessionError> {
        let mut map = self.inner.lock().unwrap();
        if let Some(live) = map.get(id) {
            live.last_touch
                .store(now_millis(), std::sync::atomic::Ordering::Relaxed);
            return Ok(live.clone());
        }
        // 浏览/打开一律冷加载:聚合投影保留,事件不驻留;写路径由
        // `ensure_hot` 按需升级。驻留内存因此只来自真正参与会话的热会话。
        let session = store.open_cold(id)?;
        let session = Arc::new(session);
        store.track_session(&session);
        let live = Arc::new(LiveSession {
            session,
            followers: broadcast::channel(1024).0,
            running: AtomicBool::new(false),
            last_touch: AtomicU64::new(now_millis()),
            cancel: std::sync::Mutex::new(None),
            pending_approvals: std::sync::Mutex::new(HashMap::new()),
            pending_asks: std::sync::Mutex::new(HashMap::new()),
        });
        map.insert(id.to_string(), live.clone());
        // 保护刚加载的这个会话:调用方(以及上面的 `live.clone()`)正持有它,
        // 计数天然 > 1,不显式排除的话它自己会被误判成"不可淘汰",裁剪白跑。
        self.trim_capacity_locked(&mut map, Some(id));
        Ok(live)
    }

    /// 淘汰判定口径:非运行、无 SSE 订阅者、且除注册表外无其他持有者。
    /// 运行中的会话绝不卸载。
    ///
    /// **调用方若正持有该会话的 `Arc`,必须通过 `protected` 参数排除它**,
    /// 不能指望这个判定自己识别 —— 早先这里硬编码
    /// `Arc::strong_count(live) == 1`,而 `ensure_hot(&live)` 这类入口的
    /// 调用方**必然**持有一个 Arc,计数至少是 2,条件恒假,热升级后的会话
    /// 永远淘汰不掉,预算因此形同虚设(实测连续加载 10 个大会话,内存从
    /// 4 MB 单调涨到 124.7 MB 且不回落)。
    ///
    /// 这里保留 `Arc::strong_count == 1` 的原意:只有注册表持有才算"没人用"。
    /// 调用方持有的那份由 `trim_capacity_locked` 的 `protected` 显式豁免。
    fn evictable(live: &Arc<LiveSession>) -> bool {
        !live.running.load(std::sync::atomic::Ordering::SeqCst)
            && live.followers.receiver_count() == 0
            && Arc::strong_count(live) == 1
    }

    /// 驻留内存预算的代理量:热会话取**真实驻留事件字节**
    /// (见 [`Session::resident_bytes`]),冷会话不驻留事件记 0。
    fn resident_bytes_of(live: &LiveSession) -> u64 {
        if live.session.is_hot() {
            live.session.resident_bytes()
        } else {
            0
        }
    }

    /// 在已持锁的 map 上执行容量裁剪:会话数超过 `max_resident` **或**
    /// 热会话的驻留字节总量超过 `max_resident_bytes` 时,按 last_touch
    /// 从旧到新淘汰"非运行、无订阅者"的会话。运行中的会话绝不卸载。
    ///
    /// `protected` 是调用方当前持有的会话 id:它可能正被使用(如刚升级为
    /// 热态的那一个),淘汰它会让调用方手里的事件表变成"表外"状态。保留它
    /// 只是让这一轮少淘汰一个,下一轮淘汰器会照常处理。
    fn trim_capacity_locked(
        &self,
        map: &mut HashMap<String, Arc<LiveSession>>,
        protected: Option<&str>,
    ) {
        let max = self.max_resident;
        let max_bytes = self.max_resident_bytes;
        let mut resident_bytes: u64 = map.values().map(|live| Self::resident_bytes_of(live)).sum();
        if map.len() <= max && resident_bytes <= max_bytes {
            return;
        }
        let mut candidates: Vec<(String, u64)> = map
            .iter()
            .filter(|(id, live)| Some(id.as_str()) != protected && Self::evictable(live))
            .map(|(id, live)| {
                (
                    id.clone(),
                    live.last_touch.load(std::sync::atomic::Ordering::Relaxed),
                )
            })
            .collect();
        candidates.sort_by_key(|(_, touched)| *touched);
        let mut count = map.len();
        for (id, _) in candidates {
            if count <= max && resident_bytes <= max_bytes {
                break;
            }
            if let Some(live) = map.get(&id) {
                // 双检:可能在收集候选后被其他路径挂上订阅/转运行。
                if Self::evictable(live) {
                    let live = map.remove(&id).expect("checked above");
                    resident_bytes = resident_bytes.saturating_sub(Self::resident_bytes_of(&live));
                    count -= 1;
                }
            }
        }
    }

    /// 读取已加载的 live 会话(不触发磁盘加载)。
    pub fn get(&self, id: &str) -> Option<Arc<LiveSession>> {
        self.inner.lock().unwrap().get(id).cloned()
    }

    /// 某会话刚被 `ensure_hot` 升级为热态后调用:重新核算驻留预算并裁剪。
    ///
    /// [`LiveSessions::get_or_load`] 只在**插入新条目**时裁剪,而热升级是
    /// 原地把已存在的冷条目换成热条目 —— 驻留量从 0 跳到几十 MB 却不经过
    /// 任何裁剪点。生产实例长期运行下常驻内存持续上涨,正是这条缺口:
    /// 每个被打开/发过消息的会话都留在表里,预算从未真正生效。
    fn trim_after_promotion(&self, keep: &str) {
        let mut map = self.inner.lock().unwrap();
        self.trim_capacity_locked(&mut map, Some(keep));
    }

    /// 升级某会话为热态,并在升级后核算驻留预算。
    ///
    /// 所有写路径都应走这个入口而不是直接 `session.ensure_hot()`:热升级
    /// 会让驻留量从 0 跳到几十 MB,必须给淘汰器一个立刻复核的机会。
    ///
    /// 刚升级的这个会话本轮不淘汰(调用方正持有它并使用其事件表),
    /// 超预算时先腾别的空闲会话;它自己留待淘汰器下一轮按 LRU 处理。
    pub fn ensure_hot(&self, live: &Arc<LiveSession>) -> Result<(), SessionError> {
        live.session.ensure_hot()?;
        self.trim_after_promotion(live.session.id());
        Ok(())
    }

    /// 当前正在跑 turn 的会话 id。运行中会话不会被空闲淘汰,只扫内存 live 表。
    pub fn running_ids(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, live)| live.running.load(Ordering::SeqCst))
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// 记录一次外部访问(follow 连接等),防止被空闲淘汰。
    pub fn touch(&self, id: &str) {
        if let Some(live) = self.inner.lock().unwrap().get(id) {
            live.last_touch
                .store(now_millis(), std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// 淘汰满足条件的空闲会话,返回被卸载的 id 列表。
    pub fn evict_idle(&self, idle_after_secs: u64) -> Vec<String> {
        let now = now_millis();
        let idle_ms = idle_after_secs.saturating_mul(1000);
        let mut to_remove = Vec::new();
        {
            let map = self.inner.lock().unwrap();
            for (id, live) in map.iter() {
                if !live.running.load(Ordering::SeqCst)
                    && Arc::strong_count(live) == 1
                    && Arc::strong_count(&live.session) == 1
                    && let Err(error) = live.session.cool()
                {
                    tracing::warn!(session = %id, %error, "could not release idle session history");
                }
                let idle = now
                    .saturating_sub(live.last_touch.load(std::sync::atomic::Ordering::Relaxed))
                    >= idle_ms;
                if idle && Self::evictable(live) {
                    to_remove.push(id.clone());
                }
            }
        }
        let mut map = self.inner.lock().unwrap();
        for id in &to_remove {
            // 双检:可能已被其他路径移除。
            if let Some(live) = map.get(id)
                && Self::evictable(live)
            {
                map.remove(id);
            }
        }
        to_remove
    }

    pub fn remove(&self, id: &str) {
        self.inner.lock().unwrap().remove(id);
    }
}

/// 服务端审批桥:把确认框挂到 LiveSession 的 pending 表,等待 REST 应答。
pub struct ServerApprovalBridge {
    live: Arc<LiveSessions>,
}

impl ServerApprovalBridge {
    pub fn new(live: Arc<LiveSessions>) -> Self {
        Self { live }
    }
}

#[async_trait]
impl ApprovalBridge for ServerApprovalBridge {
    async fn request(
        &self,
        session_id: &str,
        request_id: &str,
        cancel: CancellationToken,
    ) -> denia_core::session::PlanReviewDecision {
        use denia_core::session::PlanReviewDecision;
        let unavailable = || PlanReviewDecision {
            outcome: ApprovalOutcome::Unavailable,
            execute_mode: None,
            selection: None,
            vision_supported: None,
            feedback: None,
        };
        let cancelled = || PlanReviewDecision {
            outcome: ApprovalOutcome::Cancelled,
            execute_mode: None,
            selection: None,
            vision_supported: None,
            feedback: None,
        };
        let Some(live) = self.live.get(session_id) else {
            return unavailable();
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = live
                .pending_approvals
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            pending.insert(request_id.to_string(), tx);
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                let mut pending = live
                    .pending_approvals
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if let Some(tx) = pending.remove(request_id) {
                    let _ = tx.send(cancelled());
                }
                cancelled()
            }
            result = rx => {
                let _ = live
                    .pending_approvals
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(request_id);
                result.unwrap_or_else(|_| cancelled())
            }
        }
    }
}

/// 服务端提问桥:把 `ask` 工具的提问挂到 LiveSession 的 pending 表,
/// 落 `ask-requested` 事件(刷新页面可恢复挂起卡片),等待 REST 应答。
///
/// 与 dsh 的差异(见 `docs/ask-tool.md` 不足分析):
/// - **有超时**:`timeout_ms` 到点按 `TimedOut` 结算,工具不会永久挂起;
/// - **结局区分**:超时/取消/无通道各有独立语义,模型知道下一步怎么办;
/// - **先落盘再等待**:请求进日志后即使前端断线重连也能重建挂起态;
/// - **单次原子结算**:一个 request_id 只结算一次,重复应答返回 404。
pub struct ServerAskBridge {
    live: Arc<LiveSessions>,
}

impl ServerAskBridge {
    pub fn new(live: Arc<LiveSessions>) -> Self {
        Self { live }
    }
}

#[async_trait]
impl denia_tools::AskBridge for ServerAskBridge {
    async fn ask(
        &self,
        session_id: &str,
        request_id: &str,
        call_id: &str,
        questions: &[denia_core::session::AskQuestion],
        timeout_ms: u64,
        cancel: CancellationToken,
    ) -> denia_core::session::AskResolution {
        use denia_core::session::{AskOutcome, AskResolution, SessionEvent};
        let Some(live) = self.live.get(session_id) else {
            return AskResolution {
                outcome: AskOutcome::Unavailable,
                answers: Vec::new(),
                reason: Some("会话不在运行中".to_string()),
            };
        };
        // 子代理的提问走**统一授权检查**：只有派遣时把 `ask` 授予它的定义才能
        // 提问（内置预设默认不含 ask，管理页可为定义勾选）。这与"子代理一律
        // 不能提问"是两件事：授权了就能问；未授权时 schema 层不可见、执行层
        // 硬拒，这里只是最后一道服务端兜底。
        if let Some(child) = live.session.header().subagent.as_ref() {
            let granted = child
                .effective_tools
                .as_deref()
                .is_some_and(|tools| tools.iter().any(|name| name == "ask"));
            if !granted {
                return AskResolution {
                    outcome: AskOutcome::Unavailable,
                    answers: Vec::new(),
                    reason: Some(
                        "本次派遣没有授予 ask 工具，子代理不能向用户提问；请自行决策，并在最终结果中说明未决问题与采用的假设"
                            .to_string(),
                    ),
                };
            }
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut pending = live.pending_asks.lock().unwrap_or_else(|p| p.into_inner());
            pending.insert(request_id.to_string(), tx);
        }
        // 先落盘再等待:前端重连后按日志恢复挂起卡片。
        if let Ok(envelope) = live.session.append(SessionEvent::AskRequested {
            request_id: request_id.to_string(),
            call_id: call_id.to_string(),
            questions: questions.to_vec(),
            timeout_ms,
        }) {
            let _ = live.followers.send(envelope);
        }
        let settle = |resolution: AskResolution| {
            if let Ok(envelope) = live.session.append(SessionEvent::AskResolved {
                request_id: request_id.to_string(),
                resolution: resolution.clone(),
            }) {
                let _ = live.followers.send(envelope);
            }
            resolution
        };
        let outcome = tokio::select! {
            _ = cancel.cancelled() => {
                let mut pending = live.pending_asks.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(tx) = pending.remove(request_id) {
                    // 取消与超时同形:应答方已离场,不再有人写这条通道。
                    let _ = tx.send(AskResolution {
                        outcome: AskOutcome::Cancelled,
                        answers: Vec::new(),
                        reason: None,
                    });
                }
                AskResolution {
                    outcome: AskOutcome::Cancelled,
                    answers: Vec::new(),
                    reason: None,
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(timeout_ms)) => {
                let mut pending = live.pending_asks.lock().unwrap_or_else(|p| p.into_inner());
                pending.remove(request_id);
                AskResolution {
                    outcome: AskOutcome::TimedOut,
                    answers: Vec::new(),
                    reason: None,
                }
            }
            result = rx => {
                let _ = live
                    .pending_asks
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(request_id);
                result.unwrap_or(AskResolution {
                    outcome: AskOutcome::Cancelled,
                    answers: Vec::new(),
                    reason: None,
                })
            }
        };
        settle(outcome)
    }
}

/// RAII 运行保护:drop 时复位 running、清空 cancel token,并广播
/// `RunningChanged { running: false }` 给控制台。即使任务 panic,
/// 会话也不会永久锁死在 running 状态,前端圆点也不会卡死。
pub struct RunningGuard {
    live: Arc<LiveSession>,
    events: broadcast::Sender<ServerEvent>,
}

impl RunningGuard {
    pub fn new(live: Arc<LiveSession>, events: broadcast::Sender<ServerEvent>) -> Self {
        Self { live, events }
    }
}

impl Drop for RunningGuard {
    fn drop(&mut self) {
        // 任务无论正常/panic 结束,未答审批一律按 cancelled 结算,
        // 避免 pending oneshot 泄漏。
        {
            let mut pending = self
                .live
                .pending_approvals
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for tx in pending.drain() {
                let _ = tx.1.send(denia_core::session::PlanReviewDecision {
                    outcome: ApprovalOutcome::Cancelled,
                    execute_mode: None,
                    selection: None,
                    vision_supported: None,
                    feedback: None,
                });
            }
        }
        // 未答提问同样按 cancelled 结算,避免 pending oneshot 泄漏
        // (工具侧会把它渲染成"用户取消",模型据此收束)。
        {
            let mut pending = self
                .live
                .pending_asks
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for (_, tx) in pending.drain() {
                let _ = tx.send(denia_core::session::AskResolution {
                    outcome: denia_core::session::AskOutcome::Cancelled,
                    answers: Vec::new(),
                    reason: None,
                });
            }
        }
        let mut cancel = self.live.cancel.lock().unwrap();
        *cancel = None;
        let _ = self.events.send(ServerEvent::RunningChanged {
            id: self.live.session.id().to_string(),
            running: false,
        });
        self.live.running.store(false, Ordering::SeqCst);
    }
}

/// 后台空闲淘汰:每 `interval` 扫描一次,卸载 idle 会话。运行中永不淘汰。
///
/// `on_evict` 让调用方在会话离开常驻表时一并回收按会话累积的辅助内存态
/// (文件读取状态表、文件历史快照链)。这些表不属于 `LiveSession`,不主动
/// 回收就会只增不减 —— 长期运行下每个用过的会话都留一份。
pub fn spawn_live_evictor(
    live: Arc<LiveSessions>,
    interval_secs: u64,
    idle_after_secs: u64,
    on_evict: Arc<dyn Fn(&str) + Send + Sync>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let evicted = live.evict_idle(idle_after_secs);
            if !evicted.is_empty() {
                for id in &evicted {
                    on_evict(id);
                }
                tracing::debug!(count = evicted.len(), "evicted idle live sessions");
            }
        }
    });
}

/// Validates the `llm-openai` section: typed round-trip plus per-provider
/// custom-header sanity (HTTP header-name grammar and header-value rules),
/// so a misconfigured header fails at save time rather than at request time.
fn validate_openai(value: Value) -> Result<Value, String> {
    let section: OpenAiSection = serde_json::from_value(value).map_err(|e| e.to_string())?;
    for (provider, profile) in &section.providers {
        for (name, raw) in &profile.headers {
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                format!(
                    "provider '{provider}': header name '{name}' is not a valid HTTP header name"
                )
            })?;
            // 占位引用形式(整段 `${REF}`)跳过值的静态检查,由请求前解析兜底;
            // 其余值按 HeaderValue 规则即时拒绝(控制字符等)。
            let trimmed = raw.trim();
            let is_placeholder = trimmed
                .strip_prefix("\u{24}{")
                .and_then(|rest| rest.strip_suffix('}'))
                .is_some();
            if !is_placeholder {
                reqwest::header::HeaderValue::from_str(raw).map_err(|_| {
                    format!("provider '{provider}': header '{name}' has an invalid value")
                })?;
            }
        }
    }
    serde_json::to_value(section).map_err(|e| e.to_string())
}

pub async fn build_state(
    home: &Path,
    bound_remote: bool,
    api_port: u16,
) -> Result<AppState, Box<dyn std::error::Error + Send + Sync>> {
    let events = broadcast::channel::<ServerEvent>(64).0;
    // 环由这一个订阅者喂:22 个既有发送点不必改签名。
    let pulse = crate::event_pulse::EventPulse::spawn(events.clone());

    let settings_events = broadcast::channel::<SettingsEvent>(64);
    let credentials_events = broadcast::channel::<CredentialEvent>(64);

    let settings = Arc::new(SettingsStore::open(home)?.with_events(settings_events.0.clone()));
    let credentials =
        Arc::new(CredentialStore::open(home)?.with_events(credentials_events.0.clone()));
    let registry = Arc::new(LlmRegistry::new());

    // settings.yaml 一次性迁移必须在 register_namespaces 之前:严格反序列化
    // 会让遗留的 runtime.maxAgents/maxDepth 直接阻断启动。失败即中止启动
    // (不带着半迁移的配置继续跑)。
    let migration = crate::subagents::migration::migrate_settings(home)?;
    for diagnostic in &migration.diagnostics {
        tracing::warn!(target: "subagents", "{diagnostic}");
    }
    if let crate::subagents::migration::MigrationOutcome::Migrated {
        from_max_agents,
        backup,
    } = &migration.outcome
    {
        tracing::info!(
            target: "subagents",
            from_max_agents = ?from_max_agents,
            backup = ?backup,
            "settings.yaml 子代理配置已迁移"
        );
    }

    register_namespaces(&settings)?;
    // 子代理定义仓库:随 home 固定,项目作用域按会话 cwd 解析。
    let subagent_profiles = Arc::new(crate::subagents::ProfileStore::new(home));

    // OpenAI-compatible routes follow their settings section.
    let openai = Arc::new(OpenAiCompatAdapter::new(
        settings.clone(),
        credentials.clone(),
    ));
    sync_openai_routes(&registry, &settings, &openai);

    // Sessions, tools, and the driver.
    let sessions = Arc::new(SessionStore::open(home)?);
    // 启动一次性清扫:删掉"从未发过消息"的残留空白会话(浏览器直接关闭、
    // 进程被杀留下的空壳)。放在数据层而不是前端,是因为前端每个页面只知道
    // 自己的焦点,却能看到全局会话列表 —— 在那里做"非活跃空白即删"会误删
    // 别的页面(或共用同一数据目录的另一实例)正在使用的会话。
    // 10 分钟年龄门槛给并发实例留缓冲,避免删掉用户正停着的草稿。
    // 必须在 bootstrap 之前跑,否则这些空壳会被写进工作区账本。
    let swept = sessions.sweep_stale_blanks(10 * 60 * 1000);
    // 工作区注册表:独立持久化域;首启按会话头 cwd 自动分组。
    let workspaces = Arc::new(crate::workspace::WorkspaceRegistry::open(home)?);
    // 已初始化过的账本里可能还挂着上次运行留下的空壳引用,一并去引用。
    for id in &swept {
        workspaces.detach_session(id);
    }
    if !swept.is_empty() {
        tracing::info!(count = swept.len(), "swept stale blank sessions at startup");
    }
    workspaces.bootstrap(
        &sessions
            .list()?
            .iter()
            .map(|summary| {
                (
                    summary.id.clone(),
                    summary.cwd.clone().unwrap_or_default(),
                    summary.created_at,
                )
            })
            .collect::<Vec<_>>(),
    );
    let live = Arc::new(LiveSessions::default());
    let browser = Arc::new(denia_browser::BrowserManager::new(home.to_path_buf()));
    let browser_hub: denia_tools::BrowserHub = browser.clone();
    // 面板终端中枢:交互式 PTY,与 `bash` 工具的非交互进程互不影响。
    let terminals = denia_terminal::TerminalManager::new();
    // agent preset 名册:随附集合 + 用户目录。driver 每个 step 装配时读它,
    // 因此文件热刷新后新 step 即生效。先于系统提示词构建:create_preset 的
    // schema 要在 system prompt 的 tools provider 里与纪律段同步挂载。
    let agent_presets = crate::agent_presets::PresetStore::load(home, settings.clone());
    let system_prompt = Arc::new(crate::system_prompt_store::SystemPromptState::load(
        home,
        Some(browser_hub.clone()),
        Some(crate::preset_tool::schema()),
        // 本实例 API 地址:runtime-context 快照注入,模型改配置时打对进程。
        Some(format!("http://127.0.0.1:{api_port}")),
    ));
    let file_history = Arc::new(crate::file_history::FileHistoryStore::new(home));
    let runtime = crate::agent_runtime::Runtime::new(
        home,
        sessions.clone(),
        live.clone(),
        settings.clone(),
        events.clone(),
        registry.clone(),
        workspaces.clone(),
        subagent_profiles.clone(),
    )?;
    let mut tools = denia_tools::default_registry_with_browser(Some(browser_hub.clone()));
    denia_tools::capabilities::register(&mut tools, runtime.clone());
    tools.replace(Arc::new(
        denia_tools::BashTool::new().with_runtime(runtime.clone()),
    ));
    // `ask` 工具:控制台部署有应答通道,注册工具并挂桥。
    tools.register(Arc::new(denia_tools::AskTool::new()));
    // `create_preset` 工具:创造模式(creator preset)的落盘入口,只在有
    // preset 名册的部署注册;纪律段(tool:preset)在 build_prompt 同步注入。
    tools.register(Arc::new(crate::preset_tool::CreatePresetTool::new(
        agent_presets.clone(),
    )));
    let approval = Arc::new(ServerApprovalBridge::new(live.clone()));
    let ask = Arc::new(ServerAskBridge::new(live.clone()));
    let console = console_settings(&settings);
    let driver = Arc::new(
        SessionDriver::new(registry.clone(), Arc::new(tools), system_prompt.handle())
            .with_file_history(file_history.clone())
            .with_approval(approval)
            .with_ask(ask)
            .with_runtime(runtime.clone())
            .with_presets(agent_presets.clone())
            .with_settings_source(Arc::new(ConsoleDriverSettingsSource(settings.clone())))
            .with_compaction(compaction_settings_from(&console))
            .with_microcompact(microcompact_settings_from(&console))
            .with_parallel(denia_agent_loop::ParallelSettings {
                max_parallel_tool_calls: console.max_parallel_tool_calls,
            }),
    );

    runtime.attach(&driver);
    // 浏览器归属清理：child 停止/结束时只关闭它自己的 tab（计划 6.4）。
    runtime.attach_browser(browser_hub.clone());

    // 会话空闲淘汰:30s 一轮,5 分钟未使用的会话卸载(磁盘日志不受影响)。
    //
    // 会话离开常驻表时,顺带回收按会话累积的辅助内存态:文件历史快照链与
    // 文件读取状态表(后者存文件正文,是单会话里最大的一块)。它们不属于
    // LiveSession,不在这里回收就只增不减。
    //
    // 空闲阈值取 5 分钟:早先的 10 分钟意味着一个上午开过的会话全在内存里,
    // 而重新加载一个会话只要 0.5~1.7s(实测),省下的常驻内存远比这点延迟值钱。
    {
        let driver_for_evict = driver.clone();
        let file_history_for_evict = file_history.clone();
        spawn_live_evictor(
            live.clone(),
            30,
            300,
            Arc::new(move |id: &str| {
                file_history_for_evict.forget(id);
                driver_for_evict.clear_read_state(id);
                driver_for_evict.clear_compaction_state(id);
                driver_for_evict.clear_mcp_loads(id);
            }),
        );
    }

    // 工具名册注入 preset 名册:用户 preset 的 tools 白名单引用了部署没有
    // 的工具时,名册把它标成 broken——白名单收窄取交集,不校验的话拼错的
    // 工具名会被静默丢掉。MCP 工具动态进出,`mcp__` 前缀的引用不校验。
    agent_presets.set_known_tools(crate::mcp_runtime::builtin_tool_names(&driver.tools()));

    // MCP:按 `mcp` 命名空间的配置连接外部服务器,并把它们的工具并入
    // 注册表与系统提示词。连接失败不阻断启动(单个服务器自己标 error)。
    // 提示词侧走单一来源:manager 先挂进 SystemPromptState(attach_mcp 重建),
    // 之后每次配置变更的 sync_prompt 只是再 reload 一次,不叠加残留。
    let mcp_manager = Arc::new(denia_mcp::McpManager::new(
        crate::mcp_runtime::builtin_tool_names(&driver.tools()),
    ));
    system_prompt.attach_mcp(mcp_manager.clone());
    let mcp = Arc::new(crate::mcp_runtime::McpRuntime::new(
        mcp_manager,
        settings.clone(),
        driver.clone(),
        system_prompt.clone(),
    ));
    mcp.apply().await;

    // 远程连接中枢:先于 spawn_forwarders 构造 —— 配置变更转发要引用它。
    // 启动时顺手清理上次异常退出留下的隧道进程与残留 pid 文件。
    let remote = Arc::new(crate::remote::RemoteManager::new(
        home.to_path_buf(),
        settings.clone(),
        events.clone(),
    ));
    remote.apply_settings();
    remote.cleanup_stale_tunnel();
    remote.spawn_sweeper();

    spawn_forwarders(
        settings_events.1,
        credentials_events.1,
        events.clone(),
        settings.clone(),
        registry.clone(),
        openai.clone(),
        remote.clone(),
    );
    system_prompt.spawn_watcher(events.clone());
    agent_presets.spawn_watcher(events.clone());

    let state = AppState {
        runtime,
        home: home.to_path_buf(),
        settings,
        credentials,
        registry,
        events,
        pulse,
        http: reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()?,
        sessions,
        live,
        driver,
        workspaces,
        system_prompt,
        agent_presets,
        subagent_profiles,
        file_history,
        browser,
        terminals,
        mcp,
        open_in_app: Arc::new(crate::open_in_app::OpenInAppState::new(bound_remote)),
        remote,
        bound_remote,
    };

    Ok(state)
}

fn register_namespaces(
    settings: &SettingsStore,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    settings.register(
        "runtime",
        NamespaceSpec {
            defaults: serde_json::to_value(crate::agent_runtime::RuntimeConfig::default())?,
            validate: crate::agent_runtime::validate_config,
            secrets: &[],
            applies: Applies::Live,
        },
        json!({}),
    )?;
    settings.register(
        "goals",
        NamespaceSpec {
            defaults: serde_json::to_value(crate::agent_runtime::GoalsConfig::default())?,
            validate: crate::agent_runtime::validate_goals_config,
            secrets: &[],
            applies: Applies::Live,
        },
        json!({}),
    )?;
    // 子代理调度:唯一持有并发上限的命名空间(没有深度配置——禁止子派子是
    // 运行时硬规则,不是可调参数)。
    settings.register(
        crate::subagents::SETTINGS_NS,
        NamespaceSpec {
            defaults: serde_json::to_value(crate::subagents::SubagentPolicyConfig::default())?,
            validate: crate::subagents::validate_policy,
            secrets: &[],
            applies: Applies::Live,
        },
        json!({}),
    )?;
    settings.register(
        CONSOLE_NS,
        NamespaceSpec {
            defaults: json!({ "theme": "system", "locale": "zh" }),
            validate: validate_console,
            secrets: &[],
            applies: Applies::Live,
        },
        json!({}),
    )?;
    settings.register(
        OPENAI_SETTINGS_NS,
        NamespaceSpec {
            defaults: json!({ "providers": {} }),
            validate: validate_openai,
            secrets: &[],
            applies: Applies::Live,
        },
        json!({}),
    )?;
    settings.register(
        crate::agent_presets::SETTINGS_NS,
        NamespaceSpec {
            defaults: json!({
                "default": denia_core::preset::DEFAULT_PRESET_ID,
                "modeSelectionEnabled": true,
            }),
            validate: validate_agent_presets,
            secrets: &[],
            applies: Applies::Live,
        },
        json!({}),
    )?;
    settings.register(
        crate::remote::config::REMOTE_NS,
        NamespaceSpec {
            defaults: serde_json::to_value(crate::remote::config::RemoteSettings::default())?,
            validate: crate::remote::config::validate_remote,
            secrets: &[],
            applies: Applies::Live,
        },
        json!({}),
    )?;
    settings.register(
        crate::mcp_settings::MCP_NS,
        NamespaceSpec {
            defaults: crate::mcp_settings::defaults(),
            validate: crate::mcp_settings::validate_mcp,
            // env 与 headers 的值都是凭据:整条路径脱敏,describe 永不回传明文。
            secrets: &[
                &["servers", "*", "env", "*"],
                &["servers", "*", "headers", "*"],
            ],
            applies: Applies::Live,
        },
        json!({}),
    )?;
    Ok(())
}

/// `agent-presets` 命名空间校验:默认 preset 必须是合法 id,模式选择开关
/// 必须是布尔。
///
/// 指向不存在的 preset 不在这里拒绝——名册随用户复制/删除而变,写入时
/// 无法预知;解析默认值时按名册校验并回退随附默认值(见
/// [`crate::agent_presets::PresetStore::default_id`])。
fn validate_agent_presets(value: Value) -> Result<Value, String> {
    let Some(default) = value.get("default") else {
        return Err("缺少 default(新建会话的默认 preset id)".to_string());
    };
    let Some(id) = default.as_str() else {
        return Err("default 必须是 preset id 字符串".to_string());
    };
    if !denia_core::preset::is_valid_preset_id(id) {
        return Err(format!("default 不是合法的 preset id:{id}"));
    }
    if let Some(mode_selection) = value.get("modeSelectionEnabled")
        && mode_selection.as_bool().is_none()
    {
        return Err("modeSelectionEnabled 必须是布尔值".to_string());
    }
    Ok(value)
}

fn sync_openai_routes(
    registry: &LlmRegistry,
    settings: &SettingsStore,
    adapter: &Arc<OpenAiCompatAdapter>,
) {
    let routes: Vec<String> = OpenAiSection::from_settings(settings)
        .map(|section| section.providers.keys().cloned().collect())
        .unwrap_or_default();
    registry.sync_routes(&routes, adapter.clone(), RetryPolicy::default());
}

/// Forwards store notifications onto the console push channel and keeps
/// OpenAI-compatible routes aligned with settings writes.
fn spawn_forwarders(
    mut settings_rx: broadcast::Receiver<SettingsEvent>,
    mut credentials_rx: broadcast::Receiver<CredentialEvent>,
    events: broadcast::Sender<ServerEvent>,
    settings: Arc<SettingsStore>,
    registry: Arc<LlmRegistry>,
    openai: Arc<OpenAiCompatAdapter>,
    remote: Arc<crate::remote::RemoteManager>,
) {
    let credential_events = events.clone();
    tokio::spawn(async move {
        loop {
            match settings_rx.recv().await {
                Ok(SettingsEvent::DocumentUpdated { ns, .. }) => {
                    let _ = events.send(ServerEvent::SettingsUpdated);
                    if ns == crate::remote::config::REMOTE_NS {
                        remote.apply_settings();
                        let _ = events.send(ServerEvent::RemoteUpdated);
                    }
                    if ns == OPENAI_SETTINGS_NS {
                        let before = registry.list_providers().len();
                        sync_openai_routes(&registry, &settings, &openai);
                        let after = registry.list_providers().len();
                        if before != after {
                            let _ = events.send(ServerEvent::LlmUpdated);
                        }
                    }
                }
                Ok(SettingsEvent::Updated { .. }) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    tokio::spawn(async move {
        loop {
            match credentials_rx.recv().await {
                Ok(CredentialEvent::ReferenceUpdated { .. }) => {
                    let _ = credential_events.send(ServerEvent::CredentialsUpdated);
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_tools::AskBridge as _;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn driver_reads_live_console_settings_without_waiting_for_a_forwarder() {
        let home = temp_root();
        let state = build_state(&home, false, 3600).await.unwrap();
        let running_snapshot = state.driver.driver_settings();
        state
            .settings
            .update(
                CONSOLE_NS,
                json!({
                    "maxParallelToolCalls": 2,
                    "compaction": {"compactEnabled": false},
                    "microcompact": {"microcompactEnabled": false}
                }),
                None,
            )
            .unwrap();
        let next_snapshot = state.driver.driver_settings();
        assert_eq!(next_snapshot.parallel.max_parallel_tool_calls, 2);
        assert!(!next_snapshot.compaction.compact_enabled);
        assert!(!next_snapshot.microcompact.enabled);
        assert_eq!(running_snapshot.parallel.max_parallel_tool_calls, 10);
        assert!(running_snapshot.compaction.compact_enabled);
    }

    fn temp_root() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn running_ids_lists_only_active_turns() {
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = SessionStore::open(&root).unwrap();
        let idle = store.create(&cwd, true).unwrap();
        let active = store.create(&cwd, true).unwrap();
        let live = LiveSessions::new(8, u64::MAX);
        let idle_live = live.get_or_load(&store, idle.id()).unwrap();
        let active_live = live.get_or_load(&store, active.id()).unwrap();
        active_live.running.store(true, Ordering::SeqCst);
        let mut ids = live.running_ids();
        ids.sort();
        assert_eq!(ids, vec![active.id().to_string()]);
        drop(idle_live);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn idle_history_cools_even_with_a_connected_follower() {
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = SessionStore::open(&root).unwrap();
        let session = store.create(&cwd, true).unwrap();
        session
            .append(denia_core::session::SessionEvent::UserMessage {
                text: "preserved".into(),
                injected: false,
                channel: None,
                images: Vec::new(),
            })
            .unwrap();
        session.flush().unwrap();
        let live = LiveSessions::new(8, u64::MAX);
        let handle = live.get_or_load(&store, session.id()).unwrap();
        live.ensure_hot(&handle).unwrap();
        let follower = handle.followers.subscribe();
        assert!(live.evict_idle(300).is_empty());
        assert!(handle.session.is_hot(), "in-use handle must not cool");
        handle.running.store(true, Ordering::SeqCst);
        let id = session.id().to_string();
        drop(handle);
        assert!(live.evict_idle(300).is_empty());
        let handle = live.get(&id).unwrap();
        assert!(handle.session.is_hot(), "running history must not cool");
        handle.running.store(false, Ordering::SeqCst);
        drop(handle);
        assert!(live.evict_idle(300).is_empty());
        let handle = live.get(&id).unwrap();
        assert!(!handle.session.is_hot());
        assert_eq!(handle.session.resident_bytes(), 0);
        assert_eq!(handle.followers.receiver_count(), 1);
        live.ensure_hot(&handle).unwrap();
        assert!(handle.session.is_hot());
        assert_eq!(handle.session.events().len(), 1);
        drop(follower);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn running_snapshot_serializes_for_sse() {
        let event = ServerEvent::RunningSnapshot {
            ids: vec!["s1".into(), "s2".into()],
        };
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["type"], "running-snapshot");
        assert_eq!(value["ids"], serde_json::json!(["s1", "s2"]));
    }

    #[test]
    fn byte_budget_evicts_idle_resident_sessions() {
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = SessionStore::open(&root).unwrap();
        let first = store.create(&cwd, true).unwrap();
        let second = store.create(&cwd, true).unwrap();
        // 预算 0:任何有驻留事件的会话都超限,逼出 LRU 淘汰路径。
        //
        // 预算口径是**真实驻留字节**而不是日志文件大小:只有头行的空会话
        // 驻留量确实是 0,不会被裁 —— 所以这里必须先写进一条事件,
        // 否则测的是"空会话不占预算"这条另一条语义。
        let live = LiveSessions::new(8, 0);
        // 持有期间即使超预算也不得淘汰(外部引用即"可能正在用")。
        let handle = live.get_or_load(&store, first.id()).unwrap();
        handle.session.ensure_hot().unwrap();
        handle
            .session
            .append(denia_core::session::SessionEvent::UserMessage {
                text: "hello".into(),
                injected: false,
                channel: None,
                images: Vec::new(),
            })
            .unwrap();
        assert!(handle.session.resident_bytes() > 0, "写入后应有驻留字节");
        assert!(live.get(first.id()).is_some());
        drop(handle);
        // 第二次加载触发裁剪:idle 且无引用的热会话被卸载,新会话保留。
        live.get_or_load(&store, second.id()).unwrap();
        assert!(live.get(first.id()).is_none());
        assert!(live.get(second.id()).is_some());
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 回归:热升级后必须真的能淘汰。
    ///
    /// 早先淘汰判定硬编码 `Arc::strong_count == 1`,而 `ensure_hot(&live)`
    /// 的调用方必然持有一个 Arc —— 计数至少是 2,条件恒假,被升级的会话
    /// 永远留在表里,驻留预算形同虚设(实测连续加载 10 个大会话,内存从
    /// 4 MB 单调涨到 124 MB 且不回落)。这里断言升级后裁剪确实生效。
    #[test]
    fn promotion_through_live_sessions_is_evictable() {
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = SessionStore::open(&root).unwrap();
        let first = store.create(&cwd, true).unwrap();
        let live = LiveSessions::new(8, 0);
        let handle = live.get_or_load(&store, first.id()).unwrap();
        // 走 LiveSessions 的入口升级(生产路径),而不是直接 session.ensure_hot。
        live.ensure_hot(&handle).unwrap();
        handle
            .session
            .append(denia_core::session::SessionEvent::UserMessage {
                text: "hello".into(),
                injected: false,
                channel: None,
                images: Vec::new(),
            })
            .unwrap();
        assert!(
            handle.session.resident_bytes() > 0,
            "热升级 + 写入后应有驻留字节"
        );
        drop(handle);
        // 预算 0 + 无外部引用:下一次裁剪必须把它收走。
        // 用一个新会话触发裁剪(它自己受保护,不参与本轮淘汰)。
        let second = store.create(&cwd, true).unwrap();
        live.get_or_load(&store, second.id()).unwrap();
        assert!(
            live.get(first.id()).is_none(),
            "热升级后的空闲会话必须可淘汰,否则驻留预算永远不生效"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cold_sessions_do_not_count_toward_byte_budget() {
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = SessionStore::open(&root).unwrap();
        let first = store.create(&cwd, true).unwrap();
        // 预算 0 + 全部冷会话:谁都不占预算,谁也不该被裁掉。
        let live = LiveSessions::new(8, 0);
        live.get_or_load(&store, first.id()).unwrap();
        assert!(
            live.get(first.id()).is_some(),
            "冷会话不驻留事件,不触发预算裁剪"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn ask_questions() -> Vec<denia_core::session::AskQuestion> {
        use denia_core::session::{AskOption, AskQuestion};
        vec![AskQuestion {
            id: "q1".to_string(),
            question: "继续吗?".to_string(),
            header: None,
            detail: None,
            options: vec![AskOption {
                label: "继续".to_string(),
                description: None,
                recommended: true,
            }],
            multi_select: false,
            allow_custom: true,
        }]
    }

    fn ask_setup() -> (std::path::PathBuf, Arc<LiveSessions>, String) {
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = SessionStore::open(&root).unwrap();
        let session = store.create(&cwd, true).unwrap();
        let id = session.id().to_string();
        let live = Arc::new(LiveSessions::new(8, u64::MAX));
        live.get_or_load(&store, &id).unwrap();
        (root, live, id)
    }

    /// 作答路径:挂起请求 → 外部应答 → 桥返回 Answered,并把结果落进日志。
    #[tokio::test]
    async fn ask_bridge_settles_on_answer() {
        use denia_core::session::{AskAnswer, AskOutcome, AskResolution};
        let (root, live, id) = ask_setup();
        let bridge = ServerAskBridge::new(live.clone());
        let live_for_answer = live.clone();
        let session_id = id.clone();
        // 等请求挂起后再应答(桥先落盘 AskRequested,再进 select)。
        tokio::spawn(async move {
            loop {
                let sender = {
                    let live = live_for_answer.get(&session_id).unwrap();
                    let mut pending = live.pending_asks.lock().unwrap();
                    pending.remove("req-1")
                };
                if let Some(sender) = sender {
                    let _ = sender.send(AskResolution {
                        outcome: AskOutcome::Answered,
                        answers: vec![AskAnswer {
                            id: "q1".to_string(),
                            selected: vec!["继续".to_string()],
                            custom: None,
                            skipped: false,
                        }],
                        reason: None,
                    });
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        });
        let resolution = bridge
            .ask(
                &id,
                "req-1",
                "call-1",
                &ask_questions(),
                5_000,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(resolution.outcome, AskOutcome::Answered);
        assert_eq!(resolution.answers[0].selected, vec!["继续".to_string()]);
        // 请求与结算都落盘:刷新页面能重建挂起态与结果。
        let events = live.get(&id).unwrap().session.events();
        assert!(events
            .iter()
            .any(|envelope| matches!(&envelope.event, denia_core::session::SessionEvent::AskRequested { request_id, .. } if request_id == "req-1")));
        assert!(events
            .iter()
            .any(|envelope| matches!(&envelope.event, denia_core::session::SessionEvent::AskResolved { request_id, .. } if request_id == "req-1")));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 超时路径:无人应答时按 TimedOut 结算,工具不会永久挂起。
    #[tokio::test]
    async fn ask_bridge_times_out_without_answer() {
        use denia_core::session::AskOutcome;
        let (root, live, id) = ask_setup();
        let bridge = ServerAskBridge::new(live.clone());
        let resolution = bridge
            .ask(
                &id,
                "req-2",
                "call-2",
                &ask_questions(),
                30,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(resolution.outcome, AskOutcome::TimedOut);
        // 超时后挂起表必须清空,重复应答会得到 404(不覆盖已结算结果)。
        assert!(
            live.get(&id)
                .unwrap()
                .pending_asks
                .lock()
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 取消路径:轮次中断时挂起提问立刻结算为 Cancelled。
    #[tokio::test]
    async fn ask_bridge_cancels_on_token() {
        use denia_core::session::AskOutcome;
        let (root, live, id) = ask_setup();
        let bridge = ServerAskBridge::new(live.clone());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let resolution = bridge
            .ask(&id, "req-3", "call-3", &ask_questions(), 60_000, cancel)
            .await;
        assert_eq!(resolution.outcome, AskOutcome::Cancelled);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 子代理不与用户交互:提问一律 unavailable,并给出可执行的下一步。
    #[tokio::test]
    async fn ask_bridge_refuses_subagent_sessions() {
        use denia_core::session::{AskOutcome, SubagentDescriptor};
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let store = SessionStore::open(&root).unwrap();
        let parent = store.create(&cwd, true).unwrap();
        let child = store
            .create_subagent(
                &cwd,
                true,
                parent.id(),
                SubagentDescriptor::legacy(
                    "worker",
                    1,
                    "default",
                    denia_core::config::ModelSelection {
                        provider: "test".to_string(),
                        model: "test".to_string(),
                        reasoning_effort: None,
                    },
                    None,
                    None,
                ),
            )
            .unwrap();
        let live = Arc::new(LiveSessions::new(8, u64::MAX));
        live.get_or_load(&store, child.id()).unwrap();
        let bridge = ServerAskBridge::new(live);
        let resolution = bridge
            .ask(
                child.id(),
                "req-5",
                "call-5",
                &ask_questions(),
                60_000,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(resolution.outcome, AskOutcome::Unavailable);
        let reason = resolution.reason.expect("子代理拒绝必须给出原因");
        assert!(reason.contains("子代理"), "{reason}");
        assert!(reason.contains("自行决策"), "{reason}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// 无会话时按 unavailable 结算(模型自行决策),而不是硬错误。
    #[tokio::test]
    async fn ask_bridge_reports_unavailable_without_session() {
        use denia_core::session::AskOutcome;
        let live = Arc::new(LiveSessions::new(8, u64::MAX));
        let bridge = ServerAskBridge::new(live);
        let resolution = bridge
            .ask(
                "missing",
                "req-4",
                "call-4",
                &ask_questions(),
                1_000,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(resolution.outcome, AskOutcome::Unavailable);
        assert!(resolution.reason.is_some());
    }
}
