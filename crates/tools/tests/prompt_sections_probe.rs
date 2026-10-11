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

/// 同一机械约束只允许在一处出现:每项是(句, 允许的最大出现次数)。
///
/// 这些话都曾经在出厂提示词里连着出现两三遍——ls/find/grep/cat 的 shell 禁令
/// 在五个工具段里各重写一遍、"上下文变长不要停" 在工作风格与上下文管理两条
/// 纪律里各说一遍、"先 read_file 确认" 在 write 与 edit 两段里各说一遍。收敛后
/// 每句只剩一个家:
///
/// - 上限 1:权威段留着它,别处不许再抄。这一项是会真的失败的——往任何一段
///   里再抄一遍就变成 2。
/// - 上限 0:这句是被删掉的重复文本,一个字都不该回来。
///
/// 反向验证方式:在任意一个段里把某句加回去,本测试必须失败(简报里有实测输出)。
const BANNED_REPEATS: &[(&str, usize)] = &[
    // shell 检索禁令:总纲在 tool:bash 一段,其余工具段不再逐段重写。
    ("不要用 shell 的", 0),
    ("也不要写 PowerShell 的", 0),
    ("不要用 bash 的", 0),
    ("PowerShell 的 Get-ChildItem", 1),
    ("Select-String", 1),
    // 改动文件前先读原文:权威是 tool:write 段。
    ("先 read_file 确认", 1),
    // 上下文变长不要停:权威是 harness:context-management(它随压缩开关进退)。
    ("不要因为上下文或会话变长就停下", 1),
    // 后台结果不要轮询、pending/ready 不是成功:权威是 tool:agents 段。
    ("可以继续独立工作,或先向用户回复当前进度并结束本轮", 1),
    ("不要把 pending 或 ready 当作执行成功", 0),
    // 破坏性操作先确认:权威是 harness:risk-honesty;权限模式段里的那一句是
    // 安全关键的模式语义,不动;工作风格里只做回指,不重述整句。
    ("只有破坏性操作或真正的范围变更才停下来问", 0),
    // 语言纪律:权威是 harness:communication;persona 独占档随 persona 携带。
    ("始终使用简体中文回复", 1),
];

/// 出厂全量装配 + 能力纪律段(server 部署的真实形态:agent/jobs/skill 段在
/// 构建系统提示词后由 prompt store 单独归位)。重复检测必须看它们——
/// tool:agents 与 tool:jobs 的行为纪律就在那里。
fn full_assembly() -> denia_system_prompt::PromptAssembly {
    let (mut prompt, _tools) = denia_tools::default_shipped_with_browser_and_ask(None, true);
    denia_tools::register_capability_prompt_sections(&mut prompt).expect("capability sections");
    prompt
        .assemble(&AssembleContext {
            cwd: Some("D:\\code_project\\denia".to_string()),
            model: Some("claude-sonnet-4-5".to_string()),
            provider: Some("anthropic".to_string()),
            permission_mode: Some("auto-edit".to_string()),
        })
        .expect("assemble")
}

fn section<'a>(assembly: &'a denia_system_prompt::PromptAssembly, name: &str) -> &'a str {
    assembly
        .sections
        .iter()
        .find(|section| section.name == name)
        .unwrap_or_else(|| panic!("缺少段落 {name}"))
        .text
        .as_str()
}

/// 同一条机械约束只能在渲染结果里出现限定次数。
#[test]
fn mechanical_rules_are_stated_at_most_once() {
    let model = render_prompt(&full_assembly());
    let mut violations: Vec<String> = Vec::new();
    for (phrase, limit) in BANNED_REPEATS {
        let seen = model.matches(phrase).count();
        if seen > *limit {
            violations.push(format!("{phrase:?} 出现 {seen} 次,上限 {limit}"));
        }
    }
    assert!(
        violations.is_empty(),
        "同一条机械约束被多处重复:{violations:#?}"
    );
}

/// 收敛重复句时不许把各工具独有的用法说明一起删掉。
#[test]
fn tool_specific_usage_notes_survive_the_deduplication() {
    let assembly = full_assembly();
    for (name, needles) in [
        (
            "tool:bash",
            vec![
                "按动作界定",
                ".gitignore",
                "Get-ChildItem",
                "Select-String",
                "Get-Content",
                "read_file",
            ],
        ),
        ("tool:ls", vec!["depth(上限 5)", "不要猜文件路径"]),
        ("tool:read", vec!["大文件可分段读取"]),
        ("tool:write", vec!["覆盖前先 read_file 确认现有内容"]),
        (
            "tool:glob",
            vec!["匹配 basename", "结果只含文件、按修改时间排序"],
        ),
        ("tool:grep", vec!["include 收窄", "命中上限后收窄 pattern"]),
        ("tool:edit", vec!["两种格式严格二选一", "replace_all"]),
        (
            "tool:agents",
            vec![
                "ready 只表示结果可读取",
                "可以继续独立工作,或先向用户回复当前进度并结束本轮",
            ],
        ),
        (
            "tool:jobs",
            vec!["wait=true 也不前台等待", "job_kill 停止", "四件探索类的事"],
        ),
        ("harness:context-management", vec!["会被摘要"]),
        (
            "harness:working-style",
            vec!["只有任务完成", "不可逆或对外的操作按风险纪律先确认"],
        ),
        ("harness:communication", vec!["先给结论"]),
        ("harness:risk-honesty", vec!["先确认再做"]),
    ] {
        let text = section(&assembly, name);
        for needle in needles {
            assert!(
                text.contains(needle),
                "{name} 段丢了用法说明 {needle}:{text}"
            );
        }
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
    println!(
        "{}",
        denia_system_prompt::render_context_snapshot(&assembly)
    );
    println!("\n========== 拼接结果(模型可见,不含工具定义) ==========\n");
    println!("{}", render_prompt(&assembly));
    println!("\n========== 用户可见副本 ==========\n");
    println!("{}", render_prompt_for_user(&assembly));
}
