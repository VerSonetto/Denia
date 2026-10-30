//! 工具动作语义的静态分类。
//!
//! 权限分类需要知道"这个工具在语义上做什么"——这是工具自身的知识,应当与
//! 它的 schema、描述一起维护:新增有副作用的工具时,漏配这里就会被默认按
//! 读类放行(历史教训:检查类命令曾因参数形状不通用而几乎落进"读类放行")。
//! 调度层只在此之上叠加运行时细化:路径内外、命令级读/写/删启发式。
//!
//! 与 [`crate::permission`] 的分工:permission 回答"这条调用该不该放行",
//! 本模块只回答"它属于哪一类动作"。

/// 工具动作的静态语义分类(不含路径、命令等运行时信息)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolActionClass {
    /// 提交计划,进入审批。
    PlanSubmit,
    /// 预设创作,落盘路径由服务端固定。
    PresetCreate,
    /// 写文件;写区内/区外/记忆域由调度层按路径判定。
    FileWrite,
    /// 执行命令;读/写/删由命令级启发式细化。
    Shell,
    /// 检查命令会真实执行,参数形状不通用(命令在 `checks[].command` 里):
    /// 直接按有写副作用归类,不借命令启发式。
    ShellChecks,
    /// 只读或会话内工具。
    Read,
}

/// 按工具名回答静态动作类别。
///
/// 未登记的工具按读类放行(与历史行为一致);新增有副作用的工具必须在此
/// 登记,否则会绕过审批与只读/计划档的限制。
pub fn tool_action_class(name: &str) -> ToolActionClass {
    match name {
        "exit_plan" => ToolActionClass::PlanSubmit,
        "create_preset" => ToolActionClass::PresetCreate,
        "write_file" | "edit" => ToolActionClass::FileWrite,
        "bash" | "job_start" => ToolActionClass::Shell,
        "run_checks" => ToolActionClass::ShellChecks,
        _ => ToolActionClass::Read,
    }
}

/// 大输出预览的两头字符数分配(头部, 尾部)。
///
/// 命令类工具(bash/job_start)的失败证据集中在输出尾部(报错、退出摘要),
/// 预览向尾部倾斜;文件类与其他工具保持两头均衡——头部通常是正文开头,
/// 尾部是截断处,两头都有用。只影响超预算后给模型的预览窗口,落盘全文
/// 不受此影响。
pub fn preview_split(name: &str) -> (usize, usize) {
    match name {
        "bash" | "job_start" => (4_000, 20_000),
        _ => (8_000, 16_000),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 副作用工具必须登记;未登记工具按读类放行是历史兼容行为。
    #[test]
    fn side_effecting_tools_are_classified() {
        assert_eq!(tool_action_class("exit_plan"), ToolActionClass::PlanSubmit);
        assert_eq!(
            tool_action_class("create_preset"),
            ToolActionClass::PresetCreate
        );
        assert_eq!(tool_action_class("write_file"), ToolActionClass::FileWrite);
        assert_eq!(tool_action_class("edit"), ToolActionClass::FileWrite);
        assert_eq!(tool_action_class("bash"), ToolActionClass::Shell);
        assert_eq!(tool_action_class("job_start"), ToolActionClass::Shell);
        assert_eq!(
            tool_action_class("run_checks"),
            ToolActionClass::ShellChecks
        );
        assert_eq!(tool_action_class("read_file"), ToolActionClass::Read);
        assert_eq!(tool_action_class("grep"), ToolActionClass::Read);
    }

    /// 预览分配:命令类向尾部倾斜,其余均衡;两头之和保持预算内的固定规模。
    #[test]
    fn preview_split_tilts_command_output_toward_the_tail() {
        assert_eq!(preview_split("bash"), (4_000, 20_000));
        assert_eq!(preview_split("job_start"), (4_000, 20_000));
        assert_eq!(preview_split("read_file"), (8_000, 16_000));
        assert_eq!(preview_split("web_fetch"), (8_000, 16_000));
    }
}
