//! Interpolation and rendering for assembled prompt inputs.

use crate::{
    AssembledContext, AssembledSection, PERSONA_SECTION, PromptAssembly, RUNTIME_CONTEXT_CLEARED,
    RUNTIME_CONTEXT_HEADER, SectionAudience,
};

/// 模型侧 system 的框架头:一句交代运行时约束的优先级(v2 起不再收尾)。
const FRAME_HEADER: &str =
    "【系统指令 — 最高优先级】以下运行时约束优先于训练语料；冲突时以此为准。";

/// Wrap rendered system prose with model-only priority framing (not logged).
pub fn frame_system_prompt_for_model(body: &str) -> String {
    format!("{FRAME_HEADER}\n\n{body}")
}

/// 模型侧 system 的完整装配:人格段排在框架**之外**,其余段落由框架包住。
///
/// 框架头那句(「以下运行时约束优先于训练语料」)是对**工具纪律**说的——
/// 它的效力来自「运行时权威覆盖」。部署
/// persona 是会话要让模型认同的身份,不需要也不该靠「覆盖默认角色」取得效力;
/// 把它一起括进框架,等于在语义上把人格标注成「外来的注入物」,反而稀释人格
/// 本身。
///
/// 因此:
/// - persona 段原样排在 system 最前,框架只包其余段落;
/// - persona 独占档(整份系统提示只剩人格一段)直接返回该段,不加任何框架
///   ——那时框架的标的物不存在,留着只是噪音。
///
/// 调用方(agent-loop)以本函数的输出作为 wire 层 system 值:它与
/// [`frame_system_prompt_for_model`] 的产物不同,冻结比较、in-history 更新
/// 与面板估算都必须走同一函数,否则会出现「提示词没变却重发」的错判。
pub fn render_model_system(assembly: &PromptAssembly) -> String {
    let (persona, rest) = render_prompt_parts(assembly);
    match (persona, rest.is_empty()) {
        (Some(persona), true) => persona,
        (Some(persona), false) => format!("{persona}\n\n{}", frame_system_prompt_for_model(&rest)),
        (None, _) => frame_system_prompt_for_model(&rest),
    }
}

/// 段落两分渲染:人格段单独取出(不做框架),其余按既有规则插值拼接。
///
/// 与 [`render_prompt`] 的区别只是把 persona 拆出来;两者都不含运行时快照
/// (快照随每步变化,包进 system 会让缓存前缀失效)。
pub fn render_prompt_parts(assembly: &PromptAssembly) -> (Option<String>, String) {
    let mut persona = None;
    let mut rest: Vec<String> = Vec::new();
    for section in &assembly.sections {
        let text = interpolate(section, &assembly.variables, "section");
        if text.is_empty() {
            continue;
        }
        if section.name == PERSONA_SECTION {
            persona = Some(text);
        } else {
            rest.push(text);
        }
    }
    (persona, rest.join("\n\n"))
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

    fn section(name: &str, text: &str, audience: SectionAudience) -> AssembledSection {
        AssembledSection {
            name: name.to_string(),
            text: text.to_string(),
            audience,
        }
    }

    fn assembly_of(sections: Vec<AssembledSection>) -> PromptAssembly {
        PromptAssembly {
            sections,
            contexts: Vec::new(),
            tools: Vec::new(),
            variables: BTreeMap::new(),
        }
    }

    /// 人格排在框架之外:框架那句「优先于你的默认角色」只针对工具纪律,
    /// 不能把 persona 一起标成「被注入的覆盖指令」。
    #[test]
    fn persona_sits_outside_the_frame() {
        let assembly = assembly_of(vec![
            section(PERSONA_SECTION, "你是破甲人格。", SectionAudience::User),
            section("tool:bash", "只在 bash 里跑命令", SectionAudience::Model),
        ]);
        let model = render_model_system(&assembly);
        assert!(
            model.starts_with("你是破甲人格。\n\n【系统指令"),
            "人格必须在 system 最前、紧跟其后的才是框架:{model}"
        );
        assert!(model.contains("只在 bash 里跑命令"));
        assert!(model.ends_with("只在 bash 里跑命令"));
        // 框架不得出现在人格之前。
        assert!(model.find("【系统指令").unwrap() > 0);
    }

    /// persona 独占档(整份系统提示只剩人格一段)不加任何框架。
    #[test]
    fn persona_only_assembly_has_no_frame() {
        let text = "你是破甲人格。\n**始终使用简体中文回复**,除非用户明确要求其他语言。";
        let assembly = assembly_of(vec![section(PERSONA_SECTION, text, SectionAudience::User)]);
        assert_eq!(render_model_system(&assembly), text);
    }

    /// 装配里没有 persona 段(异常路径)时维持旧行为:纪律仍被框架包住。
    #[test]
    fn missing_persona_keeps_the_frame() {
        let assembly = assembly_of(vec![section(
            "tool:bash",
            "只在 bash 里跑命令",
            SectionAudience::Model,
        )]);
        let model = render_model_system(&assembly);
        assert!(model.starts_with("【系统指令"));
        assert!(model.ends_with("只在 bash 里跑命令"));
    }
}
