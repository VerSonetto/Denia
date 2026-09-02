//! System-prompt contributions for the shipped tool set.

use std::sync::Arc;

use denia_core::tool::ToolSchema;
use denia_system_prompt::{
    AssembleContext, PromptContext, PromptSection, PromptText, SectionOrder, SystemPrompt,
    ToolProviderResult,
};

use crate::shell;
use crate::ToolRegistry;

/// Register tool schemas, tool guidance sections, runtime facts, and variables.
pub fn register_shipped_prompt(prompt: &mut SystemPrompt, tools: &ToolRegistry) -> Result<(), String> {
    prompt.variable("cwd", |context| context.cwd.clone())?;
    prompt.variable("model", |context| context.model.clone())?;
    prompt.variable("provider", |context| context.provider.clone())?;

    prompt.context(PromptContext {
        name: "harness:runtime".to_string(),
        order: 10,
        text: PromptText::Dynamic(Arc::new(runtime_context_text)),
    })?;

    prompt.section(PromptSection {
        name: "tool:bash".to_string(),
        order: SectionOrder::ToolBash.value(),
        text: PromptText::Static(
            "每次查看 bash 结果中的退出码；失败时先排查再继续。命令语法须与本机 shell 方言一致。"
                .to_string(),
        ),
        complete: false,
    })?;
    prompt.section(PromptSection {
        name: "tool:read".to_string(),
        order: SectionOrder::ToolRead.value(),
        text: PromptText::Static(
            "用 read_file 查看文本文件,不要用 bash 的 cat/type。大文件可分段读取。".to_string(),
        ),
        complete: false,
    })?;
    prompt.section(PromptSection {
        name: "tool:write".to_string(),
        order: SectionOrder::ToolWrite.value(),
        text: PromptText::Static(
            "用 write_file 创建或整文件覆盖;覆盖前先 read_file 确认现有内容。".to_string(),
        ),
        complete: false,
    })?;
    prompt.section(PromptSection {
        name: "tool:glob".to_string(),
        order: SectionOrder::ToolGlob.value(),
        text: PromptText::Static(
            "用 glob 工具——不要用 shell 的 find——按路径模式找文件。无 \"/\" 的模式在任意深度匹配 basename,所以 \"*.rs\" 搜整棵树;结果只含文件、按修改时间排序。"
                .to_string(),
        ),
        complete: false,
    })?;
    prompt.section(PromptSection {
        name: "tool:grep".to_string(),
        order: SectionOrder::ToolGrep.value(),
        text: PromptText::Static(
            "用 grep 工具——不要用 shell 的 grep/rg——全文搜索;搜索大目录可加 include 收窄,命中上限后收窄 pattern 再看更多;命中的文件用 read_file 读上下文。"
                .to_string(),
        ),
        complete: false,
    })?;
    prompt.section(PromptSection {
        name: "tool:edit".to_string(),
        order: SectionOrder::ToolEdit.value(),
        text: PromptText::Static(
            "精确小改动用 edit 工具(必须精确出现一次的字符串替换);大段改动用 write_file。改完用 read_file 或 grep 验证结果。"
                .to_string(),
        ),
        complete: false,
    })?;
    prompt.section(PromptSection {
        name: "tool:todo".to_string(),
        order: SectionOrder::ToolWrite.value() + 1,
        text: PromptText::Static(
            "多步任务开工前先用 todo_write 拆步骤;每完成一步立即把对应项标 completed 并把下一步标 in_progress,不要攒到最后一起更新;全部做完才允许没有 in_progress 项。"
                .to_string(),
        ),
        complete: false,
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
    let mut prompt = denia_system_prompt::SystemPrompt::new(
        denia_system_prompt::SystemPromptConfig::default(),
    );
    register_shipped_prompt(&mut prompt, &tools).expect("shipped prompt registrations are valid");
    (prompt, tools)
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
    use denia_system_prompt::{render_context_snapshot, render_prompt};

    use super::*;

    #[test]
    fn shipped_prompt_includes_tool_sections_and_runtime_context() {
        let (prompt, _tools) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                model: Some("mock".to_string()),
                provider: Some("mock".to_string()),
            })
            .unwrap();
        let rendered = render_prompt(&assembly);
        assert!(rendered.contains("denia"));
        assert!(rendered.contains("/tmp/ws"));
        assert!(assembly.sections.iter().any(|section| section.name == "tool:bash"));
        assert!(assembly.sections.iter().any(|section| section.name == "tool:todo"));
        assert!(!render_context_snapshot(&assembly).is_empty());
        assert_eq!(assembly.tools.len(), 7);
    }
}
