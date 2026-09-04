//! System-prompt contributions for the shipped tool set.

use std::sync::Arc;

use denia_core::tool::ToolSchema;
use denia_system_prompt::{
    AssembleContext, PromptContext, PromptSection, PromptText, SectionAudience, SectionOrder,
    SystemPrompt, ToolProviderResult,
};

use crate::browser::BrowserHub;
use crate::recon::ReconHub;
use crate::shell;
use crate::{Tool, ToolRegistry};

/// Register tool schemas, tool guidance sections, runtime facts, and variables.
pub fn register_shipped_prompt(
    prompt: &mut SystemPrompt,
    tools: &ToolRegistry,
) -> Result<(), String> {
    prompt.variable("cwd", |context| context.cwd.clone())?;
    prompt.variable("model", |context| context.model.clone())?;
    prompt.variable("provider", |context| context.provider.clone())?;

    prompt.context(PromptContext {
        name: "harness:runtime".to_string(),
        order: 10,
        text: PromptText::Dynamic(Arc::new(runtime_context_text)),
    })?;
    prompt.context(PromptContext {
        name: "harness:permission".to_string(),
        order: 11,
        text: PromptText::Dynamic(Arc::new(|context| {
            let mode = context.permission_mode.as_deref().unwrap_or("workspace-write");
            let approval = context.approval_policy.as_deref().unwrap_or("ask");
            match approval {
                "never" => format!(
                    "当前文件策略:{mode}。审批提示已禁用:需要审批的操作会被自动拒绝——不要请求沙箱升权(不要设置 sandbox_permissions)。"
                ),
                _ => format!(
                    "当前文件策略:{mode}。审批策略:ask。被策略拒绝的操作可以携带 sandbox_permissions 与 justification 重试一次;该重试会弹出用户审批。"
                ),
            }
        })),
    })?;

    prompt.section(PromptSection {
        name: "tool:bash".to_string(),
        order: SectionOrder::ToolBash.value(),
        text: PromptText::Static(
            "每次查看 bash 结果中的退出码；失败时先排查再继续。命令语法须与本机 shell 方言一致。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:read".to_string(),
        order: SectionOrder::ToolRead.value(),
        text: PromptText::Static(
            "用 read_file 查看文本文件,不要用 bash 的 cat/type。大文件可分段读取。".to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:write".to_string(),
        order: SectionOrder::ToolWrite.value(),
        text: PromptText::Static(
            "用 write_file 创建或整文件覆盖;覆盖前先 read_file 确认现有内容。".to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:glob".to_string(),
        order: SectionOrder::ToolGlob.value(),
        text: PromptText::Static(
            "用 glob 工具——不要用 shell 的 find——按路径模式找文件。无 \"/\" 的模式在任意深度匹配 basename,所以 \"*.rs\" 搜整棵树;结果只含文件、按修改时间排序。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:grep".to_string(),
        order: SectionOrder::ToolGrep.value(),
        text: PromptText::Static(
            "用 grep 工具——不要用 shell 的 grep/rg——全文搜索;搜索大目录可加 include 收窄,命中上限后收窄 pattern 再看更多;命中的文件用 read_file 读上下文。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:edit".to_string(),
        order: SectionOrder::ToolEdit.value(),
        text: PromptText::Static(
            "精确小改动用 edit 工具(必须精确出现一次的字符串替换);大段改动用 write_file。改完用 read_file 或 grep 验证结果。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:todo".to_string(),
        order: SectionOrder::ToolWrite.value() + 1,
        text: PromptText::Static(
            "多步任务开工前先用 todo_write 拆步骤;每完成一步立即把对应项标 completed 并把下一步标 in_progress,不要攒到最后一起更新;全部做完才允许没有 in_progress 项。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;

    let schemas: Vec<ToolSchema> = tools.schemas();
    prompt.tools(move |_| ToolProviderResult {
        schemas: schemas.clone(),
        known_names: None,
    });
    Ok(())
}

/// Shipped registry pair: prompt assembly plus executable tools.
pub fn default_shipped() -> (SystemPrompt, ToolRegistry) {
    let tools = crate::default_registry();
    let mut prompt =
        denia_system_prompt::SystemPrompt::new(denia_system_prompt::SystemPromptConfig::default());
    register_shipped_prompt(&mut prompt, &tools).expect("shipped prompt registrations are valid");
    (prompt, tools)
}

/// Shipped pair + browser tool(`hub` 提供时,registry 与 prompt schemas 同步带上 browser)。
///
/// `default_shipped()` 的 prompt 在 `register_shipped_prompt` 里固化了当时 registry
/// 的 schemas;此处对同一 prompt 追加 browser schema,保证模型可见与可执行一致。
pub fn default_shipped_with_browser(hub: Option<BrowserHub>) -> (SystemPrompt, ToolRegistry) {
    let (mut prompt, tools) = default_shipped();
    if let Some(hub) = hub {
        let tool = Arc::new(crate::BrowserTool::new(hub));
        let schema = tool.schema().clone();
        prompt.tools(move |_| ToolProviderResult {
            schemas: vec![schema.clone()],
            known_names: None,
        });
        let mut registry = tools;
        registry.register(tool);
        return (prompt, registry);
    }
    (prompt, tools)
}

/// Shipped pair + browser + recon(JS 逆向)。prompt schemas 同步追加两者。
/// `SystemPrompt::tools` 是追加语义,依次 push provider,模型可见全部 schema。
pub fn default_shipped_with_browser_and_recon(
    browser_hub: Option<BrowserHub>,
    recon_hub: Option<ReconHub>,
) -> (SystemPrompt, ToolRegistry) {
    let (mut prompt, mut registry) = default_shipped_with_browser(browser_hub);
    if let Some(hub) = recon_hub {
        let tool = Arc::new(crate::ReconTool::new(hub));
        let schema = tool.schema().clone();
        prompt.tools(move |_| ToolProviderResult {
            schemas: vec![schema.clone()],
            known_names: None,
        });
        registry.register(tool);
    }
    (prompt, registry)
}

/// 自定义系统提示词正文 + 同一套 shipped 工具与工具纪律段(不含单独的 harness:identity)。
pub fn shipped_with_persona(persona_text: String) -> (SystemPrompt, ToolRegistry) {
    let tools = crate::default_registry();
    let mut prompt = denia_system_prompt::SystemPrompt::new_with_persona(
        denia_system_prompt::SystemPromptConfig {
            include_harness_identity: false,
            ..Default::default()
        },
        persona_text,
    );
    register_shipped_prompt(&mut prompt, &tools).expect("shipped prompt registrations are valid");
    (prompt, tools)
}

/// `shipped_with_persona` + browser tool(schema 同步进 prompt,registry 同步注册)。
pub fn shipped_with_persona_and_browser(
    persona_text: String,
    hub: BrowserHub,
) -> (SystemPrompt, ToolRegistry) {
    shipped_with_persona_and_browser_and_recon(persona_text, Some(hub), None)
}

/// `shipped_with_persona_and_browser` + recon 工具(schema 追加,registry 注册)。
pub fn shipped_with_persona_and_browser_and_recon(
    persona_text: String,
    hub: Option<BrowserHub>,
    recon_hub: Option<ReconHub>,
) -> (SystemPrompt, ToolRegistry) {
    let (mut prompt, mut registry) = shipped_with_persona(persona_text);
    if let Some(hub) = hub {
        let tool = Arc::new(crate::BrowserTool::new(hub));
        let schema = tool.schema().clone();
        prompt.tools(move |_| ToolProviderResult {
            schemas: vec![schema.clone()],
            known_names: None,
        });
        registry.register(tool);
    }
    if let Some(hub) = recon_hub {
        let tool = Arc::new(crate::ReconTool::new(hub));
        let schema = tool.schema().clone();
        prompt.tools(move |_| ToolProviderResult {
            schemas: vec![schema.clone()],
            known_names: None,
        });
        registry.register(tool);
    }
    (prompt, registry)
}

fn runtime_context_text(context: &AssembleContext) -> String {
    let runtime = shell::shell_runtime();
    let cwd = context.cwd.as_deref().unwrap_or("(unknown)");
    let shell_note = shell::shell_system_prompt_note(&runtime);
    format!(
        "平台:{os} ({arch})。日期:{date}。{shell_note}",
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        date = today_string(),
        shell_note = shell_note,
    )
    .replace("{{cwd}}", cwd)
}

/// 公历日期 yyyy-mm-dd,不引 chrono:unix 秒 → civil 算法。
fn today_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

#[cfg(test)]
mod tests {
    use denia_system_prompt::{render_context_snapshot, render_prompt, render_prompt_for_user};

    use super::*;

    #[test]
    fn shipped_with_persona_omits_harness_identity() {
        let (prompt, _tools) = shipped_with_persona("仅自定义正文".to_string());
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/work".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !assembly
                .sections
                .iter()
                .any(|section| section.name == "harness:identity")
        );
        let user = render_prompt_for_user(&assembly);
        assert!(user.contains("仅自定义正文"));
        assert!(!user.contains("denia 驱动的"));
    }

    #[test]
    fn shipped_with_persona_overrides_default_persona() {
        let (prompt, _tools) = shipped_with_persona("自定义 persona {{cwd}}".to_string());
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/work".to_string()),
                ..Default::default()
            })
            .unwrap();
        let rendered = render_prompt(&assembly);
        assert!(rendered.contains("自定义 persona /work"));
        assert!(
            assembly
                .sections
                .iter()
                .any(|section| section.name == "tool:bash")
        );
    }

    #[test]
    fn shipped_prompt_includes_tool_sections_and_runtime_context() {
        let (prompt, _tools) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                model: Some("mock".to_string()),
                provider: Some("mock".to_string()),
                ..Default::default()
            })
            .unwrap();
        let rendered = render_prompt(&assembly);
        assert!(rendered.contains("denia"));
        assert!(rendered.contains("/tmp/ws"));
        assert!(
            assembly
                .sections
                .iter()
                .any(|section| section.name == "tool:bash")
        );
        assert!(
            assembly
                .sections
                .iter()
                .any(|section| section.name == "tool:todo")
        );
        assert!(!render_context_snapshot(&assembly).is_empty());
        assert_eq!(assembly.tools.len(), 7);
    }
}

#[cfg(test)]
mod browser_prompt_tests {
    use super::*;

    #[test]
    fn default_shipped_with_browser_includes_schema() {
        // hub 需要 trait 对象;用 BrowserTool 侧的桥接实现 — 这里只验证 schema 进 prompt。
        // 构造一个假的 BrowserExecute 实现即可。
        struct FakeHub;
        #[async_trait::async_trait]
        impl crate::browser::BrowserExecute for FakeHub {
            async fn execute(
                &self,
                _command: denia_browser::BrowserCommand,
            ) -> denia_browser::CommandOutcome {
                denia_browser::CommandOutcome::ok_value(serde_json::Value::Null, 0)
            }
        }
        let hub: crate::BrowserHub = std::sync::Arc::new(FakeHub);
        let (prompt, registry) = default_shipped_with_browser(Some(hub));
        let assembly = prompt
            .assemble(&denia_system_prompt::AssembleContext::default())
            .expect("assemble");
        let names: Vec<&str> = assembly
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(
            names.contains(&"browser"),
            "browser schema missing from prompt tools: {names:?}"
        );
        assert!(registry.get("browser").is_some(), "browser not registered");
    }
}
