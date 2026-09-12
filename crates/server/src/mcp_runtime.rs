//! MCP 运行时状态:配置读写 → 重连 → 同步工具注册表与系统提示词。
//!
//! 一次配置变更要同时对齐三处,否则模型会看到"存在但调不动"的工具:
//! 1. `McpManager`:子进程连接与工具路由(执行面);
//! 2. `SessionDriver` 的工具注册表:`mcp__*` 工具可被派发(派发面);
//! 3. `SystemPrompt` 的 tool provider:schema 与纪律段(模型可见面)。
//!
//! 三者都在 [`McpRuntime::apply`] 里按同一份配置重建,顺序固定:先重连
//! (拿到真实工具清单),再替换注册表,最后刷新提示词。

use std::collections::HashSet;
use std::sync::Arc;

use denia_agent_loop::SessionDriver;
use denia_mcp::McpManager;
use denia_settings::SettingsStore;
use denia_system_prompt::SystemPrompt;
use denia_tools::Tool;

use crate::mcp_settings::{MCP_NS, McpSettings};

/// MCP 运行时:持有管理器与重建所需的依赖句柄。
pub struct McpRuntime {
    manager: Arc<McpManager>,
    settings: Arc<SettingsStore>,
    driver: Arc<SessionDriver>,
    /// 提示词句柄(系统提示词热替换);MCP 段与 schema 都挂在它上面。
    prompt: Arc<arc_swap::ArcSwap<SystemPrompt>>,
}

impl McpRuntime {
    pub fn new(
        manager: Arc<McpManager>,
        settings: Arc<SettingsStore>,
        driver: Arc<SessionDriver>,
        prompt: Arc<arc_swap::ArcSwap<SystemPrompt>>,
    ) -> Self {
        Self {
            manager,
            settings,
            driver,
            prompt,
        }
    }

    pub fn manager(&self) -> Arc<McpManager> {
        self.manager.clone()
    }

    /// 当前配置(从 settings 解析)。
    pub fn config(&self) -> McpSettings {
        McpSettings::from_value(
            &self
                .settings
                .resolved(MCP_NS)
                .unwrap_or_else(|_| crate::mcp_settings::defaults()),
        )
    }

    /// 按 settings 里的配置重建连接与工具面。
    ///
    /// 失败隔离:单个服务器连不上只影响它自己(manager 内已处理);
    /// 配置非法则写入层早已拒绝,这里拿到的总是合法值。
    pub async fn apply(&self) {
        let config = self.config();
        self.manager.reload(&config.servers).await;
        self.sync_tools();
    }

    /// 只重连(不动配置):用于"重新连接"按钮。
    pub async fn reconnect(&self, id: &str) {
        // 先把该服务器从连接表里踢掉(不清配置),再按配置重建。
        self.manager.disconnect(id).await;
        self.apply().await;
    }

    /// 用当前快照重建 driver 的工具注册表 + 系统提示词工具面。
    ///
    /// 内置工具从 driver 现有注册表里保留(带着 server 侧的定制,如带
    /// runtime 的 bash),再追加当前 MCP 工具。
    fn sync_tools(&self) {
        let mut registry = self.base_registry();
        denia_tools::register_mcp_tools(&mut registry, &self.manager);
        self.driver.replace_tools(registry);
        self.sync_prompt();
    }

    /// 内置工具集:重建一份与 [`crate::state::build_state`] 初始注册等价
    /// 的注册表(server 部署总是带 browser/ask/宿主能力)。
    fn base_registry(&self) -> denia_tools::ToolRegistry {
        // 从当前注册表里剔除旧的 mcp__* 条目,其余原样保留——这样 server
        // 侧对 bash 的定制(with_runtime)等不会被重建冲掉。
        let current = self.driver.tools();
        let mut rebuilt = denia_tools::ToolRegistry::default();
        for name in current.names() {
            if name.starts_with(MCP_TOOL_PREFIX) {
                continue;
            }
            if let Some(tool) = current.get(&name) {
                rebuilt.register(tool);
            }
        }
        rebuilt
    }

    /// 刷新系统提示词:MCP 工具的 schema 与纪律段按当前快照进退。
    ///
    /// 段的进退规则(AGENTS.md 硬要求):有已连接的 MCP 服务器才注入
    /// `tool:mcp`;一个都没有时不注入,模型不会看到不存在的工具。
    fn sync_prompt(&self) {
        let manager = self.manager.clone();
        let mut prompt = (**self.prompt.load()).clone();
        // 段重名会报错(已存在即跳过);schema provider 是追加语义。
        // 有 MCP 工具才注入纪律段,一个都没有时不注入(模型看不到
        // 不存在的工具,AGENTS.md 的段与工具同步要求)。
        if !manager.snapshot().tool_defs().is_empty() {
            let _ = denia_tools::register_mcp_prompt_section(&mut prompt);
        }
        prompt.tools(move |_| {
            let snapshot = manager.snapshot();
            // 直接复用 McpTool 的 schema 构造:模型可见的 schema 与可派发
            // 的工具必须是同一份(描述与分页字段不会两处漂移)。
            let schemas = snapshot
                .tool_defs()
                .into_iter()
                .map(|(name, description, parameters)| {
                    denia_tools::McpTool::new(&name, description, parameters, manager.clone())
                        .schema()
                        .clone()
                })
                .collect();
            denia_system_prompt::ToolProviderResult {
                schemas,
                known_names: None,
            }
        });
        self.prompt.store(Arc::new(prompt));
    }
}

/// MCP 工具名前缀(与 `denia_mcp::qualify_tool_name` 的输出一致)。
pub const MCP_TOOL_PREFIX: &str = "mcp__";

/// 内置工具名集合(用于 MCP 工具的命名冲突判定)。
///
/// 冲突判定在注册表热替换时发生:内置工具的完整名单(含宿主能力工具)
/// 交给 `McpManager`,与内置同名的 MCP 工具一律跳过,绝不覆盖内置能力。
pub fn builtin_tool_names(registry: &denia_tools::ToolRegistry) -> HashSet<String> {
    registry.names().into_iter().collect()
}
