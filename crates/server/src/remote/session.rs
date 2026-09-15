//! 远程会话表:opaque 令牌 → 会话记录,支持空闲/绝对超时与主动吊销。
//!
//! 为什么不是签名 JWT:JWT 的"主动吊销"必须额外维护一张黑名单表,等于又
//! 回到服务端表;而 opaque 令牌 + 服务端表天然满足"服务端可校验 + 含签发
//! 与过期时间 + 含会话 ID + 可主动吊销",还省掉签名密钥的轮换与保管问题。
//! 代价是会话不跨进程重启 —— 这在安全方向上是失败(fail closed),可以接受。
//!
//! 表键是 `sha256(令牌)`:明文只出现在 `Set-Cookie` 与内存中的那一瞬间。

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Serialize;

use super::Channel;
use super::secret;

/// 一次签发的结果。`token` 是明文,**只此一次**。
#[derive(Debug, Clone)]
pub struct Issued {
    pub id: String,
    pub token: String,
}

/// 会话记录(不含任何令牌材料)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionRecord {
    pub id: String,
    pub via: Channel,
    /// 建立会话时的来源 IP(展示用;后续换 IP 不视为失效,只留痕)。
    pub peer: String,
    pub created_at: u64,
    pub last_seen_at: u64,
    /// 绝对过期时刻(epoch ms)。
    pub expires_at: u64,
    /// 空闲过期时刻(epoch ms),每次访问续期。
    pub idle_deadline: u64,
}

/// 会话总览(状态页用)。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub total: usize,
    pub lan: usize,
    pub tunnel: usize,
    pub items: Vec<SessionRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// 表里没有:伪造、已吊销、或服务重启。
    Unknown,
    /// 空闲超时。
    Idle,
    /// 绝对超时。
    Absolute,
}

impl SessionError {
    pub fn code(self) -> &'static str {
        match self {
            SessionError::Unknown => "remote/session-invalid",
            SessionError::Idle => "remote/session-idle-timeout",
            SessionError::Absolute => "remote/session-expired",
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            SessionError::Unknown => "远程会话无效,请重新扫码连接",
            SessionError::Idle => "远程会话因长时间无操作已失效,请重新扫码连接",
            SessionError::Absolute => "远程会话已达最长有效期,请重新扫码连接",
        }
    }
}

/// 会话表。键是 `sha256(令牌)`。
#[derive(Default)]
pub struct SessionTable {
    inner: Mutex<HashMap<String, SessionRecord>>,
}

impl SessionTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// 建立会话,返回明文令牌(只此一次)与会话 id。
    pub fn issue(
        &self,
        via: Channel,
        peer: &str,
        idle_seconds: u64,
        absolute_seconds: u64,
        now_ms: u64,
    ) -> Issued {
        let token = secret::new_token();
        let id = secret::new_id();
        let record = SessionRecord {
            id: id.clone(),
            via,
            peer: peer.to_string(),
            created_at: now_ms,
            last_seen_at: now_ms,
            expires_at: now_ms.saturating_add(absolute_seconds.saturating_mul(1000)),
            idle_deadline: now_ms.saturating_add(idle_seconds.saturating_mul(1000)),
        };
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.insert(secret::hash_token(&token), record);
        Issued { id, token }
    }

    /// 校验令牌并续期空闲计时。过期的会话顺手移除,返回 `None`。
    ///
    /// 空闲时长由调用方按会话通道给出(guard 传当前配置值):用户把空闲
    /// 超时从 30 分钟改成 5 分钟,应当立刻对新请求生效。
    pub fn authenticate<F>(&self, token: &str, now_ms: u64, idle_seconds: F) -> Option<SessionRecord>
    where
        F: FnOnce(Channel) -> u64,
    {
        let key = secret::hash_token(token);
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let record = map.get_mut(&key)?;
        if now_ms >= record.expires_at || now_ms >= record.idle_deadline {
            map.remove(&key);
            return None;
        }
        record.last_seen_at = now_ms;
        record.idle_deadline = now_ms.saturating_add(idle_seconds(record.via).saturating_mul(1000));
        Some(record.clone())
    }

    /// 按会话 id 吊销(状态页逐个踢人)。返回是否命中。
    pub fn revoke_session(&self, id: &str) -> bool {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let key = map
            .iter()
            .find(|(_, record)| record.id == id)
            .map(|(key, _)| key.clone());
        match key {
            Some(key) => map.remove(&key).is_some(),
            None => false,
        }
    }

    /// 按令牌吊销(退出登录)。
    pub fn revoke_token(&self, token: &str) -> bool {
        let key = secret::hash_token(token);
        self.inner
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key)
            .is_some()
    }

    /// 吊销整条通道的会话;`via` 为 None 时吊销全部。
    pub fn revoke(&self, via: Option<Channel>) -> usize {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let before = map.len();
        match via {
            None => map.clear(),
            Some(channel) => map.retain(|_, record| record.via != channel),
        }
        before - map.len()
    }

    /// 清理已过期的会话(定时清扫调用)。
    pub fn sweep_expired(&self, now_ms: u64) -> usize {
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let before = map.len();
        map.retain(|_, record| now_ms < record.expires_at && now_ms < record.idle_deadline);
        before - map.len()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 状态总览:计数 + 明细(按建立时间排序)。
    pub fn view(&self) -> SessionView {
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let mut items: Vec<SessionRecord> = map.values().cloned().collect();
        items.sort_by_key(|record| record.created_at);
        SessionView {
            total: items.len(),
            lan: items.iter().filter(|r| r.via == Channel::Lan).count(),
            tunnel: items.iter().filter(|r| r.via == Channel::Tunnel).count(),
            items,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_700_000_000_000;
    const IDLE: u64 = 900;
    const ABS: u64 = 21600;

    fn verify(table: &SessionTable, token: &str, now: u64) -> Option<SessionRecord> {
        table.authenticate(token, now, |_| IDLE)
    }

    #[test]
    fn issue_then_authenticate_round_trip() {
        let table = SessionTable::new();
        let issued = table.issue(Channel::Tunnel, "203.0.113.9", IDLE, ABS, NOW);
        let record = verify(&table, &issued.token, NOW + 1000).expect("会话应当有效");
        assert_eq!(record.id, issued.id);
        assert_eq!(record.via, Channel::Tunnel);
        assert_eq!(record.peer, "203.0.113.9");
        assert_eq!(record.expires_at, NOW + ABS * 1000);
    }

    #[test]
    fn idle_timeout_revokes_session() {
        let table = SessionTable::new();
        let issued = table.issue(Channel::Tunnel, "peer", IDLE, ABS, NOW);
        assert!(verify(&table, &issued.token, NOW + IDLE * 1000).is_none(), "空闲到点即失效");
        assert_eq!(table.len(), 0, "失效会话必须被移除");
    }

    #[test]
    fn activity_extends_idle_window() {
        let table = SessionTable::new();
        let issued = table.issue(Channel::Lan, "peer", IDLE, ABS, NOW);
        let mut clock = NOW;
        for _ in 0..8 {
            clock += IDLE * 500;
            assert!(verify(&table, &issued.token, clock).is_some(), "clock={clock}");
        }
    }

    #[test]
    fn absolute_timeout_wins_over_activity() {
        let table = SessionTable::new();
        let issued = table.issue(Channel::Tunnel, "peer", IDLE, ABS, NOW);
        // 一直有活动,但越过绝对上限后必须失效。
        let mut clock = NOW;
        while clock < NOW + ABS * 1000 {
            clock += IDLE * 500;
            let _ = verify(&table, &issued.token, clock);
        }
        assert!(verify(&table, &issued.token, NOW + ABS * 1000).is_none());
    }

    #[test]
    fn unknown_token_is_rejected() {
        let table = SessionTable::new();
        assert!(verify(&table, "nope", NOW).is_none());
    }

    #[test]
    fn revoke_by_channel_keeps_other_channel() {
        let table = SessionTable::new();
        let lan = table.issue(Channel::Lan, "a", IDLE, ABS, NOW);
        let tunnel = table.issue(Channel::Tunnel, "b", IDLE, ABS, NOW);
        assert_eq!(table.revoke(Some(Channel::Tunnel)), 1);
        assert!(
            verify(&table, &tunnel.token, NOW + 1).is_none(),
            "隧道关闭后隧道会话必须立即失效"
        );
        assert!(verify(&table, &lan.token, NOW + 1).is_some());
    }

    #[test]
    fn revoke_all_clears_every_channel() {
        let table = SessionTable::new();
        let lan = table.issue(Channel::Lan, "a", IDLE, ABS, NOW);
        let tunnel = table.issue(Channel::Tunnel, "b", IDLE, ABS, NOW);
        assert_eq!(table.revoke(None), 2);
        assert!(verify(&table, &lan.token, NOW + 1).is_none());
        assert!(verify(&table, &tunnel.token, NOW + 1).is_none());
    }

    #[test]
    fn single_session_revoke_is_scoped() {
        let table = SessionTable::new();
        let a = table.issue(Channel::Lan, "a", IDLE, ABS, NOW);
        let b = table.issue(Channel::Lan, "b", IDLE, ABS, NOW);
        assert!(table.revoke_session(&a.id));
        assert!(!table.revoke_session(&a.id), "重复吊销返回 false");
        assert!(verify(&table, &a.token, NOW + 1).is_none());
        assert!(verify(&table, &b.token, NOW + 1).is_some());
    }

    #[test]
    fn view_counts_by_channel() {
        let table = SessionTable::new();
        table.issue(Channel::Lan, "a", IDLE, ABS, NOW);
        table.issue(Channel::Lan, "b", IDLE, ABS, NOW + 1);
        table.issue(Channel::Tunnel, "c", IDLE, ABS, NOW + 2);
        let view = table.view();
        assert_eq!(view.total, 3);
        assert_eq!(view.lan, 2);
        assert_eq!(view.tunnel, 1);
        assert_eq!(view.items[0].peer, "a", "明细按建立时间排序");
    }

    #[test]
    fn sweep_removes_only_expired() {
        let table = SessionTable::new();
        table.issue(Channel::Lan, "short", 60, 60, NOW);
        table.issue(Channel::Lan, "long", IDLE, ABS, NOW);
        assert_eq!(table.sweep_expired(NOW + 61_000), 1);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn plaintext_token_never_stored() {
        let table = SessionTable::new();
        let issued = table.issue(Channel::Lan, "a", IDLE, ABS, NOW);
        let map = table.inner.lock().unwrap();
        assert!(map.keys().all(|key| key != &issued.token && !key.contains(&issued.token)));
        assert_eq!(map.keys().next().unwrap(), &secret::hash_token(&issued.token));
    }
}
