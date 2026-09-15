//! Agent preset 词汇:决定一个会话"是什么 agent"的那份声明。
//!
//! denia 没有 dsh 那样的 Cordis 插件组装,dsh 用"组合里某一行存在与否"表达
//! 功能开关(没有 tool-jobs 行,agent 就没有后台任务);denia 的等价物是一份
//! 声明式清单:
//!
//! - `tools: None` 表示出厂全量工具集(随部署新增工具自动跟进);
//! - `tools: Some([...])` 是白名单,纪律段与工具同进退(段名到工具的映射
//!   见 `denia_agent_loop::turn::section_tools`);
//! - `features` 是功能开关(注入通道与运行时行为),每个开关连带摘除对应
//!   工具与纪律段——**features 先收窄,tools 白名单在其上继续收窄,交叠即
//!   收窄,绝不放大**;
//! - `persona: None` 表示沿用 `SYSTEM.md` / 出厂 persona;`personaComplete`
//!   让 persona 独占整个系统提示(工具 schema 保留,工具仍可用但没有指引)。
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

/// preset 的功能开关:工具面之外的注入通道与运行时行为,逐项可关。
///
/// 全部字段默认开启,省略整个 `features` 字段 = 全开,旧 preset 文件零改动
/// 兼容。未知键直接拒绝(fail loud):一个拼写错的开关默默不生效,比解析
/// 失败更难排查。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct PresetFeatures {
    /// AGENTS.md 工作区指令注入通道(对齐 dsh `agent-instructions` 行)。
    pub agents_md: bool,
    /// 项目记忆:`tool:memory` 段、MEMORY.md 索引注入与 MemoryWrite 放行。
    pub memory: bool,
    /// 上下文压缩:自动压缩 gate、microcompact 与手动 `/compact`。
    pub compaction: bool,
    /// goal 模式:工具、纪律段、状态注入与自动续跑。
    pub goal: bool,
    /// 技能:skill 工具、纪律段与技能目录注入。
    pub skills: bool,
    /// 子代理工具(spawn/fork/send/interrupt/list/wait)。
    pub subagents: bool,
    /// 后台任务工具与 bash 的 `run_in_background` 参数。
    pub jobs: bool,
    /// 浏览器工具。
    pub browser: bool,
    /// ask 提问工具。
    pub ask: bool,
    /// plan 模式的收口工具(exit_plan)。
    pub plan_mode: bool,
}

impl Default for PresetFeatures {
    fn default() -> Self {
        Self {
            agents_md: true,
            memory: true,
            compaction: true,
            goal: true,
            skills: true,
            subagents: true,
            jobs: true,
            browser: true,
            ask: true,
            plan_mode: true,
        }
    }
}

/// 后台任务工具族(`features.jobs` 关闭时摘除;与 tools crate 的
/// capabilities 注册保持同步)。
pub const JOB_TOOLS: &[&str] = &["job_start", "job_list", "job_output", "job_kill"];

/// 子代理工具族(`features.subagents` 关闭时摘除,同上)。
pub const SUBAGENT_TOOLS: &[&str] = &[
    "spawn_agent",
    "fork_agent",
    "send_message",
    "interrupt_agent",
    "list_agents",
    "wait_agent",
];

impl PresetFeatures {
    /// 全关(内置极简 preset 用)。
    pub fn all_off() -> Self {
        Self {
            agents_md: false,
            memory: false,
            compaction: false,
            goal: false,
            skills: false,
            subagents: false,
            jobs: false,
            browser: false,
            ask: false,
            plan_mode: false,
        }
    }

    /// 是否等于默认(全开):等价文本渲染时整个省略 features 字段。
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// 关闭的功能对应的工具名:装配时从工具面摘除,纪律段经段↔工具映射
    /// 联动消失(对齐 dsh"没有 tool-goal 行,goal 工具与指引一起不存在")。
    /// 列出部署不存在的名字无害——摘除是按交集收窄。
    pub fn excluded_tools(&self) -> Vec<&'static str> {
        let mut out: Vec<&'static str> = Vec::new();
        if !self.goal {
            out.extend(["get_goal", "update_goal"]);
        }
        if !self.skills {
            out.push("skill");
        }
        if !self.subagents {
            out.extend(SUBAGENT_TOOLS.iter().copied());
        }
        if !self.jobs {
            out.extend(JOB_TOOLS.iter().copied());
        }
        if !self.browser {
            out.push("browser");
        }
        if !self.ask {
            out.push("ask");
        }
        if !self.plan_mode {
            out.push("exit_plan");
        }
        out
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
    /// persona 独占:系统提示只留 persona 一段,压制部署身份、风格纪律与
    /// 全部工具指引;工具 schema 保留(工具仍可用,只是没有指引)。
    #[serde(default)]
    pub persona_complete: bool,
    /// 功能开关;省略 = 全开。
    #[serde(default)]
    pub features: PresetFeatures,
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

/// 创作模式(creator)的 persona:引导用户创建新 preset 的多轮 ask 流程。
///
/// 它是内置 creator preset 的部署 persona;具体工具纪律(`create_preset`
/// 与 `ask` 的分工)在 `tool:preset` 纪律段,这里只写角色与轮次结构。
pub const CREATOR_PERSONA: &str = "\
你是 denia 的组装（preset）创作助手：引导用户创建一个新的 agent preset，并把可直接使用的 preset.yml 落盘。

工作方式：
- 全程用 ask 工具与用户交互，每个问题都给出可点选的选项，能不让人打字就不让人打字。
- 第一轮只问系统提示词形态：选项覆盖「常规通用 persona」与「按用途定制 persona」。用户选定制时，再用一次 ask 请用户用一两句话描述用途——这是唯一必须打字的地方，persona 由你据此起草。
- 之后的轮次依次确认：工具面（给「全量工具 / 精简读写 / 仅 shell / 逐项挑选」等选项，逐项挑选时用多选列出常用工具）与功能开关（AGENTS.md 注入、项目记忆、压缩、目标、技能、子代理、后台任务、浏览器、提问、计划模式；给「全部开启 / 常用精简 / 极简全关 / 逐项挑选」等选项，逐项挑选时用多选）。
- 收集齐后，把将要写入的组装摘要（名称、id、persona 要点、开启的工具与关闭的功能）用 ask 向用户确认，确认后调用 create_preset 落盘。
- 创建成功后告诉用户：在设置的 Agent 预设页或新会话的选择器里即可选用；想进一步微调，直接编辑它的 preset.yml（把目录路径告诉用户）。

只做 preset 创作：不要在该会话里替用户执行其他编码任务。";

/// 随部署提供的 preset 集合(按展示顺序)。
///
/// 与 dsh 的随附集合同构:`standard` 是全量组装,`minimal` 是极简组装——
/// persona 一句话独占、功能全关、只留 shell(对齐 dsh minimal 的
/// `complete: true` + 持久 shell 形态);`creator` 让 Agent 引导用户创作
/// 新 preset(ask 收集 + create_preset 落盘)。
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
            persona_complete: false,
            features: PresetFeatures::default(),
            path: None,
        },
        AgentPreset {
            id: "creator".to_string(),
            name: "创造模式".to_string(),
            description: "让 Agent 帮你创建新组装:通过提问收集 persona、工具面与功能开关,直接落盘可用的 preset.yml。"
                .to_string(),
            trust: PresetTrust::Shipped,
            tools: None,
            persona: Some(CREATOR_PERSONA.to_string()),
            persona_complete: false,
            features: PresetFeatures::default(),
            path: None,
        },
        AgentPreset {
            id: "minimal".to_string(),
            name: "极简模式".to_string(),
            description: "只有持久 shell 的极简 Agent:系统提示一句话,无检索、记忆、计划与委派。"
                .to_string(),
            trust: PresetTrust::Shipped,
            tools: Some(vec!["bash".to_string()]),
            persona: Some("你是极简编码助手：直接、专注，只用 shell 完成用户的任务。".to_string()),
            persona_complete: true,
            features: PresetFeatures::all_off(),
            path: None,
        },
    ]
}

/// 创作新 preset 的规范:`create_preset` 工具的参数形状,也是 `preset.yml`
/// 磁盘格式的程序化入口。省略的字段与手写文件语义一致(tools 省略=全量,
/// features 里省略的键=开启)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PresetSpec {
    /// 新 preset 的 id(同时是目录名)。
    pub id: String,
    /// 显示名(名册与选择器展示)。
    pub name: String,
    /// 一句话说明该组装的用途(名册描述)。
    #[serde(default)]
    pub description: String,
    /// persona 正文;`None` = 沿用部署 persona。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// persona 独占(系统提示只留 persona 一段)。
    #[serde(default)]
    pub persona_complete: bool,
    /// 工具白名单;`None` = 出厂全量工具集。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    /// 功能开关;省略的键 = 开启。
    #[serde(default)]
    pub features: PresetFeatures,
}

impl Default for PresetSpec {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            description: String::new(),
            persona: None,
            persona_complete: false,
            tools: None,
            features: PresetFeatures::default(),
        }
    }
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
        let standard = presets
            .iter()
            .find(|preset| preset.id == DEFAULT_PRESET_ID)
            .expect("standard preset is shipped");
        assert!(!standard.restricts_tools());
        assert!(!standard.persona_complete);
        assert!(standard.features.is_default());
        let creator = presets
            .iter()
            .find(|preset| preset.id == "creator")
            .expect("creator preset is shipped");
        assert!(creator.restricts_tools() == false);
        assert!(creator.persona.as_deref().is_some_and(|text| text.contains("ask")));
        assert!(creator.features.is_default());
        let minimal = presets
            .iter()
            .find(|preset| preset.id == "minimal")
            .expect("minimal preset is shipped");
        assert!(minimal.restricts_tools());
        // 极简 = persona 独占 + 功能全关 + 只留 shell(对齐 dsh minimal)。
        assert!(minimal.persona_complete);
        assert_eq!(minimal.features, PresetFeatures::all_off());
        assert_eq!(minimal.tools.as_deref(), Some(&["bash".to_string()][..]));
    }

    #[test]
    fn features_default_to_all_on() {
        let features: PresetFeatures = serde_yaml::from_str("{}").unwrap();
        assert!(features.is_default());
        // 旧格式:整个 features 字段缺席 = 全开。
        let features: PresetFeatures = serde_yaml::from_str("agentsMd: true\n").unwrap();
        assert!(features.is_default());
    }

    #[test]
    fn features_parse_camel_case_keys_and_off_values() {
        let features: PresetFeatures = serde_yaml::from_str(
            "agentsMd: false\nmemory: false\nplanMode: false\n",
        )
        .unwrap();
        assert!(!features.agents_md);
        assert!(!features.memory);
        assert!(!features.plan_mode);
        assert!(features.compaction);
        assert_eq!(features.excluded_tools(), vec!["exit_plan"]);
    }

    #[test]
    fn features_reject_unknown_keys() {
        let error = serde_yaml::from_str::<PresetFeatures>("agentMd: false\n").unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn excluded_tools_covers_every_closed_feature_family() {
        let mut features = PresetFeatures::all_off();
        features.agents_md = true;
        features.memory = true;
        features.compaction = true; // 这三项不是工具面开关,不产生摘除。
        let excluded = features.excluded_tools();
        for name in ["get_goal", "update_goal", "skill", "spawn_agent", "fork_agent",
                     "send_message", "interrupt_agent", "list_agents", "wait_agent",
                     "job_start", "job_list", "job_output", "job_kill", "browser",
                     "ask", "exit_plan"]
        {
            assert!(excluded.contains(&name), "excluded_tools 缺 {name}");
        }
        // 这些开关由通道/段落层面处理,不进工具摘除清单。
        assert!(!excluded.contains(&"bash"));
        assert!(!excluded.contains(&"read_file"));
    }
}
