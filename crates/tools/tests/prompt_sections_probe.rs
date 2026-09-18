//! 手工核对入口:打印模型实际看到的完整系统提示词(不含工具定义)。
//!
//! `cargo test -p denia-tools --test prompt_sections_probe -- --nocapture`
use denia_system_prompt::{AssembleContext, render_prompt, render_prompt_for_user};

fn assembly() -> denia_system_prompt::PromptAssembly {
    let (prompt, _tools) = denia_tools::default_shipped_with_browser_and_ask(None, true);
    prompt
        .assemble(&AssembleContext {
            cwd: Some("D:\\code_project\\denia".to_string()),
            model: Some("claude-sonnet-4-5".to_string()),
            provider: Some("anthropic".to_string()),
            permission_mode: Some("auto-edit".to_string()),
        })
        .expect("assemble")
}

/// 用户可见副本只剩身份段——工具纪律、运行时快照、行为纪律都不进 UI。
#[test]
fn user_copy_is_identity_only() {
    let assembly = assembly();
    let user = render_prompt_for_user(&assembly);
    assert_eq!(
        user.trim(),
        "你是由 denia 驱动的 AI 编码 agent。",
        "用户可见副本应当只有身份段:{user}"
    );
}

/// 模型侧必须拿到全部段落(身份 + 工具纪律 + 行为纪律)。
#[test]
fn model_prompt_carries_all_sections() {
    let assembly = assembly();
    let model = render_prompt(&assembly);
    for needle in [
        "你是由 denia 驱动的 AI 编码 agent",
        "每步聚焦一件事",
        "有足够信息就动手",
        "始终使用简体中文回复",
    ] {
        assert!(model.contains(needle), "模型提示词缺少 {needle}:\n{model}");
    }
}

/// 手工核对:打印完整提示词。
#[test]
#[ignore]
fn print_full_model_prompt() {
    let assembly = assembly();
    println!("\n========== 静态段落 ==========\n");
    for section in &assembly.sections {
        println!("----- {} ({:?}) -----", section.name, section.audience);
        println!("{}\n", section.text);
    }
    println!("\n========== 运行时快照 ==========\n");
    println!("{}", denia_system_prompt::render_context_snapshot(&assembly));
    println!("\n========== 拼接结果(模型可见,不含工具定义) ==========\n");
    println!("{}", render_prompt(&assembly));
    println!("\n========== 用户可见副本 ==========\n");
    println!("{}", render_prompt_for_user(&assembly));
}
