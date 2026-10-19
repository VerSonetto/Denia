//! 通道与宿主之间的桥接面。
//!
//! 通道只依赖这组数据与 trait：谁提供工作区、会话与一轮问答由宿主决定。
//! 这样微信协议、命令状态机与绑定存储都能在无 `denia-server` 的环境里测试。

use async_trait::async_trait;
use denia_core::config::ModelSelection;
use denia_core::session::{AskQuestion, PermissionMode};
use serde::Serialize;

/// 一条工作区摘要(供 `/workspace` 列表)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    pub id: String,
    pub title: String,
    pub path: String,
    pub session_count: usize,
}

/// 一个可选模型(供 `/model` 列表与校验)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    pub provider: String,
    pub provider_name: String,
    pub model: String,
    pub model_name: String,
}

/// 一条会话摘要(供 `/switch` 列表)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub title: Option<String>,
    pub excerpt: Option<String>,
    pub created_at: u64,
    pub running: bool,
}

impl SessionInfo {
    /// 列表展示名:AI 标题优先,其次首条用户消息摘要,最后用 id 尾巴兜底。
    pub fn display_name(&self) -> String {
        self.title
            .as_deref()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .or_else(|| {
                self.excerpt
                    .as_deref()
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
            })
            .map(|text| text.chars().take(40).collect())
            .unwrap_or_else(|| format!("会话 {}", &self.id[..self.id.len().min(8)]))
    }
}

/// 一轮问答的结局。通道据此决定是否附上错误说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Aborted,
    MaxTokens,
    LoopDetected,
    Failed(String),
}

/// 一张随消息提交的图片(已解密)。
#[derive(Debug, Clone)]
pub struct ImageAttachment {
    pub mime: String,
    pub data: Vec<u8>,
}

impl TurnOutcome {
    pub fn is_completed(&self) -> bool {
        matches!(self, Self::Completed)
    }

    /// 非正常结束时给用户看的说明;正常结束返回 None。
    pub fn notice(&self) -> Option<String> {
        match self {
            Self::Completed => None,
            Self::Aborted => Some("本轮已被中断。".to_string()),
            Self::MaxTokens => Some("本轮输出达到模型上限，回复可能不完整。".to_string()),
            Self::LoopDetected => Some("检测到模型重复输出，本轮已被强制中断。".to_string()),
            Self::Failed(reason) => Some(format!("本轮执行失败：{reason}")),
        }
    }
}

/// 一轮结束后的结果。
#[derive(Debug, Clone)]
pub struct TurnReply {
    /// 最后一个模型步产出的文本(工具调用的中间说明会被后一条覆盖)。
    pub text: String,
    pub outcome: TurnOutcome,
}

/// 轮次进行中主动推给用户的富通知。
#[derive(Debug, Clone)]
pub enum Notice {
    Text(String),
    /// 工具调用需要审批;QQ 等支持键盘的平台可渲染成按钮。
    Approval {
        session_id: String,
        request_id: String,
        tool: String,
        args_preview: String,
    },
    /// `ask` 工具发起提问;支持键盘的平台可把选项渲染成按钮。
    Ask {
        session_id: String,
        request_id: String,
        questions: Vec<AskQuestion>,
        timeout_ms: u64,
    },
}

/// 轮次进行中主动推给用户的通知(支持文本与富交互两类)。
#[derive(Clone, Default)]
pub struct NoticeSink(Option<tokio::sync::mpsc::UnboundedSender<Notice>>);

impl NoticeSink {
    pub fn new(sender: tokio::sync::mpsc::UnboundedSender<Notice>) -> Self {
        Self(Some(sender))
    }

    pub fn send(&self, notice: Notice) {
        if let Some(sender) = &self.0 {
            let _ = sender.send(notice);
        }
    }
}

/// 通道需要的宿主能力。所有方法都以 `String` 报错:通道只负责转述。
#[async_trait]
pub trait ChannelHost: Send + Sync {
    /// 当前注册的工作区(顺序即注册表展示顺序)。
    async fn workspaces(&self) -> Vec<WorkspaceInfo>;

    /// 某工作区下的普通会话(不含子代理),新会话在前。
    async fn sessions(&self, workspace_id: &str) -> Result<Vec<SessionInfo>, String>;

    /// 在工作区里新建一个空白会话,并写入给定的初始权限档位。
    async fn create_session(
        &self,
        workspace_id: &str,
        permission: PermissionMode,
    ) -> Result<SessionInfo, String>;

    /// 读取会话当前权限档位。
    async fn session_permission(&self, session_id: &str) -> Result<PermissionMode, String>;

    /// 切换会话权限档位。
    async fn set_permission(&self, session_id: &str, mode: PermissionMode) -> Result<(), String>;

    /// 会话是否正在跑一轮。
    async fn session_running(&self, session_id: &str) -> bool;

    /// 可用模型目录(`/model` 展示与校验)。
    async fn models(&self) -> Vec<ModelOption>;

    /// 中断会话当前轮次(幂等)。
    async fn cancel(&self, session_id: &str) -> Result<(), String>;

    /// 结算挂起的审批请求;`request_id` 为空时取最早的一个(文本指令场景)。
    async fn answer_approval(
        &self,
        session_id: &str,
        request_id: Option<&str>,
        approve: bool,
    ) -> Result<bool, String>;

    /// 结算一次 `ask` 提问中单选问题的某个选项;返回是否命中挂起请求。
    async fn answer_ask(
        &self,
        session_id: &str,
        request_id: &str,
        question_id: &str,
        option: &str,
    ) -> Result<bool, String>;

    /// 没有显式模型选择时使用哪个模型(取目录里第一个可用项)。
    async fn default_model(&self) -> Result<ModelSelection, String>;

    /// 提交一条用户消息并等待轮次闭合,返回最终文本。
    async fn run_turn(
        &self,
        session_id: &str,
        text: &str,
        images: &[ImageAttachment],
        model: &ModelSelection,
        notices: &NoticeSink,
    ) -> Result<TurnReply, String>;
}
