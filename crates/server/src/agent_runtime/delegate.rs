use super::*;

impl Runtime {
    /// 关闭某会话名下的浏览器 tab（父会话与不支持归属的中枢都是 no-op）。
    pub(super) async fn close_owned_tabs(&self, id: &str) {
        let Some(hub) = self.browser_hub() else {
            return;
        };
        let closed = hub.close_owned(id).await;
        if closed > 0 {
            tracing::info!(session = id, closed, "已关闭会话名下遗留的浏览器 tab");
        }
    }

    /// 派遣：解析定义/临时规格 → 校验身份与授权 → 冻结快照 → 持久入队。
    ///
    /// 硬规则（第 6.2 节）在这里做**第一层**服务端判定：任何宿主派遣入口都必须
    /// 在任何分配副作用之前确认调用者不是子代理。判定依据是真实会话头里的
    /// `subagent` 身份，不看 label、depth、调用参数，也不依赖内存 children 表。
    pub(super) async fn delegate(
        &self,
        fork: bool,
        args: denia_tools::runtime_command::DelegateArgs,
        ctx: &ToolContext,
    ) -> Result<Value, String> {
        use denia_core::session::SubagentProfileRef;
        use denia_core::subagent::codes as subagent_codes;

        let owner = ctx.session_id.as_deref().ok_or("缺少会话身份")?;
        let config = self.config();
        let parent = self.live(owner).await?;

        // ① 子代理禁止派遣子代理：硬拒，且不产生任何副作用。
        if parent.session.header().subagent.is_some() {
            return Err(format!(
                "子代理不能派遣子代理（{}）：请把可并行的子任务交回父代理，或在本次任务内自己完成。",
                subagent_codes::DELEGATION_FORBIDDEN
            ));
        }
        // ② 参数契约（max_depth、profile/inline 互斥、旧字段歧义、模型成套）。
        crate::subagents::resolver::validate_delegate_args(&args)
            .map_err(|error| error.render())?;

        // ③ 父会话授权上下文：注册表 + preset/features/权限档 → parentGrant。
        let driver = self
            .inner
            .driver
            .get()
            .and_then(Weak::upgrade)
            .ok_or("会话驱动器尚未就绪，暂时无法派遣子代理")?;
        let registered = driver.tools().names();
        let parent_preset_id = parent.session.agent_preset();
        let preset = driver.preset_for(parent_preset_id.as_deref());
        let features = driver.preset_features(parent_preset_id.as_deref());
        let preset_tools = preset.as_ref().and_then(|preset| preset.tools.as_deref());
        let parent_mode = parent.session.permission_mode();
        let grant = crate::subagents::resolver::parent_grant(
            &registered,
            &features,
            preset_tools,
            parent_mode,
        );

        // ④ 解析规格：profile_id / inline / 默认 develop / 旧参数兼容路径。
        let cwd = ctx.cwd.clone();
        let project_root = cwd
            .is_dir()
            .then(|| crate::subagents::project_root_for(&cwd));
        let parent_selection = ctx.selection.clone().ok_or("子代理无法继承模型选择")?;
        let legacy_path = args.profile_id.is_none()
            && args.inline.is_none()
            && (args.persona.is_some() || args.allowed_tools.is_some());
        let mut dispatch = if legacy_path {
            crate::subagents::resolver::legacy_dispatch(&args, fork, &parent_selection, &registered)
                .map_err(|error| error.render())?
        } else {
            let resolved = match (&args.profile_id, &args.inline) {
                (Some(raw), _) => Some(
                    self.inner
                        .profiles
                        .resolve(project_root.as_deref(), raw)
                        .map_err(|error| error.render())?,
                ),
                (None, Some(_)) => None,
                (None, None) => Some(
                    self.inner
                        .profiles
                        .resolve_default(project_root.as_deref())
                        .map_err(|error| error.render())?,
                ),
            };
            crate::subagents::resolver::resolve_dispatch(
                &args,
                fork,
                &parent_selection,
                &grant,
                &registered,
                resolved.as_ref(),
                args.profile_id.is_none() && args.inline.is_none(),
            )
            .map_err(|error| error.render())?
        };
        // 模型可用性：显式配置不可用时**回退为跟随主会话**（需求：未配置、
        // 配置无效或模型不可用都不能阻断派遣）。回退事实随结果返回，界面
        // 与调用方都能看到"实际用的不是定义里写的那个"。
        let mut model_fallback: Option<String> = None;
        if let Err(error) = self
            .inner
            .registry
            .resolve_call(
                &dispatch.selection.provider,
                &dispatch.selection.model,
                dispatch.selection.reasoning_effort.as_deref(),
            )
            .await
        {
            if dispatch.model_origin == crate::subagents::resolver::ModelOrigin::ProfileExplicit {
                model_fallback = Some(format!(
                    "子代理定义的模型 {}/{} 当前不可用（{}），已回退为跟随主会话 {}/{}",
                    dispatch.selection.provider,
                    dispatch.selection.model,
                    error,
                    parent_selection.provider,
                    parent_selection.model
                ));
                dispatch.selection = parent_selection.clone();
                dispatch.model_origin = crate::subagents::resolver::ModelOrigin::Parent;
            } else {
                return Err(error.to_string());
            }
        }
        let prompt = args.prompt.clone();
        if prompt.len() > config.output_bytes {
            return Err("子代理提示词过大".into());
        }
        if dispatch.instructions.len() > denia_core::subagent::INSTRUCTIONS_MAX_BYTES {
            return Err("子代理角色提示词过大".into());
        }
        let mut deprecations: Vec<String> = Vec::new();
        if args.run_in_background.is_some() {
            deprecations
                .push("run_in_background 已废弃：派遣始终后台执行，该参数被忽略".to_string());
        }
        if legacy_path {
            deprecations.push(
                "persona/allowed_tools 是旧参数（已按保守历史上限解释）；请改用 profile_id 或 inline"
                    .to_string(),
            );
        }

        let label = args
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or(&dispatch.name)
            .chars()
            .take(200)
            .collect::<String>();
        let descriptor = SubagentDescriptor {
            label,
            // 新 child 恒为 1；谱系展示与旧数据兼容用，不作为派遣准入依据。
            depth: 1,
            mode: if fork { "fork" } else { "spawn" }.into(),
            selection: dispatch.selection.clone(),
            // 旧字段不再承载新 child 的授权语义。
            persona: None,
            allowed_tools: None,
            snapshot_version: denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION,
            name: Some(dispatch.name.clone()),
            description: Some(dispatch.description.clone()),
            profile: Some(SubagentProfileRef {
                qualified_id: dispatch.qualified_id.clone(),
                source: dispatch.source,
                revision: dispatch.revision,
                inline: dispatch.inline,
            }),
            effective_tools: Some(dispatch.tools.clone()),
            permission_ceiling: dispatch.permission_ceiling,
            created_permission_mode: Some(parent_mode.as_str().to_string()),
            // 审计字段：恒 false。真正的硬拒在服务端身份判定与 exec 层。
            delegation_allowed: false,
            instruction_scope: Some("project-only".into()),
            parent_preset: parent_preset_id.clone(),
            parent_preset_persona: preset
                .as_ref()
                .and_then(|preset| preset.persona.clone())
                .filter(|text| !text.trim().is_empty()),
            parent_features: Some(features),
            instructions_ref: Some(SUBAGENT_SNAPSHOT_FILE.to_string()),
            instructions_hash: Some(instructions_hash(&dispatch.instructions)),
            fork: None,
            legacy: None,
        };
        let descriptor = SubagentDescriptor {
            fork: fork.then(|| denia_core::session::SubagentForkProjection {
                source_session: owner.to_string(),
                cut_seq: 0,
                projection_version: denia_session::SUBAGENT_SEED_VERSION,
                dropped: Vec::new(),
            }),
            ..descriptor
        };

        // ⑤ 准入：与终态 child 恢复、消息唤醒共用同一份槽位账。
        let admission = self.inner.admission.enter().await;
        let limit = self.policy().max_concurrent_runs;
        if self.inner.admission.reserved_count() >= limit {
            return Err(format!(
                "子代理并发已达上限（{limit}）；请等已有子代理结束，或在设置里提高 subagent-policy.maxConcurrentRuns。"
            ));
        }
        if ctx.cancel.is_cancelled() {
            return Err("子代理启动已取消".into());
        }

        // ⑥ fork 投影：只取已闭合轮次，丢弃父注入/摘要/收件箱/运行态。
        let (source, fork_projection) = if fork {
            let seed = denia_session::build_subagent_seed(parent.session.events().as_slice());
            (
                seed.events,
                Some(denia_core::session::SubagentForkProjection {
                    source_session: owner.to_string(),
                    cut_seq: seed.cut_seq,
                    projection_version: denia_session::SUBAGENT_SEED_VERSION,
                    dropped: seed.dropped,
                }),
            )
        } else {
            (Vec::new(), None)
        };
        let descriptor = SubagentDescriptor {
            fork: fork_projection,
            ..descriptor
        };

        let sessions = self.inner.sessions.clone();
        let child_cwd = cwd.clone();
        let sandbox = parent.session.header().sandbox;
        let parent_id = owner.to_string();
        let desc = descriptor.clone();
        let instructions = dispatch.instructions.clone();
        let child_permission = crate::subagents::resolver::effective_permission_mode(
            dispatch.permission_ceiling,
            parent_mode,
        );
        let child_preset = parent_preset_id.clone();
        // —— 崩溃窗口（测试注入）：快照落盘后 / 入队前后 / 返回 pending 前后 ——
        // 任何一个窗口失败都必须回滚本次新增的会话、槽位与目录条目。
        if self.test_fault("create") {
            return Err("注入故障：创建子会话前失败".to_string());
        }
        let id = tokio::task::spawn_blocking(move || {
            let child = sessions
                .create_subagent(&child_cwd, sandbox, &parent_id, desc)
                .map_err(|e| e.to_string())?;
            let result: Result<String, String> = (|| {
                child.seed_from(&source).map_err(|e| e.to_string())?;
                write_subagent_snapshot(&sessions, child.id(), &instructions)?;
                child
                    .set_permission_mode(child_permission)
                    .map_err(|e| e.to_string())?;
                if let Some(preset) = &child_preset {
                    child.set_agent_preset(preset).map_err(|e| e.to_string())?;
                }
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
        if self.test_fault("snapshot") {
            // 会话与快照都已落盘，随后这一步失败：走真实回滚路径。
            self.rollback_child(&id).await;
            return Err("注入故障：快照落盘后失败".to_string());
        }
        if !self.inner.admission.reserve(&id, limit) {
            self.rollback_child(&id).await;
            return Err(format!("子代理并发已达上限（{limit}）"));
        }
        for workspace in self.inner.workspaces.list() {
            if workspace.session_ids.iter().any(|s| s == owner)
                && !self.inner.workspaces.attach(&workspace.id, &id)
            {
                self.rollback_child(&id).await;
                return Err("父工作区已删除，子代理创建已回滚".into());
            }
        }
        self.inner.children.lock().unwrap().insert(
            id.clone(),
            Child {
                id: id.clone(),
                parent_id: owner.into(),
                descriptor: descriptor.clone(),
            },
        );
        self.inner
            .selections
            .lock()
            .unwrap()
            .insert(id.clone(), dispatch.selection.clone());
        if ctx.cancel.is_cancelled() {
            self.rollback_child(&id).await;
            return Err("子代理启动已取消".into());
        }
        if self.test_fault("pre-enqueue") {
            self.rollback_child(&id).await;
            return Err("注入故障：入队前失败".to_string());
        }
        let enqueued = if self.test_fault("enqueue") {
            Err("注入故障：入队失败".to_string())
        } else {
            self.enqueue(
                &id,
                // 通知身份按 childId + generation 生成，续跑不复用上一次结果。
                uuid::Uuid::new_v4().to_string(),
                format!("[父代理 {owner} 委派任务]\n{prompt}"),
                format!("agent:{owner}"),
            )
            .await
        };
        let message_id = match enqueued {
            Ok(id) => id,
            Err(error) => {
                self.rollback_child(&id).await;
                return Err(error);
            }
        };
        if self.test_fault("post-enqueue") {
            // 派发已持久化：这里失败**不**回滚——child 的会话、首条 inbox 与
            // 完成通知都是持久事实，工具结果丢了也不能删掉正在运行的 child。
            return Err("注入故障：返回 pending 前失败".to_string());
        }
        let _ = self.inner.events.send(ServerEvent::SessionsUpdated);
        let mut trace = crate::trajectory::TraceRecord::now(owner, "subagent");
        trace.operation = Some("spawned".into());
        trace.agent_id = Some(owner.to_string());
        trace.call_id = Some(id.clone());
        trace.data = Some(json!({
            "childId": id,
            "profile": dispatch.qualified_id,
            "inline": dispatch.inline,
            "model": dispatch.selection,
            "fork": fork,
        }));
        self.inner.trace.record(&trace);
        drop(admission);
        let mut result = background_result(json!({"childId":id,"messageId":message_id}), true);
        // 只回摘要，不回整份提示快照（计划 8.3）。
        result["profile"] = json!({
            "qualifiedId": dispatch.qualified_id,
            "name": dispatch.name,
            "inline": dispatch.inline,
            "tools": dispatch.tools,
            "toolCount": dispatch.tools.len(),
            "model": dispatch.selection,
            "permissionCeiling": dispatch.permission_ceiling.as_str(),
            "fingerprint": dispatch.fingerprint(),
        });
        if !deprecations.is_empty() {
            result["deprecations"] = json!(deprecations);
        }
        if let Some(fallback) = model_fallback {
            result["modelFallback"] = json!(fallback);
        }
        Ok(result)
    }
}

/// 子代理运行快照文件：放在会话目录内的不可变大文本。
///
/// header 只保存引用与 hash（否则会话列表每行都会携带最多 64 KiB 的角色全文）。
/// 引用不存在或 hash 不匹配一律拒绝启动，不回退默认。
const SUBAGENT_SNAPSHOT_FILE: &str = "subagent.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SubagentSnapshotFile {
    snapshot_version: u32,
    instructions: String,
}

/// 稳定内容 hash（FNV-1a）：校验快照文件与 header 记录是否一致。
pub(crate) fn instructions_hash(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub(super) fn write_subagent_snapshot(
    sessions: &denia_session::SessionStore,
    id: &str,
    instructions: &str,
) -> Result<(), String> {
    let dir = sessions.root().join(id);
    std::fs::create_dir_all(&dir).map_err(|error| format!("创建会话目录失败：{error}"))?;
    let payload = serde_json::to_string(&SubagentSnapshotFile {
        snapshot_version: denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION,
        instructions: instructions.to_string(),
    })
    .map_err(|error| format!("序列化子代理快照失败：{error}"))?;
    let tmp = dir.join("subagent.json.tmp");
    std::fs::write(&tmp, payload.as_bytes()).map_err(|error| {
        format!(
            "写入子代理快照失败（{}）：{error}（snapshot/io）",
            dir.display()
        )
    })?;
    std::fs::rename(&tmp, dir.join(SUBAGENT_SNAPSHOT_FILE))
        .map_err(|error| format!("替换子代理快照失败：{error}（snapshot/io）"))?;
    Ok(())
}

/// 读取并校验一份子代理快照。
///
/// - 新快照（`snapshotVersion >= 1`）：文件必须存在，且角色正文 hash 与 header
///   一致；不一致说明会话目录被外部改动，拒绝启动而不是回退到空角色。
/// - 旧快照（版本 0）：没有引用文件，角色正文取旧 `persona` 字段（解释为
///   角色补充），授权按 [`crate::subagents::resolver::legacy_grant`] 保守构造。
pub(crate) fn read_subagent_snapshot(
    sessions: &denia_session::SessionStore,
    id: &str,
    descriptor: &SubagentDescriptor,
) -> Result<String, String> {
    use denia_core::subagent::codes as subagent_codes;
    if descriptor.snapshot_version < denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION {
        return Ok(descriptor.persona.clone().unwrap_or_default());
    }
    let Some(reference) = descriptor.instructions_ref.as_deref() else {
        return Err(format!(
            "子代理快照缺少引用（{}）：拒绝启动，避免以空角色运行",
            subagent_codes::SNAPSHOT_INVALID
        ));
    };
    if reference != SUBAGENT_SNAPSHOT_FILE {
        return Err(format!(
            "子代理快照引用了未知文件 `{reference}`（{}）",
            subagent_codes::SNAPSHOT_INVALID
        ));
    }
    let path = sessions.root().join(id).join(reference);
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        format!(
            "子代理快照文件不可读（{}）：{}（{error}）",
            path.display(),
            subagent_codes::SNAPSHOT_INVALID
        )
    })?;
    let file: SubagentSnapshotFile = serde_json::from_str(&raw).map_err(|error| {
        format!(
            "子代理快照文件格式无效（{}）：{error}",
            subagent_codes::SNAPSHOT_INVALID
        )
    })?;
    if let Some(expected) = descriptor.instructions_hash.as_deref()
        && expected != instructions_hash(&file.instructions)
    {
        return Err(format!(
            "子代理快照与 header 记录的 hash 不一致（{}）：拒绝启动，不回退默认角色",
            subagent_codes::SNAPSHOT_INVALID
        ));
    }
    Ok(file.instructions)
}
