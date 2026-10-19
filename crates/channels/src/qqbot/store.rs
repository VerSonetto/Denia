//! QQ 机器人凭据存储。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// QQ 开放平台机器人的 app_id / client_secret。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub app_id: String,
    pub client_secret: String,
    #[serde(default)]
    pub saved_at: String,
}

/// 通道数据目录:`<home>/channels/qqbot`。
pub fn channel_dir(home: &Path) -> PathBuf {
    home.join("channels").join("qqbot")
}

fn account_path(dir: &Path) -> PathBuf {
    dir.join("account.json")
}

pub fn save_account(dir: &Path, account: &Account) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|error| format!("创建 QQ 账号目录失败:{error}"))?;
    let text = serde_json::to_string_pretty(account)
        .map_err(|error| format!("序列化 QQ 账号失败:{error}"))?;
    let temp = account_path(dir).with_extension("json.tmp");
    std::fs::write(&temp, text)
        .and_then(|_| std::fs::rename(&temp, account_path(dir)))
        .map_err(|error| format!("保存 QQ 账号失败:{error}"))
}

pub fn load_account(dir: &Path) -> Option<Account> {
    let text = std::fs::read_to_string(account_path(dir)).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn clear_account(dir: &Path) -> Result<(), String> {
    match std::fs::remove_file(account_path(dir)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("删除 QQ 账号失败:{error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "denia-qqbot-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let account = Account {
            app_id: "10001".into(),
            client_secret: "secret".into(),
            saved_at: String::new(),
        };
        save_account(&dir, &account).unwrap();
        assert_eq!(load_account(&dir).unwrap().app_id, "10001");
        clear_account(&dir).unwrap();
        assert!(load_account(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
