//! PIN 二次验证:扫码之后、建立会话之前的那一步。
//!
//! challenge 把"哪张票据、来自哪个 IP"绑在一起:
//! - 一次扫码 = 一个 challenge,用完即销毁(即便后面输错也只是重试同一个,
//!   不存在"换一张票据继续试 PIN");
//! - 绑定来源 IP:challenge 泄漏给第三方也没法用;
//! - 失败次数到顶即作废,逼迫攻击者回到 ticket 那一步重新受限流约束。

use std::collections::HashMap;
use std::sync::Mutex;

use super::Channel;
use super::secret;

/// 校验结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// 通过。challenge 已销毁。
    Ok,
    /// PIN 不匹配。`attempts_left` 为 0 时 challenge 已作废。
    Mismatch { attempts_left: u32 },
    /// challenge 不存在:过期、已用、或已被断开清理。
    Unknown,
    /// 来源 IP 与发起扫码时不一致。
    PeerMismatch,
}

/// 一次 PIN 挑战。
#[derive(Debug, Clone)]
struct Challenge {
    ticket_id: String,
    via: Channel,
    peer: String,
    /// `sha256(challenge_id + pin)`:challenge id 当盐,避免同一 PIN 的哈希相同。
    pin_hash: String,
    expires_at: u64,
    attempts_left: u32,
}

/// challenge 表(键是 challenge id,本身不是凭据 —— 拿不到它也没用,
/// 因为还要求同 IP 且票据已兑换)。
#[derive(Default)]
pub struct ChallengeTable {
    inner: Mutex<HashMap<String, Challenge>>,
}

impl ChallengeTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 开一个 challenge,返回 id。
    pub fn open(
        &self,
        ticket_id: &str,
        via: Channel,
        peer: &str,
        pin: &str,
        ttl_seconds: u64,
        max_attempts: u32,
        now_ms: u64,
    ) -> String {
        let id = secret::new_id();
        let challenge = Challenge {
            ticket_id: ticket_id.to_string(),
            via,
            peer: peer.to_string(),
            pin_hash: hash_pin(&id, pin),
            expires_at: now_ms.saturating_add(ttl_seconds.saturating_mul(1000)),
            attempts_left: max_attempts,
        };
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.insert(id.clone(), challenge);
        id
    }

    /// 校验 PIN。失败会扣减次数;次数耗尽或过期即销毁 challenge。
    pub fn verify(&self, id: &str, peer: &str, pin: &str, now_ms: u64) -> VerifyOutcome {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(challenge) = map.get_mut(id) else {
            return VerifyOutcome::Unknown;
        };
        if now_ms >= challenge.expires_at {
            map.remove(id);
            return VerifyOutcome::Unknown;
        }
        if challenge.peer != peer {
            return VerifyOutcome::PeerMismatch;
        }
        if secret::constant_time_eq(
            challenge.pin_hash.as_bytes(),
            hash_pin(id, pin).as_bytes(),
        ) {
            map.remove(id);
            return VerifyOutcome::Ok;
        }
        challenge.attempts_left = challenge.attempts_left.saturating_sub(1);
        let left = challenge.attempts_left;
        if left == 0 {
            map.remove(id);
        }
        VerifyOutcome::Mismatch { attempts_left: left }
    }

    /// 取 challenge 所属通道(审计与超时参数用)。已销毁时返回 None。
    pub fn channel_of(&self, id: &str) -> Option<Channel> {
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.get(id).map(|challenge| challenge.via)
    }

    /// 清理过期 challenge。
    pub fn purge_expired(&self, now_ms: u64) -> usize {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let before = map.len();
        map.retain(|_, challenge| now_ms < challenge.expires_at);
        before - map.len()
    }

    /// 全部作废(断开连接 / 关闭隧道)。
    pub fn revoke_all(&self) -> usize {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let count = map.len();
        map.clear();
        count
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn hash_pin(challenge_id: &str, pin: &str) -> String {
    secret::hash_token(&format!("{challenge_id}:{pin}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;

    fn open(table: &ChallengeTable, peer: &str) -> String {
        table.open("ticket-1", Channel::Tunnel, peer, "123456", 120, 3, NOW)
    }

    #[test]
    fn correct_pin_opens_once() {
        let table = ChallengeTable::new();
        let id = open(&table, "203.0.113.7");
        assert_eq!(table.verify(&id, "203.0.113.7", "123456", NOW + 1), VerifyOutcome::Ok);
        assert_eq!(
            table.verify(&id, "203.0.113.7", "123456", NOW + 2),
            VerifyOutcome::Unknown,
            "challenge 必须一次性"
        );
        assert!(table.is_empty());
    }

    #[test]
    fn wrong_pin_decrements_and_burns_after_limit() {
        let table = ChallengeTable::new();
        let id = open(&table, "203.0.113.7");
        assert_eq!(
            table.verify(&id, "203.0.113.7", "000000", NOW + 1),
            VerifyOutcome::Mismatch { attempts_left: 2 }
        );
        assert_eq!(
            table.verify(&id, "203.0.113.7", "111111", NOW + 2),
            VerifyOutcome::Mismatch { attempts_left: 1 }
        );
        assert_eq!(
            table.verify(&id, "203.0.113.7", "222222", NOW + 3),
            VerifyOutcome::Mismatch { attempts_left: 0 }
        );
        assert_eq!(
            table.verify(&id, "203.0.113.7", "123456", NOW + 4),
            VerifyOutcome::Unknown,
            "次数耗尽后即便输对也必须作废,必须重新扫码"
        );
    }

    #[test]
    fn challenge_is_bound_to_peer() {
        let table = ChallengeTable::new();
        let id = open(&table, "203.0.113.7");
        assert_eq!(
            table.verify(&id, "198.51.100.9", "123456", NOW + 1),
            VerifyOutcome::PeerMismatch
        );
        // 换 IP 尝试不消耗本 IP 的额度。
        assert_eq!(table.verify(&id, "203.0.113.7", "123456", NOW + 2), VerifyOutcome::Ok);
    }

    #[test]
    fn expired_challenge_is_rejected() {
        let table = ChallengeTable::new();
        let id = open(&table, "203.0.113.7");
        assert_eq!(
            table.verify(&id, "203.0.113.7", "123456", NOW + 120_000),
            VerifyOutcome::Unknown
        );
        assert!(table.is_empty());
    }

    #[test]
    fn purge_and_revoke() {
        let table = ChallengeTable::new();
        table.open("t1", Channel::Tunnel, "a", "123456", 60, 3, NOW);
        table.open("t2", Channel::Tunnel, "b", "123456", 600, 3, NOW);
        assert_eq!(table.purge_expired(NOW + 61_000), 1);
        assert_eq!(table.len(), 1);
        assert_eq!(table.revoke_all(), 1);
        assert!(table.is_empty());
    }

    #[test]
    fn pin_hash_never_contains_plaintext_pin() {
        let table = ChallengeTable::new();
        let id = open(&table, "203.0.113.7");
        let map = table.inner.lock().unwrap();
        let challenge = map.get(&id).unwrap();
        assert!(!challenge.pin_hash.contains("123456"));
        assert_eq!(challenge.pin_hash.len(), 64);
    }
}
