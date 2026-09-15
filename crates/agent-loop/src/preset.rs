//! Agent preset 的组装应用:按会话选中的 preset 收窄工具面、persona 与功能开关。
//!
//! denia 的 agent 组装由三件事决定——工具面、persona、功能开关,preset 就是
//! 这三者的声明(见 `denia_core::preset`)。本模块负责把它落到某个 step 的装
//! 配上:
//!
//! - features 先收窄:关掉的开关连带摘除对应工具与纪律段(段名到工具的
//!   映射见 [`crate::turn::section_tools`]),并关掉纯通道类功能(memory/
//!   compaction 的段级落点在本模块,通道与运行时落点各自联动);
//! - `tools` 白名单在 features 收窄之后继续收窄(交叠即收窄,绝不放大);
//! - persona 覆盖与子代理的角色提示词落在同一个段(`deployment:persona`),
//!   因此渲染期的变量插值(`{{cwd}}`/`{{model}}`)对 preset 同样有效;
//!   `persona_complete` 让 persona 独占整个系统提示。
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

/// 按谓词收窄工具面:工具 schema 与绑定它的纪律段同进退。
///
/// `context:`/`harness:` 段不是工具纪律,不受工具集影响;拿不到的工具,其
/// 纪律段不得留在系统提示里(否则模型读到不存在的工具的用法)。
fn retain_tools(assembly: &mut PromptAssembly, keep: impl Fn(&str) -> bool) {
    assembly.tools.retain(|schema| keep(&schema.name));
    assembly
        .sections
        .retain(|section| match section_tools(&section.name) {
            Some(tools) => tools.iter().any(|name| keep(name)),
            None => true,
        });
}

/// 工具白名单收窄:工具 schema 与其纪律段同步进退。
pub(crate) fn apply_tool_allowlist(assembly: &mut PromptAssembly, allowed: &[String]) {
    retain_tools(assembly, |name| allowed.iter().any(|a| a == name));
}

/// 把一份 preset 应用到本 step 的装配:persona 覆盖/独占,再 features 与
/// tools 白名单两级收窄。
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
    // persona 独占:系统提示只留 persona 一段。运行时上下文快照(contexts)
    // 保留——cwd/平台/权限语义是执行层与注入管线的依赖,极简 agent 同样需要。
    if preset.persona_complete {
        assembly
            .sections
            .retain(|section| section.name == PERSONA_SECTION);
    }
    // features 先收窄(工具 + 纪律段联动),tools 白名单在其上继续收窄。
    let excluded = preset.features.excluded_tools();
    if !excluded.is_empty() {
        retain_tools(assembly, |name| !excluded.contains(&name));
    }
    if let Some(tools) = &preset.tools {
        apply_tool_allowlist(assembly, tools);
    }
    // jobs 关闭:bash 的 run_in_background 参数一并摘除——参数仍在时模型
    // 后台跑一条命令会落到 jobs 注册表,与"没有后台任务功能"矛盾。
    if !preset.features.jobs
        && let Some(bash) = assembly
            .tools
            .iter_mut()
            .find(|schema| schema.name == "bash")
        && let Some(object) = bash.parameters.as_object_mut()
    {
        if let Some(properties) = object.get_mut("properties").and_then(|p| p.as_object_mut()) {
            properties.remove("run_in_background");
        }
        if let Some(required) = object.get_mut("required").and_then(|r| r.as_array_mut()) {
            required.retain(|value| value.as_str() != Some("run_in_background"));
        }
    }
    // compaction 关闭:摘上下文管理纪律段;自动压缩 gate 与手动 /compact 由
    // request 层按同一开关跳过。
    if !preset.features.compaction {
        assembly
            .sections
            .retain(|section| section.name != "harness:context-management");
    }
    // memory 关闭:摘 tool:memory 段。turn.rs 的段替换逻辑只替换"已存在的
    // 段",这里摘除后不会被加回;MEMORY.md 索引注入与 MemoryWrite 放行由
    // 注入管线与权限分类各自按同一开关联动。
    if !preset.features.memory {
        assembly
            .sections
            .retain(|section| section.name != "tool:memory");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::preset::{PresetFeatures, PresetTrust};
    use denia_system_prompt::{PromptSection, PromptText, SectionAudience};

    fn schema(name: &str) -> denia_core::tool::ToolSchema {
        denia_core::tool::ToolSchema {
            name: name.to_string(),
            description: format!("{name} 工具"),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn bash_schema() -> denia_core::tool::ToolSchema {
        denia_core::tool::ToolSchema {
            name: "bash".to_string(),
            description: "bash 工具".to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"command": {"type": "string"}, "run_in_background": {"type": "boolean"}},
                "required": ["command"]
            }),
        }
    }

    fn section(name: &str) -> denia_system_prompt::AssembledSection {
        denia_system_prompt::AssembledSection {
            name: name.to_string(),
            text: format!("{name} 正文"),
            audience: SectionAudience::Model,
        }
    }

    fn assembly() -> PromptAssembly {
        PromptAssembly {
            sections: vec![
                section("deployment:persona"),
                section("tool:bash"),
                section("tool:goal"),
                section("tool:memory"),
                section("context:file-reference"),
                section("harness:context-management"),
                section("harness:runtime"),
            ],
            contexts: Vec::new(),
            tools: vec![
                bash_schema(),
                schema("read_file"),
                schema("write_file"),
                schema("get_goal"),
                schema("update_goal"),
                schema("exit_plan"),
            ],
            variables: Default::default(),
        }
    }

    fn preset(tools: Option<Vec<&str>>, persona: Option<&str>) -> AgentPreset {
        AgentPreset {
            id: "test".to_string(),
            name: "测试".to_string(),
            description: String::new(),
            trust: PresetTrust::User,
            tools: tools.map(|names| names.into_iter().map(str::to_string).collect()),
            persona: persona.map(str::to_string),
            persona_complete: false,
            features: PresetFeatures::default(),
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
            vec![
                "deployment:persona",
                "tool:bash",
                "context:file-reference",
                "harness:context-management",
                "harness:runtime"
            ],
            "非工具段必须保留,拿不到的工具的纪律段必须移除"
        );
    }

    #[test]
    fn full_toolset_preset_only_touches_persona() {
        let mut assembly = assembly();
        apply_preset(&mut assembly, &preset(None, Some("自定义 {{cwd}}")));
        assert_eq!(assembly.tools.len(), 6);
        let persona = &assembly.sections[0];
        assert!(persona.text.starts_with("自定义 {{cwd}}"));
        assert!(persona.text.contains("简体中文"));
        assert_eq!(assembly.sections.len(), 7);
    }

    #[test]
    fn blank_persona_leaves_the_deployment_persona_alone() {
        let mut assembly = assembly();
        apply_preset(&mut assembly, &preset(None, Some("   ")));
        assert_eq!(assembly.sections[0].text, "deployment:persona 正文");
    }

    #[test]
    fn persona_complete_leaves_only_the_persona_section() {
        let mut assembly = assembly();
        let mut p = preset(None, Some("一句话人格"));
        p.persona_complete = true;
        apply_preset(&mut assembly, &p);
        assert_eq!(assembly.sections.len(), 1);
        assert_eq!(assembly.sections[0].name, PERSONA_SECTION);
        assert!(assembly.sections[0].text.starts_with("一句话人格"));
        // 工具 schema 保留:工具仍可用,只是没有指引(dsh complete 语义)。
        assert_eq!(assembly.tools.len(), 6);
    }

    #[test]
    fn goal_feature_off_removes_goal_tools_and_their_section() {
        let mut assembly = assembly();
        let mut p = preset(None, None);
        p.features.goal = false;
        apply_preset(&mut assembly, &p);
        let tools: Vec<&str> = assembly.tools.iter().map(|s| s.name.as_str()).collect();
        assert!(!tools.contains(&"get_goal"));
        assert!(!tools.contains(&"update_goal"));
        assert!(tools.contains(&"bash"));
        let sections: Vec<&str> = assembly.sections.iter().map(|s| s.name.as_str()).collect();
        assert!(!sections.contains(&"tool:goal"));
        // 非目标段不受影响。
        assert!(sections.contains(&"tool:bash"));
    }

    #[test]
    fn plan_mode_feature_off_removes_exit_plan() {
        let mut assembly = assembly();
        let mut p = preset(None, None);
        p.features.plan_mode = false;
        apply_preset(&mut assembly, &p);
        assert!(!assembly.tools.iter().any(|s| s.name == "exit_plan"));
        assert!(!assembly.sections.iter().any(|s| s.name == "tool:plan"));
    }

    #[test]
    fn compaction_feature_off_removes_context_management_section() {
        let mut assembly = assembly();
        let mut p = preset(None, None);
        p.features.compaction = false;
        apply_preset(&mut assembly, &p);
        assert!(!assembly
            .sections
            .iter()
            .any(|s| s.name == "harness:context-management"));
        assert!(assembly.sections.iter().any(|s| s.name == "tool:bash"));
    }

    #[test]
    fn memory_feature_off_removes_memory_section() {
        let mut assembly = assembly();
        let mut p = preset(None, None);
        p.features.memory = false;
        apply_preset(&mut assembly, &p);
        assert!(!assembly.sections.iter().any(|s| s.name == "tool:memory"));
        // 写工具本身保留:记忆关的是通道与放行,不是文件编辑能力。
        assert!(assembly.tools.iter().any(|s| s.name == "write_file"));
    }

    #[test]
    fn jobs_feature_off_strips_run_in_background_from_bash() {
        let mut assembly = assembly();
        let mut p = preset(None, None);
        p.features.jobs = false;
        apply_preset(&mut assembly, &p);
        let bash = assembly.tools.iter().find(|s| s.name == "bash").unwrap();
        let properties = bash.parameters.get("properties").unwrap();
        assert!(properties.get("run_in_background").is_none());
        assert!(properties.get("command").is_some());
        let required = bash.parameters.get("required").unwrap().as_array().unwrap();
        assert!(!required.iter().any(|v| v.as_str() == Some("run_in_background")));
    }

    #[test]
    fn features_and_tools_whitelist_narrow_together() {
        let mut assembly = assembly();
        let mut p = preset(Some(vec!["bash", "get_goal"]), None);
        p.features.goal = false;
        apply_preset(&mut assembly, &p);
        let tools: Vec<&str> = assembly.tools.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            tools,
            vec!["bash"],
            "features 先摘 goal,白名单里的 get_goal 不会让它复活"
        );
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
