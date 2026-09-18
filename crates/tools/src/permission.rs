//! 权限策略引擎:操作类别 × 权限模式 → 三值决策。
//!
//! - 四档权限模式:read-only / auto-edit / plan / full(见
//!   [`denia_core::session::PermissionMode`]);
//! - 操作类别由派发处(exec)在调用工具前判定,工具内部不再自行判权;
//! - 决策三值:Allow 直接执行 / Ask 弹审批(串行)/ Deny 携带模型可读
//!   的中文拒绝原因。
//!
//! 与 dsh 的显式分叉:不再使用模型侧 `sandbox_permissions` 升权协议——
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
    /// 组装创作(create_preset):落盘到部署 preset 目录。写入路径由服务端
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
    use ActionClass::{BashWrite, Delete, MemoryWrite, PlanSubmit, PresetCreate, Read, WriteInside, WriteOutside};
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
        // 自动编辑:区内文件写与组装创作自动放行。组装创作虽写在工作区外,
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
            "当前为只读模式,创建组装会写入文件,已被拒绝;请用户切换权限模式后再创建。".into(),
        ),
        (Plan, WriteInside | WriteOutside | BashWrite | Delete) => Deny(
            "当前为计划模式,禁止一切写操作与有写副作用的命令;请完成调研后调用 exit_plan 提交计划,等待用户批准后再执行。".into(),
        ),
        (Plan, PresetCreate) => Deny(
            "当前为计划模式,创建组装会写入文件,已被拒绝;请先退出计划模式再创建。".into(),
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
        // 自动编辑:区内文件写与组装创作放行;区外写/删除/bash 写三类审批。
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
        assert!(memory_path_is_sensitive(std::path::Path::new("/a/node_modules/x/y.md")));
        assert!(memory_path_is_sensitive(std::path::Path::new(
            "/home/u/.claude/skills/s.md"
        )));
        assert!(memory_path_is_sensitive(std::path::Path::new("/repo/Hooks/x.md")), "大小写折叠");
        // 记忆目录本体不命中( home 目录名 .denia 不在名单)。
        assert!(!memory_path_is_sensitive(std::path::Path::new(
            "/home/u/.denia/memories/projects/x/memory/a.md"
        )));
        // 子串不算命中。
        assert!(!memory_path_is_sensitive(std::path::Path::new("/ws/gitbook/notes.md")));
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
}
