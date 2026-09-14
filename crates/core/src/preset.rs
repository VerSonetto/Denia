//! Agent preset 词汇:决定一个会话"是什么 agent"的那份声明。
//!
//! denia 没有 dsh 那样的 Cordis 插件组装,agent 的组装就是两件事——工具面
//! 与 persona。preset 因此是一份声明式的白名单 + 可选的 persona 覆盖:
//!
//! - `tools: None` 表示出厂全量工具集(随部署新增工具自动跟进);
//! - `tools: Some([...])` 是白名单,纪律段与工具同进退(段名到工具的映射
//!   见 `denia_agent_loop::turn::section_tools`);
//! - `persona: None` 表示沿用 `SYSTEM.md` / 出厂 persona。
//!
//! preset 文件是输入,绝不是持久化目标:用户目录里的 preset 由用户自己的
//! 编辑器维护,denia 只读取、复制与删除。

use serde::{Deserialize, Serialize};

/// 未指定 preset 时使用的部署默认值。
pub const DEFAULT_PRESET_ID: &str = "standard";

/// preset id 语法:小写字母或数字开头,其后是小写字母、数字与单连字符
/// (`^[a-z0-9][a-z0-9-]*$`)。id 会成为目录名,因此这条检查发生在拼接
/// 路径之前,而不是事后审视拼出来的路径。
pub fn is_valid_preset_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    let Some(first) = bytes.first() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// preset 的来源信任级。
///
/// 随部署提供(`Shipped`)的 preset 拒绝写入与删除——它们正是用来对照
/// 出问题的本地 preset 的基线;只有用户根目录下的 `User` preset 可被
/// 复制来源、可被删除。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresetTrust {
    Shipped,
    User,
}

impl PresetTrust {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shipped => "shipped",
            Self::User => "user",
        }
    }
}

/// 一份 preset:会话的工具面 + 可选 persona 覆盖。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentPreset {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub trust: PresetTrust,
    /// 工具白名单;`None` = 出厂全量工具集。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// persona 正文;`None` = 沿用 `SYSTEM.md` / 出厂 persona。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// 用户 preset 的目录(展示与"打开目录"用);随附 preset 没有路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl AgentPreset {
    /// 该 preset 是否限制工具面(全量 preset 不限制)。
    pub fn restricts_tools(&self) -> bool {
        self.tools.is_some()
    }
}

/// 随部署提供的 preset 集合(按展示顺序)。
///
/// 与 dsh 的随附集合同构:`standard` 是全量组装,`minimal` 是精简组装。
/// `minimal` 保留 shell 与 `read_file` 两件——只给 shell 会让 `tool:bash`
/// 纪律段("读文本用 read_file")指向一个不存在的工具,纪律自相矛盾。
pub fn builtin_presets() -> Vec<AgentPreset> {
    vec![
        AgentPreset {
            id: DEFAULT_PRESET_ID.to_string(),
            name: "标准模式".to_string(),
            description: "功能完整的编码 Agent:文件读写、Shell、检索、计划、目标、子代理与后台任务。"
                .to_string(),
            trust: PresetTrust::Shipped,
            tools: None,
            persona: None,
            path: None,
        },
        AgentPreset {
            id: "minimal".to_string(),
            name: "极简模式".to_string(),
            description: "只保留 Shell 与文件读取的最小 Agent;没有编辑、检索、委派与计划工具。"
                .to_string(),
            trust: PresetTrust::Shipped,
            tools: Some(vec!["bash".to_string(), "read_file".to_string()]),
            persona: None,
            path: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_id_grammar() {
        assert!(is_valid_preset_id("standard"));
        assert!(is_valid_preset_id("minimal"));
        assert!(is_valid_preset_id("a-1-b"));
        assert!(is_valid_preset_id("1st"));
        // 与 dsh 的 id 规则一致:只约束首字符与字符集合,尾随连字符合法。
        assert!(is_valid_preset_id("trail-"));
        assert!(!is_valid_preset_id(""));
        assert!(!is_valid_preset_id("Standard"));
        assert!(!is_valid_preset_id("-lead"));
        assert!(!is_valid_preset_id("has_underscore"));
        assert!(!is_valid_preset_id("has space"));
        assert!(!is_valid_preset_id("../escape"));
        assert!(!is_valid_preset_id("dot.dot"));
    }

    #[test]
    fn builtin_presets_carry_the_default_id() {
        let presets = builtin_presets();
        assert!(presets.iter().any(|preset| preset.id == DEFAULT_PRESET_ID));
        assert!(presets
            .iter()
            .all(|preset| is_valid_preset_id(&preset.id)
                && preset.trust == PresetTrust::Shipped
                && !preset.name.is_empty()));
        let minimal = presets
            .iter()
            .find(|preset| preset.id == "minimal")
            .expect("minimal preset is shipped");
        assert!(minimal.restricts_tools());
        assert!(!presets[0].restricts_tools());
    }
}
