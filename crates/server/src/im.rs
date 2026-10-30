//! 消息通道(微信)服务:账号、扫码登录、长轮询循环与进程内会话桥。
//!
//! 通道的协议与指令逻辑在 `denia-channels`;这里只做三件事:
//! 1. 把 `AppState` 的能力实现成 [`ChannelHost`](denia_channels::bridge::ChannelHost);
//! 2. 管理账号/登录/运行循环的生命周期;
//! 3. 给 HTTP API 暴露状态。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine;
use denia_channels::bindings::BindingStore;
use denia_channels::bridge::{
    ChannelHost, Notice, NoticeSink, SessionInfo, TurnOutcome, TurnReply, WorkspaceInfo,
};
use denia_channels::qqbot::bot::QqBot;
use denia_channels::qqbot::client::Client as QqClient;
use denia_channels::qqbot::gateway;
use denia_channels::qqbot::store as qq_store;
use denia_channels::weixin::bot::WeixinBot;
use denia_channels::weixin::login::spawn_login;
use denia_channels::weixin::protocol::QrState;
use denia_channels::weixin::store::{self, Account};
use denia_channels::weixin::{RuntimeStatus, SharedStatus};
use denia_core::config::ModelSelection;
use denia_core::session::{
    ApprovalOutcome, PermissionMode, PlanReviewDecision, SessionEvent, TurnEndReason,
};
use denia_core::stream::ContentBlock;
use tokio_util::sync::CancellationToken;

use crate::application::sessions::PromptImage;
use crate::state::{AppState, ServerEvent};

/// 通道服务:一个进程一份(桌面端与纯服务宿主共用)。
pub struct ImService {
    weixin_dir: PathBuf,
    bindings: Arc<BindingStore>,
    account: Mutex<Option<Account>>,
    login: Mutex<Option<LoginHandle>>,
    bot: Mutex<Option<BotHandle>>,
    status: SharedStatus,
    qqbot_dir: PathBuf,
    qq_bindings: Arc<BindingStore>,
    qq_account: Mutex<Option<qq_store::Account>>,
    qq_bot: Mutex<Option<BotHandle>>,
    qq_status: SharedStatus,
}

struct LoginHandle {
    cancel: CancellationToken,
    state: Arc<Mutex<QrState>>,
}

struct BotHandle {
    cancel: CancellationToken,
}

impl ImService {
    pub fn new(home: &Path) -> Arc<Self> {
        let weixin_dir = store::channel_dir(home);
        let bindings = Arc::new(BindingStore::open(weixin_dir.join("bindings.json")));
        let account = store::load_account(&weixin_dir);
        let qqbot_dir = qq_store::channel_dir(home);
        let qq_bindings = Arc::new(BindingStore::open(qqbot_dir.join("bindings.json")));
        let qq_account = qq_store::load_account(&qqbot_dir);
        Arc::new(Self {
            weixin_dir,
            bindings,
            account: Mutex::new(account),
            login: Mutex::new(None),
            bot: Mutex::new(None),
            status: Arc::new(Mutex::new(RuntimeStatus::default())),
            qqbot_dir,
            qq_bindings,
            qq_account: Mutex::new(qq_account),
            qq_bot: Mutex::new(None),
            qq_status: Arc::new(Mutex::new(RuntimeStatus::default())),
        })
    }

    pub fn weixin_account(&self) -> Option<Account> {
        self.account
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn weixin_login(&self) -> Option<QrState> {
        self.login
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(|handle| {
                handle
                    .state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clone()
            })
    }

    pub fn weixin_status(&self) -> (bool, Option<String>) {
        self.status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .snapshot()
    }

    /// 最近一条入站消息的诊断信息(时刻、字符数、处理动作)。
    pub fn weixin_activity(&self) -> (Option<u64>, usize, Option<String>) {
        let status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        (
            status.last_inbound_at,
            status.last_inbound_chars,
            status.last_action.clone(),
        )
    }

    pub fn qq_account(&self) -> Option<qq_store::Account> {
        self.qq_account
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn qq_status(&self) -> (bool, Option<String>) {
        self.qq_status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .snapshot()
    }

    pub fn qq_activity(&self) -> (Option<u64>, usize, Option<String>) {
        let status = self
            .qq_status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        (
            status.last_inbound_at,
            status.last_inbound_chars,
            status.last_action.clone(),
        )
    }

    /// 保存 QQ 凭据并启动网关;连接失败会反映在状态里。
    pub async fn start_qq(
        self: &Arc<Self>,
        host: Arc<dyn ChannelHost>,
        app_id: String,
        client_secret: String,
    ) -> Result<(), String> {
        let app_id = app_id.trim().to_string();
        let client_secret = client_secret.trim().to_string();
        if app_id.is_empty() || client_secret.is_empty() {
            return Err("AppID 与 ClientSecret 都不能为空".to_string());
        }
        let account = qq_store::Account {
            app_id,
            client_secret,
            saved_at: String::new(),
        };
        qq_store::save_account(&self.qqbot_dir, &account)?;
        *self
            .qq_account
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(account.clone());
        self.start_qq_bot(host, account);
        Ok(())
    }

    pub async fn logout_qq(self: &Arc<Self>) {
        if let Some(handle) = self
            .qq_bot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            handle.cancel.cancel();
        }
        if let Err(error) = qq_store::clear_account(&self.qqbot_dir) {
            tracing::warn!(%error, "删除 QQ 账号失败");
        }
        *self
            .qq_account
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        let mut status = self
            .qq_status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        status.connected = false;
        status.last_error = None;
    }

    fn start_qq_bot(self: &Arc<Self>, host: Arc<dyn ChannelHost>, account: qq_store::Account) {
        if let Some(handle) = self
            .qq_bot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            handle.cancel.cancel();
        }
        let client = match QqClient::new(account.app_id, account.client_secret) {
            Ok(client) => Arc::new(client),
            Err(error) => {
                let mut status = self
                    .qq_status
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                status.connected = false;
                status.last_error = Some(error);
                return;
            }
        };
        let bot = QqBot::new(
            host,
            self.qq_bindings.clone(),
            client.clone(),
            self.qq_status.clone(),
        );
        let cancel = CancellationToken::new();
        *self
            .qq_bot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(BotHandle {
            cancel: cancel.clone(),
        });
        tokio::spawn(gateway::run(client, bot, self.qq_status.clone(), cancel));
    }

    /// 服务启动时若已有账号则直接连上。
    pub async fn auto_connect(self: &Arc<Self>, host: Arc<dyn ChannelHost>) {
        if let Some(account) = self.weixin_account() {
            self.start_bot(host.clone(), account).await;
        }
        if let Some(account) = self.qq_account() {
            self.start_qq_bot(host, account);
        }
    }

    /// 启动扫码登录;成功后自动保存账号并开始收消息。
    pub fn start_login(self: &Arc<Self>, host: Arc<dyn ChannelHost>) {
        if let Some(handle) = self
            .login
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            handle.cancel.cancel();
        }
        let state = Arc::new(Mutex::new(QrState::waiting()));
        let cancel = CancellationToken::new();
        *self
            .login
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(LoginHandle {
            cancel: cancel.clone(),
            state: state.clone(),
        });
        let base_url = self
            .weixin_account()
            .map(|account| account.base_url)
            .unwrap_or_default();
        let service = self.clone();
        let dir = self.weixin_dir.clone();
        tokio::spawn(async move {
            let login = spawn_login(dir, base_url, state.clone(), cancel);
            let _ = login.await;
            let account = state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .account
                .clone();
            if let Some(account) = account {
                *service
                    .account
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = Some(account.clone());
                service.start_bot(host, account).await;
            }
        });
    }

    /// 断开并删除账号凭据。
    pub async fn logout(self: &Arc<Self>) {
        if let Some(handle) = self
            .login
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            handle.cancel.cancel();
        }
        if let Some(handle) = self
            .bot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            handle.cancel.cancel();
        }
        if let Err(error) = store::clear_account(&self.weixin_dir) {
            tracing::warn!(%error, "删除微信账号失败");
        }
        // 登录快照一并清掉:断开后不该残留上一次的二维码与"连接成功"。
        *self
            .login
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        *self
            .account
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        status.connected = false;
        status.last_error = None;
    }

    async fn start_bot(self: &Arc<Self>, host: Arc<dyn ChannelHost>, account: Account) {
        if let Some(handle) = self
            .bot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
        {
            handle.cancel.cancel();
        }
        let cancel = CancellationToken::new();
        let bot = match WeixinBot::new(
            host,
            &account,
            self.weixin_dir.clone(),
            self.bindings.clone(),
            self.status.clone(),
            cancel.clone(),
        ) {
            Ok(bot) => bot,
            Err(error) => {
                self.status
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .last_error = Some(error);
                return;
            }
        };
        *self.bot.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(BotHandle { cancel });
        // 账号已就绪:面板立即显示已连接;首次拉取失败会由循环改回 false。
        {
            let mut status = self
                .status
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            status.connected = true;
            status.last_error = None;
        }
        tokio::spawn(async move { bot.run().await });
    }
}

/// 把 `AppState` 的会话能力实现成通道宿主。
pub struct ServerChannelHost(pub Arc<AppState>);

fn same_path(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    crate::workspace::normalize_path(Path::new(a)) == crate::workspace::normalize_path(Path::new(b))
}

#[async_trait]
impl ChannelHost for ServerChannelHost {
    async fn workspaces(&self) -> Vec<WorkspaceInfo> {
        let sessions = self.0.sessions.list().unwrap_or_default();
        self.0
            .workspaces
            .list()
            .into_iter()
            .map(|workspace| {
                let session_count = sessions
                    .iter()
                    .filter(|summary| summary.subagent.is_none())
                    .filter(|summary| {
                        summary
                            .cwd
                            .as_deref()
                            .is_some_and(|cwd| same_path(cwd, &workspace.path))
                    })
                    .count();
                WorkspaceInfo {
                    id: workspace.id,
                    title: workspace.title,
                    path: workspace.path,
                    session_count,
                }
            })
            .collect()
    }

    async fn sessions(&self, workspace_id: &str) -> Result<Vec<SessionInfo>, String> {
        let workspace = self
            .0
            .workspaces
            .get(workspace_id)
            .ok_or_else(|| "工作区不存在".to_string())?;
        let summaries = self.0.sessions.list().map_err(|error| error.to_string())?;
        let mut rows: Vec<SessionInfo> = summaries
            .into_iter()
            .filter(|summary| summary.subagent.is_none())
            .filter(|summary| {
                summary
                    .cwd
                    .as_deref()
                    .is_some_and(|cwd| same_path(cwd, &workspace.path))
            })
            .map(|summary| SessionInfo {
                running: self
                    .0
                    .live
                    .get(&summary.id)
                    .is_some_and(|live| live.running.load(Ordering::SeqCst)),
                id: summary.id,
                title: summary.title,
                excerpt: summary.excerpt,
                created_at: summary.created_at,
            })
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.created_at));
        rows.truncate(20);
        Ok(rows)
    }

    async fn create_session(
        &self,
        workspace_id: &str,
        permission: PermissionMode,
    ) -> Result<SessionInfo, String> {
        let workspace = self
            .0
            .workspaces
            .get(workspace_id)
            .ok_or_else(|| "工作区不存在".to_string())?;
        let session = self
            .0
            .sessions
            .create(Path::new(&workspace.path), true)
            .map_err(|error| error.to_string())?;
        session
            .set_permission_mode(permission)
            .map_err(|error| error.to_string())?;
        let preset = self.0.agent_presets.default_id();
        session
            .set_agent_preset(&preset)
            .map_err(|error| error.to_string())?;
        if !self.0.workspaces.attach(&workspace.id, session.id()) {
            let _ = self.0.sessions.delete(session.id());
            return Err("工作区已被删除".to_string());
        }
        let _ = self.0.events.send(ServerEvent::SessionsUpdated);
        Ok(SessionInfo {
            id: session.id().to_string(),
            title: None,
            excerpt: None,
            created_at: session.header().created_at,
            running: false,
        })
    }

    async fn session_permission(&self, session_id: &str) -> Result<PermissionMode, String> {
        let live = self
            .0
            .live
            .get_or_load(&self.0.sessions, session_id)
            .map_err(|error| error.to_string())?;
        Ok(live.session.permission_mode())
    }

    async fn set_permission(&self, session_id: &str, mode: PermissionMode) -> Result<(), String> {
        let live = self
            .0
            .live
            .get_or_load(&self.0.sessions, session_id)
            .map_err(|error| error.to_string())?;
        self.0
            .live
            .ensure_hot(&live)
            .map_err(|error| error.to_string())?;
        let envelope = live
            .session
            .set_permission_mode(mode)
            .map_err(|error| error.to_string())?;
        let _ = live.followers.send(envelope);
        Ok(())
    }

    async fn session_running(&self, session_id: &str) -> bool {
        self.0
            .live
            .get(session_id)
            .is_some_and(|live| live.running.load(Ordering::SeqCst))
    }

    async fn models(&self) -> Vec<denia_channels::bridge::ModelOption> {
        let catalog = denia_llm::catalog::build_model_catalog(&self.0.registry).await;
        catalog
            .groups
            .into_iter()
            .flat_map(|group| {
                let provider = group.id.clone();
                let provider_name = group.name.clone();
                group
                    .models
                    .into_iter()
                    .map(move |model| denia_channels::bridge::ModelOption {
                        provider: provider.clone(),
                        provider_name: provider_name.clone(),
                        model: model.id,
                        model_name: model.name,
                    })
            })
            .collect()
    }

    async fn cancel(&self, session_id: &str) -> Result<(), String> {
        let live = self
            .0
            .live
            .get_or_load(&self.0.sessions, session_id)
            .map_err(|error| error.to_string())?;
        if let Some(token) = live
            .cancel
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
        {
            token.cancel();
        }
        {
            let mut pending = live
                .pending_approvals
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            for (_, sender) in pending.drain() {
                let _ = sender.send(PlanReviewDecision {
                    outcome: ApprovalOutcome::Cancelled,
                    execute_mode: None,
                    selection: None,
                    vision_supported: None,
                    feedback: None,
                });
            }
        }
        {
            let mut pending = live
                .pending_asks
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            for (_, sender) in pending.drain() {
                let _ = sender.send(denia_core::session::AskResolution {
                    outcome: denia_core::session::AskOutcome::Cancelled,
                    answers: Vec::new(),
                    reason: None,
                });
            }
        }
        Ok(())
    }

    async fn answer_approval(
        &self,
        session_id: &str,
        request_id: Option<&str>,
        approve: bool,
    ) -> Result<bool, String> {
        let Some(live) = self.0.live.get(session_id) else {
            return Ok(false);
        };
        let sender = {
            let mut pending = live
                .pending_approvals
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match request_id {
                Some(request_id) => pending.remove(request_id),
                None => {
                    let key = pending.keys().next().cloned();
                    key.and_then(|key| pending.remove(&key))
                }
            }
        };
        let Some(sender) = sender else {
            return Ok(false);
        };
        let decision = PlanReviewDecision {
            outcome: if approve {
                ApprovalOutcome::AllowedOnce
            } else {
                ApprovalOutcome::Rejected
            },
            execute_mode: None,
            selection: None,
            vision_supported: None,
            feedback: None,
        };
        let _ = sender.send(decision);
        Ok(true)
    }

    async fn answer_ask(
        &self,
        session_id: &str,
        request_id: &str,
        question_id: &str,
        option: &str,
    ) -> Result<bool, String> {
        use denia_core::session::{AskAnswer, AskOutcome, AskResolution};
        let Some(live) = self.0.live.get(session_id) else {
            return Ok(false);
        };
        let sender = {
            let mut pending = live
                .pending_asks
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            pending.remove(request_id)
        };
        let Some(sender) = sender else {
            return Ok(false);
        };
        let _ = sender.send(AskResolution {
            outcome: AskOutcome::Answered,
            answers: vec![AskAnswer {
                id: question_id.to_string(),
                selected: vec![option.to_string()],
                custom: None,
                skipped: false,
            }],
            reason: None,
        });
        Ok(true)
    }

    async fn default_model(&self) -> Result<ModelSelection, String> {
        let catalog = denia_llm::catalog::build_model_catalog(&self.0.registry).await;
        for group in &catalog.groups {
            if let Some(model) = group.models.first() {
                return Ok(ModelSelection {
                    provider: group.id.clone(),
                    model: model.id.clone(),
                    reasoning_effort: None,
                });
            }
        }
        Err("模型目录为空".to_string())
    }

    async fn run_turn(
        &self,
        session_id: &str,
        text: &str,
        images: &[denia_channels::bridge::ImageAttachment],
        model: &ModelSelection,
        notices: &NoticeSink,
    ) -> Result<TurnReply, String> {
        let live = self
            .0
            .live
            .get_or_load(&self.0.sessions, session_id)
            .map_err(|error| error.to_string())?;
        self.0
            .live
            .ensure_hot(&live)
            .map_err(|error| error.to_string())?;
        let start = live.session.last_seq();
        let mut wake = live.followers.subscribe();
        let prompt_images: Vec<PromptImage> = images
            .iter()
            .enumerate()
            .map(|(index, image)| {
                let extension = match image.mime.as_str() {
                    "image/png" => "png",
                    "image/gif" => "gif",
                    "image/webp" => "webp",
                    _ => "jpg",
                };
                PromptImage::new(
                    Some(format!("weixin-{}.{}", index + 1, extension)),
                    image.mime.clone(),
                    base64::engine::general_purpose::STANDARD.encode(&image.data),
                )
            })
            .collect();
        let command = crate::application::sessions::PromptCommand::text_with_images(
            text.to_string(),
            model.provider.clone(),
            model.model.clone(),
            model.reasoning_effort.clone(),
            prompt_images,
        );
        self.0
            .session_commands()
            .submit_prompt(session_id.to_string(), command)
            .await
            .map_err(|error| error.message)?;

        let mut cursor = start;
        let mut reply_text = String::new();
        let deadline = Instant::now() + Duration::from_secs(60 * 30);
        loop {
            for envelope in live.session.events_after(cursor) {
                cursor = envelope.seq;
                match envelope.event {
                    SessionEvent::AssistantMessage { blocks, .. } => {
                        let text = blocks
                            .iter()
                            .filter_map(|block| match block {
                                ContentBlock::Text { text } => Some(text.as_str()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("");
                        if !text.trim().is_empty() {
                            reply_text = text;
                        }
                    }
                    SessionEvent::ApprovalAsked {
                        request_id,
                        tool,
                        args_preview,
                        ..
                    } => {
                        notices.send(Notice::Approval {
                            session_id: session_id.to_string(),
                            request_id,
                            tool,
                            args_preview,
                        });
                    }
                    SessionEvent::AskRequested {
                        request_id,
                        questions,
                        timeout_ms,
                        ..
                    } => {
                        notices.send(Notice::Ask {
                            session_id: session_id.to_string(),
                            request_id,
                            questions,
                            timeout_ms,
                        });
                    }
                    SessionEvent::TurnEnd { reason, .. } => {
                        let outcome = match reason {
                            TurnEndReason::Completed => TurnOutcome::Completed,
                            TurnEndReason::Aborted { .. } | TurnEndReason::Interrupted => {
                                TurnOutcome::Aborted
                            }
                            TurnEndReason::MaxTokens => TurnOutcome::MaxTokens,
                            TurnEndReason::Error { failure } => {
                                TurnOutcome::Failed(failure.message)
                            }
                            TurnEndReason::LoopDetected { .. } => TurnOutcome::LoopDetected,
                        };
                        return Ok(TurnReply {
                            text: reply_text,
                            outcome,
                        });
                    }
                    _ => {}
                }
            }
            if Instant::now() > deadline {
                return Err("轮次超过 30 分钟仍未结束；可用 /cancel 中断".to_string());
            }
            let _ = tokio::time::timeout(Duration::from_millis(500), wake.recv()).await;
        }
    }
}
