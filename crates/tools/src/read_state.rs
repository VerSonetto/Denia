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

use crate::coordination::ContentFingerprint;

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
    /// 读取时的**内容指纹**;文件过大或算不出时为 `None`。
    ///
    /// `(mtime, size)` 发现不了同尺寸改写(尤其是落在同一时间粒度里的
    /// 改写),指纹能。任一侧缺指纹时,写前校验退化为 `(mtime, size)`。
    pub fingerprint: Option<ContentFingerprint>,
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
    /// 产生这条记录的**工具调用 id**。
    ///
    /// 去重的前提是"那次调用的结果仍在模型上下文里"。结果被微压缩清成
    /// 占位符后,提示"内容同上一条读取结果"就指向了不存在的内容,必须按
    /// 调用 id 精确失效([`ReadState::invalidate_calls`])。写后记录没有
    /// 对应的读取调用,为 `None`(也就不会被清理连带失效)。
    pub call_id: Option<String>,
}

impl ReadEntry {
    /// 该条目能否作为写操作的依据。
    ///
    /// 部分视图不行——内容不完整,基于它做精确匹配必然失败。
    pub fn is_writable_basis(&self) -> bool {
        !self.is_partial_view
    }
}

/// 文件当前的新鲜度指纹(来自 `stat` + 内容指纹)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    pub mtime_ms: Option<u64>,
    pub size_bytes: u64,
    /// 文件当前内容的指纹;超过 [`MAX_FINGERPRINT_BYTES`] 时是 `None`。
    pub fingerprint: Option<ContentFingerprint>,
}

impl FileStamp {
    /// 从文件系统读取指纹:`stat` + 内容指纹;文件不存在或 stat 失败时返回 `None`。
    ///
    /// 算内容指纹要把文件读一遍,所以只在**写前校验**这类低频路径上用;
    /// 读缓存的去重判定走 [`FileStamp::stat_only`]。
    pub fn of(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        let mut stamp = Self::from_metadata(&meta);
        stamp.fingerprint = fingerprint_of(path, stamp.size_bytes);
        Some(stamp)
    }

    /// 只取 `(mtime, size)`,不算内容指纹。
    ///
    /// 去重判定不需要指纹,而"每次读取都顺带把文件整读一遍"是不可接受的
    /// 代价(命中缓存的那些调用本来可以完全不读文件)。
    pub fn stat_only(path: &Path) -> Option<Self> {
        std::fs::metadata(path)
            .ok()
            .map(|meta| Self::from_metadata(&meta))
    }

    /// 从 `std::fs::Metadata` 构造(避免二次 stat);不含内容指纹。
    pub fn from_metadata(meta: &std::fs::Metadata) -> Self {
        Self {
            mtime_ms: meta.modified().ok().and_then(system_time_to_ms),
            size_bytes: meta.len(),
            fingerprint: None,
        }
    }
}

/// 内容指纹的读取上限。
///
/// 算指纹要把文件内容读一遍;设上限是为了让读路径与写前校验的额外 IO 有界。
/// 超过上限的文件指纹为 `None`,写前校验退化为 `(mtime, size)`——大文件的
/// 改写几乎总会动到 size 或 mtime,这个退化可以接受。
const MAX_FINGERPRINT_BYTES: u64 = 8 * 1024 * 1024;

fn fingerprint_of(path: &Path, size_bytes: u64) -> Option<ContentFingerprint> {
    if size_bytes > MAX_FINGERPRINT_BYTES {
        return None;
    }
    ContentFingerprint::of_path(path).ok().flatten()
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
    /// 读过,但文件在读取之后被改动过(`mtime` 前进或 `size` 变化)。
    Stale,
    /// 读过,`(mtime, size)` 都没变,但**内容指纹不符**:文件在同尺寸下
    /// 被改写(或改动落在文件系统时间粒度之内)。
    ContentChanged,
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

    /// 失效"由这些工具调用产生"的读记录(微压缩把那批结果清成占位符后调用)。
    ///
    /// 精确失效而非整体清空:只有内容真的从上下文里消失的文件才失去去重
    /// 收益,其余条目(以及写后记录)照旧命中缓存。调用 id 拿不到的条目
    /// (`None`)不在这里失效。
    ///
    /// 返回被移除的条目数。
    pub fn invalidate_calls<'a>(&mut self, call_ids: impl IntoIterator<Item = &'a str>) -> usize {
        let ids: std::collections::HashSet<&str> = call_ids.into_iter().collect();
        if ids.is_empty() {
            return 0;
        }
        let before = self.entries.len();
        self.entries
            .retain(|_, entry| !entry.call_id.as_deref().is_some_and(|id| ids.contains(id)));
        before - self.entries.len()
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
    ///
    /// 两层判定:`(mtime, size)` 变了就是 [`WriteCheck::Stale`];`(mtime, size)`
    /// 没变而内容指纹变了(同尺寸改写)是 [`WriteCheck::ContentChanged`]。
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
            (Some(cached), Some(current)) => {
                current != cached || entry.size_bytes != stamp.size_bytes
            }
            _ => entry.size_bytes != stamp.size_bytes,
        };
        if changed {
            return WriteCheck::Stale;
        }
        // `(mtime, size)` 都没变,再比内容指纹:同尺寸改写只有指纹能发现。
        // 任一侧缺指纹(文件过大,或条目来自不算指纹的路径)就跳过这一层,
        // 不能凭空判成冲突。
        match (entry.fingerprint, stamp.fingerprint) {
            (Some(cached), Some(current)) if cached != current => WriteCheck::ContentChanged,
            _ => WriteCheck::Fresh,
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
    /// 指纹取自写后的 `stamp`(即刚写进去的那份内容)。
    pub fn record_after_write(&mut self, path: &Path, content: String, stamp: FileStamp) {
        // 清掉该路径的全部旧条目(写操作让所有分页视图都过期)。
        self.entries.retain(|key, _| key.path != path);
        self.entries.insert(
            ReadKey::new(path, None, None),
            ReadEntry {
                mtime_ms: stamp.mtime_ms,
                size_bytes: stamp.size_bytes,
                fingerprint: stamp.fingerprint,
                is_partial_view: false,
                is_full_read: true,
                read_at: SystemTime::now(),
                content: Some(content),
                // 写后记录不来自读取调用,没有可失效的调用 id。
                call_id: None,
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
            fingerprint: None,
        }
    }

    /// 带内容指纹的 stat 快照:模拟写前校验时从盘上看到的当前状态。
    fn stamp_of(mtime: Option<u64>, size: u64, body: &str) -> FileStamp {
        FileStamp {
            fingerprint: Some(ContentFingerprint::of_bytes(body.as_bytes())),
            ..stamp(mtime, size)
        }
    }

    fn entry(mtime: Option<u64>, size: u64) -> ReadEntry {
        ReadEntry {
            mtime_ms: mtime,
            size_bytes: size,
            fingerprint: None,
            is_partial_view: false,
            is_full_read: true,
            read_at: SystemTime::now(),
            content: Some("content".into()),
            call_id: None,
        }
    }

    /// 记录了正文 `body`(含内容指纹)的读条目。
    fn entry_of_body(mtime: Option<u64>, size: u64, body: &str) -> ReadEntry {
        ReadEntry {
            fingerprint: Some(ContentFingerprint::of_bytes(body.as_bytes())),
            content: Some(body.to_string()),
            ..entry(mtime, size)
        }
    }

    /// 带工具调用 id 的条目(模拟 read_file 记录的那一条)。
    fn entry_of(call_id: &str) -> ReadEntry {
        ReadEntry {
            call_id: Some(call_id.to_string()),
            ..entry(Some(100), 50)
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
    fn same_size_rewrite_is_caught_by_the_content_fingerprint() {
        // 关键回归:mtime 与 size 都没变(同尺寸改写,或改动落在文件系统
        // 时间粒度之内),只有内容指纹能发现。
        let mut state = ReadState::new();
        state.record(
            ReadKey::new("a.rs", None, None),
            entry_of_body(Some(100), 4, "abcd"),
        );
        assert_eq!(
            state.check_writable(Path::new("a.rs"), &stamp_of(Some(100), 4, "abce")),
            WriteCheck::ContentChanged
        );
        // 指纹一致 → 照旧可写。
        assert_eq!(
            state.check_writable(Path::new("a.rs"), &stamp_of(Some(100), 4, "abcd")),
            WriteCheck::Fresh
        );
    }

    #[test]
    fn missing_fingerprint_falls_back_to_mtime_and_size() {
        // 文件过大(没算指纹)时不能凭空判冲突:退化为 (mtime, size) 比较。
        let mut state = ReadState::new();
        state.record(ReadKey::new("a.rs", None, None), entry(Some(100), 4));
        assert_eq!(
            state.check_writable(Path::new("a.rs"), &stamp_of(Some(100), 4, "abce")),
            WriteCheck::Fresh
        );
        assert_eq!(
            state.check_writable(Path::new("a.rs"), &stamp(Some(100), 5)),
            WriteCheck::Stale
        );
    }

    #[test]
    fn changed_stat_wins_over_the_same_size_explanation() {
        // (mtime, size) 确实变了 → 报 Stale,不必再解释"同尺寸改写"。
        let mut state = ReadState::new();
        state.record(
            ReadKey::new("a.rs", None, None),
            entry_of_body(Some(100), 4, "abcd"),
        );
        assert_eq!(
            state.check_writable(Path::new("a.rs"), &stamp_of(Some(200), 4, "abce")),
            WriteCheck::Stale
        );
    }

    #[test]
    fn file_stamp_of_reads_the_fingerprint_and_stat_only_skips_it() {
        let dir =
            std::env::temp_dir().join(format!("denia-read-state-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("probe.txt");
        std::fs::write(&file, b"abcd").unwrap();

        let stamp = FileStamp::of(&file).unwrap();
        assert_eq!(stamp.size_bytes, 4);
        assert_eq!(
            stamp.fingerprint,
            Some(ContentFingerprint::of_bytes(b"abcd"))
        );
        assert_eq!(FileStamp::stat_only(&file).unwrap().fingerprint, None);
        // 文件不存在:两种取法都给 None。
        assert!(FileStamp::of(&dir.join("missing.txt")).is_none());
        assert!(FileStamp::stat_only(&dir.join("missing.txt")).is_none());

        // 同尺寸改写 → 指纹变。
        std::fs::write(&file, b"abce").unwrap();
        assert_ne!(FileStamp::of(&file).unwrap().fingerprint, stamp.fingerprint);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn oversized_files_skip_the_fingerprint() {
        // 超过上限的文件不算指纹(额外 IO 有界),写前校验退化为 (mtime, size)。
        let dir = std::env::temp_dir().join(format!("denia-read-state-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.txt");
        std::fs::write(&file, vec![b'x'; MAX_FINGERPRINT_BYTES as usize + 1]).unwrap();

        let stamp = FileStamp::of(&file).unwrap();
        assert_eq!(stamp.size_bytes, MAX_FINGERPRINT_BYTES + 1);
        assert_eq!(stamp.fingerprint, None);
        let _ = std::fs::remove_dir_all(&dir);
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
        state.record(
            ReadKey::new("a.rs", Some(1), Some(10)),
            entry(Some(100), 50),
        );
        state.record(
            ReadKey::new("a.rs", Some(50), Some(10)),
            entry(Some(100), 50),
        );
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

    #[test]
    fn invalidate_calls_removes_only_the_cleared_call_entries() {
        // 微压缩只清掉了一部分工具结果:被清的那条对应的读记录必须失效
        // (它的内容已不在上下文里),其余文件照旧保留去重收益。
        let mut state = ReadState::new();
        state.record(ReadKey::new("a.rs", None, None), entry_of("call_a"));
        state.record(ReadKey::new("b.rs", None, None), entry_of("call_b"));
        state.record(
            ReadKey::new("a.rs", Some(200), Some(50)),
            entry_of("call_a"),
        );

        assert_eq!(state.invalidate_calls(["call_a"]), 2);
        assert!(state.get(&ReadKey::new("a.rs", None, None)).is_none());
        assert!(
            state
                .get(&ReadKey::new("a.rs", Some(200), Some(50)))
                .is_none()
        );
        assert!(state.get(&ReadKey::new("b.rs", None, None)).is_some());
    }

    #[test]
    fn invalidate_calls_keeps_entries_without_call_id_and_ignores_empty_input() {
        // 写后记录(无 call_id)不因清理失效;空集合不误伤任何条目。
        let mut state = ReadState::new();
        state.record_after_write(Path::new("w.rs"), "body".into(), stamp(Some(7), 4));
        state.record(ReadKey::new("a.rs", None, None), entry_of("call_a"));

        assert_eq!(state.invalidate_calls(Vec::<&str>::new()), 0);
        assert_eq!(state.len(), 2);
        assert_eq!(state.invalidate_calls(["call_a"]), 1);
        assert_eq!(
            state.check_writable(Path::new("w.rs"), &stamp(Some(7), 4)),
            WriteCheck::Fresh
        );
    }
}
