//! 宿主能力接口；工具层不依赖服务器或会话驱动器。
use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use serde_json::{Value, json};
use std::sync::Arc;

#[async_trait]
pub trait AgentRuntime: Send + Sync {
    async fn execute(&self, name: &str, args: Value, ctx: &ToolContext) -> Result<Value, String>;
    async fn context(&self, session: &str, cwd: &std::path::Path) -> Result<Vec<String>, String>;
    async fn drain(&self, session: &str) -> Result<Vec<String>, String>;

    /// 技能目录条目 (名称, 描述)，仅 model_invocable；描述已按配置截断。
    /// 目录注入与幂等去重由会话驱动器负责，实现方只负责发现与过滤。
    async fn skill_catalog(
        &self,
        session: &str,
        cwd: &std::path::Path,
    ) -> Result<Vec<(String, String)>, String> {
        let _ = (session, cwd);
        Ok(Vec::new())
    }

    /// 用户 `/技能名` 手势加载：校验 user_invocable 后返回 (来源, 正文)；
    /// 未知技能或不允许用户调用的技能返回 None（手势降级为普通文本）。
    async fn user_skill(
        &self,
        session: &str,
        name: &str,
        cwd: &std::path::Path,
    ) -> Result<Option<(String, String)>, String> {
        let _ = (session, name, cwd);
        Ok(None)
    }

    /// 工作区指令（AGENTS.md）注入文本；None 表示当前无需注入。
    /// 发现、单文件源上限、总预算截断与替换语义都在实现方完成；
    /// `previous` 是日志中最后一条本通道注入文本（内容未变时实现方必须
    /// 返回 None，引导语由正文是否变化决定）。
    async fn workspace_instructions(
        &self,
        cwd: &std::path::Path,
        touched: &[std::path::PathBuf],
        previous: Option<&str>,
    ) -> Result<Option<String>, String> {
        let _ = (cwd, touched, previous);
        Ok(None)
    }

    /// 当前工作区的项目记忆目录;None = 项目记忆未启用。
    /// 权限层(记忆写放行)、系统提示纪律段的进退、索引注入通道共用这
    /// 一个判定源,保证读写两端与提示文案同步开停。
    fn memory_root_for(&self, cwd: &std::path::Path) -> Option<std::path::PathBuf> {
        let _ = cwd;
        None
    }

    /// 项目记忆索引(MEMORY.md)注入文本;None = 未启用。记忆启用时实现方
    /// 必须给出注入(空桶给目录路径,有索引给全文)——路径是主代理读写
    /// 记忆的前提。幂等去重由驱动器按通道基准承担(内容不变不重发)。
    async fn project_memory_index(
        &self,
        cwd: &std::path::Path,
    ) -> Result<Option<String>, String> {
        let _ = cwd;
        Ok(None)
    }
}

pub fn schemas() -> Vec<ToolSchema> {
    let specs = [
        (
            "spawn_agent",
            "创建独立子代理会话，继承工作目录、权限与模型。默认后台返回 childId；完成后通知父会话。子代理默认只有只读工具（read_file/ls/glob/grep/skill/browser），写文件、命令、提问、再委派都留在父代理；allowed_tools 只能在这个集合内缩小。",
            json!({"prompt":{"type":"string"},"description":{"type":"string"},"provider":{"type":"string"},"model":{"type":"string"},"reasoning_effort":{"type":"string"},"run_in_background":{"type":"boolean"},"persona":{"type":"string"},"allowed_tools":{"type":"array","items":{"type":"string"},"description":"子代理可用工具；缺省 read_file/ls/glob/grep/skill/browser，只能缩小不能扩大。"},"max_depth":{"type":"integer","minimum":1}}),
            vec!["prompt"],
        ),
        (
            "fork_agent",
            "以父会话已完成的历史为种子创建子代理；其余行为同 spawn_agent。",
            json!({"prompt":{"type":"string"},"description":{"type":"string"},"run_in_background":{"type":"boolean"},"persona":{"type":"string"},"allowed_tools":{"type":"array","items":{"type":"string"},"description":"子代理可用工具；缺省 read_file/ls/glob/grep/skill，只能缩小不能扩大。"},"max_depth":{"type":"integer","minimum":1}}),
            vec!["prompt"],
        ),
        (
            "send_message",
            "向直接父代理或直接子代理发消息；运行中在下一步接收，空闲时恢复会话。",
            json!({"target":{"type":"string"},"message":{"type":"string"}}),
            vec!["target", "message"],
        ),
        (
            "interrupt_agent",
            "中断后代代理当前轮次，保留会话以便继续。",
            json!({"target":{"type":"string"}}),
            vec!["target"],
        ),
        (
            "list_agents",
            "列出当前会话的子代理及后代的持久化身份和运行状态。",
            json!({}),
            vec![],
        ),
        (
            "wait_agent",
            "等待子代理当前执行完成；超时或停止等待不会取消后台代理。",
            json!({"target":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1}}),
            vec!["target"],
        ),
        (
            "job_start",
            "在会话工作目录启动后台命令，立即返回 job id。用 job_output 领取输出，job_kill 停止。",
            json!({"command":{"type":"string"},"label":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1}}),
            vec!["command"],
        ),
        (
            "job_list",
            "列出当前会话拥有的后台任务。",
            json!({}),
            vec![],
        ),
        (
            "job_output",
            "读取任务增量输出；结束后重复读取返回完整保留结果。wait 可等待结束。",
            json!({"id":{"type":"string"},"wait":{"type":"boolean"},"timeout_ms":{"type":"integer","minimum":1}}),
            vec!["id"],
        ),
        (
            "job_kill",
            "请求停止当前会话的后台任务。重复停止是幂等操作。",
            json!({"id":{"type":"string"}}),
            vec!["id"],
        ),
        (
            "skill",
            "技能按作用域分为全局技能与项目技能：全局技能位于用户数据目录 skills/（跨项目复用），项目技能位于项目 .denia/skills 等目录（随项目走），同名时项目技能优先，来源见返回的 source。可用技能以 <available_skills> 目录注入消息提供，先从目录取准确技能名；action=load 按 name 加载，SKILL.md 全文直接随本工具结果返回（加载一次即可，目录只含摘要，未加载前不得凭摘要推断技能内容）；其余参考资料与脚本用 action=resource、name、path 按返回的 resourceBase 相对路径读取，禁止越界。",
            json!({"action":{"type":"string","enum":["list","load","resource"]},"name":{"type":"string"},"path":{"type":"string"}}),
            vec!["action"],
        ),
    ];
    specs.into_iter().map(|(name, description, properties, required)| ToolSchema {
        name: name.into(), description: description.into(), parameters: json!({"type":"object","properties":properties,"required":required,"additionalProperties":false}),
    }).collect()
}

pub fn register(registry: &mut crate::ToolRegistry, runtime: Arc<dyn AgentRuntime>) {
    for schema in schemas() {
        registry.register(Arc::new(CapabilityTool {
            schema,
            runtime: runtime.clone(),
        }));
    }
}
struct CapabilityTool {
    schema: ToolSchema,
    runtime: Arc<dyn AgentRuntime>,
}
#[async_trait]
impl Tool for CapabilityTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }
    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let result = match crate::parse_args_lenient(arguments) {
            Ok(args) => match validate_arguments(&self.schema, &args) {
                Ok(()) => self.runtime.execute(&self.schema.name, args, ctx).await,
                Err(e) => Err(e),
            },
            Err(error) => Err(format!("参数无效：{error}")),
        };
        match result {
            Ok(value) => ToolOutput {
                content: value.to_string(),
                is_error: false,
            },
            Err(content) => ToolOutput {
                content,
                is_error: true,
            },
        }
    }
}

fn validate_arguments(schema: &ToolSchema, args: &Value) -> Result<(), String> {
    let values = args.as_object().ok_or("工具参数必须为对象")?;
    let properties = schema.parameters["properties"].as_object().unwrap();
    for (key, value) in values {
        let prop = properties
            .get(key)
            .ok_or_else(|| format!("工具不支持参数：{key}"))?;
        let valid = match prop["type"].as_str() {
            Some("string") => value.is_string(),
            Some("boolean") => value.is_boolean(),
            Some("integer") => value
                .as_u64()
                .is_some_and(|v| v >= prop["minimum"].as_u64().unwrap_or(0)),
            Some("array") => value
                .as_array()
                .is_some_and(|a| a.iter().all(Value::is_string)),
            _ => true,
        };
        if !valid {
            return Err(format!("参数类型或范围无效：{key}"));
        }
        if prop["enum"]
            .as_array()
            .is_some_and(|choices| !choices.contains(value))
        {
            return Err(format!("参数值不受支持：{key}"));
        }
    }
    for key in schema.parameters["required"].as_array().unwrap() {
        if !values.contains_key(key.as_str().unwrap()) {
            return Err(format!("缺少参数：{key}"));
        }
    }
    Ok(())
}
