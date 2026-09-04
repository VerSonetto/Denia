//! 权限/沙箱策略的共享词汇(抄 dsh sandbox/escalation):
//!
//! - 三档权限模式:read-only / workspace-write / danger-full-access;
//! - 固定升权阶梯:read-only → workspace-write | danger-full-access,
//!   workspace-write → danger-full-access;
//! - 模型侧拒绝标记与升权提示,文本与 dsh 对齐,让模型认得同一套语义。
//!
//! 这一层只做策略判定,不做网络/审批通道;driver 负责把带
//! `sandbox_permissions` 的调用交给用户审批后再执行。

use denia_core::session::PermissionMode;

/// 被策略拒绝时的模型可见标记(与 dsh sandboxDenialMarker 同款)。
pub fn denial_marker(mode: PermissionMode) -> String {
    format!("[sandbox: file access denied under {} mode]", mode.as_str())
}

/// 拒绝后同一轮次的升权提示(与 dsh escalationHintMarker 同款)。
pub fn escalation_hint(subject: &str) -> String {
    format!(
        "[sandbox: escalation available — retry this exact {subject} once with sandbox_permissions (the narrowest wider mode that suffices) + justification; the approval prompt asks the user]"
    )
}

/// 校验 `sandbox_permissions`/`justification` 必须成对且理由非空(抄 dsh)。
pub fn validate_escalation_args(
    sandbox_permissions: Option<&str>,
    justification: Option<&str>,
) -> Result<(), String> {
    match (sandbox_permissions, justification) {
        (Some(_), None) => {
            Err("invalid escalation: sandbox_permissions requires a justification".to_string())
        }
        (None, Some(_)) => Err(
            "invalid escalation: justification is only valid together with sandbox_permissions"
                .to_string(),
        ),
        (Some(_), Some(reason)) if reason.trim().is_empty() => {
            Err("invalid justification: expected a non-empty sentence".to_string())
        }
        _ => Ok(()),
    }
}

/// 请求的模式是否严格宽于当前模式(抄 dsh WIDER_MODES)。
pub fn is_strictly_wider(current: PermissionMode, requested: PermissionMode) -> bool {
    match current {
        PermissionMode::ReadOnly => matches!(
            requested,
            PermissionMode::WorkspaceWrite | PermissionMode::DangerFullAccess
        ),
        PermissionMode::WorkspaceWrite => matches!(requested, PermissionMode::DangerFullAccess),
        PermissionMode::DangerFullAccess => false,
    }
}

/// 从字符串解析权限模式;未知值返回 None。
pub fn parse_permission_mode(value: &str) -> Option<PermissionMode> {
    match value.trim() {
        "read-only" => Some(PermissionMode::ReadOnly),
        "workspace-write" => Some(PermissionMode::WorkspaceWrite),
        "danger-full-access" => Some(PermissionMode::DangerFullAccess),
        _ => None,
    }
}

/// 启发式判断一条 bash 命令是否明显包含文件写副作用。
///
/// 这是本仓在无 OS 沙箱后端时的逻辑策略近似:read-only 下看到明显写
/// 命令会像 dsh 一样返回 `[sandbox: …]` 拒绝标记;模型随后可携带
/// `sandbox_permissions` 升权重试。无法静态证明的命令保持放行(与当前
/// bash 不受 OS 沙箱限制的现实一致,不做虚假承诺)。
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

    #[test]
    fn widening_ladder_is_closed() {
        assert!(is_strictly_wider(
            PermissionMode::ReadOnly,
            PermissionMode::WorkspaceWrite
        ));
        assert!(is_strictly_wider(
            PermissionMode::ReadOnly,
            PermissionMode::DangerFullAccess
        ));
        assert!(is_strictly_wider(
            PermissionMode::WorkspaceWrite,
            PermissionMode::DangerFullAccess
        ));
        assert!(!is_strictly_wider(
            PermissionMode::WorkspaceWrite,
            PermissionMode::ReadOnly
        ));
        assert!(!is_strictly_wider(
            PermissionMode::DangerFullAccess,
            PermissionMode::DangerFullAccess
        ));
    }

    #[test]
    fn pairing_validation() {
        assert!(validate_escalation_args(Some("workspace-write"), None).is_err());
        assert!(validate_escalation_args(None, Some("why")).is_err());
        assert!(validate_escalation_args(Some("workspace-write"), Some("  ")).is_err());
        assert!(validate_escalation_args(Some("workspace-write"), Some("need it")).is_ok());
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
