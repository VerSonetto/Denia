//! 文件读取状态表:同一文件的重复读取去重 + 写前新鲜度校验。
//!
//! 设计要点:
//! - Read 前先 `stat` 比对 `(mtime, size)`,未变更则**不返回内容**,只回
//!   一句"浪费的调用"提示,让模型直接引用此前的结果;
//! - Write/Edit 前校验"读过且新鲜",防止基于过期内容盲目覆写。
//!
//! 两条**关键语义**:
//!
//! 1. **range view ≠ partial view**。`offset/limit` 是模型主动指定的范围,
//!    内容对该范围是完整的;`truncatedByTokenCap` 是工具被迫截断,内容不完整。
//!    只有后者必须拒绝写操作——基于不完整内容做精确匹配替换必然失败。
//! 2. **命中缓存时不刷新 `readAt`**。否则"最近读过的文件"排序会被
//!    "刚读过但内容没变"的调用不断推高,压缩后的读状态恢复会挑错文件。
//!
//! 状态表按**会话**持有(不是按 tool 实例):同一会话的多次工具调用共享,
//! 不同会话互不干扰。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// 缓存键:`路径 + offset + limit`。
///
/// 不同分页是**不同条目**——读第 1-100 行与读第 200-300 行是两次独立读取,
/// 不能互相顶替。`offset` 缺省归一到 `1`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReadKey {
    pub path: PathBuf,
    /// 1-based 起始行;缺省归一为 1。
    pub offset: u64,
    /// 读取行数;`None` 表示读到末尾。
    pub limit: Option<u64>,
}

impl ReadKey {
    pub fn new(path: impl Into<PathBuf>, offset: Option<u64>, limit: Option<u64>) -> Self {
        Self {
            path: path.into(),
            offset: offset.unwrap_or(1),
            limit,
        }
    }
}

/// 一条读取记录。
#[derive(Debug, Clone)]
pub struct ReadEntry {
    /// 读取时的归一化 mtime(毫秒);平台不支持时为 `None`。
    pub mtime_ms: Option<u64>,
    /// 读取时的文件字节数。
    pub size_bytes: u64,
    /// 是否为**被截断的部分视图**(工具强制截断,内容不完整)。
    ///
    /// 部分视图不得用于写操作,也不命中读缓存。
    pub is_partial_view: bool,
    /// 是否为**完整读取**(offset ≤ 1 且未指定 limit)。
    ///
    /// 只有完整读取才能支撑"内容一致性"判定的兜底分支。
    pub is_full_read: bool,
    /// 最后一次真实读取的时刻(命中缓存时不刷新)。
    pub read_at: SystemTime,
    /// 读取时的完整内容(仅完整读取时保留,供写前内容比对)。
    pub content: Option<String>,
}

impl ReadEntry {
    /// 该条目能否作为写操作的依据。
    ///
    /// 部分视图不行——内容不完整,基于它做精确匹配必然失败。
    pub fn is_writable_basis(&self) -> bool {
        !self.is_partial_view
    }
}

/// 文件当前的新鲜度指纹(来自 `stat`)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub mtime_ms: Option<u64>,
    pub size_bytes: u64,
}

impl FileStamp {
    /// 从文件系统读取指纹;文件不存在或 stat 失败时返回 `None`。
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Self {
            mtime_ms: meta.modified().ok().and_then(system_time_to_ms),
            size_bytes: meta.len(),
        })
    }

    /// 从 `std::fs::Metadata` 构造(避免二次 stat)。
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        Self {
            mtime_ms: meta.modified().ok().and_then(system_time_to_ms),
            size_bytes: meta.len(),
        }
    }
}

fn system_time_to_ms(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as u64)
}

/// 读缓存命中判定:条目是否仍然反映文件当前状态。
///
/// 判定规则:
/// - **部分视图永不命中**——它不代表文件的完整状态;
/// - 优先比 `(mtime, size)`;mtime 不可用时退化为只比 `size`。
pub fn is_fresh(entry: &ReadEntry, stamp: &FileStamp) -> bool {
    if entry.is_partial_view {
        return false;
    }
    match (entry.mtime_ms, stamp.mtime_ms) {
        (Some(cached), Some(current)) => cached == current && entry.size_bytes == stamp.size_bytes,
        // mtime 不可用(某些文件系统):退化为 size 比较,并接受"大小相同即未变"
        // 的近似——比完全不做去重更划算,且 mtime 缺失是罕见情况。
        _ => entry.size_bytes == stamp.size_bytes,
    }
}

/// 写前新鲜度校验结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteCheck {
    /// 没读过这个文件。
    NeverRead,
    /// 读过,但只是部分视图(内容不完整)。
    PartialView,
    /// 读过且新鲜,可以写。
    Fresh,
    /// 读过,但文件在读取之后被改动过。
    Stale,
}

/// 文件读取状态表(按会话共享)。
#[derive(Debug, Default)]
pub struct ReadState {
    entries: HashMap<ReadKey, ReadEntry>,
}

impl ReadState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 查询缓存条目。
    pub fn get(&self, key: &ReadKey) -> Option<&ReadEntry> {
        self.entries.get(key)
    }

    /// 记录一次读取。
    pub fn record(&mut self, key: ReadKey, entry: ReadEntry) {
        self.entries.insert(key, entry);
    }

    /// 清空(压缩后调用:压缩摘要已接管旧内容的记忆职责)。
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// 已记录条目数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 按"最近读取"排序的全部条目(供压缩后读状态恢复挑选)。
    ///
    /// 只返回可作为写依据的条目(排除部分视图)。
    pub fn recent_writable(&self, limit: usize) -> Vec<(ReadKey, ReadEntry)> {
        let mut items: Vec<(ReadKey, ReadEntry)> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.is_writable_basis())
            .map(|(key, entry)| (key.clone(), entry.clone()))
            .collect();
        items.sort_by(|a, b| b.1.read_at.cmp(&a.1.read_at));
        items.truncate(limit);
        items
    }

    /// 写前校验:该文件是否"读过且新鲜"。
    ///
    /// 查找**同一路径**下最近一次读取(不限定 offset/limit)——写操作关心的是
    /// "模型是否看过这个文件的足够内容",而不是某个特定分页。
    pub fn check_writable(&self, path: &Path, stamp: &FileStamp) -> WriteCheck {
        let latest = self.latest_for_path(path);
        let Some(entry) = latest else {
            return WriteCheck::NeverRead;
        };
        if !entry.is_writable_basis() {
            return WriteCheck::PartialView;
        }
        // 文件变了 → 要求重读。判定规则:
        // 优先 mtime 前进或 size 变化;mtime 不可用时退化到 size。
        let changed = match (entry.mtime_ms, stamp.mtime_ms) {
            (Some(cached), Some(current)) => current != cached || entry.size_bytes != stamp.size_bytes,
            _ => entry.size_bytes != stamp.size_bytes,
        };
        if changed {
            WriteCheck::Stale
        } else {
            WriteCheck::Fresh
        }
    }

    /// 同路径下最近一次读取。
    fn latest_for_path(&self, path: &Path) -> Option<&ReadEntry> {
        self.entries
            .iter()
            .filter(|(key, _)| key.path == path)
            .map(|(_, entry)| entry)
            .max_by_key(|entry| entry.read_at)
    }

    /// 写入成功后刷新条目:文件内容已变成我们写进去的样子。
    ///
    /// 这样模型接着 Edit 同一文件时不会因为"写前校验"而要求重读。
    pub fn record_after_write(&mut self, path: &Path, content: String, stamp: FileStamp) {
        // 清掉该路径的全部旧条目(写操作让所有分页视图都过期)。
        self.entries.retain(|key, _| key.path != path);
        self.entries.insert(
            ReadKey::new(path, None, None),
            ReadEntry {
                mtime_ms: stamp.mtime_ms,
                size_bytes: stamp.size_bytes,
                is_partial_view: false,
                is_full_read: true,
                read_at: SystemTime::now(),
                content: Some(content),
            },
        );
    }
}

/// 会话共享的读状态句柄。
pub type SharedReadState = Arc<Mutex<ReadState>>;

/// 新建共享读状态。
pub fn shared() -> SharedReadState {
    Arc::new(Mutex::new(ReadState::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(mtime: Option<u64>, size: u64) -> FileStamp {
        FileStamp {
            mtime_ms: mtime,
            size_bytes: size,
        }
    }

    fn entry(mtime: Option<u64>, size: u64) -> ReadEntry {
        ReadEntry {
            mtime_ms: mtime,
            size_bytes: size,
            is_partial_view: false,
            is_full_read: true,
            read_at: SystemTime::now(),
            content: Some("content".into()),
        }
    }

    #[test]
    fn same_mtime_and_size_is_fresh() {
        assert!(is_fresh(&entry(Some(100), 50), &stamp(Some(100), 50)));
    }

    #[test]
    fn changed_mtime_is_not_fresh() {
        assert!(!is_fresh(&entry(Some(100), 50), &stamp(Some(200), 50)));
    }

    #[test]
    fn changed_size_is_not_fresh() {
        assert!(!is_fresh(&entry(Some(100), 50), &stamp(Some(100), 60)));
    }

    #[test]
    fn partial_view_never_hits_cache() {
        // 关键语义:被截断过的读取不代表文件完整状态,永不命中。
        let mut e = entry(Some(100), 50);
        e.is_partial_view = true;
        assert!(!is_fresh(&e, &stamp(Some(100), 50)));
    }

    #[test]
    fn missing_mtime_falls_back_to_size() {
        assert!(is_fresh(&entry(None, 50), &stamp(None, 50)));
        assert!(!is_fresh(&entry(None, 50), &stamp(None, 51)));
    }

    #[test]
    fn offset_defaults_to_one() {
        assert_eq!(ReadKey::new("a.rs", None, None).offset, 1);
        assert_eq!(ReadKey::new("a.rs", Some(1), None).offset, 1);
        assert_eq!(ReadKey::new("a.rs", Some(200), Some(50)).offset, 200);
    }

    #[test]
    fn different_ranges_are_different_entries() {
        let mut state = ReadState::new();
        state.record(ReadKey::new("a.rs", None, None), entry(Some(100), 50));
        state.record(
            ReadKey::new("a.rs", Some(200), Some(50)),
            entry(Some(100), 50),
        );
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn check_writable_reports_never_read() {
        let state = ReadState::new();
        assert_eq!(
            state.check_writable(Path::new("a.rs"), &stamp(Some(100), 50)),
            WriteCheck::NeverRead
        );
    }

    #[test]
    fn check_writable_rejects_partial_view() {
        let mut state = ReadState::new();
        let mut e = entry(Some(100), 50);
        e.is_partial_view = true;
        state.record(ReadKey::new("a.rs", None, None), e);
        assert_eq!(
            state.check_writable(Path::new("a.rs"), &stamp(Some(100), 50)),
            WriteCheck::PartialView
        );
    }

    #[test]
    fn check_writable_fresh_after_read() {
        let mut state = ReadState::new();
        state.record(ReadKey::new("a.rs", None, None), entry(Some(100), 50));
        assert_eq!(
            state.check_writable(Path::new("a.rs"), &stamp(Some(100), 50)),
            WriteCheck::Fresh
        );
    }

    #[test]
    fn check_writable_stale_after_external_change() {
        let mut state = ReadState::new();
        state.record(ReadKey::new("a.rs", None, None), entry(Some(100), 50));
        assert_eq!(
            state.check_writable(Path::new("a.rs"), &stamp(Some(999), 50)),
            WriteCheck::Stale
        );
    }

    #[test]
    fn record_after_write_makes_file_writable() {
        let mut state = ReadState::new();
        state.record_after_write(Path::new("a.rs"), "new".into(), stamp(Some(500), 3));
        assert_eq!(
            state.check_writable(Path::new("a.rs"), &stamp(Some(500), 3)),
            WriteCheck::Fresh
        );
    }

    #[test]
    fn record_after_write_clears_stale_ranges() {
        let mut state = ReadState::new();
        state.record(ReadKey::new("a.rs", Some(1), Some(10)), entry(Some(100), 50));
        state.record(ReadKey::new("a.rs", Some(50), Some(10)), entry(Some(100), 50));
        assert_eq!(state.len(), 2);
        state.record_after_write(Path::new("a.rs"), "x".into(), stamp(Some(200), 1));
        // 写操作让所有旧分页视图过期,只剩写后那一条。
        assert_eq!(state.len(), 1);
    }

    #[test]
    fn recent_writable_excludes_partial_views_and_sorts() {
        let mut state = ReadState::new();
        let mut old = entry(Some(100), 10);
        old.read_at = SystemTime::now() - std::time::Duration::from_secs(60);
        state.record(ReadKey::new("old.rs", None, None), old);

        let mut partial = entry(Some(100), 10);
        partial.is_partial_view = true;
        partial.read_at = SystemTime::now();
        state.record(ReadKey::new("partial.rs", None, None), partial);

        state.record(ReadKey::new("new.rs", None, None), entry(Some(100), 10));

        let recent = state.recent_writable(10);
        assert_eq!(recent.len(), 2, "部分视图必须被排除");
        assert_eq!(recent[0].0.path, PathBuf::from("new.rs"));
        assert_eq!(recent[1].0.path, PathBuf::from("old.rs"));
    }
}
