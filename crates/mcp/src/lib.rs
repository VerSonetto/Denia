//! MCP(Model Context Protocol)客户端:JSON-RPC over stdio。
//!
//! 范围与取舍:
//! - **只做 stdio 传输**。MCP 的 HTTP/SSE 传输在配置层就被明确拒绝
//!   (见 server 侧 `mcp_settings::validate_mcp`),这里不留半成品实现;
//! - **进程即连接**:每个 MCP 服务器是一个常驻子进程,stdin/stdout 承载
//!   JSON-RPC 帧(每行一条 JSON),stderr 丢弃(MCP 服务器常把日志写
//!   stderr,混进 stdout 会污染协议帧);
//! - **fail loud 但不拖垮别人**:单个服务器握手/拉取失败只把它自己标成
//!   error,其它服务器照常可用;错误文本是中文且可执行(模型与 UI 共用);
//! - **工具结果预算**:MCP 工具返回的长结果在工具层按字符预算截断,并提供
//!   offset/limit 分页(与 `read_file` 同形态),再由落盘层的统一输出
//!   预算兜底——两层都不让超长外呼结果撑爆上下文。

pub mod client;
pub mod config;
pub mod manager;
pub mod protocol;
pub mod transport;

pub use client::{McpClient, McpClientError};
pub use config::{
    McpServerConfig, McpServerScope, MAX_ARGS, MAX_SERVERS, SUPPORTED_TRANSPORTS,
    is_supported_transport, is_valid_server_id, is_valid_url, qualify_tool_name,
};
pub use manager::{
    McpManager, McpServerState, McpServerStatus, McpSnapshot, McpToolState, PAGE_CHARS,
    PagedResult,
};
pub use protocol::{McpCallResult, McpToolDef};
pub use transport::McpTransport;
