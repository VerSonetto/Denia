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
    // 对照 dsh FILE_REFERENCE_PROMPT:仅路径占位,内容留在 read/列目录工具后面。
    prompt.section(PromptSection {
        name: "context:file-reference".to_string(),
        order: SectionOrder::FileReference.value(),
        text: PromptText::Static(
            "用户消息中带 @ 前缀的是用户明确引用的工作区路径,相对工作区根目录。结尾带 / 的是目录:需要其内容时用 bash 列目录查看;其余是文件:需要其内容时用 read_file 读取,读取前不要声称已查看过内容,也不要猜测内容。路径含空格时用 @\"...\" 表示。"
                .to_string(),
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

/// 宿主能力工具(子代理委派 / 后台任务 / 技能)的工具纪律段。
///
/// server 部署总是注册这些工具,由 prompt store 在构建系统提示词后显式
/// 调用本函数归位纪律;无 runtime 的部署(如测试)不注入,模型不会看到
/// 不存在的工具。此前这段纪律以 `[denia 能力上下文]` 注入消息落盘,
/// 现在归位到系统提示词的 Model audience(与 tool:bash 等段落同性质)。
pub fn register_capability_prompt_sections(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:agents".to_string(),
        order: SectionOrder::ToolAgents.value(),
        text: PromptText::Static(
            "独立任务用 spawn_agent/fork_agent 委派给子代理;send_message 只能在直接父子代理之间收发消息。把可以独立进行的子任务拆给子代理分工合作,不要全部自己做;需要立即拿到结果的前台委派用 run_in_background=false 阻塞等结果,可以并行推进的后台委派保持默认后台运行,子代理完成时会通知父会话。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:jobs".to_string(),
        order: SectionOrder::ToolJobs.value(),
        text: PromptText::Static(
            "长时间命令用 job_start 后台运行,job_output 领取输出,job_kill 停止;等待子代理结果用 wait_agent。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:skill".to_string(),
        order: SectionOrder::ToolSkill.value(),
        text: PromptText::Static(
            "技能目录以独立注入的 <available_skills> 为准;开工前先对照目录检查有没有与当前任务匹配的技能,有匹配的先 load 再动手,没有匹配的就直接处理,不要为了用技能而调用技能。skill 工具 load 后 SKILL.md 全文直接在结果中返回,加载一次即可,不要再用文件读取工具读 SKILL.md;references/scripts 等其余文件用 skill 工具的 resource 操作按返回的 resourceBase 相对路径读取,技能不授予额外权限。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 浏览器工具(browser/recon)的资源回收纪律段。
///
/// 段与 browser schema 严格同步:仅在带 hub 的 builder 里注册(hub 为 Some
/// 才有工具),无 browser 的部署模型不会看到不存在的工具。机制上关闭最后
/// 一个 tab 即彻底回收(manager 层),本段只负责让模型主动收尾清理。
fn register_browser_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:browser".to_string(),
        order: SectionOrder::ToolBrowser.value(),
        text: PromptText::Static(
            "browser 与 recon 共用一个常驻浏览器实例(与控制台面板同款),打开的 tab 不会随任务结束自动关闭。启动按需:getState 与 navigate/newTab 等交互命令会拉起浏览器;list/close/activate/networkList/getDialog 是查询与善后命令,浏览器没跑时原地返回(list 返回空 tabs、close 回报已关闭、networkList 返回空列表),绝不拉起实例——收尾时的确认查询不会凭空造出 tab。每次浏览器任务完成、或确认后续不再使用浏览器时,必须彻底清除浏览器资源:先 list 查看现存 tab,再把本次打开的 tab 逐个 close;关闭最后一个 tab 会彻底回收浏览器进程与全部状态,这是唯一的彻底清除方式。判定口径:list 返回空 tabs 即已清理干净(浏览器已回收、无残留 tab),此时立即停止操作,不要再调 getState/navigate 等会启动浏览器的命令;严禁留着打开的 tab 结束任务。\nbrowser 工具默认在后台无界面操作,控制台不展示浏览器画面。仅当用户明确要求看着他操作浏览器(如\"打开给我看\"\"可视化模式\"),或用户的话里明显需要亲眼看到画面(如\"看下这个页面长什么样\"\"演示一下操作\")时,才给命令加 visualMode: true 展开浏览器侧栏;用户没有这类表示时不要开启,常规抓取、点击、检查接口等后台任务保持默认。开启后用户手动收起侧栏即为不要看,不要再重复请求展开。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
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
        register_browser_section(&mut prompt).expect("browser prompt section is valid");
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
        register_browser_section(&mut prompt).expect("browser prompt section is valid");
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

    #[test]
    fn file_reference_section_is_model_audience() {
        let (prompt, _tools) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                ..Default::default()
            })
            .unwrap();
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "context:file-reference")
            .expect("file-reference section registered");
        assert_eq!(section.audience, SectionAudience::Model);
        assert!(section.text.contains('@'));
        // 工具纪律类段不进用户可见副本。
        let user_body = render_prompt_for_user(&assembly);
        assert!(!user_body.contains("read_file 读取"));
    }
}

#[cfg(test)]
mod browser_prompt_tests {
    use super::*;

    struct FakeHub {
        visual_requests: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl crate::browser::BrowserExecute for FakeHub {
        async fn execute(
            &self,
            _command: denia_browser::BrowserCommand,
        ) -> denia_browser::CommandOutcome {
            denia_browser::CommandOutcome::ok_value(serde_json::Value::Null, 0)
        }

        async fn request_visual_mode(&self) {
            self.visual_requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn fake_hub() -> crate::BrowserHub {
        std::sync::Arc::new(FakeHub {
            visual_requests: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    async fn run_browser_tool(hub: crate::BrowserHub, arguments: &str) -> crate::ToolOutput {
        use denia_core::session::PermissionMode;
        use tokio_util::sync::CancellationToken;

        let ctx = crate::ToolContext {
            session_id: None,
            selection: None,
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: true,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::WorkspaceWrite,
            permission_override: None,
        };
        let tool = crate::BrowserTool::new(hub);
        tool.execute(arguments, &ctx).await
    }

    #[test]
    fn default_shipped_with_browser_includes_schema() {
        // hub 需要 trait 对象;用 BrowserTool 侧的桥接实现 — 这里只验证 schema 进 prompt。
        let hub = fake_hub();
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

    #[test]
    fn browser_section_registered_only_with_hub() {
        // 默认 persona 模板引用 {{cwd}},render 时必须提供,否则插值 fail loud。
        let context = denia_system_prompt::AssembleContext {
            cwd: Some("/tmp/ws".to_string()),
            ..Default::default()
        };
        // 带 hub:tool:browser 段注册,Model 受众,不进用户可见副本。
        let (prompt, _registry) = default_shipped_with_browser(Some(fake_hub()));
        let assembly = prompt.assemble(&context).expect("assemble");
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "tool:browser")
            .expect("tool:browser section registered");
        assert_eq!(section.audience, SectionAudience::Model);
        assert!(section.text.contains("彻底清除浏览器资源"));
        // 启动语义必须写清:哪些命令拉起实例、哪些只查询不拉起。
        assert!(section.text.contains("getState 与 navigate/newTab 等交互命令会拉起浏览器"));
        assert!(section.text.contains("绝不拉起实例"));
        assert!(section.text.contains("list 返回空 tabs 即已清理干净"));
        assert!(section.text.contains("visualMode"));
        let user_body = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(!user_body.contains("彻底清除浏览器资源"));

        // persona 变体带 hub 同样有段。
        let (prompt, _registry) = shipped_with_persona_and_browser_and_recon(
            "自定义 persona".to_string(),
            Some(fake_hub()),
            None,
        );
        let assembly = prompt.assemble(&context).expect("assemble");
        assert!(
            assembly
                .sections
                .iter()
                .any(|section| section.name == "tool:browser"),
            "persona+browser variant missing tool:browser section"
        );
    }

    #[test]
    fn browser_section_absent_without_hub() {
        // 无 browser 工具的部署不注入纪律段:模型不看到不存在的工具。
        for (prompt, _tools) in [
            default_shipped(),
            default_shipped_with_browser(None),
        ] {
            let assembly = prompt
                .assemble(&denia_system_prompt::AssembleContext::default())
                .expect("assemble");
            assert!(
                !assembly
                    .sections
                    .iter()
                    .any(|section| section.name == "tool:browser"),
                "tool:browser must not be registered without browser tool"
            );
        }
    }

    #[tokio::test]
    async fn visual_mode_requests_sidebar_before_command() {
        // visualMode: true → 命令执行前请求一次可视化模式;命令本身正常执行。
        let hub = std::sync::Arc::new(FakeHub {
            visual_requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let output = run_browser_tool(
            hub.clone() as crate::BrowserHub,
            r#"{"method":"navigate","url":"https://example.com","visualMode":true}"#,
        )
        .await;
        assert!(!output.is_error, "command should succeed");
        assert_eq!(
            hub.visual_requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "visual mode should be requested exactly once"
        );
    }

    #[tokio::test]
    async fn visual_mode_off_by_default() {
        // 缺省 false:不请求可视化模式,后台操作不打扰用户。
        let hub = std::sync::Arc::new(FakeHub {
            visual_requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let output = run_browser_tool(
            hub.clone() as crate::BrowserHub,
            r#"{"method":"list"}"#,
        )
        .await;
        assert!(!output.is_error, "command should succeed");
        assert_eq!(
            hub.visual_requests.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "visual mode must not be requested by default"
        );
    }
}

#[cfg(test)]
mod capability_prompt_tests {
    use super::*;

    #[test]
    fn capability_sections_land_in_model_audience() {
        let mut prompt = denia_system_prompt::SystemPrompt::new(
            denia_system_prompt::SystemPromptConfig::default(),
        );
        prompt.variable("cwd", |context| context.cwd.clone()).unwrap();
        prompt.variable("model", |context| context.model.clone()).unwrap();
        prompt
            .variable("provider", |context| context.provider.clone())
            .unwrap();
        register_capability_prompt_sections(&mut prompt).unwrap();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".into()),
                model: Some("mock".into()),
                provider: Some("mock".into()),
                ..Default::default()
            })
            .unwrap();
        let body = denia_system_prompt::render_prompt(&assembly);
        assert!(body.contains("spawn_agent/fork_agent"), "{}", &body[..body.len().min(2000)]);
        assert!(body.contains("job_start"));
        assert!(body.contains("<available_skills>"));
        // Model audience 段不进用户可见副本。
        let user_body = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(!user_body.contains("spawn_agent/fork_agent"));
        assert!(!user_body.contains("job_start"));
    }

    #[test]
    fn default_shipped_has_no_capability_sections() {
        // 无 runtime 部署(default_shipped)不注入能力纪律段:模型不看到
        // 不存在的工具。
        let (prompt, _) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".into()),
                model: Some("mock".into()),
                provider: Some("mock".into()),
                ..Default::default()
            })
            .unwrap();
        let body = denia_system_prompt::render_prompt(&assembly);
        assert!(!body.contains("spawn_agent/fork_agent"));
    }
}
