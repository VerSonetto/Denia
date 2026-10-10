//! MCP 服务器进程池:配置 → 连接 → 工具清单 → 工具调用。
//!
//! 设计要点:
//! - **快照式**:对外只暴露一份不可变快照(`Arc<McpSnapshot>`),读路径
//!   无锁;配置变更时重建快照并原子替换,模型可见的工具面在一个 step
//!   边界内是一致的;
//! - **增量重连**:按 `connection_key` 判定——命令/参数/环境/工作目录没变
//!   就不重启子进程,只更新工具开关;变了的才重连;
//! - **失败隔离**:一个服务器连不上只把它标成 error(带中文原因),其余
//!   服务器照常可用;
//! - **结果身份**:长结果缓存后拿到 `result_id`,续读必须带令牌。参数相同
//!   但没有令牌 = 新调用 = 真实执行;令牌还会校验工具与会话。找不到原结果
//!   时报错提示迁移,绝不静默重跑(外部工具有副作用);
//! - **工具名冲突**:与内置工具同名时跳过该 MCP 工具并在状态里说明,绝
//!   不允许覆盖 `bash`/`read_file` 这类内置能力。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::RwLock;

use crate::client::{McpClient, McpClientError};
use crate::config::McpServerConfig;

/// 一个服务器的连接状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpServerStatus {
    /// 已连接并拿到工具清单。
    Connected,
    /// 用户禁用(未启动)。
    Disabled,
    /// 连接/握手/拉取失败;原因见 `error`。
    Error,
}

impl McpServerStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Disabled => "disabled",
            Self::Error => "error",
        }
    }
}

/// 一个 MCP 工具(模型可见面)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolState {
    /// 服务器原始工具名(发给服务器时用)。
    pub name: String,
    /// 模型可见名 `mcp__<server>__<tool>`。
    pub qualified: String,
    pub description: String,
    /// 是否交给模型(服务器启用 + 未被逐条关闭 + 无命名冲突)。
    pub enabled: bool,
    /// 未交给模型的原因(冲突/被关闭);`None` 表示正常启用。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

/// 一个服务器在快照里的全部状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerState {
    pub id: String,
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    /// http / sse 传输的服务端地址;stdio 为 `null`。
    pub url: Option<String>,
    /// 环境变量:只回传 key(值是凭据,不出现在 UI 与日志里)。
    pub env_keys: Vec<String>,
    /// 附加请求头:只回传 key(值如 Authorization 属凭据)。
    pub header_keys: Vec<String>,
    pub cwd: Option<String>,
    pub enabled: bool,
    pub status: McpServerStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub tools: Vec<McpToolState>,
}

impl McpServerState {
    /// 模型可见的工具数量(已启用)。
    pub fn active_tool_count(&self) -> usize {
        self.tools.iter().filter(|tool| tool.enabled).count()
    }
}

/// 某一时刻的 MCP 全貌(服务器 + 可调用入口)。
pub struct McpSnapshot {
    pub servers: Vec<McpServerState>,
    /// 模型可见名 → (服务器 id, 原始工具名)。
    routes: HashMap<String, (String, String)>,
    /// 模型可见名 → 工具 schema 参数(MCP 的 inputSchema)。
    schemas: HashMap<String, Value>,
    /// 模型可见名 → description。
    descriptions: HashMap<String, String>,
}

impl McpSnapshot {
    fn empty() -> Self {
        Self {
            servers: Vec::new(),
            routes: HashMap::new(),
            schemas: HashMap::new(),
            descriptions: HashMap::new(),
        }
    }

    /// 所有模型可见的 MCP 工具名(按名排序,输出稳定)。
    pub fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.routes.keys().cloned().collect();
        names.sort();
        names
    }

    /// 构建工具 schema 所需的三元组(名/描述/参数 schema)。
    pub fn tool_defs(&self) -> Vec<(String, String, Value)> {
        let mut defs: Vec<(String, String, Value)> = self
            .tool_names()
            .into_iter()
            .map(|name| {
                let description = self.descriptions.get(&name).cloned().unwrap_or_default();
                let schema = self
                    .schemas
                    .get(&name)
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
                (name, description, schema)
            })
            .collect();
        defs.sort_by(|left, right| left.0.cmp(&right.0));
        defs
    }

    /// 解析一次调用的路由;未注册的名字返回 `None`(派发处合成错误)。
    pub fn route(&self, qualified: &str) -> Option<(&str, &str)> {
        self.routes
            .get(qualified)
            .map(|(server, tool)| (server.as_str(), tool.as_str()))
    }

    /// 是否有至少一个已连接的服务器(决定纪律段是否注入)。
    pub fn has_connected(&self) -> bool {
        self.servers
            .iter()
            .any(|server| server.status == McpServerStatus::Connected && server.enabled)
    }
}

/// 一个已连接的服务器进程句柄 + 它的工具清单。
struct ConnectedServer {
    client: Arc<McpClient>,
    tools: Vec<crate::protocol::McpToolDef>,
    /// 建立这条连接时的配置指纹;配置变了才重连。
    fingerprint: String,
}

fn fingerprint_of(config: &McpServerConfig) -> String {
    serde_json::to_string(&(
        &config.transport,
        &config.command,
        &config.args,
        &config.env,
        &config.cwd,
    ))
    .unwrap_or_default()
}

/// MCP 工具结果的分页页大小(字符)。
///
/// MCP 工具返回的是任意文本(JSON/日志/网页抽取…),按**字符**分页比按行
/// 分页更贴合:一次给一页,模型需要更多时用 offset 翻页,不必重跑工具。
pub const PAGE_CHARS: usize = 8_000;
/// 单次调用允许的最大页(字符)。
pub const MAX_PAGE_CHARS: usize = 32_000;
/// 分页缓存的单条上限(字符):超过则不缓存(提示模型缩小范围,别硬翻)。
pub const MAX_CACHED_CHARS: usize = 1_048_576;
/// 分页缓存条目上限;超出按「最久未读」淘汰单条,不整表清空——整表清空
/// 会把其它还能续读的结果一起作废。
const MAX_CACHE_ENTRIES: usize = 256;

/// 一次分页调用的入参。
///
/// 区分「新调用」与「续读某个已有结果」是这里唯一重要的事:
/// - `result_id` 为 `None` 且未请求旧式续读 → **新调用**,真实执行外部工具
///   (即使参数与上一次完全相同);
/// - `result_id` 为 `Some` → 读取那次结果的后续页,绝不重新执行;
/// - `legacy_resume` → 模型给了 `offset>0` 却没带令牌(旧写法),只在能唯一
///   确定原结果时兼容,否则报错提示迁移。
pub struct PageCall<'a> {
    /// 模型可见的工具名(`mcp__<server>__<tool>`)。
    pub qualified: &'a str,
    /// 已剔除 denia 私有分页字段的参数(原样发给服务器)。
    pub arguments: &'a Value,
    /// 本页起始字符(0-based)。
    pub offset: usize,
    /// 本页字符数上限。
    pub limit: usize,
    /// 结果身份令牌;带它调用 = 续读那次结果,不带 = 新调用。
    pub result_id: Option<&'a str>,
    /// 旧式续读:模型没带令牌,只用 `offset>0` 表达「我要看后面」。
    pub legacy_resume: bool,
    /// 结果所属会话;结果不跨会话复用。
    pub session: Option<&'a str>,
}

/// 一次分页调用的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct PagedResult {
    /// 本页文本(已按 offset/limit 切片)。
    pub text: String,
    /// MCP 侧声明的业务失败(不是协议错误);跟着结果走,翻页不再丢。
    pub is_error: bool,
    /// 完整结果总字符数。
    pub total_chars: usize,
    /// 本页起始字符偏移(0-based)。
    pub offset: usize,
    /// 本页字符数。
    pub shown_chars: usize,
    /// 是否还有后续内容。
    pub has_more: bool,
    /// 本次没有重新执行工具,而是复用了既有结果(翻页/旧式续读)。
    pub from_cache: bool,
    /// 完整结果过长,未缓存:无法翻页。
    pub uncached: bool,
    /// 结果身份:带上它 + 更大的 `offset` 可续读后续页;未缓存时为 `None`。
    pub result_id: Option<String>,
}

/// 分页调用失败的原因(决定工具层给模型什么提示)。
///
/// 三条与结果身份有关的失败**都不是「帮你重跑一次」**:找不到原结果时宁可
/// 报错,也不静默重新执行可能有副作用的外部工具。
#[derive(Debug, Clone, PartialEq)]
pub enum PagedError {
    /// 令牌不存在:从未产生、已被淘汰,或服务器重连后作废。
    UnknownResult { result_id: String },
    /// 令牌属于另一个会话。
    ForeignSession { result_id: String },
    /// 旧式 `offset` 续读,但那次结果已经不在了。
    LostResult,
    /// 外部工具调用失败(未连接/协议错误/未知工具)。
    Call(String),
}

impl std::fmt::Display for PagedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownResult { result_id } => {
                write!(f, "结果 {result_id} 已不存在(从未产生、已被淘汰或服务器重连)")
            }
            Self::ForeignSession { result_id } => {
                write!(f, "结果 {result_id} 属于另一个会话,不能在这里续读")
            }
            Self::LostResult => write!(f, "没有可续读的结果"),
            Self::Call(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for PagedError {}

/// 缓存里的一条结果:身份 → 某次真实执行的完整输出。
struct CachedResult {
    /// 结果所属会话(`None` = 无会话上下文);跨会话不复用。
    session: Option<String>,
    /// 产出该结果的工具(修饰名);令牌不能跨工具使用。
    tool: String,
    /// 产出该结果时的服务器参数(规范化 JSON 文本)。
    ///
    /// 只用于旧式 `offset` 续读的匹配,**不当身份用**:身份一律是令牌。
    arguments: String,
    /// 完整输出文本。
    text: String,
    /// MCP 侧声明的业务失败标记;跟着结果走,翻页不再丢。
    is_error: bool,
    /// 最近一次被读取的顺序号(LRU 淘汰用)。
    seq: u64,
}

/// 结果缓存:令牌 → 结果。
#[derive(Default)]
struct ResultCache {
    entries: HashMap<String, CachedResult>,
    next_seq: u64,
    next_id: u64,
}

/// MCP 服务器进程池。
pub struct McpManager {
    /// 已连接的服务器:id → 句柄。
    connected: RwLock<HashMap<String, ConnectedServer>>,
    /// 当前快照。
    ///
    /// 用 `ArcSwap` 而不是 `RwLock`:读路径在**同步**上下文里走(系统提示
    /// 装配、注册表构建),不能 await,也不该被写锁卡住——配置变更只在
    /// reload 时整体换一个新快照,读到的永远是一份自洽的工具面。
    snapshot: ArcSwap<McpSnapshot>,
    /// 内置工具名:与它们冲突的 MCP 工具一律跳过,不覆盖。
    builtin_names: HashSet<String>,
    /// 子进程工作目录缺省值(通常是 denia 的 home 或会话 cwd,目前未用)。
    default_cwd: Option<PathBuf>,
    /// 结果缓存:身份令牌 → 某次真实执行的完整结果。
    ///
    /// 续读必须复用上次结果——MCP 工具可能有副作用(下单、发消息),
    /// 用 offset 再看一页时不该把工具跑第二遍。但**只有能确定原结果身份
    /// 时才复用**:靠令牌,不是靠「工具 + 参数」猜。
    results: Mutex<ResultCache>,
}

impl McpManager {
    pub fn new(builtin_names: HashSet<String>) -> Self {
        Self {
            connected: RwLock::new(HashMap::new()),
            snapshot: ArcSwap::from_pointee(McpSnapshot::empty()),
            builtin_names,
            default_cwd: None,
            results: Mutex::new(ResultCache::default()),
        }
    }

    /// 当前快照(同步读,无 await;同步上下文也能拿到)。
    pub fn snapshot(&self) -> arc_swap::Guard<Arc<McpSnapshot>> {
        self.snapshot.load()
    }

    /// 按配置重建:增量重连 + 重建快照。
    ///
    /// 失败隔离:单个服务器连不上只影响它自己。
    pub async fn reload(&self, configs: &[McpServerConfig]) {
        let mut connected = self.connected.write().await;
        let desired: BTreeMap<String, &McpServerConfig> = configs
            .iter()
            .map(|config| (config.id.clone(), config))
            .collect();

        // 1) 移除配置里已不存在的服务器。
        let stale: Vec<String> = connected
            .keys()
            .filter(|id| !desired.contains_key(*id))
            .cloned()
            .collect();
        for id in stale {
            if let Some(entry) = connected.remove(&id) {
                entry.client.shutdown().await;
            }
        }

        // 2) 逐个对齐:禁用/配置变更 → 断开;未连接或连接键变了 → 重连。
        let mut states: Vec<McpServerState> = Vec::with_capacity(configs.len());
        let mut schema_src: HashMap<String, Value> = HashMap::new();
        // 服务器集合变了:旧结果的身份随之作废。作废不等于重跑——模型再
        // 拿旧令牌来续读只会得到「结果已不存在」的提示,不会被静默执行。
        self.results.lock().unwrap().entries.clear();
        for config in configs {
            let stale_connection = connected
                .get(&config.id)
                .is_some_and(|entry| entry.fingerprint != fingerprint_of(config));
            if (stale_connection || (!config.enabled && connected.contains_key(&config.id)))
                && let Some(entry) = connected.remove(&config.id)
            {
                entry.client.shutdown().await;
            }
            if !config.enabled {
                states.push(disabled_state(config));
                continue;
            }
            if !connected.contains_key(&config.id) {
                match self.spawn(config).await {
                    Ok(entry) => {
                        connected.insert(config.id.clone(), entry);
                    }
                    Err(error) => {
                        tracing::warn!(server = %config.id, %error, "MCP 服务器连接失败");
                        states.push(error_state(config, error.to_string()));
                        continue;
                    }
                }
            }
            // 走到这里一定已连接(刚连上或此前已连且指纹未变)。
            let entry = connected.get(&config.id).expect("just connected");
            let state = self.connected_state(config, entry);
            for tool in &entry.tools {
                schema_src.insert(
                    crate::config::qualify_tool_name(&config.id, &tool.name),
                    tool.input_schema.clone(),
                );
            }
            states.push(state);
        }

        self.snapshot.store(Arc::new(build_snapshot(
            &states,
            &schema_src,
            &self.builtin_names,
        )));
    }

    async fn spawn(&self, config: &McpServerConfig) -> Result<ConnectedServer, McpClientError> {
        let client = McpClient::connect(config).await?;
        let tools = client.list_tools().await?;
        Ok(ConnectedServer {
            client: Arc::new(client),
            tools,
            fingerprint: fingerprint_of(config),
        })
    }

    fn connected_state(&self, config: &McpServerConfig, entry: &ConnectedServer) -> McpServerState {
        let disabled: HashSet<&str> = config.disabled_tools.iter().map(String::as_str).collect();
        let tools = entry
            .tools
            .iter()
            .map(|tool| {
                let qualified = crate::config::qualify_tool_name(&config.id, &tool.name);
                let name_conflict = self.builtin_names.contains(&qualified);
                let disabled_reason = if name_conflict {
                    Some(format!(
                        "与内置工具 {} 同名,已跳过以免覆盖内置能力",
                        qualified
                    ))
                } else if disabled.contains(tool.name.as_str()) {
                    Some("已在设置中关闭".to_string())
                } else {
                    None
                };
                McpToolState {
                    name: tool.name.clone(),
                    qualified,
                    description: tool.description.clone(),
                    enabled: disabled_reason.is_none(),
                    disabled_reason,
                }
            })
            .collect();
        McpServerState {
            id: config.id.clone(),
            transport: config.transport.clone(),
            command: config.command.clone(),
            args: config.args.clone(),
            env_keys: config.env.keys().cloned().collect(),
            header_keys: config.headers.keys().cloned().collect(),
            url: config.url.clone(),
            cwd: config.cwd.as_ref().map(|path| path.display().to_string()),
            enabled: true,
            status: McpServerStatus::Connected,
            error: None,
            tools,
        }
    }

    /// 调用一个 MCP 工具(模型可见名 → 服务器 + 原始工具名),并按
    /// `offset`/`limit` 分页返回。
    ///
    /// 分页语义:
    /// - **新调用**(没带 `result_id`、也不是旧式续读)一律真实执行外部工具,
    ///   即使参数与上一次完全相同;长结果执行后缓存全文并拿到一个结果身份,
    ///   尾部提示里把它交给模型;
    /// - **续读**(带 `result_id`)只读缓存里那一条结果,绝不重跑工具——MCP
    ///   工具可能有副作用(下单、发消息);
    /// - 旧式 `offset>0`(不带令牌)只在能唯一确定原结果时兼容,否则
    ///   返回 [`PagedError::LostResult`] 让工具层提示迁移,**不静默重跑**;
    /// - 结果大到不缓存(>1MB)时不提供翻页,提示模型缩小范围。
    pub async fn call_paged(&self, call: &PageCall<'_>) -> Result<PagedResult, PagedError> {
        let qualified = call.qualified;
        // 1) 带令牌:读取那一次已有结果的后续页。
        if let Some(result_id) = call.result_id {
            let (text, is_error) = self.resume_result(result_id, qualified, call.session)?;
            return Ok(paged_of(
                &text,
                is_error,
                call,
                true,
                Some(result_id.to_string()),
            ));
        }
        // 2) 旧式 offset 续读:模型没带令牌。
        if call.legacy_resume {
            let arguments = arguments_key(call.arguments);
            match self.latest_result(qualified, &arguments, call.session) {
                Some((result_id, text, is_error)) => {
                    return Ok(paged_of(&text, is_error, call, true, Some(result_id)));
                }
                None => return Err(PagedError::LostResult),
            }
        }
        // 3) 新调用:真实执行一次,并把这次结果登记上身份。
        let result = self
            .call(qualified, call.arguments.clone())
            .await
            .map_err(PagedError::Call)?;
        let result_id = self.store_result(qualified, call.arguments, call.session, &result);
        Ok(paged_of(&result.text, result.is_error, call, false, result_id))
    }

    /// 按令牌取一条仍然有效的结果;命中即刷新它的 LRU 顺序。
    ///
    /// 三重校验(存在 / 同工具 / 同会话);任何一项不满足都是错误而不是
    /// 「重新执行一次」——外部副作用不该因为一次失效而重复发生。
    fn resume_result(
        &self,
        result_id: &str,
        qualified: &str,
        session: Option<&str>,
    ) -> Result<(String, bool), PagedError> {
        let mut cache = self.results.lock().unwrap();
        let (text, is_error) = match cache.entries.get(result_id) {
            Some(entry) if entry.tool != qualified => {
                return Err(PagedError::UnknownResult {
                    result_id: result_id.to_string(),
                });
            }
            Some(entry) if entry.session.as_deref() != session => {
                return Err(PagedError::ForeignSession {
                    result_id: result_id.to_string(),
                });
            }
            Some(entry) => (entry.text.clone(), entry.is_error),
            None => {
                return Err(PagedError::UnknownResult {
                    result_id: result_id.to_string(),
                });
            }
        };
        cache.next_seq += 1;
        let seq = cache.next_seq;
        if let Some(entry) = cache.entries.get_mut(result_id) {
            entry.seq = seq;
        }
        Ok((text, is_error))
    }

    /// 旧式续读的候选:同一工具 + 同一参数 + 同一会话下最近一次的结果。
    ///
    /// 这是唯一一处仍按「工具 + 参数」找结果的地方,只服务于尚未迁移到
    /// 令牌的旧写法;找不到就报错,绝不把参数相同当作「就是要重跑」。
    fn latest_result(
        &self,
        qualified: &str,
        arguments: &str,
        session: Option<&str>,
    ) -> Option<(String, String, bool)> {
        let cache = self.results.lock().unwrap();
        cache
            .entries
            .iter()
            .filter(|(_, entry)| {
                entry.tool == qualified
                    && entry.arguments == arguments
                    && entry.session.as_deref() == session
            })
            .max_by_key(|(_, entry)| entry.seq)
            .map(|(id, entry)| (id.clone(), entry.text.clone(), entry.is_error))
    }

    /// 登记一次真实执行的结果,返回它的身份令牌;不可翻页/超长时返回 `None`。
    fn store_result(
        &self,
        qualified: &str,
        arguments: &Value,
        session: Option<&str>,
        result: &crate::protocol::McpCallResult,
    ) -> Option<String> {
        let total_chars = result.text.chars().count();
        if result.text.is_empty() || total_chars > MAX_CACHED_CHARS {
            // 空结果没什么可翻;超大结果不缓存,让工具层提示缩小范围。
            return None;
        }
        let mut cache = self.results.lock().unwrap();
        if cache.entries.len() >= MAX_CACHE_ENTRIES
            && let Some(oldest) = cache
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.seq)
                .map(|(id, _)| id.clone())
        {
            cache.entries.remove(&oldest);
        }
        cache.next_id += 1;
        cache.next_seq += 1;
        let (result_id, seq) = (
            format!("res-{}-{}", std::process::id(), cache.next_id),
            cache.next_seq,
        );
        cache.entries.insert(
            result_id.clone(),
            CachedResult {
                session: session.map(str::to_string),
                tool: qualified.to_string(),
                arguments: arguments_key(arguments),
                text: result.text.clone(),
                is_error: result.is_error,
                seq,
            },
        );
        Some(result_id)
    }

    /// 调用一个 MCP 工具(模型可见名 → 服务器 + 原始工具名),返回完整文本。
    ///
    /// 工具层(分页/截断)用 [`Self::call_paged`];这里保留完整结果通道,
    /// 供将来需要全文的场景(如缓存层)使用。
    pub async fn call(
        &self,
        qualified: &str,
        arguments: Value,
    ) -> Result<crate::protocol::McpCallResult, String> {
        let snapshot = self.snapshot.load();
        let (server, tool) = snapshot
            .route(qualified)
            .map(|(server, tool)| (server.to_string(), tool.to_string()))
            .ok_or_else(|| format!("unknown tool: {qualified}"))?;
        let client = {
            let connected = self.connected.read().await;
            connected
                .get(&server)
                .map(|entry| entry.client.clone())
                .ok_or_else(|| {
                    format!(
                        "MCP 服务器 {server} 当前未连接;到设置 → MCP 检查该服务器状态,或点\"重新连接\"后重试"
                    )
                })?
        };
        client
            .call_tool(&tool, arguments)
            .await
            .map_err(|error| error.to_string())
    }

    /// 断开一个服务器(不清配置);随后由 `reload` 决定是否重连。
    pub async fn disconnect(&self, id: &str) {
        let mut connected = self.connected.write().await;
        if let Some(entry) = connected.remove(id) {
            entry.client.shutdown().await;
        }
        drop(connected);
        self.results.lock().unwrap().entries.clear();
    }

    /// 关闭全部子进程(进程退出/配置清空时调用)。
    pub async fn shutdown_all(&self) {
        let mut connected = self.connected.write().await;
        for (_, entry) in connected.drain() {
            entry.client.shutdown().await;
        }
        self.results.lock().unwrap().entries.clear();
        self.snapshot.store(Arc::new(McpSnapshot::empty()));
    }
}

/// 按字符切片一页,并把分页事实折进返回结构。
fn page_text(
    text: &str,
    is_error: bool,
    offset: usize,
    limit: usize,
    from_cache: bool,
) -> PagedResult {
    let limit = limit.clamp(1, MAX_PAGE_CHARS);
    let total_chars = text.chars().count();
    let start = offset.min(total_chars);
    let page: String = text.chars().skip(start).take(limit).collect();
    let shown_chars = page.chars().count();
    PagedResult {
        text: page,
        is_error,
        total_chars,
        offset: start,
        shown_chars,
        has_more: start + shown_chars < total_chars,
        from_cache,
        // 没进缓存就翻不了页:如实告知,别让模型以为 offset 还能往前推。
        uncached: total_chars > MAX_CACHED_CHARS,
        // 身份由调用方在拿到缓存令牌后补上(纯切片函数不知道结果是谁的)。
        result_id: None,
    }
}

/// 切片一页并带上结果身份。
fn paged_of(
    text: &str,
    is_error: bool,
    call: &PageCall<'_>,
    from_cache: bool,
    result_id: Option<String>,
) -> PagedResult {
    let mut paged = page_text(text, is_error, call.offset, call.limit, from_cache);
    paged.result_id = result_id;
    paged
}

/// 服务器参数的规范化文本(仅用于旧式 `offset` 续读的匹配,不作身份)。
///
/// 必须对对象键排序后再序列化:模型两次调用即使用参完全相同,键顺序
/// 也可能不同(取决于生成顺序),直接 `to_string` 会把它们判成不同结果,
/// 让合法的翻页退化成 `LostResult`。
fn arguments_key(arguments: &Value) -> String {
    fn canonical(value: &Value, out: &mut String) {
        match value {
            Value::Object(map) => {
                let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                entries.sort_unstable_by(|left, right| left.0.cmp(right.0));
                out.push('{');
                for (index, (key, child)) in entries.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(key.as_str()).unwrap_or_default());
                    out.push(':');
                    canonical(child, out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    canonical(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&serde_json::to_string(other).unwrap_or_default()),
        }
    }

    let mut out = String::new();
    canonical(arguments, &mut out);
    out
}

fn disabled_state(config: &McpServerConfig) -> McpServerState {
    McpServerState {
        id: config.id.clone(),
        transport: config.transport.clone(),
        command: config.command.clone(),
        args: config.args.clone(),
        env_keys: config.env.keys().cloned().collect(),
        header_keys: config.headers.keys().cloned().collect(),
        url: config.url.clone(),
        cwd: config.cwd.as_ref().map(|path| path.display().to_string()),
        enabled: false,
        status: McpServerStatus::Disabled,
        error: None,
        tools: Vec::new(),
    }
}

fn error_state(config: &McpServerConfig, error: String) -> McpServerState {
    McpServerState {
        id: config.id.clone(),
        transport: config.transport.clone(),
        command: config.command.clone(),
        args: config.args.clone(),
        env_keys: config.env.keys().cloned().collect(),
        header_keys: config.headers.keys().cloned().collect(),
        url: config.url.clone(),
        cwd: config.cwd.as_ref().map(|path| path.display().to_string()),
        enabled: true,
        status: McpServerStatus::Error,
        error: Some(error),
        tools: Vec::new(),
    }
}

/// 快照构建:只把"启用且无冲突"的工具放进路由与 schema 表。
///
/// `schemas` 需要每个工具的 inputSchema,它只有在连接着的服务器上才有,
/// 所以这里额外接一份 `工具修饰名 → inputSchema` 的输入(由 manager 在
/// 重建时从各连接里收集);连接已断的服务器不提供 schema,自然也不进
/// 模型可见面。
fn build_snapshot(
    states: &[McpServerState],
    schema_src: &HashMap<String, Value>,
    builtin_names: &HashSet<String>,
) -> McpSnapshot {
    let mut routes = HashMap::new();
    let mut schemas_map = HashMap::new();
    let mut descriptions = HashMap::new();
    for state in states {
        if state.status != McpServerStatus::Connected || !state.enabled {
            continue;
        }
        for tool in &state.tools {
            if !tool.enabled || builtin_names.contains(&tool.qualified) {
                continue;
            }
            routes.insert(
                tool.qualified.clone(),
                (state.id.clone(), tool.name.clone()),
            );
            descriptions.insert(tool.qualified.clone(), tool.description.clone());
            // 缺 schema 的工具给默认 object:模型仍能调用无参工具,
            // 不会因为服务器少给一个字段就整条消失。
            let schema = schema_src
                .get(&tool.qualified)
                .cloned()
                .unwrap_or_else(|| serde_json::json!({ "type": "object" }));
            schemas_map.insert(tool.qualified.clone(), schema);
        }
    }
    McpSnapshot {
        servers: states.to_vec(),
        routes,
        schemas: schemas_map,
        descriptions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(id: &str, enabled: bool) -> McpServerConfig {
        McpServerConfig {
            id: id.to_string(),
            command: "mcp-fixture".to_string(),
            enabled,
            ..Default::default()
        }
    }

    /// 构造一个"已连接"的服务器状态(测试用)。
    fn connected_state_with(tools: Vec<McpToolState>) -> McpServerState {
        let mut state = disabled_state(&config("fs", true));
        state.status = McpServerStatus::Connected;
        state.enabled = true;
        state.tools = tools;
        state
    }

    fn tool(name: &str, qualified: &str, enabled: bool, reason: Option<&str>) -> McpToolState {
        McpToolState {
            name: name.to_string(),
            qualified: qualified.to_string(),
            description: "测试工具".to_string(),
            enabled,
            disabled_reason: reason.map(|text| text.to_string()),
        }
    }

    fn no_schemas() -> HashMap<String, Value> {
        HashMap::new()
    }

    #[test]
    fn disabled_servers_have_no_tools() {
        let states = vec![disabled_state(&config("fs", false))];
        let snapshot = build_snapshot(&states, &no_schemas(), &HashSet::new());
        assert!(snapshot.tool_names().is_empty());
        assert!(!snapshot.has_connected());
    }

    #[test]
    fn error_servers_expose_reason_but_no_tools() {
        let states = vec![error_state(&config("fs", true), "启动失败".to_string())];
        let snapshot = build_snapshot(&states, &no_schemas(), &HashSet::new());
        assert_eq!(snapshot.servers[0].error.as_deref(), Some("启动失败"));
        assert!(snapshot.tool_names().is_empty());
    }

    #[test]
    fn builtin_name_conflict_is_skipped() {
        let state = connected_state_with(vec![
            tool("bash", "bash", false, Some("与内置工具 bash 同名")),
            tool("read", "mcp__fs__read", true, None),
        ]);
        let builtin: HashSet<String> = ["bash".to_string()].into_iter().collect();
        let snapshot = build_snapshot(&[state], &no_schemas(), &builtin);
        assert_eq!(snapshot.tool_names(), vec!["mcp__fs__read".to_string()]);
        assert!(snapshot.has_connected());
    }

    #[test]
    fn tool_toggled_off_never_reaches_the_model() {
        let state = connected_state_with(vec![tool(
            "write",
            "mcp__fs__write",
            false,
            Some("已在设置中关闭"),
        )]);
        let snapshot = build_snapshot(&[state], &no_schemas(), &HashSet::new());
        assert!(snapshot.tool_names().is_empty());
        // 状态里仍保留(UI 要展示它,并允许重新打开)。
        assert_eq!(snapshot.servers[0].tools.len(), 1);
        assert_eq!(snapshot.servers[0].active_tool_count(), 0);
    }

    #[test]
    fn missing_input_schema_falls_back_to_object() {
        let state = connected_state_with(vec![tool("ping", "mcp__fs__ping", true, None)]);
        let snapshot = build_snapshot(&[state], &no_schemas(), &HashSet::new());
        let defs = snapshot.tool_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].2, serde_json::json!({ "type": "object" }));
    }

    #[test]
    fn routes_resolve_qualified_names() {
        let state = connected_state_with(vec![tool("read file", "mcp__fs__read_file", true, None)]);
        let snapshot = build_snapshot(&[state], &no_schemas(), &HashSet::new());
        assert_eq!(
            snapshot.route("mcp__fs__read_file"),
            Some(("fs", "read file"))
        );
        assert!(snapshot.route("mcp__fs__nope").is_none());
    }

    #[test]
    fn paging_slices_by_chars_and_reports_more() {
        let text = "abcdefghij";
        let first = page_text(text, false, 0, 4, false);
        assert_eq!(first.text, "abcd");
        assert_eq!(first.offset, 0);
        assert_eq!(first.shown_chars, 4);
        assert_eq!(first.total_chars, 10);
        assert!(first.has_more);

        let second = page_text(text, false, 4, 4, true);
        assert_eq!(second.text, "efgh");
        assert!(second.from_cache);
        assert!(second.has_more);

        let last = page_text(text, false, 8, 4, true);
        assert_eq!(last.text, "ij");
        assert!(!last.has_more);
    }

    #[test]
    fn paging_clamps_offset_beyond_end_and_limit() {
        let text = "abc";
        // offset 超出总长:空页但总量如实上报,模型知道自己在哪。
        let beyond = page_text(text, false, 100, 10, false);
        assert_eq!(beyond.text, "");
        assert_eq!(beyond.offset, 3);
        assert_eq!(beyond.total_chars, 3);
        assert!(!beyond.has_more);
        // limit 被夹到上限内,不因超大 limit 出错。
        let huge = page_text(text, false, 0, usize::MAX, false);
        assert_eq!(huge.text, "abc");
    }

    #[test]
    fn oversized_results_are_marked_uncached() {
        let text = "字".repeat(super::MAX_CACHED_CHARS + 1);
        let paged = page_text(&text, false, 0, 32, false);
        assert!(paged.uncached);
        assert!(paged.has_more);
    }

    #[test]
    fn paging_preserves_error_flag() {
        let paged = page_text("boom", true, 0, 10, false);
        assert!(paged.is_error);
        // 纯切片函数不知道结果是谁的:身份由 call_paged 补。
        assert!(paged.result_id.is_none());
    }

    // —— 结果身份 / 会话归属 / 旧式续读 ——

    /// 登记一次「真实执行」的产出(不用真起服务器也能验证缓存语义)。
    fn store(manager: &McpManager, session: Option<&str>, text: &str, is_error: bool) -> String {
        let result = crate::protocol::McpCallResult {
            text: text.to_string(),
            is_error,
        };
        manager
            .store_result(
                "mcp__fx__big",
                &serde_json::json!({ "chars": 3 }),
                session,
                &result,
            )
            .expect("结果可缓存")
    }

    fn page_call<'a>(
        arguments: &'a Value,
        offset: usize,
        limit: usize,
        result_id: Option<&'a str>,
        legacy_resume: bool,
        session: Option<&'a str>,
    ) -> PageCall<'a> {
        PageCall {
            qualified: "mcp__fx__big",
            arguments,
            offset,
            limit,
            result_id,
            legacy_resume,
            session,
        }
    }

    /// 续读靠令牌:错误状态与身份都跟着结果走,翻页不再把 `is_error` 弄丢。
    #[tokio::test]
    async fn resume_by_token_keeps_error_flag() {
        let manager = McpManager::new(HashSet::new());
        let id = store(&manager, Some("s1"), "boom!", true);
        let arguments = serde_json::json!({ "chars": 3 });
        let paged = manager
            .call_paged(&page_call(&arguments, 0, 2, Some(&id), false, Some("s1")))
            .await
            .expect("令牌有效");
        assert_eq!(paged.text, "bo");
        assert!(paged.from_cache);
        assert!(paged.is_error, "翻页不能把业务失败状态抹掉");
        assert_eq!(paged.result_id.as_deref(), Some(id.as_str()));
        assert_eq!(paged.total_chars, 5);
    }

    /// 令牌只在产生它的会话里有效。
    #[tokio::test]
    async fn token_is_scoped_to_its_session() {
        let manager = McpManager::new(HashSet::new());
        let id = store(&manager, Some("s1"), "abcd", false);
        let arguments = serde_json::json!({ "chars": 3 });
        let foreign = manager
            .call_paged(&page_call(&arguments, 0, 2, Some(&id), false, Some("s2")))
            .await
            .expect_err("跨会话续读必须失败");
        assert_eq!(foreign, PagedError::ForeignSession { result_id: id.clone() });
        // 无会话上下文也算「另一个会话」,不把结果泄给不持有身份的调用方。
        let anonymous = manager
            .call_paged(&page_call(&arguments, 0, 2, Some(&id), false, None))
            .await
            .expect_err("无会话上下文同样不得续读");
        assert_eq!(anonymous, PagedError::ForeignSession { result_id: id });
    }

    /// 不存在的令牌报错;此处根本没有可调用的服务器,所以「尝试执行」
    /// 只会得到 `Call` 错误——拿到 `UnknownResult` 就证明没有重跑。
    #[tokio::test]
    async fn unknown_token_is_refused_without_rerunning() {
        let manager = McpManager::new(HashSet::new());
        let arguments = serde_json::json!({ "chars": 3 });
        let error = manager
            .call_paged(&page_call(&arguments, 0, 10, Some("res-0-0"), false, Some("s1")))
            .await
            .expect_err("未知令牌必须失败");
        assert_eq!(
            error,
            PagedError::UnknownResult {
                result_id: "res-0-0".to_string()
            }
        );
    }

    /// 另一种工具的令牌不能拿来翻这个工具的页。
    #[tokio::test]
    async fn token_from_another_tool_is_refused() {
        let manager = McpManager::new(HashSet::new());
        let id = store(&manager, Some("s1"), "abcd", false);
        let arguments = serde_json::json!({ "chars": 3 });
        let error = manager
            .call_paged(&PageCall {
                qualified: "mcp__fx__echo",
                arguments: &arguments,
                offset: 0,
                limit: 10,
                result_id: Some(&id),
                legacy_resume: false,
                session: Some("s1"),
            })
            .await
            .expect_err("令牌绑工具");
        assert_eq!(error, PagedError::UnknownResult { result_id: id });
    }

    /// 旧式 `offset` 续读:能找到原结果时复用最近一条,并补上它的身份。
    #[tokio::test]
    async fn legacy_resume_uses_the_latest_matching_result() {
        let manager = McpManager::new(HashSet::new());
        let first = store(&manager, Some("s1"), "旧结果", false);
        let second = store(&manager, Some("s1"), "新结果", true);
        let arguments = serde_json::json!({ "chars": 3 });
        let paged = manager
            .call_paged(&page_call(&arguments, 1, 3, None, true, Some("s1")))
            .await
            .expect("有可续读的结果");
        assert!(paged.from_cache);
        assert_eq!(paged.text, "结果");
        assert!(paged.is_error);
        assert_eq!(paged.result_id.as_deref(), Some(second.as_str()));
        assert_ne!(paged.result_id.as_deref(), Some(first.as_str()));
    }

    /// 旧式续读的参数匹配必须与对象键顺序无关,但必须与数组顺序有关。
    ///
    /// 参数完全相同的两次调用,键顺序可能因为生成顺序不同而不一样:
    /// 排序后再序列化才能让合法的翻页仍匹配到同一条结果;而数组顺序是
    /// 参数语义的一部分,规范化不得把它抹掉。
    #[test]
    fn arguments_key_ignores_object_key_order_but_keeps_array_order() {
        let left: Value = serde_json::json!({ "path": "a.txt", "limit": 10 });
        let right: Value = serde_json::json!({ "limit": 10, "path": "a.txt" });
        assert_eq!(arguments_key(&left), arguments_key(&right));

        let nested_left: Value = serde_json::json!({ "outer": { "x": 1, "y": 2 } });
        let nested_right: Value = serde_json::json!({ "outer": { "y": 2, "x": 1 } });
        assert_eq!(arguments_key(&nested_left), arguments_key(&nested_right));

        let list_left: Value = serde_json::json!({ "tags": ["a", "b"] });
        let list_right: Value = serde_json::json!({ "tags": ["b", "a"] });
        assert_ne!(arguments_key(&list_left), arguments_key(&list_right));

        assert_ne!(
            arguments_key(&left),
            arguments_key(&serde_json::json!({ "path": "b.txt", "limit": 10 }))
        );
    }

    /// 旧式续读找不到原结果时报错——此仓库里没有可用服务器,一旦它选择
    /// 「重跑」就会得到 `Call` 而不是 `LostResult`。
    #[tokio::test]
    async fn legacy_resume_without_a_result_is_refused_without_rerunning() {
        let manager = McpManager::new(HashSet::new());
        let arguments = serde_json::json!({ "chars": 3 });
        let error = manager
            .call_paged(&page_call(&arguments, 2, 10, None, true, Some("s1")))
            .await
            .expect_err("没有原结果必须失败");
        assert_eq!(error, PagedError::LostResult);
    }

    /// 缓存满了淘汰最久未读的一条,而不是整表清空:最新结果仍可续读。
    #[tokio::test]
    async fn cache_eviction_drops_only_the_oldest() {
        let manager = McpManager::new(HashSet::new());
        let oldest = store(&manager, Some("s1"), "0", false);
        let mut newest = oldest.clone();
        for _ in 0..MAX_CACHE_ENTRIES {
            newest = store(&manager, Some("s1"), "1", false);
        }
        let arguments = serde_json::json!({ "chars": 3 });
        let error = manager
            .call_paged(&page_call(&arguments, 0, 10, Some(&oldest), false, Some("s1")))
            .await
            .expect_err("最旧的一条已被淘汰");
        assert_eq!(
            error,
            PagedError::UnknownResult {
                result_id: oldest.clone()
            }
        );
        let paged = manager
            .call_paged(&page_call(&arguments, 0, 1, Some(&newest), false, Some("s1")))
            .await
            .expect("最新结果仍在缓存里");
        assert_eq!(paged.text, "1");
    }

    #[test]
    fn server_status_wire_strings() {
        assert_eq!(McpServerStatus::Connected.as_str(), "connected");
        assert_eq!(McpServerStatus::Disabled.as_str(), "disabled");
        assert_eq!(McpServerStatus::Error.as_str(), "error");
    }
}
