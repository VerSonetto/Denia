//! 一次性 / 短时效访问票据(ticket)。
//!
//! 票据是"扫码那一刻的入场券",不是会话凭据:它只证明"这个人拿到了我
//! 刚刚发出的链接"。因此:
//! - 明文只出现在响应体与二维码里,服务端表按 sha256 索引;
//! - 隧道票据**强制一次性**(配置层保证 TTL ≤ 600s,构造时强制单次使用);
//! - 兑换即扣减。若后续 PIN 输错,票据不回滚 —— 重试额度由 PIN challenge
//!   自己承担(单 challenge 允许有限次尝试),避免"票据可反复兑换"这条
//!   退化成爆破通道。

use std::collections::HashMap;
use std::sync::Mutex;

use super::Channel;
use super::secret;

/// 表里的一张票据(不含明文)。
#[derive(Debug, Clone)]
struct Ticket {
    id: String,
    via: Channel,
    created_at: u64,
    expires_at: u64,
    /// 剩余可用次数。一次性票据为 1。
    remaining: u32,
    pin_required: bool,
}

/// 兑换票据的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redeemed {
    pub id: String,
    pub via: Channel,
    pub pin_required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketError {
    /// 表里没有这张票:伪造、已用尽、或已被断开清理。
    Unknown,
    Expired,
}

impl TicketError {
    pub fn code(self) -> &'static str {
        match self {
            TicketError::Unknown => "remote/ticket-invalid",
            TicketError::Expired => "remote/ticket-expired",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            TicketError::Unknown => "访问票据无效或已使用,请在 denia 上重新生成链接",
            TicketError::Expired => "访问票据已过期,请在 denia 上重新生成链接",
        }
    }
}

/// 票据表。键是 `sha256(明文)`。
#[derive(Default)]
pub struct TicketTable {
    inner: Mutex<HashMap<String, Ticket>>,
}

impl TicketTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 签发一张票据,返回明文(只此一次)。
    pub fn issue(
        &self,
        via: Channel,
        ttl_seconds: u64,
        single_use: bool,
        pin_required: bool,
        now_ms: u64,
    ) -> String {
        let token = secret::new_token();
        let ticket = Ticket {
            id: secret::new_id(),
            via,
            created_at: now_ms,
            expires_at: now_ms.saturating_add(ttl_seconds.saturating_mul(1000)),
            remaining: if single_use { 1 } else { u32::MAX },
            pin_required,
        };
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.insert(secret::hash_token(&token), ticket);
        token
    }

    /// 兑换:校验存在与有效期,并扣减一次使用额度。
    pub fn redeem(&self, token: &str, now_ms: u64) -> Result<Redeemed, TicketError> {
        let key = secret::hash_token(token);
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(ticket) = map.get_mut(&key) else {
            return Err(TicketError::Unknown);
        };
        if now_ms >= ticket.expires_at {
            map.remove(&key);
            return Err(TicketError::Expired);
        }
        if ticket.remaining == 0 {
            map.remove(&key);
            return Err(TicketError::Unknown);
        }
        let redeemed = Redeemed {
            id: ticket.id.clone(),
            via: ticket.via,
            pin_required: ticket.pin_required,
        };
        ticket.remaining -= 1;
        if ticket.remaining == 0 {
            map.remove(&key);
        }
        Ok(redeemed)
    }

    /// 清理过期票据(断开连接与状态查询时调用,防止长期运行堆垃圾)。
    pub fn purge_expired(&self, now_ms: u64) -> usize {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let before = map.len();
        map.retain(|_, ticket| now_ms < ticket.expires_at);
        before - map.len()
    }

    /// 吊销。`via` 为 None 时清空全部(断开隧道 = 隧道票据立即失效)。
    pub fn revoke(&self, via: Option<Channel>) -> usize {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match via {
            None => {
                let count = map.len();
                map.clear();
                count
            }
            Some(channel) => {
                let before = map.len();
                map.retain(|_, ticket| ticket.via != channel);
                before - map.len()
            }
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    #[test]
    fn single_use_ticket_burns_after_first_redeem() {
        let table = TicketTable::new();
        let token = table.issue(Channel::Tunnel, 120, true, true, NOW);
        let first = table.redeem(&token, NOW + 1).expect("首次兑换必须成功");
        assert_eq!(first.via, Channel::Tunnel);
        assert!(first.pin_required);
        assert_eq!(
            table.redeem(&token, NOW + 2),
            Err(TicketError::Unknown),
            "一次性票据不得二次兑换"
        );
        assert!(table.is_empty(), "用尽后必须从表里移除");
    }

    #[test]
    fn reusable_ticket_survives_multiple_devices() {
        let table = TicketTable::new();
        let token = table.issue(Channel::Lan, 600, false, false, NOW);
        assert!(table.redeem(&token, NOW + 1).is_ok());
        assert!(table.redeem(&token, NOW + 2).is_ok());
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn expired_ticket_is_rejected_and_evicted() {
        let table = TicketTable::new();
        let token = table.issue(Channel::Tunnel, 120, true, false, NOW);
        assert_eq!(
            table.redeem(&token, NOW + 120_000),
            Err(TicketError::Expired),
            "到期时刻即失效"
        );
        assert!(table.is_empty(), "过期票据必须顺手清掉");
    }

    #[test]
    fn unknown_ticket_is_rejected() {
        let table = TicketTable::new();
        assert_eq!(table.redeem("not-a-real-ticket", NOW), Err(TicketError::Unknown));
    }

    #[test]
    fn revoke_by_channel_only_drops_that_channel() {
        let table = TicketTable::new();
        let lan = table.issue(Channel::Lan, 600, false, false, NOW);
        let tunnel = table.issue(Channel::Tunnel, 120, true, false, NOW);
        assert_eq!(table.revoke(Some(Channel::Tunnel)), 1);
        assert_eq!(table.redeem(&tunnel, NOW + 1), Err(TicketError::Unknown));
        assert!(table.redeem(&lan, NOW + 1).is_ok(), "局域网票据不受影响");
    }

    #[test]
    fn revoke_all_clears_everything() {
        let table = TicketTable::new();
        let lan = table.issue(Channel::Lan, 600, false, false, NOW);
        let tunnel = table.issue(Channel::Tunnel, 120, true, false, NOW);
        assert_eq!(table.revoke(None), 2);
        assert_eq!(table.redeem(&lan, NOW + 1), Err(TicketError::Unknown));
        assert_eq!(table.redeem(&tunnel, NOW + 1), Err(TicketError::Unknown));
    }

    #[test]
    fn purge_removes_only_expired() {
        let table = TicketTable::new();
        table.issue(Channel::Lan, 60, false, false, NOW);
        table.issue(Channel::Lan, 600, false, false, NOW);
        assert_eq!(table.purge_expired(NOW + 61_000), 1);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn plaintext_never_stored() {
        let table = TicketTable::new();
        let token = table.issue(Channel::Lan, 60, false, false, NOW);
        let map = table.inner.lock().unwrap();
        assert!(
            map.keys().all(|key| key != &token && !key.contains(&token)),
            "表里不得出现明文票据"
        );
        assert_eq!(map.keys().next().unwrap(), &secret::hash_token(&token));
    }
}
