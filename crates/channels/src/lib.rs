//! 消息平台通道:把外部 IM(当前是微信 iLink Bot)接到 denia 的会话面。
//!
//! 这一层只认识 [`bridge::ChannelHost`] 抽象:工作区、会话、权限、一轮问答。
//! 具体存储与 agent 运行时由宿主(`denia-server`)注入,便于：
//! - 协议客户端与指令状态机能脱离服务端做单测;
//! - 未来接入其它平台时只新增 adapter,不复制会话逻辑。

pub mod bindings;
pub mod bridge;
pub mod commands;
pub mod qqbot;
pub mod weixin;
