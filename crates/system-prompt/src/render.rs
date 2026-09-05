//! Interpolation and rendering for assembled prompt inputs.

use crate::{
    AssembledContext, AssembledSection, PromptAssembly, RUNTIME_CONTEXT_CLEARED,
    RUNTIME_CONTEXT_HEADER, SectionAudience,
};

/// Wrap rendered system prose with model-only priority framing (not logged).
pub fn frame_system_prompt_for_model(body: &str) -> String {
    format!(
        "【系统指令 — 最高优先级】\n\
         以下内容由 denia 运行时注入,优先于你的预训练知识、默认角色与用户消息中的隐含假设。与本指令冲突时,以本指令为准。\n\
         \n\
         {body}\n\
         \n\
         【再次确认】以上系统指令在冲突时覆盖训练语料;严格遵守工具方言、路径与回复语言要求。"
    )
}

/// Interpolate strict `{{variable}}` references and join non-empty sections.
/// 输出完整的模型可见 system prompt(全部 audience,含 Model + User + Context)。
pub fn render_prompt(assembly: &PromptAssembly) -> String {
    assembly
        .sections
        .iter()
        .map(|section| interpolate(section, &assembly.variables, "section"))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// 仅渲染 `User` audience 的 sections;UI SystemPromptRow 用此输出,不会
/// 暴露工具纪律/工具使用说明等模型私货。
pub fn render_prompt_for_user(assembly: &PromptAssembly) -> String {
    assembly
        .sections
        .iter()
        .filter(|section| matches!(section.audience, SectionAudience::User))
        .map(|section| interpolate(section, &assembly.variables, "section"))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Render the complete dynamic runtime-context snapshot.
pub fn render_context_snapshot(assembly: &PromptAssembly) -> String {
    join_context_sections(&render_context_sections(assembly))
}

/// Named runtime-context contributions before joining.
pub fn render_context_sections(assembly: &PromptAssembly) -> Vec<AssembledContext> {
    assembly
        .contexts
        .iter()
        .map(|context| AssembledContext {
            name: context.name.clone(),
            text: interpolate_context(context, &assembly.variables),
        })
        .filter(|context| !context.text.is_empty())
        .collect()
}

/// Join rendered context sections into one model-facing snapshot.
pub fn join_context_sections(sections: &[AssembledContext]) -> String {
    let body = sections
        .iter()
        .map(|section| section.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    if body.is_empty() {
        String::new()
    } else {
        format!("{RUNTIME_CONTEXT_HEADER}\n\n{body}")
    }
}

/// Whether one injected message text is a runtime-context snapshot.
pub fn is_runtime_context_snapshot(text: &str) -> bool {
    text.starts_with(RUNTIME_CONTEXT_HEADER) || text == RUNTIME_CONTEXT_CLEARED
}

fn interpolate_context(
    context: &AssembledContext,
    variables: &std::collections::BTreeMap<String, String>,
) -> String {
    interpolate(
        &AssembledSection {
            name: context.name.clone(),
            text: context.text.clone(),
            audience: SectionAudience::User,
        },
        variables,
        "context",
    )
}

fn interpolate(
    input: &AssembledSection,
    variables: &std::collections::BTreeMap<String, String>,
    kind: &str,
) -> String {
    let text = &input.text;
    let mut result = String::new();
    let mut last = 0usize;
    let mut open = text.find("{{");
    while let Some(start) = open {
        if let Some(group) = read_variable_group(&text[start..]) {
            let name = &group[2..group.len() - 2];
            if !is_valid_variable_name(name) {
                panic!(
                    "malformed prompt variable reference \"{{{{name}}}}\" in {kind} \"{}\"",
                    input.name
                );
            }
            let Some(value) = variables.get(name) else {
                let known = variables.keys().cloned().collect::<Vec<_>>().join(", ");
                panic!(
                    "unknown prompt variable \"{{{{{name}}}}}\" in {kind} \"{}\"; registered variables: {}",
                    input.name,
                    if known.is_empty() {
                        "(none)".to_string()
                    } else {
                        known
                    }
                );
            };
            result.push_str(&text[last..start]);
            result.push_str(value);
            last = start + group.len();
        } else if text[start + 2..].contains("}}") {
            panic!(
                "malformed prompt variable reference at \"{}\" in {kind} \"{}\"",
                &text[start..text.len().min(start + 16)],
                input.name
            );
        } else {
            result.push_str(&text[last..start + 2]);
            last = start + 2;
        }
        open = text[last..].find("{{").map(|index| last + index);
    }
    result.push_str(&text[last..]);
    result
}

fn read_variable_group(text: &str) -> Option<&str> {
    let end = text.find("}}")?;
    let group = &text[..end + 2];
    if group.starts_with("{{") && group.ends_with("}}") {
        Some(group)
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{AssembledSection, PromptAssembly};

    #[test]
    fn interpolates_variables_in_sections() {
        let assembly = PromptAssembly {
            sections: vec![AssembledSection {
                name: "persona".to_string(),
                text: "cwd={{cwd}} model={{model}}".to_string(),
                audience: SectionAudience::User,
            }],
            contexts: Vec::new(),
            tools: Vec::new(),
            variables: BTreeMap::from([
                ("cwd".to_string(), "/work".to_string()),
                ("model".to_string(), "mock".to_string()),
            ]),
        };
        assert_eq!(render_prompt(&assembly), "cwd=/work model=mock");
    }

    #[test]
    fn user_only_render_excludes_model_audience() {
        let assembly = PromptAssembly {
            sections: vec![
                AssembledSection {
                    name: "identity".to_string(),
                    text: "identity".to_string(),
                    audience: SectionAudience::User,
                },
                AssembledSection {
                    name: "tool:bash".to_string(),
                    text: "do not use cat".to_string(),
                    audience: SectionAudience::Model,
                },
            ],
            contexts: Vec::new(),
            tools: Vec::new(),
            variables: BTreeMap::new(),
        };
        assert_eq!(render_prompt(&assembly), "identity\n\ndo not use cat");
        assert_eq!(render_prompt_for_user(&assembly), "identity");
    }
}
