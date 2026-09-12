//! System-prompt assembly for model requests (mirrors `@deepseek-ai/dsh-system-prompt`).
//!
//! Plugins contribute ordered [`PromptSection`]s, dynamic [`PromptContext`]s,
//! tool-schema providers, and named variables. The loop calls [`SystemPrompt::assemble`]
//! once per step, renders sections into the system string, and materializes
//! runtime context as injected user messages.

mod render;

use std::collections::{BTreeMap, HashMap, HashSet};

use denia_core::tool::ToolSchema;

pub use render::{
    frame_system_prompt_for_model, is_runtime_context_snapshot, join_context_sections,
    render_context_sections, render_context_snapshot, render_prompt, render_prompt_for_user,
};

/// Deployment persona section name; shadowing replaces the global persona.
pub const PERSONA_SECTION: &str = "deployment:persona";

/// Reserved rest marker for explicit [`SystemPromptConfig::tool_order`].
pub const TOOL_ORDER_REST: &str = "<unlisted-tools>";

/// Prefix for a materialized runtime-context snapshot (DSH-compatible prose).
pub const RUNTIME_CONTEXT_HEADER: &str =
    "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.";

/// Cleared-runtime marker when no context contributions remain.
pub const RUNTIME_CONTEXT_CLEARED: &str =
    "Current runtime context: none. Earlier runtime-context snapshots no longer apply.";

/// Centrally allocated section positions (subset for the shipped tool set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionOrder {
    HarnessIdentity,
    DeploymentPersona,
    ToolBash,
    /// 列目录工具(ls)的纪律段;紧跟 bash 收口段,是"看目录里有什么"的专用入口。
    ToolLs,
    ToolRead,
    /// 用户消息中 `@路径` 文件引用说明(对照 dsh FILE_REFERENCE 段),紧跟 read 纪律。
    FileReference,
    ToolWrite,
    ToolGlob,
    ToolGrep,
    ToolEdit,
    /// 宿主能力工具(子代理委派)的工具纪律段;仅在注册了对应工具时注入。
    ToolAgents,
    /// 后台任务工具(job 三件套)的工具纪律段。
    ToolJobs,
    /// 技能工具(skill)的工具纪律段。
    ToolSkill,
    /// 浏览器工具的资源回收纪律段;仅在注册了 browser 工具时注入。
    ToolBrowser,
    /// 提问工具(ask)的使用纪律段;仅在注册了 ask 工具时注入。
    ToolAsk,
    /// 会话目标工具(get_goal/update_goal)的纪律段;随 default_registry
    /// 注册,仅存在目标的会话在上下文注入里携带目标详情。
    ToolGoal,
    /// 计划呈交工具(exit_plan)的纪律段;仅计划模式注入。
    ToolPlan,
    /// MCP 外部工具(`mcp__<server>__<tool>`)的纪律段;仅在至少有一个
    /// MCP 服务器已连接时注入。
    ToolMcp,
    /// 项目记忆(记忆目录写沉淀 + MEMORY.md 索引维护)的纪律段;仅在
    /// 项目记忆启用(runtime.memoryEnabled)时注入。
    ToolMemory,
}

impl SectionOrder {
    /// Numeric sort key for one repository-owned section placement.
    pub fn value(self) -> i32 {
        match self {
            Self::HarnessIdentity => -1000,
            Self::DeploymentPersona => 0,
            Self::ToolBash => 1000,
            Self::ToolLs => 1050,
            Self::ToolRead => 1100,
            Self::FileReference => 1150,
            Self::ToolWrite => 1200,
            Self::ToolGlob => 1300,
            Self::ToolGrep => 1400,
            Self::ToolEdit => 1500,
            Self::ToolAgents => 1600,
            Self::ToolJobs => 1700,
            Self::ToolSkill => 1800,
            Self::ToolBrowser => 1900,
            Self::ToolAsk => 2000,
            Self::ToolGoal => 1950,
            Self::ToolPlan => 2050,
            Self::ToolMcp => 2100,
            Self::ToolMemory => 2150,
        }
    }
}

/// Per-step inputs resolved while assembling a prompt.
#[derive(Debug, Clone, Default)]
pub struct AssembleContext {
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    /// 当前会话权限模式(read-only / auto-edit / plan / full)。
    pub permission_mode: Option<String>,
}

/// Audience for a system-prompt section.
///
/// `Model` —— 只发给模型(用户不应看到,如工具纪律/工具使用说明)。
/// `User`  —— 既发给模型,也展示给用户(身份块/部署 persona 等)。
/// `Context` —— 运行时上下文块,落日志但不在对话流渲染(用户只看到 metadata)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SectionAudience {
    Model,
    User,
    Context,
}

impl Default for SectionAudience {
    fn default() -> Self {
        Self::Model
    }
}

/// One contributed system-prompt section.
#[derive(Clone)]
pub struct PromptSection {
    pub name: String,
    pub order: i32,
    pub text: PromptText,
    pub complete: bool,
    /// Default [`SectionAudience::Model`]:这条 section 是给模型看的私货。
    /// 身份/部署 persona 类应显式标 `User`,UI 才会展示。
    pub audience: SectionAudience,
}

/// Static or per-assembly section/context text.
#[derive(Clone)]
pub enum PromptText {
    Static(String),
    Dynamic(std::sync::Arc<dyn Fn(&AssembleContext) -> String + Send + Sync>),
}

impl PromptText {
    fn resolve(&self, context: &AssembleContext) -> String {
        match self {
            Self::Static(text) => text.clone(),
            Self::Dynamic(provider) => provider(context),
        }
    }
}

/// One dynamic runtime-context contribution.
#[derive(Clone)]
pub struct PromptContext {
    pub name: String,
    pub order: i32,
    pub text: PromptText,
}

/// One resolved section before variable interpolation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledSection {
    pub name: String,
    pub text: String,
    /// 该段受众,UI 渲染时只显示 `User` 类。
    pub audience: SectionAudience,
}

/// One resolved runtime-context contribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssembledContext {
    pub name: String,
    pub text: String,
}

/// Tool schemas visible in one assembly plus the pre-restriction name universe.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolProviderResult {
    pub schemas: Vec<ToolSchema>,
    pub known_names: Option<Vec<String>>,
}

/// Merge-extensible assembled model input.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptAssembly {
    pub sections: Vec<AssembledSection>,
    pub contexts: Vec<AssembledContext>,
    pub tools: Vec<ToolSchema>,
    pub variables: BTreeMap<String, String>,
}

/// Plugin config for the shipped system-prompt registry.
#[derive(Debug, Clone)]
pub struct SystemPromptConfig {
    pub include_harness_identity: bool,
    pub include_runtime_context: bool,
    pub persona: String,
    pub tool_order: Option<Vec<String>>,
}

impl Default for SystemPromptConfig {
    fn default() -> Self {
        Self {
            // 身份句并入 deployment:persona,由用户整段替换,不再单独注入 harness:identity。
            include_harness_identity: false,
            include_runtime_context: true,
            persona: default_persona_template().to_string(),
            tool_order: None,
        }
    }
}

/// Registry for prompt inputs assembled before each model step.
#[derive(Clone)]
pub struct SystemPrompt {
    config: SystemPromptConfig,
    sections: HashMap<String, PromptSection>,
    contexts: HashMap<String, PromptContext>,
    tool_providers:
        Vec<std::sync::Arc<dyn Fn(&AssembleContext) -> ToolProviderResult + Send + Sync>>,
    variables:
        HashMap<String, std::sync::Arc<dyn Fn(&AssembleContext) -> Option<String> + Send + Sync>>,
    runtime_context_suppressed: bool,
}

impl SystemPrompt {
    /// Create an empty registry with validated config defaults.
    pub fn new(config: SystemPromptConfig) -> Self {
        let tool_order = config
            .tool_order
            .clone()
            .map(|order| validate_tool_order(&order));
        let mut prompt = Self {
            config: SystemPromptConfig {
                tool_order,
                ..config
            },
            sections: HashMap::new(),
            contexts: HashMap::new(),
            tool_providers: Vec::new(),
            variables: HashMap::new(),
            runtime_context_suppressed: false,
        };
        if prompt.config.include_harness_identity {
            let _ = prompt.section(PromptSection {
                name: "harness:identity".to_string(),
                order: SectionOrder::HarnessIdentity.value(),
                text: PromptText::Static("你是由 denia 驱动的 AI 编码 agent。".to_string()),
                complete: false,
                // 身份块对用户可见(展示"你被告知的身份")。
                audience: SectionAudience::User,
            });
        }
        let _ = prompt.section(PromptSection {
            name: PERSONA_SECTION.to_string(),
            order: SectionOrder::DeploymentPersona.value(),
            text: PromptText::Static(prompt.config.persona.clone()),
            complete: false,
            // 部署 persona 同样展示给用户;它是 agent 当前行事身份。
            audience: SectionAudience::User,
        });
        if !prompt.config.include_runtime_context {
            prompt.suppress_runtime_context();
        }
        prompt
    }

    /// 用自定义文本整体替换 `deployment:persona` 段(发给模型的 User 段之一)。
    pub fn with_persona_text(mut self, text: String) -> Self {
        self.config.persona = text.clone();
        if let Some(section) = self.sections.get_mut(PERSONA_SECTION) {
            section.text = PromptText::Static(text);
        }
        self
    }

    /// 出厂配置 + 自定义 persona 文本,再注册其余 shipped 段由调用方完成。
    pub fn new_with_persona(config: SystemPromptConfig, persona_text: String) -> Self {
        let mut config = config;
        config.persona = persona_text.clone();
        Self::new(config).with_persona_text(persona_text)
    }

    /// Register an ordered prompt section. Duplicate names within one registry fail.
    pub fn section(&mut self, section: PromptSection) -> Result<(), String> {
        if self.sections.contains_key(&section.name) {
            return Err(format!(
                "prompt section \"{}\" is already registered",
                section.name
            ));
        }
        self.sections.insert(section.name.clone(), section);
        Ok(())
    }

    /// Register ordered dynamic runtime context.
    pub fn context(&mut self, context: PromptContext) -> Result<(), String> {
        if self.contexts.contains_key(&context.name) {
            return Err(format!(
                "prompt context \"{}\" is already registered",
                context.name
            ));
        }
        self.contexts.insert(context.name.clone(), context);
        Ok(())
    }

    /// Suppress every dynamic runtime-context contribution.
    pub fn suppress_runtime_context(&mut self) {
        self.runtime_context_suppressed = true;
    }

    /// Register a tool-schema provider evaluated on each assembly.
    pub fn tools(
        &mut self,
        provider: impl Fn(&AssembleContext) -> ToolProviderResult + Send + Sync + 'static,
    ) {
        self.tool_providers.push(std::sync::Arc::new(provider));
    }

    /// Register a prompt variable referenced as `{{name}}` during render.
    pub fn variable(
        &mut self,
        name: &str,
        provider: impl Fn(&AssembleContext) -> Option<String> + Send + Sync + 'static,
    ) -> Result<(), String> {
        if !is_valid_variable_name(name) {
            return Err(format!(
                "invalid prompt variable name \"{name}\" (must match [a-z][a-z0-9_]*)"
            ));
        }
        if self.variables.contains_key(name) {
            return Err(format!("prompt variable \"{name}\" is already registered"));
        }
        self.variables
            .insert(name.to_string(), std::sync::Arc::new(provider));
        Ok(())
    }

    /// Assemble registered providers into one model-facing snapshot.
    pub fn assemble(&self, context: &AssembleContext) -> Result<PromptAssembly, String> {
        let mut variables = BTreeMap::new();
        for (name, provider) in &self.variables {
            let value = provider(context);
            if let Some(value) = value {
                variables.insert(name.clone(), value);
            }
        }

        let mut section_definitions: Vec<&PromptSection> = self.sections.values().collect();
        section_definitions.sort_by(|left, right| {
            left.order
                .cmp(&right.order)
                .then_with(|| left.name.cmp(&right.name))
        });

        let complete_sections: Vec<&PromptSection> = section_definitions
            .iter()
            .copied()
            .filter(|section| section.complete)
            .collect();
        if complete_sections.len() > 1 {
            return Err(format!(
                "multiple complete prompt sections are active: {}",
                complete_sections
                    .iter()
                    .map(|section| format!("\"{}\"", section.name))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let complete_section = complete_sections.first().map(|section| AssembledSection {
            name: section.name.clone(),
            text: section.text.resolve(context),
            audience: section.audience,
        });

        let sections = section_definitions
            .into_iter()
            .map(|section| AssembledSection {
                name: section.name.clone(),
                text: section.text.resolve(context),
                audience: section.audience,
            })
            .collect::<Vec<_>>();

        let contexts: Vec<AssembledContext> = if self.runtime_context_suppressed {
            Vec::new()
        } else {
            let mut entries: Vec<&PromptContext> = self.contexts.values().collect();
            entries.sort_by_key(|entry| entry.order);
            entries
                .into_iter()
                .map(|entry| AssembledContext {
                    name: entry.name.clone(),
                    text: entry.text.resolve(context),
                })
                .collect()
        };

        let mut collected = Vec::new();
        let mut known_names = HashSet::new();
        for provider in &self.tool_providers {
            let result = provider(context);
            let schemas = result.schemas;
            let accepted = result
                .known_names
                .unwrap_or_else(|| schemas.iter().map(|tool| tool.name.clone()).collect());
            collected.extend(schemas);
            known_names.extend(accepted);
        }

        let tools = order_tools(collected, self.config.tool_order.as_deref(), &known_names)?;

        let mut assembly = PromptAssembly {
            sections,
            contexts,
            tools,
            variables,
        };

        if let Some(complete) = complete_section {
            assembly.sections = vec![complete];
        }
        if self.runtime_context_suppressed {
            assembly.contexts.clear();
        }
        Ok(assembly)
    }
}

/// 出厂 persona 模板;`SYSTEM.md` 为空时沿用。含身份句,与自定义替换目标一致。
///
/// 这份模板是 prompt 里**第一个**被读到的段落(order 0),所以它说的每一句
/// 都比后面的工具纪律段更有分量——它一旦点名 bash,后面 `tool:bash` 段的收口
/// 就会被压过去。规矩句里只用专用工具名(ls/glob/grep/read_file)。
pub fn default_persona_template() -> &'static str {
    "你是由 denia 驱动的 AI 编码 agent。\n\n\
     你是运行在 denia 里的编码 agent。工作目录是 {{cwd}}（相对路径以它为根）。\
     规矩：不要猜文件路径；读取失败时先用 ls 列目录再重试；按模式找文件用 glob，搜文件内容用 grep，读文本用 read_file。\
     每步聚焦一件事；能回答时就停止调用工具。\
     始终使用简体中文回复，除非用户明确要求其他语言。"
}

fn is_valid_variable_name(name: &str) -> bool {
    let Some(first) = name.chars().next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && name
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
}

fn validate_tool_order(tool_order: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    for name in tool_order {
        if !seen.insert(name.clone()) {
            panic!("toolOrder lists \"{name}\" more than once");
        }
    }
    if !seen.contains(TOOL_ORDER_REST) {
        panic!("toolOrder must contain the \"{TOOL_ORDER_REST}\" rest entry");
    }
    tool_order.to_vec()
}

fn order_tools(
    tools: Vec<ToolSchema>,
    tool_order: Option<&[String]>,
    known_names: &HashSet<String>,
) -> Result<Vec<ToolSchema>, String> {
    if tools.iter().any(|tool| tool.name == TOOL_ORDER_REST) {
        return Err(format!(
            "tool provider returned reserved tool name \"{TOOL_ORDER_REST}\""
        ));
    }
    let Some(tool_order) = tool_order else {
        let mut ordered = tools;
        ordered.sort_by(|left, right| left.name.cmp(&right.name));
        return Ok(ordered);
    };
    let unknown: Vec<&String> = tool_order
        .iter()
        .filter(|name| **name != TOOL_ORDER_REST && !known_names.contains(*name))
        .collect();
    if !unknown.is_empty() {
        let mut known: Vec<&String> = known_names.iter().collect();
        known.sort();
        return Err(format!(
            "toolOrder lists unregistered tools {}; known tools: {}",
            unknown
                .iter()
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(", "),
            if known.is_empty() {
                "(none)".to_string()
            } else {
                known
                    .iter()
                    .map(|name| (*name).as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
    }
    let listed: HashSet<&str> = tool_order.iter().map(String::as_str).collect();
    let mut by_name: HashMap<String, ToolSchema> = tools
        .into_iter()
        .map(|tool| (tool.name.clone(), tool))
        .collect();
    let mut rest: Vec<ToolSchema> = by_name
        .values()
        .filter(|tool| !listed.contains(tool.name.as_str()))
        .cloned()
        .collect();
    rest.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(tool_order
        .iter()
        .flat_map(|name| {
            if name == TOOL_ORDER_REST {
                rest.clone()
            } else {
                by_name.remove(name).into_iter().collect::<Vec<_>>()
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assembles_persona_and_tools() {
        let mut prompt = SystemPrompt::new(SystemPromptConfig::default());
        prompt
            .variable("cwd", |_| Some("/tmp/ws".to_string()))
            .unwrap();
        prompt.tools(|_| ToolProviderResult {
            schemas: vec![ToolSchema {
                name: "echo".to_string(),
                description: "echo".to_string(),
                parameters: serde_json::json!({}),
            }],
            known_names: None,
        });
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(
            assembly
                .sections
                .first()
                .map(|section| section.name.as_str()),
            Some(PERSONA_SECTION)
        );
        assert!(
            !assembly
                .sections
                .iter()
                .any(|section| section.name == "harness:identity")
        );
        assert!(render_prompt(&assembly).contains("denia 驱动的"));
        assert!(render_prompt(&assembly).contains("/tmp/ws"));
        assert_eq!(assembly.tools.len(), 1);
    }

    #[test]
    fn with_persona_text_replaces_deployment_persona() {
        let custom = "你是自定义 agent,工作目录 {{cwd}}。".to_string();
        let mut prompt =
            SystemPrompt::new_with_persona(SystemPromptConfig::default(), custom.clone());
        prompt
            .variable("cwd", |_| Some("/tmp/custom".to_string()))
            .unwrap();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/custom".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !assembly
                .sections
                .iter()
                .any(|section| section.name == "harness:identity")
        );
        let persona = assembly
            .sections
            .iter()
            .find(|section| section.name == PERSONA_SECTION)
            .expect("persona section");
        assert!(persona.text.contains("{{cwd}}"));
        assert!(!persona.text.contains("denia 里的编码 agent"));
        let rendered = render_prompt(&assembly);
        assert!(rendered.contains("/tmp/custom"));
        assert!(rendered.contains("自定义 agent"));
    }

    #[test]
    fn runtime_context_joins_like_dsh() {
        let mut prompt = SystemPrompt::new(SystemPromptConfig::default());
        prompt
            .context(PromptContext {
                name: "policy".to_string(),
                order: 10,
                text: PromptText::Static("Mode: read-only.".to_string()),
            })
            .unwrap();
        let assembly = prompt.assemble(&AssembleContext::default()).unwrap();
        assert_eq!(
            render_context_snapshot(&assembly),
            format!("{RUNTIME_CONTEXT_HEADER}\n\nMode: read-only.")
        );
    }

    /// 出厂 persona 是 prompt 里第一个被读到的段落(order 0),比后面的工具纪律段
    /// 更有分量。它一旦点名 bash 干探索类动作,"列目录/找文件/搜内容走专用工具"
    /// 的收口就会被它压过去——这里锁死规矩句只用专用工具名。
    #[test]
    fn default_persona_never_steers_exploration_to_bash() {
        let persona = default_persona_template();
        for banned in ["bash 列目录", "用 bash", "先用 bash", "bash 找", "bash 搜"] {
            assert!(
                !persona.contains(banned),
                "出厂 persona 把探索动作推给了 bash({banned}):{persona}"
            );
        }
        for needle in ["先用 ls 列目录", "glob", "grep", "read_file"] {
            assert!(
                persona.contains(needle),
                "出厂 persona 缺少专用工具出口 {needle}:{persona}"
            );
        }
    }
}
