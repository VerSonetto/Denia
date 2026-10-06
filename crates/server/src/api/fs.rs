//! Server-side directory browsing for the workspace picker.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
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
        .route("/api/fs/tree", get(list_tree))
        .route("/api/fs/file", get(read_workspace_file))
        // 路径式而非查询式：HTML 里的 `./a.css`、`../img/b.png` 只能靠
        // **URL 路径**解析——查询串不参与路径拼接。首段是 base64url 编码的
        // 工作区根，其后是文件在该根下的真实相对路径，目录结构因此完整存在
        // 于 URL 里，浏览器自行拼接即命中本端点。详见 preview_asset 的说明。
        .route("/api/fs/preview/{root}/{*file}", get(preview_asset))
}

/// 目录选择器 seam 的能力决策(抄 dsh directory-picker-auto):
/// 远程绑定/SSH/远程连接来客 → browse;win/mac → native;linux 有显示+zenity
/// → native;其余 → browse(到处能用)。消费端遇到未知 kind 的默认行为是隐藏入口。
async fn capability(
    State(state): State<Arc<AppState>>,
    peer: Option<Extension<crate::remote::guard::RemotePeer>>,
) -> impl IntoResponse {
    // 远程来客必须走 browse:原生选择器会在这台机器上弹出窗口 ——
    // 手机点"选目录"却在电脑屏幕上弹出对话框,是纯粹的错配。
    let from_remote = peer
        .map(|Extension(peer)| peer.via != crate::remote::Via::Local)
        .unwrap_or(false);
    let kind = if state.bound_remote
        || from_remote
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
    Json(json!({ "kind": kind, "remote": from_remote }))
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
/// path, or `null` when the user cancels. Windows shows the dialog in a
/// short-lived child process so the picker can take the foreground.
async fn pick_dir() -> Result<impl IntoResponse, ApiError> {
    let path = crate::native_folder_picker::pick_folder().await?;
    Ok(Json(json!({ "path": path })))
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
    std::fs::metadata(root)
        .ok()
        .and_then(|meta| meta.modified().ok())
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

/// 解析目录前缀(相对根):拒绝 `..` 逃逸、绝对路径段与 Windows 盘符段;
/// 目录缺失/不可读返回 None(调用方决定是回空列表还是报错)。
fn resolve_relative_directory(root: &Path, directory: &str) -> Option<PathBuf> {
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
    let Some(abs) = resolve_relative_directory(root, directory) else {
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
                items.push(MentionItem { path, kind: "file" });
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
    if kind == "directory" { 0 } else { 1 }
}

/* ---- 工作区文件树(右侧「工作区文件」面板) ----
 *
 * 与上面的 @ 提及候选**分工不同**,所以不复用同一条链路:
 * - 提及候选是"全树索引 + 模糊匹配",要一次给出任意深度的命中;
 * - 文件树是**逐层懒加载**:展开哪层列哪层,首屏只读根目录一次。
 *   大仓库(几万文件)不可能整树灌进浏览器,而懒加载的代价是展开时才读盘,
 *   正好与"用户看哪展开哪"的节奏一致。
 *
 * 与 Git 无关:直接读文件系统,不查 git 追踪状态 —— 未跟踪的文件同样要在
 * 树里可见(这正是"不是 Git 工作区"的含义)。
 */

/// 单层列举上限:超大目录(如 `node_modules`)截断并显式告知,不静默丢弃。
const TREE_MAX_ENTRIES: usize = 2000;

#[derive(Debug, serde::Serialize)]
struct TreeEntry {
    name: String,
    /// 相对根目录的路径(以 `/` 分隔);拖拽引用与 `@` 提及共用同一坐标。
    path: String,
    kind: &'static str,
}

#[derive(Debug, serde::Serialize)]
struct TreeListing {
    /// 根目录绝对路径(回显,便于前端确认自己在哪棵树上)。
    root: String,
    /// 本次列举的目录(相对根,空串 = 根)。
    dir: String,
    entries: Vec<TreeEntry>,
    truncated: bool,
}

#[derive(Debug, Deserialize)]
struct TreeQuery {
    path: String,
    #[serde(default)]
    dir: Option<String>,
}

/// 逐层列举工作区目录。fs 访问放 spawn_blocking,不挡异步运行时。
async fn list_tree(Query(query): Query<TreeQuery>) -> Result<impl IntoResponse, ApiError> {
    let inner = tokio::task::spawn_blocking(move || {
        tree_impl(query.path.as_str(), query.dir.as_deref().unwrap_or(""))
    })
    .await
    .map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "fs/tree-failed",
            e.to_string(),
        )
    })?;
    Ok(Json(inner?))
}

fn tree_impl(root_path: &str, raw_dir: &str) -> Result<TreeListing, ApiError> {
    let root = PathBuf::from(root_path.trim());
    if !root.is_dir() {
        return Err(ApiError::bad_request(
            "fs/tree-not-a-directory",
            format!("'{}' is not a directory", root.display()),
        ));
    }
    // 归一化:反斜杠统一为 `/`(前端一律用 `/`),容忍首尾斜杠。
    let dir = raw_dir.replace('\\', "/");
    let dir = dir.trim_matches('/').to_string();
    // 与提及候选的宽容(拿不到就当空列表)不同:树面板的目录来自它自己的
    // 展开动作,解析不出来只可能是路径非法或目录刚被删 —— 静默回空列表会
    // 让"点了没反应"这类 bug 藏起来,这里显式报错让前端能提示。
    let Some(abs) = resolve_relative_directory(&root, &dir) else {
        return Err(ApiError::bad_request(
            "fs/tree-bad-directory",
            format!("'{}' is not a directory under the root", raw_dir),
        ));
    };
    let read = std::fs::read_dir(&abs).map_err(|e| {
        ApiError::bad_request("fs/tree-unreadable", format!("cannot read directory: {e}"))
    })?;
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut entries: Vec<TreeEntry> = Vec::new();
    let mut truncated = false;
    for entry in read.flatten() {
        if entries.len() >= TREE_MAX_ENTRIES {
            truncated = true;
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // 符号链接等其他类型不收录(与提及索引一致,避免跟随链接绕出工作区)。
        let kind = if file_type.is_dir() {
            "directory"
        } else if file_type.is_file() {
            "file"
        } else {
            continue;
        };
        entries.push(TreeEntry {
            path: format!("{prefix}{name}"),
            name,
            kind,
        });
    }
    sort_tree_entries(&mut entries);
    Ok(TreeListing {
        root: root.to_string_lossy().to_string(),
        dir,
        entries,
        truncated,
    })
}

/// 目录优先,其次按名称大小写不敏感字典序(与提及候选同一套排序语义)。
fn sort_tree_entries(entries: &mut [TreeEntry]) {
    entries.sort_by(|left, right| {
        kind_rank(left.kind)
            .cmp(&kind_rank(right.kind))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
}

/* ---- 工作区文件内容读取(右侧「文件读取」标签页) ----
 *
 * 只读展示：路径一律相对工作区根解析，与文件树/拖拽引用共用同一套坐标
 * （见 `resolve_relative_directory`）。安全口径与树列举一致：拒 `..` 逃逸、
 * 拒绝对路径段与盘符段、不跟随符号链接（避免绕出工作区）。
 */

/// 单文件读取上限：超过则明确拒绝而不是把几十 MB 基磊塞给浏览器。
const FILE_MAX_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FileView {
    /// 相对根目录的路径（以 `/` 分隔），前端标签以此作去重键。
    path: String,
    /// 文件短名（标签与标题用）。
    name: String,
    /// 文件正文。非 UTF-8（二进制）不走这里，直接报错。
    content: String,
    size: u64,
    /// 语言提示（由扩展名推得），前端语法高亮用；推不出为 null。
    language: Option<String>,
    modified_ms: u64,
    /// 行数：前端行号栏与「已截断」提示需要。
    lines: u64,
}

#[derive(Debug, Deserialize)]
struct FileQuery {
    /// 工作区根目录绝对路径。
    path: String,
    /// 相对根的路径（以 `/` 分隔）。
    file: String,
}

/// 读取工作区内一个文本文件。
///
/// 失败形态各自给码，前端据此给出不同提示：
/// - 400 路径非法（逃逸/绝对/盘符）、目标是目录、不是 UTF-8 文本；
/// - 404 文件不存在（含读取期被删）；
/// - 413 超过读取上限。
async fn read_workspace_file(
    Query(query): Query<FileQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let inner = tokio::task::spawn_blocking(move || {
        read_file_impl(query.path.as_str(), query.file.as_str())
    })
    .await
    .map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "fs/file-failed",
            e.to_string(),
        )
    })?;
    Ok(Json(inner?))
}

/// 预览资源（文件阅读器的 HTML 预览用）：把工作区里的文件按**浏览器资源**
/// 返回，而不是文本视图。
///
/// # 为什么不复用 `/api/fs/file`
///
/// HTML 预览要能解析同目录的 `./style.css`、`../img/a.png` 这类相对引用，
/// 所以 iframe 拿到的是一个**路径式 URL**（`/api/fs/preview?path=…&file=…`），
/// 浏览器据此自行拼出兄弟资源的地址；`srcDoc` 做不到这一点（它没有基址）。
/// 相对引用要能解析，iframe 就必须与 Denia 同源，于是这个端点的安全边界
/// 完全落在下面几处，缺一不可：
///
/// 1. **路径校验复用** [`resolve_relative_file`] + 根前缀复查 + 拒绝符号链接，
///    与 `/api/fs/file` 同一套口径（读不到工作区外的任何字节）。
/// 2. **只放行扩展名白名单**的静态资源，且**永不返回可执行类型**
///    （`.js`/`.mjs`/`.wasm`/`.php` 一律 404）：预览页引不到能跑的脚本。
/// 3. **HTML 走严格 CSP**（见 [`PREVIEW_CSP`]），`script-src 'none'`、
///    `object-src 'none'`，文档无法执行脚本、无法外发数据。
///
/// 配合前端 `<iframe sandbox>`（不给 `allow-scripts`），即使文档里带
/// `<script>` 也不会执行。各层互相独立，任一层失效仍不会读到工作区外的东西。
async fn preview_asset(
    axum::extract::Path((root_token, file)): axum::extract::Path<(String, String)>,
) -> Result<axum::response::Response, ApiError> {
    tokio::task::spawn_blocking(move || {
        // 首段是 base64url 编码的工作区根。解码失败直接拒，不做猜测。
        let Some(root_text) = decode_workspace_token(&root_token) else {
            return Err(ApiError::bad_request(
                "fs/preview-bad-root",
                "预览地址里的工作区标识无法解码",
            ));
        };
        let root = PathBuf::from(&root_text);
        if !root.is_dir() {
            return Err(ApiError::bad_request(
                "fs/preview-not-a-directory",
                format!("'{}' is not a directory", root.display()),
            ));
        }
        let rel = file.trim().trim_end_matches('/').to_string();
        let Some(abs) = resolve_relative_file(&root, &rel) else {
            return Err(ApiError::bad_request(
                "fs/preview-bad-path",
                format!("'{rel}' is not a readable file under the workspace root"),
            ));
        };
        if !abs.starts_with(&root) {
            return Err(ApiError::bad_request(
                "fs/preview-bad-path",
                format!("'{rel}' is not a readable file under the workspace root"),
            ));
        }
        let metadata =
            std::fs::symlink_metadata(&abs).map_err(|error| file_io_error(error, &rel))?;
        if metadata.file_type().is_symlink() {
            return Err(ApiError::bad_request(
                "fs/preview-symlink",
                format!("'{rel}' 是符号链接，不跟随打开"),
            ));
        }
        if metadata.is_dir() {
            return Err(ApiError::bad_request(
                "fs/preview-is-directory",
                format!("'{rel}' 是一个目录，无法作为预览资源"),
            ));
        }
        // 资源类型先按扩展名定；定不出来的（无扩展名的文件、约定文件名）不作为预览资源。
        let Some(content_type) = preview_content_type(&abs) else {
            return Err(ApiError::new(
                axum::http::StatusCode::NOT_FOUND,
                "fs/preview-unsupported-type",
                format!("'{rel}' 不是可预览的资源类型"),
            ));
        };
        // 体积上限比文本读取宽松些（图/字体/CSS 本来就大），但仍要有界。
        if metadata.len() > PREVIEW_MAX_BYTES {
            return Err(ApiError::new(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "fs/preview-too-large",
                format!(
                    "资源 {} MiB，超过 {} MiB 的预览上限",
                    metadata.len() / (1024 * 1024),
                    PREVIEW_MAX_BYTES / (1024 * 1024)
                ),
            ));
        }
        let bytes = std::fs::read(&abs).map_err(|error| file_io_error(error, &rel))?;

        if content_type == "text/html; charset=utf-8" {
            return Ok(preview_html_response(&bytes));
        }
        let mut response = axum::response::Response::new(axum::body::Body::from(bytes));
        response.headers_mut().insert(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static(content_type),
        );
        // 静态资源同样锁死 CSP：一张 SVG 也能内嵌脚本。
        response.headers_mut().insert(
            axum::http::header::CONTENT_SECURITY_POLICY,
            axum::http::HeaderValue::from_static(PREVIEW_CSP),
        );
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
        Ok(response)
    })
    .await
    .map_err(|e| {
        ApiError::new(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "fs/preview-failed",
            e.to_string(),
        )
    })?
}

/// 预览用的 CSP。
///
/// `script-src 'none'` 是这行的全部意义：HTML 与 SVG 里任何形式的脚本
/// （内联、外链、`javascript:` URL、事件属性）都被挡下。`connect-src 'none'`
/// 与 `form-action 'none'` 断掉外发通道，文档无法把工作区内容 POST 出去。
/// `img-src`/`style-src`/`font-src` 保留，且允许 `data:`——预览页要能显示
/// 内联样式与 data URI 图片。`default-src 'none'` 兜住其余所有类型。
const PREVIEW_CSP: &str = "default-src 'none'; script-src 'none'; object-src 'none'; \
form-action 'none'; connect-src 'none'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; \
font-src 'self' data:";

/// 预览资源体积上限（8 MiB），比文本读取的 [`FILE_MAX_BYTES`] 宽，
/// 因为图片与字体天然比源码大。
const PREVIEW_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// 工作区根 → URL 首段（base64url，无 padding）。
///
/// 用 base64url 是因为工作区路径里有 `\`、`:`、空格等在 URL 路径里必须转义
/// 的字符，而转义后的形式还要再解码回来才拿得到原路径。base64url 的字母表
/// （`A-Za-z0-9-_`）本身就是 URL 安全字符集，无需转义，前端也能用
/// `btoa` 之外的手段复现。
pub fn encode_workspace_token(root: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(root.as_bytes())
}

/// URL 首段 → 工作区根。解码失败或不是合法 UTF-8 返回 `None`（调用方 400）。
fn decode_workspace_token(token: &str) -> Option<String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(token.as_bytes())
        .ok()?;
    String::from_utf8(bytes).ok()
}

/// HTML 文档的响应：逐字节回显用户文件（预览就是给浏览器解析它），
/// 但带上严格 CSP 与 no-store。
///
/// **不改写文档**：相对引用由路径式 URL 天然解析 ——
/// `/api/fs/preview/{wsId}/src/a/index.html` 里的 `./style.css` 会解析成
/// `/api/fs/preview/{wsId}/src/a/style.css`，目录结构在 URL 路径里，
/// 浏览器自行拼接。曾试过注入 `<base href>` 指向查询串，**无效**：查询串
/// 不参与路径拼接，`/api/fs/preview` + `./x.css` 仍是 `/api/fs/x.css`。
///
/// 刻意**不加**响应头 `sandbox` 指令——那会让文档连同它的相对资源一起被
/// 当成不透明源处理，正是前端 iframe sandbox 已经在做的事；服务端再加一层
/// 会让同源相对引用失效。
fn preview_html_response(bytes: &[u8]) -> axum::response::Response {
    let mut response = axum::response::Response::new(axum::body::Body::from(bytes.to_vec()));
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response.headers_mut().insert(
        axum::http::header::CONTENT_SECURITY_POLICY,
        axum::http::HeaderValue::from_static(PREVIEW_CSP),
    );
    // 预览是工作区当前状态的一次快照；缓存住会让用户改了文件却看不到变化。
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

/// 扩展名 → 预览用的 MIME。**白名单**，且不含任何可执行类型：
/// `.js`/`.mjs`/`.cjs`/`.wasm`/`.php`/`.py` 之类一律不出现，
/// 预览页因此永远拿不到可运行的脚本源。
fn preview_content_type(abs: &Path) -> Option<&'static str> {
    let name = abs.file_name()?.to_str()?;
    // 约定文件名（`Dockerfile`、`Makefile`、`.env`）在预览里没有意义，直接拒。
    if basename_hint(name).is_some() {
        return None;
    }
    let ext = name.rsplit_once('.')?.1.to_lowercase();
    Some(match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "txt" | "md" | "markdown" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "pdf" => "application/pdf",
        _ => return None,
    })
}

fn read_file_impl(root_path: &str, raw_file: &str) -> Result<FileView, ApiError> {
    let root = PathBuf::from(root_path.trim());
    if !root.is_dir() {
        return Err(ApiError::bad_request(
            "fs/file-not-a-directory",
            format!("'{}' is not a directory", root.display()),
        ));
    }
    let rel = raw_file.replace('\\', "/");
    // 只在首尾斜杠上做**去尾**归一（前端可能给 `src/main.rs/` 这种拖拽残留），
    // 但**绝不**把开头的 `/` 抹掉：那正是绝对路径的标记，抹了就变成相对路径，
    // `/etc/passwd` 会被当成工作区内的 `etc/passwd` —— 实测就是这样漏过校验的。
    let rel = rel.trim_end_matches('/').to_string();
    if rel.is_empty() || rel.trim().is_empty() {
        return Err(ApiError::bad_request(
            "fs/file-bad-path",
            "file path is empty",
        ));
    }
    // 复用目录解析的同一条路径校验：拒 `..`/绝对路径/盘符段。
    let Some(abs) = resolve_relative_file(&root, &rel) else {
        return Err(ApiError::bad_request(
            "fs/file-bad-path",
            format!("'{raw_file}' is not a readable file under the workspace root"),
        ));
    };
    // 解析结果必须仍在根内（双保险：即使将来有人放松了 `resolve_relative_file`
    // 的段校验，越界仍会在这里被拦住）。
    if !abs.starts_with(&root) {
        return Err(ApiError::bad_request(
            "fs/file-bad-path",
            format!("'{raw_file}' is not a readable file under the workspace root"),
        ));
    }
    let metadata = std::fs::symlink_metadata(&abs).map_err(|error| file_io_error(error, &rel))?;
    // 符号链接一律拒绝：树与提及索引都不 follow 链接，读取也不应该例外 ——
    // 否则一个指向工作区外的链接就能把根约束绕过去。
    if metadata.file_type().is_symlink() {
        return Err(ApiError::bad_request(
            "fs/file-symlink",
            format!("'{rel}' 是符号链接，不跟随打开"),
        ));
    }
    if metadata.is_dir() {
        return Err(ApiError::bad_request(
            "fs/file-is-directory",
            format!("'{rel}' 是一个目录，无法作为文件打开"),
        ));
    }
    if metadata.len() > FILE_MAX_BYTES {
        return Err(ApiError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "fs/file-too-large",
            format!(
                "文件 {} MiB，超过 {} MiB 的读取上限",
                metadata.len() / (1024 * 1024),
                FILE_MAX_BYTES / (1024 * 1024)
            ),
        ));
    }
    let bytes = std::fs::read(&abs).map_err(|error| file_io_error(error, &rel))?;
    // 二进制文件拒绝：不能当作文本“尽力而为”地展示 —— 那只会得到一屏乱码。
    // 判定口径：UTF-8 解码失败或含 NUL 字节（文本文件不会有 NUL）。
    let content = match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => text,
        Ok(_) => {
            return Err(ApiError::bad_request(
                "fs/file-binary",
                format!("'{rel}' 看起来是二进制文件，无法以文本方式打开"),
            ));
        }
        Err(_) => {
            return Err(ApiError::bad_request(
                "fs/file-not-utf8",
                format!("'{rel}' 不是 UTF-8 文本文件，无法以文本方式打开"),
            ));
        }
    };
    let name = abs
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| rel.clone());
    let lines = if content.is_empty() {
        0
    } else {
        content.lines().count() as u64
    };
    Ok(FileView {
        language: language_hint(&name).map(str::to_string),
        path: rel,
        name,
        content,
        size: metadata.len(),
        modified_ms: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        lines,
    })
}

/// 解析相对根的文件路径。与 [`resolve_relative_directory`] 同一套拒绝口径，
/// 但**不**要求目标是目录（也不要求此刻存在 —— 不存在交给调用方报 404）。
fn resolve_relative_file(root: &Path, relative: &str) -> Option<PathBuf> {
    // 纯空白不是合法路径：它会被下面的分段循环全部跳过，
    // 解析成 root 自身，报出一个误导的“是目录”错误。
    if relative.trim().is_empty() {
        return None;
    }
    if relative.starts_with('/') {
        return None;
    }
    let mut abs = root.to_path_buf();
    for segment in relative.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." || segment.contains(':') {
            return None;
        }
        abs.push(segment);
    }
    Some(abs)
}

fn file_io_error(error: std::io::Error, rel: &str) -> ApiError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ApiError::new(
            axum::http::StatusCode::NOT_FOUND,
            "fs/file-not-found",
            format!("'{rel}' 不存在（可能已被删除或重命名）"),
        )
    } else if error.kind() == std::io::ErrorKind::PermissionDenied {
        ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "fs/file-denied",
            format!("没有读取 '{rel}' 的权限"),
        )
    } else {
        ApiError::bad_request("fs/file-read-failed", format!("读取 '{rel}' 失败：{error}"))
    }
}

/// 由扩展名（或无扩展名的约定文件名）推语言提示（语法高亮用）。推不出返回 None，前端按纯文本渲染。
///
/// 返回的每个标签都必须能在前端 `web/src/markdown/highlight.ts` 的
/// `LANG_ALIASES` 里解析到语法，否则该扩展名在文件阅读器里会退化成纯文本
/// ——而同样的语言在对话流的代码块里是高亮的。扩展名缺失的约定文件
/// （`Dockerfile`、`Makefile`）走 `basename_hint`，否则 `rsplit_once('.')`
/// 永远推不出它们。
fn language_hint(name: &str) -> Option<&'static str> {
    if let Some(hint) = basename_hint(name) {
        return Some(hint);
    }
    let ext = name.rsplit_once('.')?.1.to_lowercase();
    let language = match ext.as_str() {
        "rs" => "rust",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "json" | "jsonc" => "json",
        "md" | "markdown" => "markdown",
        "py" | "pyi" => "python",
        "go" => "go",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "rb" => "ruby",
        "php" => "php",
        "sh" | "bash" | "zsh" => "bash",
        "ps1" | "psm1" | "psd1" => "powershell",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "xml" | "xsd" | "xsl" => "xml",
        "html" | "htm" => "html",
        "css" => "css",
        "scss" => "scss",
        "less" => "less",
        "sql" => "sql",
        "kt" | "kts" => "kotlin",
        "swift" => "swift",
        "lua" => "lua",
        "r" => "r",
        "dart" => "dart",
        "vue" => "vue",
        "svelte" => "svelte",
        "proto" => "protobuf",
        "graphql" | "gql" => "graphql",
        "ini" | "cfg" | "conf" => "ini",
        "properties" => "properties",
        "prisma" => "prisma",
        "diff" | "patch" => "diff",
        "env" => "dotenv",
        "vim" => "vim",
        _ => return None,
    };
    Some(language)
}

/// 无扩展名或扩展名不参与识别的约定文件名 → 语言（`language_hint` 的前置分支）。
///
/// 覆盖两类：`Dockerfile` / `Makefile` 这类本身没有扩展名的构建文件，
/// 以及 `docker-compose.yml` / `xdebug.ini` 这类靠固定前缀才能认出的文件
/// ——后者带扩展名，若只按扩展名判断会落到 `yaml`/`ini` 之外或直接推不出。
/// 大小写不敏感：Windows 与容器里 `Dockerfile`、`dockerfile` 都有。
fn basename_hint(name: &str) -> Option<&'static str> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    match base.to_ascii_lowercase().as_str() {
        "dockerfile" | "dockerfile.dev" | "dockerfile.prod" => Some("dockerfile"),
        "makefile" | "gnumakefile" | "makefile.am" | "makefile.in" => Some("makefile"),
        ".env" | ".env.local" | ".env.production" | ".env.development" | ".env.example" => {
            Some("dotenv")
        }
        _ => {
            if base.to_ascii_lowercase().starts_with("docker-compose")
                || base.to_ascii_lowercase().starts_with("compose.")
            {
                Some("yaml")
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod file_read_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-file-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// 断言用的错误码（ApiError 没有实现 Serialize，直接读字段）。
    fn err_code(error: &ApiError) -> String {
        error.code.clone()
    }

    /// 正常读取：正文、行数、语言提示、大小与相对路径都要对。
    #[test]
    fn reads_utf8_text_with_metadata() {
        let root = scratch("text");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n// 注释\n").unwrap();
        let view = read_file_impl(root.to_str().unwrap(), "src/main.rs").unwrap();
        assert_eq!(view.path, "src/main.rs");
        assert_eq!(view.name, "main.rs");
        assert_eq!(view.language.as_deref(), Some("rust"));
        assert_eq!(view.lines, 2);
        assert!(view.content.contains("fn main"));
        assert!(view.size > 0);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 反斜杠与尾部斜杠要容忍（Windows 前端/拖拽可能给不同形态）。
    /// 开头的 `/` **不**容忍：那是绝对路径的标记，必须拒。
    #[test]
    fn normalizes_path_separators() {
        let root = scratch("sep");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.txt"), "hello").unwrap();
        let root_str = root.to_str().unwrap();
        assert_eq!(
            read_file_impl(root_str, "src\\a.txt").unwrap().path,
            "src/a.txt"
        );
        assert_eq!(
            read_file_impl(root_str, "src/a.txt/").unwrap().path,
            "src/a.txt"
        );
        assert_eq!(
            err_code(&read_file_impl(root_str, "/src/a.txt").unwrap_err()),
            "fs/file-bad-path"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 逃逸、绝对路径、盘符段一律拒绝：根约束不能被绕过。
    #[test]
    fn rejects_path_escape() {
        let root = scratch("escape");
        std::fs::write(root.join("ok.txt"), "x").unwrap();
        for bad in [
            "../secret.txt",
            "a/../../x",
            "/etc/passwd",
            "C:/Windows/win.ini",
        ] {
            let error = read_file_impl(root.to_str().unwrap(), bad).unwrap_err();
            assert_eq!(err_code(&error), "fs/file-bad-path", "path {bad} 应当被拒");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// 空路径、目录、缺失、二进制各自给不同码，前端据此分别提示。
    #[test]
    fn failure_modes_are_distinguishable() {
        let root = scratch("fail");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("bin.dat"), [0x00, 0x01, 0x02, 0x03]).unwrap();
        std::fs::write(root.join("latin.txt"), [0xff, 0xfe, 0x41]).unwrap();
        let root_str = root.to_str().unwrap();

        assert_eq!(
            err_code(&read_file_impl(root_str, "  ").unwrap_err()),
            "fs/file-bad-path"
        );
        assert_eq!(
            err_code(&read_file_impl(root_str, "sub").unwrap_err()),
            "fs/file-is-directory"
        );
        assert_eq!(
            err_code(&read_file_impl(root_str, "nope.txt").unwrap_err()),
            "fs/file-not-found"
        );
        // 含 NUL → 二进制；非法 UTF-8 → 非 UTF-8。两者都不当文本展示。
        assert_eq!(
            err_code(&read_file_impl(root_str, "bin.dat").unwrap_err()),
            "fs/file-binary"
        );
        assert_eq!(
            err_code(&read_file_impl(root_str, "latin.txt").unwrap_err()),
            "fs/file-not-utf8"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 超限必须拒绝而不是把几十 MB 塞给浏览器。
    #[test]
    fn rejects_oversized_file() {
        let root = scratch("big");
        let big = root.join("big.txt");
        std::fs::write(&big, vec![b'a'; (FILE_MAX_BYTES + 1) as usize]).unwrap();
        let error = read_file_impl(root.to_str().unwrap(), "big.txt").unwrap_err();
        assert_eq!(err_code(&error), "fs/file-too-large");
        std::fs::remove_dir_all(&root).ok();
    }

    /// 根不是目录 → 显式报错（fail loud，不静默回空）。
    #[test]
    fn non_directory_root_is_rejected() {
        let root = scratch("rootfile");
        let file = root.join("plain.txt");
        std::fs::write(&file, "x").unwrap();
        let error = read_file_impl(file.to_str().unwrap(), "plain.txt").unwrap_err();
        assert_eq!(err_code(&error), "fs/file-not-a-directory");
        std::fs::remove_dir_all(&root).ok();
    }

    /// 扩展名推不出语言时为 None（前端按纯文本渲染，不报错）。
    #[test]
    fn unknown_extension_has_no_language_hint() {
        let root = scratch("lang");
        std::fs::write(root.join("notes.zzz"), "x").unwrap();
        assert!(
            read_file_impl(root.to_str().unwrap(), "notes.zzz")
                .unwrap()
                .language
                .is_none()
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 每个 `language_hint` 可能输出的标签都取样一遍。
    ///
    /// 这是一条**跨语言契约**：这里的每个断言值都必须能在前端
    /// `web/src/markdown/highlight.ts` 的 `LANG_ALIASES` 里解析到语法。
    /// 前端漏一个别名，对应扩展名就会在文件阅读器里静默退化成纯文本
    /// （而同样的语言在对话流代码块里是正常高亮的），只能靠这个测试拦住。
    #[test]
    fn every_emitted_language_tag_is_covered() {
        // 覆盖每个 match 分支，外加约定文件名分支。
        for (name, expected) in [
            ("a.rs", "rust"),
            ("a.ts", "typescript"),
            ("a.tsx", "typescript"),
            ("a.jsx", "javascript"),
            ("a.json", "json"),
            ("a.md", "markdown"),
            ("a.py", "python"),
            ("a.go", "go"),
            ("a.java", "java"),
            ("a.h", "c"),
            ("a.hpp", "cpp"),
            ("a.cs", "csharp"),
            ("a.rb", "ruby"),
            ("a.php", "php"),
            ("a.sh", "bash"),
            ("a.ps1", "powershell"),
            ("a.toml", "toml"),
            ("a.yml", "yaml"),
            ("a.xsd", "xml"),
            ("a.htm", "html"),
            ("a.css", "css"),
            ("a.scss", "scss"),
            ("a.less", "less"),
            ("a.sql", "sql"),
            ("a.kt", "kotlin"),
            ("a.swift", "swift"),
            ("a.lua", "lua"),
            ("a.r", "r"),
            ("a.dart", "dart"),
            ("a.vue", "vue"),
            ("a.svelte", "svelte"),
            ("a.proto", "protobuf"),
            ("a.gql", "graphql"),
            ("a.ini", "ini"),
            ("a.properties", "properties"),
            ("a.prisma", "prisma"),
            ("a.patch", "diff"),
            ("a.env", "dotenv"),
            ("a.vim", "vim"),
            // 无扩展名 / 靠前缀识别的约定文件。
            ("Dockerfile", "dockerfile"),
            ("dockerfile", "dockerfile"),
            ("Dockerfile.prod", "dockerfile"),
            ("Makefile", "makefile"),
            ("GNUmakefile", "makefile"),
            (".env", "dotenv"),
            (".env.production", "dotenv"),
            ("docker-compose.yml", "yaml"),
            ("compose.yaml", "yaml"),
            // 带目录前缀时只看基名。
            ("src/nested/Dockerfile", "dockerfile"),
        ] {
            assert_eq!(
                language_hint(name),
                Some(expected),
                "文件 {name} 的语言提示"
            );
        }
    }

    /// `.less` 必须是独立语法，不能并进 `scss`（两者关键字集不同）。
    #[test]
    fn less_is_not_folded_into_scss() {
        assert_eq!(language_hint("theme.less"), Some("less"));
        assert_ne!(language_hint("theme.less"), language_hint("theme.scss"));
    }

    /// 无扩展名的约定文件走 `basename_hint`；`name` 只有基名部分时也要正确。
    #[test]
    fn basename_hints_ignore_directories() {
        assert_eq!(language_hint("Dockerfile"), Some("dockerfile"));
        assert_eq!(language_hint("web\\Dockerfile"), Some("dockerfile"));
        assert_eq!(language_hint("a/b/c/Makefile"), Some("makefile"));
        // 普通扩展名文件不受 basename 分支影响。
        assert_eq!(language_hint("a/b/c/main.rs"), Some("rust"));
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    use axum::http::HeaderValue;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-preview-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    /// 响应里的 CSP 必须禁掉脚本，并且不含可执行类型。
    fn csp_of(response: &axum::response::Response) -> String {
        response
            .headers()
            .get(axum::http::header::CONTENT_SECURITY_POLICY)
            .and_then(|value: &HeaderValue| value.to_str().ok())
            .unwrap_or_default()
            .to_string()
    }

    /// HTML 预览页必须带 `script-src 'none'`。
    ///
    /// 这是预览安全的第一道服务端防线：即使前端 iframe 少配了一个
    /// `sandbox` 属性，文档里的 `<script>` 也不会执行。
    #[test]
    fn html_response_carries_script_blocking_csp() {
        let response = preview_html_response(b"<script>alert(1)</script>");
        let csp = csp_of(&response);
        assert!(csp.contains("script-src 'none'"), "CSP 必须禁脚本: {csp}");
        assert!(
            csp.contains("object-src 'none'"),
            "CSP 必须禁 object: {csp}"
        );
        // 外发通道也要断，文档不能把工作区内容发出去。
        assert!(
            csp.contains("connect-src 'none'"),
            "CSP 必须禁 connect: {csp}"
        );
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "text/html; charset=utf-8"
        );
        // 预览是即时快照，不许缓存。
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CACHE_CONTROL)
                .unwrap(),
            "no-store"
        );
    }

    /// 白名单里**不能有任何可执行类型**。
    ///
    /// 这是第二道防线：即使 CSP 配漏了，预览页也拿不到能跑的脚本源。
    #[test]
    fn preview_whitelist_excludes_executable_types() {
        for ext in [
            "js", "mjs", "cjs", "jsx", "ts", "tsx", "wasm", "php", "py", "rb", "sh", "exe", "dll",
            "jar", "ps1",
        ] {
            let path = PathBuf::from(format!("a.{ext}"));
            assert_eq!(
                preview_content_type(&path),
                None,
                ".{ext} 不应作为预览资源返回"
            );
        }
    }

    /// 白名单内的静态资源与图片要有正确 MIME。
    #[test]
    fn preview_whitelist_covers_static_assets() {
        for (name, expected) in [
            ("a.html", "text/html; charset=utf-8"),
            ("a.htm", "text/html; charset=utf-8"),
            ("a.css", "text/css; charset=utf-8"),
            ("a.svg", "image/svg+xml"),
            ("a.png", "image/png"),
            ("a.jpg", "image/jpeg"),
            ("a.woff2", "font/woff2"),
            ("a.md", "text/plain; charset=utf-8"),
        ] {
            assert_eq!(
                preview_content_type(Path::new(name)),
                Some(expected),
                "{name} 的 MIME"
            );
        }
    }

    /// 约定文件名（无扩展名）不作为预览资源。
    #[test]
    fn preview_rejects_basename_only_files() {
        for name in ["Dockerfile", "Makefile", ".env"] {
            assert_eq!(
                preview_content_type(Path::new(name)),
                None,
                "{name} 不应作为预览资源"
            );
        }
    }

    /// 路径逃逸与绝对路径必须被拒 —— 预览端点不得比文本读取更宽松。
    #[test]
    fn preview_path_escape_is_rejected() {
        let root = scratch("escape");
        std::fs::write(root.join("a.html"), "<p>ok</p>").unwrap();
        let root_str = root.to_str().unwrap();
        for bad in [
            "../outside.html",
            "a/../../outside.html",
            "/etc/passwd",
            "C:/Windows/win.ini",
        ] {
            assert!(
                resolve_relative_file(Path::new(root_str), bad).is_none()
                    || !resolve_relative_file(Path::new(root_str), bad)
                        .unwrap()
                        .starts_with(root_str),
                "{bad} 不该解析到工作区内"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// 路径里的 `..` 段与绝对路径必须被拒（与文本读取同一套口径）。
    ///
    /// 路径式 URL 把用户可控的片段拼进了 URL，因此这里比查询串版本更需要
    /// 逐段校验 —— `%2e%2e` 解码后同样是 `..`。
    #[test]
    fn preview_path_segments_cannot_escape() {
        let root = Path::new("/ws");
        for bad in [
            "../../../etc/passwd",
            "src/../../outside.css",
            "/absolute.css",
            "C:/Windows/win.ini",
            "a:b",
        ] {
            assert!(
                resolve_relative_file(root, bad).is_none(),
                "{bad} 不该解析出路径"
            );
        }
        // 正常相对路径仍要通过
        assert!(resolve_relative_file(root, "src/a/style.css").is_some());
    }

    /// 工作区 token 必须能无损往返 —— 前端按同一套算法拼 URL。
    ///
    /// 路径含 Windows 反斜杠、盘符冒号与空格，这些在 URL 路径里都得转义，
    /// 所以走 base64url。往返不保真会让预览读错根目录（静默 404）。
    #[test]
    fn workspace_token_round_trips() {
        for root in [
            r"D:\my proj",
            "D:/Denia",
            "/home/me/项目",
            r"C:\Users\a b\c-d_e",
        ] {
            let token = encode_workspace_token(root);
            // base64url 字母表必须全是 URL 安全字符，不能出现需要转义的。
            assert!(
                token
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{token} 含 URL 不安全字符"
            );
            assert_eq!(
                decode_workspace_token(&token).as_deref(),
                Some(root),
                "{root} 往返失败"
            );
        }
    }

    /// 非法 token 必须被拒，而不是解出垃圾路径。
    ///
    /// 空串不在此列：它是**合法的** base64（解出空串），随后会被
    /// `root.is_dir()` 判否 —— 空路径不是目录，第一层就挡掉了。
    #[test]
    fn workspace_token_rejects_garbage() {
        for bad in ["!!!!", "not-valid-base64!!", "a", "////"] {
            assert!(decode_workspace_token(bad).is_none(), "{bad} 不该解出内容");
        }
        // 空 token 解出空根，不是目录，同样会被上层拒绝。
        assert_eq!(decode_workspace_token("").as_deref(), Some(""));
    }
}

#[cfg(test)]
mod tree_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-tree-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn names(listing: &TreeListing) -> Vec<&str> {
        listing
            .entries
            .iter()
            .map(|item| item.name.as_str())
            .collect()
    }

    #[test]
    fn root_listing_puts_directories_first() {
        let root = scratch("root");
        std::fs::create_dir(root.join("zeta")).unwrap();
        std::fs::create_dir(root.join("alpha")).unwrap();
        std::fs::write(root.join("beta.txt"), "x").unwrap();
        let listing = tree_impl(root.to_str().unwrap(), "").unwrap();
        assert_eq!(names(&listing), vec!["alpha", "zeta", "beta.txt"]);
        assert_eq!(listing.entries[0].kind, "directory");
        assert_eq!(listing.entries[2].path, "beta.txt");
        assert!(!listing.truncated);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 隐藏文件与 `node_modules` 这类目录**照常列出**:这是文件浏览器,
    /// 不是 Git 工作区,也不套用提及候选的排除集。
    #[test]
    fn hidden_and_heavy_directories_are_listed() {
        let root = scratch("hidden");
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join(".env"), "").unwrap();
        let listing = tree_impl(root.to_str().unwrap(), "").unwrap();
        assert_eq!(names(&listing), vec!["node_modules", ".env"]);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn subdirectory_listing_uses_relative_paths() {
        let root = scratch("sub");
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        // 尾斜杠与反斜杠都要能容忍(前端与 Windows 各自可能给不同形态)。
        let listing = tree_impl(root.to_str().unwrap(), "src/").unwrap();
        assert_eq!(listing.dir, "src");
        assert_eq!(names(&listing), vec!["nested", "main.rs"]);
        assert_eq!(listing.entries[1].path, "src/main.rs");
        let backslash = tree_impl(root.to_str().unwrap(), "src\\").unwrap();
        assert_eq!(names(&backslash), vec!["nested", "main.rs"]);
        std::fs::remove_dir_all(&root).ok();
    }

    /// 逃逸与不存在都必须**报错**而不是回空列表:树面板靠这个区分
    /// "目录是空的"与"路径有问题"。
    #[test]
    fn escape_and_missing_directory_are_errors() {
        let root = scratch("escape");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        assert!(tree_impl(root.to_str().unwrap(), "..").is_err());
        assert!(tree_impl(root.to_str().unwrap(), "../").is_err());
        assert!(tree_impl(root.to_str().unwrap(), "a/../..").is_err());
        assert!(tree_impl(root.to_str().unwrap(), "nope").is_err());
        // 绝对路径同样拒绝(否则根约束形同虚设)。
        assert!(tree_impl(root.to_str().unwrap(), "/etc").is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn non_directory_root_is_bad_request() {
        let root = scratch("file-root");
        let file = root.join("plain.txt");
        std::fs::write(&file, "").unwrap();
        assert!(tree_impl(file.to_str().unwrap(), "").is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn oversized_directory_is_truncated_loudly() {
        let root = scratch("big");
        for index in 0..(TREE_MAX_ENTRIES + 5) {
            std::fs::write(root.join(format!("f{index:05}.txt")), "").unwrap();
        }
        let listing = tree_impl(root.to_str().unwrap(), "").unwrap();
        assert_eq!(listing.entries.len(), TREE_MAX_ENTRIES);
        assert!(listing.truncated);
        std::fs::remove_dir_all(&root).ok();
    }
}

#[cfg(test)]
mod mention_tests {
    use super::*;

    /// 一次性测试目录(按 name 隔离,启动时清残留),结束时清理。
    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("denia-mentions-{}-{name}", std::process::id()));
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
        assert!(
            mentions_impl(root.to_str().unwrap(), "../")
                .unwrap()
                .is_empty()
        );
        assert!(
            mentions_impl(root.to_str().unwrap(), "../..")
                .unwrap()
                .is_empty()
        );
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
