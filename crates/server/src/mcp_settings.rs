//! MCP 设置:`mcp` 命名空间的配置形状与校验。
//!
//! 与 console/runtime 命名空间同构:`McpSettings` 提供类型与默认值,
//! `validate_mcp` 在写入层拒绝非法配置并点名 offending 字段(fail loud)。
//!
//! 环境值是凭据:`secrets` 声明整条路径,让 settings 的 describe 永不
//! 回传明文(UI 只看到 key)。

use std::collections::BTreeMap;

use denia_mcp::{MAX_ARGS, MAX_SERVERS, McpServerConfig, is_valid_server_id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// 命名空间名。
pub const MCP_NS: &str = "mcp";

/// `mcp` 段:`{ "servers": [ … ] }`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSettings {
    #[serde(default)]
    pub servers: Vec<McpServerConfig>,
}

impl McpSettings {
    /// 从已解析的命名空间值读取(解析失败给空配置,不阻断启动)。
    pub fn from_value(value: &Value) -> Self {
        serde_json::from_value(value.clone()).unwrap_or_default()
    }
}

/// 写入层校验:任何一条不合法即拒绝整段(点名字段,便于 UI 提示)。
pub fn validate_mcp(value: Value) -> Result<Value, String> {
    let parsed: McpSettings =
        serde_json::from_value(value).map_err(|error| format!("mcp 配置无效:{error}"))?;
    if parsed.servers.len() > MAX_SERVERS {
        return Err(format!(
            "servers 最多 {MAX_SERVERS} 条,当前 {} 条",
            parsed.servers.len()
        ));
    }
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    for server in &parsed.servers {
        validate_server(server)?;
        if seen.insert(server.id.as_str(), ()).is_some() {
            return Err(format!("服务器 id 重复:'{}'", server.id));
        }
    }
    serde_json::to_value(parsed).map_err(|error| error.to_string())
}

fn validate_server(server: &McpServerConfig) -> Result<(), String> {
    if !is_valid_server_id(&server.id) {
        return Err(format!(
            "服务器 id '{}' 不合法:必须小写字母开头,只含 a-z/0-9/-,长度 1-32",
            server.id
        ));
    }
    // 传输三选一:stdio(本地命令)/ http(Streamable HTTP)/ sse(HTTP+SSE)。
    // 非 stdio 必须给 url;stdio 必须给 command。
    if !denia_mcp::is_supported_transport(&server.transport) {
        return Err(format!(
            "服务器 '{}' 的 transport '{}' 不受支持;可用:{}",
            server.id,
            server.transport,
            denia_mcp::SUPPORTED_TRANSPORTS.join(" / ")
        ));
    }
    if server.is_http() {
        let url = server.url.as_deref().unwrap_or("");
        if !denia_mcp::is_valid_url(url) {
            return Err(format!(
                "服务器 '{}' 是 {} 传输,url 必须是合法的 http/https 地址;当前是 '{}'",
                server.id, server.transport, url
            ));
        }
        // stdio 专属字段留着没意义:静默忽略会让用户以为生效了,直接点名。
        if !server.command.trim().is_empty() {
            return Err(format!(
                "服务器 '{}' 是 {} 传输,不接受 command/args/env;请改选 stdio 或清空这些字段",
                server.id, server.transport
            ));
        }
    } else if server.command.trim().is_empty() {
        return Err(format!("服务器 '{}' 缺少 command", server.id));
    }
    for key in server.headers.keys() {
        if key.trim().is_empty() {
            return Err(format!("服务器 '{}' 的请求头名不能为空", server.id));
        }
    }
    if server.args.len() > MAX_ARGS {
        return Err(format!(
            "服务器 '{}' 的参数过多(最多 {MAX_ARGS} 个)",
            server.id
        ));
    }
    for arg in &server.args {
        if arg.trim().is_empty() {
            return Err(format!("服务器 '{}' 的参数不能为空字符串", server.id));
        }
    }
    for key in server.env.keys() {
        if key.trim().is_empty() {
            return Err(format!("服务器 '{}' 的环境变量名不能为空", server.id));
        }
    }
    if let Some(cwd) = &server.cwd
        && !cwd.is_absolute()
    {
        return Err(format!(
            "服务器 '{}' 的 cwd 必须是绝对路径,当前是 '{}'",
            server.id,
            cwd.display()
        ));
    }
    // 协议版本:目前固定 2024-11-05;保留字段以便后续升级,不在白名单内即拒绝。
    if server.protocol_version.trim().is_empty() {
        return Err(format!("服务器 '{}' 的协议版本不能为空", server.id));
    }
    // 作用域:user 立刻生效;project 字段已存,本期按 user 同样处理
    // (工作区域生效需要把 server 绑定到工作区元数据并按会话 cwd 过滤,
    // 涉及 sessions.cwd → workspace_id 的链路重构,先做对 UI 与配置的兼容)。
    if matches!(server.scope, denia_mcp::McpServerScope::Project) && server.workspace_id.is_none() {
        return Err(format!(
            "服务器 '{}' 选了「项目」作用域却没指定 workspace_id",
            server.id
        ));
    }
    if let Some(timeout) = server.call_timeout_ms
        && (timeout == 0 || timeout > 600_000)
    {
        return Err(format!(
            "服务器 '{}' 的 callTimeoutMs 必须在 1-600000 之间",
            server.id
        ));
    }
    Ok(())
}

/// 默认值:没有服务器。
pub fn defaults() -> Value {
    json!({ "servers": [] })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server(id: &str) -> Value {
        json!({ "id": id, "transport": "stdio", "command": "npx", "args": ["-y", "x"] })
    }

    #[test]
    fn accepts_empty_and_valid_configs() {
        assert!(validate_mcp(json!({ "servers": [] })).is_ok());
        assert!(validate_mcp(json!({ "servers": [server("fs")] })).is_ok());
    }

    #[test]
    fn rejects_bad_ids() {
        let long_id = "x".repeat(33);
        for bad in ["GitHub", "1st", "has_underscore", "", long_id.as_str()] {
            let value = json!({ "servers": [server(bad)] });
            let error = validate_mcp(value).unwrap_err();
            assert!(error.contains("不合法"), "{bad}: {error}");
        }
    }

    #[test]
    fn rejects_duplicate_ids() {
        let value = json!({ "servers": [server("fs"), server("fs")] });
        let error = validate_mcp(value).unwrap_err();
        assert!(error.contains("重复"));
    }

    #[test]
    fn rejects_unsupported_transport_and_bad_urls() {
        // 白名单外的传输:点名可用值。
        let value = json!({ "servers": [{ "id": "fs", "transport": "websocket", "command": "x" }] });
        let error = validate_mcp(value).unwrap_err();
        assert!(error.contains("不受支持"), "{error}");
        assert!(error.contains("stdio"), "应列出可用传输:{error}");

        // http/sse 必须给合法 http(s) 的 url。
        let bad_url = json!({
            "servers": [{ "id": "web", "transport": "http", "url": "ftp://x" }]
        });
        assert!(validate_mcp(bad_url).unwrap_err().contains("url"));

        // http 传输不接受 command/args/env:静默忽略会让用户以为生效了。
        let mixed = json!({
            "servers": [{ "id": "web", "transport": "http", "url": "https://x/mcp", "command": "npx" }]
        });
        assert!(validate_mcp(mixed).unwrap_err().contains("不接受 command"));
    }

    #[test]
    fn accepts_http_and_sse_transports() {
        for transport in ["http", "sse"] {
            let value = json!({
                "servers": [{ "id": "web", "transport": transport, "url": "https://example.com/mcp" }]
            });
            assert!(
                validate_mcp(value).is_ok(),
                "{transport} 传输应被接受"
            );
        }
    }

    #[test]
    fn rejects_missing_command_and_bad_args() {
        let no_command =
            json!({ "servers": [{ "id": "fs", "transport": "stdio", "command": "  " }] });
        assert!(validate_mcp(no_command).unwrap_err().contains("缺少 command"));

        let blank_arg = json!({
            "servers": [{ "id": "fs", "transport": "stdio", "command": "npx", "args": ["", "y"] }]
        });
        assert!(validate_mcp(blank_arg).unwrap_err().contains("参数不能为空"));

        let too_many = json!({
            "servers": [{
                "id": "fs", "transport": "stdio", "command": "npx",
                "args": (0..=MAX_ARGS).map(|i| i.to_string()).collect::<Vec<_>>()
            }]
        });
        assert!(validate_mcp(too_many).unwrap_err().contains("参数过多"));
    }

    #[test]
    fn rejects_relative_cwd() {
        let value = json!({
            "servers": [{ "id": "fs", "transport": "stdio", "command": "npx", "cwd": "relative" }]
        });
        assert!(validate_mcp(value).unwrap_err().contains("绝对路径"));
    }

    #[test]
    fn rejects_too_many_servers() {
        let servers: Vec<Value> = (0..=MAX_SERVERS)
            .map(|i| server(&format!("s{i}")))
            .collect();
        let error = validate_mcp(json!({ "servers": servers })).unwrap_err();
        assert!(error.contains("最多"));
    }

    #[test]
    fn accepts_scope_and_protocol_version() {
        let user_only = json!({
            "servers": [{
                "id": "fs", "transport": "stdio", "command": "npx",
                "scope": "user", "protocolVersion": "2024-11-05", "callTimeoutMs": 30000
            }]
        });
        assert!(validate_mcp(user_only).is_ok());
    }

    #[test]
    fn rejects_project_scope_without_workspace_id() {
        let value = json!({
            "servers": [{
                "id": "fs", "transport": "stdio", "command": "npx",
                "scope": "project"
            }]
        });
        assert!(validate_mcp(value).unwrap_err().contains("workspace_id"));
    }

    #[test]
    fn rejects_out_of_range_call_timeout() {
        let zero = json!({
            "servers": [{ "id": "fs", "transport": "stdio", "command": "npx", "callTimeoutMs": 0 }]
        });
        assert!(validate_mcp(zero).unwrap_err().contains("callTimeoutMs"));
        let huge = json!({
            "servers": [{ "id": "fs", "transport": "stdio", "command": "npx", "callTimeoutMs": 700_000 }]
        });
        assert!(validate_mcp(huge).unwrap_err().contains("callTimeoutMs"));
    }

    #[test]
    fn rejects_missing_protocol_version() {
        let value = json!({
            "servers": [{ "id": "fs", "transport": "stdio", "command": "npx", "protocolVersion": "" }]
        });
        assert!(validate_mcp(value).unwrap_err().contains("协议版本"));
    }

    #[test]
    fn defaults_are_empty_servers() {
        assert_eq!(defaults(), json!({ "servers": [] }));
        assert!(McpSettings::from_value(&defaults()).servers.is_empty());
        // 坏值不阻断启动:退化成空配置。
        assert!(McpSettings::from_value(&json!("not an object")).servers.is_empty());
    }
}
