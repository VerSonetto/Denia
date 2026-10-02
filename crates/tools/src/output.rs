use std::collections::BTreeMap;
use std::hash::Hasher;
use std::path::PathBuf;
use std::sync::Arc;
use async_trait::async_trait;
use denia_core::tool::ToolSchema;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;
use crate::{Tool, ToolContext, ToolOutput};

#[cfg(test)]
mod tests {
    use super::*;

    async fn store(call: u64, session: u64) -> (PathBuf, Arc<OutputStore>) {
        let root = std::env::temp_dir().join(format!("denia-output-{}", uuid::Uuid::new_v4()));
        let store = OutputStore::with_limits(root.clone(), call, session).await.unwrap();
        (root, store)
    }

    #[test]
    fn preview_is_bounded_and_preserves_unicode_tail() {
        let mut preview = Preview::default();
        let text = "测试🦀\n".repeat(100_000) + "尾部错误";
        for bytes in text.as_bytes().chunks(8192) { preview.push(bytes); }
        let rendered = preview.render(4000, 8000);
        assert!(rendered.chars().count() <= 12000);
        assert!(rendered.ends_with("尾部错误"));
        assert!(!rendered.contains(char::REPLACEMENT_CHARACTER));
        assert!(preview.head.len() <= HEAD_BYTES && preview.tail.len() <= TAIL_BYTES);
    }

    #[tokio::test]
    async fn paging_restart_and_session_isolation() {
        let (root, storage) = store(100_000, 200_000).await;
        let capture = storage.start(&["stdout", "stderr"]).await;
        capture.append("stdout", "第一行\n第二行🦀\n第三行\n".as_bytes()).await;
        capture.append("stderr", "尾部错误\n".as_bytes()).await;
        let artifact = capture.finish(true).await;
        assert!(artifact.complete);
        let restored = OutputStore::open(root.clone()).await.unwrap();
        let page = restored.read(&artifact.output_id, Some("stdout"), 2, 1).await.unwrap();
        assert!(page.contains("第二行🦀") && !page.contains("第三行"));
        assert!(restored.read(&artifact.output_id, Some("stderr"), 1, 400).await.unwrap().contains("尾部错误"));
        let (other_root, other) = store(100_000, 200_000).await;
        assert!(other.read(&artifact.output_id, None, 1, 400).await.is_err());
        assert!(storage.read("../escape", None, 1, 400).await.is_err());
        assert!(storage.read(&artifact.output_id, Some("../stderr"), 1, 400).await.is_err());
        tokio::fs::remove_dir_all(root).await.unwrap();
        tokio::fs::remove_dir_all(other_root).await.unwrap();
    }

    #[tokio::test]
    async fn parallel_quota_and_failed_storage_remain_bounded() {
        let (root, storage) = store(50, 80).await;
        let first = storage.start(&["text"]).await;
        let second = storage.start(&["text"]).await;
        tokio::join!(first.append("text", &[b'a'; 100]), second.append("text", &[b'b'; 100]));
        let first = first.finish(true).await;
        let second = second.finish(true).await;
        assert!(!first.complete && !second.complete);
        assert_eq!(first.streams["text"].stored_bytes + second.streams["text"].stored_bytes, 80);
        let restored = OutputStore::with_limits(root.clone(), 50, 80).await.unwrap();
        let third = restored.start(&["text"]).await;
        third.append("text", b"overflow").await;
        assert_eq!(third.finish(true).await.streams["text"].stored_bytes, 0);
        tokio::fs::remove_dir_all(&root).await.unwrap();
        let failed = storage.start(&["text"]).await;
        failed.append("text", b"still consumed").await;
        let failed = failed.finish(true).await;
        assert!(!failed.complete && failed.storage_error.is_some());
        assert_eq!(failed.streams["text"].bytes, 14);
    }

    #[tokio::test]
    async fn repeated_open_shares_live_quota_and_read_tool_enforces_paging() {
        let root = std::env::temp_dir().join(format!("denia-output-{}", uuid::Uuid::new_v4()));
        let first = OutputStore::open(root.clone()).await.unwrap();
        let second = OutputStore::open(root.clone()).await.unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        let capture = first.start(&["text"]).await;
        capture.append("text", "one\ntwo\nthree\n".as_bytes()).await;
        let artifact = capture.finish(true).await;
        let page = second.read(&artifact.output_id, None, 2, 1).await.unwrap();
        assert!(page.ends_with("two\n"));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}

pub const CALL_OUTPUT_LIMIT: u64 = 64 * 1024 * 1024;
pub const SESSION_OUTPUT_LIMIT: u64 = 256 * 1024 * 1024;
const HEAD_BYTES: usize = 16_000;
const TAIL_BYTES: usize = 32_000;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StreamInfo {
    pub bytes: u64,
    pub stored_bytes: u64,
    pub complete: bool,
    pub digest: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutputArtifact {
    pub output_id: String,
    pub streams: BTreeMap<String, StreamInfo>,
    pub complete: bool,
    pub storage_error: Option<String>,
}

pub struct OutputStore {
    root: PathBuf,
    used: Mutex<u64>,
    call_limit: u64,
    session_limit: u64,
}

impl OutputStore {
    pub async fn open(root: PathBuf) -> std::io::Result<Arc<Self>> {
        // Background producers and a new turn must share the same quota lock.
        static STORES: std::sync::OnceLock<std::sync::Mutex<BTreeMap<PathBuf, std::sync::Weak<OutputStore>>>> = std::sync::OnceLock::new();
        tokio::fs::create_dir_all(&root).await?;
        if tokio::fs::symlink_metadata(&root).await?.file_type().is_symlink() {
            return Err(std::io::Error::other("output directory cannot be a symlink"));
        }
        let key = tokio::fs::canonicalize(&root).await?;
        let stores = STORES.get_or_init(Default::default);
        if let Some(store) = stores.lock().unwrap().get(&key).and_then(std::sync::Weak::upgrade) { return Ok(store); }
        let candidate = Self::with_limits(root, CALL_OUTPUT_LIMIT, SESSION_OUTPUT_LIMIT).await?;
        let mut stores = stores.lock().unwrap();
        if let Some(store) = stores.get(&key).and_then(std::sync::Weak::upgrade) { return Ok(store); }
        stores.retain(|_, store| store.strong_count() > 0);
        stores.insert(key, Arc::downgrade(&candidate));
        Ok(candidate)
    }

    pub async fn with_limits(root: PathBuf, call_limit: u64, session_limit: u64) -> std::io::Result<Arc<Self>> {
        tokio::fs::create_dir_all(&root).await?;
        if tokio::fs::symlink_metadata(&root).await?.file_type().is_symlink() {
            return Err(std::io::Error::other("output directory cannot be a symlink"));
        }
        let root = tokio::fs::canonicalize(root).await?;
        let mut entries = tokio::fs::read_dir(&root).await?;
        let mut used = 0u64;
        while let Some(entry) = entries.next_entry().await? {
            if entry.path().extension().is_some_and(|extension| extension != "json") {
                used = used.saturating_add(entry.metadata().await?.len());
            }
        }
        Ok(Arc::new(Self { root, used: Mutex::new(used), call_limit, session_limit }))
    }

    pub async fn start(self: &Arc<Self>, streams: &[&str]) -> Capture {
        let output_id = uuid::Uuid::new_v4().to_string();
        let mut files = BTreeMap::new();
        let mut info = BTreeMap::new();
        let mut storage_error = None;
        for stream in streams {
            match tokio::fs::OpenOptions::new().write(true).create_new(true)
                .open(self.root.join(format!("{output_id}.{stream}"))).await {
                Ok(file) => { files.insert(stream.to_string(), file); }
                Err(error) => { storage_error = Some(error.to_string()); }
            }
            info.insert(stream.to_string(), StreamInfo { complete: true, ..Default::default() });
        }
        Capture {
            store: self.clone(),
            state: Arc::new(Mutex::new(CaptureState {
                artifact: OutputArtifact { output_id, streams: info, complete: storage_error.is_none(), storage_error },
                files, hashes: BTreeMap::new(), stored: 0,
            })),
        }
    }

    async fn checked_path(&self, output_id: &str, stream: &str) -> std::io::Result<PathBuf> {
        if uuid::Uuid::parse_str(output_id).is_err() || !matches!(stream, "json" | "text" | "stdout" | "stderr") {
            return Err(std::io::Error::other("invalid output id or stream"));
        }
        let path = self.root.join(format!("{output_id}.{stream}"));
        if !tokio::fs::symlink_metadata(&path).await?.file_type().is_file() {
            return Err(std::io::Error::other("output is not a regular file"));
        }
        let canonical = tokio::fs::canonicalize(&path).await?;
        if canonical.parent() != Some(self.root.as_path()) {
            return Err(std::io::Error::other("output escaped this session"));
        }
        Ok(canonical)
    }

    pub async fn read(&self, output_id: &str, stream: Option<&str>, offset: u64, limit: u64) -> std::io::Result<String> {
        let manifest_path = self.checked_path(output_id, "json").await?;
        let mut manifest = Vec::new();
        tokio::fs::File::open(manifest_path).await?.take(64_000).read_to_end(&mut manifest).await?;
        let artifact: OutputArtifact = serde_json::from_slice(&manifest).map_err(std::io::Error::other)?;
        let stream = stream.unwrap_or(if artifact.streams.contains_key("text") { "text" } else { "stdout" });
        if !artifact.streams.contains_key(stream) { return Err(std::io::Error::other("stream not found")); }
        let mut file = tokio::fs::File::open(self.checked_path(output_id, stream).await?).await?;
        let mut preview = Preview::default();
        let mut buffer = [0u8; 8192];
        let mut line = 1u64;
        let last = offset.saturating_add(limit);
        loop {
            let count = file.read(&mut buffer).await?;
            if count == 0 || line >= last { break; }
            let mut selected = Vec::with_capacity(count);
            for byte in &buffer[..count] {
                if line >= offset && line < last { selected.push(*byte); }
                if *byte == b'\n' { line = line.saturating_add(1); }
            }
            preview.push(&selected);
        }
        Ok(format!("产物 {output_id} / {stream}，行 {offset} 起，最多 {limit} 行；保存完整: {}\n{}", artifact.complete, preview.render(4_000, 8_000)))
    }
}

struct CaptureState {
    artifact: OutputArtifact,
    files: BTreeMap<String, tokio::fs::File>,
    hashes: BTreeMap<String, std::collections::hash_map::DefaultHasher>,
    stored: u64,
}

#[derive(Clone)]
pub struct Capture {
    store: Arc<OutputStore>,
    state: Arc<Mutex<CaptureState>>,
}

impl Capture {
    pub async fn append(&self, stream: &str, bytes: &[u8]) {
        let mut state = self.state.lock().await;
        if !state.artifact.streams.contains_key(stream) { return; }
        state.artifact.streams.get_mut(stream).unwrap().bytes += bytes.len() as u64;
        state.hashes.entry(stream.to_string()).or_default().write(bytes);
        let mut used = self.store.used.lock().await;
        let available = self.store.call_limit.saturating_sub(state.stored)
            .min(self.store.session_limit.saturating_sub(*used));
        let count = (bytes.len() as u64).min(available) as usize;
        *used += count as u64;
        state.stored += count as u64;
        let result = match state.files.get_mut(stream) {
            Some(file) => file.write_all(&bytes[..count]).await,
            None => Err(std::io::Error::other("output file unavailable")),
        };
        if result.is_ok() {
            state.artifact.streams.get_mut(stream).unwrap().stored_bytes += count as u64;
        }
        if count < bytes.len() || result.is_err() {
            state.artifact.complete = false;
            state.artifact.streams.get_mut(stream).unwrap().complete = false;
            if let Err(error) = result { state.artifact.storage_error = Some(error.to_string()); }
        }
    }

    pub async fn finish(&self, drained: bool) -> OutputArtifact {
        let mut state = self.state.lock().await;
        if !drained {
            state.artifact.complete = false;
            for info in state.artifact.streams.values_mut() { info.complete = false; }
        }
        let mut flush_error = None;
        for file in state.files.values_mut() {
            if let Err(error) = file.flush().await { flush_error = Some(error.to_string()); }
        }
        state.files.clear();
        if let Some(error) = flush_error {
            state.artifact.storage_error = Some(error);
            state.artifact.complete = false;
        }
        let hashes: Vec<_> = state.hashes.iter().map(|(stream, hash)| (stream.clone(), format!("{:016x}", hash.finish()))).collect();
        for (stream, digest) in hashes {
            state.artifact.streams.get_mut(&stream).unwrap().digest = digest;
        }
        let path = self.store.root.join(format!("{}.json", state.artifact.output_id));
        let serialized = serde_json::to_vec(&state.artifact).unwrap();
        let result = async {
            let mut file = tokio::fs::OpenOptions::new().write(true).create_new(true).open(path).await?;
            file.write_all(&serialized).await?;
            file.flush().await
        }.await;
        if let Err(error) = result {
            state.artifact.complete = false;
            state.artifact.storage_error = Some(error.to_string());
        }
        state.artifact.clone()
    }
}

#[derive(Default)]
pub struct Preview {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    bytes: u64,
}

impl Preview {
    pub fn push(&mut self, bytes: &[u8]) {
        self.bytes += bytes.len() as u64;
        let take = bytes.len().min(HEAD_BYTES.saturating_sub(self.head.len()));
        self.head.extend_from_slice(&bytes[..take]);
        let suffix = &bytes[bytes.len().saturating_sub(TAIL_BYTES)..];
        while self.tail.len() + suffix.len() > TAIL_BYTES { self.tail.pop_front(); }
        self.tail.extend(suffix);
    }

    pub fn render(&self, head_chars: usize, tail_chars: usize) -> String {
        if head_chars + tail_chars == 0 { return String::new(); }
        let mut head_end = self.head.len();
        if let Err(error) = std::str::from_utf8(&self.head) {
            if error.error_len().is_none() { head_end = error.valid_up_to(); }
        }
        let head = String::from_utf8_lossy(&self.head[..head_end]);
        if self.bytes <= HEAD_BYTES as u64 && head.chars().count() <= head_chars + tail_chars {
            return head.into_owned();
        }
        let head: String = head.chars().take(head_chars).collect();
        let tail_bytes: Vec<_> = self.tail.iter().copied().collect();
        let tail_start = tail_bytes.iter().position(|byte| byte & 0xc0 != 0x80).unwrap_or(tail_bytes.len());
        let tail = String::from_utf8_lossy(&tail_bytes[tail_start..]);
        let notice: String = format!("\n…[输出预览，原始 {} 字节；完整内容请用 read_tool_output]…\n", self.bytes).chars().take(tail_chars).collect();
        let tail: String = tail.chars().rev().take(tail_chars.saturating_sub(notice.chars().count())).collect::<Vec<_>>().into_iter().rev().collect();
        format!("{head}{notice}{tail}")
    }
}

pub fn artifact_notice(artifact: Option<&OutputArtifact>) -> String {
    match artifact {
        Some(artifact) => format!("\n[输出产物: {}；保存完整: {}；用 read_tool_output 读取。{}]", artifact.output_id, artifact.complete,
            artifact.storage_error.as_deref().unwrap_or("")),
        None => "\n[未保存完整输出：当前调用无可用会话输出存储]".to_string(),
    }
}

pub struct ReadToolOutput { schema: ToolSchema }

impl Default for ReadToolOutput {
    fn default() -> Self { Self { schema: Self::make_schema() } }
}

impl ReadToolOutput {
    fn make_schema() -> ToolSchema {
        ToolSchema { name: "read_tool_output".into(), description: "分页读取当前会话工具产物；仅接受 output_id，不接受路径或其他会话 ID。".into(), parameters: serde_json::json!({
            "type":"object", "properties": {
                "output_id":{"type":"string"}, "stream":{"type":"string","enum":["text","stdout","stderr"]},
                "offset":{"type":"integer","minimum":1}, "limit":{"type":"integer","minimum":1,"maximum":1000}
            }, "required":["output_id"] }) }
    }
}

#[async_trait]
impl Tool for ReadToolOutput {
    fn schema(&self) -> &ToolSchema { &self.schema }

    async fn execute(&self, arguments: &str, context: &ToolContext) -> ToolOutput {
        #[derive(Deserialize)]
        struct Args { output_id: String, stream: Option<String>, offset: Option<u64>, limit: Option<u64> }
        let args = match crate::support::parse_tool_args::<Args>(arguments) { Ok(args) => args, Err(error) => return ToolOutput::error(error) };
        let (offset, limit) = (args.offset.unwrap_or(1), args.limit.unwrap_or(400));
        if offset == 0 || limit == 0 || limit > 1000 { return ToolOutput::error("offset 必须 >= 1，limit 必须在 1..=1000"); }
        let Some(store) = &context.output_store else { return ToolOutput::error("当前会话没有输出产物存储"); };
        match store.read(&args.output_id, args.stream.as_deref(), offset, limit).await {
            Ok(content) => ToolOutput::text(content), Err(error) => ToolOutput::error(format!("无法读取本会话输出: {error}")),
        }
    }
}
