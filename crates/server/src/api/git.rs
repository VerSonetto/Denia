//! 审查面板 API:Git 状态与文件 Diff。
//!
//! # 与 ZCode 的对应关系
//!
//! ZCode 的审查面板有四个「来源」(source):
//! `unstaged` / `staged` / `branch` / `last-turn`。这里实现前三个 ——
//! `last-turn` 在 ZCode 里由**会话轮次快照**派生(不是 git 概念),
//! denia 的对应物是 `file_history`/checkpoints,走另一条 API,不混进来。
//!
//! # 为什么不用 libgit2 / git2-rs
//!
//! 直接调 `git` 可执行文件有三个实际好处:
//! - **零编译成本**:`git2` 会把 libgit2(C 代码)拉进构建,增量编译明显变慢;
//! - **行为与用户在终端看到的一致**:用户 `git status` 看到什么,面板就显示
//!   什么。libgit2 对 `.gitignore` 边缘语法、submodule、sparse-checkout 的
//!   处理与官方 git 存在差异,会出现"终端里没有的文件面板里有";
//! - **配置天然生效**:`core.quotepath`、`diff.external`、`core.autocrlf`
//!   等用户配置不用自己重新实现一遍。
//!
//! 代价是每次请求起一个进程。缓解办法:状态查询合并成**一次** `git status`
//! 调用(而不是每个文件问一次),Diff 按需懒加载(前端只在展开文件时请求)。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use crate::error::ApiError;
use crate::state::AppState;

/// 单次 git 调用的超时:防止超大仓库把请求挂死。
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Diff 内容上限:超过就只回摘要(前端降级为"已裁剪"提示)。
///
/// 与 ZCode 的 `mOt = 180_000` 同量级:大文件全量塞进 JSON 会让前端
/// 序列化/渲染卡住,不如让前端知道"太大了,只给你摘要"。
const DIFF_CHAR_LIMIT: usize = 180_000;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/git/status", get(status))
        .route("/api/git/diff", get(diff))
}

fn error(message: String) -> ApiError {
    ApiError::bad_request("git/operation-failed", message)
}

#[derive(Deserialize)]
struct PathQuery {
    /// 工作区绝对路径。
    path: String,
}

/// 在指定目录跑一条 git 命令,返回 stdout(已按 UTF-8 宽松解码)。
///
/// 退出码非 0 **不一定**是错误:`git diff --quiet` 用 1 表示"有差异",
/// `git rev-parse` 在非仓库里返回 128。调用方按需判定,所以这里只回
/// `(success, stdout, stderr)`,不在这里判错。
///
/// ## 参数顺序是有讲究的
///
/// `git -c key=value <subcommand>` 里的 `-c` **必须排在子命令之前**。
/// 早先这里把 `.arg("-c")` 写在了 `.args(args)` 之后,实际发出的是
/// `git rev-parse --show-toplevel -c core.quotepath=false` —— git 直接
/// 报错退出,而调用方只看到"退出码非 0",于是**每个工作区都被判成
/// "不是 Git 仓库"**。这类"参数顺序错导致静默降级"的坑不会报错,只会
/// 让功能整体失效,所以顺序在这里显式写明。
async fn run_git(cwd: &Path, args: &[&str]) -> Result<(bool, String, String), String> {
    let mut command = Command::new("git");
    command
        // `core.quotepath` 默认会把非 ASCII 路径转义成 `\344\275\240`,
        // 中文文件名会显示成乱码。关掉它,让 git 直接输出 UTF-8。
        // **必须在子命令之前**。
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // 强制 C 语言与无分页:本地化输出会破坏解析,分页器会把进程挂住。
        .env("LC_ALL", "C")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0");

    let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
        .await
        .map_err(|_| "git 命令超时".to_string())?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "未找到 git 可执行文件,请先安装 Git 并确保在 PATH 中".to_string()
            } else {
                format!("执行 git 失败:{e}")
            }
        })?;

    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/// 解析 `git status --porcelain=v1 -z` 的输出。
///
/// 用 `-z` 而不是默认格式:默认格式会 C 风格转义路径(引号+反斜杠),
/// 解析要自己实现一遍反转义;`-z` 用 NUL 分隔且**不转义**,直接按字节切。
///
/// 重命名/复制的记录是**两条** NUL 项(`R  new\0old\0`),必须成对消费,
/// 否则后续所有条目都会错位。
fn parse_status(raw: &[u8]) -> Vec<Value> {
    let mut entries = Vec::new();
    let mut fields = raw.split(|byte| *byte == 0);
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            continue;
        }
        let index_status = field[0] as char;
        let worktree_status = field[1] as char;
        // field[2] 是空格分隔符,路径从 3 开始。
        let path = String::from_utf8_lossy(&field[3..]).into_owned();
        // 重命名/复制:紧跟一条原路径。
        let renamed_from = if index_status == 'R' || index_status == 'C' {
            fields
                .next()
                .map(|origin| String::from_utf8_lossy(origin).into_owned())
        } else {
            None
        };

        let staged = index_status != ' ' && index_status != '?';
        let unstaged = worktree_status != ' ' && worktree_status != '?';
        let untracked = index_status == '?' && worktree_status == '?';
        let conflicted = index_status == 'U'
            || worktree_status == 'U'
            || (index_status == 'A' && worktree_status == 'A')
            || (index_status == 'D' && worktree_status == 'D');

        entries.push(json!({
            "path": path,
            "indexStatus": index_status.to_string(),
            "worktreeStatus": worktree_status.to_string(),
            "staged": staged,
            "unstaged": unstaged,
            "untracked": untracked,
            "conflicted": conflicted,
            "renamedFrom": renamed_from,
        }));
    }
    entries
}

/// GET /api/git/status?path=... — 仓库摘要 + 改动清单。
async fn status(
    State(_state): State<Arc<AppState>>,
    Query(query): Query<PathQuery>,
) -> Result<Json<Value>, ApiError> {
    let cwd = PathBuf::from(&query.path);
    if !cwd.is_dir() {
        return Err(error(format!("目录不存在:{}", cwd.display())));
    }

    // 是否在仓库里。`rev-parse --show-toplevel` 在非仓库里返回 128 并往
    // stderr 写 "not a git repository";**其他**失败(参数错、git 挂了)必须
    // 报错而不是降级成"不是仓库" —— 否则一个参数顺序错误会让整个面板
    // 静默变成空态,查起来毫无线索。
    let (in_repo, top_level, rev_err) = run_git(&cwd, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(error)?;
    if !in_repo {
        let stderr = rev_err.to_ascii_lowercase();
        let looks_like_not_a_repo = stderr.contains("not a git repository")
            || stderr.contains("not a repository")
            // 空目录 + 没有父仓库时,git 也可能只给一句 "fatal: ..."。
            || stderr.contains("fatal:");
        if looks_like_not_a_repo {
            return Ok(Json(json!({
                "isRepository": false,
                "entries": [],
            })));
        }
        // 不是"非仓库"这一种已知情况 → fail loud,别把配置错误伪装成空态。
        return Err(error(format!(
            "git rev-parse 失败:{}",
            rev_err.trim()
        )));
    }
    let repo_root = top_level.trim().to_string();

    // 分支名 + 是否 detached:失败(空仓库)不算错。
    let (_, branch_raw, _) = run_git(&cwd, &["symbolic-ref", "--short", "-q", "HEAD"])
        .await
        .map_err(error)?;
    let branch = branch_raw.trim();
    let (detached, head_raw, _) = run_git(&cwd, &["rev-parse", "--short", "HEAD"])
        .await
        .map_err(error)?;
    let head = head_raw.trim();
    let is_detached = branch.is_empty() && !head.is_empty() && detached;

    // 领先/落后:有上游才算,没有就返回 null(不是 0)。
    let mut ahead = Value::Null;
    let mut behind = Value::Null;
    if let Ok((true, counts, _)) = run_git(
        &cwd,
        &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"],
    )
    .await
    {
        let parts: Vec<&str> = counts.split_whitespace().collect();
        if parts.len() == 2 {
            ahead = parts[0].parse::<u64>().map_or(Value::Null, |v| json!(v));
            behind = parts[1].parse::<u64>().map_or(Value::Null, |v| json!(v));
        }
    }

    // 改动清单:`--untracked-files=all` 让未跟踪目录展开到文件级
    // (默认只给目录名,面板里点不开具体文件)。
    let (ok, status_raw, status_err) = run_git(
        &cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await
    .map_err(error)?;
    if !ok {
        return Err(error(format!("git status 失败:{}", status_err.trim())));
    }
    let entries = parse_status(status_raw.as_bytes());

    Ok(Json(json!({
        "isRepository": true,
        "repoRoot": repo_root,
        "branch": if branch.is_empty() { Value::Null } else { json!(branch) },
        "head": if head.is_empty() { Value::Null } else { json!(head) },
        "detached": is_detached,
        "ahead": ahead,
        "behind": behind,
        "entries": entries,
    })))
}

#[derive(Deserialize)]
struct DiffQuery {
    path: String,
    /// 仓库内相对路径。
    file: String,
    /// `unstaged` | `staged` | `branch`。
    #[serde(default)]
    source: Option<String>,
}

/// GET /api/git/diff?path=...&file=...&source=... — 单文件 Diff。
///
/// 返回结构与 ZCode 的 `GitDiffState` 对齐:
/// `{ availability, patch, beforeContent, afterContent, summary }`,
/// `availability` ∈ `patch | binary | truncated | unavailable`。
/// 前端据此三级降级(富文本 diff → 纯文本行 → 仅提示)。
async fn diff(
    State(_state): State<Arc<AppState>>,
    Query(query): Query<DiffQuery>,
) -> Result<Json<Value>, ApiError> {
    let cwd = PathBuf::from(&query.path);
    if !cwd.is_dir() {
        return Err(error(format!("目录不存在:{}", cwd.display())));
    }
    let source = query.source.as_deref().unwrap_or("unstaged");

    // 路径以 `:/` 前缀强制按仓库根解析:用户传的是相对路径,而 `git diff`
    // 的路径参数是相对**当前目录**的。若 cwd 是仓库子目录,不加前缀会
    // 静默返回空 diff(文件明明改了,面板却显示无改动)。
    let repo_relative = format!(":/{}", query.file.trim_start_matches('/'));

    let args: Vec<String> = match source {
        "staged" => vec![
            "diff".into(),
            "--cached".into(),
            "--no-color".into(),
            "--".into(),
            repo_relative.clone(),
        ],
        "branch" => vec![
            "diff".into(),
            "--no-color".into(),
            "@{upstream}".into(),
            "--".into(),
            repo_relative.clone(),
        ],
        // 未跟踪文件没有 diff 基线:直接读文件内容当"新增"。
        _ => vec![
            "diff".into(),
            "--no-color".into(),
            "--".into(),
            repo_relative.clone(),
        ],
    };
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();

    // 先判是不是未跟踪:未跟踪文件 `git diff` 永远为空,要单独处理。
    let (_, untracked_raw, _) = run_git(
        &cwd,
        &[
            "ls-files",
            "--others",
            "--exclude-standard",
            "--",
            &repo_relative,
        ],
    )
    .await
    .map_err(error)?;
    let is_untracked = !untracked_raw.trim().is_empty();

    let (ok, patch, diff_err) = run_git(&cwd, &arg_refs).await.map_err(error)?;
    if !ok {
        return Ok(Json(json!({
            "path": query.file,
            "availability": "unavailable",
            "patch": Value::Null,
            "beforeContent": Value::Null,
            "afterContent": Value::Null,
            "summary": diff_err.trim(),
        })));
    }

    let absolute = cwd.join(&query.file);
    // 二进制判定:git 在 patch 里写 "Binary files ... differ"。
    if patch.contains("Binary files") || patch.contains("GIT binary patch") {
        return Ok(Json(json!({
            "path": query.file,
            "availability": "binary",
            "patch": Value::Null,
            "beforeContent": Value::Null,
            "afterContent": Value::Null,
            "summary": Value::Null,
        })));
    }

    // 未跟踪(或 staged 为空)时,用文件内容作为"新增"全文。
    if patch.trim().is_empty() && (is_untracked || source == "unstaged") {
        match std::fs::read_to_string(&absolute) {
            Ok(content) if !content.is_empty() => {
                if content.chars().count() > DIFF_CHAR_LIMIT {
                    return Ok(Json(json!({
                        "path": query.file,
                        "availability": "truncated",
                        "patch": Value::Null,
                        "beforeContent": Value::Null,
                        "afterContent": Value::Null,
                        "summary": Value::Null,
                    })));
                }
                return Ok(Json(json!({
                    "path": query.file,
                    "availability": "patch",
                    "patch": Value::Null,
                    "beforeContent": Value::Null,
                    "afterContent": content,
                    "summary": Value::Null,
                })));
            }
            // 读不出来(二进制/无权限):当二进制处理,不报错。
            Ok(_) => {}
            Err(_) => {
                return Ok(Json(json!({
                    "path": query.file,
                    "availability": "binary",
                    "patch": Value::Null,
                    "beforeContent": Value::Null,
                    "afterContent": Value::Null,
                    "summary": Value::Null,
                })));
            }
        }
    }

    // 超大 patch:只回摘要,让前端显示"已裁剪"而不是硬塞。
    if patch.chars().count() > DIFF_CHAR_LIMIT {
        return Ok(Json(json!({
            "path": query.file,
            "availability": "truncated",
            "patch": Value::Null,
            "beforeContent": Value::Null,
            "afterContent": Value::Null,
            "summary": Value::Null,
        })));
    }

    // 附加前后全文:前端拿它做并排 diff(比解析 patch 更准,且支持
    // "patch 里被 git 省略的上下文")。取不到不算错。
    let (before, after) = read_before_after(&cwd, &query.file, source).await;
    Ok(Json(json!({
        "path": query.file,
        "availability": "patch",
        "patch": patch,
        "beforeContent": before,
        "afterContent": after,
        "summary": Value::Null,
    })))
}

/// 取文件改动前后的全文(拿不到就返回 null)。
async fn read_before_after(
    cwd: &Path,
    file: &str,
    source: &str,
) -> (Value, Value) {
    let repo_relative = format!(":/{}", file.trim_start_matches('/'));
    // 未暂存:HEAD 版本 → 工作区版本。
    // 已暂存:HEAD 版本 → 索引版本(`git show :file`)。
    let before_spec = "HEAD";
    let before = match run_git(cwd, &["show", &format!("{before_spec}:{file}")]).await {
        Ok((true, text, _)) => json!(text),
        _ => Value::Null,
    };
    let after = if source == "staged" {
        match run_git(cwd, &["show", &format!(":{file}")]).await {
            Ok((true, text, _)) => json!(text),
            _ => Value::Null,
        }
    } else {
        match std::fs::read_to_string(cwd.join(file)) {
            Ok(text) => json!(text),
            Err(_) => Value::Null,
        }
    };
    // `repo_relative` 只为可读性保留在作用域里;上面两条命令都用相对
    // 仓库根的 `file`,因为 `git show` 的路径参数本就按仓库根解析。
    let _ = repo_relative;
    (before, after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_status_handles_modified_and_untracked() {
        // " M file.txt\0?? new.txt\0"
        let raw = b" M file.txt\0?? new.txt\0";
        let entries = parse_status(raw);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["path"], "file.txt");
        assert_eq!(entries[0]["unstaged"], true);
        assert_eq!(entries[0]["staged"], false);
        assert_eq!(entries[1]["path"], "new.txt");
        assert_eq!(entries[1]["untracked"], true);
    }

    #[test]
    fn parse_status_consumes_rename_origin_pair() {
        // 重命名是两条 NUL 项:`R  new\0old\0`。必须成对消费,
        // 否则后面所有条目错位(这是最容易踩的坑)。
        let raw = b"R  new.txt\0old.txt\0 M other.txt\0";
        let entries = parse_status(raw);
        assert_eq!(entries.len(), 2, "重命名只应产生一条记录");
        assert_eq!(entries[0]["path"], "new.txt");
        assert_eq!(entries[0]["renamedFrom"], "old.txt");
        assert_eq!(entries[1]["path"], "other.txt");
    }

    #[test]
    fn parse_status_marks_conflicts() {
        let raw = b"UU both.txt\0";
        let entries = parse_status(raw);
        assert_eq!(entries[0]["conflicted"], true);
    }

    #[test]
    fn parse_status_skips_short_fields() {
        assert!(parse_status(b"\0\0ab\0").is_empty());
    }

    /// 真实跑一次 git:证明参数顺序正确、输出能被解析。
    ///
    /// 这条用例守的是一个实测踩到的坑:`-c core.quotepath=false` 若写在
    /// 子命令**之后**,git 会直接报错退出,而调用方只看退出码 → 每个仓库
    /// 都被判成"不是 Git 仓库"。纯函数测试抓不到,必须真起进程。
    #[tokio::test]
    async fn run_git_accepts_config_before_subcommand() {
        let cwd = std::env::current_dir().expect("取当前目录");
        // 这个仓库本身就是 git 仓库(测试在仓库里跑)。
        let (ok, stdout, stderr) = run_git(&cwd, &["rev-parse", "--show-toplevel"])
            .await
            .expect("git 应当能执行");
        assert!(
            ok,
            "rev-parse 应当成功(参数顺序错会让它失败):stdout={stdout:?} stderr={stderr:?}"
        );
        assert!(!stdout.trim().is_empty(), "应当输出仓库根路径");
    }

    /// 非仓库目录必须干净地报 `isRepository: false`,而不是报错。
    #[tokio::test]
    async fn run_git_reports_non_repo_without_erroring() {
        let dir = std::env::temp_dir().join(format!("denia-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (ok, _, stderr) = run_git(&dir, &["rev-parse", "--show-toplevel"])
            .await
            .expect("git 应当能执行");
        // 非仓库:退出码非 0,且 stderr 是"不是仓库"这一类。
        assert!(!ok);
        let lower = stderr.to_ascii_lowercase();
        assert!(
            lower.contains("not a git repository") || lower.contains("fatal:"),
            "非仓库的 stderr 应当可识别:{stderr:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--porcelain=v1 -z` 的输出能被真实解析出改动(端到端)。
    #[tokio::test]
    async fn status_parses_real_repository_changes() {
        let cwd = std::env::current_dir().expect("取当前目录");
        let (ok, raw, _) = run_git(
            &cwd,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        )
        .await
        .expect("git 应当能执行");
        if !ok {
            // 不在仓库里(比如从 tarball 解出来跑测试):跳过而不是误报。
            return;
        }
        let entries = parse_status(raw.as_bytes());
        // 本仓库此刻必然有未提交改动(正在开发),但即便全干净也不该 panic。
        for entry in &entries {
            assert!(
                entry["path"].as_str().is_some_and(|path| !path.is_empty()),
                "每条记录都应当有非空路径:{entry}"
            );
        }
    }
}
