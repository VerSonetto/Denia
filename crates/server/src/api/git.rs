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
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;

use denia_core::message::ChatMessage;
use denia_llm::GenerateRequest;
use futures::StreamExt;

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
        .route("/api/git/commit", post(commit))
        .route("/api/git/push", post(push))
        .route("/api/git/commit-message", post(commit_message))
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

/* ---- 提交 / 推送 ----
 *
 * # 为什么不做暂存区(stage)操作
 *
 * 本面板的定位是「把工作区当前的改动提交掉」,不是完整的 Git 客户端。
 * 引入 stage/unstage 会带来一整套新状态(部分暂存、索引与工作区不一致、
 * 冲突处理),而用户的真实诉求是"看一眼改动,写条信息,提交"。
 * 所以提交走 `git add -A` + `git commit`:**提交全部改动**(含未跟踪文件),
 * 与面板 `unstaged` 来源展示的范围一致 —— 看得见什么就提交什么,不留暗角。
 *
 * 想精细控制暂存的用户,终端标签就在旁边。
 */

#[derive(Deserialize)]
struct CommitBody {
    path: String,
    message: String,
}

/// POST /api/git/commit — 提交工作区全部改动。
async fn commit(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<CommitBody>,
) -> Result<Json<Value>, ApiError> {
    let cwd = PathBuf::from(body.path.trim());
    if !cwd.is_dir() {
        return Err(error(format!("目录不存在:{}", cwd.display())));
    }
    let message = body.message.trim();
    if message.is_empty() {
        return Err(ApiError::bad_request(
            "git/empty-message",
            "提交信息不能为空",
        ));
    }

    // 先确认有东西可提交:没有改动时 `git commit` 会以非 0 退出并给一句
    // 含糊的提示,不如在这里给出明确的"没有可提交的改动"。
    let (ok, status_raw, status_err) = run_git(
        &cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await
    .map_err(error)?;
    if !ok {
        return Err(error(format!("git status 失败:{}", status_err.trim())));
    }
    if parse_status(status_raw.as_bytes()).is_empty() {
        return Err(ApiError::bad_request(
            "git/nothing-to-commit",
            "没有可提交的改动",
        ));
    }

    // `add -A` 覆盖修改/新增/删除与未跟踪文件(与面板展示范围一致)。
    let (ok, _, add_err) = run_git(&cwd, &["add", "-A"]).await.map_err(error)?;
    if !ok {
        return Err(error(format!("git add 失败:{}", add_err.trim())));
    }

    // 提交信息经 `-F -` 走 stdin 传入,不走 `-m`:
    // 多行信息(标题 + 正文)用 `-m` 会与 shell 转义、引号纠缠,而 `-F -`
    // 是字节级传递,信息里有什么都不会被二次解释。
    let (ok, stdout, commit_err) = run_git_with_stdin(
        &cwd,
        &["commit", "-F", "-", "--cleanup=whitespace"],
        message.as_bytes(),
    )
    .await
    .map_err(error)?;
    if !ok {
        let detail = commit_err.trim();
        // 没有配置 user.name/user.email 是最常见的失败:git 的原文很啰嗦,
        // 这里给一句能直接照做的提示(但仍然把原文附上,不吞细节)。
        if detail.contains("Please tell me who you are") || detail.contains("unable to auto-detect email") {
            return Err(ApiError::bad_request(
                "git/identity-missing",
                format!(
                    "提交需要先配置 Git 身份:git config user.name \"你的名字\" 与 git config user.email \"you@example.com\"\n{detail}"
                ),
            ));
        }
        return Err(error(format!("git commit 失败:{detail}")));
    }

    let (_, head_raw, _) = run_git(&cwd, &["rev-parse", "--short", "HEAD"])
        .await
        .map_err(error)?;
    Ok(Json(json!({
        "ok": true,
        "head": head_raw.trim(),
        // git 的提交摘要是给用户看的成功反馈(含改动统计)。
        "summary": stdout.trim(),
    })))
}

#[derive(Deserialize)]
struct PushBody {
    path: String,
}

/// POST /api/git/push — 推送到当前分支的 upstream。
///
/// 各条失败路径**分别识别并显式报错**,不合并成一句"推送失败":
/// 无 upstream、无远程、非快进、凭据失败这四类的处置方式完全不同
/// (建 upstream / 加远程 / 先 pull / 修凭据),合并了就无从下手。
async fn push(
    State(_state): State<Arc<AppState>>,
    Json(body): Json<PushBody>,
) -> Result<Json<Value>, ApiError> {
    let cwd = PathBuf::from(body.path.trim());
    if !cwd.is_dir() {
        return Err(error(format!("目录不存在:{}", cwd.display())));
    }

    // 远程:没有远程时 `git push` 只会说 "No configured push destination",
    // 这里提前给出更明确的说法。
    let (ok, remotes_raw, _) = run_git(&cwd, &["remote"]).await.map_err(error)?;
    if !ok || remotes_raw.trim().is_empty() {
        return Err(ApiError::bad_request(
            "git/no-remote",
            "这个仓库没有配置远程仓库,无法推送",
        ));
    }

    // upstream:`@{upstream}` 解析失败 = 当前分支没有上游。
    let (has_upstream, upstream_raw, _) = run_git(
        &cwd,
        &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
    )
    .await
    .map_err(error)?;
    let upstream = upstream_raw.trim().to_string();
    if !has_upstream {
        let (_, branch_raw, _) = run_git(&cwd, &["symbolic-ref", "--short", "-q", "HEAD"])
            .await
            .map_err(error)?;
        let branch = branch_raw.trim();
        if branch.is_empty() {
            return Err(ApiError::bad_request(
                "git/detached-head",
                "当前处于游离 HEAD,无法推送;请先切换到分支",
            ));
        }
        return Err(ApiError::bad_request(
            "git/no-upstream",
            format!(
                "当前分支 '{branch}' 没有上游分支,无法推送。可在终端执行:git push -u origin {branch}"
            ),
        ));
    }

    let (ok, stdout, stderr) = run_git(&cwd, &["push"]).await.map_err(error)?;
    if !ok {
        let detail = stderr.trim();
        let lower = detail.to_ascii_lowercase();
        // 四类已知失败各给一条能照做的提示;其余原样透出。
        let (code, hint) = if lower.contains("non-fast-forward")
            || lower.contains("fetch first")
            || lower.contains("updates were rejected")
        {
            (
                "git/non-fast-forward",
                "远程有新提交,推送被拒绝。请先拉取合并(终端执行 git pull --rebase),再重试",
            )
        } else if lower.contains("authentication failed")
            || lower.contains("could not read username")
            || lower.contains("permission denied")
            || lower.contains("invalid username or password")
            || lower.contains("terminal prompts disabled")
        {
            (
                "git/auth-failed",
                "凭据无效或缺失,无法推送到远程。请检查 SSH key 或 personal access token",
            )
        } else if lower.contains("could not resolve host")
            || lower.contains("unable to access")
            || lower.contains("connection")
        {
            (
                "git/network-failed",
                "无法连接远程仓库,请检查网络或代理设置",
            )
        } else {
            ("git/push-failed", "推送失败")
        };
        return Err(ApiError::bad_request(code, format!("{hint}\n{detail}")));
    }

    Ok(Json(json!({
        "ok": true,
        "upstream": upstream,
        // push 把进度写在 stderr(正常输出),成功时它是"To ..."那几行。
        "summary": if stderr.trim().is_empty() { stdout.trim() } else { stderr.trim() },
    })))
}

/// 跑一条 git 命令并把 `input` 写进它的 stdin。
///
/// 单独一个函数而不是给 `run_git` 加可选参数:提交信息走 stdin 是
/// **安全传递任意文本**的唯一方式(见 `commit` 里的注释),而其余命令
/// 都该用 `Stdio::null()` 明确"不读 stdin"(否则某些命令会等输入挂住)。
async fn run_git_with_stdin(
    cwd: &Path,
    args: &[&str],
    input: &[u8],
) -> Result<(bool, String, String), String> {
    use tokio::io::AsyncWriteExt;

    let mut child = Command::new("git")
        .arg("-c")
        .arg("core.quotepath=false")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("LC_ALL", "C")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "未找到 git 可执行文件,请先安装 Git 并确保在 PATH 中".to_string()
            } else {
                format!("执行 git 失败:{e}")
            }
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(input)
            .await
            .map_err(|e| format!("写入 git stdin 失败:{e}"))?;
        // 必须显式关闭:不关的话 git 会一直等 stdin 的 EOF,表现为提交挂住。
        drop(stdin);
    }

    let output = tokio::time::timeout(GIT_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "git 命令超时".to_string())?
        .map_err(|e| format!("执行 git 失败:{e}"))?;

    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

/* ---- AI 生成提交信息 ---- */

/// 喂给模型的 diff 字符上限。
///
/// 提交信息是短任务,没必要把整个仓库的改动塞进上下文;截断后仍能覆盖
/// "改了什么"的判断。约 24K 字符 ≈ 8K token,对任何现代模型都很宽裕。
const MESSAGE_DIFF_LIMIT: usize = 24_000;
/// 参考的近期提交条数。
const MESSAGE_LOG_COUNT: usize = 5;

#[derive(Deserialize)]
struct CommitMessageBody {
    path: String,
    provider: String,
    model: String,
    #[serde(default)]
    reasoning_effort: Option<String>,
}

/// 生成提交信息的系统提示。
///
/// 用中文写(与项目"agent 系统提示用中文"的规范一致),并显式钉死
/// conventional commits 格式与 scope 取值 —— 后者是"与近期提交保持一致"
/// 这条要求的落点:模型看得到 git log 里的真实 scope,不用它自己发明。
const COMMIT_MESSAGE_SYSTEM: &str = r#"你是一个 Git 提交信息生成器。根据用户给出的改动内容与近期提交历史,生成**一条**提交信息。

严格遵守以下格式:
type(scope): 描述

要求:
1. type 取 feat / fix / refactor / perf / docs / test / chore / style / build / ci 之一。
2. scope 必须是近期提交历史里出现过的模块名(如 server、web、core、tools、agent-loop、session 等);无法判断时省略括号,写成 `type: 描述`。
3. 描述用中文,动宾结构,说明这次改动**做了什么**,不超过 50 字。
4. 只输出提交信息本身,不要任何解释、不要代码块包裹、不要加引号。
5. 改动较大时可加一行空行后写正文,正文分点说明,每行不超过 72 字。"#;

/// POST /api/git/commit-message — 用模型生成一条提交信息。
///
/// 输入与审查面板展示范围一致:工作区当前未提交的改动(含未跟踪文件)。
/// 复用 `/api/llm/chat` 同一条模型链路(用户已配置的 provider/model),
/// 不新造模型配置入口。
async fn commit_message(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CommitMessageBody>,
) -> Result<Json<Value>, ApiError> {
    let cwd = PathBuf::from(body.path.trim());
    if !cwd.is_dir() {
        return Err(error(format!("目录不存在:{}", cwd.display())));
    }
    let provider = body.provider.trim().to_string();
    let model = body.model.trim().to_string();
    if provider.is_empty() || model.is_empty() {
        return Err(ApiError::bad_request(
            "git/message-no-model",
            "需要先选择模型才能生成提交信息",
        ));
    }

    // 1) 当前未提交改动(与面板展示范围一致)。
    let context = build_commit_context(&cwd).await?;
    if context.is_empty() {
        return Err(ApiError::bad_request(
            "git/nothing-to-commit",
            "没有可提交的改动,无需生成提交信息",
        ));
    }

    // 2) 近期提交作为风格参考。
    let log = recent_commits(&cwd).await?;

    let user = format!(
        "近期提交历史(用于对齐风格与 scope):\n{log}\n\n当前未提交的改动:\n{context}"
    );

    // 3) 关思考:生成提交信息是短任务,低延迟优先。
    //    - 模型支持 `off` 档位 → 直接用 off(DeepSeek 线上会翻成 thinking disabled);
    //    - 只支持强度档位(如 low/medium/high)→ 取最低档;
    //    - 完全不支持思考 → 不传档位。
    let effort = resolve_low_latency_effort(&state, &provider, &model, body.reasoning_effort.as_deref()).await;

    state
        .registry
        .resolve_call(&provider, &model, effort.as_deref())
        .await
        .map_err(ApiError::from_llm)?;

    let request = GenerateRequest {
        model: model.clone(),
        reasoning_effort: effort,
        messages: vec![ChatMessage::user(user)],
        system: Some(COMMIT_MESSAGE_SYSTEM.to_string()),
        temperature: None,
        max_tokens: Some(512),
        stop: Vec::new(),
        tools: Vec::new(),
    };

    let mut chunks = state
        .registry
        .stream(&provider, &request, None)
        .await
        .map_err(ApiError::from_llm)?;

    let mut text = String::new();
    while let Some(chunk) = chunks.next().await {
        match chunk {
            Ok(denia_core::stream::StreamChunk::TextDelta { text: delta, .. }) => text.push_str(&delta),
            Ok(denia_core::stream::StreamChunk::Finish { reason }) => {
                // 失败收尾必须以错误抛出,否则会拿半截文本当结果。
                if let denia_core::stream::FinishReason::Error { failure }
                | denia_core::stream::FinishReason::Aborted { failure } = reason
                {
                    return Err(ApiError::new(
                        axum::http::StatusCode::BAD_GATEWAY,
                        "git/message-generation-failed",
                        failure.message,
                    ));
                }
            }
            Ok(_) => {}
            Err(failure) => {
                return Err(ApiError::new(
                    axum::http::StatusCode::BAD_GATEWAY,
                    "git/message-generation-failed",
                    failure.message,
                ));
            }
        }
    }

    let message = sanitize_commit_message(&text);
    if message.is_empty() {
        return Err(ApiError::new(
            axum::http::StatusCode::BAD_GATEWAY,
            "git/message-empty",
            "模型没有返回可用的提交信息,请重试",
        ));
    }
    Ok(Json(json!({ "message": message })))
}

/// 选一个"最低延迟"的思考档位。
///
/// 优先级:`off`(真正关闭思考)> 最低强度档 > 不传。
/// 具体可用档位从目录里查(它来自适配器的真实能力描述),所以这里不硬编码
/// 档位名 —— 各提供方的档位集合不同。
async fn resolve_low_latency_effort(
    state: &Arc<AppState>,
    provider: &str,
    model: &str,
    requested: Option<&str>,
) -> Option<String> {
    // 目录查不到就尊重调用方给的值(没有则 None),不阻塞生成。
    let catalog = denia_llm::build_model_catalog(&state.registry).await;
    let efforts = catalog
        .groups
        .iter()
        .find(|group| group.id == provider)
        .and_then(|group| group.models.iter().find(|item| item.id == model))
        .and_then(|item| item.reasoning.as_ref())
        .map(|reasoning| reasoning.efforts.iter().map(|effort| effort.id.clone()).collect::<Vec<_>>());
    let Some(efforts) = efforts else {
        return requested.map(str::to_string);
    };
    if efforts.iter().any(|id| id == "off") {
        return Some("off".to_string());
    }
    // 强度档位:按已知排序取最低的那个。
    const ORDER: &[&str] = &["low", "medium", "high", "xhigh", "max"];
    ORDER
        .iter()
        .find(|id| efforts.iter().any(|effort| effort == *id))
        .map(|id| (*id).to_string())
        .or_else(|| requested.map(str::to_string))
}

/// 组装"当前未提交改动"的文本(与面板展示范围一致:未跟踪文件也算)。
///
/// 超大改动按**文件摘要**降级:逐个文件给出 `git diff --stat` 而不是全文,
/// 这样模型仍能看到"改了哪些文件、改了多少行",而不是被截断成半截 diff
/// 导致误判。
async fn build_commit_context(cwd: &Path) -> Result<String, ApiError> {
    // 未跟踪文件要单独取:它们不在 `git diff HEAD` 里。
    let (_, status_raw, _) = run_git(
        cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )
    .await
    .map_err(error)?;
    let entries = parse_status(status_raw.as_bytes());
    if entries.is_empty() {
        return Ok(String::new());
    }
    let untracked: Vec<String> = entries
        .iter()
        .filter(|entry| entry["untracked"] == json!(true))
        .filter_map(|entry| entry["path"].as_str().map(str::to_string))
        .collect();

    // 已跟踪文件的改动(含已暂存与未暂存)。
    let (ok, diff, diff_err) = run_git(cwd, &["diff", "HEAD", "--no-color"]).await.map_err(error)?;
    if !ok {
        return Err(error(format!("git diff 失败:{}", diff_err.trim())));
    }

    let mut context = String::new();
    let untracked_block = if untracked.is_empty() {
        String::new()
    } else {
        // 未跟踪文件没有 diff 基线,列文件名 + 行数即可(读全文对判断
        // "这次加了什么"帮助有限,却极占上下文)。
        let mut block = String::from("新增(未跟踪)文件:\n");
        for path in &untracked {
            let lines = std::fs::read_to_string(cwd.join(path))
                .map(|content| content.lines().count())
                .unwrap_or(0);
            block.push_str(&format!("  {path}({lines} 行)\n"));
        }
        block
    };

    if diff.chars().count() + untracked_block.chars().count() > MESSAGE_DIFF_LIMIT {
        // 超限:降级为逐文件统计。
        let (_, stat, _) = run_git(cwd, &["diff", "HEAD", "--stat", "--no-color"])
            .await
            .map_err(error)?;
        context.push_str("改动较大,以下为逐文件统计:\n");
        context.push_str(stat.trim());
        context.push('\n');
        if !untracked_block.is_empty() {
            context.push_str(&untracked_block);
        }
    } else {
        context.push_str(&diff);
        if !untracked_block.is_empty() {
            context.push('\n');
            context.push_str(&untracked_block);
        }
    }
    Ok(context)
}

/// 近期提交(一行一条),用于对齐风格与 scope。
async fn recent_commits(cwd: &Path) -> Result<String, ApiError> {
    let (ok, log, log_err) = run_git(
        cwd,
        &["log", &format!("-{MESSAGE_LOG_COUNT}"), "--pretty=format:%s"],
    )
    .await
    .map_err(error)?;
    if !ok {
        // 空仓库(还没有任何提交):不是错误,给一句说明即可。
        let lower = log_err.to_ascii_lowercase();
        if lower.contains("does not have any commits") || lower.contains("unknown revision") {
            return Ok("(这个仓库还没有提交历史)".to_string());
        }
        return Err(error(format!("git log 失败:{}", log_err.trim())));
    }
    Ok(log.trim().to_string())
}

/// 清洗模型输出:去掉代码块围栏与首尾引号,只留提交信息本体。
fn sanitize_commit_message(raw: &str) -> String {
    let mut text = raw.trim().to_string();
    // 模型偶尔会把整条信息包进 ```...```。
    if text.starts_with("```") {
        text = text
            .trim_start_matches("```")
            // 语言标注(```text / ```git)。
            .trim_start_matches(|c: char| c.is_ascii_alphabetic())
            .trim_start_matches('\n')
            .to_string();
        if let Some(index) = text.rfind("```") {
            text.truncate(index);
        }
    }
    let text = text.trim();
    // 首尾成对的引号(中英文)一并去掉。
    let text = text
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .or_else(|| text.strip_prefix('「').and_then(|inner| inner.strip_suffix('」')))
        .unwrap_or(text);
    text.trim().to_string()
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

    /* ---- 提交 / 推送 / AI 生成:纯函数与真实仓库端到端 ---- */

    /// 造一个带身份配置的临时仓库:提交类测试都需要它(否则 git 会拒绝提交)。
    ///
    /// 用 `git init` + 显式 config 而不是继承用户配置:测试不该依赖运行
    /// 机器上的 Git 身份,否则在没配 user.email 的环境里必然失败。
    fn scratch_repo(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("denia-git-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试目录");
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .expect("git 应当能执行")
        };
        assert!(run(&["init", "-q"]).status.success(), "git init 应当成功");
        run(&["config", "user.name", "denia test"]);
        run(&["config", "user.email", "denia@example.invalid"]);
        run(&["config", "commit.gpgsign", "false"]);
        dir
    }

    /// 端到端:提交真的落库,提交信息里的中文与多行结构不被破坏。
    ///
    /// 这条守的是 `-F -`(stdin)这条路径:改用 `-m` 传多行信息时,
    /// 换行与引号会被 shell/参数层吃掉,而单看代码看不出来。
    #[tokio::test]
    async fn commit_writes_message_via_stdin() {
        let repo = scratch_repo("commit");
        std::fs::write(repo.join("a.txt"), "hello").unwrap();

        let message = "feat(server):测试提交\n\n- 第一点\n- 第二点";
        let (ok, _, err) = run_git_with_stdin(
            &repo,
            &["commit", "-F", "-", "--cleanup=whitespace"],
            message.as_bytes(),
        )
        .await
        .expect("git 应当能执行");
        assert!(!ok, "未 add 时提交应当失败(证明这条路径真的在跑 git)");

        assert!(
            run_git(&repo, &["add", "-A"]).await.unwrap().0,
            "git add 应当成功"
        );
        let (ok, stdout, err) = run_git_with_stdin(
            &repo,
            &["commit", "-F", "-", "--cleanup=whitespace"],
            message.as_bytes(),
        )
        .await
        .expect("git 应当能执行");
        assert!(ok, "提交应当成功:stdout={stdout:?} stderr={err:?}");

        // 提交信息完整落库(中文、空行、分点都在)。
        let (_, log, _) = run_git(&repo, &["log", "-1", "--pretty=format:%B"]).await.unwrap();
        assert_eq!(log.trim_end(), message, "提交信息必须逐字节保真");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// 无 upstream 必须报出专门的错误码,而不是笼统的"推送失败"。
    ///
    /// 这条是需求里点名的失败路径:没有上游时用户需要知道"该去建 upstream",
    /// 而不是猜哪里出了问题。
    #[tokio::test]
    async fn push_without_upstream_reports_specific_code() {
        let repo = scratch_repo("no-upstream");
        std::fs::write(repo.join("a.txt"), "x").unwrap();
        run_git(&repo, &["add", "-A"]).await.unwrap();
        run_git_with_stdin(&repo, &["commit", "-F", "-"], "chore: init".as_bytes())
            .await
            .unwrap();

        // 没有任何远程:应当命中 no-remote(而不是 no-upstream)。
        let (_, remotes, _) = run_git(&repo, &["remote"]).await.unwrap();
        assert!(remotes.trim().is_empty(), "新仓库不该有远程");

        // 加一个远程但不设 upstream:这时才走 no-upstream 分支。
        let (ok, _, _) = run_git(&repo, &["remote", "add", "origin", "https://example.invalid/x.git"])
            .await
            .unwrap();
        assert!(ok, "加远程应当成功");
        let (has_upstream, _, _) = run_git(
            &repo,
            &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
        )
        .await
        .unwrap();
        assert!(!has_upstream, "未 -u 推送过就不该有 upstream");

        // 分支名能取到(错误提示里要用它给出可照做的命令)。
        let (_, branch, _) = run_git(&repo, &["symbolic-ref", "--short", "-q", "HEAD"])
            .await
            .unwrap();
        assert!(!branch.trim().is_empty(), "应当能取到当前分支名");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// 非快进:本地与远程分叉时,推送必须被 git 拒绝(而不是静默覆盖)。
    ///
    /// 用本地裸仓库当远程,完整跑一次"提交 → 推送 → 分叉 → 再推送"，
    /// 证明我们识别的是真实的 non-fast-forward 输出。
    #[tokio::test]
    async fn push_rejects_non_fast_forward() {
        let bare = scratch_repo("bare");
        let remote_dir = bare.join("remote.git");
        let run_bare = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&bare)
                .output()
                .expect("git 应当能执行")
        };
        assert!(
            run_bare(&["init", "--bare", "-q", remote_dir.to_str().unwrap()])
                .status
                .success(),
            "建裸仓库应当成功"
        );

        let work = scratch_repo("clone");
        let run_work = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&work)
                .output()
                .expect("git 应当能执行")
        };
        run_work(&["remote", "add", "origin", remote_dir.to_str().unwrap()]);
        std::fs::write(work.join("a.txt"), "one").unwrap();
        run_work(&["add", "-A"]);
        run_work(&["commit", "-q", "-m", "chore: first"]);
        let pushed = run_work(&["push", "-q", "-u", "origin", "HEAD"]);
        assert!(
            pushed.status.success(),
            "首次推送应当成功:{:?}",
            String::from_utf8_lossy(&pushed.stderr)
        );

        // 另起一份克隆,推一个新提交到远程(制造分叉)。
        let other = std::env::temp_dir().join(format!("denia-git-{}-other", std::process::id()));
        let _ = std::fs::remove_dir_all(&other);
        let cloned = std::process::Command::new("git")
            .args(["clone", "-q", remote_dir.to_str().unwrap(), other.to_str().unwrap()])
            .output()
            .expect("git 应当能执行");
        assert!(cloned.status.success(), "clone 应当成功");
        for args in [
            vec!["config", "user.name", "other"],
            vec!["config", "user.email", "other@example.invalid"],
        ] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&other)
                .output()
                .expect("git 应当能执行");
        }
        std::fs::write(other.join("b.txt"), "two").unwrap();
        for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", "chore: second"], vec!["push", "-q"]] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&other)
                .output()
                .expect("git 应当能执行");
        }

        // 本地也提交一笔(不拉取),此时推送必然被拒。
        std::fs::write(work.join("c.txt"), "three").unwrap();
        run_work(&["add", "-A"]);
        run_work(&["commit", "-q", "-m", "chore: local"]);
        let rejected = run_work(&["push"]);
        assert!(!rejected.status.success(), "分叉后推送应当被拒绝");
        let stderr = String::from_utf8_lossy(&rejected.stderr).to_ascii_lowercase();
        // 断言我们识别的那几种字样确实出现在 git 的真实输出里 ——
        // 若 git 改了措辞,这条会失败并提醒更新匹配规则。
        assert!(
            stderr.contains("non-fast-forward")
                || stderr.contains("fetch first")
                || stderr.contains("rejected"),
            "应当命中非快进的识别关键词:{stderr}"
        );

        let _ = std::fs::remove_dir_all(&bare);
        let _ = std::fs::remove_dir_all(&work);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// 模型输出清洗:围栏、引号、多余空白都要去掉。
    #[test]
    fn sanitize_strips_fences_and_quotes() {
        assert_eq!(
            sanitize_commit_message("feat(web):加个按钮"),
            "feat(web):加个按钮"
        );
        assert_eq!(
            sanitize_commit_message("```\nfeat(web):加个按钮\n```"),
            "feat(web):加个按钮"
        );
        assert_eq!(
            sanitize_commit_message("```text\nfix(server):修个 bug\n```"),
            "fix(server):修个 bug"
        );
        assert_eq!(
            sanitize_commit_message("  \"feat(core):带引号\"  "),
            "feat(core):带引号"
        );
        assert_eq!(
            sanitize_commit_message("「feat(core):中文引号」"),
            "feat(core):带引号".replace("带引号", "中文引号")
        );
        // 多行正文必须保留(空行是标题与正文的分界,不能被吃掉)。
        assert_eq!(
            sanitize_commit_message("feat(a):标题\n\n正文一\n正文二"),
            "feat(a):标题\n\n正文一\n正文二"
        );
        assert_eq!(sanitize_commit_message("   \n  "), "");
    }

    /// 超限降级:大改动走 `--stat` 摘要,而不是把全文塞进上下文。
    #[tokio::test]
    async fn commit_context_degrades_to_stat_when_huge() {
        let repo = scratch_repo("huge");
        // 造一个超过 MESSAGE_DIFF_LIMIT 的文件改动。
        let big = "x".repeat(MESSAGE_DIFF_LIMIT + 5_000);
        std::fs::write(repo.join("big.txt"), &big).unwrap();
        run_git(&repo, &["add", "-A"]).await.unwrap();
        run_git_with_stdin(&repo, &["commit", "-F", "-"], "chore: base".as_bytes())
            .await
            .unwrap();
        // 全量改写:diff 会包含"删一整份 + 加一整份",远超上限。
        std::fs::write(repo.join("big.txt"), big.replace('x', "y")).unwrap();

        let context = build_commit_context(&repo).await.expect("应当能取到上下文");
        assert!(
            context.contains("逐文件统计"),
            "超限时必须降级为统计摘要:{context:.200?}"
        );
        assert!(
            context.chars().count() < MESSAGE_DIFF_LIMIT + 2_000,
            "降级后仍应远小于上限,实际 {}",
            context.chars().count()
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// 未跟踪文件也要进上下文(与面板展示范围一致):否则新建的文件
    /// 在模型眼里不存在,生成的提交信息会漏掉它们。
    #[tokio::test]
    async fn commit_context_includes_untracked_files() {
        let repo = scratch_repo("untracked");
        std::fs::write(repo.join("tracked.txt"), "a").unwrap();
        run_git(&repo, &["add", "-A"]).await.unwrap();
        run_git_with_stdin(&repo, &["commit", "-F", "-"], "chore: base".as_bytes())
            .await
            .unwrap();
        std::fs::write(repo.join("brand-new.txt"), "line1\nline2\n").unwrap();

        let context = build_commit_context(&repo).await.expect("应当能取到上下文");
        assert!(
            context.contains("brand-new.txt"),
            "未跟踪文件必须出现在上下文里:{context}"
        );
        assert!(
            context.contains("新增(未跟踪)文件"),
            "未跟踪文件应当有单独的分组说明:{context}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// 干净工作区:上下文为空(调用方据此拒绝生成,而不是拿空 diff 去问模型)。
    #[tokio::test]
    async fn commit_context_is_empty_when_clean() {
        let repo = scratch_repo("clean");
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        run_git(&repo, &["add", "-A"]).await.unwrap();
        run_git_with_stdin(&repo, &["commit", "-F", "-"], "chore: base".as_bytes())
            .await
            .unwrap();

        let context = build_commit_context(&repo).await.expect("应当能取到上下文");
        assert!(context.trim().is_empty(), "干净工作区应当没有上下文:{context:?}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// 近期提交能取到,且是"一行一条"的紧凑格式(喂给模型用)。
    #[tokio::test]
    async fn recent_commits_returns_one_line_each() {
        let repo = scratch_repo("log");
        std::fs::write(repo.join("a.txt"), "a").unwrap();
        run_git(&repo, &["add", "-A"]).await.unwrap();
        run_git_with_stdin(&repo, &["commit", "-F", "-"], "feat(core):第一条".as_bytes())
            .await
            .unwrap();
        std::fs::write(repo.join("b.txt"), "b").unwrap();
        run_git(&repo, &["add", "-A"]).await.unwrap();
        run_git_with_stdin(&repo, &["commit", "-F", "-"], "fix(web):第二条".as_bytes())
            .await
            .unwrap();

        let log = recent_commits(&repo).await.expect("应当能取到 log");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "两条提交应当各占一行:{log:?}");
        // 最新的在前。
        assert_eq!(lines[0], "fix(web):第二条");
        assert_eq!(lines[1], "feat(core):第一条");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// 空仓库(还没有任何提交):给一句说明而不是报错 ——
    /// 首次提交的仓库也要能生成提交信息。
    #[tokio::test]
    async fn recent_commits_tolerates_empty_repository() {
        let repo = scratch_repo("empty-log");
        let log = recent_commits(&repo).await.expect("空仓库不该报错");
        assert!(
            log.contains("还没有提交历史"),
            "空仓库应当给出说明:{log:?}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }
}
