//! Agent preset 的组装应用:按会话选中的 preset 收窄工具面与 persona。
//!
//! denia 的 agent 组装由两件事决定——工具面与 persona,preset 就是这两者的
//! 声明(见 `denia_core::preset`)。本模块负责把它落到某个 step 的装配上:
//!
//! - 工具白名单与子代理的 `allowed_tools` 走同一条收窄路径,纪律段与工具
//!   同进退(段名到工具的映射见 [`crate::turn::section_tools`]);
//! - persona 覆盖与子代理的角色提示词落在同一个段(`deployment:persona`),
//!   因此渲染期的变量插值(`{{cwd}}`/`{{model}}`)对 preset 同样有效。
//!
//! preset 缺失(文件被删或损坏)不中断会话:回退到部署默认组装,由 server
//! 侧在名册里把该行标成 broken,让人看得见该修什么。

use denia_core::preset::{AgentPreset, DEFAULT_PRESET_ID};
use denia_system_prompt::{PERSONA_SECTION, PromptAssembly};

use crate::turn::section_tools;

/// preset 名册来源:server 按 id 提供组装声明,driver 在每个 step 装配时查。
pub trait AgentPresetSource: Send + Sync {
    /// 解析一个 preset id;未知名(已被删除或文件损坏)返回 `None`。
    fn resolve(&self, id: &str) -> Option<AgentPreset>;

    /// 部署默认 id:会话未指定 preset 时使用。
    fn default_id(&self) -> String {
        DEFAULT_PRESET_ID.to_string()
    }
}

/// 工具白名单收窄:工具 schema 与其纪律段同步进退。
///
/// `context:`/`harness:` 段不是工具纪律,不受工具集影响;拿不到的工具,其
/// 纪律段不得留在系统提示里(否则模型读到不存在的工具的用法)。
pub(crate) fn apply_tool_allowlist(assembly: &mut PromptAssembly, allowed: &[String]) {
    assembly
        .tools
        .retain(|schema| allowed.iter().any(|name| name == &schema.name));
    assembly
        .sections
        .retain(|section| match section_tools(&section.name) {
            Some(tools) => tools
                .iter()
                .any(|name| allowed.iter().any(|allowed| allowed == name)),
            None => true,
        });
}

/// 把一份 preset 应用到本 step 的装配:先 persona 覆盖,再工具白名单。
///
/// 与子代理的收窄顺序一致:preset 先收窄,子代理的 `allowed_tools` 在其后
/// 继续收窄(继承即收窄,绝不放大)。
pub(crate) fn apply_preset(assembly: &mut PromptAssembly, preset: &AgentPreset) {
    if let Some(persona) = preset.persona.as_deref().filter(|text| !text.trim().is_empty())
        && let Some(section) = assembly
            .sections
            .iter_mut()
            .find(|section| section.name == PERSONA_SECTION)
    {
        section.text = format!("{persona}\n始终使用简体中文回复，除非用户明确要求其他语言。");
    }
    if let Some(tools) = &preset.tools {
        apply_tool_allowlist(assembly, tools);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_system_prompt::{PromptSection, PromptText, SectionAudience};

    fn schema(name: &str) -> denia_core::tool::ToolSchema {
        denia_core::tool::ToolSchema {
            name: name.to_string(),
            description: format!("{name} 工具"),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn assembly() -> PromptAssembly {
        PromptAssembly {
            sections: vec![
                denia_system_prompt::AssembledSection {
                    name: "deployment:persona".to_string(),
                    text: "出厂 persona".to_string(),
                    audience: SectionAudience::User,
                },
                denia_system_prompt::AssembledSection {
                    name: "tool:bash".to_string(),
                    text: "bash 纪律".to_string(),
                    audience: SectionAudience::Model,
                },
                denia_system_prompt::AssembledSection {
                    name: "tool:read".to_string(),
                    text: "read 纪律".to_string(),
                    audience: SectionAudience::Model,
                },
                denia_system_prompt::AssembledSection {
                    name: "harness:runtime".to_string(),
                    text: "运行时事实".to_string(),
                    audience: SectionAudience::Context,
                },
            ],
            contexts: Vec::new(),
            tools: vec![schema("bash"), schema("read_file"), schema("write_file")],
            variables: Default::default(),
        }
    }

    fn preset(tools: Option<Vec<&str>>, persona: Option<&str>) -> AgentPreset {
        AgentPreset {
            id: "test".to_string(),
            name: "测试".to_string(),
            description: String::new(),
            trust: denia_core::preset::PresetTrust::User,
            tools: tools.map(|names| names.into_iter().map(str::to_string).collect()),
            persona: persona.map(str::to_string),
            path: None,
        }
    }

    #[test]
    fn allowlist_keeps_tools_and_their_sections_in_step() {
        let mut assembly = assembly();
        apply_preset(&mut assembly, &preset(Some(vec!["bash"]), None));
        let tools: Vec<&str> = assembly.tools.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(tools, vec!["bash"]);
        let sections: Vec<&str> = assembly.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            sections,
            vec!["deployment:persona", "tool:bash", "harness:runtime"],
            "非工具段必须保留,拿不到的工具的纪律段必须移除"
        );
    }

    #[test]
    fn full_toolset_preset_only_touches_persona() {
        let mut assembly = assembly();
        apply_preset(&mut assembly, &preset(None, Some("自定义 {{cwd}}")));
        assert_eq!(assembly.tools.len(), 3);
        let persona = &assembly.sections[0];
        assert!(persona.text.starts_with("自定义 {{cwd}}"));
        assert!(persona.text.contains("简体中文"));
        assert_eq!(assembly.sections.len(), 4);
    }

    #[test]
    fn blank_persona_leaves_the_deployment_persona_alone() {
        let mut assembly = assembly();
        apply_preset(&mut assembly, &preset(None, Some("   ")));
        assert_eq!(assembly.sections[0].text, "出厂 persona");
    }

    #[test]
    fn every_shipped_tool_section_maps_to_tools() {
        // 段名到工具的映射是收窄的唯一依据:新增 `tool:*` 纪律段而不登记
        // 映射,该段在预设收窄后会继续留在提示词里,模型读到不存在的工具。
        let (mut prompt, _registry) = denia_tools::default_shipped();
        prompt
            .section(PromptSection {
                name: "tool:agents".to_string(),
                order: 900,
                text: PromptText::Static("能力纪律".to_string()),
                complete: false,
                audience: SectionAudience::Model,
            })
            .unwrap();
        prompt
            .section(PromptSection {
                name: "tool:browser".to_string(),
                order: 901,
                text: PromptText::Static("浏览器纪律".to_string()),
                complete: false,
                audience: SectionAudience::Model,
            })
            .unwrap();
        let assembly = prompt
            .assemble(&denia_system_prompt::AssembleContext::default())
            .expect("assembly is valid");
        let unmapped: Vec<&str> = assembly
            .sections
            .iter()
            .filter(|section| section.name.starts_with("tool:"))
            .map(|section| section.name.as_str())
            .filter(|name| section_tools(name).is_none())
            .collect();
        assert!(unmapped.is_empty(), "未登记工具映射的纪律段:{unmapped:?}");
    }

    #[test]
    fn tool_schema_restriction_keeps_unknown_names_out() {
        let mut assembly = assembly();
        apply_tool_allowlist(
            &mut assembly,
            &["read_file".to_string(), "nope".to_string()],
        );
        let tools: Vec<&str> = assembly.tools.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(tools, vec!["read_file"]);
    }
}
