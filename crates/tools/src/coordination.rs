//! 路径级写协调:同一路径的读-改-写临界区串行,不同路径仍然并行。
//!
//! 覆盖范围是**进程内**经工具层发生的写入,并且跨会话、跨父/子代理共享同一
//! 张表 —— 锁身份只由规范化路径决定,与哪个会话发起无关。它**不**覆盖
//! arbitrary shell 命令、用户编辑器或其他进程的写入:那些变化由写前的
//! [`ContentFingerprint`] 检测,不宣称实现了外部原子 CAS。

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// 进程级路径锁注册表。
#[derive(Default)]
pub struct PathLocks {
    entries: Mutex<BTreeMap<PathBuf, Weak<AsyncMutex<()>>>>,
}

impl PathLocks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 进程级单例:父会话、子代理与后台任务共享同一张锁表。
    pub fn shared() -> &'static Arc<PathLocks> {
        static SHARED: OnceLock<Arc<PathLocks>> = OnceLock::new();
        SHARED.get_or_init(PathLocks::new)
    }

    /// 进入某路径的临界区;守卫析构即释放。
    ///
    /// 同一路径(按 [`lock_key`] 规范化后)串行,不同路径互不阻塞。
    pub async fn lock(&self, path: &Path) -> OwnedMutexGuard<()> {
        self.handle(&lock_key(path)).lock_owned().await
    }

    fn handle(&self, key: &Path) -> Arc<AsyncMutex<()>> {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(existing) = entries.get(key).and_then(Weak::upgrade) {
            return existing;
        }
        // 顺手回收已无人持有的锁,避免长会话里键无限增长。
        entries.retain(|_, weak| weak.strong_count() > 0);
        let fresh = Arc::new(AsyncMutex::new(()));
        entries.insert(key.to_path_buf(), Arc::downgrade(&fresh));
        fresh
    }
}

/// 锁身份:规范化的真实路径。
///
/// 文件存在时 `canonicalize`,消解 `\\?\` 前缀、短名与符号链接/junction;
/// 不存在时退化为词法规范化(绝对化 + 消解 `.`/`..`)。Windows 上再折叠
/// ASCII 大小写,避免同一个文件因为拼写差异拿到两把锁。
///
/// **已知窄缝**:两条分支的口径不同 —— `canonicalize` 会解析符号链接与
/// junction,词法分支只做拼写规范化、不解析任何链接。于是同一个文件经「软链
/// 路径」与「真实路径」进入时,只要有一侧落进词法分支(典型是目标文件尚不
/// 存在,或 canonicalize 因权限等原因失败),它就会拿到两把不同的锁:这里的
/// 锁身份只保证同一拼写形态下的互斥,跨链接形态的等价性不在覆盖范围内。
/// 这是刻意留着的窄缝,不修。
pub fn lock_key(path: &Path) -> PathBuf {
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| lexical_normalize(path));
    let plain = strip_verbatim(&resolved);
    if cfg!(windows) {
        PathBuf::from(plain.to_string_lossy().to_ascii_lowercase())
    } else {
        plain
    }
}

/// 内容指纹:写前校验用,检测不遵守内部锁的外部改动。
///
/// 与 `(mtime, size)` 相比,同尺寸的改写也能被发现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentFingerprint(u64);

impl ContentFingerprint {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        bytes.hash(&mut hasher);
        Self(hasher.finish())
    }

    /// 读磁盘当前内容算指纹;文件不存在时返回 `None`。
    pub fn of_path(path: &Path) -> std::io::Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(Self::of_bytes(&bytes))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn as_u64(self) -> u64 {
        self.0
    }
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(windows)]
fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    let Some(rest) = text.strip_prefix(r"\\?\") else {
        return path.to_path_buf();
    };
    match rest.strip_prefix("UNC\\") {
        Some(unc) => PathBuf::from(format!(r"\\{unc}")),
        None => PathBuf::from(rest),
    }
}

#[cfg(not(windows))]
fn strip_verbatim(path: &Path) -> PathBuf {
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_key_treats_spelling_variants_as_one_file() {
        let dir = std::env::temp_dir();
        let file = dir.join("denia-lock-key-probe.txt");
        std::fs::write(&file, b"x").expect("写探针文件");
        let dotted = dir.join(".").join("denia-lock-key-probe.txt");
        assert_eq!(lock_key(&file), lock_key(&dotted));
        #[cfg(windows)]
        {
            let upper = dir.join("DENIA-LOCK-KEY-PROBE.TXT");
            assert_eq!(lock_key(&file), lock_key(&upper));
        }
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn lock_key_handles_a_path_that_does_not_exist_yet() {
        // 新建文件(preview/write)路径不存在时必须仍能给出稳定身份。
        let dir = std::env::temp_dir();
        let ghost = dir
            .join("denia-lock-key-ghost")
            .join("..")
            .join("ghost.txt");
        let expected = lock_key(&dir.join("ghost.txt"));
        assert_eq!(lock_key(&ghost), expected);
    }

    #[test]
    fn fingerprint_sees_a_same_size_rewrite() {
        assert_ne!(
            ContentFingerprint::of_bytes(b"abcd"),
            ContentFingerprint::of_bytes(b"abce")
        );
        assert_eq!(
            ContentFingerprint::of_bytes(b"abcd"),
            ContentFingerprint::of_bytes(b"abcd")
        );
    }
}
