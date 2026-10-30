use super::*;

impl Runtime {
    pub async fn skills(&self, cwd: PathBuf) -> Result<Vec<crate::skills::Skill>, String> {
        let home = self.inner.home.clone();
        tokio::task::spawn_blocking(move || crate::skills::discover(&home, &cwd))
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

pub(super) fn background_result(mut result: Value, pending: bool) -> Value {
    result["status"] = json!(if pending { "pending" } else { "ready" });
    result["pending"] = json!(pending);
    if pending {
        result["notification"] = json!("automatic");
        result["next_action"] = json!(
            "后台仍在执行，完成结果会自动送达本会话。不要轮询或重复等待；可以继续独立工作，或先向用户回复当前进度并结束本轮。pending 不代表执行成功。"
        );
    }
    result
}

#[async_trait]
impl AgentRuntime for Runtime {
    async fn execute_command(
        &self,
        command: denia_tools::runtime_command::RuntimeCommand,
        ctx: &ToolContext,
    ) -> Result<Value, String> {
        let owner = ctx.session_id.as_deref().ok_or("该能力需要会话身份")?;
        let config = self.config();
        use denia_tools::runtime_command::{RuntimeCommand, SkillCommand};
        match command {
            RuntimeCommand::Skill(action) => match action {
                SkillCommand::List => Ok(json!(
                    self.skills(ctx.cwd.clone())
                        .await?
                        .into_iter()
                        .filter(|s| s.model_invocable)
                        .collect::<Vec<_>>()
                )),
                SkillCommand::Resource { name, path } => {
                    let skills = self.skills(ctx.cwd.clone()).await?;
                    tokio::task::spawn_blocking(move || {
                        crate::skills::resource(&skills, &name, &path)
                    })
                    .await
                    .map_err(|e| e.to_string())?
                }
                SkillCommand::Load { name } => {
                    // SKILL.md 全文直接随工具结果返回,一次性进历史;
                    // load 前经 skills::load 校验 model_invocable(user=false),
                    // disable-model-invocation 技能只能走用户 /name 手势。
                    self.load_skill(ctx.cwd.clone(), name, false).await
                }
            },
            RuntimeCommand::JobStart(args) => {
                let command = args.command;
                let timeout = args
                    .timeout_ms
                    .unwrap_or(config.job_timeout_ms)
                    .min(config.job_timeout_ms);
                Ok(json!(self.inner.jobs.start(
                    ctx,
                    &command,
                    args.label.as_deref().unwrap_or(&command),
                    timeout,
                    config.max_jobs,
                    config.retained_jobs,
                    config.output_bytes
                )?))
            }
            RuntimeCommand::JobList(_) => Ok(json!(self.inner.jobs.list(owner))),
            RuntimeCommand::JobOutput(args) => {
                let id = args.id;
                let _lease = self.inner.jobs.wait_lease(&id, owner)?;
                let result = self.inner.jobs.read(&id, owner)?;
                let pending = result["job"]["finishedAt"].is_null();
                Ok(background_result(result, pending))
            }
            RuntimeCommand::JobKill(args) => {
                self.inner.jobs.kill(&args.id, owner)?;
                Ok(json!({"ok":true}))
            }
            RuntimeCommand::ListAgents(_) => Ok(self.list(owner)),
            RuntimeCommand::InterruptAgent(args) => {
                self.interrupt(owner, &args.target).await?;
                Ok(json!({"ok":true}))
            }
            RuntimeCommand::SendMessage(args) => {
                let target = args.target;
                self.authorize(owner, &target, false)?;
                let id = self
                    .enqueue(
                        &target,
                        uuid::Uuid::new_v4().to_string(),
                        format!("[代理 {owner} 发来消息]\n{}", args.message),
                        format!("agent:{owner}"),
                    )
                    .await?;
                Ok(json!({"messageId":id,"target":target}))
            }
            RuntimeCommand::WaitAgent(args) => {
                let target = args.target;
                self.authorize(owner, &target, true)?;
                let live = self.live(&target).await?;
                let pending = self.is_active(&target);
                let messages = if pending {
                    Vec::new()
                } else {
                    live.session
                        .derive_messages()
                        .into_iter()
                        .rev()
                        .take(1)
                        .collect::<Vec<_>>()
                };
                // 交接信息与完成通知**同一份计算**(`subagent_handoff`):返回对象
                // 直接铺开这份结构的字段(不是手抄一份键表),父代理在通知里读到
                // 的字段与这里查到的因此不可能漂移。
                let child = self.inner.children.lock().unwrap().get(&target).cloned();
                let handoff = subagent_handoff(child.as_ref(), &live.session, pending);
                let mut result = handoff.fields();
                // 既有字段原位保留:调用方与前端读的就是这三个(`id` 与交接里的
                // 子会话 id 同值,这里以命令参数为准重写一次)。
                result["id"] = json!(target);
                result["running"] = json!(pending);
                result["messages"] = json!(messages);
                Ok(background_result(result, pending))
            }
            RuntimeCommand::Delegate { fork, args } => {
                let runtime = self.clone();
                let ctx = ctx.clone();
                tokio::spawn(async move { runtime.delegate(fork, args, &ctx).await })
                    .await
                    .map_err(|e| format!("子代理启动任务失败：{e}"))?
            }
        }
    }
    async fn context(&self, session: &str, cwd: &Path) -> Result<Vec<String>, String> {
        let _ = cwd;
        let parent = self
            .inner
            .children
            .lock()
            .unwrap()
            .get(session)
            .map(|c| c.parent_id.clone());
        Ok(vec![format!(
            "[denia 能力上下文]\n当前代理：{session}；父代理：{}。",
            parent.as_deref().unwrap_or("无")
        )])
    }
    async fn skill_catalog(
        &self,
        session: &str,
        cwd: &Path,
    ) -> Result<Vec<(String, String)>, String> {
        let _ = session;
        let max = self.config().skill_catalog_description_max_chars;
        let skills = self.skills(cwd.to_path_buf()).await?;
        Ok(skills
            .iter()
            .filter(|s| s.model_invocable)
            .map(|s| {
                (
                    s.name.clone(),
                    crate::skills::truncate_chars(&s.description, max),
                )
            })
            .collect())
    }
    async fn user_skill(
        &self,
        session: &str,
        name: &str,
        cwd: &Path,
    ) -> Result<Option<(String, String)>, String> {
        let _ = session;
        // load(user=true) 校验 user_invocable;找不到/不允许/解析失败一律 None,
        // 手势降级为普通文本(未知名字不是这条边界认识的声明)。
        match self
            .load_skill(cwd.to_path_buf(), name.to_string(), true)
            .await
        {
            Ok(value) => Ok(Some((
                value["skill"]["source"].as_str().unwrap_or_default().into(),
                value["body"].as_str().unwrap_or_default().into(),
            ))),
            Err(_) => Ok(None),
        }
    }
    async fn workspace_instructions(
        &self,
        session: &str,
        cwd: &Path,
        touched: &[PathBuf],
        previous: Option<&str>,
    ) -> Result<Option<String>, String> {
        let config = self.config();
        if config.workspace_instructions_max_bytes == 0 {
            return Ok(None);
        }
        // 作用域由宿主按**真实会话身份**决定，模型与 profile 都无法指定。
        // 父 preset 关闭 agentsMd 时整条通道由调用方跳过（全局排除不等于重新
        // 打开父已禁用的注入能力）。
        let sessions = self.inner.sessions.clone();
        let session = session.to_string();
        let is_child = tokio::task::spawn_blocking(move || {
            sessions
                .read_log_header(&session)
                .map(|header| header.subagent.is_some())
        })
        .await
        .map_err(|error| error.to_string())?
        .unwrap_or(false);
        let scope = if is_child {
            crate::workspace_instructions::InstructionScope::ProjectOnly
        } else {
            crate::workspace_instructions::InstructionScope::GlobalAndProject
        };
        let home = self.inner.home.clone();
        let cwd = cwd.to_path_buf();
        let touched = touched.to_vec();
        // 每次重新发现并渲染，没有跨会话/跨作用域缓存：root 的项目指令缓存
        // 不可能被 child 复用（计划 9.2 的缓存键要求在此实现下天然成立）。
        let files = tokio::task::spawn_blocking(move || {
            crate::workspace_instructions::discover(
                &home,
                &cwd,
                &touched,
                config.workspace_instructions_max_source_bytes,
                scope,
            )
        })
        .await
        .map_err(|e| e.to_string())?;
        Ok(crate::workspace_instructions::render(
            &files,
            config.workspace_instructions_max_bytes as usize,
            previous,
        ))
    }
    fn memory_root_for(&self, cwd: &Path) -> Option<PathBuf> {
        if !self.config().memory_enabled {
            return None;
        }
        Some(crate::project_memory::memory_root(&self.inner.home, cwd))
    }
    /// 项目记忆索引(MEMORY.md)注入文本;None = 未启用。注入整份索引
    /// (只受行/字节预算约束),不按会话状态挑选条目:选择层曾按本轮累积的
    /// 触碰路径挑相关条目,结果每个新输入都换一次正文、整块重发,旧块还留在
    /// 模型面上。索引一行一条、体量很小,一次注入比反复增量便宜。
    async fn project_memory_index(&self, cwd: &Path) -> Result<Option<String>, String> {
        let config = self.config();
        if !config.memory_enabled {
            return Ok(None);
        }
        let home = self.inner.home.clone();
        let cwd = cwd.to_path_buf();
        let max_bytes = config.project_memory_max_bytes as usize;
        // 索引非空才注入;空桶不注入——记忆目录路径已由 assemble 阶段
        // 内联进「# 项目记忆」段,模型任何 step 都可见。

        tokio::task::spawn_blocking(move || {
            let root = crate::project_memory::memory_root(&home, &cwd);
            match crate::project_memory::read_index_for_injection(&root, max_bytes) {
                Some(index) if !index.trim().is_empty() => Ok(Some(
                    crate::project_memory::render_index_block(&root, &index),
                )),
                _ => Ok(None),
            }
        })
        .await
        .map_err(|e| e.to_string())?
    }
    /// 子代理运行快照：角色正文从会话目录内的文件读取并校验，授权取派遣时
    /// 冻结的显式列表。旧描述符（版本 0）没有文件与显式列表，按历史只读上限
    /// 保守构造，绝不等于 inherit。
    async fn subagent_prompt(
        &self,
        session: &str,
    ) -> Result<Option<denia_tools::capabilities::SubagentPrompt>, String> {
        let live = self.live(session).await?;
        let Some(descriptor) = live.session.header().subagent.clone() else {
            return Ok(None);
        };
        let sessions = self.inner.sessions.clone();
        let id = session.to_string();
        let desc = descriptor.clone();
        let instructions =
            tokio::task::spawn_blocking(move || read_subagent_snapshot(&sessions, &id, &desc))
                .await
                .map_err(|error| error.to_string())??;
        let legacy = descriptor.snapshot_version < denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION;
        let effective_tools = descriptor.effective_tools.clone().unwrap_or_else(|| {
            crate::subagents::resolver::legacy_grant(descriptor.allowed_tools.as_deref())
        });
        Ok(Some(denia_tools::capabilities::SubagentPrompt {
            name: descriptor
                .name
                .clone()
                .or_else(|| Some(descriptor.label.clone())),
            instructions,
            effective_tools,
            permission_ceiling: Some(descriptor.permission_ceiling.as_str().to_string()),
            parent_preset_persona: descriptor.parent_preset_persona.clone(),
            legacy,
        }))
    }

    /// 父代理可见的派遣目录 + 派遣纪律。
    ///
    /// 只公布限定 id、名称、描述与摘要（不含 instructions 全文）；子代理永不
    /// 注入；父 preset 关闭 subagents 时不出现（工具与纪律段同进退）。
    async fn subagent_catalog(&self, session: &str) -> Result<Option<String>, String> {
        let live = self.live(session).await?;
        if live.session.header().subagent.is_some() {
            return Ok(None);
        }
        let Some(driver) = self.inner.driver.get().and_then(Weak::upgrade) else {
            return Ok(None);
        };
        if !driver.features_for(&live.session).subagents {
            return Ok(None);
        }
        let cwd = PathBuf::from(live.session.header().cwd.clone());
        let root = cwd
            .is_dir()
            .then(|| crate::subagents::project_root_for(&cwd));
        let entries = self.inner.profiles.catalog_entries(root.as_deref());
        if entries.is_empty() {
            return Ok(None);
        }
        let mut lines = vec![
            "[denia 子代理目录]".to_string(),
            "可用子代理类型（qualifiedId｜名称｜用途｜工具｜模型）：".to_string(),
        ];
        for entry in &entries {
            lines.push(format!(
                "- {}｜{}｜{}｜{}｜{}",
                entry.qualified_id, entry.name, entry.description, entry.tools, entry.model
            ));
        }
        lines.push(String::new());
        lines.push(
            "派遣方式：profile_id 用上面的 qualifiedId；inline 在调用时给出临时定义（不落盘）；两者都省略时使用默认的 develop 定义。profile_id 与 inline 互斥。"
                .to_string(),
        );
        lines.push(
            "分工与等待：按不重叠的文件或模块分工，最终由你统一集成。派遣后立即返回 pending，不要轮询——继续独立工作或先向用户回复进度并结束本轮；完成结果会自动送达。执行完成前不要宣称工作已完成。"
                .to_string(),
        );
        lines.push(
            "结果汇报区分三种情形：成功结果、任务失败（附失败原因）、等待你决策。子代理不能再派遣子代理；子代理不会自动继承全局 AGENTS.md，只发现项目级规则。"
                .to_string(),
        );
        let text = lines.join("\n");
        // 独立字节预算：目录每步注入，必须自带上限与诊断，不能挤占指令预算，
        // 也不能因为超限就静默截断——截断必须明示。
        let budget = self.config().subagent_catalog_max_bytes as usize;
        if text.len() > budget {
            tracing::warn!(
                session = session,
                bytes = text.len(),
                budget,
                "子代理目录超出预算，已按 UTF-8 边界截断"
            );
            return Ok(Some(format!(
                "{}\n（目录已按 {budget} 字节预算截断，部分定义未列出；未列出的定义仍可直接用 profile_id 指定。）",
                truncate_utf8(&text, budget)
            )));
        }
        Ok(Some(text))
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
            inner.delivery.expedite(live.session.id());
            Ok(Vec::new())
        })
        .await
        .map_err(|e| e.to_string())?
    }

    /// 会话的冻结工具授权：MCP 目录等按会话投影的目录必须用它过滤。
    fn granted_tools(&self, session: &str) -> Option<Vec<String>> {
        if let Some(live) = self.inner.live.get(session) {
            return grant_of(live.session.header());
        }
        self.inner
            .sessions
            .read_log_header(session)
            .ok()
            .and_then(|header| grant_of(&header))
    }
}

/// 会话头里的冻结授权；非子代理返回 None（= 没有被定义收窄）。
pub(super) fn grant_of(header: &denia_core::session::SessionHeader) -> Option<Vec<String>> {
    let child = header.subagent.as_ref()?;
    Some(child.effective_tools.clone().unwrap_or_else(|| {
        denia_core::subagent::legacy_child_tools(child.allowed_tools.as_deref())
    }))
}
