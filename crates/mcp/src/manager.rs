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
                let description = self
                    .descriptions
                    .get(&name)
                    .cloned()
                    .unwrap_or_default();
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
/// 分页缓存条目上限;超出整体清空(条目数 = 工具数,实际很小)。
const MAX_CACHE_ENTRIES: usize = 256;

/// 一次分页调用的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct PagedResult {
    /// 本页文本(已按 offset/limit 切片)。
    pub text: String,
    /// MCP 侧声明的业务失败(不是协议错误)。
    pub is_error: bool,
    /// 完整结果总字符数。
    pub total_chars: usize,
    /// 本页起始字符偏移(0-based)。
    pub offset: usize,
    /// 本页字符数。
    pub shown_chars: usize,
    /// 是否还有后续内容。
    pub has_more: bool,
    /// 本次是否复用了上一次调用的缓存(未真正执行工具)。
    pub from_cache: bool,
    /// 完整结果过长,未缓存:无法翻页。
    pub uncached: bool,
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
    /// 分页缓存:工具修饰名 → (参数指纹, 完整文本)。
    ///
    /// 翻页必须复用上次结果——MCP 工具可能有副作用(下单、发消息),
    /// 用 offset 再看一页时不该把工具跑第二遍。
    last_results: Mutex<HashMap<String, (String, String)>>,
}

impl McpManager {
    pub fn new(builtin_names: HashSet<String>) -> Self {
        Self {
            connected: RwLock::new(HashMap::new()),
            snapshot: ArcSwap::from_pointee(McpSnapshot::empty()),
            builtin_names,
            default_cwd: None,
            last_results: Mutex::new(HashMap::new()),
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
        // 服务器集合变了,旧的分页缓存不再可信(重连后工具实现可能不同)。
        self.last_results.lock().unwrap().clear();
        for config in configs {
            let stale_connection = connected
                .get(&config.id)
                .is_some_and(|entry| entry.fingerprint != fingerprint_of(config));
            if stale_connection || (!config.enabled && connected.contains_key(&config.id)) {
                if let Some(entry) = connected.remove(&config.id) {
                    entry.client.shutdown().await;
                }
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

        self.snapshot
            .store(Arc::new(build_snapshot(&states, &schema_src, &self.builtin_names)));
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
        let disabled: HashSet<&str> = config
            .disabled_tools
            .iter()
            .map(String::as_str)
            .collect();
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
    /// 分页语义(与 `read_file` 同形态,用户补充要求):
    /// - 结果超过一页时**先执行工具、缓存全文、返回第一页**,尾部给出
    ///   分页提示;模型想看后面就用更大的 `offset` 再调一次;
    /// - 翻页命中缓存(同一工具 + 同一组其它参数)时不重跑工具——MCP
    ///   工具可能有副作用,翻页不该触发第二次执行;
    /// - 结果大到不缓存(>1MB)时不提供翻页,提示模型缩小范围。
    pub async fn call_paged(
        &self,
        qualified: &str,
        offset: usize,
        limit: usize,
        other_args: &Value,
    ) -> Result<PagedResult, String> {
        let fingerprint = serde_json::to_string(other_args).unwrap_or_default();
        let cached = {
            let cache = self.last_results.lock().unwrap();
            cache
                .get(qualified)
                .filter(|(cached_key, _)| *cached_key == fingerprint)
                .map(|(_, text)| text.clone())
        };
        let (text, is_error, from_cache) = match cached {
            Some(text) => (text, false, true),
            None => {
                let result = self.call(qualified, other_args.clone()).await?;
                let text = result.text;
                if !text.is_empty() && text.chars().count() <= MAX_CACHED_CHARS {
                    let mut cache = self.last_results.lock().unwrap();
                    if cache.len() >= MAX_CACHE_ENTRIES {
                        cache.clear();
                    }
                    cache.insert(qualified.to_string(), (fingerprint, text.clone()));
                    (text, result.is_error, false)
                } else {
                    (text, result.is_error, false)
                }
            }
        };
        Ok(page_text(&text, is_error, offset, limit, from_cache))
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
        self.last_results.lock().unwrap().clear();
    }

    /// 关闭全部子进程(进程退出/配置清空时调用)。
    pub async fn shutdown_all(&self) {
        let mut connected = self.connected.write().await;
        for (_, entry) in connected.drain() {
            entry.client.shutdown().await;
        }
        self.last_results.lock().unwrap().clear();
        self.snapshot
            .store(Arc::new(McpSnapshot::empty()));
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
    }
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
        let state =
            connected_state_with(vec![tool("read file", "mcp__fs__read_file", true, None)]);
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
    }

    #[test]
    fn server_status_wire_strings() {
        assert_eq!(McpServerStatus::Connected.as_str(), "connected");
        assert_eq!(McpServerStatus::Disabled.as_str(), "disabled");
        assert_eq!(McpServerStatus::Error.as_str(), "error");
    }
}

