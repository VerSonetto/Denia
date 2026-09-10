//! MCP 服务器配置与模型可见工具名的规范化。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 一个 MCP 服务器的配置(settings `mcp` 命名空间的一段)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct McpServerConfig {
    /// 服务器 id:同时是工具名前缀,必须 `[a-z][a-z0-9-]*`。
    pub id: String,
    /// 传输方式:`stdio` | `http`(Streamable HTTP) | `sse`(HTTP + SSE)。
    pub transport: String,
    /// stdio 传输:可执行程序。
    pub command: String,
    /// stdio 传输:命令行参数。
    pub args: Vec<String>,
    /// stdio 传输:附加环境变量(值属敏感信息,只存不回传明文)。
    pub env: BTreeMap<String, String>,
    /// stdio 传输:子进程工作目录;`None` = 继承 denia 进程。
    pub cwd: Option<PathBuf>,
    /// http / sse 传输:服务端地址(Streamable HTTP 的 POST 端点,
    /// 或 SSE 的建立流地址)。
    pub url: Option<String>,
    /// http / sse 传输:附加请求头(如 `Authorization`);
    /// 值属敏感信息,设置页只回传 key。
    pub headers: BTreeMap<String, String>,
    /// 总开关:关闭后不启动、不注册工具。
    pub enabled: bool,
    /// 逐工具关闭列表(服务器原始工具名);关闭的工具不进 schema。
    pub disabled_tools: Vec<String>,
    /// 作用域:`user` = 全局,所有会话可见;`project` = 仅工作区可见,
    /// 必须配 `workspace_id` 一起读(UI 上按当前工作区过滤显示)。
    #[serde(default = "default_scope")]
    pub scope: McpServerScope,
    /// 作用域为 `project` 时绑定的 workspace id。
    #[serde(default)]
    pub workspace_id: Option<String>,
    /// 协议版本(目前固定 `2024-11-05`,预留以便未来协议升级)。
    #[serde(default = "default_protocol_version")]
    pub protocol_version: String,
    /// 工具调用超时(毫秒);`None` = 用客户端默认。
    #[serde(default)]
    pub call_timeout_ms: Option<u64>,
}

/// 作用范围:对齐 dsh UI 的「用户 / 项目」下拉。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpServerScope {
    /// 全局:所有会话可见。保存在 settings 的 `mcp` 段。
    User,
    /// 工作区:仅 `workspace_id` 命中的会话可见。保存在工作区元数据里。
    Project,
}

fn default_scope() -> McpServerScope {
    McpServerScope::User
}

fn default_protocol_version() -> String {
    "2024-11-05".to_string()
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            id: String::new(),
            transport: "stdio".to_string(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            url: None,
            headers: BTreeMap::new(),
            enabled: true,
            disabled_tools: Vec::new(),
            scope: McpServerScope::User,
            workspace_id: None,
            protocol_version: default_protocol_version(),
            call_timeout_ms: None,
        }
    }
}

impl McpServerConfig {
    /// 决定是否需要重连的指纹:与连接有关、与工具开关无关的字段。
    /// (工具开关只是过滤,不需要重启子进程/重连。)
    pub fn connection_key(
        &self,
    ) -> (String, Vec<String>, BTreeMap<String, String>, Option<PathBuf>, String, bool, Option<String>, BTreeMap<String, String>)
    {
        (
            self.command.clone(),
            self.args.clone(),
            self.env.clone(),
            self.cwd.clone(),
            self.transport.clone(),
            self.enabled,
            self.url.clone(),
            self.headers.clone(),
        )
    }

    /// 传输是否走 HTTP(需要 url)。
    pub fn is_http(&self) -> bool {
        matches!(self.transport.as_str(), "http" | "sse")
    }
}

/// 受支持的传输方式;配置校验与 UI 共用同一份清单,不会两处漂移。
pub const SUPPORTED_TRANSPORTS: &[&str] = &["stdio", "http", "sse"];

/// 传输是否受支持。
pub fn is_supported_transport(transport: &str) -> bool {
    SUPPORTED_TRANSPORTS.contains(&transport)
}

/// URL 校验:只允许 http/https(其它 scheme 没有意义,且可能是注入)。
pub fn is_valid_url(raw: &str) -> bool {
    match reqwest::Url::parse(raw) {
        Ok(url) => matches!(url.scheme(), "http" | "https"),
        Err(_) => false,
    }
}

/// 服务器 id 语法:`[a-z][a-z0-9-]*`(同时是工具名前缀,必须能被模型稳定写出)。
pub fn is_valid_server_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 32 || !bytes[0].is_ascii_lowercase() {
        return false;
    }
    bytes
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

/// 模型可见工具名:`mcp__<server>__<tool>`。
///
/// 服务器原始工具名可能带空格、点号等模型容易写错的字符,统一折叠成
/// `[A-Za-z0-9_-]`;折叠后为空(纯符号名)则退化成 `tool`。下划线分隔符
/// 是保留字符,所以折叠时把连续下划线压成一个,避免出现 `mcp__fs__a_b`
/// 里的歧义。
pub fn qualify_tool_name(server: &str, tool: &str) -> String {
    let mut sanitized = String::with_capacity(tool.len());
    let mut last_was_underscore = false;
    for ch in tool.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch);
            last_was_underscore = false;
        } else if ch == '_' || ch == '-' || ch.is_whitespace() || ch == '.' {
            if !last_was_underscore {
                sanitized.push('_');
                last_was_underscore = true;
            }
        } else {
            // 其余字符(如 `*`、中文标点)直接丢弃:留着会污染工具名。
        }
    }
    let sanitized = sanitized.trim_matches('_');
    let sanitized = if sanitized.is_empty() {
        "tool"
    } else {
        sanitized
    };
    format!("mcp__{server}__{sanitized}")
}

/// 配置里服务器数量的硬上限(防止一条配置拖出几十个子进程)。
pub const MAX_SERVERS: usize = 32;
/// 单个命令允许的最大参数个数。
pub const MAX_ARGS: usize = 64;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_id_grammar() {
        assert!(is_valid_server_id("github"));
        assert!(is_valid_server_id("fs-2"));
        assert!(!is_valid_server_id(""));
        assert!(!is_valid_server_id("GitHub"));
        assert!(!is_valid_server_id("1st"));
        assert!(!is_valid_server_id("has_underscore"));
        assert!(!is_valid_server_id(&"a".repeat(33)));
    }

    #[test]
    fn tool_names_are_model_writable() {
        assert_eq!(qualify_tool_name("fs", "read_file"), "mcp__fs__read_file");
        // 空格/点号折叠成下划线,模型不会被奇怪字符卡住。
        assert_eq!(qualify_tool_name("fs", "read file.v2"), "mcp__fs__read_file_v2");
        // 连续符号压成一个下划线,不留 `a__b` 这种歧义。
        assert_eq!(qualify_tool_name("fs", "a  --  b"), "mcp__fs__a_b");
        // 纯符号名退化成 tool,不会产出空名字。
        assert_eq!(qualify_tool_name("fs", "***"), "mcp__fs__tool");
        assert_eq!(qualify_tool_name("fs", "  "), "mcp__fs__tool");
    }

    #[test]
    fn connection_key_ignores_tool_toggles() {
        let a = McpServerConfig {
            id: "fs".to_string(),
            command: "npx".to_string(),
            ..Default::default()
        };
        let mut b = a.clone();
        b.disabled_tools.push("read_file".to_string());
        // 只改工具开关不该触发重连。
        assert_eq!(a.connection_key(), b.connection_key());
        // 改命令/环境要触发重连。
        b.command = "node".to_string();
        assert_ne!(a.connection_key(), b.connection_key());
    }
}
