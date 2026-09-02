//! 工作区注册表:独立持久化域(抄 dsh packages/workspace 的思路)。
//!
//! 工作区是注册表记录,不是从会话派生;成员显示 = 会话账本 ∩
//! (会话头 cwd == 工作区路径),目录被删/会话损坏时成员静默脱落。
//! 首启 bootstrap 按会话头 cwd 自动分组(老用户升级即分组)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

/// 工作区记录。id 是 uuid,永远不用路径当 id(路径会被规范化重写)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceRecord {
    pub id: String,
    /// 创建时规范化,之后永不改写,哪怕目录被删。
    pub path: String,
    /// 默认 basename,允许重名。
    pub title: String,
    pub created_at: u64,
    /// 手动顺序的候选账本;读取时按会话头 cwd 过滤。
    #[serde(default)]
    pub session_ids: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryDoc {
    #[serde(default)]
    initialized: bool,
    /// 权威显示顺序,新建 prepend。
    #[serde(default)]
    order: Vec<String>,
    #[serde(default)]
    workspaces: HashMap<String, WorkspaceRecord>,
}

pub struct WorkspaceRegistry {
    file: PathBuf,
    state: RwLock<RegistryDoc>,
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 规范化路径:realpath 解析符号链接/..;Windows 去掉 `\\?\` 前缀。
pub fn normalize_path(path: &Path) -> PathBuf {
    match std::fs::canonicalize(path) {
        Ok(real) => {
            let text = real.to_string_lossy().to_string();
            PathBuf::from(text.strip_prefix("\\\\?\\").unwrap_or(&text))
        }
        Err(_) => path.to_path_buf(),
    }
}

impl WorkspaceRegistry {
    pub fn open(home: &Path) -> Result<Self, std::io::Error> {
        let file = home.join("workspaces.json");
        let doc = if file.exists() {
            let raw = std::fs::read_to_string(&file)?;
            serde_json::from_str(&raw).unwrap_or_default()
        } else {
            RegistryDoc::default()
        };
        Ok(Self {
            file,
            state: RwLock::new(doc),
        })
    }

    /// 一次性历史引导:首启按会话头 cwd 分组自动建工作区,
    /// 组顺序按组内最新会话时间降序。
    pub fn bootstrap(&self, sessions: &[(String, String, u64)]) {
        let mut state = self.state.write().unwrap();
        if state.initialized {
            return;
        }
        let mut groups: HashMap<String, Vec<(String, u64)>> = HashMap::new();
        for (session_id, cwd, created_at) in sessions {
            if cwd.is_empty() {
                continue;
            }
            groups
                .entry(normalize_path(Path::new(cwd)).to_string_lossy().to_string())
                .or_default()
                .push((session_id.clone(), *created_at));
        }
        let mut ordered: Vec<(String, Vec<(String, u64)>)> = groups.into_iter().collect();
        ordered.sort_by(|a, b| {
            let latest_a = a.1.iter().map(|(_, t)| t).max().unwrap_or(&0);
            let latest_b = b.1.iter().map(|(_, t)| t).max().unwrap_or(&0);
            latest_b.cmp(latest_a)
        });
        for (path, mut members) in ordered {
            members.sort_by(|a, b| b.1.cmp(&a.1));
            let id = uuid::Uuid::new_v4().to_string();
            let title = Path::new(&path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| path.clone());
            state.order.push(id.clone());
            state.workspaces.insert(
                id.clone(),
                WorkspaceRecord {
                    id,
                    path,
                    title,
                    created_at: now_millis(),
                    session_ids: members.into_iter().map(|(sid, _)| sid).collect(),
                },
            );
        }
        state.initialized = true;
        let _ = self.save_locked(&state);
    }

    /// 幂等创建:同规范化路径返回已有记录;否则新记录 prepend。
    pub fn create(&self, path: &Path, title: Option<String>) -> Result<WorkspaceRecord, String> {
        if !path.is_dir() {
            return Err(format!("'{}' 不是目录", path.display()));
        }
        let normalized = normalize_path(path).to_string_lossy().to_string();
        let mut state = self.state.write().unwrap();
        if let Some(existing) = state.workspaces.values().find(|w| w.path == normalized) {
            return Ok(existing.clone());
        }
        let id = uuid::Uuid::new_v4().to_string();
        let title = title.unwrap_or_else(|| {
            path.file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| normalized.clone())
        });
        let record = WorkspaceRecord {
            id: id.clone(),
            path: normalized,
            title,
            created_at: now_millis(),
            session_ids: Vec::new(),
        };
        state.order.insert(0, id.clone());
        state.workspaces.insert(id, record.clone());
        let _ = self.save_locked(&state);
        Ok(record)
    }

    pub fn list(&self) -> Vec<WorkspaceRecord> {
        let state = self.state.read().unwrap();
        state
            .order
            .iter()
            .filter_map(|id| state.workspaces.get(id).cloned())
            .collect()
    }

    pub fn get(&self, id: &str) -> Option<WorkspaceRecord> {
        self.state.read().unwrap().workspaces.get(id).cloned()
    }

    pub fn resolve_by_path(&self, path: &Path) -> Option<WorkspaceRecord> {
        let normalized = normalize_path(path).to_string_lossy().to_string();
        self.state
            .read()
            .unwrap()
            .workspaces
            .values()
            .find(|w| w.path == normalized)
            .cloned()
    }

    /// 删除工作区注册并返回被移除的记录。
    pub fn take(&self, id: &str) -> Option<WorkspaceRecord> {
        let mut state = self.state.write().unwrap();
        let removed = state.workspaces.remove(id)?;
        state.order.retain(|x| x != id);
        let _ = self.save_locked(&state);
        Some(removed)
    }

    /// 把会话 prepend 进工作区账本;调用方须保证会话头 cwd == 工作区路径
    /// (会话换不了工作区,头的 cwd 不可变)。
    pub fn attach(&self, workspace_id: &str, session_id: &str) -> bool {
        let mut state = self.state.write().unwrap();
        let Some(record) = state.workspaces.get_mut(workspace_id) else {
            return false;
        };
        if !record.session_ids.iter().any(|s| s == session_id) {
            record.session_ids.insert(0, session_id.to_string());
            let _ = self.save_locked(&state);
        }
        true
    }

    /// 从**所有**工作区账本中移除一个会话 id(会话被删除时的全局清理,
    /// 保证注册表零残留)。返回是否有账本被改动。
    pub fn detach_session(&self, session_id: &str) -> bool {
        let mut state = self.state.write().unwrap();
        let mut changed = false;
        for record in state.workspaces.values_mut() {
            let before = record.session_ids.len();
            record.session_ids.retain(|id| id != session_id);
            if record.session_ids.len() != before {
                changed = true;
            }
        }
        if changed {
            let _ = self.save_locked(&state);
        }
        changed
    }

    fn save_locked(&self, state: &RegistryDoc) -> Result<(), std::io::Error> {
        let raw = serde_json::to_string_pretty(state)?;
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(&tmp, raw)?;
        std::fs::rename(&tmp, &self.file)
    }
}
