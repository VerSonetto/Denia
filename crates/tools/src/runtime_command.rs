//! Model JSON is parsed at the tool boundary. Hosts dispatch typed operations.
use denia_core::subagent::DelegateArgs;
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
            // 派遣参数在这里一次解析成严格类型：`Runtime::delegate` 不再零散
            // 读 JSON，字段错误在产生任何副作用之前报出。
            let args: DelegateArgs = serde_json::from_value(args).map_err(|error| {
                format!("subagent/invalid-args: 派遣参数无效（{error}）；可用字段：prompt、description、profile_id、inline、allowed_tools")
            })?;
            if args.prompt.trim().is_empty() {
                return Err("subagent/invalid-args: prompt 不能为空".into());
            }
            return Ok(Self::Delegate {
                fork: name == "fork_agent",
                args,
            });
        }
        serde_json::from_value(json!({"tool": name, "args": args}))
            .map_err(|error| format!("运行时参数无效 ({name}): {error}"))
    }
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
