//! 微信通道主循环:长轮询入站、指令分发、会话轮次与出站分片。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use denia_core::config::ModelSelection;
use denia_core::session::PermissionMode;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::bindings::BindingStore;
use crate::bridge::{ChannelHost, ImageAttachment, Notice, NoticeSink};
use crate::commands::{self, Command};

use super::SharedStatus;
use super::client::Client;
use super::media;
use super::protocol::{
    ILINK_BASE_URL, ImageItem, InboundMessage, MAX_TEXT_CHARS, RATE_LIMIT_ERRCODE,
    SESSION_EXPIRED_ERRCODE,
};
use super::store::{self, Account};

/// 去重窗口:同一条消息在 5 分钟内重复投递只处理一次。
const DEDUP_TTL: Duration = Duration::from_secs(300);
/// 连续失败达到这个次数后把退避从 2 秒升到 30 秒。
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
/// 一条消息最多带入几张图片:微信单条消息可含多图,限制上限避免把一轮撑爆。
const MAX_IMAGES_PER_MESSAGE: usize = 4;

pub struct WeixinBot {
    host: Arc<dyn ChannelHost>,
    client: Client,
    account_id: String,
    dir: PathBuf,
    bindings: Arc<BindingStore>,
    status: SharedStatus,
    sync_buf: Mutex<String>,
    context_tokens: Mutex<HashMap<String, String>>,
    dedup: Mutex<HashMap<String, Instant>>,
    send_lock: tokio::sync::Mutex<()>,
    cancel: CancellationToken,
}

impl WeixinBot {
    pub fn new(
        host: Arc<dyn ChannelHost>,
        account: &Account,
        dir: PathBuf,
        bindings: Arc<BindingStore>,
        status: SharedStatus,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, String> {
        let base_url = if account.base_url.trim().is_empty() {
            ILINK_BASE_URL
        } else {
            account.base_url.as_str()
        };
        let client = Client::new(base_url, &account.token)?;
        Ok(Arc::new(Self {
            host,
            client,
            account_id: account.account_id.clone(),
            sync_buf: Mutex::new(store::load_sync(&dir)),
            dir,
            bindings,
            status,
            context_tokens: Mutex::new(HashMap::new()),
            dedup: Mutex::new(HashMap::new()),
            send_lock: tokio::sync::Mutex::new(()),
            cancel,
        }))
    }

    /// 长轮询主循环。取消后立即返回。
    pub async fn run(self: Arc<Self>) {
        let mut failures = 0u32;
        loop {
            let sync_buf = self
                .sync_buf
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone();
            let result = tokio::select! {
                _ = self.cancel.cancelled() => break,
                result = self.client.get_updates(&sync_buf) => result,
            };
            match result {
                Ok(response) => {
                    if response.is_error() {
                        if response.is_session_expired() {
                            self.set_error("微信会话已过期，请在设置页重新扫码登录".to_string());
                            self.sleep(Duration::from_secs(600)).await;
                            failures = 0;
                            continue;
                        }
                        failures += 1;
                        self.set_error(format!("拉取消息失败：{}", response.error_text()));
                        self.backoff(failures).await;
                        continue;
                    }
                    failures = 0;
                    self.clear_error();
                    for message in response.msgs {
                        let this = self.clone();
                        tokio::spawn(async move { this.handle_message(message).await });
                    }
                    if let Some(buffer) = response.get_updates_buf
                        && !buffer.is_empty()
                        && buffer != sync_buf
                    {
                        *self
                            .sync_buf
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner()) = buffer.clone();
                        store::save_sync(&self.dir, &buffer);
                    }
                }
                Err(error) => {
                    failures += 1;
                    self.set_error(error);
                    self.backoff(failures).await;
                }
            }
        }
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        status.connected = false;
    }

    fn set_error(&self, error: String) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        status.connected = false;
        status.last_error = Some(error);
    }

    /// 记录一条已解析入站消息,供面板区分“没收到”与“收到了但处理失败”。
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

    fn clear_error(&self) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !status.connected || status.last_error.is_some() {
            status.connected = true;
            status.last_error = None;
        }
    }

    async fn backoff(&self, failures: u32) {
        let delay = if failures >= MAX_CONSECUTIVE_FAILURES {
            Duration::from_secs(30)
        } else {
            Duration::from_secs(2)
        };
        self.sleep(delay).await;
    }

    async fn sleep(&self, duration: Duration) {
        tokio::select! {
            _ = self.cancel.cancelled() => {}
            _ = tokio::time::sleep(duration) => {}
        }
    }

    async fn handle_message(self: Arc<Self>, message: InboundMessage) {
        let Some(sender) = message
            .from_user_id
            .clone()
            .filter(|sender| !sender.trim().is_empty())
        else {
            return;
        };
        if sender == self.account_id {
            return;
        }
        if let Some(token) = message
            .context_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
        {
            self.context_tokens
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(sender.clone(), token.to_string());
        }
        let message_id = message.message_id.clone().unwrap_or_default();
        let text = message.text();
        let images = message.images();
        if text.trim().is_empty() && images.is_empty() {
            if message.has_media() {
                self.send_text(&sender, "目前支持文字与图片；语音、文件暂未接入。")
                    .await;
            }
            return;
        }
        // 有稳定的 message_id 时只按 id 去重:用户重复发同一句话是正常行为,
        // 不能当成上游重发吞掉。content 去重仅作为无 id 消息的兜底。
        if !message_id.is_empty() {
            if self.seen(&format!("id:{message_id}")) {
                return;
            }
        } else if self.seen(&format!("content:{sender}:{}", hash_text(&text))) {
            return;
        }
        self.mark_inbound(text.chars().count());
        self.handle_text(&sender, text.trim(), &images).await;
    }

    async fn handle_text(self: &Arc<Self>, sender: &str, text: &str, images: &[&ImageItem]) {
        if let Some(command) = commands::parse(command_source(text)) {
            self.handle_command(sender, command).await;
            return;
        }
        self.run_prompt(sender, text, images).await;
    }

    async fn handle_command(self: &Arc<Self>, sender: &str, command: Command) {
        self.set_action(format!("命令 {}", command.name()));
        match command {
            Command::Help => self.send_text(sender, commands::help_text()).await,
            Command::Workspace(None) => {
                let workspaces = self.host.workspaces().await;
                let current = self.bindings.get(sender).workspace_id;
                self.send_text(
                    sender,
                    commands::workspace_list(&workspaces, current.as_deref()),
                )
                .await;
            }
            Command::Workspace(Some(argument)) => self.select_workspace(sender, &argument).await,
            Command::Switch(None) => self.list_sessions(sender).await,
            Command::Switch(Some(argument)) => self.switch_session(sender, &argument).await,
            Command::Model(None) => self.show_models(sender).await,
            Command::Model(Some(argument)) => self.change_model(sender, &argument).await,
            Command::Permission(None) => self.show_permission(sender).await,
            Command::Permission(Some(argument)) => self.change_permission(sender, &argument).await,
            Command::Cancel => self.cancel_turn(sender).await,
            Command::Approve => self.answer_approval(sender, true).await,
            Command::Reject => self.answer_approval(sender, false).await,
            Command::Unknown(name) => {
                self.send_text(
                    sender,
                    format!("未知命令 /{name}。\n\n{}", commands::help_text()),
                )
                .await;
            }
        }
    }

    async fn select_workspace(self: &Arc<Self>, sender: &str, argument: &str) {
        let workspaces = self.host.workspaces().await;
        let found = match argument.trim().parse::<usize>() {
            Ok(index) if index >= 1 => workspaces.get(index - 1),
            _ => workspaces.iter().find(|workspace| {
                workspace.title.eq_ignore_ascii_case(argument.trim())
                    || workspace.path.eq_ignore_ascii_case(argument.trim())
                    || workspace.title.contains(argument.trim())
            }),
        };
        match found {
            Some(workspace) => {
                let (id, title, path) = (
                    workspace.id.clone(),
                    workspace.title.clone(),
                    workspace.path.clone(),
                );
                self.bindings.update(sender, |binding| {
                    binding.workspace_id = Some(id);
                    binding.session_id = None;
                });
                self.send_text(
                    sender,
                    format!(
                        "已选择工作区「{title}」({path})。\n尚未选择会话：下一条消息会新建会话。\n用 /switch 查看该工作区的会话。"
                    ),
                )
                .await;
            }
            None => {
                self.send_text(sender, "没有找到该工作区。发送 /workspace 查看列表。")
                    .await;
            }
        }
    }

    async fn current_workspace(
        &self,
        sender: &str,
    ) -> Result<crate::bridge::WorkspaceInfo, String> {
        let binding = self.bindings.get(sender);
        let Some(workspace_id) = binding.workspace_id else {
            return Err("请先发送 /workspace 选择工作区。".to_string());
        };
        let workspaces = self.host.workspaces().await;
        workspaces
            .into_iter()
            .find(|workspace| workspace.id == workspace_id)
            .ok_or_else(|| {
                self.bindings.update(sender, |binding| {
                    binding.workspace_id = None;
                    binding.session_id = None;
                });
                "该工作区已不存在，请重新用 /workspace 选择。".to_string()
            })
    }

    async fn list_sessions(self: &Arc<Self>, sender: &str) {
        let workspace = match self.current_workspace(sender).await {
            Ok(workspace) => workspace,
            Err(message) => {
                self.send_text(sender, message).await;
                return;
            }
        };
        match self.host.sessions(&workspace.id).await {
            Ok(sessions) => {
                let current = self.bindings.get(sender).session_id;
                self.send_text(
                    sender,
                    commands::session_list(&workspace, &sessions, current.as_deref()),
                )
                .await;
            }
            Err(error) => {
                self.send_text(sender, format!("读取会话失败：{error}")).await;
            }
        }
    }

    async fn switch_session(self: &Arc<Self>, sender: &str, argument: &str) {
        let workspace = match self.current_workspace(sender).await {
            Ok(workspace) => workspace,
            Err(message) => {
                self.send_text(sender, message).await;
                return;
            }
        };
        let argument = argument.trim();
        if argument.eq_ignore_ascii_case("new") {
            self.bindings.update(sender, |binding| binding.session_id = None);
            self.send_text(sender, "已切到新会话：下一条消息会新建会话。")
                .await;
            return;
        }
        let sessions = match self.host.sessions(&workspace.id).await {
            Ok(sessions) => sessions,
            Err(error) => {
                self.send_text(sender, format!("读取会话失败：{error}")).await;
                return;
            }
        };
        let found = match argument.parse::<usize>() {
            Ok(index) if index >= 1 => sessions.get(index - 1),
            _ => sessions
                .iter()
                .find(|session| session.display_name().contains(argument)),
        };
        match found {
            Some(session) => {
                let (id, name) = (session.id.clone(), session.display_name());
                self.bindings.update(sender, |binding| {
                    binding.session_id = Some(id);
                });
                self.send_text(sender, format!("已切换到会话「{name}」。"))
                    .await;
            }
            None => {
                self.send_text(sender, "没有找到该会话。发送 /switch 查看列表。")
                    .await;
            }
        }
    }

    async fn show_models(self: &Arc<Self>, sender: &str) {
        let binding = self.bindings.get(sender);
        let models = self.host.models().await;
        self.send_text(sender, commands::model_list(binding.model.as_ref(), &models))
            .await;
    }

    async fn change_model(self: &Arc<Self>, sender: &str, argument: &str) {
        let argument = argument.trim();
        let Some((provider, model)) = argument.split_once(char::is_whitespace) else {
            self.send_text(
                sender,
                "用法：/model <供应商id> <模型id>，例如 /model Astra deepseek_v4_flash。",
            )
            .await;
            return;
        };
        let (provider, model) = (provider.trim(), model.trim());
        if provider.is_empty() || model.is_empty() {
            self.send_text(sender, "供应商 id 与模型 id 都不能为空。")
                .await;
            return;
        }
        // 必须在模型目录里,但允许省略 `cn:` 这类前缀或用显示名。
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
                    sender,
                    format!(
                        "模型已切换为 {}/{}，下一轮消息生效。",
                        option.provider, option.model
                    ),
                )
                .await;
            }
            commands::ModelLookup::NotFound => {
                self.send_text(
                    sender,
                    format!("模型目录里没有 {provider}/{model}。发送 /model 查看可用列表。"),
                )
                .await;
            }
            commands::ModelLookup::Ambiguous(options) => {
                let mut list = String::new();
                for option in options.iter().take(10) {
                    list.push_str(&format!("- {} {}\n", option.provider, option.model));
                }
                self.send_text(sender, format!("匹配到多个模型，请使用完整 id：\n{list}"))
                    .await;
            }
        }
    }

    async fn show_permission(self: &Arc<Self>, sender: &str) {
        let binding = self.bindings.get(sender);
        let current = match &binding.session_id {
            Some(session_id) => self
                .host
                .session_permission(session_id)
                .await
                .unwrap_or_else(|_| binding.permission.unwrap_or(PermissionMode::AutoEdit)),
            None => binding.permission.unwrap_or(PermissionMode::AutoEdit),
        };
        self.send_text(sender, commands::permission_list(current)).await;
    }

    async fn change_permission(self: &Arc<Self>, sender: &str, argument: &str) {
        let Some(mode) = PermissionMode::parse(argument) else {
            self.send_text(
                sender,
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
                        sender,
                        format!(
                            "当前会话权限已切换为{}（{}）。",
                            commands::permission_label(mode),
                            mode.as_str()
                        ),
                    )
                    .await;
                }
                Err(error) => {
                    self.send_text(sender, format!("切换权限失败：{error}")).await;
                }
            }
        } else {
            self.send_text(
                sender,
                format!(
                    "已设置新建会话的权限为{}（{}）。",
                    commands::permission_label(mode),
                    mode.as_str()
                ),
            )
            .await;
        }
    }

    async fn cancel_turn(self: &Arc<Self>, sender: &str) {
        let Some(session_id) = self.bindings.get(sender).session_id else {
            self.send_text(sender, "当前没有绑定会话。").await;
            return;
        };
        match self.host.cancel(&session_id).await {
            Ok(()) => self.send_text(sender, "已请求中断当前轮次。").await,
            Err(error) => {
                self.send_text(sender, format!("中断失败：{error}")).await;
            }
        }
    }

    async fn answer_approval(self: &Arc<Self>, sender: &str, approve: bool) {
        let Some(session_id) = self.bindings.get(sender).session_id else {
            self.send_text(sender, "当前没有绑定会话。").await;
            return;
        };
        match self.host.answer_approval(&session_id, None, approve).await {
            Ok(true) => {
                self.send_text(sender, if approve { "已放行。" } else { "已拒绝。" })
                    .await;
            }
            Ok(false) => {
                self.send_text(sender, "当前没有待审批的请求。").await;
            }
            Err(error) => {
                self.send_text(sender, format!("结算审批失败：{error}")).await;
            }
        }
    }

    async fn run_prompt(self: &Arc<Self>, sender: &str, text: &str, images: &[&ImageItem]) {
        let binding = self.bindings.get(sender);
        let Some(workspace_id) = binding.workspace_id.clone() else {
            self.set_action("未选择工作区");
            self.send_text(sender, "请先发送 /workspace 选择工作区。")
                .await;
            return;
        };
        let workspaces = self.host.workspaces().await;
        if !workspaces.iter().any(|workspace| workspace.id == workspace_id) {
            self.set_action("工作区已不存在");
            self.bindings.update(sender, |binding| {
                binding.workspace_id = None;
                binding.session_id = None;
            });
            self.send_text(sender, "该工作区已不存在，请重新用 /workspace 选择。")
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
                        sender,
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
            None => match self
                .host
                .create_session(&workspace_id, permission)
                .await
            {
                Ok(info) => {
                    self.bindings.update(sender, |binding| {
                        binding.session_id = Some(info.id.clone());
                    });
                    info.id
                }
                Err(error) => {
                    self.set_action(format!("新建会话失败：{error}"));
                    self.send_text(sender, format!("新建会话失败：{error}")).await;
                    return;
                }
            },
        };
        let mut attachments = Vec::new();
        for image in images.iter().take(MAX_IMAGES_PER_MESSAGE) {
            match self.download_image(image).await {
                Ok(attachment) => attachments.push(attachment),
                Err(error) => tracing::warn!(%error, "微信图片下载失败"),
            }
        }
        if !images.is_empty() && attachments.is_empty() {
            self.set_action("图片下载失败");
            self.send_text(sender, "图片下载失败，请重试或改发文字。")
                .await;
            return;
        }
        self.run_turn(sender, text, &attachments, &model, session_id)
            .await;
    }

    /// 提交一轮并等待结果:文字与图片共用同一条路径。
    async fn run_turn(
        self: &Arc<Self>,
        sender: &str,
        text: &str,
        images: &[ImageAttachment],
        model: &ModelSelection,
        session_id: String,
    ) {
        if self.host.session_running(&session_id).await {
            self.set_action("上一轮仍在执行");
            self.send_text(
                sender,
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
            let sender = sender.to_string();
            tokio::spawn(async move {
                while let Some(notice) = notice_rx.recv().await {
                    let text = match notice {
                        Notice::Text(text) => text,
                        Notice::Approval {
                            tool, args_preview, ..
                        } => format!(
                            "需要审批：工具 {tool}\n参数预览：{args_preview}\n回复 /approve 放行一次，/reject 拒绝。"
                        ),
                        Notice::Ask { questions, .. } => {
                            let mut out = String::from("需要你的回答：");
                            for question in &questions {
                                out.push_str(&format!("\n\n[{}] {}", question.id, question.question));
                                for option in &question.options {
                                    out.push_str(&format!("\n- {}", option.label));
                                }
                            }
                            out.push_str("\n\n（微信端暂不支持按钮作答，请在控制台回答）");
                            out
                        }
                    };
                    this.send_text(&sender, &text).await;
                }
            })
        };
        // 只有图片没有文字时给模型一个明确指令。
        let prompt = if text.trim().is_empty() && !images.is_empty() {
            "请看这张图片。"
        } else {
            text
        };
        let result = self
            .host
            .run_turn(&session_id, prompt, images, model, &NoticeSink::new(notice_tx))
            .await;
        notifier.abort();
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
                self.send_text(sender, &out).await;
            }
            Err(error) => {
                // 模型路由失败时清掉记住的选择:下次消息重新从目录里挑一个,
                // 避免配置变更后一直撞同一个失效模型。
                if error.contains("模型") || error.to_ascii_lowercase().contains("model") {
                    self.bindings
                        .update(sender, |binding| binding.model = None);
                }
                self.set_action(format!("执行失败：{error}"));
                self.send_text(sender, &format!("执行失败：{error}")).await;
            }
        }
    }

    /// 下载并解密一张图片。
    async fn download_image(&self, image: &ImageItem) -> Result<ImageAttachment, String> {
        let url = media::image_url(image)?;
        let raw = self.client.download(&url).await?;
        let data = media::decrypt_image(image, &raw)?;
        Ok(ImageAttachment {
            mime: media::guess_mime(&data).to_string(),
            data,
        })
    }

    async fn send_text(&self, to: &str, text: impl AsRef<str>) {
        let text = text.as_ref();
        let context_token = self
            .context_tokens
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(to)
            .cloned();
        let chunks = split_text(text, MAX_TEXT_CHARS);
        if chunks.is_empty() {
            return;
        }
        // iLink 有频率限制:同一通道的出站串行发送,片间留间隔。
        let _guard = self.send_lock.lock().await;
        for (index, chunk) in chunks.into_iter().enumerate() {
            if index > 0 {
                tokio::time::sleep(Duration::from_millis(600)).await;
            }
            if let Err(error) = self.send_chunk(to, &chunk, context_token.clone()).await {
                tracing::warn!(%error, "微信消息发送失败");
                self.set_error(format!("发送消息失败：{error}"));
                return;
            }
        }
    }

    async fn send_chunk(
        &self,
        to: &str,
        text: &str,
        context_token: Option<String>,
    ) -> Result<(), String> {
        let mut context_token = context_token;
        let mut retried_without_token = false;
        let mut attempt = 0u32;
        loop {
            match self
                .client
                .send_text(to, text, context_token.as_deref(), &client_id())
                .await
            {
                Ok(value) => {
                    let (ret, errcode) = response_codes(&value);
                    if ret == 0 && errcode == 0 {
                        return Ok(());
                    }
                    let expired =
                        ret == SESSION_EXPIRED_ERRCODE || errcode == SESSION_EXPIRED_ERRCODE;
                    let limited = ret == RATE_LIMIT_ERRCODE || errcode == RATE_LIMIT_ERRCODE;
                    if (expired || limited) && !retried_without_token && context_token.is_some() {
                        retried_without_token = true;
                        context_token = None;
                        continue;
                    }
                    if attempt >= 3 {
                        return Err(format!("ret={ret} errcode={errcode}"));
                    }
                    attempt += 1;
                    tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                }
                Err(error) => {
                    if attempt >= 3 {
                        return Err(error);
                    }
                    attempt += 1;
                    tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                }
            }
        }
    }

    fn seen(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut dedup = self
            .dedup
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        dedup.retain(|_, seen_at| now.duration_since(*seen_at) < DEDUP_TTL);
        if dedup.contains_key(key) {
            return true;
        }
        dedup.insert(key.to_string(), now);
        false
    }
}

fn response_codes(value: &Value) -> (i64, i64) {
    (
        value.get("ret").and_then(Value::as_i64).unwrap_or(0),
        value.get("errcode").and_then(Value::as_i64).unwrap_or(0),
    )
}

/// 引用消息的正文在 `[引用: …]\n` 之后;命令识别只看正文,整条文本仍原样
/// 交给模型(引用内容是有用上下文)。
fn command_source(text: &str) -> &str {
    let trimmed = text.trim_start();
    if trimmed.starts_with('[')
        && let Some(index) = trimmed.find("]\n")
    {
        return trimmed[index + 2..].trim_start();
    }
    trimmed
}

fn hash_text(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

fn epoch_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn client_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("denia-weixin-{nanos}-{:016x}", rand::random::<u64>())
}

/// 出站分片:先按行打包到上限,超长行硬切;跨片保持代码围栏闭合。
pub fn split_text(content: &str, max: usize) -> Vec<String> {
    let mut chunks = split_raw(content.trim(), max);
    rebalance_fences(&mut chunks);
    chunks
}

fn split_raw(content: &str, max: usize) -> Vec<String> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for line in content.split('\n') {
        let line_len = line.chars().count();
        let separator = usize::from(!current.is_empty());
        if current_len + separator + line_len <= max {
            if separator == 1 {
                current.push('\n');
            }
            current.push_str(line);
            current_len += separator + line_len;
            continue;
        }
        if !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if line_len <= max {
            current.push_str(line);
            current_len = line_len;
            continue;
        }
        let mut buffer = String::new();
        let mut buffer_len = 0usize;
        for character in line.chars() {
            if buffer_len == max {
                chunks.push(std::mem::take(&mut buffer));
                buffer_len = 0;
            }
            buffer.push(character);
            buffer_len += 1;
        }
        if !buffer.is_empty() {
            current = buffer;
            current_len = buffer_len;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// 若某片在代码围栏内结束,补 ```;下一片开头重开围栏。
fn rebalance_fences(chunks: &mut [String]) {
    let mut open = false;
    for chunk in chunks.iter_mut() {
        let mut rebuilt = String::new();
        if open {
            rebuilt.push_str("```\n");
        }
        let mut in_fence = open;
        for line in chunk.lines() {
            if line.trim_start().starts_with("```") {
                in_fence = !in_fence;
            }
            rebuilt.push_str(line);
            rebuilt.push('\n');
        }
        if rebuilt.ends_with('\n') {
            rebuilt.pop();
        }
        if in_fence {
            rebuilt.push_str("\n```");
        }
        open = in_fence;
        *chunk = rebuilt;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_without_exceeding_limit() {
        let text = (0..100)
            .map(|index| format!("第 {index} 行内容"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = split_text(&text, 80);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(
                chunk.chars().count() <= 88,
                "分片超限: {} 字符",
                chunk.chars().count()
            );
        }
    }

    #[test]
    fn keeps_fences_balanced_across_chunks() {
        let mut text = String::from("说明\n```rust\n");
        for index in 0..200 {
            text.push_str(&format!("let value_{index} = {index};\n"));
        }
        text.push_str("```\n结尾");
        let chunks = split_text(&text, 120);
        assert!(chunks.len() > 1);
        let total_fences: usize = chunks
            .iter()
            .map(|chunk| {
                chunk
                    .lines()
                    .filter(|line| line.trim_start().starts_with("```"))
                    .count()
            })
            .sum();
        assert!(total_fences >= 4, "围栏应当成对出现: {total_fences}");
    }

    #[test]
    fn dedup_remembers_message_ids() {
        let host = NoopHost;
        let dir = std::env::temp_dir().join(format!("denia-bot-dedup-{}", std::process::id()));
        let bot = WeixinBot::new(
            Arc::new(host),
            &Account {
                account_id: "bot".into(),
                token: "token".into(),
                base_url: ILINK_BASE_URL.into(),
                user_id: String::new(),
                saved_at: String::new(),
            },
            dir.clone(),
            Arc::new(BindingStore::open(dir.join("bindings.json"))),
            Arc::new(Mutex::new(Default::default())),
            CancellationToken::new(),
        )
        .unwrap();
        assert!(!bot.seen("id:1"));
        assert!(bot.seen("id:1"));
        assert!(!bot.seen("id:2"));
    }

    struct NoopHost;

    #[async_trait::async_trait]
    impl ChannelHost for NoopHost {
        async fn workspaces(&self) -> Vec<crate::bridge::WorkspaceInfo> {
            Vec::new()
        }
        async fn sessions(
            &self,
            _workspace_id: &str,
        ) -> Result<Vec<crate::bridge::SessionInfo>, String> {
            Err("noop".into())
        }
        async fn create_session(
            &self,
            _workspace_id: &str,
            _permission: PermissionMode,
        ) -> Result<crate::bridge::SessionInfo, String> {
            Err("noop".into())
        }
        async fn session_permission(&self, _session_id: &str) -> Result<PermissionMode, String> {
            Err("noop".into())
        }
        async fn set_permission(
            &self,
            _session_id: &str,
            _mode: PermissionMode,
        ) -> Result<(), String> {
            Err("noop".into())
        }
        async fn session_running(&self, _session_id: &str) -> bool {
            false
        }
        async fn models(&self) -> Vec<crate::bridge::ModelOption> {
            Vec::new()
        }
        async fn cancel(&self, _session_id: &str) -> Result<(), String> {
            Err("noop".into())
        }
        async fn answer_approval(
            &self,
            _session_id: &str,
            _request_id: Option<&str>,
            _approve: bool,
        ) -> Result<bool, String> {
            Err("noop".into())
        }

        async fn answer_ask(
            &self,
            _session_id: &str,
            _request_id: &str,
            _question_id: &str,
            _option: &str,
        ) -> Result<bool, String> {
            Err("noop".into())
        }
        async fn default_model(&self) -> Result<denia_core::config::ModelSelection, String> {
            Err("noop".into())
        }
        async fn run_turn(
            &self,
            _session_id: &str,
            _text: &str,
            _images: &[crate::bridge::ImageAttachment],
            _model: &denia_core::config::ModelSelection,
            _notices: &NoticeSink,
        ) -> Result<crate::bridge::TurnReply, String> {
            Err("noop".into())
        }
    }
}
