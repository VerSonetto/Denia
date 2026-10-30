//! `session_trace` 工具的服务端实现：权限裁决 + 冷/热投影 + 辅助记录合并。

use super::*;
use crate::trajectory::{
    TRACE_LIMIT_MAX, TraceFilter, TraceOutput, project_cold, project_hot, related,
};
use denia_tools::runtime_command::SessionTraceArgs;

impl Runtime {
    /// 查询当前会话或直接亲属会话的结构化轨迹。
    pub(super) async fn session_trace(
        &self,
        args: SessionTraceArgs,
        ctx: &ToolContext,
    ) -> Result<Value, String> {
        let current = ctx.session_id.as_deref().ok_or("缺少会话身份")?;
        let target = match args.session.as_deref().map(str::trim) {
            None | Some("") | Some("current") => current.to_string(),
            Some(id) => id.to_string(),
        };
        // 关系表来自运行时 children（启动时从会话头重建，覆盖所有子代理）。
        let parent_of: HashMap<String, String> = self
            .inner
            .children
            .lock()
            .unwrap()
            .iter()
            .map(|(id, child)| (id.clone(), child.parent_id.clone()))
            .collect();
        if !related(current, &target, &parent_of) {
            return Err(format!(
                "trace/forbidden:只能查询当前会话、直接父代理或直接子代理；{target} 不在这个范围内"
            ));
        }
        let filter = TraceFilter::from_args(&args);
        let main = match self.inner.live.get(&target) {
            Some(live) => project_hot(&live.session, &filter),
            None => {
                let store = self.inner.sessions.clone();
                let id = target.clone();
                let filter = filter.clone();
                tokio::task::spawn_blocking(move || project_cold(&store, &id, &filter))
                    .await
                    .map_err(|error| format!("trace/join:读取会话日志失败：{error}"))??
            }
        };

        // 辅助记录（队列提交、子代理定义等）与会话日志合并；日志是主事实，
        // 辅助记录只补它没有的动作。合并后按时间排序，再套用同一份 limit。
        let mut aux: Vec<crate::trajectory::TraceRecord> = self
            .inner
            .trace
            .read(&target, TRACE_LIMIT_MAX)
            .into_iter()
            // 辅助记录由多个入口写入，文本可能含密钥；读取时统一脱敏，
            // 兼容修复前已落盘的旧记录。
            .map(|record| crate::trajectory::sanitize_record(record, filter.text_limit))
            .filter(|record| filter.accepts(record))
            .collect();
        let aux_matched = aux.len();
        let mut output: TraceOutput = main;
        output.records.append(&mut aux);
        output.records.sort_by_key(|record| record.time);
        if output.records.len() > filter.limit {
            let overflow = output.records.len() - filter.limit;
            output.records.drain(0..overflow);
            output.truncated = true;
        }
        output.matched += aux_matched;
        output.note = format!(
            "记录来自会话日志的结构化投影，并合并本会话的队列/子代理辅助记录；\
             文本已脱敏并按上限截断。locator: session={} + seq（可带 turn/step/callId）。",
            output.session
        );
        serde_json::to_value(output).map_err(|error| format!("trace/serialize:{error}"))
    }
}
