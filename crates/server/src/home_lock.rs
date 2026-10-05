//! A running host owns the data directory exclusively. The OS releases the lock on exit.

use std::fs::{File, OpenOptions};
use std::path::Path;

pub(crate) fn acquire(home: &Path) -> Result<File, Box<dyn std::error::Error + Send + Sync>> {
    std::fs::create_dir_all(home)?;
    let path = home.join("server.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    file.try_lock().map_err(|error| {
        format!("数据目录 {} 已被另一个服务占用或无法锁定: {error}。请使用不同的 --home / DESKTOP_HOME。", home.display())
    })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_host_cannot_write_the_same_home_until_the_owner_exits() {
        let home = std::env::temp_dir().join(format!("denia-lock-{}", uuid::Uuid::new_v4()));
        let owner = acquire(&home).unwrap();
        assert!(acquire(&home).is_err());
        drop(owner);
        drop(acquire(&home).unwrap());
        std::fs::remove_dir_all(home).unwrap();
    }
}
