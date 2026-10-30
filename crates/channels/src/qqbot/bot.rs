//! QQ 通道的消息处理:绑定、指令、会话轮次与出站。
//!
//! 与微信通道共用同一套 `ChannelHost`、`bindings` 与 `commands`;区别在于:
//! - 绑定 key 是发送者 openid,回复目标是会话(群 group_openid / 私聊 openid);
//! - 被动回复第一片携带入站 msg_id;
//! - 单条消息上限 4000 字符。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use denia_core::config::ModelSelection;
use denia_core::session::{AskQuestion, PermissionMode};
use tokio::sync::mpsc;

use crate::bindings::BindingStore;
use crate::bridge::{ChannelHost, ImageAttachment, Notice, NoticeSink};
use crate::commands::{self, Command};
use crate::weixin::SharedStatus;
use crate::weixin::bot::split_text;
use crate::weixin::media;

use super::client::Client;
use super::protocol::{ChatType, InboundMessage, MAX_MESSAGE_LENGTH};

const DEDUP_TTL: Duration = Duration::from_secs(300);
const MAX_IMAGES_PER_MESSAGE: usize = 4;

/// 待决 ask:按钮点击与文字回答都用它结算。
#[derive(Clone)]
struct PendingAsk {
    session_id: String,
    request_id: String,
    question: AskQuestion,
    sender: String,
}

pub struct QqBot {
    host: Arc<dyn ChannelHost>,
    bindings: Arc<BindingStore>,
    client: Arc<Client>,
    status: SharedStatus,
    /// 每个会话最近一条入站消息 id:被动回复要带。
    last_msg_ids: Mutex<HashMap<String, String>>,
    dedup: Mutex<HashMap<String, Instant>>,
    /// 待决 ask(request_id → 提问),按钮点击与文字回答都用它结算。
    pending_asks: Mutex<HashMap<String, PendingAsk>>,
}

impl QqBot {
    pub fn new(
        host: Arc<dyn ChannelHost>,
        bindings: Arc<BindingStore>,
        client: Arc<Client>,
        status: SharedStatus,
    ) -> Arc<Self> {
        Arc::new(Self {
            host,
            bindings,
            client,
            status,
            last_msg_ids: Mutex::new(HashMap::new()),
            dedup: Mutex::new(HashMap::new()),
            pending_asks: Mutex::new(HashMap::new()),
        })
    }

    pub async fn handle_inbound(self: &Arc<Self>, message: InboundMessage) {
        if message.sender.trim().is_empty() {
            return;
        }
        if self.seen(&message.id) {
            return;
        }
        self.last_msg_ids
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(message.chat_id.clone(), message.id.clone());
        self.mark_inbound(message.content.chars().count());

        let mut images = Vec::new();
        for attachment in message
            .attachments
            .iter()
            .filter(|attachment| attachment.is_image())
            .take(MAX_IMAGES_PER_MESSAGE)
        {
            let url = attachment.url.trim();
            if url.is_empty() {
                continue;
            }
            match self.client.download(url).await {
                Ok(data) => {
                    let mime = if attachment.content_type.trim().is_empty() {
                        media::guess_mime(&data).to_string()
                    } else {
                        attachment.content_type.clone()
                    };
                    images.push(ImageAttachment { mime, data });
                }
                Err(error) => tracing::warn!(%error, "QQ 图片下载失败"),
            }
        }

        let text = message.content.trim();
        // 待决 ask 优先:纯文本/数字直接结算,避免被当成新一轮提交。
        if !text.is_empty()
            && !text.starts_with('/')
            && let Some((request_id, pending)) = self.pending_ask_for(&message.sender)
        {
            if let Some(option) = option_for_answer(&pending.question, text) {
                let reply = match self
                    .host
                    .answer_ask(
                        &pending.session_id,
                        &pending.request_id,
                        &pending.question.id,
                        &option,
                    )
                    .await
                {
                    Ok(true) => format!("已选择：{option}"),
                    Ok(false) => "该提问已处理或已过期。".to_string(),
                    Err(error) => format!("提交回答失败：{error}"),
                };
                self.pending_asks
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&request_id);
                self.send_text(&message.chat_id, message.chat_type, reply)
                    .await;
                return;
            }
            self.send_text(
                &message.chat_id,
                message.chat_type,
                "请先回答上面的问题（回复数字），或发送 /cancel 结束本轮。",
            )
            .await;
            return;
        }
        if text.is_empty() && images.is_empty() {
            if !message.attachments.is_empty() {
                self.send_text(
                    &message.chat_id,
                    message.chat_type,
                    "目前支持文字与图片；语音、文件暂未接入。",
                )
                .await;
            }
            return;
        }
        self.handle_text(
            &message.sender,
            &message.chat_id,
            message.chat_type,
            text,
            &images,
        )
        .await;
    }

    async fn handle_text(
        self: &Arc<Self>,
        sender: &str,
        target: &str,
        chat_type: ChatType,
        text: &str,
        images: &[ImageAttachment],
    ) {
        if let Some(command) = commands::parse(text) {
            self.handle_command(sender, target, chat_type, command)
                .await;
            return;
        }
        self.run_prompt(sender, target, chat_type, text, images)
            .await;
    }

    async fn handle_command(
        self: &Arc<Self>,
        sender: &str,
        target: &str,
        chat_type: ChatType,
        command: Command,
    ) {
        self.set_action(format!("命令 {}", command.name()));
        match command {
            Command::Help => {
                self.send_text(target, chat_type, commands::help_text())
                    .await;
            }
            Command::Workspace(None) => {
                let workspaces = self.host.workspaces().await;
                let current = self.bindings.get(sender).workspace_id;
                self.send_text(
                    target,
                    chat_type,
                    commands::workspace_list(&workspaces, current.as_deref()),
                )
                .await;
            }
            Command::Workspace(Some(argument)) => {
                self.select_workspace(sender, target, chat_type, &argument)
                    .await
            }
            Command::Switch(None) => self.list_sessions(sender, target, chat_type).await,
            Command::Switch(Some(argument)) => {
                self.switch_session(sender, target, chat_type, &argument)
                    .await
            }
            Command::Model(None) => self.show_models(sender, target, chat_type).await,
            Command::Model(Some(argument)) => {
                self.change_model(sender, target, chat_type, &argument)
                    .await
            }
            Command::Permission(None) => self.show_permission(sender, target, chat_type).await,
            Command::Permission(Some(argument)) => {
                self.change_permission(sender, target, chat_type, &argument)
                    .await
            }
            Command::Cancel => self.cancel_turn(sender, target, chat_type).await,
            Command::Approve => self.answer_approval(sender, target, chat_type, true).await,
            Command::Reject => self.answer_approval(sender, target, chat_type, false).await,
            Command::Unknown(name) => {
                self.send_text(
                    target,
                    chat_type,
                    format!("未知命令 /{name}。\n\n{}", commands::help_text()),
                )
                .await;
            }
        }
    }

    async fn select_workspace(
        self: &Arc<Self>,
        sender: &str,
        target: &str,
        chat_type: ChatType,
        argument: &str,
    ) {
        let workspaces = self.host.workspaces().await;
        let found = match argument.trim().parse::<usize>() {
            Ok(index) if index >= 1 => workspaces.get(index - 1),
            _ => {
                let argument = argument.trim();
                workspaces.iter().find(|workspace| {
                    workspace.title.eq_ignore_ascii_case(argument)
                        || workspace.path.eq_ignore_ascii_case(argument)
                        || workspace.title.contains(argument)
                })
            }
        };
        match found {
            Some(workspace) => {
                self.bindings.update(sender, |binding| {
                    binding.workspace_id = Some(workspace.id.clone());
                    binding.session_id = None;
                });
                self.send_text(
                    target,
                    chat_type,
                    format!(
                        "已选择工作区「{}」({})。\n尚未选择会话：下一条消息会新建会话。\n用 /switch 查看该工作区的会话。",
                        workspace.title, workspace.path
                    ),
                )
                .await;
            }
            None => {
                self.send_text(
                    target,
                    chat_type,
                    "没有找到该工作区。发送 /workspace 查看列表。",
                )
                .await;
            }
        }
    }

    async fn list_sessions(self: &Arc<Self>, sender: &str, target: &str, chat_type: ChatType) {
        let binding = self.bindings.get(sender);
        let Some(workspace_id) = binding.workspace_id.clone() else {
            self.send_text(target, chat_type, "请先发送 /workspace 选择工作区。")
                .await;
            return;
        };
        let workspaces = self.host.workspaces().await;
        let Some(workspace) = workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
        else {
            self.send_text(
                target,
                chat_type,
                "该工作区已不存在，请重新 /workspace 选择。",
            )
            .await;
            return;
        };
        match self.host.sessions(&workspace_id).await {
            Ok(sessions) => {
                self.send_text(
                    target,
                    chat_type,
                    commands::session_list(workspace, &sessions, binding.session_id.as_deref()),
                )
                .await;
            }
            Err(error) => {
                self.send_text(target, chat_type, format!("读取会话失败：{error}"))
                    .await;
            }
        }
    }

    async fn switch_session(
        self: &Arc<Self>,
        sender: &str,
        target: &str,
        chat_type: ChatType,
        argument: &str,
    ) {
        let binding = self.bindings.get(sender);
        let Some(workspace_id) = binding.workspace_id.clone() else {
            self.send_text(target, chat_type, "请先发送 /workspace 选择工作区。")
                .await;
            return;
        };
        if argument.eq_ignore_ascii_case("new") {
            self.bindings
                .update(sender, |binding| binding.session_id = None);
            self.send_text(target, chat_type, "已切到新会话：下一条消息会新建会话。")
                .await;
            return;
        }
        let sessions = match self.host.sessions(&workspace_id).await {
            Ok(sessions) => sessions,
            Err(error) => {
                self.send_text(target, chat_type, format!("读取会话失败：{error}"))
                    .await;
                return;
            }
        };
        let found = match argument.trim().parse::<usize>() {
            Ok(index) if index >= 1 => sessions.get(index - 1),
            _ => sessions
                .iter()
                .find(|session| session.display_name().contains(argument.trim())),
        };
        match found {
            Some(session) => {
                self.bindings.update(sender, |binding| {
                    binding.session_id = Some(session.id.clone());
                });
                self.send_text(
                    target,
                    chat_type,
                    format!("已切换到会话「{}」。", session.display_name()),
                )
                .await;
            }
            None => {
                self.send_text(target, chat_type, "没有找到该会话。发送 /switch 查看列表。")
                    .await;
            }
        }
    }

    async fn show_models(self: &Arc<Self>, sender: &str, target: &str, chat_type: ChatType) {
        let binding = self.bindings.get(sender);
        let models = self.host.models().await;
        self.send_text(
            target,
            chat_type,
            commands::model_list(binding.model.as_ref(), &models),
        )
        .await;
    }

    async fn change_model(
        self: &Arc<Self>,
        sender: &str,
        target: &str,
        chat_type: ChatType,
        argument: &str,
    ) {
        let argument = argument.trim();
        let Some((provider, model)) = argument.split_once(char::is_whitespace) else {
            self.send_text(
                target,
                chat_type,
                "用法：/model <供应商id> <模型id>，例如 /model Astra deepseek_v4_flash。",
            )
            .await;
            return;
        };
        let (provider, model) = (provider.trim(), model.trim());
        if provider.is_empty() || model.is_empty() {
            self.send_text(target, chat_type, "供应商 id 与模型 id 都不能为空。")
                .await;
            return;
        }
        let models = self.host.models().await;
        match commands::find_model(&models, provider, model) {
            commands::ModelLookup::Found(option) => {
                self.bindings.update(sender, |binding| {
                    binding.model = Some(ModelSelection {
                        provider: option.provider.clone(),
                        model: option.model.clone(),
                        reasoning_effort: None,
                    });
                });
                self.send_text(
                    target,
                    chat_type,
                    format!(
                        "模型已切换为 {}/{}，下一轮消息生效。",
                        option.provider, option.model
                    ),
                )
                .await;
            }
            commands::ModelLookup::NotFound => {
                self.send_text(
                    target,
                    chat_type,
                    format!("模型目录里没有 {provider}/{model}。发送 /model 查看可用列表。"),
                )
                .await;
            }
            commands::ModelLookup::Ambiguous(options) => {
                let mut list = String::new();
                for option in options.iter().take(10) {
                    list.push_str(&format!("- {} {}\n", option.provider, option.model));
                }
                self.send_text(
                    target,
                    chat_type,
                    format!("匹配到多个模型，请使用完整 id：\n{list}"),
                )
                .await;
            }
        }
    }

    async fn show_permission(self: &Arc<Self>, sender: &str, target: &str, chat_type: ChatType) {
        let binding = self.bindings.get(sender);
        let current = match &binding.session_id {
            Some(session_id) => self
                .host
                .session_permission(session_id)
                .await
                .unwrap_or_else(|_| binding.permission.unwrap_or(PermissionMode::AutoEdit)),
            None => binding.permission.unwrap_or(PermissionMode::AutoEdit),
        };
        self.send_text(target, chat_type, commands::permission_list(current))
            .await;
    }

    async fn change_permission(
        self: &Arc<Self>,
        sender: &str,
        target: &str,
        chat_type: ChatType,
        argument: &str,
    ) {
        let Some(mode) = PermissionMode::parse(argument) else {
            self.send_text(
                target,
                chat_type,
                "未知权限档位。可选：read-only、auto-edit、plan、full。",
            )
            .await;
            return;
        };
        self.bindings.update(sender, |binding| {
            binding.permission = Some(mode);
        });
        let binding = self.bindings.get(sender);
        if let Some(session_id) = binding.session_id {
            match self.host.set_permission(&session_id, mode).await {
                Ok(()) => {
                    self.send_text(
                        target,
                        chat_type,
                        format!(
                            "当前会话权限已切换为{}（{}）。",
                            commands::permission_label(mode),
                            mode.as_str()
                        ),
                    )
                    .await;
                }
                Err(error) => {
                    self.send_text(target, chat_type, format!("切换权限失败：{error}"))
                        .await;
                }
            }
        } else {
            self.send_text(
                target,
                chat_type,
                format!(
                    "已设置新建会话的权限为{}（{}）。",
                    commands::permission_label(mode),
                    mode.as_str()
                ),
            )
            .await;
        }
    }

    async fn cancel_turn(self: &Arc<Self>, sender: &str, target: &str, chat_type: ChatType) {
        let binding = self.bindings.get(sender);
        let Some(session_id) = binding.session_id else {
            self.send_text(target, chat_type, "当前没有会话。").await;
            return;
        };
        match self.host.cancel(&session_id).await {
            Ok(()) => self.send_text(target, chat_type, "已中断当前轮次。").await,
            Err(error) => {
                self.send_text(target, chat_type, format!("中断失败：{error}"))
                    .await;
            }
        }
    }

    async fn answer_approval(
        self: &Arc<Self>,
        sender: &str,
        target: &str,
        chat_type: ChatType,
        approve: bool,
    ) {
        let Some(session_id) = self.bindings.get(sender).session_id else {
            self.send_text(target, chat_type, "当前没有会话。").await;
            return;
        };
        match self.host.answer_approval(&session_id, None, approve).await {
            Ok(true) => {
                let text = if approve {
                    "已放行。"
                } else {
                    "已拒绝。"
                };
                self.send_text(target, chat_type, text).await;
            }
            Ok(false) => {
                self.send_text(target, chat_type, "当前没有待审批的请求。")
                    .await;
            }
            Err(error) => {
                self.send_text(target, chat_type, format!("结算审批失败：{error}"))
                    .await;
            }
        }
    }

    async fn run_prompt(
        self: &Arc<Self>,
        sender: &str,
        target: &str,
        chat_type: ChatType,
        text: &str,
        images: &[ImageAttachment],
    ) {
        let binding = self.bindings.get(sender);
        let Some(workspace_id) = binding.workspace_id.clone() else {
            self.set_action("未选择工作区");
            self.send_text(target, chat_type, "请先发送 /workspace 选择工作区。")
                .await;
            return;
        };
        let workspaces = self.host.workspaces().await;
        if !workspaces
            .iter()
            .any(|workspace| workspace.id == workspace_id)
        {
            self.set_action("工作区已不存在");
            self.bindings.update(sender, |binding| {
                binding.workspace_id = None;
                binding.session_id = None;
            });
            self.send_text(
                target,
                chat_type,
                "该工作区已不存在，请重新用 /workspace 选择。",
            )
            .await;
            return;
        }
        let model = match binding.model.clone() {
            Some(model) => model,
            None => match self.host.default_model().await {
                Ok(model) => {
                    self.bindings.update(sender, |binding| {
                        binding.model = Some(model.clone());
                    });
                    model
                }
                Err(error) => {
                    self.set_action("没有可用模型");
                    self.send_text(
                        target,
                        chat_type,
                        format!(
                            "没有可用模型：{error}\n请先在 denia 控制台配置模型提供商并选择模型。"
                        ),
                    )
                    .await;
                    return;
                }
            },
        };
        let permission = binding.permission.unwrap_or(PermissionMode::AutoEdit);
        let session_id = match binding.session_id.clone() {
            Some(session_id) => session_id,
            None => match self.host.create_session(&workspace_id, permission).await {
                Ok(info) => {
                    self.bindings.update(sender, |binding| {
                        binding.session_id = Some(info.id.clone());
                    });
                    info.id
                }
                Err(error) => {
                    self.set_action(format!("新建会话失败：{error}"));
                    self.send_text(target, chat_type, format!("新建会话失败：{error}"))
                        .await;
                    return;
                }
            },
        };
        if self.host.session_running(&session_id).await {
            self.set_action("上一轮仍在执行");
            self.send_text(
                target,
                chat_type,
                "上一轮仍在执行。发送 /cancel 可中断，或等它结束后再发。",
            )
            .await;
            return;
        }
        self.set_action(format!(
            "已提交到会话 {}",
            &session_id[..session_id.len().min(8)]
        ));
        let (notice_tx, mut notice_rx) = mpsc::unbounded_channel::<Notice>();
        let notifier = {
            let this = self.clone();
            let target = target.to_string();
            let sender = sender.to_string();
            tokio::spawn(async move {
                while let Some(notice) = notice_rx.recv().await {
                    match notice {
                        Notice::Text(text) => this.send_text(&target, chat_type, &text).await,
                        Notice::Approval {
                            session_id,
                            request_id,
                            tool,
                            args_preview,
                        } => {
                            let content = format!("需要审批：{tool}\n{args_preview}");
                            let keyboard =
                                super::protocol::approval_keyboard(&session_id, &request_id);
                            this.send_with_keyboard(&target, chat_type, &content, keyboard)
                                .await;
                        }
                        Notice::Ask {
                            session_id,
                            request_id,
                            questions,
                            ..
                        } => {
                            let Some(question) = questions.first() else {
                                continue;
                            };
                            let mut content = format!("需要你的回答：{}", question.question);
                            for (index, option) in question.options.iter().enumerate() {
                                content.push_str(&format!("\n{}. {}", index + 1, option.label));
                            }
                            content.push_str("\n回复数字选择，或直接点按钮。");
                            this.pending_asks
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .insert(
                                    request_id.clone(),
                                    PendingAsk {
                                        session_id: session_id.clone(),
                                        request_id: request_id.clone(),
                                        question: question.clone(),
                                        sender: sender.clone(),
                                    },
                                );
                            if let Some(keyboard) =
                                super::protocol::ask_keyboard(&session_id, &request_id, question)
                            {
                                this.send_with_keyboard(&target, chat_type, &content, keyboard)
                                    .await;
                            } else {
                                this.send_text(&target, chat_type, content).await;
                            }
                        }
                    }
                }
            })
        };
        let prompt = if text.trim().is_empty() && !images.is_empty() {
            "请看这张图片。"
        } else {
            text
        };
        let result = self
            .host
            .run_turn(
                &session_id,
                prompt,
                images,
                &model,
                &NoticeSink::new(notice_tx),
            )
            .await;
        notifier.abort();
        self.clear_pending_asks_for_session(&session_id);
        match result {
            Ok(reply) => {
                let mut out = reply.text.trim().to_string();
                if let Some(notice) = reply.outcome.notice() {
                    out = if out.is_empty() {
                        notice
                    } else {
                        format!("{out}\n\n{notice}")
                    };
                }
                if out.is_empty() {
                    out = "（本轮没有文本输出）".to_string();
                }
                self.set_action(format!("已回复 {} 字", out.chars().count()));
                self.send_text(target, chat_type, &out).await;
            }
            Err(error) => {
                if error.contains("模型") || error.to_ascii_lowercase().contains("model") {
                    self.bindings.update(sender, |binding| binding.model = None);
                }
                self.set_action(format!("执行失败：{error}"));
                self.send_text(target, chat_type, format!("执行失败：{error}"))
                    .await;
            }
        }
    }

    /// 处理按钮点击:ACK 后按 button_data 结算审批或 ask。
    pub async fn handle_interaction(self: &Arc<Self>, d: serde_json::Value) {
        let Some(interaction) = super::protocol::parse_interaction(&d) else {
            return;
        };
        if let Err(error) = self.client.ack_interaction(&interaction.id).await {
            tracing::warn!(%error, "QQ 交互 ACK 失败");
        }
        match super::protocol::parse_button_data(&interaction.button_data) {
            Some(super::protocol::ButtonAction::Approve {
                session_id,
                request_id,
                allow,
            }) => {
                let reply = match self
                    .host
                    .answer_approval(&session_id, Some(&request_id), allow)
                    .await
                {
                    Ok(true) => {
                        if allow {
                            "已允许。".to_string()
                        } else {
                            "已拒绝。".to_string()
                        }
                    }
                    Ok(false) => "该审批已处理或已过期。".to_string(),
                    Err(error) => format!("审批失败：{error}"),
                };
                self.send_text(&interaction.chat_id, interaction.chat_type, reply)
                    .await;
            }
            Some(super::protocol::ButtonAction::Ask {
                session_id,
                request_id,
                question_id,
                option_index,
            }) => {
                let option = self
                    .pending_asks
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .get(&request_id)
                    .and_then(|pending| pending.question.options.get(option_index))
                    .map(|option| option.label.clone());
                let Some(option) = option else {
                    self.send_text(
                        &interaction.chat_id,
                        interaction.chat_type,
                        "该提问已处理或选项已失效。",
                    )
                    .await;
                    return;
                };
                let reply = match self
                    .host
                    .answer_ask(&session_id, &request_id, &question_id, &option)
                    .await
                {
                    Ok(true) => {
                        self.pending_asks
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .remove(&request_id);
                        format!("已选择：{option}")
                    }
                    Ok(false) => "该提问已处理或已过期。".to_string(),
                    Err(error) => format!("提交回答失败：{error}"),
                };
                self.send_text(&interaction.chat_id, interaction.chat_type, reply)
                    .await;
            }
            None => {
                tracing::warn!(button_data = %interaction.button_data, "未知的 QQ 按钮数据");
            }
        }
    }

    /// 发送带键盘的单条消息(审批/ask 按钮)。失败时降级为文本并把原因写进
    /// 状态,避免"用户什么也看不到"。
    async fn send_with_keyboard(
        &self,
        target: &str,
        chat_type: ChatType,
        text: &str,
        keyboard: serde_json::Value,
    ) {
        let msg_id = self
            .last_msg_ids
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(target)
            .cloned();
        let seq = (epoch_millis() % 65_536) as u32;
        match self
            .client
            .send_text_with_keyboard(
                chat_type,
                target,
                text,
                msg_id.as_deref(),
                seq,
                Some(keyboard),
            )
            .await
        {
            Ok(()) => self.set_action("已发送按钮消息"),
            Err(error) => {
                tracing::warn!(%error, "QQ 键盘消息发送失败,降级为文本");
                self.set_action(format!("按钮消息发送失败：{error}"));
                // 降级:去掉键盘再发一次,保证用户至少能看到问题。
                if let Err(second) = self
                    .client
                    .send_text(chat_type, target, text, msg_id.as_deref(), seq)
                    .await
                {
                    self.set_error(format!("发送消息失败：{second}"));
                }
            }
        }
    }

    fn pending_ask_for(&self, sender: &str) -> Option<(String, PendingAsk)> {
        let pending = self
            .pending_asks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        pending
            .iter()
            .find(|(_, ask)| ask.sender == sender)
            .map(|(request_id, ask)| (request_id.clone(), ask.clone()))
    }

    fn clear_pending_asks_for_session(&self, session_id: &str) {
        self.pending_asks
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .retain(|_, ask| ask.session_id != session_id);
    }

    async fn send_text(&self, target: &str, chat_type: ChatType, text: impl AsRef<str>) {
        let text = text.as_ref();
        let chunks = split_text(text, MAX_MESSAGE_LENGTH);
        if chunks.is_empty() {
            return;
        }
        let msg_id = self
            .last_msg_ids
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(target)
            .cloned();
        for (index, chunk) in chunks.into_iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let reply = if index == 0 { msg_id.as_deref() } else { None };
            let seq = (epoch_millis() % 65_536) as u32;
            if let Err(error) = self
                .client
                .send_text(chat_type, target, &chunk, reply, seq)
                .await
            {
                tracing::warn!(%error, "QQ 消息发送失败");
                self.set_error(format!("发送消息失败：{error}"));
                return;
            }
        }
    }

    fn mark_inbound(&self, chars: usize) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        status.last_inbound_at = Some(epoch_millis());
        status.last_inbound_chars = chars;
        status.last_action = None;
    }

    fn set_action(&self, action: impl Into<String>) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        status.last_action = Some(action.into());
    }

    fn set_error(&self, error: String) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        status.connected = false;
        status.last_error = Some(error);
    }

    fn seen(&self, id: &str) -> bool {
        let now = Instant::now();
        let mut map = self
            .dedup
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        map.retain(|_, time| now.duration_since(*time) < DEDUP_TTL);
        if map.contains_key(id) {
            true
        } else {
            map.insert(id.to_string(), now);
            false
        }
    }
}

/// 把用户文本回答映射到选项:优先数字序号,其次精确匹配 label。
fn option_for_answer(question: &AskQuestion, text: &str) -> Option<String> {
    let text = text.trim();
    if let Ok(index) = text.parse::<usize>()
        && index >= 1
        && index <= question.options.len()
    {
        return Some(question.options[index - 1].label.clone());
    }
    question
        .options
        .iter()
        .find(|option| option.label.eq_ignore_ascii_case(text))
        .map(|option| option.label.clone())
}

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
