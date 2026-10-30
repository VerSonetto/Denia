//! 微信用户 -> denia 工作区/会话的绑定状态。
//!
//! 每个发送者一份:`/workspace` 选中工作区后清空会话,下一条消息自动
//! 新建会话;`/switch` 选中具体会话后沿用。权限档位与模型选择同样按
//! 发送者保存,重启后仍生效。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use denia_core::config::ModelSelection;
use denia_core::session::PermissionMode;
use serde::{Deserialize, Serialize};

/// 单个发送者的绑定。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// 新建会话时使用的权限档位;`None` = 宿主默认(auto-edit)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission: Option<PermissionMode>,
    /// 首次成功解析后固定下来,避免每次消息都重建模型目录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelSelection>,
}

/// 绑定存储:内存为主,变更后原子落盘。
pub struct BindingStore {
    path: PathBuf,
    inner: Mutex<HashMap<String, Binding>>,
}

impl BindingStore {
    pub fn open(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<HashMap<String, Binding>>(&text).ok())
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }

    pub fn get(&self, user: &str) -> Binding {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(user)
            .cloned()
            .unwrap_or_default()
    }

    /// 修改并落盘。写入失败只记日志:绑定是体验状态,不阻断消息处理。
    ///
    /// 落盘仍在锁内:写文件很短,而锁外写会让两个并发消息的旧快照覆盖新快照
    /// (绑定丢失比这点写延迟更糟)。
    pub fn update(&self, user: &str, change: impl FnOnce(&mut Binding)) {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let entry = map.entry(user.to_string()).or_default();
        change(entry);
        if let Some(parent) = self.path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(%error, "通道绑定目录创建失败");
            return;
        }
        match serde_json::to_vec_pretty(&*map) {
            Ok(bytes) => {
                let temp = self.path.with_extension("json.tmp");
                if let Err(error) =
                    std::fs::write(&temp, &bytes).and_then(|_| std::fs::rename(&temp, &self.path))
                {
                    tracing::warn!(%error, "通道绑定落盘失败");
                }
            }
            Err(error) => tracing::warn!(%error, "通道绑定序列化失败"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds_round_trip_and_reloads() {
        let dir = std::env::temp_dir().join(format!("denia-bindings-{}", std::process::id()));
        let path = dir.join("bindings.json");
        let _ = std::fs::remove_file(&path);
        let store = BindingStore::open(&path);
        store.update("u1", |binding| {
            binding.workspace_id = Some("ws-1".into());
            binding.permission = Some(PermissionMode::Full);
            binding.model = Some(ModelSelection {
                provider: "p".into(),
                model: "m".into(),
                reasoning_effort: None,
            });
        });
        let restored = BindingStore::open(&path).get("u1");
        assert_eq!(restored.workspace_id.as_deref(), Some("ws-1"));
        assert_eq!(restored.permission, Some(PermissionMode::Full));
        assert_eq!(restored.model.unwrap().model, "m");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
