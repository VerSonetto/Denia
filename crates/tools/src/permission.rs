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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionClass {
    /// 读类:读文件/搜索/浏览器/无写副作用的命令。
    Read,
    /// 文件写,落点在工作区内。
    WriteInside,
    /// 文件写,落点在工作区外(自动编辑档唯一的 Ask 点)。
    WriteOutside,
    /// 有写副作用的命令(bash / job_start)。
    BashWrite,
    /// 提交计划(exit_plan)。
    PlanSubmit,
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
pub fn decide(mode: PermissionMode, class: ActionClass) -> Decision {
    use ActionClass::{BashWrite, PlanSubmit, Read, WriteInside, WriteOutside};
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
        // 完全访问档全部放行。
        (Full, _) => Allow,
        // 自动编辑:工作区内写与命令自动放行;越界写文件是唯一 Ask 点。
        (AutoEdit, WriteInside | BashWrite) => Allow,
        (AutoEdit, WriteOutside) => Ask,
        // 只读与计划:一切写类操作拒绝。
        (ReadOnly, WriteInside | WriteOutside | BashWrite) => Deny(
            "当前为只读模式,该操作会修改文件或产生写副作用,已被拒绝;请仅做阅读与分析,或请用户切换权限模式。".into(),
        ),
        (Plan, WriteInside | WriteOutside | BashWrite) => Deny(
            "当前为计划模式,禁止一切写操作与有写副作用的命令;请完成调研后调用 exit_plan 提交计划,等待用户批准后再执行。".into(),
        ),
    }
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
        }
        // 完全访问档全放行。
        for class in [Read, WriteInside, WriteOutside, BashWrite] {
            assert_eq!(decide(Full, class), Decision::Allow);
        }
        // 自动编辑:区内写与命令放行,越界写 Ask。
        assert_eq!(decide(AutoEdit, WriteInside), Decision::Allow);
        assert_eq!(decide(AutoEdit, BashWrite), Decision::Allow);
        assert_eq!(decide(AutoEdit, WriteOutside), Decision::Ask);
        // 只读与计划:写类全拒。
        for class in [WriteInside, WriteOutside, BashWrite] {
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
}
