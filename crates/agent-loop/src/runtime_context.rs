//! Durable projection state for dynamic runtime context (DSH-compatible).

use std::sync::{Arc, Weak};

use denia_core::message::ChatRole;
use denia_core::session::SurfaceMessage;
use denia_session::Session;
use denia_system_prompt::{
    PromptAssembly, RUNTIME_CONTEXT_CLEARED, is_runtime_context_snapshot, render_context_snapshot,
};

/// 运行时上下文快照的注入通道名。
///
/// 与 `turn.rs` 落盘时的字面量、`denia_core::session::BASELINE_INJECTION_CHANNELS`
/// 里登记的那一项必须是**同一个字符串**:恢复判据(本模块)与模型面折叠(core)
/// 一旦各认一套身份,就会出现"一边认、一边不认"——旧快照既不折叠也不补发,
/// 或者在模型面上叠出两份互相矛盾的基线。
pub const RUNTIME_CONTEXT_CHANNEL: &str = "runtime-context";

/// Tracks the last retained runtime-context snapshot without owning session commits.
#[derive(Debug, Default)]
pub struct RuntimeContextProjection {
    retained: Option<String>,
    /// 会话的弱引用:只为在轮内压缩后把基准重新对齐到当前模型面,不延长
    /// 会话寿命,也不参与任何提交(日志仍是唯一耐久真值源)。
    session: Weak<Session>,
    /// 基准对齐到 surface 时日志的末尾 seq(与注入基线 `InjectionBaselines`
    /// 同一机制:只扫这个点之后新落的事件就能发现需要重新对齐)。
    synced_seq: u64,
}

impl RuntimeContextProjection {
    /// Restore from the derived model surface, then follow new injected snapshots.
    ///
    /// 恢复判据读**派生 surface**,不是原始日志:压缩(`CompactionSummary`)只把
    /// 被压区间移出模型面、日志保持 append-only,而快照通常正落在最老的那一段。
    /// 读日志会得出"已经注入过"的错误结论——压缩之后只要模型名/cwd/平台/权限档
    /// 这些渲染输入不变,`project` 便认为"没变"而永不补发,模型此后再也看不到
    /// 运行时事实。读 surface 后,快照被压掉 = 本通道在模型面上没有文本 =
    /// 下一次恢复自然重发。
    pub fn restore(session: &Arc<Session>) -> Self {
        Self {
            retained: retained_snapshot(&session.derive_surface()),
            session: Arc::downgrade(session),
            synced_seq: session.with_events(crate::injections::last_seq),
        }
    }

    /// Returns snapshot text when the rendered runtime context changed.
    pub fn project(&mut self, assembly: &PromptAssembly) -> Option<String> {
        // 轮内压缩把快照挤出模型面时,基准立刻作废(与注入基线
        // `realign_if_compacted` 同一套对齐机制):渲染输入一字未变也必须重发,
        // 否则本轮剩下的 step 都拿一个模型已经看不到的文本判"没变"。
        self.realign_if_compacted();

        let current = render_context_snapshot(assembly);
        if self.retained.is_none() && current.is_empty() {
            return None;
        }
        let snapshot = if current.is_empty() {
            RUNTIME_CONTEXT_CLEARED.to_string()
        } else {
            current
        };
        if self.retained.as_deref() == Some(snapshot.as_str()) {
            return None;
        }
        self.retained = Some(snapshot.clone());
        Some(snapshot)
    }

    /// 上一步之后新落了压缩事件时,把基准重新对齐到当前模型面。
    ///
    /// 检测与注入基线共用 [`crate::injections::compaction_since`]:只扫对齐点
    /// 之后新落的事件,未压缩会话的每 step 代价是 O(新增),不重跑全量派生。
    fn realign_if_compacted(&mut self) {
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let (compacted, last) = crate::injections::compaction_since(&session, self.synced_seq);
        self.synced_seq = last;
        if compacted {
            self.retained = retained_snapshot(&session.derive_surface());
        }
    }
}

/// 模型面上最后一条 runtime-context 文本(幂等基准)。
///
/// 与注入基线(`InjectionBaselines`)同一来源、`injections::injected_text` 同一
/// 做法:读**派生 surface**,逆序扫描取最后一条本通道文本。身份口径两条:
/// `channel` 字段(`"runtime-context"`,与 core 的折叠名单同源)优先;旧日志
/// (没有该字段)按 `is_runtime_context_snapshot` 的正文口径兜底——快照头部
/// 前缀或 `RUNTIME_CONTEXT_CLEARED` 终局标记。字段存在时以字段为准,不做
/// 无边界的文本猜测;判据只有这一处,恢复与折叠不会各认一套身份。
///
/// 旧日志的正文兜底不查 `injected` 位(surface 不携带它):用户恰好粘贴同前缀
/// 正文时基准会等于那段正文,而那段正文本来就在模型面上,重发与否都无害。
fn retained_snapshot(surface: &[SurfaceMessage]) -> Option<String> {
    surface.iter().rev().find_map(|item| {
        if item.message.role != ChatRole::User {
            return None;
        }
        let text = &item.message.content;
        let matched = match item.channel.as_deref() {
            Some(channel) => channel == RUNTIME_CONTEXT_CHANNEL,
            None => is_runtime_context_snapshot(text),
        };
        matched.then(|| text.clone())
    })
}

#[cfg(test)]
mod tests {
    use denia_core::session::SessionEvent;
    use denia_session::Session;
    use denia_system_prompt::{AssembledContext, PromptAssembly, RUNTIME_CONTEXT_HEADER};

    use super::*;

    fn temp_session() -> Arc<Session> {
        let dir = std::env::temp_dir().join(format!(
            "denia-runtime-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        Arc::new(Session::create(&dir, uuid::Uuid::new_v4().to_string(), &dir, true, None).unwrap())
    }

    /// 一份渲染输入:快照正文恒为 `Mode: read-only.`。
    fn assembly() -> PromptAssembly {
        PromptAssembly {
            sections: Vec::new(),
            contexts: vec![AssembledContext {
                name: "policy".to_string(),
                text: "Mode: read-only.".to_string(),
            }],
            tools: Vec::new(),
            variables: Default::default(),
        }
    }

    /// 记录一条快照注入(与 `turn.rs` 落盘同形)。
    fn inject_snapshot(session: &Arc<Session>, snapshot: &str) -> u64 {
        session
            .append(SessionEvent::UserMessage {
                text: snapshot.to_string(),
                injected: true,
                images: Vec::new(),
                channel: Some(RUNTIME_CONTEXT_CHANNEL.to_string()),
            })
            .unwrap()
            .seq
    }

    /// 真实压缩的最小等价构造:整段历史(快照在被压区间里)折叠成摘要。
    fn compact_everything(session: &Arc<Session>) -> u64 {
        let last_seq =
            session.with_events(|events| events.last().map(|item| item.seq).unwrap_or(0));
        session
            .append(SessionEvent::CompactionSummary {
                turn: 1,
                step: 1,
                summary: "既往工作已总结。".to_string(),
                replaces_from: 1,
                replaces_to: last_seq,
                keep_from: last_seq + 1,
                pre_tokens: 0,
                post_tokens: 0,
            })
            .unwrap()
            .seq
    }

    #[test]
    fn projects_changed_runtime_context_only_once() {
        let mut projection = RuntimeContextProjection::default();
        let assembly = assembly();
        let first = projection.project(&assembly).unwrap();
        assert!(first.starts_with(RUNTIME_CONTEXT_HEADER));
        assert!(projection.project(&assembly).is_none());
    }

    /// 旧日志(无 `channel` 字段)按正文口径认:快照头部前缀与 cleared 终局
    /// 标记两种形态都要认出来,不能只认新写法。
    #[test]
    fn restore_recognizes_both_snapshot_and_cleared_text() {
        let session = temp_session();
        let snapshot = format!("{RUNTIME_CONTEXT_HEADER}\n\nMode: on.");
        session
            .append(SessionEvent::UserMessage {
                text: snapshot.clone(),
                injected: true,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        let projection = RuntimeContextProjection::restore(&session);
        assert_eq!(projection.retained.as_deref(), Some(snapshot.as_str()));

        session
            .append(SessionEvent::UserMessage {
                text: RUNTIME_CONTEXT_CLEARED.to_string(),
                injected: true,
                images: Vec::new(),
                channel: None,
            })
            .unwrap();
        let projection = RuntimeContextProjection::restore(&session);
        assert_eq!(
            projection.retained.as_deref(),
            Some(RUNTIME_CONTEXT_CLEARED)
        );
    }

    /// 别的通道顶着快照前缀也不能冒充 runtime-context(字段优先)。
    #[test]
    fn restore_ignores_other_channels_carrying_the_header() {
        let session = temp_session();
        let snapshot = format!("{RUNTIME_CONTEXT_HEADER}\n\nMode: on.");
        session
            .append(SessionEvent::UserMessage {
                text: snapshot.clone(),
                injected: true,
                images: Vec::new(),
                channel: Some("feedback".to_string()),
            })
            .unwrap();
        let projection = RuntimeContextProjection::restore(&session);
        assert_eq!(projection.retained, None);
    }

    /// 回归:压缩把快照挤出模型面后,渲染输入一字未变也必须补发完整快照。
    ///
    /// 判据读**日志**时,压缩后恢复仍能找到那条注入 → `project` 认为"没变"
    /// → 模型再也看不到运行时快照(工作目录、平台、权限档)。
    #[test]
    fn compaction_evicting_the_snapshot_forces_a_resend_across_turns() {
        let session = temp_session();
        let assembly = assembly();
        let snapshot = format!("{RUNTIME_CONTEXT_HEADER}\n\nMode: read-only.");
        inject_snapshot(&session, &snapshot);

        // 幂等:渲染输入未变 → 不重发。
        let mut projection = RuntimeContextProjection::restore(&session);
        assert!(
            projection.project(&assembly).is_none(),
            "渲染输入未变时不得重复注入(幂等)"
        );

        let compacted_at = compact_everything(&session);
        assert!(
            session
                .derive_surface()
                .iter()
                .all(|item| item.channel.as_deref() != Some(RUNTIME_CONTEXT_CHANNEL)),
            "前置条件:压缩后快照必须已离开模型面"
        );

        // 下一轮:渲染输入一字未变(同一个 assembly),判据必须从模型面
        // 发现"已经没了"并补发全文。
        let mut projection = RuntimeContextProjection::restore(&session);
        let resent = projection
            .project(&assembly)
            .expect("压缩把快照挤出模型面后必须重发");
        assert_eq!(resent, snapshot, "重发内容必须与首次逐字节一致");
        assert!(compacted_at > 0);
    }

    /// 轮内压缩(同一轮剩下的 step 用的还是轮次开始那一份 projection)同样
    /// 必须补发:对齐检测与注入基线共用同一份 `compaction_since`。
    #[test]
    fn in_turn_compaction_also_forces_a_resend() {
        let session = temp_session();
        let assembly = assembly();
        let snapshot = format!("{RUNTIME_CONTEXT_HEADER}\n\nMode: read-only.");
        inject_snapshot(&session, &snapshot);
        let mut projection = RuntimeContextProjection::restore(&session);
        assert!(projection.project(&assembly).is_none());

        // 同一轮内压缩:不重新 restore,直接进下一步。
        compact_everything(&session);
        assert_eq!(
            projection.project(&assembly).as_deref(),
            Some(snapshot.as_str()),
            "轮内压缩后,同一份基准也必须发现模型面已失去快照"
        );
    }
}
