//! Rust 原生子代理编排；复用 SessionDriver，只有一份会话日志与运行状态。
use crate::{
    jobs::Jobs,
    state::{LiveSessions, RunningGuard, ServerEvent},
};
use async_trait::async_trait;
use denia_core::{
    config::ModelSelection,
    session::{SessionEnvelope, SessionEvent, SubagentDescriptor},
};
use denia_tools::{ToolContext, capabilities::AgentRuntime};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak, atomic::Ordering},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeConfig {
    pub max_agents: usize,
    pub max_depth: usize,
    pub max_jobs: usize,
    pub retained_jobs: usize,
    pub output_bytes: usize,
    pub max_wait_ms: u64,
    pub job_timeout_ms: u64,
    pub max_pending_messages: usize,
    pub max_consecutive_wakes: usize,
    pub custom_skill_dirs: Vec<PathBuf>,
}
impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            max_agents: 8,
            max_depth: 4,
            max_jobs: 8,
            retained_jobs: 64,
            output_bytes: 64 * 1024,
            max_wait_ms: 600_000,
            job_timeout_ms: 600_000,
            max_pending_messages: 64,
            max_consecutive_wakes: 3,
            custom_skill_dirs: Vec::new(),
        }
    }
}
pub fn validate_config(value: Value) -> Result<Value, String> {
    let c: RuntimeConfig =
        serde_json::from_value(value).map_err(|e| format!("运行时配置无效：{e}"))?;
    if !(1..=64).contains(&c.max_agents)
        || !(1..=16).contains(&c.max_depth)
        || !(1..=64).contains(&c.max_jobs)
        || c.retained_jobs < c.max_jobs
        || c.retained_jobs > 1024
        || !(1024..=1048576).contains(&c.output_bytes)
        || !(1..=600_000).contains(&c.max_wait_ms)
        || !(1..=86_400_000).contains(&c.job_timeout_ms)
        || !(1..=1024).contains(&c.max_pending_messages)
        || !(1..=16).contains(&c.max_consecutive_wakes)
        || c.custom_skill_dirs.iter().any(|p| !p.is_absolute())
    {
        return Err("运行时配置超出允许范围；技能自定义目录必须为绝对路径".into());
    }
    serde_json::to_value(c).map_err(|e| e.to_string())
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Child {
    id: String,
    parent_id: String,
    descriptor: SubagentDescriptor,
}
#[derive(Default)]
struct Inbox {
    seq: u64,
    pending: BTreeMap<u64, (String, String, String)>,
    seen: HashSet<String>,
}
struct Inner {
    home: PathBuf,
    sessions: Arc<denia_session::SessionStore>,
    live: Arc<LiveSessions>,
    settings: Arc<denia_settings::SettingsStore>,
    events: tokio::sync::broadcast::Sender<ServerEvent>,
    driver: OnceLock<Weak<denia_agent_loop::SessionDriver>>,
    children: Mutex<BTreeMap<String, Child>>,
    inboxes: Mutex<HashMap<String, Inbox>>,
    selections: Mutex<HashMap<String, ModelSelection>>,
    wakes: Mutex<HashMap<String, usize>>,
    paused: Mutex<HashSet<String>>,
    // 已加载技能正文(session → 技能名 → (来源, SKILL.md 正文))；
    // 正文经上下文注入通道进入模型视野，不随 skill load 工具结果返回。
    active_skills: Mutex<HashMap<String, BTreeMap<String, (String, String)>>>,
    admission: tokio::sync::Mutex<()>,
    reserved: Mutex<HashSet<String>>,
    registry: Arc<denia_llm::LlmRegistry>,
    workspaces: Arc<crate::workspace::WorkspaceRegistry>,
    pub jobs: Arc<Jobs>,
}
#[derive(Clone)]
pub struct Runtime {
    inner: Arc<Inner>,
}
impl Runtime {
    pub fn new(
        home: &Path,
        sessions: Arc<denia_session::SessionStore>,
        live: Arc<LiveSessions>,
        settings: Arc<denia_settings::SettingsStore>,
        events: tokio::sync::broadcast::Sender<ServerEvent>,
        registry: Arc<denia_llm::LlmRegistry>,
        workspaces: Arc<crate::workspace::WorkspaceRegistry>,
    ) -> Result<Arc<Self>, String> {
        let mut children = BTreeMap::new();
        // 只读会话头，不加载/修复另一个实例正在写入的日志。
        use std::io::BufRead;
        for entry in sessions.list().map_err(|e| e.to_string())? {
            if entry.parent_session.is_none() {
                continue;
            }
            let file = std::fs::File::open(sessions.root().join(&entry.id).join("session.jsonl"))
                .map_err(|e| e.to_string())?;
            let line = std::io::BufReader::new(file)
                .lines()
                .next()
                .transpose()
                .map_err(|e| e.to_string())?
                .unwrap_or_default();
            let header: denia_core::session::SessionHeader =
                serde_json::from_str(&line).map_err(|e| e.to_string())?;
            if let (Some(descriptor), Some(parent_id)) = (header.subagent, header.parent_session) {
                children.insert(
                    entry.id.clone(),
                    Child {
                        id: entry.id,
                        parent_id,
                        descriptor,
                    },
                );
            }
        }
        Ok(Arc::new(Self {
            inner: Arc::new(Inner {
                home: home.into(),
                sessions,
                live,
                settings,
                events,
                driver: OnceLock::new(),
                children: Mutex::new(children),
                inboxes: Mutex::new(HashMap::new()),
                selections: Mutex::new(HashMap::new()),
                wakes: Mutex::new(HashMap::new()),
                paused: Mutex::new(HashSet::new()),
                active_skills: Mutex::new(HashMap::new()),
                admission: tokio::sync::Mutex::new(()),
                reserved: Mutex::new(HashSet::new()),
                registry,
                workspaces,
                jobs: Jobs::new(),
            }),
        }))
    }
    pub fn attach(&self, driver: &Arc<denia_agent_loop::SessionDriver>) {
        let _ = self.inner.driver.set(Arc::downgrade(driver));
        let runtime = self.clone();
        let mut done = self.inner.jobs.done.subscribe();
        tokio::spawn(async move {
            loop {
                match done.recv().await {
                    Ok(job) => {
                        if let Some(result) = runtime.inner.jobs.claim_notice(&job.id, &job.owner) {
                            if let Err(e) = runtime
                                .enqueue(
                                    &job.owner,
                                    format!("job:{}", job.id),
                                    format!("[后台任务完成]\n{result}"),
                                    "job-completed".into(),
                                )
                                .await
                            {
                                runtime.inner.jobs.release_notice(&job.id, &job.owner);
                                tracing::error!(error=%e,"任务通知投递失败");
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        tracing::error!("后台任务通知队列溢出，请用 job_list 领取结果");
                    }
                    Err(_) => break,
                }
            }
        });
    }
    pub fn jobs(&self) -> Arc<Jobs> {
        self.inner.jobs.clone()
    }
    pub fn config(&self) -> RuntimeConfig {
        self.inner
            .settings
            .resolved("runtime")
            .ok()
            .and_then(|s| serde_json::from_value(s).ok())
            .unwrap_or_default()
    }
    pub fn human_turn(&self, id: &str, selection: &ModelSelection) {
        self.inner.paused.lock().unwrap().remove(id);
        self.inner
            .selections
            .lock()
            .unwrap()
            .insert(id.into(), selection.clone());
        self.inner.wakes.lock().unwrap().insert(id.into(), 0);
    }
    fn is_active(&self, id: &str) -> bool {
        self.inner
            .live
            .get(id)
            .is_some_and(|l| l.running.load(Ordering::SeqCst))
            || (!self.inner.paused.lock().unwrap().contains(id)
                && self
                    .inner
                    .inboxes
                    .lock()
                    .unwrap()
                    .get(id)
                    .is_some_and(|i| !i.pending.is_empty()))
    }
    fn descendants(&self, root: &str) -> Vec<Child> {
        let children = self.inner.children.lock().unwrap();
        let mut ids = HashSet::from([root.to_string()]);
        let mut result = Vec::new();
        loop {
            let next: Vec<_> = children
                .values()
                .filter(|c| ids.contains(&c.parent_id) && !ids.contains(&c.id))
                .cloned()
                .collect();
            if next.is_empty() {
                break;
            }
            for child in next {
                ids.insert(child.id.clone());
                result.push(child);
            }
        }
        result
    }
    pub fn list(&self, owner: &str) -> Value {
        json!(self.descendants(owner).into_iter().map(|child|{
        let running=self.inner.live.get(&child.id).is_some_and(|l|l.running.load(Ordering::SeqCst));
        let waiting=self.descendants(&child.id).iter().any(|c|self.inner.live.get(&c.id).is_some_and(|l|l.running.load(Ordering::SeqCst)));
        json!({"id":child.id,"parentId":child.parent_id,"label":child.descriptor.label,"depth":child.descriptor.depth,"mode":child.descriptor.mode,"selection":child.descriptor.selection,"status":if running{"running"}else if waiting{"waiting"}else{"settled"}})
    }).collect::<Vec<_>>())
    }
    fn authorize(&self, owner: &str, target: &str, ancestor: bool) -> Result<(), String> {
        if owner == target {
            return Err("不能向自身执行代理控制操作".into());
        }
        let children = self.inner.children.lock().unwrap();
        if children.get(target).is_some_and(|c| c.parent_id == owner)
            || (!ancestor && children.get(owner).is_some_and(|c| c.parent_id == target))
        {
            return Ok(());
        }
        drop(children);
        if ancestor && self.descendants(owner).iter().any(|c| c.id == target) {
            return Ok(());
        }
        Err("目标不在当前代理允许控制的谱系范围内".into())
    }
    async fn live(&self, id: &str) -> Result<Arc<crate::state::LiveSession>, String> {
        let inner = self.inner.clone();
        let id = id.to_string();
        tokio::task::spawn_blocking(move || {
            inner
                .live
                .get_or_load(&inner.sessions, &id)
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    fn sync_inbox(inbox: &mut Inbox, session: &denia_session::Session) {
        for event in session.events_after(inbox.seq) {
            inbox.seq = event.seq;
            match event.event {
                SessionEvent::AgentInbox { id, text, source } => {
                    inbox.seen.insert(id.clone());
                    inbox.pending.insert(event.seq, (id, text, source));
                }
                SessionEvent::AgentDelivery { id, .. } => {
                    inbox.seen.insert(id.clone());
                    inbox.pending.retain(|_, (key, _, _)| *key != id);
                }
                _ => {}
            }
        }
    }
    pub async fn enqueue(
        &self,
        target: &str,
        id: String,
        text: String,
        source: String,
    ) -> Result<String, String> {
        if text.len() > self.config().output_bytes * 2 {
            return Err("代理消息超过配置的体积上限".into());
        }
        if source.starts_with("agent:") {
            self.inner.paused.lock().unwrap().remove(target);
            self.inner.wakes.lock().unwrap().insert(target.into(), 0);
        }
        let live = self.live(target).await?;
        let inner = self.inner.clone();
        let target = target.to_string();
        let result_id = id.clone();
        let wake_target = target.clone();
        let limit = self.config().max_pending_messages;
        tokio::task::spawn_blocking(move || {
            let mut inboxes = inner.inboxes.lock().unwrap();
            let inbox = inboxes.entry(target).or_default();
            Self::sync_inbox(inbox, &live.session);
            if inbox.seen.contains(&id) {
                return Ok(());
            }
            if inbox.pending.len() >= limit {
                return Err("目标代理待收消息已达上限".to_string());
            }
            let event = live
                .session
                .append(SessionEvent::AgentInbox { id, text, source })
                .map_err(|e| e.to_string())?;
            live.session.flush().map_err(|e| e.to_string())?;
            let _ = live.followers.send(event);
            Self::sync_inbox(inbox, &live.session);
            Ok(())
        })
        .await
        .map_err(|e| e.to_string())??;
        self.wake(&wake_target);
        Ok(result_id)
    }
    fn wake(&self, id: &str) {
        let runtime = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            if let Err(error) = runtime.resume_pending(&id).await {
                tracing::error!(session=%id,%error,"恢复代理消息失败");
            }
        });
    }
    async fn resume_pending(&self, id: &str) -> Result<(), String> {
        if self.inner.paused.lock().unwrap().contains(id) {
            return Ok(());
        }
        let live = self.live(id).await?;
        let pending = {
            let mut inboxes = self.inner.inboxes.lock().unwrap();
            let inbox = inboxes.entry(id.into()).or_default();
            Self::sync_inbox(inbox, &live.session);
            !inbox.pending.is_empty()
        };
        if !pending {
            return Ok(());
        }
        let selection = self
            .inner
            .selections
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .or_else(|| {
                live.session
                    .header()
                    .subagent
                    .as_ref()
                    .map(|s| s.selection.clone())
            })
            .or_else(|| {
                live.session.events().iter().rev().find_map(|e| {
                    if let SessionEvent::RequestHeader {
                        header: snapshot, ..
                    } = &e.event
                    {
                        Some(ModelSelection {
                            provider: snapshot.config.provider.clone(),
                            model: snapshot.config.model.clone(),
                            reasoning_effort: snapshot.config.reasoning_effort.clone(),
                        })
                    } else {
                        None
                    }
                })
            });
        let admission = self.inner.admission.lock().await;
        if self.inner.paused.lock().unwrap().contains(id) {
            return Ok(());
        }
        let Some(selection) = selection else {
            return Ok(());
        };
        {
            let mut wakes = self.inner.wakes.lock().unwrap();
            let n = wakes.entry(id.into()).or_default();
            if *n >= self.config().max_consecutive_wakes {
                return Ok(());
            }
            if live
                .running
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Ok(());
            }
            if live.session.header().subagent.is_some() {
                let mut slots = self.inner.reserved.lock().unwrap();
                if !slots.contains(id) && slots.len() >= self.config().max_agents {
                    live.running.store(false, Ordering::SeqCst);
                    return Ok(());
                }
                slots.insert(id.to_string());
            }
            *n += 1;
        }
        let guard = RunningGuard::new(live.clone(), self.inner.events.clone());
        let token = CancellationToken::new();
        *live.cancel.lock().unwrap() = Some(token.clone());
        drop(admission);
        let _ = self.inner.events.send(ServerEvent::RunningChanged {
            id: id.into(),
            running: true,
        });
        let driver = self
            .inner
            .driver
            .get()
            .and_then(Weak::upgrade)
            .ok_or("会话驱动器尚未就绪")?;
        let followers = live.followers.clone();
        let emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync> = Arc::new(move |e| {
            let _ = followers.send(e.clone());
        });
        driver
            .run_turn(
                &live.session,
                &selection,
                "",
                Vec::new(),
                Vec::new(),
                Vec::new(),
                self.inner
                    .registry
                    .resolve_call(
                        &selection.provider,
                        &selection.model,
                        selection.reasoning_effort.as_deref(),
                    )
                    .await
                    .map(|r| r.info.input_modalities.iter().any(|m| m == "image"))
                    .unwrap_or(false),
                token,
                emit,
            )
            .await;
        drop(guard);
        self.on_idle(id);
        Ok(())
    }
    pub fn on_idle(&self, id: &str) {
        self.inner.reserved.lock().unwrap().remove(id);
        let aborted = self.inner.live.get(id).is_some_and(|l| {
            l.session
                .events()
                .iter()
                .rev()
                .find_map(|e| {
                    if let SessionEvent::TurnEnd { reason, .. } = &e.event {
                        Some(matches!(
                            reason,
                            denia_core::session::TurnEndReason::Aborted { .. }
                        ))
                    } else {
                        None
                    }
                })
                .unwrap_or(false)
        });
        if aborted {
            self.inner.paused.lock().unwrap().insert(id.into());
        } else {
            self.wake(id);
        }
        let queued: Vec<_> = self
            .inner
            .inboxes
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, i)| !i.pending.is_empty())
            .map(|(id, _)| id.clone())
            .collect();
        for queued_id in queued {
            if queued_id != id {
                self.wake(&queued_id);
            }
        }
        let runtime = self.clone();
        let id = id.to_string();
        tokio::spawn(async move {
            let current = id;
            loop {
                let child = runtime
                    .inner
                    .children
                    .lock()
                    .unwrap()
                    .get(&current)
                    .cloned();
                let Some(child) = child else {
                    break;
                };
                if runtime
                    .descendants(&current)
                    .iter()
                    .any(|c| runtime.is_active(&c.id))
                {
                    break;
                }
                if runtime.is_active(&current) {
                    break;
                }
                if let Ok(live) = runtime.live(&current).await {
                    let events = live.session.events();
                    let last = events.iter().rev().find_map(|e| {
                        if let SessionEvent::AssistantMessage { blocks, .. } = &e.event {
                            Some(blocks.clone())
                        } else {
                            None
                        }
                    });
                    let text = last
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|b| {
                            if let denia_core::stream::ContentBlock::Text { text } = b {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let reason = events.iter().rev().find_map(|e| {
                        if let SessionEvent::TurnEnd { reason, .. } = &e.event {
                            Some(reason)
                        } else {
                            None
                        }
                    });
                    let summary = format!(
                        "[子代理执行结束] {} ({})\n状态：{}\n{}",
                        child.descriptor.label,
                        current,
                        serde_json::to_string(&reason).unwrap_or_default(),
                        text.chars()
                            .take(runtime.config().output_bytes / 4)
                            .collect::<String>()
                    );
                    if let Err(error) = runtime
                        .enqueue(
                            &child.parent_id,
                            format!("settled:{}:{}", current, live.session.next_turn_number()),
                            summary,
                            "subagent-settled".into(),
                        )
                        .await
                    {
                        tracing::error!(%error,"子代理结果通知失败");
                    }
                }
                break;
            }
        });
    }
    pub async fn interrupt(&self, owner: &str, target: &str) -> Result<(), String> {
        let _admission = self.inner.admission.lock().await;
        self.authorize(owner, target, true)?;
        self.inner.paused.lock().unwrap().insert(target.into());
        if self
            .inner
            .live
            .get(target)
            .is_none_or(|l| !l.running.load(Ordering::SeqCst))
        {
            self.inner.reserved.lock().unwrap().remove(target);
        }
        if let Some(live) = self.inner.live.get(target) {
            if let Some(token) = live.cancel.lock().unwrap().as_ref() {
                token.cancel();
            }
        }
        Ok(())
    }
    pub fn ensure_removable(&self, owner: &str) -> Result<(), String> {
        if self
            .descendants(owner)
            .iter()
            .any(|c| self.is_active(&c.id))
        {
            return Err("仍有运行中或待处理的子代理，请先停止后再删除会话".into());
        }
        Ok(())
    }
    pub async fn remove_owner(&self, owner: &str) -> Result<(), String> {
        self.ensure_removable(owner)?;
        let children = self.descendants(owner);
        if children.iter().any(|c| {
            self.inner
                .live
                .get(&c.id)
                .is_some_and(|l| l.running.load(Ordering::SeqCst))
        }) {
            return Err("仍有运行中的子代理，请先停止后再删除会话".into());
        }
        for child in children.iter().rev() {
            self.inner.jobs.cancel_owner(&child.id).await;
            let sessions = self.inner.sessions.clone();
            let id = child.id.clone();
            tokio::task::spawn_blocking(move || sessions.delete(&id))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
            self.inner.live.remove(&child.id);
            self.inner.workspaces.detach_session(&child.id);
            self.inner.children.lock().unwrap().remove(&child.id);
        }
        self.inner.jobs.cancel_owner(owner).await;
        self.inner.children.lock().unwrap().remove(owner);
        Ok(())
    }
    async fn rollback_child(&self, id: &str) {
        self.inner.reserved.lock().unwrap().remove(id);
        self.inner.children.lock().unwrap().remove(id);
        self.inner.selections.lock().unwrap().remove(id);
        self.inner.inboxes.lock().unwrap().remove(id);
        self.inner.live.remove(id);
        self.inner.workspaces.detach_session(id);
        let sessions = self.inner.sessions.clone();
        let id = id.to_string();
        if let Err(e) = tokio::task::spawn_blocking(move || sessions.delete(&id)).await {
            tracing::error!(error=%e,"回滚子代理失败");
        }
    }
    async fn delegate(&self, name: &str, args: Value, ctx: &ToolContext) -> Result<Value, String> {
        let owner = ctx.session_id.as_deref().ok_or("缺少会话身份")?;
        let config = self.config();

        let prompt = string(&args, "prompt")?;
        let parent = self.live(owner).await?;
        let mut selection = ctx.selection.clone().ok_or("子代理无法继承模型选择")?;
        if let Some(v) = args["provider"].as_str() {
            selection.provider = v.into();
        }
        if let Some(v) = args["model"].as_str() {
            selection.model = v.into();
        }
        if let Some(v) = args["reasoning_effort"].as_str() {
            selection.reasoning_effort = Some(v.into());
        }
        self.inner
            .registry
            .resolve_call(
                &selection.provider,
                &selection.model,
                selection.reasoning_effort.as_deref(),
            )
            .await
            .map_err(|e| e.to_string())?;
        if prompt.len() > config.output_bytes {
            return Err("子代理提示词过大".into());
        }
        let depth = parent
            .session
            .header()
            .subagent
            .as_ref()
            .map_or(1, |s| s.depth + 1);
        let depth_cap = args
            .get("max_depth")
            .map(|v| {
                v.as_u64()
                    .filter(|n| *n > 0)
                    .ok_or("max_depth 必须为正整数")
            })
            .transpose()?
            .map_or(config.max_depth, |v| (v as usize).min(config.max_depth));
        if depth > depth_cap {
            return Err("子代理委派深度已达上限".into());
        }
        let _admission = self.inner.admission.lock().await;
        if self.inner.reserved.lock().unwrap().len() >= config.max_agents {
            return Err("子代理并发已达上限".into());
        }
        if ctx.cancel.is_cancelled() {
            return Err("子代理启动已取消".into());
        }
        let descriptor = SubagentDescriptor {
            label: args["description"]
                .as_str()
                .unwrap_or("子代理")
                .chars()
                .take(200)
                .collect(),
            depth,
            mode: if name == "fork_agent" {
                "fork"
            } else {
                "spawn"
            }
            .into(),
            selection: selection.clone(),
            persona: args
                .get("persona")
                .map(|v| v.as_str().map(str::to_string).ok_or("persona 必须为文本"))
                .transpose()?,
            allowed_tools: args
                .get("allowed_tools")
                .map(|v| {
                    serde_json::from_value::<Vec<String>>(v.clone())
                        .map_err(|_| "allowed_tools 必须为工具名称数组")
                })
                .transpose()?
                .or_else(|| {
                    parent
                        .session
                        .header()
                        .subagent
                        .as_ref()
                        .and_then(|s| s.allowed_tools.clone())
                }),
        };
        if descriptor
            .persona
            .as_ref()
            .is_some_and(|p| p.len() > config.output_bytes)
        {
            return Err("子代理角色提示词过大".into());
        }
        if let Some(allowed) = &descriptor.allowed_tools {
            let mut known = denia_tools::default_registry().schemas();
            known.extend(denia_tools::capabilities::schemas());
            let names: HashSet<_> = known
                .iter()
                .map(|s| s.name.as_str())
                .chain(["browser", "recon"])
                .collect();
            if allowed.iter().any(|name| !names.contains(name.as_str())) {
                return Err("子代理工具过滤包含未知工具名称".into());
            }
            if let Some(parent_allowed) = parent
                .session
                .header()
                .subagent
                .as_ref()
                .and_then(|s| s.allowed_tools.as_ref())
            {
                if allowed.iter().any(|name| !parent_allowed.contains(name)) {
                    return Err("子代理不能扩大父代理的工具集合".into());
                }
            }
        }
        let sessions = self.inner.sessions.clone();
        let cwd = ctx.cwd.clone();
        let sandbox = parent.session.header().sandbox;
        let parent_id = owner.to_string();
        let desc = descriptor.clone();
        let permission = parent.session.permission_mode();
        // fork 只取闭合轮次，不复制正在产生的 tool-call 半截历史。
        let source = if name == "fork_agent" {
            let e = parent.session.events();
            let cut = e
                .iter()
                .rposition(|e| matches!(e.event, SessionEvent::TurnEnd { .. }))
                .map_or(0, |i| i + 1);
            e[..cut]
                .iter()
                .filter(|e| {
                    !matches!(
                        e.event,
                        SessionEvent::AgentInbox { .. } | SessionEvent::AgentDelivery { .. }
                    )
                })
                .cloned()
                .collect()
        } else {
            Vec::new()
        };
        let id = tokio::task::spawn_blocking(move || {
            let child = sessions
                .create_subagent(&cwd, sandbox, &parent_id, desc)
                .map_err(|e| e.to_string())?;
            let result: Result<String, String> = (|| {
                child.seed_from(&source).map_err(|e| e.to_string())?;
                child
                    .set_permission_mode(permission)
                    .map_err(|e| e.to_string())?;
                child.flush().map_err(|e| e.to_string())?;
                Ok(child.id().to_string())
            })();
            if result.is_err() {
                let _ = sessions.delete(child.id());
            }
            result
        })
        .await
        .map_err(|e| e.to_string())??;
        for workspace in self.inner.workspaces.list() {
            if workspace.session_ids.iter().any(|s| s == owner)
                && !self.inner.workspaces.attach(&workspace.id, &id)
            {
                self.rollback_child(&id).await;
                return Err("父工作区已删除，子代理创建已回滚".into());
            }
        }
        self.inner.reserved.lock().unwrap().insert(id.clone());
        self.inner.children.lock().unwrap().insert(
            id.clone(),
            Child {
                id: id.clone(),
                parent_id: owner.into(),
                descriptor,
            },
        );
        self.inner
            .selections
            .lock()
            .unwrap()
            .insert(id.clone(), selection);
        if ctx.cancel.is_cancelled() {
            self.rollback_child(&id).await;
            return Err("子代理启动已取消".into());
        }
        let message_id = self
            .enqueue(
                &id,
                uuid::Uuid::new_v4().to_string(),
                format!("[父代理 {owner} 委派任务]\n{prompt}"),
                format!("agent:{owner}"),
            )
            .await;
        let message_id = match message_id {
            Ok(id) => id,
            Err(error) => {
                self.rollback_child(&id).await;
                return Err(error);
            }
        };
        let _ = self.inner.events.send(ServerEvent::SessionsUpdated);
        drop(_admission);
        if args["run_in_background"].as_bool() == Some(false) {
            // 初始准入后后台执行由管理器持有；调用方只取消自己的等待。

            loop {
                let live = self.live(&id).await?;
                if !self.is_active(&id) {
                    return Ok(
                        json!({"childId":id,"messageId":message_id,"messages":live.session.derive_messages().into_iter().rev().take(1).collect::<Vec<_>>()}),
                    );
                }
                tokio::select! {_=ctx.cancel.cancelled()=>return Err(format!("等待中断，子代理 {id} 继续运行")),_=tokio::time::sleep(std::time::Duration::from_millis(100))=>{}}
            }
        }
        Ok(json!({"childId":id,"messageId":message_id}))
    }
    pub async fn skills(&self, cwd: PathBuf) -> Result<Vec<crate::skills::Skill>, String> {
        let home = self.inner.home.clone();
        let custom = self.config().custom_skill_dirs;
        tokio::task::spawn_blocking(move || crate::skills::discover(&home, &cwd, &custom))
            .await
            .map_err(|e| e.to_string())?
    }
    pub async fn load_skill(
        &self,
        cwd: PathBuf,
        name: String,
        user: bool,
    ) -> Result<Value, String> {
        let skills = self.skills(cwd).await?;
        tokio::task::spawn_blocking(move || crate::skills::load(&skills, &name, user))
            .await
            .map_err(|e| e.to_string())?
    }
}

fn string(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("缺少非空参数：{key}"))
}
fn wait_ms(args: &Value, config: &RuntimeConfig) -> Result<u64, String> {
    match args.get("timeout_ms") {
        None => Ok(30_000.min(config.max_wait_ms)),
        Some(v) => v
            .as_u64()
            .filter(|v| *v > 0)
            .map(|v| v.min(config.max_wait_ms))
            .ok_or_else(|| "timeout_ms 必须为正整数".into()),
    }
}

#[async_trait]
impl AgentRuntime for Runtime {
    async fn execute(&self, name: &str, args: Value, ctx: &ToolContext) -> Result<Value, String> {
        let owner = ctx.session_id.as_deref().ok_or("该能力需要会话身份")?;
        let config = self.config();
        match name {
            "skill" => match string(&args, "action")?.as_str() {
                "list" => Ok(json!(
                    self.skills(ctx.cwd.clone())
                        .await?
                        .into_iter()
                        .filter(|s| s.model_invocable)
                        .collect::<Vec<_>>()
                )),
                "resource" => {
                    let skills = self.skills(ctx.cwd.clone()).await?;
                    let name = string(&args, "name")?;
                    let path = string(&args, "path")?;
                    tokio::task::spawn_blocking(move || {
                        crate::skills::resource(&skills, &name, &path)
                    })
                    .await
                    .map_err(|e| e.to_string())?
                }
                "load" => {
                    let name = string(&args, "name")?;
                    let value = self
                        .load_skill(ctx.cwd.clone(), name.clone(), false)
                        .await?;
                    // 注入式加载：SKILL.md 正文进入上下文注入通道，随后续请求
                    // 自动注入；工具结果只回元信息，避免模型再用文件工具读一遍。
                    let source = value["skill"]["source"].as_str().unwrap_or("").to_string();
                    let body = value["body"]
                        .as_str()
                        .ok_or_else(|| format!("技能 {name} 正文缺失"))?
                        .to_string();
                    self.inner
                        .active_skills
                        .lock()
                        .unwrap()
                        .entry(owner.to_string())
                        .or_default()
                        .insert(name, (source, body));
                    let mut result = value;
                    result["body"] = json!(null);
                    result["injected"] = json!(true);
                    Ok(result)
                }
                _ => Err("未知技能操作".into()),
            },
            "job_start" => {
                let command = string(&args, "command")?;
                let timeout = args
                    .get("timeout_ms")
                    .map(|v| {
                        v.as_u64()
                            .filter(|n| *n > 0)
                            .ok_or("timeout_ms 必须为正整数")
                    })
                    .transpose()?
                    .unwrap_or(config.job_timeout_ms)
                    .min(config.job_timeout_ms);
                Ok(json!(self.inner.jobs.start(
                    ctx,
                    &command,
                    args["label"].as_str().unwrap_or(&command),
                    timeout,
                    config.max_jobs,
                    config.retained_jobs,
                    config.output_bytes
                )?))
            }
            "job_list" => Ok(json!(self.inner.jobs.list(owner))),
            "job_output" => {
                let id = string(&args, "id")?;
                // 等待者拥有本次结果领取；完成监听器不会同时再注入一份通知。
                let _lease = self.inner.jobs.wait_lease(&id, owner)?;
                if args["wait"].as_bool().unwrap_or(false) {
                    self.inner
                        .jobs
                        .wait(&id, owner, wait_ms(&args, &config)?, &ctx.cancel)
                        .await?;
                }
                self.inner.jobs.read(&id, owner)
            }
            "job_kill" => {
                self.inner.jobs.kill(&string(&args, "id")?, owner)?;
                Ok(json!({"ok":true}))
            }
            "list_agents" => Ok(self.list(owner)),
            "interrupt_agent" => {
                self.interrupt(owner, &string(&args, "target")?).await?;
                Ok(json!({"ok":true}))
            }
            "send_message" => {
                let target = string(&args, "target")?;
                self.authorize(owner, &target, false)?;
                let id = self
                    .enqueue(
                        &target,
                        uuid::Uuid::new_v4().to_string(),
                        format!("[代理 {owner} 发来消息]\n{}", string(&args, "message")?),
                        format!("agent:{owner}"),
                    )
                    .await?;
                Ok(json!({"messageId":id,"target":target}))
            }
            "wait_agent" => {
                let target = string(&args, "target")?;
                self.authorize(owner, &target, true)?;
                let duration = std::time::Duration::from_millis(wait_ms(&args, &config)?);
                tokio::select! {_=ctx.cancel.cancelled()=>return Err("等待已中断，子代理继续运行".into()),_=tokio::time::timeout(duration,async{loop {if !self.is_active(&target){break;}tokio::time::sleep(std::time::Duration::from_millis(100)).await;}})=>{}}
                let live = self.live(&target).await?;
                Ok(
                    json!({"id":target,"running":live.running.load(Ordering::SeqCst),"messages":live.session.derive_messages().into_iter().rev().take(1).collect::<Vec<_>>()}),
                )
            }
            "spawn_agent" | "fork_agent" => {
                let runtime = self.clone();
                let name = name.to_string();
                let ctx = ctx.clone();
                tokio::spawn(async move { runtime.delegate(&name, args, &ctx).await })
                    .await
                    .map_err(|e| format!("子代理启动任务失败：{e}"))?
            }
            _ => Err(format!("未知宿主能力：{name}")),
        }
    }
    async fn context(&self, session: &str, cwd: &Path) -> Result<Vec<String>, String> {
        let skills = self.skills(cwd.into()).await?;
        let catalog = skills
            .into_iter()
            .filter(|s| s.model_invocable)
            .map(|s| format!("- {}：{}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        let parent = self
            .inner
            .children
            .lock()
            .unwrap()
            .get(session)
            .map(|c| c.parent_id.clone());
        let active = self
            .inner
            .active_skills
            .lock()
            .unwrap()
            .get(session)
            .cloned()
            .unwrap_or_default();
        let mut parts = vec![format!(
            "[denia 能力上下文]\n始终使用简体中文回复，除非用户明确要求其他语言。\n当前代理：{session}；父代理：{}。独立任务可用 spawn_agent/fork_agent 委派；send_message 仅允许直接父子通信。后台任务用 job_start 启动，job_output 领取，job_kill 停止。技能分全局技能（用户数据目录 skills/，跨项目复用）与项目技能（项目 .denia/skills 等目录，随项目走），同名项目技能优先；skill load 加载后 SKILL.md 正文会注入本上下文，无需再用文件读取工具读 SKILL.md；references/scripts 等其余文件用 skill resource 按 resourceBase 相对路径读取，技能不授予额外权限。\n可由模型调用的技能：\n{catalog}",
            parent.as_deref().unwrap_or("无")
        )];
        for (name, (source, body)) in active {
            parts.push(format!("[技能正文 {name}（来源：{source}）]\n{body}"));
        }
        Ok(parts)
    }
    async fn drain(&self, session: &str) -> Result<Vec<String>, String> {
        let live = self.live(session).await?;
        let inner = self.inner.clone();
        let id = session.to_string();
        tokio::task::spawn_blocking(move || {
            let mut inboxes = inner.inboxes.lock().unwrap();
            let inbox = inboxes.entry(id).or_default();
            Self::sync_inbox(inbox, &live.session);
            let pending: Vec<_> = inbox.pending.values().cloned().collect();
            for (id, text, source) in pending {
                let event = live
                    .session
                    .append(SessionEvent::AgentDelivery { id, text, source })
                    .map_err(|e| e.to_string())?;
                let _ = live.followers.send(event);
            }
            live.session.flush().map_err(|e| e.to_string())?;
            Self::sync_inbox(inbox, &live.session);
            Ok(Vec::new())
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use denia_core::{
        error::LlmError,
        stream::{ContentBlock, FinishReason, StreamChunk},
    };
    use denia_llm::{
        ChunkStream, GenerateRequest, LlmAdapter, LlmModelInfo, LlmResolvedModelInfo, ProviderInfo,
    };
    struct Mock;
    #[async_trait]
    impl LlmAdapter for Mock {
        fn provider_info(&self, p: &str) -> ProviderInfo {
            ProviderInfo {
                id: p.into(),
                name: "测试".into(),
            }
        }
        async fn list_models(&self, _: &str) -> Result<Vec<LlmModelInfo>, LlmError> {
            Ok(Vec::new())
        }
        async fn resolve_model(&self, p: &str, m: &str) -> Result<LlmResolvedModelInfo, LlmError> {
            Ok(LlmResolvedModelInfo {
                info: LlmModelInfo {
                    provider: p.into(),
                    id: m.into(),
                    name: m.into(),
                    description: None,
                    input_modalities: vec!["text".into()],
                },
                context_window: Some(100_000),
                default_max_tokens: Some(4000),
                reasoning: None,
            })
        }
        async fn stream(
            &self,
            _: &str,
            request: &GenerateRequest,
        ) -> Result<ChunkStream, LlmError> {
            assert!(request.tools.iter().any(|s| s.name == "skill"));
            if request.model == "hold" {
                return Ok(Box::pin(futures::stream::pending()));
            }
            if request.model == "orchestrator" {
                let results: Vec<_> = request
                    .messages
                    .iter()
                    .filter(|m| m.role == denia_core::message::ChatRole::Tool)
                    .collect();
                let call = match results.len() {
                    0 => Some((
                        "spawn_agent",
                        json!({"prompt":"执行子任务","model":"done","description":"接口验证子代理"}),
                    )),
                    1 => Some((
                        "bash",
                        json!({"command":"echo runtime-e2e","run_in_background":true}),
                    )),
                    2 => Some((
                        "job_output",
                        json!({"id":serde_json::from_str::<Value>(&results[1].content).unwrap()["id"],"wait":true}),
                    )),
                    3 => Some(("skill", json!({"action":"load","name":"runtime-test"}))),
                    _ => None,
                };
                if let Some((name, args)) = call {
                    return Ok(Box::pin(futures::stream::iter(vec![
                        Ok(StreamChunk::BlockEnd {
                            index: 0,
                            block: ContentBlock::ToolCall {
                                id: format!("call-{}", results.len()),
                                name: name.into(),
                                arguments: args.to_string(),
                            },
                        }),
                        Ok(StreamChunk::Finish {
                            reason: FinishReason::ToolCalls,
                        }),
                    ])));
                }
            }
            Ok(Box::pin(futures::stream::iter(vec![
                Ok(StreamChunk::BlockEnd {
                    index: 0,
                    block: ContentBlock::Text {
                        text: "子代理测试完成".into(),
                    },
                }),
                Ok(StreamChunk::Finish {
                    reason: FinishReason::Stop,
                }),
            ])))
        }
    }
    fn setup() -> (crate::state::AppState, ToolContext) {
        let home =
            std::env::temp_dir().join(format!("denia-runtime-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let state = crate::state::build_state(&home, false).unwrap();
        state
            .registry
            .register(
                &["runtime-test".into()],
                Arc::new(Mock),
                denia_llm::RetryPolicy::default(),
            )
            .unwrap();
        let session = state.sessions.create(&home, true).unwrap();
        let id = session.id().to_string();
        drop(session);
        let live = state.live.get_or_load(&state.sessions, &id).unwrap();
        live.running.store(true, Ordering::SeqCst); // 父轮次由测试控制，通知先排队。
        let ctx = ToolContext {
            session_id: Some(id),
            selection: Some(ModelSelection {
                provider: "runtime-test".into(),
                model: "done".into(),
                reasoning_effort: None,
            }),
            cwd: home,
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: false,
            emit_event: None,
            file_history: None,
            permission_mode: denia_core::session::PermissionMode::WorkspaceWrite,
            permission_override: None,
        };
        (state, ctx)
    }
    async fn settle(runtime: &Runtime, id: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while runtime.is_active(id) {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("子代理应在测试时限内结束");
    }
    #[tokio::test]
    async fn spawn_resume_lineage_and_exactly_once_delivery() {
        let (state, ctx) = setup();
        let owner = ctx.session_id.as_ref().unwrap();
        let result = state
            .runtime
            .execute(
                "spawn_agent",
                json!({"prompt":"检查实现","description":"测试子代理"}),
                &ctx,
            )
            .await
            .unwrap();
        let id = result["childId"].as_str().unwrap();
        settle(&state.runtime, id).await;
        let live = state.live.get(id).unwrap();
        assert_eq!(
            live.session.header().parent_session.as_deref(),
            Some(owner.as_str())
        );
        assert!(live.session.header().subagent.is_some());
        assert!(
            state
                .sessions
                .list()
                .unwrap()
                .iter()
                .find(|s| s.id == id)
                .unwrap()
                .excerpt
                .is_some()
        );
        assert!(
            state
                .runtime
                .execute(
                    "send_message",
                    json!({"target":"foreign","message":"不允许"}),
                    &ctx
                )
                .await
                .is_err()
        );
        let first = live.session.next_turn_number();
        state
            .runtime
            .execute(
                "send_message",
                json!({"target":id,"message":"继续检查"}),
                &ctx,
            )
            .await
            .unwrap();
        settle(&state.runtime, id).await;
        assert!(live.session.next_turn_number() > first);
        let events = live.session.events();
        let delivered = events
            .iter()
            .filter(|e| matches!(e.event, SessionEvent::AgentDelivery { .. }))
            .count();
        assert_eq!(delivered, 2);
        state.runtime.drain(id).await.unwrap();
        assert_eq!(
            live.session
                .events()
                .iter()
                .filter(|e| matches!(e.event, SessionEvent::AgentDelivery { .. }))
                .count(),
            2
        );
        // 冷恢复重建目录和收件箱，从持久化领取事件排除已投递项。
        let restored = Runtime::new(
            &state.home,
            state.sessions.clone(),
            state.live.clone(),
            state.settings.clone(),
            state.events.clone(),
            state.registry.clone(),
            state.workspaces.clone(),
        )
        .unwrap();
        assert_eq!(restored.list(owner).as_array().unwrap().len(), 1);
        restored.drain(id).await.unwrap();
        assert_eq!(
            live.session
                .events()
                .iter()
                .filter(|e| matches!(e.event, SessionEvent::AgentDelivery { .. }))
                .count(),
            2
        );
    }
    #[tokio::test]
    async fn admission_interrupt_and_owner_cleanup() {
        let (state, mut ctx) = setup();
        ctx.selection.as_mut().unwrap().model = "hold".into();
        state
            .settings
            .update("runtime", json!({"maxAgents":1}), None)
            .unwrap();
        let result = state
            .runtime
            .execute("spawn_agent", json!({"prompt":"保持运行"}), &ctx)
            .await
            .unwrap();
        let id = result["childId"].as_str().unwrap();
        assert!(
            state
                .runtime
                .execute("spawn_agent", json!({"prompt":"超额"}), &ctx)
                .await
                .is_err()
        );
        assert!(
            state
                .runtime
                .remove_owner(ctx.session_id.as_deref().unwrap())
                .await
                .is_err()
        );
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while state
                .live
                .get(id)
                .is_none_or(|l| l.cancel.lock().unwrap().is_none())
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        state
            .runtime
            .interrupt(ctx.session_id.as_deref().unwrap(), id)
            .await
            .unwrap();
        settle(&state.runtime, id).await;
        state
            .runtime
            .remove_owner(ctx.session_id.as_deref().unwrap())
            .await
            .unwrap();
        assert!(state.sessions.load(id).is_err());
    }
    #[tokio::test]
    async fn cancelled_start_leaves_no_child_and_config_rejects_invalid() {
        let (state, ctx) = setup();
        ctx.cancel.cancel();
        assert!(
            state
                .runtime
                .execute("spawn_agent", json!({"prompt":"不应创建"}), &ctx)
                .await
                .is_err()
        );
        assert!(
            state
                .runtime
                .list(ctx.session_id.as_deref().unwrap())
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            state
                .settings
                .update("runtime", json!({"maxDepth":0}), None)
                .is_err()
        );
        assert!(
            state
                .settings
                .update("runtime", json!({"customSkillDirs":["relative"]}), None)
                .is_err()
        );
    }

    #[tokio::test]
    async fn http_prompt_tools_jobs_skills_and_child_controls() {
        let (state, _) = setup();
        std::fs::create_dir_all(state.home.join("skills/runtime-test")).unwrap();
        std::fs::write(
            state.home.join("skills/runtime-test/SKILL.md"),
            "---\nname: runtime-test\ndescription: 接口验证技能\n---\n读取并验证运行时结果。",
        )
        .unwrap();
        let state = Arc::new(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let router = crate::api::router().with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::new();
        let created: Value = client
            .post(format!("{base}/api/sessions"))
            .json(&json!({"cwd":state.home}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = created["session"]["id"].as_str().unwrap();
        assert_eq!(client.post(format!("{base}/api/sessions/{id}/prompt")).json(&json!({"prompt":"验证三个系统","provider":"runtime-test","model":"orchestrator","skills":["runtime-test"]})).send().await.unwrap().status(),axum::http::StatusCode::ACCEPTED);
        tokio::time::timeout(std::time::Duration::from_secs(15), async {
            loop {
                let live = state.live.get(id).unwrap();
                let count = live
                    .session
                    .events()
                    .iter()
                    .filter(|e| matches!(e.event, SessionEvent::ToolResult { .. }))
                    .count();
                if count >= 4 && !live.running.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap();
        let live = state.live.get(id).unwrap();
        let events = live.session.events();
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.event, SessionEvent::ToolResult { is_error: true, .. })),
            "工具链不得出错：{events:?}"
        );
        let children: Value = client
            .get(format!("{base}/api/sessions/{id}/agents"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let child = children["agents"][0]["id"].as_str().unwrap();
        assert!(
            !client
                .post(format!("{base}/api/sessions/{child}/prompt"))
                .json(&json!({"prompt":"直接写入"}))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let jobs: Value = client
            .get(format!("{base}/api/sessions/{id}/jobs"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let job = jobs["jobs"][0]["id"].as_str().unwrap();
        let output: Value = client
            .get(format!("{base}/api/sessions/{id}/jobs/{job}/output"))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(output["output"].as_str().unwrap().contains("runtime-e2e"));
        assert!(
            !client
                .get(format!("{base}/api/sessions/{child}/jobs/{job}/output"))
                .send()
                .await
                .unwrap()
                .status()
                .is_success()
        );
        let skills: Value = client
            .get(format!("{base}/api/sessions/{id}/skills"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(
            skills["skills"]
                .as_array()
                .unwrap()
                .iter()
                .any(|s| s["name"] == "runtime-test")
        );
        // 注入式加载：skill load 后 SKILL.md 正文应出现在能力上下文，而非等模型读文件。
        let injected = state
            .runtime
            .context(id, &state.home)
            .await
            .unwrap()
            .join("\n");
        assert!(
            injected.contains("读取并验证运行时结果。"),
            "SKILL.md 正文应经上下文注入：{injected}"
        );
        server.abort();
    }
}
