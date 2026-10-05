//! Console schema, validation and live driver configuration.
use denia_core::session::PermissionMode;
use denia_settings::SettingsStore;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// Console preferences namespace (settings page).
pub const CONSOLE_NS: &str = "console";

pub(crate) struct ConsoleDriverSettingsSource(pub(crate) Arc<SettingsStore>);

impl denia_agent_loop::DriverSettingsSource for ConsoleDriverSettingsSource {
    fn settings(&self) -> denia_agent_loop::DriverSettings {
        let console = console_settings(&self.0);
        denia_agent_loop::DriverSettings {
            compaction: compaction_settings_from(&console),
            microcompact: microcompact_settings_from(&console),
            parallel: denia_agent_loop::ParallelSettings {
                max_parallel_tool_calls: console.max_parallel_tool_calls,
            },
        }
    }
}

/// The `console` settings section shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleSettings {
    /// `system` | `light` | `dark`.
    #[serde(default = "default_theme")]
    pub theme: String,
    /// `zh` | `en`;zh 是源语言(学 dsh 的 i18n 约定)。
    #[serde(default = "default_locale")]
    pub locale: String,
    /// 层叠上下文管理(学 dsh 压力驱动 + Claude Code compact):
    /// 低压力不动(前缀缓存稳定),高压力 LLM 总结压缩。
    /// 全部字段带默认值;见 `ConsoleCompactionSettings`。
    #[serde(default)]
    pub compaction: ConsoleCompactionSettings,
    /// 工具结果微压缩:摘要之前的廉价清理。
    /// 全部字段带默认值;见 `ConsoleMicrocompactSettings`。
    #[serde(default)]
    pub microcompact: ConsoleMicrocompactSettings,
    /// 工具并行执行上限(学 codex `ToolCallRuntime` + dsh `maxParallelToolCalls`)。
    /// 一次 step 内模型返回的多个工具调用并发执行,受此上限约束。
    #[serde(default = "default_max_parallel_tool_calls")]
    pub max_parallel_tool_calls: usize,
    /// 新建会话的默认权限模式(四档,但不含 plan —— 计划模式是用户
    /// 在某次会话里显式进入的临时档位,不适合当全局默认值)。
    #[serde(default = "default_permission_mode")]
    pub default_permission_mode: String,
}

fn default_max_parallel_tool_calls() -> usize {
    10
}

/// 出厂默认权限模式:自动编辑(与会话日志的初始档位一致)。
pub fn default_permission_mode() -> String {
    PermissionMode::AutoEdit.as_str().to_string()
}

/// 设置里可选的新建会话默认档位(剔除 plan,见字段注释)。
pub const SELECTABLE_DEFAULT_PERMISSION_MODES: [&str; 3] = ["read-only", "auto-edit", "full"];

/// `console.compaction` 段(层叠上下文管理配置,默认对齐 Claude Code
/// auto-compact 缓冲语义)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConsoleCompactionSettings {
    /// LLM 总结压缩总开关。
    pub compact_enabled: bool,
    /// 压力 ≥ 窗口 × 该比例时触发 LLM 总结压缩。
    pub compact_ratio: f64,
    /// 压缩后保留窗口下限 token。
    pub compact_min_keep_tokens: u64,
    /// 压缩后保留窗口上限 token。
    pub compact_max_keep_tokens: u64,
    /// 保留窗口至少包含的文本消息数。
    pub compact_min_text_messages: usize,
    /// 摘要请求的输出预算。
    pub compact_summary_max_tokens: u64,
    /// 摘要请求 PTL/失败的截断重试上限。
    pub compact_max_attempts: u32,
}

impl Default for ConsoleCompactionSettings {
    fn default() -> Self {
        Self {
            compact_enabled: true,
            compact_ratio: 0.90,
            compact_min_keep_tokens: 10_000,
            compact_max_keep_tokens: 40_000,
            compact_min_text_messages: 5,
            compact_summary_max_tokens: 20_000,
            compact_max_attempts: 3,
        }
    }
}

/// `console.microcompact` 段(工具结果微压缩配置)。
///
/// 这是 LLM 摘要之前的廉价清理层:把已经用过的旧工具结果内容替换为
/// 占位符,不调模型、不重写历史。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ConsoleMicrocompactSettings {
    /// 微压缩总开关。
    pub microcompact_enabled: bool,
    /// 保留最近多少组工具结果(一组 = 一次 assistant 消息发起的全部调用)。
    pub microcompact_keep_recent_groups: usize,
    /// 空闲触发阈值(分钟):模型闲置超过这么久,下次进入时清理一次。
    pub microcompact_idle_threshold_minutes: u64,
    /// 最小节省 token 数:省不到这么多就不动手(保护 provider 前缀缓存)。
    pub microcompact_min_savings: u64,
    /// 可清理的工具白名单。
    pub microcompact_tools: Vec<String>,
}

impl Default for ConsoleMicrocompactSettings {
    fn default() -> Self {
        Self {
            microcompact_enabled: true,
            microcompact_keep_recent_groups: 5,
            microcompact_idle_threshold_minutes: 60,
            microcompact_min_savings: 256,
            microcompact_tools: denia_agent_loop::microcompact::default_compactable_tools(),
        }
    }
}

/// 把 console compaction 配置翻译成 driver 的 [`CompactionSettings`]。
pub fn compaction_settings_from(console: &ConsoleSettings) -> denia_agent_loop::CompactionSettings {
    let c = &console.compaction;
    denia_agent_loop::CompactionSettings {
        compact_enabled: c.compact_enabled,
        compact_ratio: c.compact_ratio,
        min_keep_tokens: c.compact_min_keep_tokens,
        max_keep_tokens: c.compact_max_keep_tokens,
        min_text_messages: c.compact_min_text_messages,
        summary_max_tokens: c.compact_summary_max_tokens,
        max_attempts: c.compact_max_attempts,
    }
}

/// 把 console microcompact 配置翻译成 driver 的 [`MicrocompactSettings`]。
pub fn microcompact_settings_from(
    console: &ConsoleSettings,
) -> denia_agent_loop::microcompact::MicrocompactSettings {
    let c = &console.microcompact;
    denia_agent_loop::microcompact::MicrocompactSettings {
        enabled: c.microcompact_enabled,
        keep_recent_groups: c.microcompact_keep_recent_groups,
        idle_threshold_minutes: c.microcompact_idle_threshold_minutes,
        min_savings: c.microcompact_min_savings,
        compactable_tools: c.microcompact_tools.clone(),
        clear_error_results: false,
    }
}

fn default_theme() -> String {
    "system".to_string()
}

fn default_locale() -> String {
    "zh".to_string()
}

pub(crate) fn validate_console(value: Value) -> Result<Value, String> {
    let parsed: ConsoleSettings = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if !matches!(parsed.theme.as_str(), "system" | "light" | "dark") {
        return Err(format!(
            "theme must be 'system', 'light', or 'dark'; got '{}'",
            parsed.theme
        ));
    }
    if !matches!(parsed.locale.as_str(), "zh" | "en") {
        return Err(format!(
            "locale must be 'zh' or 'en'; got '{}'",
            parsed.locale
        ));
    }
    let c = &parsed.compaction;
    if !(0.0..=1.0).contains(&c.compact_ratio) {
        return Err(format!(
            "compaction.compactRatio must be in [0, 1]; got {}",
            c.compact_ratio
        ));
    }
    if parsed.max_parallel_tool_calls == 0 || parsed.max_parallel_tool_calls > 64 {
        return Err(format!(
            "maxParallelToolCalls must be in [1, 64]; got {}",
            parsed.max_parallel_tool_calls
        ));
    }
    // 计划模式只在会话内显式进入,不允许设为新建会话的默认档位。
    let Some(mode) = PermissionMode::parse(&parsed.default_permission_mode) else {
        return Err(format!(
            "defaultPermissionMode must be one of {}; got '{}'",
            SELECTABLE_DEFAULT_PERMISSION_MODES.join(", "),
            parsed.default_permission_mode
        ));
    };
    if mode == PermissionMode::Plan {
        return Err(
            "defaultPermissionMode must not be 'plan' (plan mode is entered per session)"
                .to_string(),
        );
    }
    serde_json::to_value(parsed).map_err(|e| e.to_string())
}

/// 新建会话采用的权限模式:解析控制台默认值,非法值回落自动编辑。
pub fn console_default_permission_mode(settings: &SettingsStore) -> PermissionMode {
    PermissionMode::parse(&console_settings(settings).default_permission_mode)
        .filter(|mode| *mode != PermissionMode::Plan)
        .unwrap_or(PermissionMode::AutoEdit)
}

/// Reads the resolved console settings, tolerating an absent provider.
pub fn console_settings(settings: &SettingsStore) -> ConsoleSettings {
    settings
        .resolved(CONSOLE_NS)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or(ConsoleSettings {
            theme: "system".to_string(),
            locale: "zh".to_string(),
            compaction: ConsoleCompactionSettings::default(),
            microcompact: ConsoleMicrocompactSettings::default(),
            max_parallel_tool_calls: 10,
            default_permission_mode: default_permission_mode(),
        })
}
