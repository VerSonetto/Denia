//! `remote` 命名空间:远程连接的配置结构与写入层校验。
//!
//! 校验在这里承担安全职责:隧道侧的几个上限不是"建议值",而是把
//! "临时隧道不得退化成长期入口"这条约束钉死在写入层 —— 越界的值在
//! 保存时就拒绝(fail loud),而不是在运行时悄悄夹紧。夹紧会让用户以为
//! 自己配的值生效了,而实际行为与之不符。

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REMOTE_NS: &str = "remote";

/// 隧道 ticket 有效期上限(秒)。隧道 URL 会出现在公网,票据窗口必须短。
pub const TUNNEL_TICKET_TTL_MAX: u64 = 600;
/// 隧道会话绝对超时上限(秒):24 小时。
pub const TUNNEL_ABSOLUTE_TIMEOUT_MAX: u64 = 24 * 60 * 60;
/// 局域网 ticket 有效期上限(秒):1 小时。
pub const LAN_TICKET_TTL_MAX: u64 = 3600;
/// 会话空闲超时的下限(秒):给不出低于 30 秒的空闲窗口。
pub const IDLE_TIMEOUT_MIN: u64 = 30;
/// 任何会话的绝对超时上限(秒):24 小时。
pub const ABSOLUTE_TIMEOUT_MAX: u64 = 24 * 60 * 60;
/// PIN challenge 的有效期(秒)。扫码到输完 PIN 之间的窗口,不需要更长。
pub const PIN_CHALLENGE_TTL_SECONDS: u64 = 120;

/// `remote` 段整体。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RemoteSettings {
    /// 远程连接功能总开关。关掉后所有远程端点返回 403,已有远程 listener 关闭。
    pub enabled: bool,
    pub lan: LanSettings,
    pub tunnel: TunnelSettings,
    pub rate_limit: RateLimitSettings,
    pub audit: AuditSettings,
    /// 额外允许的 Host(自定义域名等)。本机地址、局域网候选地址与当前隧道
    /// host 由运行时自动纳入白名单,这里只补用户自己的域名。
    pub allowed_hosts: Vec<String>,
}

impl Default for RemoteSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            lan: LanSettings::default(),
            tunnel: TunnelSettings::default(),
            rate_limit: RateLimitSettings::default(),
            audit: AuditSettings::default(),
            allowed_hosts: Vec::new(),
        }
    }
}

/// 局域网连接(低渗透风险,实现从简)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LanSettings {
    /// 监听端口;0 = 由系统分配空闲端口(默认,避免与主 listener 争端口)。
    pub port: u16,
    /// 绑定地址;空 = 0.0.0.0(全部网卡)。也可指定某块网卡 IP。
    pub bind_address: String,
    pub ticket_ttl_seconds: u64,
    /// 局域网 ticket 是否一次性。默认 false:同一 Wi-Fi 下多台设备(手机+平板)
    /// 用同一个二维码都能连,这是局域网"从简"的题中之义。
    pub ticket_single_use: bool,
    pub session_idle_timeout_seconds: u64,
    pub session_absolute_timeout_seconds: u64,
    /// 局域网是否也要求 PIN。默认否(局域网本身已是可信网段的边界)。
    pub require_pin: bool,
}

impl Default for LanSettings {
    fn default() -> Self {
        Self {
            port: 0,
            bind_address: String::new(),
            ticket_ttl_seconds: 600,
            ticket_single_use: false,
            session_idle_timeout_seconds: 1800,
            session_absolute_timeout_seconds: 43200,
            require_pin: false,
        }
    }
}

/// Cloudflare 临时公网隧道(安全优先)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TunnelSettings {
    /// cloudflared 可执行文件路径(默认走 PATH)。
    pub cloudflared_path: String,
    pub ticket_ttl_seconds: u64,
    pub session_idle_timeout_seconds: u64,
    pub session_absolute_timeout_seconds: u64,
    /// 是否要求 6 位 PIN 二次验证。默认开启;关闭只影响"扫码后要不要输 PIN",
    /// 不影响 ticket/会话/HTTPS/Host 校验这些硬约束。
    pub require_pin: bool,
    /// 是否要求请求经 HTTPS 到达。**恒为 true**(校验层拒绝改成 false):
    /// 公网明文回源等于把会话 cookie 交给链路上的任何人。
    pub require_https: bool,
    /// cloudflared 到 Cloudflare 边缘的传输协议(cloudflared `--protocol`)。
    ///
    /// - `""`(自动)/`quic`:走 QUIC(HTTP/3 over UDP)。默认档位,握手 RTT 更少、
    ///   丢包恢复更快,蜂窝网络上通常明显优于 TCP。
    /// - `http2`:回退到 TCP 上的 HTTP/2。部分运营商与办公网对 UDP 限速或直接
    ///   封锁,此时 QUIC 会退化得很难看,这个档位是逃生舱。
    ///
    /// 只做"能连上但更慢"这一类的取舍,不影响安全边界(隧道本身仍是 HTTPS 回源)。
    pub transport_protocol: String,
}

impl Default for TunnelSettings {
    fn default() -> Self {
        Self {
            cloudflared_path: "cloudflared".to_string(),
            ticket_ttl_seconds: 120,
            session_idle_timeout_seconds: 900,
            session_absolute_timeout_seconds: 21600,
            require_pin: true,
            require_https: true,
            // 留空 = 不传 --protocol,cloudflared 自己默认就是 quic。
            transport_protocol: String::new(),
        }
    }
}

/// 限流与防爆破。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct RateLimitSettings {
    /// 连续失败多少次后进入指数退避。
    pub max_failures: u32,
    /// 退避基础延迟(毫秒):第 n 次连续失败延迟 base * 2^(n-1)。
    pub base_delay_ms: u64,
    /// 退避延迟上限(毫秒)。
    pub max_delay_ms: u64,
    /// 滑动窗口内允许的尝试次数上限。
    pub window_max_attempts: u32,
    /// 滑动窗口长度(秒)。
    pub window_seconds: u64,
    /// 单个 PIN challenge 允许的失败次数,超出即作废(必须重新扫码)。
    pub pin_max_attempts: u32,
}

impl Default for RateLimitSettings {
    fn default() -> Self {
        Self {
            max_failures: 5,
            base_delay_ms: 1000,
            max_delay_ms: 300_000,
            window_max_attempts: 20,
            window_seconds: 60,
            pin_max_attempts: 3,
        }
    }
}

/// 审计日志。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AuditSettings {
    pub enabled: bool,
    /// 相对 `~/.denia` 的路径(绝对路径原样使用)。
    pub path: String,
}

impl Default for AuditSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            path: "remote/audit.jsonl".to_string(),
        }
    }
}

/// 隧道传输协议的允许值。空串 = 不传 `--protocol`,用 cloudflared 自己的默认。
///
/// 只列这两个:`quic`(UDP/HTTP3,移动网络上握手更少、丢包恢复更好)与
/// `http2`(TCP 兜底,应对运营商封 UDP)。写成闭集而不是自由文本,是因为
/// cloudflared **对非法取值静默回退到默认值** —— 自由文本会让人以为配好了,
/// 实际跑的是默认档,这种"看起来生效其实没生效"必须挡在写入层。
pub const TUNNEL_PROTOCOLS: [&str; 2] = ["quic", "http2"];

fn transport_protocol_valid(protocol: &str) -> bool {
    let trimmed = protocol.trim();
    trimmed.is_empty() || TUNNEL_PROTOCOLS.contains(&trimmed)
}

/// 写入层校验:类型往返 + 安全上限。
///
/// 拒绝而不是夹紧——用户填了 3600 秒的隧道 ticket TTL,应当看到"太长了",
/// 而不是保存成功却按 600 秒跑。
pub fn validate_remote(value: Value) -> Result<Value, String> {
    let parsed: RemoteSettings = serde_json::from_value(value).map_err(|e| e.to_string())?;

    let t = &parsed.tunnel;
    if t.ticket_ttl_seconds == 0 || t.ticket_ttl_seconds > TUNNEL_TICKET_TTL_MAX {
        return Err(format!(
            "tunnel.ticketTtlSeconds 必须在 1..={TUNNEL_TICKET_TTL_MAX} 之间;公网隧道的一次性票据窗口不允许更长"
        ));
    }
    if !parsed.audit.enabled {
        // 公网隧道必须留痕:没有审计日志就没有事后追溯能力。
        return Err("audit.enabled 不能关闭:公网隧道必须保留连接与失败审计".to_string());
    }
    if parsed.audit.path.trim().is_empty() {
        return Err("audit.path 不能为空".to_string());
    }
    if t.session_absolute_timeout_seconds < IDLE_TIMEOUT_MIN
        || t.session_absolute_timeout_seconds > TUNNEL_ABSOLUTE_TIMEOUT_MAX
    {
        return Err(format!(
            "tunnel.sessionAbsoluteTimeoutSeconds 必须在 {IDLE_TIMEOUT_MIN}..={TUNNEL_ABSOLUTE_TIMEOUT_MAX} 之间"
        ));
    }
    if t.session_idle_timeout_seconds < IDLE_TIMEOUT_MIN
        || t.session_idle_timeout_seconds > t.session_absolute_timeout_seconds
    {
        return Err(format!(
            "tunnel.sessionIdleTimeoutSeconds 必须在 {IDLE_TIMEOUT_MIN}..=sessionAbsoluteTimeoutSeconds 之间"
        ));
    }
    if !t.require_https {
        return Err(
            "tunnel.requireHttps 不能关闭:公网明文回源会把会话 cookie 暴露给链路中间人".to_string(),
        );
    }

    if !transport_protocol_valid(&t.transport_protocol) {
        return Err(format!(
            "tunnel.transportProtocol 只能是 {} 之一(留空 = 用 cloudflared 默认);得到 '{}'",
            TUNNEL_PROTOCOLS.join(" / "),
            t.transport_protocol
        ));
    }

    let l = &parsed.lan;
    if l.ticket_ttl_seconds == 0 || l.ticket_ttl_seconds > LAN_TICKET_TTL_MAX {
        return Err(format!(
            "lan.ticketTtlSeconds 必须在 1..={LAN_TICKET_TTL_MAX} 之间"
        ));
    }
    if l.session_absolute_timeout_seconds < IDLE_TIMEOUT_MIN
        || l.session_absolute_timeout_seconds > ABSOLUTE_TIMEOUT_MAX
    {
        return Err(format!(
            "lan.sessionAbsoluteTimeoutSeconds 必须在 {IDLE_TIMEOUT_MIN}..={ABSOLUTE_TIMEOUT_MAX} 之间"
        ));
    }
    if l.session_idle_timeout_seconds < IDLE_TIMEOUT_MIN
        || l.session_idle_timeout_seconds > l.session_absolute_timeout_seconds
    {
        return Err(format!(
            "lan.sessionIdleTimeoutSeconds 必须在 {IDLE_TIMEOUT_MIN}..=sessionAbsoluteTimeoutSeconds 之间"
        ));
    }
    if !l.bind_address.is_empty() && l.bind_address.parse::<std::net::IpAddr>().is_err() {
        return Err(format!(
            "lan.bindAddress 必须是合法的 IP 地址或留空;得到 '{}'",
            l.bind_address
        ));
    }

    let r = &parsed.rate_limit;
    if r.max_failures == 0 || r.max_failures > 100 {
        return Err("rateLimit.maxFailures 必须在 1..=100 之间".to_string());
    }
    if r.base_delay_ms == 0 || r.base_delay_ms > 60_000 {
        return Err("rateLimit.baseDelayMs 必须在 1..=60000 之间".to_string());
    }
    if r.max_delay_ms < r.base_delay_ms {
        return Err("rateLimit.maxDelayMs 不能小于 baseDelayMs".to_string());
    }
    if r.window_max_attempts == 0 || r.window_seconds == 0 {
        return Err("rateLimit.windowMaxAttempts 与 windowSeconds 都必须大于 0".to_string());
    }
    if r.pin_max_attempts == 0 || r.pin_max_attempts > 10 {
        return Err("rateLimit.pinMaxAttempts 必须在 1..=10 之间".to_string());
    }

    for host in &parsed.allowed_hosts {
        if host.trim().is_empty() || host.contains('/') {
            return Err(format!("allowedHosts 里的 '{host}' 不是合法的 host(不含协议与路径)"));
        }
    }

    serde_json::to_value(parsed).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn defaults_round_trip_and_pass_validation() {
        let value = serde_json::to_value(RemoteSettings::default()).unwrap();
        let checked = validate_remote(value).expect("默认值必须通过校验");
        let parsed: RemoteSettings = serde_json::from_value(checked).unwrap();
        assert!(parsed.enabled);
        assert!(parsed.tunnel.require_pin, "隧道默认必须要求 PIN");
        assert!(parsed.tunnel.require_https, "隧道默认必须要求 HTTPS");
        assert_eq!(parsed.tunnel.ticket_ttl_seconds, 120);
    }

    #[test]
    fn partial_section_fills_defaults() {
        // 前端可能只写一小段;其余字段必须落到默认值而不是解析失败。
        let parsed: RemoteSettings =
            serde_json::from_value(json!({"tunnel": {"ticketTtlSeconds": 300}})).unwrap();
        assert_eq!(parsed.tunnel.ticket_ttl_seconds, 300);
        assert_eq!(parsed.lan.ticket_ttl_seconds, 600);
        assert!(parsed.tunnel.require_pin);
    }

    #[test]
    fn tunnel_ticket_ttl_is_capped_at_write_time() {
        let value = json!({"tunnel": {"ticketTtlSeconds": 3600}});
        let error = validate_remote(value).unwrap_err();
        assert!(error.contains("ticketTtlSeconds"), "错误信息要指出字段:{error}");
    }

    #[test]
    fn tunnel_absolute_timeout_is_capped() {
        let value = json!({"tunnel": {"sessionAbsoluteTimeoutSeconds": 48 * 3600}});
        assert!(validate_remote(value).is_err());
    }

    #[test]
    fn tunnel_https_cannot_be_disabled() {
        let value = json!({"tunnel": {"requireHttps": false}});
        let error = validate_remote(value).unwrap_err();
        assert!(error.contains("requireHttps"), "{error}");
    }

    #[test]
    fn idle_timeout_cannot_exceed_absolute() {
        let value = json!({
            "lan": {"sessionIdleTimeoutSeconds": 7200, "sessionAbsoluteTimeoutSeconds": 3600}
        });
        assert!(validate_remote(value).is_err());
    }

    #[test]
    fn bind_address_must_be_ip_or_empty() {
        assert!(validate_remote(json!({"lan": {"bindAddress": "192.168.1.7"}})).is_ok());
        assert!(validate_remote(json!({"lan": {"bindAddress": ""}})).is_ok());
        let error = validate_remote(json!({"lan": {"bindAddress": "eth0"}})).unwrap_err();
        assert!(error.contains("bindAddress"), "{error}");
    }

    #[test]
    fn allowed_hosts_reject_urls() {
        let error = validate_remote(json!({"allowedHosts": ["https://example.com"]})).unwrap_err();
        assert!(error.contains("allowedHosts"), "{error}");
    }

    #[test]
    fn rate_limit_sanity() {
        assert!(validate_remote(json!({"rateLimit": {"maxFailures": 0}})).is_err());
        assert!(validate_remote(json!({"rateLimit": {"pinMaxAttempts": 99}})).is_err());
        assert!(
            validate_remote(json!({"rateLimit": {"baseDelayMs": 5000, "maxDelayMs": 1000}}))
                .is_err()
        );
    }

    #[test]
    fn audit_cannot_be_disabled() {
        let error = validate_remote(json!({"audit": {"enabled": false}})).unwrap_err();
        assert!(error.contains("audit.enabled"), "{error}");
        assert!(validate_remote(json!({"audit": {"path": "  "}})).is_err());
    }
}
