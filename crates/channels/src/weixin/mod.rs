//! 微信 iLink Bot 通道。

pub mod bot;
pub mod client;
pub mod login;
pub mod media;
pub mod protocol;
pub mod store;

use std::sync::Mutex;

/// 通道运行状态,由 bot 循环维护、由 API 读取。
#[derive(Debug, Default)]
pub struct RuntimeStatus {
    pub connected: bool,
    pub last_error: Option<String>,
    /// 最近一条入站消息的时刻(epoch ms)、字符数与处理动作。
    /// 普通消息出问题时,面板据此区分"没收到"与"收到了但某一步失败"。
    pub last_inbound_at: Option<u64>,
    pub last_inbound_chars: usize,
    pub last_action: Option<String>,
}

impl RuntimeStatus {
    pub fn snapshot(&self) -> (bool, Option<String>) {
        (self.connected, self.last_error.clone())
    }
}

/// 多个任务共享的状态句柄。
pub type SharedStatus = std::sync::Arc<Mutex<RuntimeStatus>>;
