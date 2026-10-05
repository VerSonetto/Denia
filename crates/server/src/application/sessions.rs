//! Human prompt admission, attachments and background turn execution.
use super::error::{CommandError, FailureKind};
use crate::state::{RunningGuard, ServerEvent};
use denia_core::config::ModelSelection;
use denia_core::session::SessionEnvelope;
use serde::Deserialize;
use std::sync::{Arc, atomic::Ordering};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptCommand {
    prompt: String,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    /// 粘贴的内联图片(base64);模型必须标记为可识图。
    #[serde(default)]
    images: Vec<PromptImage>,
    /// 已上传文件(绝对路径);作为注入上下文随消息发送。
    #[serde(default)]
    files: Vec<String>,
    /// 轨迹引用(标题, 正文);以 injected 上下文消息随本轮注入。
    #[serde(default)]
    quoted: Vec<PromptQuote>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptQuote {
    title: String,
    text: String,
}

/// 引用正文上限:单条 64k 字符、合计 256k,防异常体积拖垮本轮。
const QUOTE_TEXT_LIMIT: usize = 64 * 1024;
const QUOTE_TOTAL_LIMIT: usize = 256 * 1024;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptImage {
    #[serde(default)]
    name: Option<String>,
    mime: String,
    data: String,
}

/// 模型是否声明了图片输入能力(input_modalities 含 image)。
pub(crate) fn model_vision_supported(resolved: &denia_llm::LlmResolvedModelInfo) -> bool {
    resolved
        .info
        .input_modalities
        .iter()
        .any(|modality| modality.eq_ignore_ascii_case("image"))
}

pub struct SessionServices<'a> {
    pub home: &'a std::path::Path,
    pub sessions: &'a denia_session::SessionStore,
    pub live: &'a crate::state::LiveSessions,
    pub registry: Arc<denia_llm::LlmRegistry>,
    pub driver: Arc<denia_agent_loop::SessionDriver>,
    pub runtime: Arc<crate::agent_runtime::Runtime>,
    pub events: tokio::sync::broadcast::Sender<ServerEvent>,
}

pub struct SessionCommands<'a> {
    services: SessionServices<'a>,
}
impl<'a> SessionCommands<'a> {
    pub fn new(services: SessionServices<'a>) -> Self {
        Self { services }
    }
    pub async fn submit_prompt(&self, id: String, body: PromptCommand) -> Result<(), CommandError> {
        let prompt = body.prompt.trim().to_string();
        if prompt.is_empty() {
            return Err(CommandError::bad_request(
                "session/empty-prompt",
                "prompt is empty",
            ));
        }
        let live = self
            .services
            .live
            .get_or_load(self.services.sessions, &id)
            .map_err(CommandError::from_session)?;
        self.services
            .live
            .ensure_hot(&live)
            .map_err(CommandError::from_session)?;
        if live.session.header().subagent.is_some() {
            return Err(CommandError::bad_request(
                "subagent/read-only",
                "请从父会话向子代理发送指令",
            ));
        }
        {
            let session = live.session.clone();
            let cwd = session.header().cwd.clone();
            if !std::path::Path::new(&cwd).is_dir() {
                return Err(CommandError::bad_request(
                    "session/dead-cwd",
                    format!(
                        "this session's working directory no longer exists: {cwd} — start a new session in an existing directory"
                    ),
                ));
            }
        }
        if live
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(CommandError::new(
                FailureKind::Conflict,
                "session/running",
                "a turn is already running on this session",
            ));
        }
        let admission = RunningGuard::new(live.clone(), self.services.events.clone());
        // 运行状态立即推给控制台(侧栏圆点/发送按钮,无需 follow 长连接)。
        let _ = self.services.events.send(ServerEvent::RunningChanged {
            id: id.clone(),
            running: true,
        });
        // 消息落库后摘要变化:通知控制台刷新列表(excerpt 即时可见)。
        let _ = self.services.events.send(ServerEvent::SessionsUpdated);

        // 模型选择无服务端默认:前端为每个请求携带"上次使用的模型"。
        let provider = body.provider.unwrap_or_default().trim().to_string();
        let model = body.model.unwrap_or_default().trim().to_string();
        if provider.is_empty() || model.is_empty() {
            return Err(CommandError::bad_request(
                "session/model-required",
                "请先在输入栏选择模型再发送消息",
            ));
        }
        let selection = ModelSelection {
            provider,
            model,
            reasoning_effort: body.reasoning_effort,
        };
        let resolved = match self
            .services
            .registry
            .resolve_call(
                &selection.provider,
                &selection.model,
                selection.reasoning_effort.as_deref(),
            )
            .await
        {
            Ok(resolved) => resolved,
            Err(error) => {
                return Err(CommandError::from_llm(error));
            }
        };
        let vision_supported = model_vision_supported(&resolved);
        // 用户要求:图片只允许发给标记为可识图的模型。
        if !body.images.is_empty() && !vision_supported {
            let shown = if selection.model.is_empty() {
                "当前模型".to_string()
            } else {
                format!("模型 '{}'", selection.model)
            };
            return Err(CommandError::bad_request(
                "model/no-vision",
                format!("{shown} 未标记为可识图,粘贴的图片不能发送;请切换到支持图片输入的模型。"),
            ));
        }
        // 上传文件校验:必须存在且位于会话工作区或本会话上传目录内。
        let mut upload_files = Vec::new();
        {
            let cwd = live.session.header().cwd.clone();
            let uploads_root = self.services.home.join("uploads").join(&id);
            for file in &body.files {
                let path = std::path::PathBuf::from(file);
                let allowed =
                    (path.starts_with(&cwd) || path.starts_with(&uploads_root)) && path.is_file();
                if !allowed {
                    return Err(CommandError::bad_request(
                        "session/bad-attachment",
                        format!("文件 '{}' 不在会话工作区或上传目录内", file),
                    ));
                }
                upload_files.push(path.to_string_lossy().to_string());
            }
        }
        // 粘贴图片落盘:与上传文件同一目录、同一套消毒规则。内联 data URL 仍是
        // 首选视觉通道,落盘只是给模型留一条 read_file 可走的备份 —— 协议转换层
        // 或提供方把图片 part 丢掉时,有路径才可能自救。失败一律降级为无路径:
        // 图仍照发,不因备份写不出而拒收整条消息。
        let persisted: Vec<Option<String>> = if body.images.is_empty() {
            Vec::new()
        } else {
            let home = self.services.home.to_path_buf();
            let sid = id.clone();
            let uploads = body
                .images
                .iter()
                .enumerate()
                .map(|(index, image)| {
                    (
                        index,
                        crate::infrastructure::uploads::pasted_image_name(
                            image.name.as_deref(),
                            &image.mime,
                            index,
                        ),
                        image.data.clone(),
                        image.mime.clone(),
                    )
                })
                .collect::<Vec<_>>();
            tokio::task::spawn_blocking(move || {
            let engine = &base64::engine::general_purpose::STANDARD;
            let mut slots: Vec<Option<String>> = vec![None; uploads.len()];
            for (index, name, data, mime) in uploads {
                let Ok(bytes) = base64::Engine::decode(engine, data.as_bytes()) else {
                    tracing::warn!(target: "denia::uploads", "粘贴图片 base64 解码失败,跳过落盘: {name}");
                    continue;
                };
                match crate::infrastructure::uploads::write_upload(&home, sid.as_str(), &name, &bytes) {
                    Ok((_, path)) => slots[index] = Some(path.to_string_lossy().into_owned()),
                    Err(error) => tracing::warn!(
                        target: "denia::uploads",
                        "粘贴图片落盘失败({mime}): {error}"
                    ),
                }
            }
            slots
        })
        .await
        .unwrap_or_default()
        };
        let images: Vec<denia_core::message::ImageData> = body
            .images
            .into_iter()
            .enumerate()
            .map(|(index, image)| denia_core::message::ImageData {
                mime: image.mime,
                data: image.data,
                path: persisted.get(index).cloned().flatten(),
            })
            .collect();
        // 轨迹引用:非空校验 + 体积上限(fail loud,不静默丢弃)。
        let mut quoted: Vec<(String, String)> = Vec::with_capacity(body.quoted.len());
        for name in &body.skills {
            let skill = match self
                .services
                .runtime
                .load_skill(live.session.header().cwd.clone().into(), name.clone(), true)
                .await
            {
                Ok(skill) => skill,
                Err(e) => {
                    return Err(CommandError::bad_request("skill/invocation-failed", e));
                }
            };
            quoted.push((format!("用户显式调用技能：{name}"), skill.to_string()));
        }
        let mut quoted_total = 0usize;
        for quote in body.quoted {
            let title = quote.title.trim().to_string();
            let text = quote.text.trim().to_string();
            if text.is_empty() {
                return Err(CommandError::bad_request(
                    "session/empty-quote",
                    "引用的轨迹内容为空",
                ));
            }
            if title.len() > 200 || text.len() > QUOTE_TEXT_LIMIT {
                return Err(CommandError::bad_request(
                    "session/quote-too-large",
                    format!("引用过大:标题 ≤ 200 字符、单条正文 ≤ {QUOTE_TEXT_LIMIT} 字符"),
                ));
            }
            quoted_total += text.len();
            if quoted_total > QUOTE_TOTAL_LIMIT {
                return Err(CommandError::bad_request(
                    "session/quote-too-large",
                    format!("引用总量超过 {QUOTE_TOTAL_LIMIT} 字符上限"),
                ));
            }
            quoted.push((title, text));
        }

        let token = CancellationToken::new();
        self.services.runtime.human_turn(&id, &selection);
        *live.cancel.lock().unwrap() = Some(token.clone());

        // 会话标题:第一轮用户消息(本会话还没有任何用户消息、非分支会话)
        // 发出后,后台用同路由模型生成;dsh `first-prompt` 节奏的对齐——
        // 分支会话沿用源会话标题,子代理有 label,都不生成。
        if live.session.header().parent_session.is_none()
            && live.session.first_prompt_excerpt(1).is_none()
        {
            crate::session_title::schedule(
                self.services.registry.clone(),
                live.clone(),
                self.services.events.clone(),
                selection.clone(),
                prompt.clone(),
            );
        }

        let followers_for_turn = live.followers.clone();
        let driver = self.services.driver.clone();
        let session = live.session.clone();
        let runtime = self.services.runtime.clone();
        tokio::spawn(async move {
            // RAII:任务结束(含 panic)自动复位 running + 清 cancel + 广播结束。
            let _guard = admission;
            let emit: Arc<dyn Fn(&SessionEnvelope) + Send + Sync> =
                Arc::new(move |envelope: &SessionEnvelope| {
                    // 无人订阅时不克隆:流式期间每秒几十条 chunk,每条都克隆一份
                    // 只为了喂给一个空的广播队列,纯属浪费。
                    if followers_for_turn.receiver_count() == 0 {
                        return;
                    }
                    let _ = followers_for_turn.send(envelope.clone());
                });
            let _reason = driver
                .run_turn(
                    &session,
                    &selection,
                    &prompt,
                    images,
                    upload_files,
                    quoted,
                    vision_supported,
                    token,
                    emit,
                )
                .await;
            drop(_guard);
            runtime.on_idle(session.id());
            // 上下文管理(投影剪枝 + LLM 压缩)已内建于 driver 的请求构造前
            // (denia_agent_loop::SessionDriver):轮次闭合后不再落盘替换,
            // 日志保持 append-only,provider 前缀缓存不被破坏。
        });

        Ok(())
    }
}
