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
                "read-only" => "当前权限模式:只读。一切会修改文件或产生写副作用的操作(写文件、编辑、有写副作用的命令)都会被直接拒绝;请只做阅读、检索与分析。".to_string(),
                "plan" => "当前权限模式:计划。禁止一切写操作与有写副作用的命令;先完成调研,再用 exit_plan 工具提交完整计划,等待用户审批。计划被批准后会话自动切换到执行模式,届时直接开始执行;被拒绝且用户附带了补充建议时,按建议修订后重新提交。".to_string(),
                "full" => "当前权限模式:完全访问。所有操作自动放行、无需审批,文件写入可以离开工作区;破坏性操作仍要先说明再执行。".to_string(),
                _ => "当前权限模式:自动编辑。工作区内的文件编辑与命令执行自动放行;写工作区之外的文件会弹出用户审批,等用户放行后继续。".to_string(),
            }
        })),
    })?;

    prompt.section(PromptSection {
        name: "tool:bash".to_string(),
        order: SectionOrder::ToolBash.value(),
        text: PromptText::Static(
            "bash 只用来跑命令:构建、测试、git、装依赖、启动进程。四件探索类的事一律不许走 bash——列目录用 ls,按路径模式找文件用 glob,搜文件内容用 grep,读文本用 read_file。禁止范围按动作界定,不看命令叫什么名:PowerShell 的 Get-ChildItem(含 -Recurse/Filter/Include)、Select-String、Get-Content 与 bash 的 ls、find、grep/rg、cat 一样都不许写。原因:shell 检索命令不读 .gitignore,会连着 node_modules 与构建产物一起扫,比专用工具慢几个数量级;列目录/找文件/搜内容不是\"一次性看一眼\",而是每轮对话都会重复发生的高频动作。此外:每次调用都新起一个 shell 进程,不保留工作目录与变量;命令语法须与本机 shell 方言一致;每次查看退出码,失败时先排查再继续。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:ls".to_string(),
        order: SectionOrder::ToolLs.value(),
        text: PromptText::Static(
            "用 ls 查看一个目录里有哪些条目——不要用 shell 的 ls/dir/Get-ChildItem。默认只列一层;确实要看更深时显式给 depth(上限 5),不要用 shell 递归摊平整棵树。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:read".to_string(),
        order: SectionOrder::ToolRead.value(),
        text: PromptText::Static(
            "用 read_file 查看文本文件,不要用 bash 的 cat/type,也不要写 PowerShell 的 Get-Content。大文件可分段读取。".to_string(),
        ),
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
            "用户消息中带 @ 前缀的是用户明确引用的工作区路径,相对工作区根目录。结尾带 / 的是目录:需要其内容时用 ls 查看;其余是文件:需要其内容时用 read_file 读取,读取前不要声称已查看过内容,也不要猜测内容。路径含空格时用 @\"...\" 表示。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:write".to_string(),
        order: SectionOrder::ToolWrite.value(),
        text: PromptText::Static(
            "用 write_file 创建或整文件覆盖;覆盖前先 read_file 确认现有内容。".to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:glob".to_string(),
        order: SectionOrder::ToolGlob.value(),
        text: PromptText::Static(
            "用 glob 工具——不要用 shell 的 find,也不要写 PowerShell 的 Get-ChildItem -Recurse——按路径模式找文件。无 \"/\" 的模式在任意深度匹配 basename,所以 \"*.rs\" 搜整棵树;结果只含文件、按修改时间排序。只想看目录里有什么就用 ls,不要拿 glob 绕。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:grep".to_string(),
        order: SectionOrder::ToolGrep.value(),
        text: PromptText::Static(
            "用 grep 工具——不要用 shell 的 grep/rg/findstr,也不要写 PowerShell 的 Select-String——全文搜索;搜索大目录可加 include 收窄,命中上限后收窄 pattern 再看更多;命中的文件用 read_file 读上下文。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:edit".to_string(),
        order: SectionOrder::ToolEdit.value(),
        text: PromptText::Static(
            "精确小改动用 edit 工具(必须精确出现一次的字符串替换);大段改动用 write_file。改完用 read_file 或 grep 验证结果。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:todo".to_string(),
        order: SectionOrder::ToolWrite.value() + 1,
        text: PromptText::Static(
            "多步任务开工前先用 todo_write 拆步骤(每步是可直接执行的具体动作,不是笼统阶段名)。\n强制约束:todo 必须与真实进度实时同步——每完成一步,在开始下一步的同一个工具批次里立刻把该项标 completed、把正在做的那项标 in_progress;状态落后于事实等同于谎报进度。禁止攒到最后一次性批量更新,禁止把已完成的工作留在 in_progress,也禁止把没做的事标成 completed。\n任何时刻只有一项 in_progress(工作真并行时例外:并发子代理/后台任务可多条同时 in_progress);全部工作完成才允许没有 in_progress 项。任务范围变化时重发完整列表反映新计划,不要另开一份。琐碎的单步任务不用建清单。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    register_plan_prompt_section(prompt)?;
    register_goal_prompt_section(prompt)?;

    let schemas: Vec<ToolSchema> = tools.schemas();
    prompt.tools(move |_| ToolProviderResult {
        schemas: schemas.clone(),
        known_names: None,
    });
    Ok(())
}

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
            "独立任务用 spawn_agent/fork_agent 委派给子代理;send_message 只能在直接父子代理之间收发消息。把可以独立进行的子任务拆给子代理分工合作,不要全部自己做;需要立即拿到结果的前台委派用 run_in_background=false 阻塞等结果,可以并行推进的后台委派保持默认后台运行,子代理完成时会通知父会话。子代理默认只有只读工具(read_file/glob/grep/skill/browser):它不写文件、不跑命令、不向用户提问、也不再委派——需要落盘改动或与用户确认的任务留在父代理做,或让子代理只做调查并把结论与待办交回。子代理用 browser 抓取网页同样受浏览器收尾纪律约束(任务完成 list 确认无 tab)。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:jobs".to_string(),
        order: SectionOrder::ToolJobs.value(),
        text: PromptText::Static(
            "长时间命令用 job_start 后台运行,job_output 领取输出,job_kill 停止;等待子代理结果用 wait_agent。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    prompt.section(PromptSection {
        name: "tool:skill".to_string(),
        order: SectionOrder::ToolSkill.value(),
        text: PromptText::Static(
            "技能目录以独立注入的 <available_skills> 为准;开工前先对照目录检查有没有与当前任务匹配的技能,有匹配的先 load 再动手,没有匹配的就直接处理,不要为了用技能而调用技能。skill 工具 load 后 SKILL.md 全文直接在结果中返回,加载一次即可,不要再用文件读取工具读 SKILL.md;references/scripts 等其余文件用 skill 工具的 resource 操作按返回的 resourceBase 相对路径读取,技能不授予额外权限。"
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
/// 机制上由 Playwright 驱动**真实浏览器**(headless 无窗口后台运行,画面
/// 走控制台侧边栏),本段负责让模型明白:引用何时失效、收尾该关哪些 tab、
/// 什么时候才该开可视化模式。
fn register_browser_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:browser".to_string(),
        order: SectionOrder::ToolBrowser.value(),
        text: PromptText::Static(
            "browser 由 Playwright 驱动真实浏览器,无窗口后台运行(headless),画面只在控制台浏览器侧栏实时展示。启动按需:交互命令会拉起浏览器;list/close 等查询与善后命令在浏览器没跑时原地返回,绝不凭空拉起实例。\n\
             元素引用:snapshot 产出的 `[ref=eN]` 直接传 `e6`(也接受 `@e6`)。引用只在最近一次 snapshot 之后有效,且仅对同一 tab;页面导航、提交、SPA 路由切换后必须重新 snapshot——拿旧引用操作会报错(这是刻意的,避免静默点错元素)。\n\
             收尾义务:任务完成、或确认后续不再使用浏览器时,把本次打开的 tab 逐个 close;判定口径 = list 里本次打开的 tab 全部消失,用户原有的 tab 原样保留。**绝不关闭不是自己开的 tab,绝不退出浏览器**。\n\
             可视化模式:仅当用户明确要求看着他操作(如\"打开给我看\"\"可视化模式\"),或用户的话里明显需要亲眼看到画面(如\"看下这个页面长什么样\"\"演示一下操作\")时,才给命令加 visualMode: true 展开浏览器侧栏;常规抓取、点击、检查接口等后台任务保持默认。开启后用户手动收起侧栏即为不要看,不要再重复请求展开。"
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
            "ask 工具用于真正卡住的时刻:需求自相矛盾、破坏性操作需要拍板、或缺少只有用户才知道的信息。能自己查证的事实(read_file/grep/glob/bash/浏览器)不要问用户;能给出合理默认的决策先做,把假设写进结论再继续——提问不是拖延的借口。一次把同批相关问题问全,不要分多轮反复打断用户;每问一道都要能说清\"答案会改变我的下一步什么\"。\n拿到结局后的动作:answered 按用户选择继续;timed-out 不要原样重复同一问题,据现有信息继续并在结论里写明采用的假设;cancelled 视为不要沿这条路径继续,停下说明当前状态与可选方案;unavailable 自行决策并明确标注假设。用户跳过某题(skipped)时按缺省继续,不要把跳过当成需要再问的信号。"
                .to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
}

/// MCP 外部工具的使用纪律段。
///
/// 与实际注册的工具严格同步:仅在有已连接 MCP 服务器、且确实注册了
/// `mcp__*` 工具时注入(无 MCP 的部署模型不会看到不存在的工具)。
/// 纪律与 description 分工不重叠:description 写调用机制与分页字段,
/// 本段写"这些工具是什么、什么时候用、失败怎么办"的行为准则。
pub fn register_mcp_prompt_section(prompt: &mut SystemPrompt) -> Result<(), String> {
    prompt.section(PromptSection {
        name: "tool:mcp".to_string(),
        order: SectionOrder::ToolMcp.value(),
        text: PromptText::Static(
            "形如 mcp__<服务器>__<工具> 的是 MCP 外部工具,能力来自用户在设置里接入的第三方 MCP 服务器,不是 denia 内置功能。用法:与内置工具一样直接调用,参数按工具声明的 JSON Schema 给;不要向用户解释你在\"用 MCP\"。\n结果分页:长结果按字符分页返回,尾部会给出\"第 N-M 字符 / 共 X 字符\"与下一个 offset;要看后面的内容,用那个 offset 再调一次同一工具(不要改其它参数,否则会重新执行工具而不是翻页)。优先把查询范围收窄,不要靠翻页从头读到尾。\n失败处理:调用失败的文本通常来自外部服务器(未连接/超时/参数被拒)。先读错误里的可执行建议——多半是让用户到设置 → MCP 检查服务器状态或重新连接;一次失败不要反复重试同一个调用,换内置工具或请用户处理。同名能力优先用内置工具(ls/glob/grep/read_file 等),MCP 工具只在内置工具做不到时才用。".to_string(),
        ),
        complete: false,
        audience: SectionAudience::Model,
    })?;
    Ok(())
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
            "计划模式的收尾义务:调研完成后必须用 exit_plan 提交结构化计划(plan 用 markdown 写清目标、分步方案、将修改或新建的文件、风险、验证方式),不要把计划散落在回复里等用户自己领会;一次提交完整计划,不要拆成多次试探性提交。提交后本轮阻塞等待用户决策,期间不要继续调用其他工具。拿到决策后的动作:批准 → 按计划直接开始执行,不要再次向用户确认;批准并附带补充建议 → 把建议一并落实;拒绝并附带补充建议 → 按建议修订计划后重新 exit_plan,只改受影响的部分,除非用户要求否则不要推倒重来;拒绝且无建议 → 先用 ask 询问用户的顾虑再修订。计划获批执行时,若发现必须偏离计划(额外破坏性操作、方案走不通),停下来向用户说明现状与建议,不要擅自扩大范围。"
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
            "会话存在目标时,一切工作以达成目标为先:每轮开始时先看最新的 [denia 目标] 状态注入(目标、状态、轮次与预算用量),评估当前进展再决定并执行下一步,不要原地等待指示。目标真正达成时立刻用 update_goal 的 complete 标记,不要把已完成的目标挂着续跑;被外部条件卡住(缺凭据、依赖方不可用、需要用户拍板)且无法绕开时,用 blocked 标记并写清阻塞条件,不要空转烧预算。用户插话是对目标的 steering:按新指示调整方向,必要时用 update_goal 的 edit 同步目标文本。get_goal 用于在动手前确认目标状态与预算用量。不要虚构目标状态,一切以 get_goal 返回为准。"
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
        "平台:{os} ({arch})。日期:{date}。{shell_note}",
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        date = today_string(),
        shell_note = shell_note,
    )
    .replace("{{cwd}}", cwd)
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
        assert!(rendered.contains("/tmp/ws"));
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
        // bash/read/write/todo/ls/glob/grep/edit/exit_plan/get_goal/update_goal。
        assert_eq!(assembly.tools.len(), 11);
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
            if let Some(section) = assembly.sections.iter().find(|section| section.name == name) {
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
        assert!(section.text.contains("Get-ChildItem"), "{}", section.text);
        assert!(section.text.contains("depth"), "{}", section.text);
        let user_body = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(!user_body.contains("不要用 shell 的 ls"), "tool:ls 段泄进了用户可见副本");
    }

    #[test]
    fn bash_section_bans_shell_exploration_in_every_dialect() {
        // 收口总纲必须按"动作"界定而不是按 POSIX 命令名:PowerShell 宿主上
        // 模型写的是 Get-ChildItem/Select-String/Get-Content,只禁 ls/find/grep
        // 等于没禁。四件探索类动作各自的专用工具也要点到。
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
        for needle in [
            // 专用工具出口。
            "ls",
            "glob",
            "grep",
            "read_file",
            // POSIX 侧命令名。
            "find",
            "cat",
            // PowerShell 侧命令名——缺了这些,条文在 Windows 宿主上命中不了。
            "Get-ChildItem",
            "Select-String",
            "Get-Content",
            // 收口口径:按动作界定,不看命令名。
            "按动作界定",
            // 理由要写清,否则模型把它当风格偏好而不是性能约束。
            ".gitignore",
        ] {
            assert!(
                section.text.contains(needle),
                "tool:bash 段缺少 {needle}:{}",
                section.text
            );
        }
        let user_body = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(!user_body.contains("按动作界定"), "tool:bash 段泄进了用户可见副本");
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
        // 新架构(Playwright):纪律段讲清引用有效期、收尾口径与可视化模式。
        assert!(
            section.text.contains("Playwright"),
            "纪律段应说明底层是 Playwright"
        );
        assert!(
            section.text.contains("引用只在最近一次 snapshot 之后有效"),
            "纪律段应说明 ref 的有效期"
        );
        assert!(
            section.text.contains("绝不关闭不是自己开的 tab"),
            "纪律段应写明不碰用户原有标签页"
        );
        assert!(
            section.text.contains("list 里本次打开的 tab 全部消失"),
            "纪律段应给出收尾判定口径"
        );
        assert!(
            section.text.contains("绝不凭空拉起实例"),
            "纪律段应说明查询命令不拉起浏览器"
        );
        assert!(section.text.contains("visualMode"));
        let user_body = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(!user_body.contains("绝不关闭不是自己开的 tab"));

        // persona 变体带 hub 同样有段。
        let (prompt, _registry) =
            shipped_with_persona_and_browser_and_ask("自定义 persona".to_string(), Some(fake_hub()), false);
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
        for (prompt, _tools) in [
            default_shipped(),
            default_shipped_with_browser(None),
        ] {
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

    /// 子代理只读集合含 browser:对应的纪律段与 schema 必须一起出现,
    /// 否则子代理用 browser 抓网页却不知道收尾义务。
    #[test]
    fn browser_section_and_schema_available_to_subagents() {
        assert!(
            crate::SUBAGENT_READ_ONLY_TOOLS.contains(&"browser"),
            "browser 应授予子代理"
        );
        let (prompt, registry) =
            default_shipped_with_browser_and_ask(Some(fake_hub()), true);
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
        assert!(section.text.contains("真正卡住的时刻"));
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
        assert!(!user_body.contains("真正卡住的时刻"));

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
        let (prompt, registry) = shipped_with_persona_and_browser_and_ask(
            "自定义 persona".to_string(),
            None,
            true,
        );
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
            hub.visual_requests.load(std::sync::atomic::Ordering::SeqCst),
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
        let output = run_browser_tool(
            hub.clone() as crate::BrowserHub,
            r#"{"method":"list"}"#,
        )
        .await;
        assert!(!output.is_error, "command should succeed");
        assert_eq!(
            hub.visual_requests.load(std::sync::atomic::Ordering::SeqCst),
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
        prompt.variable("cwd", |context| context.cwd.clone()).unwrap();
        prompt.variable("model", |context| context.model.clone()).unwrap();
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
        assert!(body.contains("spawn_agent/fork_agent"), "{}", &body[..body.len().min(2000)]);
        assert!(body.contains("job_start"));
        assert!(body.contains("<available_skills>"));
        // Model audience 段不进用户可见副本。
        let user_body = denia_system_prompt::render_prompt_for_user(&assembly);
        assert!(!user_body.contains("spawn_agent/fork_agent"));
        assert!(!user_body.contains("job_start"));
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
