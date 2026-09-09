//! `ask` 工具:模型向用户提问并阻塞等待结构化回答。
//!
//! 对照 dsh `ask_user_question`(packages/interaction/tool-ask-user),本实现
//! 在协议与体验上做了几处补强(dsh 的不足见工具 description 与 README):
//! 超时预算、结局区分、推荐选项结构化、跳过与"允许自定义"分列、子代理可问
//! (以父会话为应答方)。
//!
//! 工具本身不做 UI、不碰会话表:它只把问题交给 [`AskBridge`],由宿主把请求
//! 挂到会话的挂起表并等待 REST 应答;回答以普通工具结果回注 agent loop。

use async_trait::async_trait;
use denia_core::session::{AskAnswer, AskOption, AskOutcome, AskQuestion, AskResolution};
use denia_core::tool::ToolSchema;
use serde::Deserialize;

use crate::support::{parse_tool_args, tool_error};
use crate::{Tool, ToolContext, ToolOutput};

/// 默认等待预算:5 分钟。dsh 无超时,工具会永久挂起(见不足分析);
/// 这里给模型可调的有限预算,到点按 `TimedOut` 结算,循环永不空转。
pub const DEFAULT_TIMEOUT_MS: u64 = 300_000;
/// 等待预算上限:30 分钟。再长就该拆分任务而不是干等。
pub const MAX_TIMEOUT_MS: u64 = 1_800_000;
/// 一次提问的问题数上限。
const MAX_QUESTIONS: usize = 8;
/// 单题选项数上限。
const MAX_OPTIONS: usize = 12;

const DESCRIPTION: &str = r#"向用户提问并等待回答,用于需要确认、选择或补充信息才能继续的场景。调用会阻塞到用户作答、超时或取消,回答作为普通工具结果返回。
用法:
- questions:问题数组,每项 {id, question, header?, detail?, options?, multiSelect?, allowCustom?};id 由你指定,原样回显在回答里用于配对。
- options:可选项 [{label, description?, recommended?}]。推荐项用 recommended: true 标记(结构化字段,不要写进 label 文本)。
- multiSelect:是否多选,缺省单选。
- allowCustom:是否允许用户自由填写(缺省 true);纯文本提问可省 options。
- timeoutMs:等待预算(缺省 5 分钟,上限 30 分钟)。到点按超时结算,不会永久挂起。
回答形态:{answers:[{id, selected:[标签], custom?, skipped?}], outcome}。outcome 取值:
- answered:用户已作答;
- timed-out:用户未在预算内作答,请据现有信息继续或改道,不要重复追问同一问题;
- cancelled:用户主动取消,视为"不要继续这条路径";
- unavailable:当前会话无法提问(如无人值守部署),请自行决策并在结论里说明假设。
纪律:
- 只在真正卡住时提问:能用工具查证的事实不要问用户;能给出合理默认的决策先做,把假设写进结论。
- 一次把相关问题问全(同一批次),不要连珠炮式反复打断用户。
- 用户答"跳过"时按缺省继续,不要把跳过当成需要再问的信号。"#;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskArgs {
    questions: Vec<AskArg>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskArg {
    id: String,
    question: String,
    #[serde(default)]
    header: Option<String>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    options: Vec<AskOptionArg>,
    #[serde(default)]
    multi_select: bool,
    #[serde(default)]
    allow_custom: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AskOptionArg {
    label: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    recommended: bool,
}

/// 校验并规范化问题组:非空、id 唯一、选项标签非空且不重复、
/// 至少有一种回答途径(选项或允许自定义)。
fn to_questions(raw: Vec<AskArg>) -> Result<Vec<AskQuestion>, String> {
    if raw.is_empty() {
        return Err("questions 不能为空:至少提供一个要问的问题".to_string());
    }
    if raw.len() > MAX_QUESTIONS {
        return Err(format!(
            "questions 过多({} 个,上限 {MAX_QUESTIONS} 个):请拆成多轮或合并同类问题",
            raw.len()
        ));
    }
    let mut questions = Vec::with_capacity(raw.len());
    let mut seen = std::collections::HashSet::new();
    for item in raw {
        let id = item.id.trim().to_string();
        if id.is_empty() {
            return Err("问题的 id 不能为空(回答按 id 回显配对)".to_string());
        }
        if !seen.insert(id.clone()) {
            return Err(format!("问题 id 重复:{id:?}(每个问题必须有唯一 id)"));
        }
        let question = item.question.trim().to_string();
        if question.is_empty() {
            return Err(format!("问题 {id:?} 的 question 不能为空"));
        }
        let allow_custom = item.allow_custom.unwrap_or(true);
        if item.options.is_empty() && !allow_custom {
            return Err(format!(
                "问题 {id:?} 既没有 options 又不允许自定义回答:用户将无从作答"
            ));
        }
        if item.options.len() > MAX_OPTIONS {
            return Err(format!(
                "问题 {id:?} 的选项过多({} 个,上限 {MAX_OPTIONS} 个)",
                item.options.len()
            ));
        }
        let mut labels = std::collections::HashSet::new();
        let mut options = Vec::with_capacity(item.options.len());
        for option in item.options {
            let label = option.label.trim().to_string();
            if label.is_empty() {
                return Err(format!("问题 {id:?} 存在空选项标签"));
            }
            if !labels.insert(label.clone()) {
                return Err(format!("问题 {id:?} 的选项标签重复:{label:?}"));
            }
            options.push(AskOption {
                label,
                description: option.description.filter(|text| !text.trim().is_empty()),
                recommended: option.recommended,
            });
        }
        questions.push(AskQuestion {
            id,
            question,
            header: item.header.filter(|text| !text.trim().is_empty()),
            detail: item.detail.filter(|text| !text.trim().is_empty()),
            options,
            multi_select: item.multi_select,
            allow_custom,
        });
    }
    Ok(questions)
}

/// 把结局渲染成模型可见文本。
///
/// dsh 只用一句 `Error: …` 概括全部失败;这里按结局给出可执行的下一步,
/// 并把已收到的部分答案一并回传(超时/取消时用户可能已答了一部分)。
fn render_resolution(resolution: &AskResolution) -> (String, bool) {
    let mut text = String::new();
    let is_error = false;
    match resolution.outcome {
        AskOutcome::Answered => text.push_str("用户已回答。"),
        AskOutcome::TimedOut => text.push_str(
            "提问超时:用户在等待预算内未作答。请据现有信息继续(把采用的假设写进结论),不要原样重复同一问题。",
        ),
        AskOutcome::Cancelled => text.push_str(
            "用户取消了本次提问:视为不要沿这条路径继续,请停下并说明当前状态与可选方案。",
        ),
        AskOutcome::Unavailable => {
            text.push_str("当前会话无法向用户提问");
            if let Some(reason) = &resolution.reason {
                text.push_str(&format!("({reason})"));
            }
            text.push_str(":请自行决策继续,并在结论里明确写出你采用的假设。");
        }
    }
    if resolution.answers.is_empty() {
        text.push_str("\nanswers: []");
        return (text, is_error);
    }
    let rendered: Vec<String> = resolution
        .answers
        .iter()
        .map(|answer| {
            let mut parts = vec![format!("\"id\":{}", json_str(&answer.id))];
            if answer.skipped {
                parts.push("\"skipped\":true".to_string());
            }
            if !answer.selected.is_empty() {
                let labels: Vec<String> = answer.selected.iter().map(|l| json_str(l)).collect();
                parts.push(format!("\"selected\":[{}]", labels.join(",")));
            }
            if let Some(custom) = &answer.custom {
                parts.push(format!("\"custom\":{}", json_str(custom)));
            }
            format!("{{{}}}", parts.join(","))
        })
        .collect();
    text.push_str(&format!("\nanswers: [{}]", rendered.join(",")));
    (text, is_error)
}

/// 最小 JSON 字符串转义(回答里可能含引号/换行)。
fn json_str(value: &str) -> String {
    serde_json::Value::String(value.to_string()).to_string()
}

/// `ask` 工具:模型侧提问入口。
pub struct AskTool {
    schema: ToolSchema,
}

impl AskTool {
    pub fn new() -> Self {
        Self {
            schema: ToolSchema {
                name: "ask".to_string(),
                description: DESCRIPTION.to_string(),
                parameters: serde_json::json!({
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "questions": {
                            "type": "array",
                            "description": "要问用户的问题;一次问全相关问题,不要分多轮反复打断。",
                            "items": {
                                "type": "object",
                                "additionalProperties": false,
                                "properties": {
                                    "id": {
                                        "type": "string",
                                        "description": "本问题的稳定 id,回答里原样回显。"
                                    },
                                    "question": {
                                        "type": "string",
                                        "description": "要问用户的具体问题。"
                                    },
                                    "header": {
                                        "type": "string",
                                        "description": "可选短标题(如\"确认\"\"选择模式\")。"
                                    },
                                    "detail": {
                                        "type": "string",
                                        "description": "可选补充说明(可含 markdown),渲染在问题下方。"
                                    },
                                    "options": {
                                        "type": "array",
                                        "description": "可选项;推荐项用 recommended: true 标记。",
                                        "items": {
                                            "type": "object",
                                            "additionalProperties": false,
                                            "properties": {
                                                "label": {
                                                    "type": "string",
                                                    "description": "简短的用户可见标签(也是回答取值)。"
                                                },
                                                "description": {
                                                    "type": "string",
                                                    "description": "一句话说明取舍或影响。"
                                                },
                                                "recommended": {
                                                    "type": "boolean",
                                                    "description": "标记为推荐项;UI 会高亮。缺省 false。"
                                                }
                                            },
                                            "required": ["label"]
                                        }
                                    },
                                    "multiSelect": {
                                        "type": "boolean",
                                        "description": "是否允许多选;缺省单选。"
                                    },
                                    "allowCustom": {
                                        "type": "boolean",
                                        "description": "是否允许用户自由填写;缺省 true。"
                                    }
                                },
                                "required": ["id", "question"]
                            }
                        },
                        "timeoutMs": {
                            "type": "integer",
                            "description": "等待预算(毫秒);缺省 300000(5 分钟),上限 1800000(30 分钟)。"
                        }
                    },
                    "required": ["questions"]
                }),
            },
        }
    }
}

impl Default for AskTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for AskTool {
    fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    async fn execute(&self, arguments: &str, ctx: &ToolContext) -> ToolOutput {
        let args: AskArgs = match parse_tool_args(arguments) {
            Ok(args) => args,
            Err(error) => {
                return tool_error(
                    format!("参数解析失败:{error}"),
                    "参数必须是 JSON 对象,必填字段 questions(数组,每项含 id 与 question)",
                );
            }
        };
        let questions = match to_questions(args.questions) {
            Ok(questions) => questions,
            Err(error) => return tool_error(error, "修正后重新发起提问"),
        };
        let timeout_ms = args
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1_000, MAX_TIMEOUT_MS);

        let Some(bridge) = &ctx.ask else {
            // 无通道时按 dsh 的 fail-loud 结算,但给出可执行出路而不是纯报错。
            let resolution = AskResolution {
                outcome: AskOutcome::Unavailable,
                answers: Vec::new(),
                reason: Some("该调用没有绑定应答通道".to_string()),
            };
            let (text, is_error) = render_resolution(&resolution);
            return ToolOutput {
                content: text,
                is_error,
            };
        };
        let Some(session_id) = ctx.session_id.as_deref() else {
            let resolution = AskResolution {
                outcome: AskOutcome::Unavailable,
                answers: Vec::new(),
                reason: Some("该调用不属于任何代理会话".to_string()),
            };
            let (text, is_error) = render_resolution(&resolution);
            return ToolOutput {
                content: text,
                is_error,
            };
        };
        // request_id 与 call_id 分离:dsh 用同一 id 承担两者,重试时旧请求
        // 的应答可能误配到新请求(见不足分析)。这里 request_id 唯一标识
        // 本次挂起,call_id 供 UI 把卡片挂到对应工具行。
        let request_id = format!("ask-{}", uuid::Uuid::new_v4());
        let call_id = ctx.call_id.clone().unwrap_or_default();
        let resolution = bridge
            .ask(
                session_id,
                &request_id,
                &call_id,
                &questions,
                timeout_ms,
                ctx.cancel.clone(),
            )
            .await;
        let (text, is_error) = render_resolution(&resolution);
        ToolOutput {
            content: text,
            is_error,
        }
    }
}

/// 判定一个回答是否为空(既没选也没写、也没显式跳过)。
pub fn answer_is_blank(answer: &AskAnswer) -> bool {
    answer.selected.is_empty() && answer.custom.as_deref().unwrap_or("").trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::session::PermissionMode;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    struct StubBridge {
        resolution: AskResolution,
        captured: std::sync::Mutex<Option<(Vec<AskQuestion>, u64)>>,
    }

    #[async_trait]
    impl crate::AskBridge for StubBridge {
        async fn ask(
            &self,
            _session_id: &str,
            _request_id: &str,
            _call_id: &str,
            questions: &[AskQuestion],
            timeout_ms: u64,
            _cancel: CancellationToken,
        ) -> AskResolution {
            *self.captured.lock().unwrap() = Some((questions.to_vec(), timeout_ms));
            self.resolution.clone()
        }
    }

    fn ctx_with(bridge: Option<Arc<dyn crate::AskBridge>>) -> ToolContext {
        ToolContext {
            session_id: Some("s1".to_string()),
            selection: None,
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: true,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::AutoEdit,
            ask: bridge,
            call_id: Some("call-1".to_string()),
            goal_reader: None,
        }
    }

    fn answered(id: &str, selected: &[&str], custom: Option<&str>) -> AskResolution {
        AskResolution {
            outcome: AskOutcome::Answered,
            answers: vec![AskAnswer {
                id: id.to_string(),
                selected: selected.iter().map(|s| s.to_string()).collect(),
                custom: custom.map(str::to_string),
                skipped: false,
            }],
            reason: None,
        }
    }

    #[tokio::test]
    async fn renders_answered_labels_and_custom() {
        let bridge = Arc::new(StubBridge {
            resolution: answered("pick", &["甲"], Some("补充说明")),
            captured: std::sync::Mutex::new(None),
        });
        let tool = AskTool::new();
        let output = tool
            .execute(
                r#"{"questions":[{"id":"pick","question":"选哪个?","options":[{"label":"甲"},{"label":"乙","recommended":true}]}]}"#,
                &ctx_with(Some(bridge.clone() as Arc<dyn crate::AskBridge>)),
            )
            .await;
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("用户已回答"), "{}", output.content);
        assert!(output.content.contains("\"selected\":[\"甲\"]"), "{}", output.content);
        assert!(output.content.contains("\"custom\":\"补充说明\""), "{}", output.content);
        let captured = bridge.captured.lock().unwrap().clone().unwrap();
        assert!(captured.0[0].options[1].recommended);
        assert_eq!(captured.1, DEFAULT_TIMEOUT_MS);
    }

    #[tokio::test]
    async fn timeout_outcome_is_actionable_not_error() {
        let bridge = Arc::new(StubBridge {
            resolution: AskResolution {
                outcome: AskOutcome::TimedOut,
                answers: Vec::new(),
                reason: None,
            },
            captured: std::sync::Mutex::new(None),
        });
        let tool = AskTool::new();
        let output = tool
            .execute(
                r#"{"questions":[{"id":"q","question":"继续吗?"}]}"#,
                &ctx_with(Some(bridge as Arc<dyn crate::AskBridge>)),
            )
            .await;
        assert!(!output.is_error, "超时是正常结局,不应标记为工具错误");
        assert!(output.content.contains("提问超时"), "{}", output.content);
        assert!(output.content.contains("不要原样重复同一问题"), "{}", output.content);
    }

    #[tokio::test]
    async fn cancelled_and_unavailable_have_distinct_guidance() {
        let tool = AskTool::new();
        for (outcome, needle) in [
            (AskOutcome::Cancelled, "用户取消了本次提问"),
            (AskOutcome::Unavailable, "当前会话无法向用户提问"),
        ] {
            let bridge = Arc::new(StubBridge {
                resolution: AskResolution {
                    outcome,
                    answers: Vec::new(),
                    reason: Some("测试原因".to_string()),
                },
                captured: std::sync::Mutex::new(None),
            });
            let output = tool
                .execute(
                    r#"{"questions":[{"id":"q","question":"继续吗?"}]}"#,
                    &ctx_with(Some(bridge as Arc<dyn crate::AskBridge>)),
                )
                .await;
            assert!(!output.is_error, "{:?}", outcome);
            assert!(output.content.contains(needle), "{:?} → {}", outcome, output.content);
        }
    }

    #[tokio::test]
    async fn no_bridge_yields_unavailable_guidance() {
        let tool = AskTool::new();
        let output = tool
            .execute(
                r#"{"questions":[{"id":"q","question":"继续吗?"}]}"#,
                &ctx_with(None),
            )
            .await;
        assert!(!output.is_error);
        assert!(output.content.contains("无法向用户提问"), "{}", output.content);
        assert!(output.content.contains("自行决策"), "{}", output.content);
    }

    #[tokio::test]
    async fn timeout_budget_is_clamped_to_ceiling() {
        let bridge = Arc::new(StubBridge {
            resolution: answered("q", &[], Some("好")),
            captured: std::sync::Mutex::new(None),
        });
        let tool = AskTool::new();
        tool.execute(
            r#"{"questions":[{"id":"q","question":"?"}],"timeoutMs":99999999}"#,
            &ctx_with(Some(bridge.clone() as Arc<dyn crate::AskBridge>)),
        )
        .await;
        let captured = bridge.captured.lock().unwrap().clone().unwrap();
        assert_eq!(captured.1, MAX_TIMEOUT_MS);
    }

    #[test]
    fn rejects_malformed_question_sets() {
        let cases: Vec<(&str, &str)> = vec![
            (r#"{"questions":[]}"#, "不能为空"),
            (
                r#"{"questions":[{"id":"a","question":"?"},{"id":"a","question":"?"}]}"#,
                "id 重复",
            ),
            (r#"{"questions":[{"id":"","question":"?"}]}"#, "id 不能为空"),
            (r#"{"questions":[{"id":"a","question":"  "}]}"#, "question 不能为空"),
            (
                r#"{"questions":[{"id":"a","question":"?","allowCustom":false}]}"#,
                "无从作答",
            ),
            (
                r#"{"questions":[{"id":"a","question":"?","options":[{"label":"x"},{"label":"x"}]}]}"#,
                "选项标签重复",
            ),
        ];
        for (raw, needle) in cases {
            let args: AskArgs = serde_json::from_str(raw).expect("shape parses");
            let error = to_questions(args.questions).expect_err(raw);
            assert!(error.contains(needle), "{raw} → {error}");
        }
    }

    #[test]
    fn normalizes_and_defaults() {
        let args: AskArgs = serde_json::from_str(
            r#"{"questions":[{"id":" a ","question":" 选? ","options":[{"label":" 甲 ","recommended":true}],"header":"  ","detail":"  "}]}"#,
        )
        .unwrap();
        let questions = to_questions(args.questions).unwrap();
        assert_eq!(questions[0].id, "a");
        assert_eq!(questions[0].question, "选?");
        assert_eq!(questions[0].options[0].label, "甲");
        assert!(questions[0].options[0].recommended);
        assert!(questions[0].header.is_none(), "空白 header 应归一化为 None");
        assert!(questions[0].detail.is_none());
        assert!(questions[0].allow_custom, "allowCustom 缺省 true");
        assert!(!questions[0].multi_select);
    }

    #[test]
    fn description_carries_outcomes_and_discipline() {
        let tool = AskTool::new();
        let description = &tool.schema().description;
        for needle in [
            "timed-out",
            "unavailable",
            "recommended: true",
            "只在真正卡住时提问",
            "不要连珠炮",
        ] {
            assert!(description.contains(needle), "description 缺少 {needle:?}");
        }
        assert_eq!(tool.schema().name, "ask");
    }
}
