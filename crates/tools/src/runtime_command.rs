//! Model JSON is parsed at the tool boundary. Hosts dispatch typed operations.
use denia_core::subagent::SubagentInlineSpec;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
#[serde(tag = "tool", content = "args", rename_all = "snake_case")]
pub enum RuntimeCommand {
    Skill(SkillCommand),
    JobStart(JobStartArgs),
    JobList(EmptyArgs),
    JobOutput(IdArgs),
    JobKill(IdArgs),
    ListAgents(EmptyArgs),
    InterruptAgent(TargetArgs),
    SendMessage(SendArgs),
    WaitAgent(TargetArgs),
    /// 派遣：参数在这里就完成类型化校验，宿主不再零散读取 JSON。
    #[serde(skip)]
    Delegate {
        fork: bool,
        args: DelegateArgs,
    },
}

impl RuntimeCommand {
    pub fn parse(name: &str, args: Value) -> Result<Self, String> {
        if args
            .get("timeout_ms")
            .is_some_and(|v| v.as_u64().is_none_or(|n| n == 0))
        {
            return Err("timeout_ms 必须为正整数".into());
        }
        if matches!(name, "spawn_agent" | "fork_agent") {
            let args: DelegateArgs = serde_json::from_value(args)
                .map_err(|error| format!("派遣参数无效（{name}）：{error}"))?;
            return Ok(Self::Delegate {
                fork: name == "fork_agent",
                args,
            });
        }
        serde_json::from_value(json!({"tool": name, "args": args}))
            .map_err(|error| format!("运行时参数无效 ({name}): {error}"))
    }
}

/// 派遣参数契约（外层工具参数 snake_case；`inline` 内部是 camelCase DTO）。
///
/// 未知参数直接拒绝：含糊的混合别名一律不接受，兼容适配集中在这里与
/// `resolve_dispatch` 的显式分支里。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegateArgs {
    /// 任务提示（必填，非空）。
    pub prompt: String,
    /// 一句话说明这次派遣做什么；缺省时用定义描述。
    #[serde(default)]
    pub description: Option<String>,
    /// 已保存定义的限定 id（`builtin:develop` 等）。
    #[serde(default)]
    pub profile_id: Option<String>,
    /// 调用时创建的临时定义（不落盘）。
    #[serde(default)]
    pub inline: Option<SubagentInlineSpec>,
    /// 调用级模型覆盖（provider/model 必须成套）。
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    /// 旧参数：显式工具列表。与 profile/inline 同时出现会被拒绝（歧义）。
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// 旧参数：无论 true/false 都立即返回并在后台执行（兼容语义）。
    #[serde(default)]
    pub run_in_background: Option<bool>,
    /// 旧参数：角色补充提示，映射为 instructions。
    #[serde(default)]
    pub persona: Option<String>,
    /// 已移除的参数：出现即报 `subagent/depth-config-removed`。
    #[serde(default)]
    pub max_depth: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SkillCommand {
    List,
    Load {
        #[serde(deserialize_with = "required")]
        name: String,
    },
    Resource {
        #[serde(deserialize_with = "required")]
        name: String,
        #[serde(deserialize_with = "required")]
        path: String,
    },
}

#[derive(Debug, Deserialize)]
pub struct EmptyArgs {}

#[derive(Debug, Deserialize)]
pub struct JobStartArgs {
    #[serde(deserialize_with = "required")]
    pub command: String,
    pub label: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct IdArgs {
    #[serde(deserialize_with = "required")]
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct TargetArgs {
    #[serde(deserialize_with = "required")]
    pub target: String,
}

#[derive(Debug, Deserialize)]
pub struct SendArgs {
    #[serde(deserialize_with = "required")]
    pub target: String,
    #[serde(deserialize_with = "required")]
    pub message: String,
}

fn required<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.trim().is_empty() {
        return Err(serde::de::Error::custom("参数必须为非空字符串"));
    }
    Ok(value)
}
