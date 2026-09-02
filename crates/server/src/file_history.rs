//! 文件回退历史:写文件前备份原文件,按用户消息固化快照,支持预览与恢复。
//!
//! 布局:`<home>/file-history/<session_id>/snapshots.json` + 若干备份文件。
//! 备份文件名随机生成,快照记录相对工作区的路径 → 版本。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use denia_agent_loop::FileHistoryProvider;
use denia_tools::FileHistoryBackend;
use serde::{Deserialize, Serialize};

/// 一个文件在某一快照时刻的版本;`backup: None` 表示该文件当时不存在。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileVersion {
    pub backup: Option<String>,
    pub version: u32,
}

/// 一个用户消息对应的文件快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSnapshot {
    pub message_seq: u64,
    pub time: u64,
    /// 相对 cwd 路径 -> 版本。
    pub files: HashMap<String, FileVersion>,
}

/// 回退预览的一项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDiffEntry {
    pub path: String,
    /// `restore` = 恢复该文件内容;`delete` = 删除该文件(目标点不存在)。
    pub action: String,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

struct SessionHistoryInner {
    cwd: PathBuf,
    tracked: HashSet<String>,
    /// 当前(最新)已备份版本;快照继承时使用。
    current: HashMap<String, FileVersion>,
    /// 所有已开始固化的快照;最后一条是当前用户消息正在积累的快照。
    snapshots: Vec<FileSnapshot>,
}

pub struct SessionFileHistory {
    root: PathBuf,
    inner: Mutex<SessionHistoryInner>,
}

impl SessionFileHistory {
    fn new(session_id: &str, home: &Path, cwd: &Path) -> Self {
        Self {
            root: home.join("file-history").join(session_id),
            inner: Mutex::new(SessionHistoryInner {
                cwd: cwd.to_path_buf(),
                tracked: HashSet::new(),
                current: HashMap::new(),
                snapshots: Vec::new(),
            }),
        }
    }

    fn relative_key(&self, cwd: &Path, path: &Path) -> String {
        path.strip_prefix(cwd)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/")
    }

    fn persist(&self, inner: &SessionHistoryInner) -> Result<(), String> {
        let bytes = serde_json::to_vec(&inner.snapshots).map_err(|e| e.to_string())?;
        let dir = self.root.clone();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let final_path = dir.join("snapshots.json");
        let tmp = dir.join("snapshots.json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &final_path).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 快照索引文件读取(新会话为空)。
    fn read_snapshots(root: &Path) -> Vec<FileSnapshot> {
        let path = root.join("snapshots.json");
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }
}

#[async_trait]
impl FileHistoryBackend for SessionFileHistory {
    async fn track_before_write(&self, path: &Path) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let key = self.relative_key(&inner.cwd, path);
        let version = capture_current(&self.root, path, inner.current.get(&key).cloned())?;
        inner.tracked.insert(key.clone());
        inner.current.insert(key.clone(), version.clone());

        // 把“写前状态”补进当前用户消息快照;同一条消息内第二次写同一文件
        // 不覆盖第一次(第一次才是该消息发出前的状态)。
        if let Some(snapshot) = inner.snapshots.last_mut() {
            snapshot.files.entry(key.clone()).or_insert(version);
            self.persist(&inner)?;
        }
        Ok(())
    }
}

pub struct FileHistoryStore {
    home: PathBuf,
    sessions: Mutex<HashMap<String, Arc<SessionFileHistory>>>,
}

impl FileHistoryStore {
    pub fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn get_or_create(&self, session_id: &str, cwd: &Path) -> Arc<SessionFileHistory> {
        let mut map = self.sessions.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(session) = map.get(session_id) {
            return session.clone();
        }
        let session = Arc::new(SessionFileHistory::new(session_id, &self.home, cwd));
        // 恢复持久化快照。
        {
            let mut inner = session.inner.lock().unwrap_or_else(|poison| poison.into_inner());
            inner.snapshots = SessionFileHistory::read_snapshots(&session.root);
            let snapshots = inner.snapshots.clone();
            for snapshot in &snapshots {
                for key in snapshot.files.keys() {
                    inner.tracked.insert(key.clone());
                }
                for (key, version) in &snapshot.files {
                    inner.current.insert(key.clone(), version.clone());
                }
            }
        }
        map.insert(session_id.to_string(), session.clone());
        session
    }

    /// 返回工具备份句柄。
    pub fn backend(&self, session_id: &str, cwd: &Path) -> Arc<dyn FileHistoryBackend> {
        self.get_or_create(session_id, cwd)
    }

    /// 用户消息落库后调用:开启一个新快照。
    pub async fn snapshot(
        &self,
        session_id: &str,
        cwd: &Path,
        message_seq: u64,
    ) -> Result<(), String> {
        let session = self.get_or_create(session_id, cwd);
        let mut inner = session.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        // 先把所有已跟踪文件的磁盘最新状态备份进 current,再让新快照继承。
        // 这样“本消息没改过的文件”也保留当时版本,回退不会误删。
        let keys: Vec<String> = inner.current.keys().cloned().collect();
        for key in keys {
            let path = Path::new(&inner.cwd).join(&key);
            let version = capture_current(&session.root, &path, inner.current.get(&key).cloned())?;
            inner.current.insert(key.clone(), version);
        }
        let files = inner.current.clone();
        inner.snapshots.push(FileSnapshot {
            message_seq,
            time: now_millis(),
            files,
        });
        session.persist(&inner)
    }

    /// 预览回退到目标消息时的文件变化。
    pub async fn diff(
        &self,
        session_id: &str,
        cwd: &Path,
        message_seq: u64,
    ) -> Result<Vec<FileDiffEntry>, String> {
        let session = self.get_or_create(session_id, cwd);
        let inner = session.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let target = inner
            .snapshots
            .iter()
            .rev()
            .find(|s| s.message_seq == message_seq)
            .ok_or_else(|| format!("file history snapshot for seq {message_seq} not found"))?;

        let mut entries = Vec::new();
        let mut keys: HashSet<String> = inner.tracked.clone();
        for key in target.files.keys() {
            keys.insert(key.clone());
        }
        for key in keys {
            let path = Path::new(&inner.cwd).join(&key);
            let target_version = target.files.get(&key);
            let exists = path.exists();
            match target_version {
                Some(FileVersion {
                    backup: Some(backup),
                    ..
                }) => {
                    let backup_path = session.root.join(backup);
                    let changed = if exists {
                        !files_equal(&path, &backup_path)
                    } else {
                        true
                    };
                    if changed {
                        entries.push(FileDiffEntry {
                            path: key,
                            action: "restore".to_string(),
                        });
                    }
                }
                Some(FileVersion { backup: None, .. }) => {
                    if exists {
                        entries.push(FileDiffEntry {
                            path: key,
                            action: "delete".to_string(),
                        });
                    }
                }
                None => {
                    // 目标点之后才第一次 tracked:按“当时不存在”处理。
                    if exists {
                        entries.push(FileDiffEntry {
                            path: key,
                            action: "delete".to_string(),
                        });
                    }
                }
            }
        }
        Ok(entries)
    }

    /// 物理回退文件到目标消息快照,返回实际变更的文件列表。
    pub async fn rewind(
        &self,
        session_id: &str,
        cwd: &Path,
        message_seq: u64,
    ) -> Result<Vec<String>, String> {
        let session = self.get_or_create(session_id, cwd);
        let mut inner = session.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let target_index = inner
            .snapshots
            .iter()
            .rposition(|s| s.message_seq == message_seq)
            .ok_or_else(|| format!("file history snapshot for seq {message_seq} not found"))?;
        let target = inner.snapshots[target_index].clone();

        let mut changed = Vec::new();
        let mut keys: HashSet<String> = inner.tracked.clone();
        for key in target.files.keys() {
            keys.insert(key.clone());
        }
        for key in keys {
            let path = Path::new(&inner.cwd).join(&key);
            let target_version = target.files.get(&key);
            match target_version {
                Some(FileVersion {
                    backup: Some(backup),
                    ..
                }) => {
                    let backup_path = session.root.join(backup);
                    if files_equal(&path, &backup_path) {
                        continue;
                    }
                    if let Some(parent) = path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
                    }
                    std::fs::copy(&backup_path, &path).map_err(|e| e.to_string())?;
                    changed.push(key);
                }
                Some(FileVersion { backup: None, .. }) | None => {
                    if path.exists() {
                        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
                        changed.push(key);
                    }
                }
            }
        }

        // 丢弃目标点之后的快照;保留目标点及更早。
        inner.snapshots.truncate(target_index + 1);
        // 当前版本集合同步到目标快照,后续 track 从该状态继续计数。
        inner.current = target.files.clone();
        session.persist(&inner)?;
        Ok(changed)
    }
}

fn files_equal(a: &Path, b: &Path) -> bool {
    match (std::fs::read(a), std::fs::read(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// 把磁盘当前状态归一化为一个版本:未变化则复用现有备份,变化则新建备份,
/// 文件缺失则记录 `None`。
fn capture_current(
    root: &Path,
    path: &Path,
    current: Option<FileVersion>,
) -> Result<FileVersion, String> {
    if path.exists() {
        if let Some(FileVersion {
            backup: Some(backup),
            ..
        }) = &current
        {
            let backup_path = root.join(backup);
            if files_equal(path, &backup_path) {
                return Ok(current.unwrap());
            }
        }
        let next = current.as_ref().map(|v| v.version).unwrap_or(0) + 1;
        let name = format!("{next}-{}", uuid::Uuid::new_v4());
        let backup_path = root.join(&name);
        std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
        std::fs::copy(path, &backup_path).map_err(|e| e.to_string())?;
        Ok(FileVersion {
            backup: Some(name),
            version: next,
        })
    } else {
        match current {
            Some(FileVersion { backup: None, version }) => {
                Ok(FileVersion { backup: None, version })
            }
            Some(FileVersion { version, .. }) => Ok(FileVersion {
                backup: None,
                version: version + 1,
            }),
            None => Ok(FileVersion {
                backup: None,
                version: 1,
            }),
        }
    }
}

#[async_trait]
impl FileHistoryProvider for FileHistoryStore {
    async fn backend(
        &self,
        session_id: &str,
        cwd: &Path,
    ) -> Option<Arc<dyn FileHistoryBackend>> {
        Some(self.backend(session_id, cwd))
    }

    async fn snapshot(
        &self,
        session_id: &str,
        cwd: &Path,
        message_seq: u64,
    ) -> Result<(), String> {
        self.snapshot(session_id, cwd, message_seq).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "denia-file-history-{}",
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn track_snapshot_rewind_restores_versions() {
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let file = cwd.join("a.txt");
        std::fs::write(&file, "v1").unwrap();

        let store = FileHistoryStore::new(&root);
        let session_id = "s1";
        let backend = store.backend(session_id, &cwd);

        // 用户消息 1:开启快照,写文件前备份 v1,然后改成 v2。
        store.snapshot(session_id, &cwd, 1).await.unwrap();
        backend.track_before_write(&file).await.unwrap();
        std::fs::write(&file, "v2").unwrap();

        // 用户消息 2:开启快照,写文件前备份 v2,然后改成 v3。
        store.snapshot(session_id, &cwd, 2).await.unwrap();
        backend.track_before_write(&file).await.unwrap();
        std::fs::write(&file, "v3").unwrap();

        // diff 到消息 1:当前 v3 应变化。
        let diff = store.diff(session_id, &cwd, 1).await.unwrap();
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].path, "a.txt");
        assert_eq!(diff[0].action, "restore");

        // 回退到消息 1:文件恢复为 v1。
        let changed = store.rewind(session_id, &cwd, 1).await.unwrap();
        assert_eq!(changed, vec!["a.txt"]);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");

        // 回退后再次 diff 到消息 2 应已不存在(快照被截断)。
        assert!(store.diff(session_id, &cwd, 2).await.is_err());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn track_missing_file_records_delete() {
        let root = temp_root();
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let file = cwd.join("new.txt");

        let store = FileHistoryStore::new(&root);
        let session_id = "s2";
        let backend = store.backend(session_id, &cwd);

        store.snapshot(session_id, &cwd, 1).await.unwrap();
        // 写一个不存在的新文件:track 应记录“当时不存在”。
        backend.track_before_write(&file).await.unwrap();
        std::fs::write(&file, "hello").unwrap();

        let diff = store.diff(session_id, &cwd, 1).await.unwrap();
        assert_eq!(diff.len(), 1);
        assert_eq!(diff[0].action, "delete");

        let changed = store.rewind(session_id, &cwd, 1).await.unwrap();
        assert_eq!(changed, vec!["new.txt"]);
        assert!(!file.exists());

        std::fs::remove_dir_all(&root).unwrap();
    }
}