//! 宿主能力接口；工具层不依赖服务器或会话驱动器。
use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use serde_json::{Value, json};
use std::sync::Arc;

#[async_trait]
pub trait AgentRuntime: Send + Sync {
    async fn execute(&self, name: &str, args: Value, ctx: &ToolContext) -> Result<Value, String> {
        let command = crate::runtime_command::RuntimeCommand::parse(name, args)?;
        self.execute_command(command, ctx).await
    }
    async fn execute_command(
        &self,
        command: crate::runtime_command::RuntimeCommand,
        ctx: &ToolContext,
    ) -> Result<Value, String>;
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
    ///
    /// `session` 是**宿主传入**的会话身份：实现方据此决定作用域——root 会话
    /// 用"全局 + 项目"，子代理只用项目级。模型与 profile 都不能传
    /// `includeGlobal=true`，因此这里没有可被绕过的开关参数。
    async fn workspace_instructions(
        &self,
        session: &str,
        cwd: &std::path::Path,
        touched: &[std::path::PathBuf],
        previous: Option<&str>,
    ) -> Result<Option<String>, String> {
        let _ = (session, cwd, touched, previous);
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
    async fn project_memory_index(&self, cwd: &std::path::Path) -> Result<Option<String>, String> {
        let _ = cwd;
        Ok(None)
    }

    /// 子代理运行快照（角色正文 + 冻结授权）。
    ///
    /// 宿主负责读取与校验：引用缺失或 hash 不一致必须返回 `Err`，装配层据此
    /// 拒绝启动，**不**回退到默认角色或更宽的工具面。返回 `Ok(None)` 表示该
    /// 会话不是子代理（或部署没有定义管理能力，按旧描述符处理）。
    async fn subagent_prompt(&self, session: &str) -> Result<Option<SubagentPrompt>, String> {
        let _ = session;
        Ok(None)
    }

    /// 父代理可见的子代理派遣目录（只含限定 id、名称、描述与摘要）。
    /// 子代理永不注入；父不能派遣时也返回 `None`。
    async fn subagent_catalog(&self, session: &str) -> Result<Option<String>, String> {
        let _ = session;
        Ok(None)
    }

    /// 某会话的**冻结**工具授权。
    ///
    /// `None` = 该会话没有被定义收窄（普通会话），调用方按"部署全集"处理。
    /// `Some(list)` = 子代理派遣时冻结的显式列表（旧描述符由宿主按历史只读
    /// 上限保守解释）。MCP 目录等"按会话授权的目录投影"必须用它，禁止把
    /// 未授权的工具列给模型（计划 6.4：目录、首次加载、真实执行三处一致）。
    fn granted_tools(&self, session: &str) -> Option<Vec<String>> {
        let _ = session;
        None
    }
}

/// 子代理运行快照的模型可见部分。
#[derive(Debug, Clone, Default)]
pub struct SubagentPrompt {
    /// 定义显示名（结果汇报与身份说明用）。
    pub name: Option<String>,
    /// 角色补充提示（定义 instructions）；追加到父基础提示之后。
    pub instructions: String,
    /// 派遣时冻结的有效工具名；空表示零业务工具。
    pub effective_tools: Vec<String>,
    /// `PermissionCeiling::as_str()`；`None` = 未记录（旧快照）。
    pub permission_ceiling: Option<String>,
    /// 父 preset 的 persona 快照：preset 文件后来被删也不回退部署默认人格。
    pub parent_preset_persona: Option<String>,
    /// 旧描述符（迁移前）：授权是保守历史集合，只能查看与受限继续。
    pub legacy: bool,
}

pub fn schemas() -> Vec<ToolSchema> {
    let specs = [
        (
            "spawn_agent",
            "创建独立子代理会话：继承工作目录、权限上限、父系统提示基础与模型默认值；角色提示是追加，不替换父 persona。始终后台执行，立即返回 childId 与 pending，完成结果自动通知父会话；pending 后不要轮询或重复等待，可以继续独立工作。派遣方式三选一：profile_id 用已保存的定义；inline 在调用时给出临时定义；两者都省略时使用默认的 develop 定义。profile_id 与 inline 互斥，也不与旧参数 persona/allowed_tools 混用。子代理禁止派遣子代理，也不能使用宿主配置与会话主控工具，且不会自动继承全局 AGENTS.md。",
            json!({
                "prompt": {"type": "string", "description": "交给子代理的完整任务说明（自包含：目标、范围、验收条件）。"},
                "description": {"type": "string", "description": "一句话说明这次派遣做什么，用于父会话列表与结果通知。"},
                "profile_id": {"type": "string", "description": "已保存定义的限定 id，如 builtin:explore / builtin:develop / builtin:verify / user:<id> / project:<id>；可用候选见系统提示里的子代理目录。"},
                "inline": {
                    "type": "object",
                    "description": "调用时创建的临时子代理定义（不落盘、不进管理目录）。",
                    "properties": {
                        "name": {"type": "string"},
                        "description": {"type": "string", "description": "用途与选择时机。"},
                        "instructions": {"type": "string", "description": "角色补充提示：追加在父系统提示之后，不替换父 persona。"},
                        "tools": {"type": "object", "description": "{mode:\"inherit\"} 继承父可授予工具；或 {mode:\"allowlist\",names:[...]} 显式列表（空数组=无工具，纯推理任务）。"},
                        "model": {"type": "object", "description": "{mode:\"inherit\"} 或 {mode:\"explicit\",selection:{provider,model,reasoningEffort}}。"},
                        "permissionCeiling": {"type": "string", "enum": ["inherit", "read-only"], "description": "只允许收窄：read-only 强制禁止写与命令。"}
                    },
                    "required": ["name", "description", "tools"],
                    "additionalProperties": false
                },
                "provider": {"type": "string"},
                "model": {"type": "string"},
                "reasoning_effort": {"type": "string"},
                "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "已废弃：显式工具列表。请改用 profile_id 或 inline.tools；与 profile_id/inline 同时出现会被拒绝。"},
                "run_in_background": {"type": "boolean", "description": "已废弃：无论 true 或 false 都立即返回并在后台执行。"},
                "persona": {"type": "string", "description": "已废弃：角色补充提示。请改用 profile_id 或 inline.instructions。"}
            }),
            vec!["prompt"],
        ),
        (
            "fork_agent",
            "以父会话已闭合的历史为种子创建子代理（工具调用与结果成对，不含全局指令与旧摘要注入）；沿用父模型（跨模型 fork 会被拒绝），其余行为同 spawn_agent。始终后台执行并立即返回 pending，完成结果自动通知父会话。",
            json!({
                "prompt": {"type": "string", "description": "交给子代理的完整任务说明。"},
                "description": {"type": "string"},
                "profile_id": {"type": "string", "description": "已保存定义的限定 id；省略时使用默认的 develop 定义。"},
                "inline": {
                    "type": "object",
                    "description": "调用时创建的临时子代理定义（不落盘）。",
                    "properties": {
                        "name": {"type": "string"},
                        "description": {"type": "string"},
                        "instructions": {"type": "string"},
                        "tools": {"type": "object"},
                        "model": {"type": "object"},
                        "permissionCeiling": {"type": "string", "enum": ["inherit", "read-only"]}
                    },
                    "required": ["name", "description", "tools"],
                    "additionalProperties": false
                },
                "provider": {"type": "string"},
                "model": {"type": "string"},
                "reasoning_effort": {"type": "string"},
                "allowed_tools": {"type": "array", "items": {"type": "string"}, "description": "已废弃：显式工具列表；与 profile_id/inline 同时出现会被拒绝。"},
                "run_in_background": {"type": "boolean", "description": "已废弃：无论 true 或 false 都立即返回并在后台执行。"},
                "persona": {"type": "string", "description": "已废弃：角色补充提示。"}
            }),
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
            "立即查看子代理结果，不在前台等待。未完成返回 pending，完成结果会自动送达父会话；不要循环调用，可以继续独立工作或先回复用户进度并结束本轮。已结束则直接返回结果；ready 只表示结果可读取，不代表执行成功。",
            json!({"target":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"description":"兼容旧请求；不再前台等待，也不改变子代理执行期限。"}}),
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
            "立即读取任务当前增量输出，不在前台等待。未完成返回 pending，结束后结果自动送达当前会话；不要反复轮询，可以继续独立工作或先回复用户进度并结束本轮。结束后重复读取返回完整保留结果；ready 只表示结果可读取，执行成败见 job.status 和 exitCode。",
            json!({"id":{"type":"string"},"wait":{"type":"boolean","description":"兼容旧请求；true 也立即返回当前输出，未完成时由后台完成通知回传结果。"},"timeout_ms":{"type":"integer","minimum":1,"description":"兼容旧请求；不再前台等待，也不改变后台命令的执行超时。"}}),
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
            "技能按作用域分为全局技能与项目技能：全局技能位于用户主目录的 ~/.denia/skills/（跨项目复用），项目技能位于项目 .denia/skills 等目录（随项目走），同名时项目技能优先，来源见返回的 source。可用技能以 <available_skills> 目录注入消息提供，先从目录取准确技能名；action=load 按 name 加载，SKILL.md 全文直接随本工具结果返回（加载一次即可，目录只含摘要，未加载前不得凭摘要推断技能内容）；其余参考资料与脚本用 action=resource、name、path 按返回的 resourceBase 相对路径读取，禁止越界。",
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
                artifact: None,
                content: value.to_string(),
                is_error: false,
            },
            Err(content) => ToolOutput {
                artifact: None,
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
        let prop = match properties.get(key) {
            Some(prop) => prop,
            // 已移除的参数给出稳定 code，而不是笼统的"不支持参数"：模型与用户
            // 都需要知道"深度配置"是被删除，不是拼错。
            None if key == "max_depth" => {
                return Err(
                    "max_depth 已移除（subagent/depth-config-removed）：子代理禁止派遣子代理，不存在可配置的委派深度。"
                        .to_string(),
                );
            }
            None => return Err(format!("工具不支持参数：{key}")),
        };
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
