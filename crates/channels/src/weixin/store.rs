//! 微信通道的本地状态:账号凭据、拉取游标、扫码登录快照。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 扫码登录换到的凭据。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub account_id: String,
    pub token: String,
    #[serde(default)]
    pub base_url: String,
    #[serde(default)]
    pub user_id: String,
    #[serde(default)]
    pub saved_at: String,
}

/// 通道数据目录:`<home>/channels/weixin`。
pub fn channel_dir(home: &Path) -> PathBuf {
    home.join("channels").join("weixin")
}

fn account_path(dir: &Path) -> PathBuf {
    dir.join("account.json")
}

fn sync_path(dir: &Path) -> PathBuf {
    dir.join("sync.json")
}

pub fn save_account(dir: &Path, account: &Account) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("创建微信账号目录失败:{error}"))?;
    let text = serde_json::to_string_pretty(account)
        .map_err(|error| format!("序列化微信账号失败:{error}"))?;
    let temp = account_path(dir).with_extension("json.tmp");
    std::fs::write(&temp, text)
        .and_then(|_| std::fs::rename(&temp, account_path(dir)))
        .map_err(|error| format!("保存微信账号失败:{error}"))
}

pub fn load_account(dir: &Path) -> Option<Account> {
    let text = std::fs::read_to_string(account_path(dir)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn clear_account(dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(account_path(dir)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("删除微信账号失败:{error}")),
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SyncCursor {
    #[serde(default)]
    get_updates_buf: String,
}

pub fn load_sync(dir: &Path) -> String {
    std::fs::read_to_string(sync_path(dir))
        .ok()
        .and_then(|text| serde_json::from_str::<SyncCursor>(&text).ok())
        .map(|cursor| cursor.get_updates_buf)
        .unwrap_or_default()
}

pub fn save_sync(dir: &Path, buffer: &str) {
    let cursor = SyncCursor {
        get_updates_buf: buffer.to_string(),
    };
    let Ok(text) = serde_json::to_string(&cursor) else {
        return;
    };
    let path = sync_path(dir);
    let temp = path.with_extension("json.tmp");
    if let Err(error) = std::fs::create_dir_all(dir)
        .and_then(|_| std::fs::write(&temp, text))
        .and_then(|_| std::fs::rename(&temp, &path))
    {
        tracing::warn!(%error, "微信拉取游标落盘失败");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_and_cursor_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "denia-weixin-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let account = Account {
            account_id: "bot@im.bot".into(),
            token: "secret".into(),
            base_url: "https://example.invalid".into(),
            user_id: "u".into(),
            saved_at: String::new(),
        };
        save_account(&dir, &account).unwrap();
        assert_eq!(load_account(&dir).unwrap().account_id, "bot@im.bot");
        save_sync(&dir, "cursor-1");
        assert_eq!(load_sync(&dir), "cursor-1");
        clear_account(&dir).unwrap();
        assert!(load_account(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
