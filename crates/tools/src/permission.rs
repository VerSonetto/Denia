//! 权限策略引擎:操作类别 × 权限模式 → 三值决策。
//!
//! - 四档权限模式:read-only / auto-edit / plan / full(见
//!   [`denia_core::session::PermissionMode`]);
//! - 操作类别由派发处(exec)在调用工具前判定,工具内部不再自行判权;
//! - 决策三值:Allow 直接执行 / Ask 弹审批(串行)/ Deny 携带模型可读
//!   的中文拒绝原因。
//!
//! 不使用模型侧 `sandbox_permissions` 升权协议——
//! Ask 决策由 harness 主动发起审批,模型无需携带任何字段重试。模型若
//! 因旧习惯带上该字段,由参数宽容解析自然忽略。

use denia_core::session::PermissionMode;

/// 一次工具调用的操作类别(派发前判定)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActionClass {
    /// 读类:读文件/搜索/浏览器/无写副作用的命令。
    Read,
    /// 文件写,落点在工作区内。
    WriteInside,
    /// 文件写,落点在工作区外(自动编辑档的 Ask 点之一)。
    WriteOutside,
    /// 文件删除(删除不可逆,即使落点在工作区内也要审批)。
    Delete,
    /// 文件写,落点在项目记忆目录内的 `.md` 文件(记忆是 harness 行为,
    /// 不是用户任务写:后台提取与手工沉淀在四档下都不打断)。
    MemoryWrite,
    /// 有写副作用的命令(bash / job_start)。
    BashWrite,
    /// 提交计划(exit_plan)。
    PlanSubmit,
    /// 预设创作(create_preset):落盘到部署 preset 目录。写入路径由服务端
    /// 固定,不受模型操纵,属会话任务的交付物(自动编辑档直接放行)。
    PresetCreate,
}

/// 策略决策。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 直接执行。
    Allow,
    /// 需要用户审批(弹审批卡,串行执行)。
    Ask,
    /// 策略拒绝;携带模型可读的原因与修复建议。
    Deny(String),
}

/// 策略矩阵:模式 × 类别 → 决策。
///
/// - 完全访问字如其名:除计划提交(工具语义限定计划档)外全部放行;
/// - 自动编辑:工作区内文件写自动放行;三类操作必须过用户审批——
///   工作区外写、文件删除(删除不可逆,区内也拦)、bash 增删改文件;
/// - 只读:写类全拒;
/// - 计划:调研 + exit_plan 审批,其余写类全拒。
pub fn decide(mode: PermissionMode, class: ActionClass) -> Decision {
    use ActionClass::{
        BashWrite, Delete, MemoryWrite, PlanSubmit, PresetCreate, Read, WriteInside, WriteOutside,
    };
    use Decision::{Allow, Ask, Deny};
    use PermissionMode::{AutoEdit, Full, Plan, ReadOnly};
    match (mode, class) {
        // 计划提交必须走审批(Ask);其余档 schema 已过滤,这里是执行层兜底。
        (Plan, PlanSubmit) => Ask,
        (_, PlanSubmit) => Deny(
            "当前不在计划模式,exit_plan 工具不可用;请按当前权限模式直接执行任务。".into(),
        ),
        // 读类永远放行。
        (_, Read) => Allow,
        // 记忆目录写是 harness 行为(后台提取/索引维护),不是用户任务写:
        // 四档一律放行;敏感路径在派发处(exec)先行拒绝。
        (_, MemoryWrite) => Allow,
        // 完全访问档全部放行。
        (Full, _) => Allow,
        // 自动编辑:区内文件写与预设创作自动放行。预设创作虽写在工作区外,
        // 但落盘路径由服务端固定(preset 根 + 合法 id),且是用户明确要求的
        // 交付物,与区内写同档放行。区外写/删除/bash 写三类走审批,用户
        // 可选"放行本次"或"本窗口放行"(会话内同类不再询问,由派发处
        // 查会话放行表短路成 Allow)。
        (AutoEdit, WriteInside | PresetCreate) => Allow,
        (AutoEdit, WriteOutside | BashWrite | Delete) => Ask,
        // 只读与计划:一切写类操作拒绝。
        (ReadOnly, WriteInside | WriteOutside | BashWrite | Delete) => Deny(
            "当前为只读模式,该操作会修改文件或产生写副作用,已被拒绝;请仅做阅读与分析,或请用户切换权限模式。".into(),
        ),
        (ReadOnly, PresetCreate) => Deny(
            "当前为只读模式,创建预设会写入文件,已被拒绝;请用户切换权限模式后再创建。".into(),
        ),
        (Plan, WriteInside | WriteOutside | BashWrite | Delete) => Deny(
            "当前为计划模式,禁止一切写操作与有写副作用的命令;请完成调研后调用 exit_plan 提交计划,等待用户批准后再执行。".into(),
        ),
        (Plan, PresetCreate) => Deny(
            "当前为计划模式,创建预设会写入文件,已被拒绝;请先退出计划模式再创建。".into(),
        ),
    }
}

/// 沙箱(confined)生效的档位:只读与计划的调研隔离——区外读写一律在
/// 工具层拒绝。自动编辑与完全访问不沙箱:前者工作区外写由策略引擎转
/// 用户审批(Ask),后者全部放行;对这两档再开沙箱,区外写会在路径解析
/// 处被硬拦,审批卡永远弹不出来,策略引擎形同虚设。
pub fn sandbox_applies(mode: PermissionMode) -> bool {
    matches!(mode, PermissionMode::ReadOnly | PermissionMode::Plan)
}

/// 记忆写敏感段:git 钩子、依赖树、其他 agent harness 的配置/技能目录。
/// 按路径组件匹配、不区分大小写——命中即拒,防止记忆写被诱导落到
/// 可执行/供应链位置。
///
/// 名单里的 `.zcode` 是**实际存在的目录名**(其他 agent 工具在本机留下的
/// 配置目录),属于"要防的第三方 harness 目录"这一类,不是对某个产品的
/// 引用——它与 `.claude`、`.agents` 同性质。删掉它会让该目录变成记忆写的
/// 可落点,是安全回退,故保留。
pub fn memory_path_is_sensitive(path: &std::path::Path) -> bool {
    const SENSITIVE: &[&str] = &[
        ".git",
        "hooks",
        "node_modules",
        ".claude",
        ".zcode",
        ".agents",
    ];
    path.components().any(|component| {
        let text = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        SENSITIVE.contains(&text.as_str())
    })
}

/// 启发式判断一条 bash 命令是否明显包含文件写副作用。
///
/// 静态无法证明的命令按读类放行(与 bash 不受 OS 沙箱限制的现实一致,
/// 不做虚假承诺);误报的代价是模型收到可读拒绝后改用文件写工具或
/// 请用户切档。
pub fn bash_may_write(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let markers = [
        " > ",
        " >> ",
        " 2> ",
        " &> ",
        ">|",
        "rm ",
        "rmdir ",
        "mv ",
        "cp ",
        "mkdir ",
        "touch ",
        "tee ",
        "sed -i",
        "perl -i",
        "truncate ",
        "install ",
        "ln -",
        "chmod ",
        "chown ",
    ];
    markers.iter().any(|marker| lower.contains(marker))
}

/// 判断一条命令是否调用了指定名字的程序(词级匹配)。
///
/// 按 shell 分隔符(空白/管道/分号/逻辑与/括号)切 token,每个 token 取
/// 文件名部分(剥引号与路径前缀)后 ASCII 小写精确比较——`rm`、`/bin/rm`、
/// `sudo rm`、`xargs rm` 都命中,而 `confirm` 不再被 "rm " 的子串匹配误伤。
fn bash_invokes(command: &str, names: &[&str]) -> bool {
    command
        .split(|c: char| c.is_whitespace() || matches!(c, '|' | ';' | '&' | '(' | ')'))
        .any(|token| {
            let name = token
                .trim_matches(|c| c == '"' || c == '\'')
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(token)
                .to_ascii_lowercase();
            names.contains(&name.as_str())
        })
}

/// 启发式判断一条 bash 命令是否是文件删除操作(删除不可逆,单独归类:
/// 自动编辑档下即使落点在工作区内也要过用户审批)。
///
/// 覆盖 Unix(rm/rmdir/unlink/shred)、Windows cmd(del/erase/rd)与
/// PowerShell(Remove-Item;`rm` 是其别名同样命中),以及 find 的
/// `-delete` 谓词。词级精确匹配,不做子串猜测。
pub fn bash_may_delete(command: &str) -> bool {
    bash_invokes(
        command,
        &[
            "rm",
            "rmdir",
            "unlink",
            "shred",
            "del",
            "erase",
            "rd",
            "remove-item",
            "-delete",
        ],
    )
}

/// 严格只读命令判定:只有能"证明"只读的命令才返回 true(fail-closed)。
///
/// 与 [`bash_may_write`]/[`bash_may_delete`] 的启发式方向相反:那两个函数为
/// 写副作用找证据,**证明不了就按读类放行**;本函数为只读找证据,**证明不了
/// 就拒绝**。调度层若拿它换取免审批或放宽沙箱,误判的代价是未经审批的写
/// 副作用,所以拒绝侧必须保守。
///
/// 判定顺序:
/// 1. 形态检查:重定向(`>`/`>>`)、命令替换(`$(`、反引号)一律 false——
///    它们让"文本"与"实际执行的动作"脱钩,静态证明不了任何东西;
/// 2. 按管道/分号/逻辑与/后台符/换行分段,在**每一段**上判定,任一段不
///    满足即整体 false(`ls | tee out`、`mkdir x && ls` 都不是只读);
/// 3. 逐段:命令名(剥路径、引号与 `.exe` 后缀)命中写命令黑名单直接
///    false;其余命令必须命中只读白名单,且不含写选项(`sort -o`、
///    `git --output=<file>`);
/// 4. 未命中白名单 → false(`sed`、`find`、`curl`、`python -c` 等都会
///    因此拒绝,即使它们偶尔有只读用法——fail-closed 不猜)。
pub fn certified_readonly_command(command: &str) -> bool {
    let trimmed = command.trim();
    if trimmed.is_empty()
        || trimmed.contains('>')
        || trimmed.contains("$(")
        || trimmed.contains('`')
    {
        return false;
    }
    let mut segments = 0usize;
    for segment in trimmed.split(['|', ';', '&', '\n']) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        segments += 1;
        if !readonly_segment(segment) {
            return false;
        }
    }
    // 全是分隔符(如 "&&")的命令没有可判定的段,同样拒绝。
    segments > 0
}

/// 写命令黑名单:命中即 false。
///
/// 名单里的命令本就不在只读白名单(`_ => false` 也会拒绝),显式列出是
/// 双重保险:白名单日后扩项时,这些写形态(`sed -i`、`find -delete` 或
/// `find -exec`、`chmod`/`chown`/`mv`/`rm`/`dd`/`tee`/`truncate`,以及
/// PowerShell 的 `Set-Content`/`Out-File` 一类)仍然被这里挡住,不必依赖
/// 每个调用点重新论证。
const WRITE_COMMANDS: &[&str] = &[
    "sed",
    "find",
    "chmod",
    "chown",
    "chgrp",
    "mv",
    "dd",
    "tee",
    "truncate",
    "mkdir",
    "touch",
    "cp",
    "rm",
    "rmdir",
    "shred",
    "unlink",
    "install",
    "ln",
    "patch",
    "del",
    "erase",
    "rd",
    "remove-item",
    "set-content",
    "add-content",
    "out-file",
    "new-item",
    "move-item",
    "copy-item",
    "rename-item",
];

/// `git branch` 的查询类选项:参数全部命中才判只读。
///
/// `git branch <名字>` 是**创建分支**、`-d`/`-D`/`-m`/`-M` 等是写,所以
/// 只要出现集合外参数(包括普通名字)就拒绝。
const GIT_BRANCH_READONLY_ARGS: &[&str] = &[
    "-a",
    "-r",
    "-v",
    "-vv",
    "--all",
    "--remotes",
    "--verbose",
    "--list",
    "--show-current",
    "--contains",
    "--merged",
    "--no-merged",
    "--points-at",
    "--format",
];

/// 单段(已不含管道与命令分隔符)的只读判定。
fn readonly_segment(segment: &str) -> bool {
    let tokens = shell_tokens(segment);
    let Some((head, args)) = tokens.split_first() else {
        return false;
    };
    let program = command_name(head);
    if WRITE_COMMANDS.contains(&program.as_str()) {
        return false;
    }
    match program.as_str() {
        // 无写选项的基础只读命令;`echo` 的重定向形态已在形态检查里排除。
        "echo" | "ls" | "pwd" | "cat" | "head" | "tail" | "wc" | "which" | "where" | "uniq"
        | "diff" | "stat" | "file" => true,
        // PowerShell 侧的等价只读命令:bash 工具在 Windows 上经 pwsh 执行,
        // 只认 Unix 名单等于在主力平台上失效。这些命令本身没有写形式。
        "get-childitem" | "get-content" | "get-item" | "get-process" | "get-location"
        | "test-path" | "select-string" | "resolve-path" => true,
        // `sort -o <file>` 会直接写文件,只放行无输出参数的形式。
        "sort" => !args.iter().any(|arg| is_output_flag(arg)),
        // 版本查询属于只读;`node -e`/`node script.js` 都能写,不放行。
        "node" => matches!(args, ["--version"] | ["-v"]),
        "git" => git_is_readonly(args),
        _ => false,
    }
}

/// `git` 只读判定:子命令必须在白名单里,且不带写参数。
fn git_is_readonly(args: &[&str]) -> bool {
    let Some(subcommand) = args.first() else {
        return false;
    };
    let rest = &args[1..];
    // `--output=<file>` 是 diff/log 家族的写出口,统一先挡。
    if rest.iter().any(|arg| is_output_flag(arg)) {
        return false;
    }
    match *subcommand {
        "status" | "log" | "diff" | "show" | "rev-parse" | "rev-list" | "blame" | "describe"
        | "shortlog" | "ls-files" | "ls-tree" | "cat-file" | "grep" | "whatchanged"
        | "merge-base" | "name-rev" | "for-each-ref" => true,
        "branch" => rest
            .iter()
            .all(|arg| GIT_BRANCH_READONLY_ARGS.contains(arg)),
        _ => false,
    }
}

/// `-o`/`--output` 类"写文件"参数(sort 与部分 git 子命令支持)。
fn is_output_flag(arg: &str) -> bool {
    arg == "-o" || (arg.starts_with("-o") && arg.len() > 2) || arg.starts_with("--output")
}

/// 按空白切 token 并剥掉首尾引号。
///
/// 不做完整 shell 解析——fail-closed 前提下,解析不了的形态(含空格的
/// 引号包裹路径等)自然落回"未命中白名单",不会误判成只读。
fn shell_tokens(segment: &str) -> Vec<&str> {
    segment
        .split_whitespace()
        .map(|token| token.trim_matches(|c| c == '"' || c == '\''))
        .collect()
}

/// 命令 token 归一化:取路径最后一段、剥 `.exe` 后缀、ASCII 小写。
fn command_name(token: &str) -> String {
    let name = token.rsplit(['/', '\\']).next().unwrap_or(token);
    let lower = name.to_ascii_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deny_text(mode: PermissionMode, class: ActionClass) -> String {
        match decide(mode, class) {
            Decision::Deny(text) => text,
            other => panic!("expected Deny for {mode:?}/{class:?}, got {other:?}"),
        }
    }

    #[test]
    fn decide_matrix_is_complete() {
        use ActionClass::*;
        use PermissionMode::*;
        // 读类全放行。
        for mode in [ReadOnly, AutoEdit, Plan, Full] {
            assert_eq!(decide(mode, Read), Decision::Allow);
            // 记忆写是 harness 行为,四档一律放行(敏感段在派发处拒)。
            assert_eq!(decide(mode, MemoryWrite), Decision::Allow);
        }
        // 完全访问档全放行(字如其名)。
        for class in [Read, WriteInside, WriteOutside, BashWrite, Delete] {
            assert_eq!(decide(Full, class), Decision::Allow);
        }
        // 自动编辑:区内文件写与预设创作放行;区外写/删除/bash 写三类审批。
        assert_eq!(decide(AutoEdit, WriteInside), Decision::Allow);
        assert_eq!(decide(AutoEdit, PresetCreate), Decision::Allow);
        for class in [WriteOutside, BashWrite, Delete] {
            assert_eq!(decide(AutoEdit, class), Decision::Ask, "{class:?} 应走审批");
        }
        // 只读与计划:写类全拒。
        for class in [WriteInside, WriteOutside, BashWrite, Delete] {
            assert!(matches!(decide(ReadOnly, class), Decision::Deny(_)));
            assert!(matches!(decide(Plan, class), Decision::Deny(_)));
        }
        // 计划提交只在计划档开放,且必须经用户审批。
        assert_eq!(decide(Plan, PlanSubmit), Decision::Ask);
        for mode in [ReadOnly, AutoEdit, Full] {
            assert!(matches!(decide(mode, PlanSubmit), Decision::Deny(_)));
        }
    }

    #[test]
    fn memory_sensitive_paths_match_component_wise() {
        assert!(memory_path_is_sensitive(std::path::Path::new(
            "C:\\repo\\.git\\hooks\\post-commit"
        )));
        assert!(memory_path_is_sensitive(std::path::Path::new(
            "/a/node_modules/x/y.md"
        )));
        assert!(memory_path_is_sensitive(std::path::Path::new(
            "/home/u/.claude/skills/s.md"
        )));
        assert!(
            memory_path_is_sensitive(std::path::Path::new("/repo/Hooks/x.md")),
            "大小写折叠"
        );
        // 记忆目录本体不命中( home 目录名 .denia 不在名单)。
        assert!(!memory_path_is_sensitive(std::path::Path::new(
            "/home/u/.denia/memories/projects/x/memory/a.md"
        )));
        // 子串不算命中。
        assert!(!memory_path_is_sensitive(std::path::Path::new(
            "/ws/gitbook/notes.md"
        )));
    }

    #[test]
    fn deny_texts_are_actionable() {
        let readonly = deny_text(PermissionMode::ReadOnly, ActionClass::WriteInside);
        assert!(readonly.contains("只读模式"));
        let plan = deny_text(PermissionMode::Plan, ActionClass::BashWrite);
        assert!(plan.contains("exit_plan"));
    }

    #[test]
    fn detects_obvious_writes() {
        assert!(bash_may_write("echo hi > out.txt"));
        assert!(bash_may_write("rm -rf node_modules"));
        assert!(bash_may_write("ls | tee list.txt"));
        assert!(!bash_may_write("ls -la"));
        assert!(!bash_may_write("git status"));
    }

    #[test]
    fn detects_deletions_word_wise() {
        // Unix 家族。
        assert!(bash_may_delete("rm -rf node_modules"));
        assert!(bash_may_delete("rmdir build"));
        assert!(bash_may_delete("/usr/bin/unlink f.txt"));
        assert!(bash_may_delete("find . -name '*.tmp' -delete"));
        // Windows cmd 与 PowerShell。
        assert!(bash_may_delete("del /q cache\\*.log"));
        assert!(bash_may_delete("Remove-Item -Recurse dist"));
        assert!(bash_may_delete("echo hi | Remove-Item"));
        // 管道另一端的删除命令同样命中。
        assert!(bash_may_delete("git ls-files | xargs rm"));
        // 词级匹配不吃子串误报:confirm/chdir 不算 rm/del。
        assert!(!bash_may_delete("confirm deployment"));
        assert!(!bash_may_delete("chdir build && ls"));
        assert!(!bash_may_delete("git remote prune origin"));
        // 写但不删。
        assert!(!bash_may_delete("echo hi > out.txt"));
        assert!(!bash_may_delete("mv a.txt b.txt"));
    }

    #[test]
    fn certifies_only_provable_readonly_commands() {
        // 正例:能证明只读的命令(含 PowerShell 侧与 git 只读子命令)。
        for command in [
            "ls -la",
            "git status",
            "git status --short",
            "echo hi",
            "cat src/lib.rs",
            "wc -l src/lib.rs",
            "git log --oneline -5",
            "git branch",
            "git branch -a",
            "sort -u names.txt",
            "node --version",
            "Get-ChildItem -Recurse",
            "Test-Path target",
        ] {
            assert!(certified_readonly_command(command), "{command} 应判只读");
        }
        // 反例:写形态、未知命令、以及一切能藏写的形态,一律拒绝(fail-closed)。
        for command in [
            "",
            "rm -rf x",
            "echo hi > f",
            "echo hi >> f",
            "git commit -m x",
            "git branch -d feature",
            "git branch new-branch",
            "git diff --output=patch.diff",
            "sort -o out.txt in.txt",
            "sed -i s/a/b/ f.txt",
            "find . -name '*.tmp' -delete",
            "chmod 755 script.sh",
            "mkdir build && ls",
            "ls | tee list.txt",
            "ls | grep foo",
            "echo $(rm -rf x)",
            "python -c \"open('f','w')\"",
            "curl -X POST https://example.com",
            "node -e \"require('fs').writeFileSync('f','x')\"",
            "Get-ChildItem | Remove-Item",
            "cat f | sh",
        ] {
            assert!(!certified_readonly_command(command), "{command} 不应判只读");
        }
    }
}
