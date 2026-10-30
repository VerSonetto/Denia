use super::*;

impl Runtime {
    pub(super) fn descendants(&self, root: &str) -> Vec<Child> {
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
        json!(self.descendants(owner).into_iter().filter(|child| child.descriptor.mode != "memory").map(|child|{
        let live=self.inner.live.get(&child.id);
        let running=live.as_ref().is_some_and(|l|l.running.load(Ordering::SeqCst));
        let waiting=self.descendants(&child.id).iter().any(|c|self.inner.live.get(&c.id).is_some_and(|l|l.running.load(Ordering::SeqCst)));
        let descriptor = &child.descriptor;
        let legacy = descriptor.snapshot_version < denia_core::subagent::SUBAGENT_SNAPSHOT_VERSION;
        // 等待交互：未结算的审批/提问请求数（等待状态必须能被父代理看见，
        // 否则卡片只存在于子会话页面里）。同一遍扫描顺带给出最后终态摘要。
        let (pending_approvals, pending_asks, result) = live
            .as_ref()
            .map(|live| pending_interactions(&live.session))
            .unwrap_or((0, 0, None));
        let projection_applied = live
            .as_ref()
            .is_some_and(|live| live.session.history_projection().is_some());
        let waiting_interaction = pending_approvals > 0 || pending_asks > 0;
        json!({"id":child.id,"parentId":child.parent_id,"label":descriptor.label,"depth":descriptor.depth,"mode":descriptor.mode,"selection":descriptor.selection,
            "status":if running{"running"}else if waiting{"waiting"}else{"settled"},
            "profile":descriptor.profile,
            "name":descriptor.name,
            "description":descriptor.description,
            "toolCount":descriptor.effective_tools.as_ref().map(Vec::len).unwrap_or_else(|| denia_core::subagent::legacy_child_tools(descriptor.allowed_tools.as_deref()).len()),
            "tools":descriptor.effective_tools.clone().unwrap_or_else(|| denia_core::subagent::legacy_child_tools(descriptor.allowed_tools.as_deref())),
            "permissionCeiling":descriptor.permission_ceiling.as_str(),
            "createdPermissionMode":descriptor.created_permission_mode,
            "instructionScope":descriptor.instruction_scope,
            "delegationAllowed":false,
            "pendingApprovals":pending_approvals,
            "pendingAsks":pending_asks,
            "waitingInteraction":waiting_interaction,
            "result":result,
            "migration": if legacy { json!({
                // 旧 child 不再需要重新派遣：首次继续时自动建立模型历史投影
                // （见 HISTORY_PROJECTION_FILE）。这里只报告投影是否已建立。
                "needsRedispatch": false,
                "version": descriptor.snapshot_version,
                "projection": if projection_applied { "applied" } else { "pending" },
                "diagnostic": if projection_applied {
                    "旧子代理已建立模型历史投影：旧父运行态与全部自动注入通道不再进模型历史，审计日志保持不变。"
                } else {
                    "旧子代理尚未建立模型历史投影；首次继续（发消息/唤醒）时自动建立，之后在线与冷恢复一致。"
                }}) } else { Value::Null }})
    }).collect::<Vec<_>>())
    }
    pub(super) fn authorize(
        &self,
        owner: &str,
        target: &str,
        ancestor: bool,
    ) -> Result<(), String> {
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
    pub async fn interrupt(&self, owner: &str, target: &str) -> Result<(), String> {
        let _admission = self.inner.admission.enter().await;
        self.authorize(owner, target, true)?;
        self.inner.paused.lock().unwrap().insert(target.into());
        if self
            .inner
            .live
            .get(target)
            .is_none_or(|l| !l.running.load(Ordering::SeqCst))
        {
            self.inner.admission.release(target);
        }
        if let Some(live) = self.inner.live.get(target)
            && let Some(token) = live.cancel.lock().unwrap().as_ref()
        {
            token.cancel();
        }
        // 停止 child 时一并终止它自己的后台进程树：只释放槽位而让进程继续写
        // 文件是最坏的结果（计划 10.4）。取消是幂等的，正常结束前重复调用无害。
        self.inner.jobs.cancel_owner(target).await;
        // 浏览器资源按 child 归属清理：只关它自己的 tab，不碰父代理与其他
        // 子代理的（计划 6.4 / L04）。
        self.close_owned_tabs(target).await;
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
            self.close_owned_tabs(&child.id).await;
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
        // 会话没了,它名下的挂起唤醒再被消费也只是对着空会话空转。
        self.inner.deferred_wakes.lock().unwrap().remove(owner);
        Ok(())
    }
    pub(super) async fn rollback_child(&self, id: &str) {
        self.inner.admission.release(id);
        self.inner.children.lock().unwrap().remove(id);
        self.inner.selections.lock().unwrap().remove(id);
        self.inner.inboxes.lock().unwrap().remove(id);
        self.inner.delivery.forget(id);
        self.inner.completion_marks.lock().unwrap().remove(id);
        self.inner.live.remove(id);
        self.inner.workspaces.detach_session(id);
        let sessions = self.inner.sessions.clone();
        let id = id.to_string();
        if let Err(e) = tokio::task::spawn_blocking(move || sessions.delete(&id)).await {
            tracing::error!(error=%e,"回滚子代理失败");
        }
    }
}
