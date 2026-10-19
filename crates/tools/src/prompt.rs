//! System-prompt contributions for the shipped tool set.

use std::sync::Arc;

use denia_core::tool::ToolSchema;
use denia_system_prompt::{
    AssembleContext, PromptContext, PromptSection, PromptText, SectionAudience, SectionOrder,
    SystemPrompt, ToolProviderResult,
};

use crate::browser::BrowserHub;
use crate::shell;
use crate::{Tool, ToolRegistry};

/// 语言纪律的唯一权威正文。
///
/// 它平时由 `harness:communication` 承担,但那条段落在 persona 独占档
/// (`persona_complete` 会把系统提示收成只剩 `deployment:persona`)里会被
/// 一起摘掉,所以 persona 侧必须能把这句话随身带走。抽成常量是为了让两侧
/// 引用同一份字节:同一句话写两遍,改一处漏一处只是时间问题。
pub const LANGUAGE_RULE: &str = "默认简体中文，用户明确要求其他语言除外。";

/// Register tool schemas, tool guidance sections, runtime facts, and variables.
pub fn register_shipped_prompt(
    prompt: &mut SystemPrompt,
    tools: &ToolRegistry,
) -> Result<(), String> {
    prompt.variable("cwd", |context| context.cwd.clone())?;
    prompt.variable("model", |context| context.model.clone())?;
    prompt.variable("provider", |context| context.provider.clone())?;

    prompt.context(PromptContext {
        name: "harness:runtime".to_string(),
        order: 10,
        text: PromptText::Dynamic(Arc::new(runtime_context_text)),
    })?;
    prompt.context(PromptContext {
        name: "harness:permission".to_string(),
        order: 11,
        text: PromptText::Dynamic(Arc::new(|context| {
            let mode = context.permission_mode.as_deref().unwrap_or("auto-edit");
            match mode {
                "read-only" => "当前权限模式:只读。只开放 ls、read_file、glob、grep 等只读类工具,bash 与写文件工具不可用;请只做阅读、检索与分析,或请用户切换权限模式。".to_string(),
                "plan" => "当前权限模式:计划。禁止一切写操作与有写副作用的命令;先完成调研,再用 exit_plan 工具提交完整计划,等待用户审批。计划被批准后会话自动切换到执行模式,届时直接开始执行;被拒绝且用户附带了补充建议时,按建议修订后重新提交。".to_string(),
                "full" => "当前权限模式:完全访问。所有操作自动放行、无需审批,文件写入可以离开工作区;破坏性操作仍要先说明再执行。".to_string(),
                _ => "当前权限模式:自动编辑。工作区内的文件编辑(write_file/edit)自动放行;三类操作会先弹用户审批再执行:写工作区之外的文件、删除文件(rm/del 等,删除不可逆)、用 bash 增删改写文件。审批被拒时改用工作区内的方案。".to_string(),
            }
        })),
    })?;

    prompt.section(PromptSection {
        name: "tool:bash".to_string(),
        order: SectionOrder::ToolBash.value(),
        text: PromptText::Static(
            "- bash 只跑构建、测试、git、安装、启动等命令；列目录、找文件、搜内容、读文本分别用 ls/glob/grep/read_file。界限按动作划分而非命令名，job_start 同样适用，即使 bash 不可用。每次 shell 都是新进程，目录与变量不保留；服从本机方言，检查退出码，失败先排查再继续。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:output".to_string(),
        order: SectionOrder::ToolOutput.value(),
        text: PromptText::Static("- 工具输出预算 32,000 字符，截断后按本会话 output_id 用 read_tool_output 回读，bash 可选 stdout/stderr；offset 为 1-based 行号，每次默认 400 行、最多 1,000 行。保存不完整时不要假设产物含全部证据。".to_string()),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:ls".to_string(),
        order: SectionOrder::ToolLs.value(),
        text: PromptText::Static(
            "- ls 只列一层，深入时显式给 depth≤5；不猜路径，读取失败先 ls 再试。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:read".to_string(),
        order: SectionOrder::ToolRead.value(),
        text: PromptText::Static("- 读取前不声称已看过、不猜内容。同一分页未变更的重复读取只返回提示。".to_string()),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    // 对照 dsh context/file-reference 的 FILE_REFERENCE_PROMPT。dsh 原文对目录只写
    // "list it"(工具无关,因为它没有列目录工具);denia 有 ls,所以这里点名 ls——
    // 但要留神别再退回"用 bash 列目录":这条段落在工具纪律段之前注入,点名 bash
    // 等于抢在收口段前面给模型授权。
    prompt.section(PromptSection {
        name: "context:file-reference".to_string(),
        order: SectionOrder::FileReference.value(),
        text: PromptText::Static(
            "- 用户消息中 @ 明确引用工作区根目录相对路径；需要其内容时，末尾带“/”的用 ls，其余用 read_file；含空格写 @\"...\"。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:write".to_string(),
        order: SectionOrder::ToolWrite.value(),
        text: PromptText::Static(
            "- write_file 用于新建或整文件覆盖，覆盖前先读；已有文件本会话未读、或读取后被改动会拒写，按提示重读后再写，不原样重试。".to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:glob".to_string(),
        order: SectionOrder::ToolGlob.value(),
        text: PromptText::Static(
            "- glob 无“/”时按任意深度匹配 basename，只返回文件并按修改时间排序，不代替 ls。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:grep".to_string(),
        order: SectionOrder::ToolGrep.value(),
        text: PromptText::Static(
            "- grep 搜大目录可用 include，命中上限后收窄 pattern；命中文件读上下文，不用 shell 重搜。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:edit".to_string(),
        order: SectionOrder::ToolEdit.value(),
        text: PromptText::Static(
            "- 精确修改用 edit，两种格式严格二选一：{path,old_string,new_string,replace_all?} 或 {path,edits:[{old_string,new_string,replace_all?},...]}，不可混用、不传空 edits。同文件多处一次提交，按序且全有或全无；old_string 逐字符匹配（含空白换行），默认唯一，replace_all=true 才全替换。编辑成功即已落盘，不为验证重读；需要确认影响面用 grep 搜位置。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:webfetch".to_string(),
        order: SectionOrder::ToolWebFetch.value(),
        text: PromptText::Static(
            "- web_fetch 接收完整 URL，纯 HTTP 返回 Markdown 正文，不执行 JavaScript、不具备搜索能力。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:todo".to_string(),
        order: SectionOrder::ToolTodo.value(),
        text: PromptText::Static(
            "- 多步任务先用 todo_write 列可直接执行的具体动作，琐碎单步免建。每完成一步，在开始下一步的同一批工具调用里立即标 completed 并推进 in_progress，不滞后、不提前、不攒到最后。未完成时恰好一项进行中（真实并行的子代理/后台任务可多项），全部完成才可无进行项；任务范围变化时重发完整清单。todo不只是给用户看的，更是给你看的，你必须实时根据任务的进度更新它的状态"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    register_plan_prompt_section(prompt)?;
    register_goal_prompt_section(prompt)?;
    register_task_prompt_section(prompt)?;
    // 行为与表达纪律:放在工具段之后,约束"怎么推进工作"、"怎么说话"、
    // "上下文变长时怎么办"、"代码写成什么样"、"结果怎么报告",
    // 与工具用法不重叠。
    register_working_style_section(prompt)?;
    register_communication_section(prompt)?;
    register_context_management_section(prompt)?;
    register_code_style_section(prompt)?;
    register_risk_honesty_section(prompt)?;
    register_principle_and_group_sections(prompt)?;

    let schemas: Vec<ToolSchema> = tools.schemas();
    prompt.tools(move |_| ToolProviderResult {
        schemas: schemas.clone(),
        known_names: None,
    });
    Ok(())
}

/// v2 常驻层:决策原则段 + 各分组标题段(`## 行动与安全` 等)。
///
/// 分组标题独立成段、不绑任何工具(`section_tools` 返回 None),这样组内
/// 工具段被 preset 摘掉后标题仍在,不会留下半个空组;两者都是 Model
/// audience,不进用户可见副本。
pub fn register_principle_and_group_sections(prompt: &mut SystemPrompt) -> Result<(), String> {
    let groups = [
        (
            "harness:core-principles",
            SectionOrder::CorePrinciples.value(),
            CORE_PRINCIPLES,
        ),
        (
            "harness:group-action",
            SectionOrder::GroupAction.value(),
            GROUP_ACTION,
        ),
        (
            "harness:group-tools",
            SectionOrder::GroupTools.value(),
            GROUP_TOOLS,
        ),
        (
            "harness:group-progress",
            SectionOrder::GroupProgress.value(),
            GROUP_PROGRESS,
        ),
        (
            "harness:group-verify",
            SectionOrder::GroupVerify.value(),
            GROUP_VERIFY,
        ),
        (
            "harness:group-ask-plan",
            SectionOrder::GroupAskPlan.value(),
            GROUP_ASK_PLAN,
        ),
        (
            "harness:group-preset",
            SectionOrder::GroupPreset.value(),
            GROUP_PRESET,
        ),
        (
            "harness:group-comm",
            SectionOrder::GroupComm.value(),
            GROUP_COMM,
        ),
    ];
    for (name, order, text) in groups {
        prompt.section(PromptSection {
            name: name.to_string(),
            order,
            text: PromptText::Static(text.to_string()),
            complete: false,
            audience: SectionAudience::Model,
        })?;
    }
    Ok(())
}

/// 常驻决策原则:每次行动前的价值/成本/完成判断。
const CORE_PRINCIPLES: &str = "## 核心原则\n\
- 走最短可靠路径：规划、委派、额外验证都以收益大于时间与上下文成本为前提，不为流程而流程，也不为用工具而用工具。\n\
- 动手前明确用户要的结果、不可破坏的约束与完成标准；区分事实、假设与未知，优先核实会改变方案的未知，而不是全面铺开收集。\n\
- 每次调用前自问：它消除哪个关键未知、推进哪个交付？不会改变决定就不做；调试优先选能区分不同原因的最小实验，而非扩大搜索。\n\
- 失败是信息：必须改变假设或动作。输入与错误都没变时不重试同一调用。\n\
- 证据足以支持交付即停止，不为求安心扩大验证，也不重做已完成的工作。\n\
- 如实交代：失败给输出、跳过明说、已完成且验证过的直接陈述，不含糊。";

const GROUP_ACTION: &str = "## 行动与安全";

const GROUP_TOOLS: &str =
    "## 工具准则\n（参数、默认值、返回状态以工具定义为准，此处只写选择原则与不可违反的边界）";

const GROUP_PROGRESS: &str = "## 进度、协作与目标";

const GROUP_VERIFY: &str = "## 验证与完成判断";

const GROUP_ASK_PLAN: &str = "## 提问与计划";

const GROUP_PRESET: &str = "## 预设创建";

const GROUP_COMM: &str = "## 沟通与交付";

/// 宿主能力工具(子代理委派 / 后台任务 / 技能)的工具纪律段。
///
/// server 部署总是注册这些工具,由 prompt store 在构建系统提示词后显式
/// 调用本函数归位纪律;无 runtime 的部署(如测试)不注入,模型不会看到
/// 不存在的工具。此前这段纪律以 `[denia 能力上下文]` 注入消息落盘,
/// 现在归位到系统提示词的 Model audience(与 tool:bash 等段落同性质)。
pub fn register_capability_prompt_sections(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:agents".to_string(),
        order: SectionOrder::ToolAgents.value(),
        text: PromptText::Static(
            "- 委派要算成本：只有子任务可独立完成、边界清楚，且收益大于启动、上下文复制与集成开销时，才用 spawn_agent/fork_agent；主代理能直接做完的短任务不派。尽量按不重叠的文件或模块分工，由主代理统一集成；send_message 仅限直接父子。类型按注入的子代理目录选择：默认 develop 实现与修复（可写可跑命令），explore 只读调查，verify 验证；inline 为临时定义，不落盘。\n\
             - 子代理继承系统提示基础、工作目录、权限模式与模型默认值，角色提示是追加而非替换；不自动继承全局 AGENTS.md，只自动发现项目级规则。不得再派遣子代理，也不得使用宿主配置与会话主控工具。结果必须区分成功、失败与待主代理决策；需要落盘改动或与用户确认的关键决定留在主代理。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:jobs".to_string(),
        order: SectionOrder::ToolJobs.value(),
        text: PromptText::Static(
            // 段文本自足,不回指 `tool:agents`——subagents 关闭时那段不存在,
            // 悬空引用会让模型去找一段没有的东西。
            //
            // 探索边界在这里再说一遍,补只读/计划档的空档:那两个档会摘掉
            // `tool:bash`,而 job_start/job_output/job_kill 仍在工具面、与 bash
            // 共用同一套命令启发式(读类命令放行)。v2 把「按动作划分」的口径写在
            // tool:bash 段里并声明「即使 bash 不可用」,但那段本身在只读档就
            // 不存在——所以这里必须自足成句,否则那个声明在两个档下落空。
            "- 委派与后台任务都后台执行并立即返回：wait_agent 只查看当前结果；长命令用 job_start 启动、job_output 取当前输出（wait=true 也不前台等待）、job_kill 停止。未完成返回 pending、完成后自动送达，ready 仅表示结果可读取，两者都不代表成功。不轮询、不重复等待、不用 sleep 占住轮次；可继续独立工作或报告进度结束本轮，结果到达再续，未完成前不宣称完成。\n\
             - 后台命令不是探索工具：列目录、找文件、搜内容、读文本仍用 ls/glob/grep/read_file，界限按动作划分而非命令名，bash 不在工具面时（只读/计划档）这条对 job_start 同样成立。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:skill".to_string(),
        order: SectionOrder::ToolSkill.value(),
        text: PromptText::Static(
            "- 开工先核对 <available_skills>：与任务匹配则先 skill load，不匹配直接处理，不为用技能而调用技能。load 已返回 SKILL.md 全文，只加载一次、不另读；其余参考资料用 resource，路径相对返回的 resourceBase。技能不授予额外权限。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 浏览器工具(browser)的使用纪律段。
///
/// 段与 browser schema 严格同步:仅在带 hub 的 builder 里注册(hub 为 Some
/// 才有工具),无 browser 的部署模型不会看到不存在的工具。
///
/// 机制上通过 CDP 驱动**本机 Chrome/Edge**(headless 无窗口后台运行,画面
/// 走控制台侧边栏),本段负责让模型明白:引用何时失效、收尾该关哪些 tab、
/// 实例进程为什么必须回收、什么时候才该开可视化模式。
fn register_browser_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:browser".to_string(),
        order: SectionOrder::ToolBrowser.value(),
        text: PromptText::Static(
            "- browser 由 CDP 驱动本机 Chrome/Edge，无窗口后台运行，画面仅在控制台侧栏；交互命令会按需启动，未运行时 list/shutdownBrowser 等查询与善后原地返回，不凭空拉起实例。snapshot 的 [ref=eN] 直接传 eN（也接受 @eN），仅对同一 tab 的最近一次快照有效；导航、提交、SPA 路由切换后必须重新 snapshot。\n\
             - 任务完成或确认不再使用时：逐个 close 本次打开的 tab，并以 list 确认它们全部消失（用户原有的 tab 原样保留，绝不关闭不是自己打开的 tab），然后**必须**调 shutdownBrowser 回收浏览器进程，再用 list 确认（未运行时它返回空 tabs 且 running=false，而 getState 会按需拉起实例，收尾时不要用它）。只 close 不算收尾：实例占住 profile 的单例锁，没回收会让后续启动直接失败；关掉最后一个 tab 会自动回收，但不要依赖它——只要还剩别的 tab（比如用户自己开的），进程就仍在跑。\n\
             - 仅在用户明确要求观看或明显需要亲眼看画面时设 visualMode:true，其余保持后台；用户手动收起后不再重复请求展开。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 提问工具(ask)的使用纪律段。
///
/// 与 `ask` schema 严格同步:仅在注册了该工具的部署注入。纪律与 description
/// 分工不重叠——description 写调用机制与结局语义,本段写"什么时候该问、
/// 什么时候不该问、拿到各结局怎么办"的行为准则。
pub fn register_ask_prompt_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:ask".to_string(),
        order: SectionOrder::ToolAsk.value(),
        text: PromptText::Static(
            "- ask 只在真正卡住时用：需求自相矛盾、破坏性操作需要拍板、缺少只有用户知道的信息。能自己查证的事实不要问，能给出合理默认的先做并把假设写进结论；同批问题一次问全，每问都要能说清答案会如何改变下一步，不以提问拖延。\n\
             - 拿到答复后：answered 按选择继续；timed-out 不原样重复同一问题，据现有信息继续并写明采用的假设；cancelled 视为不要沿该路径继续，停下说明状态与可选方案；unavailable 自行决策并标注假设；skipped 按默认继续，不把跳过当成需要再问。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 预设创作工具(create_preset)的纪律段。
///
/// 与 `create_preset` schema 严格同步:仅在注册了该工具的部署(控制台,
/// 有 preset 名册)注入。纪律与 description 分工不重叠——description 写
/// 参数机制与落盘行为,本段写"何时创建、怎么和 ask 配合收集、禁止什么、
/// 创建后怎么收尾"的行为准则;角色与轮次结构由创造模式 preset 的 persona
/// 承担。
pub fn register_preset_prompt_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:preset".to_string(),
        order: SectionOrder::ToolPreset.value(),
        text: PromptText::Static(
            "- create_preset 只用于用户明确要求创建新的 agent preset：一次会话聚焦一个，不主动提议，不用于修改或删除已有预设，也不绕道写工作区文件；改名与微调引导用户在设置 → 预设页编辑，或直接改 preset.yml。\n\
             - 创建前先用 ask 分轮收集需求：首轮问系统提示词形态，之后问工具面与功能开关；每题给可点选项，可多选的用 multiSelect，尽量让用户少打字。信息齐后先用 ask 展示摘要（名称、id、persona 要点、开启的工具与关闭的功能）取得确认，再调用工具。\n\
             - 参数语义与手写 preset.yml 一致：tools 省略 = 全量工具集；features 省略的键 = 开启，只写要关闭的键；id 只允许小写字母、数字与连字符（同时是目录名）；name 与 description 用中文，description 一句话说清用途。\n\
             - 成功后告知用户可在设置 → 预设页或新会话选择器切换到它，微调可点「编辑」或直接改 preset.yml，并报告工具返回的目录路径；失败（如 id 已被占用）转告可读原因并给出替代 id，不静默换名重试。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// MCP 外部工具的使用纪律段。
///
/// 与实际注册的工具严格同步:仅在部署有 MCP 管理器时注入(无 MCP 的部署
/// 模型不会看到不存在的工具)。段文本是**动态的**:纪律部分固定,首行的
/// "已接入 MCP 服务器"目录按当前快照渲染——工具清单默认不随请求下发,
/// 服务器名册常驻可见,模型据此知道"接入了什么能力",需要时再用 mcp_list
/// 查具体工具。纪律与 description 分工不重叠:description 写调用机制与
/// 分页字段,本段写"这些工具是什么、什么时候用、失败怎么办"的行为准则。
pub fn register_mcp_prompt_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:mcp".to_string(),
        order: SectionOrder::ToolMcp.value(),
        text: PromptText::Static(render_mcp_section(&[])),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 带连接目录的 MCP 纪律段:文本每次装配按 `McpManager` 快照实时渲染。
pub fn register_mcp_prompt_section_with_manager(
    prompt: &mut SystemPrompt,
    manager: Arc<denia_mcp::McpManager>,
) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:mcp".to_string(),
        order: SectionOrder::ToolMcp.value(),
        text: PromptText::Dynamic(Arc::new(move |_| {
            render_mcp_section(&connected_server_ids(&manager))
        })),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 当前已连接且启用的 MCP 服务器 id(快照同步读,无 await)。
fn connected_server_ids(manager: &denia_mcp::McpManager) -> Vec<String> {
    let snapshot = manager.snapshot();
    snapshot
        .servers
        .iter()
        .filter(|server| server.enabled && server.status == denia_mcp::McpServerStatus::Connected)
        .map(|server| server.id.clone())
        .collect()
}

/// 纪律段文本(纯函数):目录行 + 固定纪律。
pub fn render_mcp_section(server_ids: &[String]) -> String {
    let catalog = if server_ids.is_empty() {
        "(当前没有已连接的 MCP 服务器)".to_string()
    } else {
        server_ids.join("、")
    };
    format!(
        "当前已接入的 MCP 服务器:{catalog}。\n\
         - mcp__<服务器>__<工具> 是用户接入的第三方能力，不是内置功能，也不向用户解释“用 MCP”。同名能力内置优先，内置做不到才用外部。先用 mcp_list（可按 server 筛选）发现，再按清单名称调用；首次调用只返回 JSON Schema、不执行，按定义尽快重新调用执行，不猜参数、不反复装载同一工具。长结果按字符分页，用尾部给出的下一 offset 调同一工具且其他参数不变，否则会重新执行；优先收窄查询，不靠翻页从头读到尾。失败先读错误里的可执行建议（常需在设置 → MCP 检查或重连），不反复重试同一调用，换内置工具或请用户处理。"
    )
}

/// 计划呈交工具(exit_plan)的纪律段。
///
/// 与 `exit_plan` schema 严格同步注册(default_registry 总是带该工具);
/// 段的动态进退(仅计划模式可见)由 assemble 阶段按权限模式过滤。
/// 纪律与 description 分工不重叠:description 写调用机制,本段写
/// "何时提交、计划长什么样、拿到各决策怎么办"的行为准则。
pub fn register_plan_prompt_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:plan".to_string(),
        order: SectionOrder::ToolPlan.value(),
        text: PromptText::Static(
            "- 在计划模式且调研完成后，必须一次用 exit_plan 提交完整 Markdown 计划（目标、分步方案、将修改或新建的文件、风险、验证方式），提交后阻塞等待决策，期间不调用其他工具。适用于需要用户拍板的中大型改动：跨多个文件或模块、多个可行方案待定、需求含糊需先调研、架构或接口层面的选择；单行修复、错别字、做法明确唯一、纯问答与检索不必提交。非计划模式下会被拒绝。\n\
             - 计划获批就直接执行、不再确认，附带建议一并落实；被拒绝且附建议时只改受影响部分后重新提交（除非用户要求推倒重来），无建议则先用 ask 问清顾虑。获批执行后不再重新提交，除非用户明确要求重进计划模式；执行中发现必须偏离计划（方案走不通、需额外破坏性操作），停下说明现状与建议，不擅自扩大范围。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 会话目标工具(get_goal/update_goal)的纪律段。
///
/// 与 `default_registry` 严格同步:两个工具在所有部署注册,本段同样总是
/// 注入(与 tool:bash 等基础段同性质)。目标详情(objective/状态/用量)
/// 走 `channel: "goal"` 上下文注入,无目标的会话模型不会看到目标语境。
pub fn register_goal_prompt_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:goal".to_string(),
        order: SectionOrder::ToolGoal.value(),
        text: PromptText::Static(
            "- 有目标时以达成目标为先：每轮先看最新 [denia 目标] 的目标、状态、轮次与预算，动手前用 get_goal 确认并以返回值为准，不虚构状态、不原地等待指示。目标达成即 update_goal complete；外部条件阻塞且无法绕开则 blocked 并写清阻塞条件，不空耗预算。用户插话是对目标的调整，必要时用 edit 同步目标文本。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 任务验证账本工具(`run_checks`)的纪律段。
///
/// 与 `default_registry` 严格同步:工具在所有部署注册,本段同样总是注入
/// (与 tool:goal 这些基础段同性质)。preset 的 `taskLedger` 开关关闭时,工具
/// 与本段一起消失(段↔工具映射见 `denia_agent_loop::turn::section_tools`)。
///
/// 纪律与 description 分工不重叠:description 写参数机制,本段写“什么时候该跑
/// 检查、结论由谁写、什么做不到”。
pub fn register_task_prompt_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:task".to_string(),
        order: SectionOrder::ToolTask.value(),
        text: PromptText::Static(
            "- 改完代码要主张完成，必须跑 run_checks：给出命令、期望退出码、工作目录与超时，由宿主实际执行并据退出码把结论写进账本；模型侧没有写结论的入口，不能自称验证通过。\n\
             - 检查的选择以覆盖用户目标为准。一条通过的检查只证明它真跑过的那件事，不用局部通过冒充整件任务完成；失败就如实报告，不绕过检查，也不把没跑完的命令说成通过。\n\
             - 账本只按最新一条有效结论裁决（未验证完成/验收通过/验收失败），后来失败的检查会作废先前的通过，不要挑一条通过的就当完成。输入快照只覆盖工作目录与文件内容指纹、不含环境变量，相关文件改动后须重跑。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 项目记忆段(`# 项目记忆`)。
///
/// 与记忆能力的装配严格同步:仅在 server 部署注册(system_prompt_store
/// build_prompt),运行中的进退与路径内联由 agent-loop 的 assemble 阶段
/// 处理——记忆启用时按 runtime.memory_root_for 换成 `render_memory_section`
/// 的带路径版本(同会话 cwd 不可变,字节稳定不破缓存),关闭时整段摘除。
/// 注册基座是不带路径的同一正文,仅作替换失败时的兜底。
/// 纪律与工具 schema 分工不重叠:write_file/edit 的用法各自段落已讲,
/// 本段写"这是你自己的记忆系统、什么时候写、怎么写、怎么维护索引"。
pub fn register_memory_prompt_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:memory".to_string(),
        order: SectionOrder::ToolMemory.value(),
        text: PromptText::Static(render_memory_section(None)),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 渲染 `## 项目记忆` 段正文。`root` 为 Some 时把记忆目录路径内联进段,
/// 模型在任何 step 都能直接看到目录在哪;None 时指向「项目记忆」注入
/// 消息。段覆盖:该记什么(带判据)、frontmatter 格式、[[链接]] 语义、
/// 四类型、索引格式、去重与删除义务、不该记什么(带判据)、引用前验证义务。
///
/// # 为什么"该记"与"不该记"两侧都要给判据
///
/// 记忆的产出完全由模型主动决定(没有后台提取兜底),所以纪律必须把
/// **判断标准**写清楚,而不是只说"记有用的"。只强调一侧会失衡:
/// - 只写"多记"→ 流水账、把仓库已有的事实抄一遍、每条都记等于没记;
/// - 只写"别乱记"→ 模型趋于保守,真正该沉淀的纠正与决策全丢掉,
///   记忆目录长期空着(这正是加这段之前实测的状态)。
///
/// 两侧都给具体判据 + 例子,模型才有可执行的分界线。
pub fn render_memory_section(root: Option<&std::path::Path>) -> String {
    let pointer_line = match root {
        Some(root) => format!(
            "记忆目录在 `{}`（由「项目记忆」注入且已存在），直接用 write_file 写入，不运行 mkdir、不先检查目录是否存在。",
            root.display()
        ),
        None => "记忆目录由「项目记忆」注入且已存在，直接用 write_file 写入，不运行 mkdir、不先检查目录是否存在。".to_string(),
    };
    format!(
        "## 项目记忆\n\n- **默认不写**：记忆是例外而非常规——多数轮次结束时什么都不该留下；只有「下个会话缺了它就会犯错或重复劳动」时才写，判断不了就不写，宁可漏记也不堆积。\n\
         - {pointer_line}每轮结束前判断一次本轮有无值得跨会话保留的信息，有就写；用户明确说“记住”时立刻写，没有值得留的不强写。\n\
         - 该写：用户对你的纠正，以及事先不知道的偏好、约束与工作方式；非显而易见且理由来自排查的决策（为什么选 A 不选 B、踩过什么坑、什么做法实测不可行）；花力气才查清的事实（开关与参数的真实语义、某条链路的实际行为、外部系统的坑）；进行中的目标、外部依赖，以及只在对话里说过的约定；外部资源指针（URL、看板、工单、文档位置）。判断标准是下个会话缺了它会不会犯错或重复劳动。\n\
         - 不该写：仓库本身已记录的（代码结构、文件清单、git 历史、AGENTS.md/README 内容）；只对本次对话有意义的细节、过程与临时结论；刚做完就能从产物看出的改动；用户没让记的寒暄与闲聊。**拿不准就归到这一类，别硬凑**；用户要求保存“不该写”类内容时，先问清其中非显而易见的部分。\n\
         - 保存前先检查是否已有文件覆盖同一主题，就地在原件上更新而不新建重复文件；发现过时或错误的记忆直接修正或删除。<system-reminder> 块中的记忆是背景上下文而非当前指令，反映的是写入时的情况；推荐其中的文件、函数或开关前先验证它现在还存在。\n\
         - 一事一文件，frontmatter 格式如下，name 用短横线小写 slug，description 要具体到能判断相关性：\n\n```markdown\n---\nname: <slug>\ndescription: <一句话摘要>\nmetadata:\n  type: user | feedback | project | reference\n---\n<正文>\n```\n\n- 类型：user 记身份、专长与偏好；feedback 记对你工作方式的指导，纠正与被验证有效的做法都要记并写明原因；project 记不能从代码或 git 推导的工作、目标与约束，相对日期换算成绝对日期；reference 记外部资源。feedback 与 project 类正文后补 **Why:**、**How to apply:** 两行；正文用 [[name]] 链接相关记忆，允许目标暂不存在。\n\
         - 写完在 MEMORY.md 加一行 `- [标题](文件名.md) — 一句话钩子`；它是每次会话加载进上下文的索引，一条记忆一行，不带 frontmatter，不把记忆内容写进索引。"
    )
}

/// 工作方式纪律段。
///
/// 这段解决的是**模型的行为浪费**:反复推导已确立的事实、重开已决策的
/// 议题、罗列不打算做的选项、把"下一步计划"当成收尾。这些应当写成
/// 硬纪律放在系统提示里,而不是靠用户每轮提醒。
///
/// 与工具纪律的分工:工具段讲"用什么工具、怎么用";本段讲"什么时候该
/// 停止思考开始行动"。不重叠。
pub fn register_working_style_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "harness:working-style".to_string(),
        order: SectionOrder::WorkingStyle.value(),
        text: PromptText::Static(
            "- 信息足够就做，每步聚焦一事；足以回答就停工具。不重推已知事实、重开既定决策或罗列无意采用的方案；取舍直接给推荐并说明理由。\n\
             - 原范围内可逆的动作直接做，不问“要不要”；真正变更范围才询问。用户只在描述、提问或思考时，交付判断后停止，不顺手修复。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 输出与沟通纪律段。
///
/// 这段解决的是**表达质量**:模型容易把过程笔记当交付、把结论埋在最后、
/// 为了简洁牺牲可读性。
pub fn register_communication_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "harness:communication".to_string(),
        order: SectionOrder::Communication.value(),
        text: PromptText::Static(format!(
            "- {LANGUAGE_RULE}首次调用工具前用一句话说明要做什么；发现关键信息或改变方向时给简短更新。\n\
             - 工具调用之间的文字可能不展示给用户：本轮用户需要的一切——答案、总结、发现、结论、交付物——必须落在最后一条文字消息里，且其后不再有工具调用；中间文字只保留简短状态说明。\n\
             - 先给结论，再给依据。面向刚离开工位、正在补进度的同事：不假定用户看过你的思考或原始输出，不写你中途发明的代号与简写，让内容一次读通。\n\
             - 可读优先于短：删掉不影响读者下一步动作的细节，但不要用碎片、私有缩写、箭头链或行话代替完整句子与技术术语。\n\
             - 详略匹配问题与读者：简单问题一段话直接答，不上标题与分节；表格只用于简短可枚举的事实，解释放在表格外的正文里；对专家可以紧一些，对新手多解释几句。\n\
             - Markdown 语法写完整：标题的 # 后加空格，标题、表格、列表、代码围栏各自独占一行；写歪的语法会把标记原样展示给用户。"
        )),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 上下文管理纪律段。
///
/// 与 `agent-loop` 的压缩机制配套:压缩是自动的、无损于工作连续性的,
/// 但模型不知道这件事时,会在上下文变长时本能地收尾、交接或急着做总结——
/// 那是把一次可继续的工作提前掐断。这段把机制本身告诉模型。
pub fn register_context_management_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "harness:context-management".to_string(),
        order: SectionOrder::ContextManagement.value(),
        text: PromptText::Static(
            "- 不以未执行的计划、分析、提问或承诺收尾：能做、能重试、能自行补信息就做，仅完成或确需用户提供信息时结束（后台等待例外见下）。长对话会以摘要和剩余历史续接，不因上下文变长提前收尾、交接或急作阶段总结。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 代码风格纪律段。
///
/// 两条约束针对的是两类常见的"写给审查者而不是下一个读者"的噪音:
/// 风格与周围代码脱节、注释解释代码已经说清的事。
pub fn register_code_style_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "harness:code-style".to_string(),
        order: SectionOrder::CodeStyle.value(),
        text: PromptText::Static(
            "- 写代码像周围的代码：匹配它的命名、惯用法与注释密度；只在代码本身表达不了的约束上写注释，不写来源、不逐行释义、不为正确性辩护。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// 风险与诚实纪律段。
///
/// 只保留与**工程判断**直接相关的部分:不可逆/对外操作的确认门槛、
/// 删除前先核对目标、以及如实报告结果。不含任何要求模型审查用户意图
/// 是否合规的内容——那类内容会把注意力从工程问题上挪走。
pub fn register_risk_honesty_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "harness:risk-honesty".to_string(),
        order: SectionOrder::RiskHonesty.value(),
        text: PromptText::Static(
            "- 重启、删除、改配置等状态变更须有证据支持，不凭与已知故障相似就动手。难撤销或对外的操作先确认，已有持续授权或明确执行要求除外；单次批准不跨上下文。发往外部即发布，删除也可能留有缓存或索引。删除、覆盖前先看目标，与描述不符或非自己创建时先说明。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// Shipped registry pair: prompt assembly plus executable tools.
pub fn default_shipped() -> (SystemPrompt, ToolRegistry) {
    let tools = crate::default_registry();
    let mut prompt =
        denia_system_prompt::SystemPrompt::new(denia_system_prompt::SystemPromptConfig::default());
    register_shipped_prompt(&mut prompt, &tools).expect("shipped prompt registrations are valid");
    (prompt, tools)
}

/// Shipped pair + browser tool(`hub` 提供时,registry 与 prompt schemas 同步带上 browser)。
///
/// `default_shipped()` 的 prompt 在 `register_shipped_prompt` 里固化了当时 registry
/// 的 schemas;此处对同一 prompt 追加 browser schema,保证模型可见与可执行一致。
pub fn default_shipped_with_browser(hub: Option<BrowserHub>) -> (SystemPrompt, ToolRegistry) {
    let (mut prompt, tools) = default_shipped();
    if let Some(hub) = hub {
        register_browser_section(&mut prompt).expect("browser prompt section is valid");
        let tool = Arc::new(crate::BrowserTool::new(hub));
        let schema = tool.schema().clone();
        prompt.tools(move |_| ToolProviderResult {
            schemas: vec![schema.clone()],
            known_names: None,
        });
        let mut registry = tools;
        registry.register(tool);
        return (prompt, registry);
    }
    (prompt, tools)
}

/// Shipped pair + optional `ask` 工具。
///
/// `ask` 是交互式工具:只有带应答通道的部署(server + 控制台)才注册,
/// 纪律段与 schema 在同一分支注册,模型不会看到不存在的工具。
pub fn default_shipped_with_browser_and_ask(
    browser_hub: Option<BrowserHub>,
    ask: bool,
) -> (SystemPrompt, ToolRegistry) {
    let (mut prompt, mut registry) = default_shipped_with_browser(browser_hub);
    if ask {
        register_ask_prompt_section(&mut prompt).expect("ask prompt section is valid");
        let tool = Arc::new(crate::AskTool::new());
        let schema = tool.schema().clone();
        prompt.tools(move |_| ToolProviderResult {
            schemas: vec![schema.clone()],
            known_names: None,
        });
        registry.register(tool);
    }
    (prompt, registry)
}

/// 自定义系统提示词正文 + 同一套 shipped 工具与工具纪律段(不含单独的 harness:identity)。
pub fn shipped_with_persona(persona_text: String) -> (SystemPrompt, ToolRegistry) {
    let tools = crate::default_registry();
    let mut prompt = denia_system_prompt::SystemPrompt::new_with_persona(
        denia_system_prompt::SystemPromptConfig {
            include_harness_identity: false,
            ..Default::default()
        },
        persona_text,
    );
    register_shipped_prompt(&mut prompt, &tools).expect("shipped prompt registrations are valid");
    (prompt, tools)
}

/// `shipped_with_persona` + browser tool(schema 同步进 prompt,registry 同步注册)。
pub fn shipped_with_persona_and_browser(
    persona_text: String,
    hub: BrowserHub,
) -> (SystemPrompt, ToolRegistry) {
    shipped_with_persona_and_browser_and_ask(persona_text, Some(hub), false)
}

/// `shipped_with_persona_and_browser` + 可选 `ask` 工具。
pub fn shipped_with_persona_and_browser_and_ask(
    persona_text: String,
    hub: Option<BrowserHub>,
    ask: bool,
) -> (SystemPrompt, ToolRegistry) {
    let (mut prompt, mut registry) = shipped_with_persona(persona_text);
    if ask {
        register_ask_prompt_section(&mut prompt).expect("ask prompt section is valid");
        let tool = Arc::new(crate::AskTool::new());
        let schema = tool.schema().clone();
        prompt.tools(move |_| ToolProviderResult {
            schemas: vec![schema.clone()],
            known_names: None,
        });
        registry.register(tool);
    }
    if let Some(hub) = hub {
        register_browser_section(&mut prompt).expect("browser prompt section is valid");
        let tool = Arc::new(crate::BrowserTool::new(hub));
        let schema = tool.schema().clone();
        prompt.tools(move |_| ToolProviderResult {
            schemas: vec![schema.clone()],
            known_names: None,
        });
        registry.register(tool);
    }
    (prompt, registry)
}

fn runtime_context_text(context: &AssembleContext) -> String {
    let runtime = shell::shell_runtime();
    let cwd = context.cwd.as_deref().unwrap_or("(unknown)");
    let shell_note = shell::shell_system_prompt_note(&runtime);
    format!(
        "工作目录:{cwd}（相对路径以它为根）。平台:{os} ({arch})。日期:{date}。{shell_note}",
        cwd = cwd,
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        date = today_string(),
        shell_note = shell_note,
    )
}

/// 公历日期 yyyy-mm-dd,不引 chrono:unix 秒 → civil 算法。
fn today_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}", y, m, d)
}

#[cfg(test)]
mod tests {
    use denia_system_prompt::{render_context_snapshot, render_prompt, render_prompt_for_user};

    use super::*;

    /// web_fetch 的双路径断言:随 default_registry 注册——纪律段存在、
    /// audience 为 Model、不进用户可见副本,schema 同步进 prompt 工具面;
    /// 子代理白名单收窄(不含 web_fetch)时段与 schema 一起消失。
    #[test]
    fn webfetch_section_follows_tool_registration() {
        let (prompt, registry) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/work".to_string()),
                ..Default::default()
            })
            .unwrap();
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "tool:webfetch")
            .expect("tool:webfetch 段必须随 shipped 工具注册");
        assert_eq!(section.audience, SectionAudience::Model);
        assert!(
            section.text.contains("不执行 JavaScript"),
            "纪律段应说明能力边界(不执行 JS)"
        );
        assert!(
            section.text.contains("不具备搜索能力"),
            "纪律段应说明 web_fetch 不是搜索工具"
        );
        assert!(
            registry.get("web_fetch").is_some(),
            "web_fetch 应注册进 shipped 工具面"
        );
        let tool_names: Vec<String> = assembly
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect();
        assert!(
            tool_names.contains(&"web_fetch".to_string()),
            "web_fetch schema 必须进 prompt 工具面:{tool_names:?}"
        );
        let user = render_prompt_for_user(&assembly);
        assert!(
            !user.contains("JavaScript"),
            "纪律段是模型侧私货,不应进用户可见副本"
        );
        // 段与工具同进退的收窄路径(agent-loop 的白名单机制)在
        // agent-loop/tests.rs 里断言。
    }

    /// 行为与表达纪律段的两条路径断言:
    /// 带工具时存在、audience 为 Model、不进用户可见副本。
    #[test]
    fn working_style_and_communication_sections_are_model_only() {
        let (prompt, _tools) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/work".to_string()),
                ..Default::default()
            })
            .unwrap();

        for name in ["harness:working-style", "harness:communication"] {
            let section = assembly
                .sections
                .iter()
                .find(|section| section.name == name)
                .unwrap_or_else(|| panic!("{name} 段必须存在"));
            assert_eq!(
                section.audience,
                SectionAudience::Model,
                "{name} 是模型侧纪律,不应展示给用户"
            );
        }

        let model = render_prompt(&assembly);
        assert!(model.contains("信息足够就做"), "工作方式纪律必须进模型提示");
        assert!(model.contains("先给结论"), "输出纪律必须进模型提示");
        assert!(
            model.contains("Markdown 语法写完整"),
            "markdown 语法纪律必须进模型提示"
        );

        // 用户可见副本不含这两段(它们是给模型的私货)。
        let user = render_prompt_for_user(&assembly);
        assert!(!user.contains("信息足够就做"));
        assert!(!user.contains("先给结论"));
    }

    /// 新增的三段行为纪律:上下文管理、代码风格、风险与诚实。
    #[test]
    fn behavior_discipline_sections_are_registered() {
        let (prompt, _tools) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/work".to_string()),
                ..Default::default()
            })
            .unwrap();
        let model = render_prompt(&assembly);
        // 上下文管理:告诉模型压缩后可以继续,不必提前收尾。
        assert!(
            model.contains("长对话会以摘要和剩余历史续接"),
            "上下文管理纪律必须进模型提示"
        );
        assert!(
            model.contains("不因上下文变长提前收尾"),
            "上下文管理纪律必须写明不必提前收尾"
        );
        // 代码风格:匹配周围代码 + 注释只写约束。
        assert!(
            model.contains("写代码像周围的代码"),
            "代码风格纪律必须进模型提示"
        );
        assert!(
            model.contains("只在代码本身表达不了的约束上写注释"),
            "注释纪律必须进模型提示"
        );
        // 风险与诚实:不可逆操作确认 + 如实交代。
        assert!(
            model.contains("难撤销或对外的操作先确认"),
            "风险确认纪律必须进模型提示"
        );
        assert!(model.contains("如实交代"), "诚实报告纪律必须进模型提示");

        let user = render_prompt_for_user(&assembly);
        for needle in [
            "长对话会以摘要和剩余历史续接",
            "写代码像周围的代码",
            "如实交代",
        ] {
            assert!(!user.contains(needle), "用户副本不应含模型侧纪律:{needle}");
        }
    }

    /// 提示词不得包含要求模型审查用户意图是否合规的安全政策。
    ///
    /// 那类内容会把注意力从工程问题挪到自我审查上,直接拉低交付质量;
    /// 需要拒绝的东西已经由权限模式与工具层承担,不靠模型读提示词来判。
    #[test]
    fn prompt_has_no_intent_review_policy() {
        let (prompt, _tools) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/work".to_string()),
                ..Default::default()
            })
            .unwrap();
        let mut corpus = render_prompt(&assembly);
        corpus.push_str(&render_context_snapshot(&assembly));
        for schema in &assembly.tools {
            corpus.push_str(&schema.description);
        }
        for banned in [
            // 只列安全审查政策特有的措辞:"授权"这类词在正常的风险确认
            // 语境里也会出现(如"除非已有持续授权"),不能拿来当判据。
            "安全测试",
            "渗透测试",
            "dual-use",
            "CTF",
            "拒绝请求",
            "是否合法",
            "是否合规",
            "恶意用途",
        ] {
            assert!(
                !corpus.contains(banned),
                "提示词混入了要求模型审查用户意图的内容({banned})"
            );
        }
    }

    /// 段位顺序(缓存前缀策略):常驻原则段在 persona 之后、一切纪律之前;
    /// 行为纪律(「行动与安全」组)排在工具准则之前;沟通与代码风格
    /// (「沟通与交付」组)收在工具段之后。分组标题段各自领住本组。
    ///
    /// 顺序与 `SectionOrder::value()` 同源:`WorkingStyle`/`RiskHonesty`/
    /// `ContextManagement` 的值都小于 `ToolBash`。
    #[test]
    fn behavior_sections_come_before_tool_sections() {
        let (prompt, _tools) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/work".to_string()),
                ..Default::default()
            })
            .unwrap();
        // 装配结果已按 order 排序,直接用位置比较。
        let index_of = |name: &str| {
            assembly
                .sections
                .iter()
                .position(|section| section.name == name)
                .unwrap_or_else(|| panic!("{name} 段必须存在"))
        };
        // 表值本身:行为纪律三段都排在 tool:bash 之前。
        assert!(SectionOrder::WorkingStyle.value() < SectionOrder::ToolBash.value());
        assert!(SectionOrder::RiskHonesty.value() < SectionOrder::ToolBash.value());
        assert!(SectionOrder::ContextManagement.value() < SectionOrder::ToolBash.value());
        // 原则段领起「行动与安全」组,组内是 工作方式 → 风险与诚实 → 上下文管理。
        assert!(index_of("harness:core-principles") < index_of("harness:group-action"));
        assert!(index_of("harness:group-action") < index_of("harness:working-style"));
        assert!(index_of("harness:working-style") < index_of("harness:risk-honesty"));
        assert!(index_of("harness:risk-honesty") < index_of("harness:context-management"));
        // 行为纪律整组收在「工具准则」组标题之前,再往后才是工具纪律段。
        assert!(index_of("harness:context-management") < index_of("harness:group-tools"));
        assert!(index_of("harness:group-tools") < index_of("tool:bash"));
        // 沟通与代码风格仍在工具段之后(「沟通与交付」组收尾)。
        assert!(index_of("tool:edit") < index_of("harness:group-comm"));
        assert!(index_of("harness:group-comm") < index_of("harness:communication"));
        assert!(index_of("harness:communication") < index_of("harness:code-style"));
    }

    /// 权限模式(plan / auto-edit / full / read-only)互切时,段集合与
    /// 工具 schema 集合必须逐字节稳定:模式语义由权限引擎与运行时快照
    /// 承担,系统提示与工具面跨模式不变(提供方前缀缓存不破)。
    #[test]
    fn permission_mode_flip_keeps_sections_and_tools_identical() {
        let (prompt, _tools) = default_shipped();
        let base = AssembleContext {
            cwd: Some("/work".to_string()),
            model: Some("mock".to_string()),
            provider: Some("mock".to_string()),
            ..Default::default()
        };
        let mut reference: Option<(Vec<denia_system_prompt::AssembledSection>, Vec<String>)> = None;
        for mode in ["read-only", "auto-edit", "plan", "full"] {
            let assembly = prompt
                .assemble(&AssembleContext {
                    permission_mode: Some(mode.to_string()),
                    ..base.clone()
                })
                .unwrap();
            let tool_names: Vec<String> = assembly.tools.iter().map(|t| t.name.clone()).collect();
            match &reference {
                None => reference = Some((assembly.sections.clone(), tool_names)),
                Some((sections, tools)) => {
                    assert_eq!(&assembly.sections, sections, "模式 {mode} 改变了段集合");
                    assert_eq!(&tool_names, tools, "模式 {mode} 改变了工具 schema 集合");
                }
            }
        }
    }

    #[test]
    fn shipped_with_persona_omits_harness_identity() {
        let (prompt, _tools) = shipped_with_persona("仅自定义正文".to_string());
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/work".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !assembly
                .sections
                .iter()
                .any(|section| section.name == "harness:identity")
        );
        let user = render_prompt_for_user(&assembly);
        assert!(user.contains("仅自定义正文"));
        assert!(!user.contains("denia 驱动的"));
    }

    #[test]
    fn shipped_with_persona_overrides_default_persona() {
        let (prompt, _tools) = shipped_with_persona("自定义 persona {{cwd}}".to_string());
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/work".to_string()),
                ..Default::default()
            })
            .unwrap();
        let rendered = render_prompt(&assembly);
        assert!(rendered.contains("自定义 persona /work"));
        assert!(
            assembly
                .sections
                .iter()
                .any(|section| section.name == "tool:bash")
        );
    }

    #[test]
    fn shipped_prompt_includes_tool_sections_and_runtime_context() {
        let (prompt, _tools) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                model: Some("mock".to_string()),
                provider: Some("mock".to_string()),
                ..Default::default()
            })
            .unwrap();
        let rendered = render_prompt(&assembly);
        assert!(rendered.contains("denia"));
        // 工作目录属于运行时快照,不进静态段落(它每步可能变,静态段会被缓存)。
        assert!(!rendered.contains("/tmp/ws"));
        let snapshot = render_context_snapshot(&assembly);
        assert!(
            snapshot.contains("/tmp/ws"),
            "工作目录必须由运行时快照携带:{snapshot}"
        );
        assert!(
            assembly
                .sections
                .iter()
                .any(|section| section.name == "tool:bash")
        );
        assert!(
            assembly
                .sections
                .iter()
                .any(|section| section.name == "tool:todo")
        );
        assert!(!render_context_snapshot(&assembly).is_empty());
        // bash/read/write/todo/ls/glob/grep/edit/exit_plan/get_goal/update_goal/web_fetch
        // + 任务验证账本 run_checks。
        assert_eq!(assembly.tools.len(), 14);
        assert!(
            assembly
                .tools
                .iter()
                .any(|tool| tool.name == "read_tool_output")
        );
    }

    /// 全量兜底:出厂提示词里**任何一处**都不许把探索动作推给 bash。
    ///
    /// 上一轮只审了工具 schema 与 `tool:` 纪律段,漏了出厂 persona
    /// (`deployment:persona`,order 0)和 `context:file-reference`——两处都写着
    /// "用 bash 列目录",而它们注入得比收口段更早。这里把 persona、全部段落、
    /// 上下文快照、全部工具描述拼成一份语料统一查,避免再漏。
    #[test]
    fn shipped_prompt_never_steers_exploration_to_bash() {
        let (prompt, _registry) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                ..Default::default()
            })
            .expect("assemble");
        let mut corpus = denia_system_prompt::default_persona_template().to_string();
        corpus.push_str(&render_prompt(&assembly));
        corpus.push_str(&render_context_snapshot(&assembly));
        for schema in &assembly.tools {
            corpus.push_str(&schema.description);
            corpus.push_str(&schema.parameters.to_string());
        }
        for banned in [
            "bash 列目录",
            "先用 bash",
            "用 bash 列",
            "bash 找文件",
            "bash 搜",
            "bash 读取",
        ] {
            assert!(
                !corpus.contains(banned),
                "出厂提示词又把探索动作推给 bash({banned})"
            );
        }
    }

    /// 手动核对入口:打印模型实际看到的探索类纪律段与 bash 工具描述。
    ///
    /// `cargo test -p denia-tools --lib -- --ignored --nocapture print_exploration_discipline`
    ///
    /// 改这几个段的文案时用它对照,比隔着 UI 猜模型看到什么靠谱。
    #[test]
    #[ignore]
    fn print_exploration_discipline() {
        let (prompt, _registry) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                ..Default::default()
            })
            .expect("assemble");
        eprintln!(
            "\n===== deployment:persona (出厂模板,order 0) =====\n{}",
            denia_system_prompt::default_persona_template()
        );
        for name in [
            "context:file-reference",
            "tool:bash",
            "tool:ls",
            "tool:glob",
            "tool:grep",
            "tool:read",
        ] {
            if let Some(section) = assembly
                .sections
                .iter()
                .find(|section| section.name == name)
            {
                eprintln!("\n===== {name} =====\n{}", section.text);
            }
        }
        for tool in ["bash", "ls"] {
            if let Some(schema) = assembly.tools.iter().find(|schema| schema.name == tool) {
                eprintln!("\n===== {tool} schema =====\n{}", schema.description);
            }
        }
    }

    #[test]
    fn ls_section_registered_and_user_invisible() {
        // tool:ls 纪律段:默认 shipped 注册存在、audience 为 Model、
        // 不进用户可见副本;文案必须给出可执行的替代与禁止事项。
        let (prompt, registry) = default_shipped();
        assert!(registry.get("ls").is_some(), "ls 工具必须随 shipped 注册");
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                ..Default::default()
            })
            .expect("assemble");
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "tool:ls")
            .expect("tool:ls section registered with the ls tool");
        assert_eq!(section.audience, SectionAudience::Model);
        // 段里只留本工具独有的用法(depth 上限、别猜路径):"探索类动作不许走 shell"
        // 的总纲收在 tool:bash 一段——tool:bash 与 bash 工具同进退,有 shell 可误用
        // 时它必然在,没有 shell 时禁令本来就无意义。
        assert!(section.text.contains("depth"), "{}", section.text);
        assert!(section.text.contains("不猜路径"), "{}", section.text);
        let user_body = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(
            !user_body.contains("不猜路径"),
            "tool:ls 段泄进了用户可见副本"
        );
    }

    #[test]
    fn bash_section_points_at_the_exploration_tools() {
        // 命令名清单与理由(Get-ChildItem、.gitignore 等)已在 2026-10-18 按用户
        // 要求从 tool:bash 段移除,方言无关的收口口径改由 tool:job 段独家承担
        // (见 agent-loop/tests.rs 的只读档断言)。这里只守 bash 段自己:四件
        // 探索动作必须点名专用工具出口,不许留退回 shell 检索的口子。
        let (prompt, _registry) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                ..Default::default()
            })
            .expect("assemble");
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "tool:bash")
            .expect("tool:bash section registered");
        for needle in ["ls", "glob", "grep", "read_file"] {
            assert!(
                section.text.contains(needle),
                "tool:bash 段缺少探索动作的专用工具出口 {needle}:{}",
                section.text
            );
        }
        assert!(
            section.text.contains("分别用 ls/glob/grep/read_file"),
            "tool:bash 段没把探索动作指给专用工具:{}",
            section.text
        );
        let user_body = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(
            !user_body.contains("分别用 ls/glob/grep/read_file"),
            "tool:bash 段泄进了用户可见副本"
        );
    }

    #[test]
    fn plan_section_follows_tool_grant_rules() {
        // tool:plan 纪律段两路径:随 exit_plan 注册存在、audience 为 Model、
        // 不进用户可见副本。
        let (prompt, _tools) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                ..Default::default()
            })
            .unwrap();
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "tool:plan")
            .expect("tool:plan section registered with exit_plan");
        assert_eq!(section.audience, SectionAudience::Model);
        assert!(section.text.contains("exit_plan"));
        let user_body = render_prompt_for_user(&assembly);
        assert!(!user_body.contains("exit_plan"));
        // schema 同步:exit_plan 在默认部署的工具列表里(模式过滤在 assemble
        // 阶段,见 agent-loop 的 turn.rs)。
        assert!(assembly.tools.iter().any(|tool| tool.name == "exit_plan"));
    }

    #[test]
    fn goal_section_follows_registry_and_audience_rules() {
        // tool:goal 纪律段两路径:goal 工具随 default_registry 总是注册,
        // 段总是存在、audience 为 Model、不进用户可见副本;子代理白名单
        // 过滤(不含 get_goal/update_goal)由 section_tools 映射承担,
        // agent-loop 的 subagent_sections_follow_tool_grant 测试覆盖。
        let (prompt, registry) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                ..Default::default()
            })
            .unwrap();
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "tool:goal")
            .expect("tool:goal section registered with default registry");
        assert_eq!(section.audience, SectionAudience::Model);
        assert!(section.text.contains("update_goal"));
        let user_body = render_prompt_for_user(&assembly);
        assert!(!user_body.contains("update_goal"));
        assert!(registry.get("get_goal").is_some());
        assert!(registry.get("update_goal").is_some());
        assert!(assembly.tools.iter().any(|tool| tool.name == "get_goal"));
        assert!(assembly.tools.iter().any(|tool| tool.name == "update_goal"));
    }

    #[test]
    fn memory_section_follows_registration_and_audience_rules() {
        // tool:memory 纪律段:注册后存在、audience 为 Model、不进用户可见
        // 副本;文案必须给出一事一文件格式、四类型语义、索引维护与验证义务。
        let mut prompt = denia_system_prompt::SystemPrompt::new(
            denia_system_prompt::SystemPromptConfig::default(),
        );
        prompt
            .variable("cwd", |context| context.cwd.clone())
            .unwrap();
        register_memory_prompt_section(&mut prompt).unwrap();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".into()),
                ..Default::default()
            })
            .unwrap();
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "tool:memory")
            .expect("tool:memory section registered");
        assert_eq!(section.audience, SectionAudience::Model);
        for needle in [
            "# 项目记忆",
            "一事一文件",
            "MEMORY.md",
            "type: user | feedback | project | reference",
            "[[name]]",
            "就地在原件上更新而不新建重复文件",
            "推荐其中的文件、函数或开关前先验证它现在还存在",
            // 「该记」侧的判据:用户纠正、非显而易见决策、花力气查清的事实。
            "用户对你的纠正",
            "非显而易见且理由来自排查的决策",
            // 「不该记」侧的判据:仓库已有、一次性细节。
            "仓库本身已记录的",
            "只对本次对话有意义的",
            // 两侧都要有,否则模型会偏向保守或偏向流水账。
            "该写：",
            "不该写：",
        ] {
            assert!(section.text.contains(needle), "tool:memory 段缺少 {needle}");
        }
        let user_body = render_prompt_for_user(&assembly);
        assert!(
            !user_body.contains("一事一文件"),
            "tool:memory 段泄进了用户可见副本"
        );
    }

    #[test]
    fn default_shipped_has_no_memory_section() {
        // 无 runtime 的部署不注册记忆纪律段:模型不看到不存在的记忆能力。
        let (prompt, _registry) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".into()),
                ..Default::default()
            })
            .unwrap();
        assert!(
            !assembly
                .sections
                .iter()
                .any(|section| section.name == "tool:memory"),
            "tool:memory must not be registered in the bare shipped prompt"
        );
    }

    #[test]
    fn file_reference_section_is_model_audience() {
        let (prompt, _tools) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                ..Default::default()
            })
            .unwrap();
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "context:file-reference")
            .expect("file-reference section registered");
        assert_eq!(section.audience, SectionAudience::Model);
        assert!(section.text.contains('@'));
        // 工具纪律类段不进用户可见副本。
        let user_body = render_prompt_for_user(&assembly);
        assert!(!user_body.contains("read_file 读取"));
    }
}

#[cfg(test)]
mod browser_prompt_tests {
    use super::*;

    struct FakeHub {
        visual_requests: std::sync::atomic::AtomicUsize,
    }
    #[async_trait::async_trait]
    impl crate::browser::BrowserExecute for FakeHub {
        async fn execute(
            &self,
            _command: denia_browser::BrowserCommand,
        ) -> denia_browser::CommandOutcome {
            denia_browser::CommandOutcome::ok_value(serde_json::Value::Null, 0)
        }

        async fn request_visual_mode(&self) {
            self.visual_requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn fake_hub() -> crate::BrowserHub {
        std::sync::Arc::new(FakeHub {
            visual_requests: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    async fn run_browser_tool(hub: crate::BrowserHub, arguments: &str) -> crate::ToolOutput {
        use denia_core::session::PermissionMode;
        use tokio_util::sync::CancellationToken;

        let ctx = crate::ToolContext {
            output_store: None,
            session_id: None,
            selection: None,
            cwd: std::env::temp_dir(),
            cancel: CancellationToken::new(),
            confined: true,
            vision_supported: true,
            emit_event: None,
            file_history: None,
            permission_mode: PermissionMode::AutoEdit,
            ask: None,
            call_id: None,
            goal_reader: None,
            task_ledger: None,
            read_state: None,
        };
        let tool = crate::BrowserTool::new(hub);
        tool.execute(arguments, &ctx).await
    }

    #[test]
    fn default_shipped_with_browser_includes_schema() {
        // hub 需要 trait 对象;用 BrowserTool 侧的桥接实现 — 这里只验证 schema 进 prompt。
        let hub = fake_hub();
        let (prompt, registry) = default_shipped_with_browser(Some(hub));
        let assembly = prompt
            .assemble(&denia_system_prompt::AssembleContext::default())
            .expect("assemble");
        let names: Vec<&str> = assembly
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(
            names.contains(&"browser"),
            "browser schema missing from prompt tools: {names:?}"
        );
        assert!(registry.get("browser").is_some(), "browser not registered");
    }

    #[test]
    fn browser_section_registered_only_with_hub() {
        // 默认 persona 模板引用 {{cwd}},render 时必须提供,否则插值 fail loud。
        let context = denia_system_prompt::AssembleContext {
            cwd: Some("/tmp/ws".to_string()),
            ..Default::default()
        };
        // 带 hub:tool:browser 段注册,Model 受众,不进用户可见副本。
        let (prompt, _registry) = default_shipped_with_browser(Some(fake_hub()));
        let assembly = prompt.assemble(&context).expect("assemble");
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "tool:browser")
            .expect("tool:browser section registered");
        assert_eq!(section.audience, SectionAudience::Model);
        // 新架构(CDP):纪律段讲清引用有效期、收尾口径与可视化模式。
        assert!(
            section.text.contains("Chrome/Edge"),
            "纪律段应说明底层浏览器"
        );
        assert!(
            section.text.contains("仅对同一 tab 的最近一次快照有效"),
            "纪律段应说明 ref 的有效期"
        );
        assert!(
            section.text.contains("绝不关闭不是自己打开的 tab"),
            "纪律段应写明不碰用户原有标签页"
        );
        assert!(
            section.text.contains("并以 list 确认它们全部消失"),
            "纪律段应给出 tab 收尾判定口径"
        );
        // 进程回收是本次改动的核心:只 close 不算收尾,必须显式收掉实例。
        assert!(
            section.text.contains("shutdownBrowser 回收浏览器进程"),
            "纪律段应要求收尾时回收浏览器进程"
        );
        assert!(
            section.text.contains("没回收会让后续启动直接失败"),
            "纪律段应说明不回收的真实代价(profile 单例锁)"
        );
        assert!(
            section.text.contains("不凭空拉起实例"),
            "纪律段应说明查询命令不拉起浏览器"
        );
        assert!(section.text.contains("visualMode"));
        let user_body = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(!user_body.contains("绝不关闭不是自己打开的 tab"));

        // persona 变体带 hub 同样有段。
        let (prompt, _registry) = shipped_with_persona_and_browser_and_ask(
            "自定义 persona".to_string(),
            Some(fake_hub()),
            false,
        );
        let assembly = prompt.assemble(&context).expect("assemble");
        assert!(
            assembly
                .sections
                .iter()
                .any(|section| section.name == "tool:browser"),
            "persona+browser variant missing tool:browser section"
        );
    }

    #[test]
    fn browser_section_absent_without_hub() {
        // 无 browser 工具的部署不注入纪律段:模型不看到不存在的工具。
        for (prompt, _tools) in [default_shipped(), default_shipped_with_browser(None)] {
            let assembly = prompt
                .assemble(&denia_system_prompt::AssembleContext::default())
                .expect("assemble");
            assert!(
                !assembly
                    .sections
                    .iter()
                    .any(|section| section.name == "tool:browser"),
                "tool:browser must not be registered without browser tool"
            );
        }
    }

    /// 历史只读集合含 browser（旧日志/旧参数的保守上限）：对应的纪律段与
    /// schema 必须一起出现，否则模型用 browser 抓网页却不知道收尾义务。
    /// 新定义（含内置 explore）各自声明工具，explore 初始**不**授予 browser。
    #[test]
    fn browser_section_and_schema_available_with_the_legacy_ceiling() {
        assert!(
            crate::SUBAGENT_READ_ONLY_TOOLS.contains(&"browser"),
            "历史只读集合应含 browser（旧描述符的保守上限）"
        );
        let (prompt, registry) = default_shipped_with_browser_and_ask(Some(fake_hub()), true);
        let assembly = prompt
            .assemble(&denia_system_prompt::AssembleContext {
                cwd: Some("/tmp/ws".to_string()),
                ..Default::default()
            })
            .expect("assemble");
        assert!(
            assembly.sections.iter().any(|s| s.name == "tool:browser"),
            "带 hub 时必须注册 tool:browser 段"
        );
        assert!(registry.get("browser").is_some());
    }

    #[test]
    fn ask_section_and_schema_register_together() {
        let context = denia_system_prompt::AssembleContext {
            cwd: Some("/tmp/ws".to_string()),
            ..Default::default()
        };
        // 带 ask:段注册、audience=Model、不进用户可见副本,schema 同步可见。
        let (prompt, registry) = default_shipped_with_browser_and_ask(None, true);
        let assembly = prompt.assemble(&context).expect("assemble");
        let section = assembly
            .sections
            .iter()
            .find(|section| section.name == "tool:ask")
            .expect("tool:ask section registered");
        assert_eq!(section.audience, SectionAudience::Model);
        assert!(section.text.contains("只在真正卡住时用"));
        assert!(section.text.contains("timed-out"));
        assert!(section.text.contains("cancelled"));
        assert!(section.text.contains("unavailable"));
        let names: Vec<&str> = assembly
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        assert!(names.contains(&"ask"), "ask schema missing: {names:?}");
        assert!(registry.get("ask").is_some(), "ask not registered");
        let user_body = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(!user_body.contains("只在真正卡住时用"));

        // 不带 ask:段与 schema 都不出现。
        let (prompt, registry) = default_shipped_with_browser_and_ask(None, false);
        let assembly = prompt.assemble(&context).expect("assemble");
        assert!(
            !assembly
                .sections
                .iter()
                .any(|section| section.name == "tool:ask"),
            "tool:ask must not be registered without the ask tool"
        );
        assert!(
            !assembly.tools.iter().any(|tool| tool.name == "ask"),
            "ask schema must not appear without the ask tool"
        );
        assert!(registry.get("ask").is_none());

        // persona 变体同样成对。
        let (prompt, registry) =
            shipped_with_persona_and_browser_and_ask("自定义 persona".to_string(), None, true);
        let assembly = prompt.assemble(&context).expect("assemble");
        assert!(assembly.sections.iter().any(|s| s.name == "tool:ask"));
        assert!(registry.get("ask").is_some());
    }

    #[tokio::test]
    async fn visual_mode_requests_sidebar_before_command() {
        // visualMode: true → 命令执行前请求一次可视化模式;命令本身正常执行。
        let hub = std::sync::Arc::new(FakeHub {
            visual_requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let output = run_browser_tool(
            hub.clone() as crate::BrowserHub,
            r#"{"method":"navigate","url":"https://example.com","visualMode":true}"#,
        )
        .await;
        assert!(!output.is_error, "command should succeed");
        assert_eq!(
            hub.visual_requests
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "visual mode should be requested exactly once"
        );
    }

    #[tokio::test]
    async fn visual_mode_off_by_default() {
        // 缺省 false:不请求可视化模式,后台操作不打扰用户。
        let hub = std::sync::Arc::new(FakeHub {
            visual_requests: std::sync::atomic::AtomicUsize::new(0),
        });
        let output =
            run_browser_tool(hub.clone() as crate::BrowserHub, r#"{"method":"list"}"#).await;
        assert!(!output.is_error, "command should succeed");
        assert_eq!(
            hub.visual_requests
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "visual mode must not be requested by default"
        );
    }
}

#[cfg(test)]
mod capability_prompt_tests {
    use super::*;

    #[test]
    fn capability_sections_land_in_model_audience() {
        let mut prompt = denia_system_prompt::SystemPrompt::new(
            denia_system_prompt::SystemPromptConfig::default(),
        );
        prompt
            .variable("cwd", |context| context.cwd.clone())
            .unwrap();
        prompt
            .variable("model", |context| context.model.clone())
            .unwrap();
        prompt
            .variable("provider", |context| context.provider.clone())
            .unwrap();
        register_capability_prompt_sections(&mut prompt).unwrap();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".into()),
                model: Some("mock".into()),
                provider: Some("mock".into()),
                ..Default::default()
            })
            .unwrap();
        let body = denia_system_prompt::render_prompt(&assembly);
        assert!(
            body.contains("spawn_agent/fork_agent"),
            "{}",
            &body[..body.len().min(2000)]
        );
        assert!(body.contains("job_start"));
        assert!(body.contains("<available_skills>"));
        // Model audience 段不进用户可见副本。
        let user_body = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(!user_body.contains("spawn_agent/fork_agent"));
        assert!(!user_body.contains("job_start"));
    }

    #[test]
    fn memory_section_carries_path_and_discipline() {
        // 带路径版本:路径内联,纪律要点齐全。
        let rooted = render_memory_section(Some(std::path::Path::new("/m/memory")));
        assert!(rooted.contains("# 项目记忆"));
        assert!(rooted.contains("记忆目录在 `/m/memory`"));
        assert!(rooted.contains("直接用 write_file 写入"));
        assert!(rooted.contains("type: user | feedback | project | reference"));
        assert!(rooted.contains("[[name]]"));
        assert!(rooted.contains("- [标题](文件名.md) — 一句话钩子"));
        assert!(rooted.contains("AGENTS.md"));
        // 无路径基座(注册兜底):指向注入消息,其余正文一致。
        let plain = render_memory_section(None);
        assert!(plain.contains("「项目记忆」注入且已存在"));
        assert!(!plain.contains("`/m/"));
    }

    #[test]
    fn default_shipped_has_no_capability_sections() {
        // 无 runtime 部署(default_shipped)不注入能力纪律段:模型不看到
        // 不存在的工具。
        let (prompt, _) = default_shipped();
        let assembly = prompt
            .assemble(&AssembleContext {
                cwd: Some("/tmp/ws".into()),
                model: Some("mock".into()),
                provider: Some("mock".into()),
                ..Default::default()
            })
            .unwrap();
        let body = denia_system_prompt::render_prompt(&assembly);
        assert!(!body.contains("spawn_agent/fork_agent"));
    }
}
