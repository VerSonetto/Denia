//! `create_preset` 工具:创造模式的落盘入口。
//!
//! 专用于在用户 preset 目录创建一份新组装(区别于 `copy` 复制创作:复制
//! 只搬运既有目录,本工具接受模型按 ask 收集结果组装的规范)。写入路径由
//! 服务端固定(preset 根 + 合法 id),不受模型操纵;权限上按独立类别
//! `PresetCreate` 判定。工具纪律(何时创建、与 ask 的配合、收尾义务)在
//! `tool:preset` 系统提示段,description 只写参数机制。

use std::sync::Arc;

use async_trait::async_trait;
use denia_core::preset::PresetSpec;
use denia_tools::{Tool, ToolContext, ToolOutput};

use crate::agent_presets::PresetStore;

const DESCRIPTION: &str = "在用户的 agent preset 目录创建一份新的组装(组装决定会话可用哪些工具、以什么角色工作)。创建成功后立即进入名册:设置里的 Agent 预设页与新会话的选择器都能看到它,新会话即可选用。参数:id 是目录名(小写字母、数字与连字符,字母或数字开头);tools 省略 = 全量工具集;features 里省略的键 = 开启,只写要关闭的键(可用键:agentsMd, memory, compaction, goal, skills, subagents, jobs, browser, ask, planMode);persona 省略 = 沿用部署 persona。已有同名 id 会被拒绝,不会覆盖。调用前先与用户确认组装摘要。";

/// 工具 schema(静态):注册进注册表(可执行)与系统提示词的 tools
/// provider(模型可见)必须用同一份,两处同步是 server 部署路径的义务。
pub fn schema() -> denia_core::tool::ToolSchema {
    denia_core::tool::ToolSchema {
        name: "create_preset".to_string(),
        description: DESCRIPTION.to_string(),
        parameters: serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["id", "name"],
            "properties": {
                "id": {
                    "type": "string",
                    "description": "新组装的 id,同时是目录名;小写字母/数字开头,只能含小写字母、数字与连字符。"
                },
                "name": {
                    "type": "string",
                    "description": "显示名(名册与选择器展示),用中文。"
                },
                "description": {
                    "type": "string",
                    "description": "一句话说明该组装的用途(名册描述),用中文。"
                },
                "persona": {
                    "type": "string",
                    "description": "该组装的 persona 正文;省略 = 沿用部署 persona。"
                },
                "personaComplete": {
                    "type": "boolean",
                    "description": "true = persona 独占整个系统提示(极简形态,工具仍可用但没有指引);缺省 false。"
                },
                "tools": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "工具白名单(如 bash, read_file, write_file, edit, glob, grep, skill, spawn_agent, job_start, browser, ask, get_goal, exit_plan);省略 = 全量工具集。"
                },
                "features": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "agentsMd": {"type": "boolean", "description": "AGENTS.md 工作区指令注入(缺省开启)。"},
                        "memory": {"type": "boolean", "description": "项目记忆(缺省开启)。"},
                        "compaction": {"type": "boolean", "description": "上下文压缩(缺省开启)。"},
                        "goal": {"type": "boolean", "description": "目标模式(缺省开启)。"},
                        "skills": {"type": "boolean", "description": "技能(缺省开启)。"},
                        "subagents": {"type": "boolean", "description": "子代理(缺省开启)。"},
                        "jobs": {"type": "boolean", "description": "后台任务(缺省开启)。"},
                        "browser": {"type": "boolean", "description": "浏览器(缺省开启)。"},
                        "ask": {"type": "boolean", "description": "提问工具(缺省开启)。"},
                        "planMode": {"type": "boolean", "description": "计划模式(缺省开启)。"}
                    },
                    "description": "功能开关;省略的键 = 开启,只写要关闭的键。"
                }
            }
        }),
    }
}

/// 组装创作工具:持有名册存储,执行即落盘 + 热刷新名册。
pub struct CreatePresetTool {
    schema: denia_core::tool::ToolSchema,
    store: Arc<PresetStore>,
}

impl CreatePresetTool {
    pub fn new(store: Arc<PresetStore>) -> Self {
        Self {
            schema: schema(),
            store,
        }
    }
}

#[async_trait]
impl Tool for CreatePresetTool {
    fn schema(&self) -> &denia_core::tool::ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, _ctx: &ToolContext) -> ToolOutput {
        let spec: PresetSpec = match denia_tools::parse_args_lenient(arguments) {
            Ok(spec) => spec,
            Err(error) => {
                return ToolOutput::error(format!("create_preset 参数解析失败:{error}。请检查 JSON 是否完整(必填:id、name)。"))
            }
        };
        let store = Arc::clone(&self.store);
        let spec_clone = spec.clone();
        // 落盘是阻塞 fs 操作,不占异步运行时。
        let result = tokio::task::spawn_blocking(move || store.author(&spec_clone))
            .await
            .map_err(|error| format!("create_preset 执行失败:{error}"));
        match result {
            Ok(Ok(row)) => ToolOutput::text(format!(
                "已创建组装「{}」(id: {}),目录:{}。\n它已进入名册:设置 → Agent 预设,或新会话的组装选择器里即可选用;微调可直接编辑该目录下的 preset.yml,改动对后续步骤与新会话即时生效。",
                row.name, row.id,
                row.path.as_deref().unwrap_or("<未知目录>")
            )),
            Ok(Err(reason)) => ToolOutput::error(format!("创建 preset 被拒绝:{reason}。请按原因调整(如换一个 id)后重试,不要静默换名重试。")),
            Err(error) => ToolOutput::error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::preset::PresetFeatures;
    use denia_settings::SettingsStore;

    fn temp_home() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-preset-tool-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn open_store(home: &std::path::Path) -> Arc<PresetStore> {
        let settings = Arc::new(SettingsStore::open(home).unwrap());
        settings
            .register(
                crate::agent_presets::SETTINGS_NS,
                denia_settings::NamespaceSpec {
                    defaults: serde_json::json!({
                        "default": denia_core::preset::DEFAULT_PRESET_ID,
                        "modeSelectionEnabled": true,
                    }),
                    validate: |value| Ok(value),
                    secrets: &[],
                    applies: denia_settings::Applies::Live,
                },
                serde_json::json!({}),
            )
            .unwrap();
        PresetStore::load(home, settings)
    }

    #[tokio::test]
    async fn creates_a_preset_and_reports_its_directory() {
        let home = temp_home();
        let store = open_store(&home);
        store.set_known_tools(["bash".to_string(), "read_file".to_string()]);
        let tool = CreatePresetTool::new(Arc::clone(&store));
        let output = tool
            .execute(
                r#"{"id": "writer", "name": "写手", "description": "只写不删",
                    "persona": "你是文字工作者。", "tools": ["bash", "read_file"],
                    "features": {"memory": false}}"#,
                &test_ctx(),
            )
            .await;
        assert!(!output.is_error, "{:?}", output.content);
        assert!(output.content.contains("writer"));
        let preset = store.resolve("writer").expect("创建后名册立即可解析");
        assert_eq!(preset.name, "写手");
        assert!(!preset.features.memory);
        assert!(preset.features.goal, "未声明的键保持开启");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn rejects_occupied_or_invalid_ids_with_readable_reasons() {
        let home = temp_home();
        let store = open_store(&home);
        let tool = CreatePresetTool::new(Arc::clone(&store));
        let output = tool
            .execute(r#"{"id": "standard", "name": "冒充标准"}"#, &test_ctx())
            .await;
        assert!(output.is_error);
        assert!(output.content.contains("已被占用"));
        let output = tool
            .execute(r#"{"id": "../escape", "name": "越狱"}"#, &test_ctx())
            .await;
        assert!(output.is_error);
        assert!(output.content.contains("非法的 preset id"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[tokio::test]
    async fn rejects_unknown_tool_names_once_known_tools_are_injected() {
        let home = temp_home();
        let store = open_store(&home);
        store.set_known_tools(["bash".to_string()]);
        let tool = CreatePresetTool::new(Arc::clone(&store));
        let output = tool
            .execute(
                r#"{"id": "ghost", "name": "幽灵", "tools": ["bash", "not_a_tool"]}"#,
                &test_ctx(),
            )
            .await;
        assert!(output.is_error);
        assert!(output.content.contains("not_a_tool"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn features_survive_lenient_parsing_round_trip() {
        let spec: PresetSpec = denia_tools::parse_args_lenient(
            r#"{"id":"a","name":"A","features":{"goal":false}} 尾部垃圾"#,
        )
        .unwrap();
        assert!(!spec.features.goal);
        assert_eq!(spec.features, PresetFeatures {
            goal: false,
            ..PresetFeatures::default()
        });
    }

    fn test_ctx() -> ToolContext {
        ToolContext {
            session_id: None,
            selection: None,
            cwd: std::env::temp_dir(),
            cancel: tokio_util::sync::CancellationToken::new(),
            confined: false,
            vision_supported: false,
            permission_mode: denia_core::session::PermissionMode::Full,
            emit_event: None,
            file_history: None,
            ask: None,
            call_id: None,
            goal_reader: None,
            read_state: None,
        }
    }
}
