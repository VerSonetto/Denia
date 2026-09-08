//! Server-side directory browsing for the workspace picker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

const MAX_ENTRIES: usize = 500;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/fs/dirs", get(list_dirs))
        .route("/api/fs/pick", post(pick_dir))
        .route("/api/fs/mkdir", post(mkdir))
        .route("/api/fs/capability", get(capability))
        .route("/api/fs/mentions", get(list_mentions))
}

/// 目录选择器 seam 的能力决策(抄 dsh directory-picker-auto):
/// 远程绑定/SSH → browse;win/mac → native;linux 有显示+zenity → native;
/// 其余 → browse(到处能用)。消费端遇到未知 kind 的默认行为是隐藏入口。
async fn capability(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let kind = if state.bound_remote
        || std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var_os("SSH_TTY").is_some()
    {
        "browse"
    } else if cfg!(windows) || cfg!(target_os = "macos") {
        "native"
    } else if cfg!(target_os = "linux") {
        let has_display =
            std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some();
        let has_tool = which("zenity") || which("kdialog");
        if has_display && has_tool {
            "native"
        } else {
            "browse"
        }
    } else {
        "browse"
    };
    Json(json!({ "kind": kind }))
}

fn which(tool: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(tool).exists()))
        .unwrap_or(false)
}

/// browse 能力的建文件夹原语(抄 dsh browse seam 的 createDirectory)。
async fn mkdir(Json(body): Json<MkdirBody>) -> Result<impl IntoResponse, ApiError> {
    let parent = std::path::PathBuf::from(body.path.trim());
    let name = body.name.trim();
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        return Err(ApiError::bad_request(
            "directory-picker/create-failed",
            "invalid folder name",
        ));
    }
    let target = parent.join(name);
    if target.exists() {
        return Err(ApiError::bad_request(
            "directory-picker/exists",
            format!("'{}' 已存在", target.display()),
        ));
    }
    std::fs::create_dir_all(&target).map_err(|e| {
        ApiError::bad_request(
            "directory-picker/create-failed",
            format!("create failed: {e}"),
        )
    })?;
    Ok(Json(json!({ "path": target.to_string_lossy() })))
}

#[derive(Debug, serde::Deserialize)]
struct MkdirBody {
    path: String,
    name: String,
}

/// Opens the OS-native directory chooser on the host and returns the picked
/// path, or `null` when the user cancels. Blocks only a worker thread while
/// the dialog is open.
async fn pick_dir() -> Result<impl IntoResponse, ApiError> {
    let picked = tokio::task::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("denia: choose a workspace directory")
            .pick_folder()
    })
    .await
    .map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "fs/pick-failed",
            e.to_string(),
        )
    })?;
    Ok(Json(json!({
        "path": picked.map(|p| p.to_string_lossy().to_string()),
    })))
}

#[derive(Debug, Deserialize)]
struct BrowseQuery {
    #[serde(default)]
    path: Option<String>,
}

fn default_root() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

async fn list_dirs(Query(query): Query<BrowseQuery>) -> Result<impl IntoResponse, ApiError> {
    tokio::task::spawn_blocking(move || browse_impl(query.path))
        .await
        .map_err(|e| {
            ApiError::new(
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "fs/browse-failed",
                e.to_string(),
            )
        })?
}

fn browse_impl(path: Option<String>) -> Result<impl IntoResponse, ApiError> {
    let base = match path.filter(|p| !p.trim().is_empty()) {
        Some(raw) => PathBuf::from(raw.trim()),
        None => default_root(),
    };
    if !base.is_dir() {
        return Err(ApiError::bad_request(
            "fs/not-a-directory",
            format!("'{}' is not a directory", base.display()),
        ));
    }
    let read = std::fs::read_dir(&base).map_err(|e| {
        ApiError::bad_request("fs/unreadable", format!("cannot read directory: {e}"))
    })?;
    let mut dirs: Vec<String> = Vec::new();
    for entry in read.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if metadata.is_dir() {
            dirs.push(entry.file_name().to_string_lossy().to_string());
        }
        if dirs.len() >= MAX_ENTRIES {
            break;
        }
    }
    // At a filesystem root on Windows, offer the other drives so the picker
    // can cross from C: to D: and friends.
    let parent = base.parent().map(|p| p.to_string_lossy().to_string());
    if parent.is_none() && cfg!(windows) {
        dirs = (b'A'..=b'Z')
            .map(|letter| format!("{}:\\", letter as char))
            .filter(|root| PathBuf::from(root.as_str()).is_dir())
            .collect();
    }
    dirs.sort_by_key(|name| name.to_lowercase());
    Ok(Json(json!({
        "path": base.to_string_lossy().to_string(),
        "parent": parent,
        "dirs": dirs,
    })))
}

/* ---- 输入框 `@` 文件/文件夹提及候选(对照 dsh file-reference-local) ----
 * 候选只含路径,选中值保持普通提示词文本;文件内容始终留在模型侧的
 * read 类工具后面。前端只传已存活会话 cwd;path 不是目录直接 400(fail loud)。 */

/// 索引收录上限(防爆):深度优先遍历下覆盖主模块整条深链 + 多个并列模块。
/// 大项目(数万目录)无法全树收录,预算内优先保证"深路径文件"可达。
const MENTION_MAX_ENTRIES: usize = 10_000;
/// 单次查询返回上限(对照 dsh DEFAULT_FILE_SEARCH_MAX_RESULTS)。
const MENTION_MAX_RESULTS: usize = 50;
/// 永不遍历/收录的目录 basename(对照 dsh DEFAULT_FILE_SEARCH_EXCLUDED_DIRECTORIES)。
const MENTION_EXCLUDED_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "dist",
    "build",
    "out",
    "coverage",
    "target",
    ".next",
    ".nuxt",
    ".turbo",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".gradle",
];

/// 每个工作区一份的全树索引(裸词查询共享;目录下钻实时读,不走索引)。
struct MentionIndex {
    /// 建立索引时的根目录 mtime;根目录直接子项增删会变,作辅助失效信号。
    root_mtime: Option<SystemTime>,
    /// 索引条目,查询时按 Arc 共享,只 clone 命中的条目。
    entries: Arc<Vec<MentionItem>>,
    /// 建立时刻,TTL 是主失效路径。
    built_at: Instant,
}

/// 裸词查询的全局索引缓存(按 cwd 隔离;与 skills.rs 的 SUMMARY_CACHE 同款
/// OnceLock<Mutex<...>> 模式)。过期/根变化时在锁外重建,旧索引保持应答到新索引就绪。
static MENTION_INDEX_CACHE: OnceLock<Mutex<HashMap<PathBuf, MentionIndex>>> = OnceLock::new();

/// 索引新鲜窗口:连续输入(`@abc` → `@abcd`)期间共享索引,兼顾新文件可见性。
/// Windows/NTFS 不向上传播目录 mtime,深层增删只能靠这个窗口兜底;
/// 0 结果查询会穿透重建(见 `search_mention_index`),所以窗口内新建文件立即可搜。
const MENTION_INDEX_TTL: Duration = Duration::from_secs(2);
/// 0 结果穿透的最小间隔:连续空搜索不能把每次查询都变成全树遍历。
const MENTION_PENETRATE_GAP: Duration = Duration::from_millis(800);
/// 缓存保留的工作区索引上限,超出整体重置(长期运行不膨胀)。
const MENTION_INDEX_MAX_WORKSPACES: usize = 32;

fn mention_index_cache() -> &'static Mutex<HashMap<PathBuf, MentionIndex>> {
    MENTION_INDEX_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 裸词查询:命中且新鲜直接过滤(零系统调用);0 结果时穿透即时重建一次
/// (新文件在旧索引里不可见),否则锁外全树扫描 + 双检写入。
fn search_mention_index(root: &Path, query: &str) -> Vec<MentionItem> {
    let needle = query.to_lowercase();
    if let Some(entries) = fresh_mention_index(root) {
        let hits = filter_mention_entries(&entries, &needle);
        if !hits.is_empty() {
            return hits;
        }
        // 0 结果最常见于"新建文件不在旧索引里":绕过新鲜窗口立即重建一次。
        // 限频:刚重建过(< MENTION_PENETRATE_GAP)就直接返回空,防止连续
        // 空搜索把每次查询都变成全树遍历。
        let rebuilt_recently = mention_index_cache()
            .lock()
            .unwrap()
            .get(root)
            .map(|index| index.built_at.elapsed() < MENTION_PENETRATE_GAP)
            .unwrap_or(false);
        if rebuilt_recently {
            return hits;
        }
    }
    let scanned = Arc::new(scan_mention_index(root));
    let index = MentionIndex {
        root_mtime: root_mtime(root),
        entries: scanned.clone(),
        built_at: Instant::now(),
    };
    let mut cache = mention_index_cache().lock().unwrap();
    if cache.len() >= MENTION_INDEX_MAX_WORKSPACES {
        cache.clear();
    }
    cache.insert(root.to_path_buf(), index);
    filter_mention_entries(&scanned, &needle)
}

/// 命中且新鲜的索引(锁只覆盖 HashMap 访问,过滤在锁外做)。
fn fresh_mention_index(root: &Path) -> Option<Arc<Vec<MentionItem>>> {
    let cache = mention_index_cache().lock().unwrap();
    let index = cache.get(root)?;
    if index.built_at.elapsed() >= MENTION_INDEX_TTL {
        return None;
    }
    if index.root_mtime != root_mtime(root) {
        return None;
    }
    Some(index.entries.clone())
}

fn root_mtime(root: &Path) -> Option<SystemTime> {
    std::fs::metadata(root).ok().and_then(|meta| meta.modified().ok())
}

/// 全树深度优先扫描(对照 dsh scanWorkspace,但用 DFS 替代 BFS):
/// 大项目目录数远超预算,DFS 沿 read_dir 顺序的第一条链扎到底,保证
/// `app/src/main/java/.../feature/launcher` 这类深路径文件在预算内可达;
/// 排除默认排除集 + 全部隐藏目录;类型用 dirent 的 file_type 判定免 stat。
fn scan_mention_index(root: &Path) -> Vec<MentionItem> {
    let mut indexed: Vec<MentionItem> = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((abs, rel)) = stack.pop() {
        if indexed.len() >= MENTION_MAX_ENTRIES {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&abs) else {
            continue;
        };
        let mut subdirs: Vec<(PathBuf, String)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                // 目录:排除默认排除集 + 全部隐藏目录,不收录也不下钻。
                if MENTION_EXCLUDED_DIRS.contains(&name.as_str()) || name.starts_with('.') {
                    continue;
                }
                subdirs.push((entry.path(), child_rel.clone()));
                indexed.push(MentionItem {
                    path: child_rel,
                    kind: "directory",
                });
            } else if file_type.is_file() {
                indexed.push(MentionItem {
                    path: child_rel,
                    kind: "file",
                });
            }
            // 符号链接等其他类型不收录(dsh 同样不 follow)。
        }
        // 逆序入栈:栈顶保持 read_dir 顺序的第一个子目录,先沿第一条链深入。
        stack.extend(subdirs.into_iter().rev());
    }
    indexed
}

/// 对索引条目做大小写不敏感子串过滤 + 目录优先/字典序 + 截断。
/// 隐藏文件只在 query 以 `.` 开头时可见(显式引用点文件)。
fn filter_mention_entries(entries: &[MentionItem], needle: &str) -> Vec<MentionItem> {
    let show_hidden = needle.starts_with('.');
    let mut items: Vec<MentionItem> = entries
        .iter()
        .filter(|item| {
            if !show_hidden && item.path.split('/').any(|segment| segment.starts_with('.')) {
                return false;
            }
            item.path.to_lowercase().contains(needle)
        })
        .map(|item| MentionItem {
            path: item.path.clone(),
            kind: item.kind,
        })
        .collect();
    sort_mention_items(&mut items);
    items.truncate(MENTION_MAX_RESULTS);
    items
}

#[derive(Debug, serde::Serialize)]
struct MentionItem {
    /// 相对 cwd 的路径(以 `/` 分隔;目录不带尾 `/`,展示/插入层补)。
    path: String,
    kind: &'static str,
}

#[derive(Debug, Deserialize)]
struct MentionsQuery {
    path: String,
    #[serde(default)]
    query: Option<String>,
}

/// `@` 提及候选:query 为空或含 `/` 时按目录定向列举(可下钻),裸词时
/// 全树索引 + 大小写不敏感子串匹配。fs 访问放 spawn_blocking,不挡异步运行时。
async fn list_mentions(Query(query): Query<MentionsQuery>) -> Result<impl IntoResponse, ApiError> {
    let inner = tokio::task::spawn_blocking(move || {
        mentions_impl(query.path.as_str(), query.query.as_deref().unwrap_or(""))
    })
    .await
    .map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "fs/mentions-failed",
            e.to_string(),
        )
    })?;
    let items = inner?;
    Ok(Json(json!({ "items": items })))
}

fn mentions_impl(path: &str, raw_query: &str) -> Result<Vec<MentionItem>, ApiError> {
    let root = PathBuf::from(path.trim());
    if !root.is_dir() {
        return Err(ApiError::bad_request(
            "fs/mentions-not-a-directory",
            format!("'{}' is not a directory", root.display()),
        ));
    }
    let query = raw_query.replace('\\', "/");
    if query.is_empty() || query.contains('/') {
        // 目录定向列举(空查询 = 根目录)。
        let slash = query.rfind('/').map(|index| index + 1).unwrap_or(0);
        let directory = query[..slash].strip_prefix("./").unwrap_or(&query[..slash]);
        let fragment = &query[slash..];
        Ok(list_mention_directory(&root, directory, fragment))
    } else {
        Ok(search_mention_index(&root, &query))
    }
}

/// 解析目录前缀(相对 cwd):拒绝 `..` 逃逸、绝对路径段与 Windows 盘符段;
/// 目录缺失/不可读返回 None(按空列表处理,候选是辅助性的)。
fn resolve_mention_directory(root: &Path, directory: &str) -> Option<PathBuf> {
    if directory.is_empty() {
        return Some(root.to_path_buf());
    }
    if directory.starts_with('/') {
        return None;
    }
    let mut abs = root.to_path_buf();
    for segment in directory.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains(':') {
            return None;
        }
        abs.push(segment);
    }
    abs.is_dir().then_some(abs)
}

/// 目录定向列举:按 basename 大小写不敏感子串过滤 fragment,目录优先 +
/// 路径字典序。隐藏条目只在 fragment 以 `.` 开头时出现(显式引用点文件)。
fn list_mention_directory(root: &Path, directory: &str, fragment: &str) -> Vec<MentionItem> {
    let Some(abs) = resolve_mention_directory(root, directory) else {
        return Vec::new();
    };
    let needle = fragment.to_lowercase();
    let show_hidden = fragment.starts_with('.');
    let mut items: Vec<MentionItem> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&abs) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !show_hidden && name.starts_with('.') {
                continue;
            }
            if !needle.is_empty() && !name.to_lowercase().contains(&needle) {
                continue;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = format!("{directory}{name}");
            if file_type.is_dir() {
                if MENTION_EXCLUDED_DIRS.contains(&name.as_str()) {
                    continue;
                }
                items.push(MentionItem {
                    path,
                    kind: "directory",
                });
            } else if file_type.is_file() {
                items.push(MentionItem {
                    path,
                    kind: "file",
                });
            }
        }
    }
    sort_mention_items(&mut items);
    items.truncate(MENTION_MAX_RESULTS);
    items
}

fn sort_mention_items(items: &mut [MentionItem]) {
    items.sort_by(|left, right| {
        kind_rank(left.kind)
            .cmp(&kind_rank(right.kind))
            .then_with(|| left.path.to_lowercase().cmp(&right.path.to_lowercase()))
            .then_with(|| left.path.cmp(&right.path))
    });
}

fn kind_rank(kind: &str) -> u8 {
    if kind == "directory" {
        0
    } else {
        1
    }
}

#[cfg(test)]
mod mention_tests {
    use super::*;

    /// 一次性测试目录(按 name 隔离,启动时清残留),结束时清理。
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-mentions-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn missing_root_is_bad_request() {
        assert!(mentions_impl("Z:/definitely/not/a/dir", "").is_err());
    }

    #[test]
    fn empty_query_lists_root_directories_first() {
        let root = scratch("root");
        std::fs::create_dir(root.join("zeta")).unwrap();
        std::fs::create_dir(root.join("alpha")).unwrap();
        std::fs::write(root.join("beta.txt"), "x").unwrap();
        let items = mentions_impl(root.to_str().unwrap(), "").unwrap();
        let paths: Vec<&str> = items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["alpha", "zeta", "beta.txt"]);
        assert_eq!(items[0].kind, "directory");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn slash_query_drills_into_directory() {
        let root = scratch("drill");
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        std::fs::write(root.join("src/nested/mod.rs"), "").unwrap();
        std::fs::write(root.join("notes.md"), "").unwrap();
        let items = mentions_impl(root.to_str().unwrap(), "src/").unwrap();
        let paths: Vec<&str> = items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["src/nested", "src/main.rs"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn traversal_escape_is_rejected() {
        let root = scratch("escape");
        std::fs::create_dir_all(root.join("a")).unwrap();
        assert!(mentions_impl(root.to_str().unwrap(), "../").unwrap().is_empty());
        assert!(mentions_impl(root.to_str().unwrap(), "../..").unwrap().is_empty());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn excluded_and_hidden_directories_are_skipped() {
        let root = scratch("exclude");
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::create_dir(root.join(".hidden")).unwrap();
        std::fs::write(root.join("node_modules/x.js"), "").unwrap();
        std::fs::write(root.join(".hidden/s.txt"), "").unwrap();
        std::fs::write(root.join("app.ts"), "").unwrap();
        let items = mentions_impl(root.to_str().unwrap(), "").unwrap();
        let paths: Vec<&str> = items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["app.ts"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn bare_query_matches_full_path_substring() {
        let root = scratch("search");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.ts"), "").unwrap();
        std::fs::write(root.join("README.md"), "").unwrap();
        let items = mentions_impl(root.to_str().unwrap(), "ma").unwrap();
        let paths: Vec<&str> = items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["src/main.ts"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn backslash_query_is_normalized() {
        let root = scratch("backslash");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "").unwrap();
        let items = mentions_impl(root.to_str().unwrap(), "src\\").unwrap();
        let paths: Vec<&str> = items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["src/lib.rs"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn index_cache_expires_by_ttl() {
        let root = scratch("cache-ttl");
        std::fs::write(root.join("one.ts"), "").unwrap();
        // 塞一条已过 TTL 的陈旧索引:下次查询必须重建而不是应答旧条目。
        mention_index_cache().lock().unwrap().insert(
            root.clone(),
            MentionIndex {
                root_mtime: root_mtime(&root),
                entries: Arc::new(vec![MentionItem {
                    path: "stale.ts".into(),
                    kind: "file",
                }]),
                built_at: Instant::now() - MENTION_INDEX_TTL - Duration::from_secs(1),
            },
        );
        let items = search_mention_index(&root, "one");
        let paths: Vec<&str> = items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["one.ts"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn index_cache_invalidates_on_root_mtime_change() {
        let root = scratch("cache-mtime");
        std::fs::write(root.join("a.ts"), "").unwrap();
        // 根目录 mtime 与缓存记录不符:即使 TTL 内也视为陈旧。
        mention_index_cache().lock().unwrap().insert(
            root.clone(),
            MentionIndex {
                root_mtime: Some(SystemTime::UNIX_EPOCH),
                entries: Arc::new(Vec::new()),
                built_at: Instant::now(),
            },
        );
        let items = search_mention_index(&root, "a");
        assert_eq!(items.len(), 1);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn hidden_file_requires_dot_query() {
        let root = scratch("hidden-file");
        std::fs::write(root.join(".env"), "").unwrap();
        std::fs::write(root.join("app.ts"), "").unwrap();
        // 普通词搜不到隐藏文件;query 以 `.` 开头时才可见(显式引用点文件)。
        let items = mentions_impl(root.to_str().unwrap(), "env").unwrap();
        assert!(items.iter().all(|item| item.path != ".env"));
        let items = mentions_impl(root.to_str().unwrap(), ".env").unwrap();
        let paths: Vec<&str> = items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec![".env"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn empty_hits_penetrate_rebuild() {
        let root = scratch("penetrate");
        std::fs::write(root.join("a.ts"), "").unwrap();
        // 新鲜窗口内的空索引(新建文件没进旧索引的形态):0 结果查询应穿透重建并命中。
        mention_index_cache().lock().unwrap().insert(
            root.clone(),
            MentionIndex {
                root_mtime: root_mtime(&root),
                entries: Arc::new(Vec::new()),
                built_at: Instant::now() - Duration::from_secs(1),
            },
        );
        let items = search_mention_index(&root, "a");
        let paths: Vec<&str> = items.iter().map(|item| item.path.as_str()).collect();
        assert_eq!(paths, vec!["a.ts"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn empty_hits_respect_penetrate_gap() {
        let root = scratch("penetrate-gap");
        // 索引刚重建过(空索引):0 结果查询限频,不立刻再次全树遍历。
        mention_index_cache().lock().unwrap().insert(
            root.clone(),
            MentionIndex {
                root_mtime: root_mtime(&root),
                entries: Arc::new(Vec::new()),
                built_at: Instant::now(),
            },
        );
        let items = search_mention_index(&root, "nope");
        assert!(items.is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
