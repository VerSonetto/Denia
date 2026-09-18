//! 自定义系统提示词:`<home>/SYSTEM.md` 持久化 + 热加载。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use denia_system_prompt::{SystemPrompt, ToolProviderResult};
use denia_tools::Tool as _;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::broadcast;

use crate::state::ServerEvent;

const FILE_NAME: &str = "SYSTEM.md";

/// 全局系统提示词状态:读路径无锁,写/文件变更原子替换。
pub struct SystemPromptState {
    current: Arc<ArcSwap<SystemPrompt>>,
    home: PathBuf,
    browser_hub: Option<denia_tools::BrowserHub>,
    /// 组装创作工具(create_preset)的 schema:server 部署注册该工具,
    /// schema 与 tool:preset 纪律段在这里同步挂进系统提示词(模型可见);
    /// None(如测试)则两者都不注入。
    preset_tool_schema: Option<denia_core::tool::ToolSchema>,
    /// 本实例 HTTP API 地址:注入 runtime-context 快照,让模型改配置时
    /// 永远打自己所在的进程(多实例共享数据目录时猜端口会写错实例)。
    api_base: Option<String>,
    /// MCP 管理器句柄:提示词的 MCP 段与 schema provider 按它重建
    /// (单一来源,避免配置热更新在旧实例上叠加 provider)。
    mcp_manager: arc_swap::ArcSwap<Option<Arc<denia_mcp::McpManager>>>,
}

impl SystemPromptState {
    /// 启动时从磁盘加载;空或缺失则沿用出厂 persona。
    /// `api_base` 是本实例 HTTP API 地址(如 `http://127.0.0.1:3601`),
    /// 注入 runtime-context 快照;None = 无 API 部署(测试),不注入。
    pub fn load(
        home: &Path,
        browser_hub: Option<denia_tools::BrowserHub>,
        preset_tool_schema: Option<denia_core::tool::ToolSchema>,
        api_base: Option<String>,
    ) -> Self {
        let prompt = build_prompt(
            read_file_text(home),
            browser_hub.clone(),
            preset_tool_schema.clone(),
            None,
            api_base.clone(),
        );
        Self {
            current: Arc::new(ArcSwap::from_pointee(prompt)),
            home: home.to_path_buf(),
            browser_hub,
            preset_tool_schema,
            api_base,
            mcp_manager: arc_swap::ArcSwap::from_pointee(None),
        }
    }

    pub fn handle(&self) -> Arc<ArcSwap<SystemPrompt>> {
        self.current.clone()
    }

    pub fn file_path(&self) -> PathBuf {
        self.home.join(FILE_NAME)
    }

    /// 注入 MCP 管理器并重建提示词:server 启动接线时调用一次,之后
    /// SYSTEM.md 热更新走同一来源自动带上 MCP 段与 provider。
    pub fn attach_mcp(&self, manager: Arc<denia_mcp::McpManager>) {
        self.mcp_manager.store(Arc::new(Some(manager)));
        self.reload();
    }

    /// 当前文件全文与来源标记。
    pub fn text_and_source(&self) -> (String, &'static str) {
        match std::fs::read_to_string(self.file_path()) {
            Ok(text) if !text.trim().is_empty() => (text, "file"),
            _ => (String::new(), "default"),
        }
    }

    /// 写入 `SYSTEM.md` 并热替换内存中的 `SystemPrompt`。
    pub fn write(&self, text: &str) -> std::io::Result<()> {
        let path = self.file_path();
        if text.trim().is_empty() {
            let _ = std::fs::remove_file(&path);
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, text)?;
        }
        self.reload();
        Ok(())
    }

    /// 删除自定义文件并恢复出厂 persona。
    pub fn reset(&self) -> std::io::Result<()> {
        let _ = std::fs::remove_file(self.file_path());
        self.reload();
        Ok(())
    }

    pub fn reload(&self) {
        let prompt = build_prompt(
            read_file_text(&self.home),
            self.browser_hub.clone(),
            self.preset_tool_schema.clone(),
            self.mcp_manager.load_full().as_ref().clone(),
            self.api_base.clone(),
        );
        self.current.store(Arc::new(prompt));
    }

    fn is_system_md(path: &Path) -> bool {
        path.file_name().is_some_and(|name| name == FILE_NAME)
    }

    /// 监听 `SYSTEM.md` 变更,debounce 后重载并广播。
    pub fn spawn_watcher(self: &Arc<Self>, events: broadcast::Sender<ServerEvent>) {
        let home = self.home.clone();
        let store = Arc::clone(self);
        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut watcher = match RecommendedWatcher::new(
                move |result| {
                    let _ = tx.send(result);
                },
                notify::Config::default(),
            ) {
                Ok(watcher) => watcher,
                Err(error) => {
                    tracing::warn!(%error, "system prompt watcher failed to start");
                    return;
                }
            };
            if let Err(error) = watcher.watch(&home, RecursiveMode::NonRecursive) {
                tracing::warn!(%error, "system prompt watcher could not watch home");
                return;
            }

            let mut last_reload = std::time::Instant::now()
                .checked_sub(Duration::from_secs(1))
                .unwrap_or_else(std::time::Instant::now);
            while let Ok(result) = rx.recv() {
                let Ok(event) = result else {
                    continue;
                };
                let relevant = matches!(
                    event.kind,
                    EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                ) && event.paths.iter().any(|path| Self::is_system_md(path));
                if !relevant {
                    continue;
                }
                if last_reload.elapsed() < Duration::from_millis(200) {
                    continue;
                }
                last_reload = std::time::Instant::now();
                store.reload();
                let _ = events.send(ServerEvent::SystemPromptChanged);
            }
        });
    }
}

fn read_file_text(home: &Path) -> Option<String> {
    let text = std::fs::read_to_string(home.join(FILE_NAME)).ok()?;
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_system_prompt::{AssembleContext, SectionAudience};

    #[test]
    fn preset_authoring_section_and_schema_travel_together() {
        // server 部署传 schema:纪律段 + tools provider 同批注入,模型可见
        // 即模型可执行。
        let schema = crate::preset_tool::schema();
        let prompt = build_prompt(None, None, Some(schema), None, None);
        let assembly = prompt
            .assemble(&AssembleContext::default())
            .expect("assembly is valid");
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "tool:preset")
            .expect("server 部署必须注入 tool:preset 纪律段");
        assert!(matches!(section.audience, SectionAudience::Model));
        assert!(
            assembly.tools.iter().any(|schema| schema.name == "create_preset"),
            "纪律段进提示词的同时,schema 必须在请求工具列表里"
        );
        // 纪律段不进用户可见副本。
        let user_copy = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(!user_copy.contains("create_preset"));

        // 无 schema(非 server 部署形态):两者都不出现。
        let bare = build_prompt(None, None, None, None, None);
        let assembly = bare.assemble(&AssembleContext::default()).unwrap();
        assert!(!assembly.sections.iter().any(|section| section.name == "tool:preset"));
        assert!(!assembly.tools.iter().any(|schema| schema.name == "create_preset"));
    }

    #[test]
    fn mcp_section_and_provider_follow_the_manager() {
        use std::collections::HashSet;

        // 带 manager:纪律段注入,文本动态(无连接时目录为"(当前没有…)"、
        // provider 无 MCP schema);装配一次 mcp_list 不重复堆积。
        let manager = std::sync::Arc::new(denia_mcp::McpManager::new(HashSet::new()));
        let prompt = build_prompt(None, None, None, Some(manager.clone()), None);
        let assembly = prompt.assemble(&AssembleContext::default()).unwrap();
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "tool:mcp")
            .expect("有 MCP 管理器的部署必须注入 tool:mcp 纪律段");
        assert!(matches!(section.audience, SectionAudience::Model));
        assert!(section.text.contains("当前没有已连接"));
        assert!(
            !assembly.tools.iter().any(|schema| schema.name == denia_tools::MCP_LIST_TOOL),
            "没有任何可用工具时,mcp_list 也不得暴露给模型"
        );
        // 纪律段不进用户可见副本。
        let user_copy = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(!user_copy.contains("mcp_list"));

        // 无 manager(非 server 部署形态):段与 provider 都不注入。
        let bare = build_prompt(None, None, None, None, None);
        let assembly = bare.assemble(&AssembleContext::default()).unwrap();
        assert!(!assembly.sections.iter().any(|section| section.name == "tool:mcp"));
        assert!(!assembly.tools.iter().any(|schema| schema.name == denia_tools::MCP_LIST_TOOL));

        // 重建幂等:同一 manager 连续 reload 不叠加 provider(schema 数量不变)。
        let store = std::sync::Arc::new(SystemPromptState::load(
            std::env::temp_dir().as_path(),
            None,
            None,
            None,
        ));
        store.attach_mcp(manager.clone());
        store.reload();
        let assembly = store
            .handle()
            .load()
            .assemble(&AssembleContext::default())
            .unwrap();
        let list_count = assembly
            .tools
            .iter()
            .filter(|schema| schema.name == denia_tools::MCP_LIST_TOOL)
            .count();
        assert_eq!(list_count, 0, "空快照下 mcp_list 不暴露,且不得有重复条目");
    }

    /// 本实例 API 地址随 runtime-context 快照注入:模型改配置必须打自己
    /// 所在的进程——数据目录共享时猜端口会把配置写进别的实例(实测缺陷:
    /// 3601 会话接 MCP 却写了 3600,面板毫无变化)。
    #[test]
    fn instance_api_base_reaches_the_runtime_snapshot() {
        let prompt = build_prompt(
            None,
            None,
            None,
            None,
            Some("http://127.0.0.1:3601".to_string()),
        );
        let assembly = prompt.assemble(&AssembleContext::default()).unwrap();
        let snapshot = denia_system_prompt::render_context_snapshot(&assembly);
        assert!(
            snapshot.contains("http://127.0.0.1:3601"),
            "runtime-context 快照必须带本实例 API:{snapshot}"
        );

        let bare = build_prompt(None, None, None, None, None);
        let assembly = bare.assemble(&AssembleContext::default()).unwrap();
        assert!(
            !denia_system_prompt::render_context_snapshot(&assembly).contains("本实例 API"),
            "无 API 部署不得注入"
        );
    }
}

fn build_prompt(
    text: Option<String>,
    browser_hub: Option<denia_tools::BrowserHub>,
    preset_tool_schema: Option<denia_core::tool::ToolSchema>,
    mcp_manager: Option<Arc<denia_mcp::McpManager>>,
    api_base: Option<String>,
) -> SystemPrompt {
    // server 部署有应答通道:系统提示词与工具注册必须同步带上 `ask`
    // (schema + tool:ask 纪律段),模型可见即模型可执行。
    let mut prompt = match (text, browser_hub) {
        (Some(persona), Some(hub)) => {
            denia_tools::shipped_with_persona_and_browser_and_ask(persona, Some(hub), true).0
        }
        (Some(persona), None) => {
            denia_tools::shipped_with_persona_and_browser_and_ask(persona, None, true).0
        }
        (None, Some(hub)) => denia_tools::default_shipped_with_browser_and_ask(Some(hub), true).0,
        (None, None) => denia_tools::default_shipped_with_browser_and_ask(None, true).0,
    };
    // 本实例 API 地址:随 runtime-context 快照注入(端口进程内恒定,快照
    // 每会话只追加一次,不扰动前缀缓存)。帮用户改配置的会话必须打自己
    // 所在的进程——数据目录共享时,猜端口会把配置写到另一个实例里,
    // 模型这边"接入成功"而用户面板毫无变化。
    if let Some(base) = api_base {
        prompt
            .context(denia_system_prompt::PromptContext {
                name: "harness:instance".to_string(),
                order: 12,
                text: denia_system_prompt::PromptText::Static(format!(
                    "本实例 API:{base}(本机 HTTP)。任何读取或修改 denia 自身配置的操作(提供商、密钥、MCP、preset、系统提示词)一律调用这个地址;多实例并存时不要猜默认端口,它可能不是当前实例。"
                )),
            })
            .expect("instance context slot is free");
    }
    // server 部署总是注册宿主能力工具(委派/后台任务/技能),其纪律段
    // 归位系统提示词(原先是 [denia 能力上下文] 注入消息里的静态内容)。
    denia_tools::register_capability_prompt_sections(&mut prompt)
        .expect("capability prompt sections are valid");
    // 项目记忆纪律段同批注册;运行中的进退(记忆关闭不注入)由
    // agent-loop 的 assemble 阶段按 runtime.memory_root_for 过滤。
    denia_tools::register_memory_prompt_section(&mut prompt)
        .expect("memory prompt section is valid");
    // 组装创作工具纪律段与 schema:与 state.rs 注册的 create_preset 可执行
    // 实例严格同步(server 部署两者都有);创造模式 preset 的 persona 承担
    // 角色与轮次,schema 经 tools provider 进请求(与 ask/browser 同路)。
    if let Some(schema) = preset_tool_schema {
        denia_tools::register_preset_prompt_section(&mut prompt)
            .expect("preset prompt section is valid");
        prompt.tools(move |_| ToolProviderResult {
            schemas: vec![schema.clone()],
            known_names: None,
        });
    }
    // MCP:部署有管理器才有纪律段与 schema provider——纪律段文本是动态的
    // (服务器目录按快照渲染),provider 返回 mcp_list + 快照里的全量
    // `mcp__*` schema(agent-loop 的装配阶段再把未装载条目移出请求面)。
    // 这里是 MCP 提示词面的唯一构建来源:热更新/配置变更都整体重建,
    // 不存在 provider 叠加残留或段摘不掉的问题。
    if let Some(manager) = mcp_manager {
        denia_tools::register_mcp_prompt_section_with_manager(&mut prompt, manager.clone())
            .expect("mcp prompt section is valid");
        prompt.tools(move |_| {
            let snapshot = manager.snapshot();
            let defs = snapshot.tool_defs();
            let mut schemas = Vec::with_capacity(defs.len() + 1);
            if !defs.is_empty() {
                // 目录工具:与 mcp__* schema 同批进退,没有可用工具时
                // 模型也不会看到一个查不到东西的 mcp_list。
                schemas.push(denia_tools::mcp_list_schema());
            }
            schemas.extend(defs.into_iter().map(|(name, description, parameters)| {
                denia_tools::McpTool::new(&name, description, parameters, manager.clone())
                    .schema()
                    .clone()
            }));
            ToolProviderResult {
                schemas,
                known_names: None,
            }
        });
    }
    prompt
}
