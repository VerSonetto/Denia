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
}

pub fn schemas() -> Vec<ToolSchema> {
    let specs = [
        (
            "spawn_agent",
            "创建独立子代理会话，继承工作目录、权限与模型。默认后台返回 childId；完成后通知父会话。",
            json!({"prompt":{"type":"string"},"description":{"type":"string"},"provider":{"type":"string"},"model":{"type":"string"},"reasoning_effort":{"type":"string"},"run_in_background":{"type":"boolean"},"persona":{"type":"string"},"allowed_tools":{"type":"array","items":{"type":"string"}},"max_depth":{"type":"integer","minimum":1}}),
            vec!["prompt"],
        ),
        (
            "fork_agent",
            "以父会话已完成的历史为种子创建子代理；其余行为同 spawn_agent。",
            json!({"prompt":{"type":"string"},"description":{"type":"string"},"run_in_background":{"type":"boolean"},"persona":{"type":"string"},"allowed_tools":{"type":"array","items":{"type":"string"}},"max_depth":{"type":"integer","minimum":1}}),
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
            "技能按作用域分为全局技能与项目技能：全局技能位于用户数据目录 skills/（跨项目复用），项目技能位于项目 .denia/skills 等目录（随项目走），同名时项目技能优先，来源见返回的 source。先用 action=list 发现可用技能；action=load 按 name 加载后，SKILL.md 正文会自动注入后续上下文，无需再用文件读取工具读 SKILL.md；其余参考资料与脚本用 action=resource、name、path 按返回的 resourceBase 相对路径读取，禁止越界。",
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
