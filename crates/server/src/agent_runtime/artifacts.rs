use super::*;

/// 一条产物引用(`ToolResult.meta.outputArtifact`)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRef {
    /// 产物 id(`outputArtifact.output_id`);`read_tool_output` 按它读取。
    pub output_id: String,
    /// 产生它的工具调用 id 与工具名(`-` = 日志里找不到对应的调用)。
    pub call_id: String,
    pub tool: String,
    /// 输出是否完整保留;`None` = 工具没报这个事实。
    pub complete: Option<bool>,
}

/// 一条工具调用的 `path` 参数(相对或绝对都可以)。
pub(super) fn tool_path_argument(arguments: &str) -> Option<PathBuf> {
    let value: serde_json::Value = serde_json::from_str(arguments.trim()).ok()?;
    let raw = value.get("path")?.as_str()?.trim();
    (!raw.is_empty()).then(|| PathBuf::from(raw))
}

/// 展示口径:工作区内记相对路径(分隔符统一为 `/`),工作区外记绝对路径 ——
/// 与 `run_checks` 冻结输入指纹时用的是同一套口径。
pub(super) fn display_path(cwd: &Path, path: &Path) -> String {
    match path.strip_prefix(cwd) {
        Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

/// 去重插入;返回值表示是否真的插入了(口径统计用)。
pub(super) fn push_unique(items: &mut Vec<String>, item: String) -> bool {
    if items.iter().any(|existing| existing == &item) {
        return false;
    }
    items.push(item);
    true
}

/// 归集子会话产生的产物引用(`ToolResult.meta.outputArtifact`)。
pub(super) fn artifacts(events: &[SessionEnvelope]) -> Vec<ArtifactRef> {
    let mut names: HashMap<&str, &str> = HashMap::new();
    for envelope in events {
        if let SessionEvent::ToolCall { call_id, name, .. } = &envelope.event {
            names.insert(call_id.as_str(), name.as_str());
        }
    }
    let mut out: Vec<ArtifactRef> = Vec::new();
    for envelope in events {
        let SessionEvent::ToolResult {
            call_id,
            meta: Some(meta),
            ..
        } = &envelope.event
        else {
            continue;
        };
        let Some(artifact) = meta.get("outputArtifact") else {
            continue;
        };
        let Some(output_id) = artifact.get("output_id").and_then(Value::as_str) else {
            continue;
        };
        if out.iter().any(|item| item.output_id == output_id) {
            continue;
        }
        out.push(ArtifactRef {
            output_id: output_id.to_string(),
            call_id: call_id.clone(),
            tool: names
                .get(call_id.as_str())
                .copied()
                .unwrap_or("-")
                .to_string(),
            complete: artifact.get("complete").and_then(Value::as_bool),
        });
    }
    out
}
