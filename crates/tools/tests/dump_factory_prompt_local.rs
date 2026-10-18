//! 本地临时脚本(不提交):按源码出厂配置拼出完整系统提示词。
//!
//! `cargo test -p denia-tools --test dump_factory_prompt_local -- --nocapture`

use std::path::PathBuf;

use async_trait::async_trait;
use denia_system_prompt::{
    AssembleContext, PromptContext, PromptSection, PromptText, SectionAudience, SectionOrder,
    render_context_snapshot, render_model_system, render_prompt, render_prompt_for_user,
};

struct StubHub;

#[async_trait]
impl denia_tools::BrowserExecute for StubHub {
    async fn execute(&self, _command: denia_browser::BrowserCommand) -> denia_browser::CommandOutcome {
        unimplemented!("stub hub 只用于装配提示词,不执行命令")
    }
}

#[test]
fn dump_factory_prompt() {
    // 与 server 部署 build_prompt 同一套注册,persona 用出厂模板(SYSTEM.md 为空)。
    let hub: denia_tools::BrowserHub = std::sync::Arc::new(StubHub);
    let (mut prompt, _tools) = denia_tools::shipped_with_persona_and_browser_and_ask(
        denia_system_prompt::default_persona_template().to_string(),
        Some(hub),
        true,
    );
    denia_tools::register_capability_prompt_sections(&mut prompt).expect("capability sections");
    denia_tools::register_memory_prompt_section(&mut prompt).expect("memory section");
    denia_tools::register_preset_prompt_section(&mut prompt).expect("preset section");
    prompt
        .context(PromptContext {
            name: "harness:instance".to_string(),
            order: 12,
            text: PromptText::Static(
                "本实例 API:http://127.0.0.1:[PORT](本机 HTTP)。任何读取或修改 denia 自身配置的操作(提供商、密钥、MCP、preset、系统提示词)一律调用这个地址;多实例并存时不要猜默认端口,它可能不是当前实例。".to_string(),
            ),
        })
        .expect("instance context");
    // MCP 段的目录行由运行时名册渲染;这里给占位,正文纪律来自源码。
    let ids = vec![
        "[MCP_SERVER_A]".to_string(),
        "[MCP_SERVER_B]".to_string(),
    ];
    prompt
        .section(PromptSection {
            name: "tool:mcp".to_string(),
            order: SectionOrder::ToolMcp.value(),
            text: PromptText::Static(denia_tools::prompt::render_mcp_section(&ids)),
            complete: false,
            audience: SectionAudience::Model,
        })
        .expect("mcp section");

    let assembly = prompt
        .assemble(&AssembleContext {
            cwd: Some("D:\\code_project\\denia".to_string()),
            model: Some("[MODEL]".to_string()),
            provider: Some("[PROVIDER]".to_string()),
            permission_mode: Some("auto-edit".to_string()),
        })
        .expect("assemble");

    let mut out = String::new();
    out.push_str("########## 全部段落(按 order) ##########\n\n");
    for section in &assembly.sections {
        out.push_str(&format!(
            "----- {} ({:?}) -----\n{}\n\n",
            section.name, section.audience, section.text
        ));
    }
    out.push_str("\n########## 运行时上下文快照 ##########\n\n");
    out.push_str(&render_context_snapshot(&assembly));
    out.push_str("\n\n########## 拼接结果(模型可见正文) ##########\n\n");
    out.push_str(&render_prompt(&assembly));
    out.push_str("\n\n########## 用户可见副本 ##########\n\n");
    out.push_str(&render_prompt_for_user(&assembly));
    out.push('\n');

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..");
    let path = root.join(".tmp-factory-prompt.txt");
    std::fs::write(&path, &out).expect("write dump");
    // 发给模型时的完整 system 字段:人格段在框架之外、其余段落被框架包住。
    std::fs::write(
        root.join(".tmp-prompt-framed.txt"),
        render_model_system(&assembly),
    )
    .expect("write framed system");
    // 运行时上下文快照单独一条注入消息,不在 system 字段里。
    std::fs::write(
        root.join(".tmp-prompt-snapshot.txt"),
        render_context_snapshot(&assembly),
    )
    .expect("write snapshot");
    // 工具 schema 也导出:它同样占请求的 token,但不在 system 文本里。
    let schema_json = serde_json::to_string_pretty(&assembly.tools).expect("tools json");
    std::fs::write(root.join(".tmp-tools-schema.json"), &schema_json).expect("write tools json");
    println!(
        "工具 {} 个,schema JSON {} 字符",
        assembly.tools.len(),
        schema_json.chars().count()
    );
    println!("written {} chars to {}", out.chars().count(), path.display());
    println!("模型正文 {} 字符", render_prompt(&assembly).chars().count());
}
