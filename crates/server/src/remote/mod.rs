//! 远程连接中枢:局域网直连与 Cloudflare 临时隧道的统一入口。
//!
//! ## 形态
//!
//! 主 listener(`127.0.0.1:3600`)保持原样 —— 本机浏览器访问不引入任何
//! 鉴权负担。开启远程连接时,用**同一个 Router** 另起一个 listener:
//!
//! - 局域网模式:绑 `0.0.0.0:<port>`(或用户指定的网卡),局域网设备直连;
//! - 隧道模式:若局域网 listener 没开,则只绑 `127.0.0.1:<port>`(公网入口
//!   只在隧道上,不为只想要隧道的用户多开一个网卡监听),cloudflared 回源到它。
//!
//! 关闭连接 = 取消该 listener 任务并等待退出,端口随之释放。
//!
//! ## 安全模型(摘要)
//!
//! 远程 listener 上挂一道「远程门」(见 [`guard`]),它按请求来源分三类:
//!
//! | 来源 | 判定 | 处理 |
//! |---|---|---|
//! | `Local` | peer 是回环且不带 Cloudflare 回源头 | 直通(桌面 UI 自己) |
//! | `Tunnel` | peer 是回环且带 `cf-ray` | 强制 https + Host 白名单 + 票据/会话 |
//! | `Lan` | peer 非回环 | Host 白名单 + 票据/会话 |
//!
//! 判定顺序有意如此:只有 cloudflared 会从回环发起带 CF 头的请求,所以
//! "回环 + CF 头"是隧道流量的可靠指纹;局域网客户端即使伪造 `cf-ray`,
//! 也只会把自己判成隧道流量而被 https 校验拦下,拿不到直通。

pub mod audit;
pub mod config;
pub mod guard;
pub mod net;
pub mod pin;
pub mod qr;
pub mod ratelimit;
pub mod secret;
pub mod session;
pub mod ticket;
pub mod tunnel;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Router;
use denia_settings::SettingsStore;
use serde::Serialize;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::state::ServerEvent;
use audit::{AuditLog, AuditRecord};
use config::{PIN_CHALLENGE_TTL_SECONDS, RemoteSettings, TunnelSettings};
use qr::Rendered;
use ratelimit::{Admission, RateLimiter};
use session::{SessionRecord, SessionTable, SessionView};
use ticket::{TicketError, TicketTable};
use tunnel::TunnelManager;

/// 会话 Cookie 名。
pub const SESSION_COOKIE: &str = "denia_remote_session";

/// 连接通道。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    Lan,
    Tunnel,
}

impl Channel {
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Lan => "lan",
            Channel::Tunnel => "tunnel",
        }
    }
}

/// 一次请求的来源判定(与 [`Channel`] 不同:它还包含"本机直通")。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    /// 本机浏览器直连(桌面 UI)。
    Local,
    Lan,
    Tunnel,
}

impl Via {
    pub fn as_str(self) -> &'static str {
        match self {
            Via::Local => "local",
            Via::Lan => "lan",
            Via::Tunnel => "tunnel",
        }
    }

    pub fn channel(self) -> Option<Channel> {
        match self {
            Via::Local => None,
            Via::Lan => Some(Channel::Lan),
            Via::Tunnel => Some(Channel::Tunnel),
        }
    }
}

/// 票据兑换的结果。
#[derive(Debug, Clone)]
pub enum ExchangeOutcome {
    /// 需要先过 PIN。
    PinRequired { challenge: String, attempts: u32 },
    /// 直接发放会话令牌。`via` 是票据所属通道(不是请求来源)—— cookie
    /// 的 `Secure` 属性与超时参数都要按它决定。
    Session { token: String, via: Channel },
}

/// 一次失败:HTTP 状态、稳定错误码、可读信息,以及可选的 `Retry-After`。
///
/// 限流失败必须是 429 + `Retry-After` 而不是 401:前者是标准语义,客户端
/// 与中间层能据此自动退避;把它混进 401 会让"票据错了"和"你被限流了"
/// 在调用方看起来一模一样。
#[derive(Debug, Clone)]
pub struct RemoteFailure {
    pub status: axum::http::StatusCode,
    pub code: String,
    pub message: String,
    pub retry_after_seconds: Option<u64>,
}

impl RemoteFailure {
    /// 票据/PIN 类失败:401。
    fn unauthorized(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            status: axum::http::StatusCode::UNAUTHORIZED,
            code: code.into(),
            message: message.into(),
            retry_after_seconds: None,
        }
    }

    /// 限流失败:429 + 重试等待秒数。
    fn limited(code: impl Into<String>, retry_after_seconds: u64) -> Self {
        Self {
            status: axum::http::StatusCode::TOO_MANY_REQUESTS,
            code: code.into(),
            message: format!("尝试过于频繁,请 {retry_after_seconds} 秒后再试"),
            retry_after_seconds: Some(retry_after_seconds),
        }
    }
}

/// 局域网连接的状态快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LanStatus {
    /// 实际绑定地址(可能是 0.0.0.0)。
    pub bind: String,
    pub port: u16,
    /// 候选网卡地址(供用户切换访问地址)。
    pub candidates: Vec<CandidateView>,
    /// 当前用于生成链接的地址。
    pub address: String,
    pub link: LinkView,
}

/// 网卡候选(给 UI 的下拉框)。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateView {
    pub interface: String,
    pub address: String,
    pub recommended: bool,
}

/// 隧道连接的状态快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelStatus {
    pub url: String,
    pub host: String,
    pub pid: u32,
    pub started_at: u64,
    pub link: LinkView,
}

/// 一条可分享的访问链接及其二维码。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkView {
    /// 不带票据的基址(展示用;直接访问会被远程门拦下)。
    pub url: String,
    /// 带票据的完整链接 —— 扫码与分享用这一个。
    pub ticket_url: String,
    pub ticket_expires_at: u64,
    pub ticket_single_use: bool,
    pub require_pin: bool,
    /// 仅本机请求可见:本机 UI 要能看到 PIN 才能转告用户。
    pub pin: Option<String>,
    pub qr_svg: String,
    pub qr_text: String,
    pub qr_width: usize,
    pub qr_modules: Vec<bool>,
}

/// 整体状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    pub enabled: bool,
    pub lan: Option<LanStatus>,
    pub tunnel: Option<TunnelStatus>,
    pub sessions: SessionView,
    pub audit_path: String,
    /// 当前处于退避中的来源 IP 数(状态展示用)。
    pub blocked_peers: usize,
}

/// 不参与并发的会话外状态。
#[derive(Default)]
struct Inner {
    /// 当前隧道 PIN(仅内存,关闭即清)。
    tunnel_pin: Option<String>,
    /// 当前局域网 PIN(局域网开了二次验证时才有)。
    lan_pin: Option<String>,
    /// 局域网模式下用户选中的访问地址(用于生成链接)。
    lan_address: Option<String>,
    /// 最近一次签发的带票据链接,**按通道分开存**。
    ///
    /// 不能共用一个槽:局域网与隧道可以同时开着,共用会让局域网卡片显示
    /// 隧道的链接与二维码(用户扫了会连到公网入口,与卡片描述完全不符)。
    /// 票据表里存的是哈希,这里留明文只为让状态查询能重复展示同一个二维码。
    lan_ticket: Option<IssuedTicket>,
    tunnel_ticket: Option<IssuedTicket>,
}

/// 一张已签发票据的展示信息。
#[derive(Debug, Clone)]
struct IssuedTicket {
    url: String,
    expires_at: u64,
}

impl Inner {
    fn ticket_for(&self, via: Channel) -> Option<&IssuedTicket> {
        match via {
            Channel::Lan => self.lan_ticket.as_ref(),
            Channel::Tunnel => self.tunnel_ticket.as_ref(),
        }
    }

    fn set_ticket(&mut self, via: Channel, ticket: Option<IssuedTicket>) {
        match via {
            Channel::Lan => self.lan_ticket = ticket,
            Channel::Tunnel => self.tunnel_ticket = ticket,
        }
    }
}

/// 一个正在运行的远程 listener。
struct ListenerHandle {
    bind: SocketAddr,
    /// 是否局域网模式(绑到非回环地址)。隧道专用的回环 listener 为 false。
    lan: bool,
    /// 这个 listener 是否同时充当隧道的回源目标。
    ///
    /// 隧道与局域网要同时可用时,只能有一个 listener —— 先开隧道再开局域网,
    /// 就是把回环 listener 换成绑局域网地址的 listener(沿用同一端口,因为
    /// cloudflared 已经按端口回源)。这个标记记住"它还得继续伺候隧道",
    /// 好在局域网关掉时把回环 listener 放回去。
    tunnel_origin: bool,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

/// 关闭 listener 的宽限期。
///
/// 优雅停机只对"正在传输中"的请求有意义;而 HTTP 客户端(浏览器、手机
/// Safari)会长期保持 keep-alive 空闲连接 —— 那种连接永远等不到自然结束,
/// 只等它就会让"关闭连接"和 Ctrl+C 一起卡死。所以给一个宽限期,到点直接
/// abort:abort 会 drop 掉 `axum::serve` 的 future,listener 随之释放端口。
///
/// 宽限期必须**短**:用户的启停操作全部串在这条路径上(开隧道会先关旧
/// listener、关局域网要重建回源 listener),3 秒的等待会让每次点击都像
/// 卡死。keep-alive 空闲连接本来就不值得等 —— 正在传输的请求只需毫秒级
/// 收尾,所以 400ms 足够区分"在传"与"空转"。
const LISTENER_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

/// 停掉一个 listener 并**确认端口已释放**。
async fn shutdown_listener(handle: ListenerHandle) {
    handle.cancel.cancel();
    let mut task = handle.task;
    match tokio::time::timeout(LISTENER_SHUTDOWN_GRACE, &mut task).await {
        // 已在宽限期内自然收尾:任务已经完成,不能再 poll 一次。
        Ok(_) => {}
        Err(_) => {
            // 有 keep-alive 空闲连接挂着是常态,不是异常 —— 不记 warn,
            // 免得每次关闭连接都在日志里刷一条吓人的告警。
            tracing::debug!(
                grace_ms = LISTENER_SHUTDOWN_GRACE.as_millis(),
                "remote listener still had open connections; aborting to release the port"
            );
            task.abort();
            // abort 是异步的:必须等任务真正结束,否则端口可能还没释放就返回,
            // 调用方紧接着的"重新绑定同一端口"会失败。
            let _ = task.await;
        }
    }
}

/// 远程连接中枢。整个进程一个实例,挂在 `AppState` 上。
pub struct RemoteManager {
    home: PathBuf,
    settings: Arc<SettingsStore>,
    events: broadcast::Sender<ServerEvent>,
    tickets: TicketTable,
    sessions: SessionTable,
    challenges: pin::ChallengeTable,
    limiter: RateLimiter,
    audit: Mutex<AuditLog>,
    tunnel: TunnelManager,
    /// 远程 listener 的 Router 与监听句柄。
    listener: Mutex<Option<ListenerHandle>>,
    router: Mutex<Option<Router>>,
    inner: Mutex<Inner>,
}

impl RemoteManager {
    pub fn new(
        home: PathBuf,
        settings: Arc<SettingsStore>,
        events: broadcast::Sender<ServerEvent>,
    ) -> Self {
        let audit = AuditLog::new(&home, &config::AuditSettings::default().path);
        Self {
            home,
            settings,
            events,
            tickets: TicketTable::new(),
            sessions: SessionTable::new(),
            challenges: pin::ChallengeTable::new(),
            limiter: RateLimiter::new(config::RateLimitSettings::default()),
            audit: Mutex::new(audit),
            tunnel: TunnelManager::new(),
            listener: Mutex::new(None),
            router: Mutex::new(None),
            inner: Mutex::new(Inner::default()),
        }
    }

    /* ---- 配置 ---- */

    /// 当前生效的 `remote` 配置(解析失败回落默认值,并告警)。
    pub fn config(&self) -> RemoteSettings {
        match self.settings.resolved(config::REMOTE_NS) {
            Ok(value) => serde_json::from_value(value).unwrap_or_else(|error| {
                tracing::warn!(%error, "remote settings unreadable; using defaults");
                RemoteSettings::default()
            }),
            Err(_) => RemoteSettings::default(),
        }
    }

    /// 配置变更后刷新依赖配置的部件(限流阈值、审计路径)。
    pub fn apply_settings(&self) {
        let settings = self.config();
        self.limiter.update_settings(settings.rate_limit.clone());
        let wanted = self.audit_target(&settings);
        let mut audit = self.audit.lock().unwrap_or_else(|p| p.into_inner());
        if audit.path() != wanted.as_deref() {
            *audit = match &wanted {
                Some(path) => AuditLog::new(&self.home, &path.to_string_lossy()),
                None => AuditLog::disabled(),
            };
        }
    }

    /// 审计日志目标路径;配置关闭时 None。
    fn audit_target(&self, settings: &RemoteSettings) -> Option<PathBuf> {
        if !settings.audit.enabled {
            return None;
        }
        let raw = settings.audit.path.trim();
        if raw.is_empty() {
            return None;
        }
        let candidate = PathBuf::from(raw);
        Some(if candidate.is_absolute() {
            candidate
        } else {
            self.home.join(candidate)
        })
    }

    fn audit(&self, record: AuditRecord) {
        self.audit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .write(&record);
    }

    fn audit_path(&self) -> String {
        self.audit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .path()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    }

    /// 隧道 pid 文件路径(异常退出后清理残留用)。
    fn pid_file(&self) -> PathBuf {
        self.home.join("remote").join("tunnel.pid")
    }

    /// 启动时调用:清理上次异常退出留下的隧道进程。
    pub fn cleanup_stale_tunnel(&self) {
        tunnel::cleanup_stale(&self.pid_file());
    }

    /* ---- 装配 ---- */

    /// 注入远程 listener 要用的 Router。必须在任何 `start_*` 之前调用。
    pub fn attach_router(&self, router: Router) {
        *self.router.lock().unwrap_or_else(|p| p.into_inner()) = Some(router);
    }

    /// 是否有正在运行的远程 listener(局域网或隧道回源)。
    pub fn listener_active(&self) -> bool {
        self.listener
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /* ---- 状态 ---- */

    /// 组装状态快照。`include_secrets` 只对**本机**请求为 true:PIN 与
    /// 带票据链接绝不能回给远程客户端(那等于把凭据发给被验证方)。
    pub async fn status(&self, include_secrets: bool) -> RemoteStatus {
        let settings = self.config();
        let (lan_bind, lan_address, lan_ticket, tunnel_ticket, tunnel_pin, lan_pin) = {
            let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            let listener = self.listener.lock().unwrap_or_else(|p| p.into_inner());
            (
                listener
                    .as_ref()
                    .filter(|handle| handle.lan)
                    .map(|handle| handle.bind),
                inner.lan_address.clone(),
                // 带票据的链接本身就是入场券:只回给本机。
                include_secrets.then(|| inner.lan_ticket.clone()).flatten(),
                include_secrets.then(|| inner.tunnel_ticket.clone()).flatten(),
                inner.tunnel_pin.clone(),
                inner.lan_pin.clone(),
            )
        };
        let tunnel_info = self.tunnel.info().await;

        let lan = lan_bind.map(|bind| {
            let address = lan_address
                .clone()
                .or_else(|| net::preferred(&net::candidates()).map(|ip| ip.to_string()))
                .unwrap_or_else(|| "127.0.0.1".to_string());
            let base = format!("http://{address}:{}", bind.port());
            LanStatus {
                bind: bind.to_string(),
                port: bind.port(),
                candidates: candidate_views(),
                address,
                link: self.link_view(
                    &base,
                    Channel::Lan,
                    include_secrets.then(|| lan_pin.clone()).flatten(),
                    lan_ticket,
                ),
            }
        });

        let tunnel = tunnel_info.map(|info| TunnelStatus {
            url: info.url.clone(),
            host: info.host.clone(),
            pid: info.pid,
            started_at: info.started_at,
            link: self.link_view(
                &info.url,
                Channel::Tunnel,
                include_secrets.then(|| tunnel_pin.clone()).flatten(),
                tunnel_ticket,
            ),
        });

        RemoteStatus {
            enabled: settings.enabled,
            lan,
            tunnel,
            sessions: self.sessions.view(),
            audit_path: self.audit_path(),
            blocked_peers: self.blocked_peers(),
        }
    }

    /// 组一条链接的完整视图(含二维码)。
    fn link_view(
        &self,
        base: &str,
        via: Channel,
        pin: Option<String>,
        ticket: Option<IssuedTicket>,
    ) -> LinkView {
        let settings = self.config();
        let (single_use, require_pin) = match via {
            Channel::Lan => (settings.lan.ticket_single_use, settings.lan.require_pin),
            Channel::Tunnel => (true, settings.tunnel.require_pin),
        };
        let (ticket_url, expires_at) = match ticket {
            Some(ticket) => (ticket.url, ticket.expires_at),
            // 没有可用票据(刚刷新过状态、票据已用尽)时退回基址:
            // 二维码扫出来的链接会被远程门拦下并提示重新生成,不会误导用户。
            None => (base.to_string(), 0),
        };
        let rendered = qr::render(&ticket_url).unwrap_or_else(|error| {
            tracing::warn!(%error, "could not render QR for remote link");
            Rendered {
                svg: String::new(),
                ascii: String::new(),
                width: 0,
                modules: Vec::new(),
            }
        });
        LinkView {
            url: base.to_string(),
            ticket_url,
            ticket_expires_at: expires_at,
            ticket_single_use: single_use,
            require_pin,
            pin,
            qr_svg: rendered.svg,
            qr_text: rendered.ascii,
            qr_width: rendered.width,
            qr_modules: rendered.modules,
        }
    }

    /* ---- 局域网 ---- */

    /// 开启局域网连接。`address` 为选定的访问地址(缺省取推荐项)。
    pub async fn start_lan(&self, address: Option<String>) -> Result<LanStatus, String> {
        let settings = self.config();
        if !settings.enabled {
            return Err("远程连接功能已在设置中关闭".to_string());
        }
        let requested = address.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let bind_ip = match requested {
            Some(raw) => raw
                .parse::<std::net::IpAddr>()
                .map_err(|_| format!("不是合法的 IP 地址:{raw}"))?,
            None => match settings.lan.bind_address.trim() {
                "" => std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                raw => raw
                    .parse()
                    .map_err(|_| format!("配置里的 bindAddress 不是合法 IP:{raw}"))?,
            },
        };

        // 重开 = 先关:端口、票据、会话一起作废,不留上一轮的残留入口。
        self.stop_lan().await;

        // 隧道可能已经开着,而它的回源 listener 只绑在回环上。要同时提供
        // 局域网访问,得把这个 listener 换成绑局域网地址的 —— 但**必须沿用
        // 同一个端口**:cloudflared 已经按端口回源,换端口会让隧道指向一个
        // 没人监听的地址(表现为"隧道还在,但打不开")。
        let tunnel_port = self.take_loopback_listener_port().await;
        let port = tunnel_port.unwrap_or(settings.lan.port);

        let actual = match self
            .spawn_listener(SocketAddr::new(bind_ip, port), true, tunnel_port.is_some())
            .await
        {
            Ok(actual) => actual,
            Err(error) if tunnel_port.is_some() => {
                // 局域网地址绑不上(端口被占/权限不足)时把回环 listener 放回去,
                // 别让一次失败的局域网启动顺手把已经在跑的隧道打死。
                let restore = self
                    .spawn_listener(
                        SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port),
                        false,
                        true,
                    )
                    .await;
                return Err(match restore {
                    Ok(_) => format!("{error};已恢复隧道回源监听,隧道不受影响"),
                    Err(restore_error) => {
                        format!("{error};且隧道回源监听未能恢复({restore_error}),请重开隧道")
                    }
                });
            }
            Err(error) => return Err(error),
        };

        // 链接用哪个地址:显式指定的优先;否则选一块可用网卡。
        let chosen = match requested {
            Some(raw) => raw.to_string(),
            None if !bind_ip.is_unspecified() => bind_ip.to_string(),
            None => net::preferred(&net::candidates())
                .map(|ip| ip.to_string())
                .ok_or_else(|| {
                    "没有探测到可用的局域网地址;请在设置里显式指定绑定地址".to_string()
                })?,
        };

        let base = format!("http://{chosen}:{}", actual.port());
        let pin = settings.lan.require_pin.then(secret::new_pin);
        let ticket_url = self.issue_ticket(Channel::Lan, &base, pin.clone());
        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            inner.lan_address = Some(chosen.clone());
        }
        self.publish();
        self.audit(
            AuditRecord::new("lan-start", "local", "lan", "ok")
                .detail(format!("bind {actual}, address {chosen}")),
        );

        println!();
        println!("  局域网连接已开启");
        println!("    地址:  {base}");
        println!("    链接:  {ticket_url}");
        if let Some(pin) = &pin {
            println!("    PIN:   {pin}");
        }
        if let Ok(block) = qr::terminal_block(&ticket_url) {
            print!("{block}");
        }
        println!();

        self.lan_status()
            .await
            .ok_or_else(|| "局域网连接启动后状态异常".to_string())
    }

    /// 取出隧道专用的回环 listener,返回它绑的端口。
    ///
    /// 只对"隧道专用"(非局域网)的 listener 生效;真正的局域网 listener
    /// 不动它 —— 那是用户已经开好的连接。
    async fn take_loopback_listener_port(&self) -> Option<u16> {
        let handle = {
            let mut listener = self.listener.lock().unwrap_or_else(|p| p.into_inner());
            match listener.as_ref() {
                Some(handle) if !handle.lan => listener.take(),
                _ => None,
            }
        };
        let handle = handle?;
        let port = handle.bind.port();
        shutdown_listener(handle).await;
        Some(port)
    }

    /// 关闭局域网连接:释放端口、吊销局域网票据与会话。
    pub async fn stop_lan(&self) -> bool {
        let handle = {
            let mut listener = self.listener.lock().unwrap_or_else(|p| p.into_inner());
            match listener.as_ref() {
                Some(handle) if handle.lan => listener.take(),
                _ => None,
            }
        };
        let Some(handle) = handle else {
            return false;
        };
        let port = handle.bind.port();
        // 这个 listener 是否还在替隧道回源 —— 关局域网时据此决定要不要把
        // 回环 listener 放回去。
        let served_tunnel = handle.tunnel_origin;
        shutdown_listener(handle).await;

        // 隧道还开着的话,它的回源目标刚刚被这个 listener 占着 —— 必须把
        // 回环 listener 重新绑回同一个端口,否则隧道会指向一个没人监听的
        // 地址(隧道看起来还在,实际打不开)。
        if served_tunnel
            && let Err(error) = self
                .spawn_listener(
                    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port),
                    false,
                    true,
                )
                .await
        {
            tracing::error!(
                %error,
                port,
                "could not restore tunnel origin listener after stopping LAN"
            );
        }

        self.tickets.revoke(Some(Channel::Lan));
        let revoked = self.sessions.revoke(Some(Channel::Lan));
        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            inner.lan_address = None;
            inner.lan_pin = None;
            inner.lan_ticket = None;
        }
        self.audit(
            AuditRecord::new("lan-stop", "local", "lan", "ok")
                .detail(format!("revoked {revoked} sessions")),
        );
        self.publish();
        true
    }

    /* ---- 隧道 ---- */

    /// 开启 Cloudflare 临时隧道。
    pub async fn start_tunnel(&self) -> Result<TunnelStatus, String> {
        let settings = self.config();
        if !settings.enabled {
            return Err("远程连接功能已在设置中关闭".to_string());
        }
        if self.tunnel.info().await.is_some() {
            return Err("隧道已在运行;请先关闭再重开".to_string());
        }

        // 隧道需要一个回源 listener:局域网已开就复用它,否则只绑回环 ——
        // 只想要公网入口的用户不该被动多开一个网卡监听。
        let existing = {
            let listener = self.listener.lock().unwrap_or_else(|p| p.into_inner());
            listener.as_ref().map(|handle| handle.bind.port())
        };
        let port = match existing {
            Some(port) => {
                // 复用已有的 listener 时把它标成"兼作隧道回源":关掉局域网
                // 时需要按这个标记决定是否重建回环监听。
                let mut slot = self.listener.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(handle) = slot.as_mut() {
                    handle.tunnel_origin = true;
                }
                port
            }
            None => {
                self.spawn_listener(
                    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0),
                    false,
                    true,
                )
                .await?
                .port()
            }
        };
        let origin = format!("http://127.0.0.1:{port}");

        let info = self
            .tunnel
            .start(&settings.tunnel.cloudflared_path, &origin, &self.pid_file())
            .await
            .inspect_err(|error| {
                self.audit(
                    AuditRecord::new("tunnel-start", "local", "tunnel", "denied")
                        .detail(error.clone()),
                );
            })?;

        let pin = settings.tunnel.require_pin.then(secret::new_pin);
        let ticket_url = self.issue_tunnel_ticket(&info.url, pin.clone(), &settings.tunnel);

        self.audit(
            AuditRecord::new("tunnel-start", "local", "tunnel", "ok")
                .detail(format!("{} (pid {})", info.url, info.pid)),
        );
        self.publish();

        println!();
        println!("  ⚠  公网隧道已开启 —— 关闭前 denia 可从公网访问");
        println!("    地址:  {}", info.url);
        println!("    链接:  {ticket_url}");
        match &pin {
            Some(pin) => println!("    PIN:   {pin}(扫码后需输入)"),
            None => println!("    PIN:   未启用(设置里可开启二次验证)"),
        }
        println!("    关闭:  控制台顶部「关闭隧道」,或 Ctrl+C 结束进程");
        if let Ok(block) = qr::terminal_block(&ticket_url) {
            print!("{block}");
        }
        println!();

        self.tunnel_status().await
    }

    /// 关闭隧道:杀进程、清票据、吊销全部隧道会话与 PIN 挑战。
    pub async fn stop_tunnel(&self, reason: &str) -> bool {
        let stopped = self.tunnel.stop(&self.pid_file()).await;
        let orphan_listener = {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            inner.tunnel_pin = None;
            inner.tunnel_ticket = None;
            let mut listener = self.listener.lock().unwrap_or_else(|p| p.into_inner());
            match listener.as_ref() {
                // 隧道专用的回环 listener 一并关掉,不留无人使用的监听。
                Some(handle) if !handle.lan => listener.take(),
                _ => None,
            }
        };
        if let Some(handle) = orphan_listener {
            shutdown_listener(handle).await;
        }

        self.tickets.revoke(Some(Channel::Tunnel));
        let revoked = self.sessions.revoke(Some(Channel::Tunnel));
        let challenges = self.challenges.revoke_all();
        self.audit(AuditRecord::new("tunnel-stop", "local", "tunnel", "ok").detail(format!(
            "reason: {reason}; revoked {revoked} sessions, {challenges} challenges"
        )));
        self.publish();
        stopped.is_some()
    }

    /* ---- 断开全部 ---- */

    /// 「立即关闭隧道并吊销全部会话」入口。
    pub async fn disconnect_all(&self, reason: &str) -> usize {
        self.stop_tunnel(reason).await;
        self.stop_lan().await;
        self.tickets.revoke(None);
        let revoked = self.sessions.revoke(None);
        self.challenges.revoke_all();
        self.limiter.reset();
        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            inner.tunnel_pin = None;
            inner.lan_pin = None;
            inner.lan_ticket = None;
            inner.tunnel_ticket = None;
        }
        self.audit(
            AuditRecord::new("disconnect-all", "local", "local", "ok")
                .detail(format!("reason: {reason}; revoked {revoked} sessions")),
        );
        self.publish();
        revoked
    }

    /// 进程退出时的兜底:杀隧道、清票据与会话、关 listener。
    pub async fn shutdown(&self) {
        let _ = self.stop_tunnel("server-shutdown").await;
        let _ = self.stop_lan().await;
        self.tickets.revoke(None);
        self.sessions.revoke(None);
        self.challenges.revoke_all();
        self.tunnel.shutdown(&self.pid_file()).await;
    }

    /* ---- 票据与兑换 ---- */

    /// 签发一张票据并记住明文链接(供状态查询重复展示)。
    fn issue_ticket(&self, via: Channel, base: &str, pin: Option<String>) -> String {
        let settings = self.config();
        let (ttl, single_use, require_pin) = match via {
            Channel::Lan => (
                settings.lan.ticket_ttl_seconds,
                settings.lan.ticket_single_use,
                settings.lan.require_pin,
            ),
            Channel::Tunnel => (
                settings.tunnel.ticket_ttl_seconds,
                true,
                settings.tunnel.require_pin,
            ),
        };
        let token = self
            .tickets
            .issue(via, ttl, single_use, require_pin, audit::now_millis());
        let url = format!("{base}/?ticket={token}");
        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            inner.set_ticket(
                via,
                Some(IssuedTicket {
                    url: url.clone(),
                    expires_at: audit::now_millis() + ttl * 1000,
                }),
            );
            match via {
                Channel::Lan => inner.lan_pin = pin,
                Channel::Tunnel => inner.tunnel_pin = pin,
            }
        }
        self.audit(
            AuditRecord::new("ticket-issued", "local", via.as_str(), "ok")
                .detail(format!("ttl {ttl}s, singleUse {single_use}")),
        );
        url
    }

    /// 隧道票据:构造时即钉死一次性,配置只影响 TTL。
    fn issue_tunnel_ticket(
        &self,
        base: &str,
        pin: Option<String>,
        settings: &TunnelSettings,
    ) -> String {
        let token = self.tickets.issue(
            Channel::Tunnel,
            settings.ticket_ttl_seconds,
            true,
            settings.require_pin,
            audit::now_millis(),
        );
        let url = format!("{base}/?ticket={token}");
        {
            let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
            inner.tunnel_pin = pin;
            inner.set_ticket(
                Channel::Tunnel,
                Some(IssuedTicket {
                    url: url.clone(),
                    expires_at: audit::now_millis() + settings.ticket_ttl_seconds * 1000,
                }),
            );
        }
        self.audit(
            AuditRecord::new("ticket-issued", "local", "tunnel", "ok").detail(format!(
                "ttl {}s, singleUse true",
                settings.ticket_ttl_seconds
            )),
        );
        url
    }

    /// 重新签发当前连接的票据(旧链接不失效,各自到期为止)。
    pub async fn refresh_ticket(&self) -> Result<String, String> {
        let tunnel = self.tunnel.info().await;
        let (base, via, pin) = if let Some(info) = tunnel {
            let pin = self.current_pin(Channel::Tunnel);
            (info.url, Channel::Tunnel, pin)
        } else {
            let (bind, address) = {
                let listener = self.listener.lock().unwrap_or_else(|p| p.into_inner());
                let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
                match listener.as_ref().filter(|handle| handle.lan) {
                    Some(handle) => (handle.bind, inner.lan_address.clone()),
                    None => return Err("当前没有开启的远程连接".to_string()),
                }
            };
            let address = address
                .or_else(|| net::preferred(&net::candidates()).map(|ip| ip.to_string()))
                .ok_or_else(|| "没有可用的局域网地址".to_string())?;
            (
                format!("http://{address}:{}", bind.port()),
                Channel::Lan,
                self.current_pin(Channel::Lan),
            )
        };
        Ok(self.issue_ticket(via, &base, pin))
    }

    /// 兑换票据。
    pub fn exchange(&self, token: &str, peer: &str) -> Result<ExchangeOutcome, RemoteFailure> {
        let now = audit::now_millis();
        if let Admission::Deny {
            retry_after_seconds,
            reason,
        } = self.limiter.admit(peer, now)
        {
            self.audit(
                AuditRecord::new("exchange", peer, "unknown", "limited")
                    .detail(format!("{} (retry after {retry_after_seconds}s)", reason.code())),
            );
            return Err(RemoteFailure::limited(reason.code(), retry_after_seconds));
        }
        match self.tickets.redeem(token, now) {
            Ok(redeemed) => {
                self.limiter.record_success(peer);
                let settings = self.config();
                if redeemed.pin_required {
                    let pin_value = self.current_pin(redeemed.via).unwrap_or_default();
                    let challenge = self.challenges.open(
                        &redeemed.id,
                        redeemed.via,
                        peer,
                        &pin_value,
                        PIN_CHALLENGE_TTL_SECONDS,
                        settings.rate_limit.pin_max_attempts,
                        now,
                    );
                    self.audit(
                        AuditRecord::new("exchange", peer, redeemed.via.as_str(), "ok")
                            .detail("pin required"),
                    );
                    return Ok(ExchangeOutcome::PinRequired {
                        challenge,
                        attempts: settings.rate_limit.pin_max_attempts,
                    });
                }
                let session = self.open_session(redeemed.via, peer);
                self.audit(
                    AuditRecord::new("exchange", peer, redeemed.via.as_str(), "ok")
                        .detail("session issued"),
                );
                Ok(ExchangeOutcome::Session {
                    token: session.token,
                    via: redeemed.via,
                })
            }
            Err(error) => {
                self.limiter.record_failure(peer, now);
                self.audit(AuditRecord::new(
                    "exchange",
                    peer,
                    "unknown",
                    if error == TicketError::Expired {
                        "expired"
                    } else {
                        "denied"
                    },
                ));
                Err(RemoteFailure::unauthorized(error.code(), error.message()))
            }
        }
    }

    /// 校验 PIN,通过则发放会话令牌。
    pub fn verify_pin(
        &self,
        challenge: &str,
        pin: &str,
        peer: &str,
    ) -> Result<session::Issued, RemoteFailure> {
        let now = audit::now_millis();
        if let Admission::Deny {
            retry_after_seconds,
            reason,
        } = self.limiter.admit(peer, now)
        {
            self.audit(
                AuditRecord::new("pin-verify", peer, "unknown", "limited")
                    .detail(format!("{} (retry after {retry_after_seconds}s)", reason.code())),
            );
            return Err(RemoteFailure::limited(reason.code(), retry_after_seconds));
        }
        let via = self
            .challenges
            .channel_of(challenge)
            .unwrap_or(Channel::Tunnel);
        match self.challenges.verify(challenge, peer, pin, now) {
            pin::VerifyOutcome::Ok => {
                self.limiter.record_success(peer);
                let session = self.open_session(via, peer);
                self.audit(AuditRecord::new("pin-verify", peer, via.as_str(), "ok"));
                Ok(session)
            }
            pin::VerifyOutcome::Mismatch { attempts_left } => {
                self.limiter.record_failure(peer, now);
                self.audit(
                    AuditRecord::new("pin-verify", peer, via.as_str(), "denied")
                        .detail(format!("wrong pin; {attempts_left} attempts left")),
                );
                if attempts_left == 0 {
                    Err(RemoteFailure::unauthorized(
                        "remote/pin-exhausted",
                        "PIN 错误次数过多,请重新扫码获取新链接",
                    ))
                } else {
                    Err(RemoteFailure::unauthorized(
                        "remote/pin-invalid",
                        format!("PIN 不正确,还可尝试 {attempts_left} 次"),
                    ))
                }
            }
            pin::VerifyOutcome::PeerMismatch => {
                self.limiter.record_failure(peer, now);
                self.audit(
                    AuditRecord::new("pin-verify", peer, via.as_str(), "denied")
                        .detail("challenge issued to another peer"),
                );
                Err(RemoteFailure::unauthorized(
                    "remote/pin-invalid",
                    "校验信息与发起扫码的设备不一致,请重新扫码",
                ))
            }
            pin::VerifyOutcome::Unknown => {
                self.limiter.record_failure(peer, now);
                self.audit(AuditRecord::new("pin-verify", peer, via.as_str(), "expired"));
                Err(RemoteFailure::unauthorized(
                    "remote/challenge-expired",
                    "校验已过期,请重新扫码",
                ))
            }
        }
    }

    fn current_pin(&self, via: Channel) -> Option<String> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match via {
            Channel::Tunnel => inner.tunnel_pin.clone(),
            Channel::Lan => inner.lan_pin.clone(),
        }
    }

    /// 建立会话并返回明文令牌(只此一次)。
    fn open_session(&self, via: Channel, peer: &str) -> session::Issued {
        let settings = self.config();
        let (idle, absolute) = match via {
            Channel::Lan => (
                settings.lan.session_idle_timeout_seconds,
                settings.lan.session_absolute_timeout_seconds,
            ),
            Channel::Tunnel => (
                settings.tunnel.session_idle_timeout_seconds,
                settings.tunnel.session_absolute_timeout_seconds,
            ),
        };
        let issued = self
            .sessions
            .issue(via, peer, idle, absolute, audit::now_millis());
        self.audit(
            AuditRecord::new("session-open", peer, via.as_str(), "ok").detail(format!(
                "session {}; idle {idle}s, absolute {absolute}s",
                issued.id
            )),
        );
        self.publish();
        issued
    }

    /// 校验会话令牌并刷新空闲计时。空闲时长按会话通道取当前配置值。
    pub fn authenticate(&self, token: &str, peer: &str) -> Option<SessionRecord> {
        let settings = self.config();
        let record = self
            .sessions
            .authenticate(token, audit::now_millis(), |via| match via {
                Channel::Lan => settings.lan.session_idle_timeout_seconds,
                Channel::Tunnel => settings.tunnel.session_idle_timeout_seconds,
            })?;
        if record.peer != peer {
            // 同一会话换 IP 不是错误(手机切 Wi-Fi/蜂窝),但值得留痕。
            tracing::debug!(session = %record.id, "remote session used from a different peer");
        }
        Some(record)
    }

    /// 吊销指定会话(状态页里逐个踢人用)。
    pub fn revoke_session(&self, id: &str) -> bool {
        let revoked = self.sessions.revoke_session(id);
        if revoked {
            self.audit(
                AuditRecord::new("session-revoked", "local", "local", "ok").detail(id.to_string()),
            );
            self.publish();
        }
        revoked
    }

    /// 吊销一个会话令牌(退出登录用)。
    pub fn revoke_token(&self, token: &str) -> bool {
        self.sessions.revoke_token(token)
    }

    /// 某个 PIN challenge 所属的通道(决定会话 cookie 属性)。
    pub fn challenge_channel(&self, challenge: &str) -> Option<Channel> {
        self.challenges.channel_of(challenge)
    }

    /// 会话 cookie 是否要带 `Secure`。
    ///
    /// 只有隧道通道带:公网必须走 https,而局域网是 http,带上 `Secure`
    /// 浏览器会直接丢弃这个 cookie —— 那等于把局域网连接做成"扫码后一直
    /// 登录不上",而不是更安全。
    pub fn cookie_secure(&self, via: Via) -> bool {
        match via {
            Via::Tunnel => true,
            Via::Local => false,
            // 局域网:仅在用户显式要求 https 时才带(当前版本恒为 false)。
            Via::Lan => false,
        }
    }

    /// 会话绝对有效期(秒),用于 cookie 的 `Max-Age`。
    pub fn session_absolute_seconds(&self, via: Via) -> u64 {
        let settings = self.config();
        match via {
            Via::Tunnel => settings.tunnel.session_absolute_timeout_seconds,
            // 本机/局域网:局域网那份配置就是默认值(两者共用一套超时)。
            Via::Lan | Via::Local => settings.lan.session_absolute_timeout_seconds,
        }
    }

    /// Host 白名单:本机名、局域网候选、当前隧道 host、用户配置的额外 host。
    ///
    /// 这是防 DNS rebinding 的那一步:攻击者把 `evil.com` 解析到 127.0.0.1,
    /// 浏览器会带上我们的 cookie 发请求,但 Host 头仍是 `evil.com`。
    pub fn host_allowed(&self, host_header: &str, tunnel_host: Option<&str>) -> bool {
        let Some(host) = normalize_host(host_header) else {
            return false;
        };
        if matches!(host.as_str(), "localhost" | "127.0.0.1" | "[::1]" | "::1") {
            return true;
        }
        if let Some(tunnel_host) = tunnel_host
            && normalize_host(tunnel_host).as_deref() == Some(host.as_str())
        {
            return true;
        }
        if net::candidates()
            .iter()
            .any(|candidate| candidate.address.to_string() == host)
        {
            return true;
        }
        let settings = self.config();
        if let Ok(address) = settings.lan.bind_address.trim().parse::<std::net::IpAddr>()
            && address.to_string() == host
        {
            return true;
        }
        settings
            .allowed_hosts
            .iter()
            .filter_map(|allowed| normalize_host(allowed))
            .any(|allowed| allowed == host)
    }

    /// 当前隧道 host(未开隧道时 None)。
    pub async fn tunnel_host(&self) -> Option<String> {
        self.tunnel.info().await.map(|info| info.host)
    }

    /// 本机请求判定:回环 + 不带 Cloudflare 回源头。
    pub fn classify(peer: Option<SocketAddr>, has_cf_header: bool) -> Via {
        match peer {
            Some(addr) if addr.ip().is_loopback() => {
                if has_cf_header {
                    Via::Tunnel
                } else {
                    Via::Local
                }
            }
            // 拿不到 peer 信息时按最保守的一类处理:要求票据与会话。
            _ => Via::Lan,
        }
    }

    /// 有效来源 IP:只有"回环 + 带 CF 头"(即经 cloudflared 回源)时才
    /// 采信 `cf-connecting-ip`,否则一律用 TCP peer —— 局域网客户端伪造
    /// 这个头只会把自己伪装成别人,不能让限流按伪造值计数。
    pub fn effective_peer(
        peer: Option<SocketAddr>,
        via: Via,
        cf_connecting_ip: Option<&str>,
    ) -> String {
        if via == Via::Tunnel
            && let Some(raw) = cf_connecting_ip
            && let Some(ip) = raw.split(',').next().map(str::trim).filter(|s| !s.is_empty())
            && ip.parse::<std::net::IpAddr>().is_ok()
        {
            return ip.to_string();
        }
        peer.map(|addr| addr.ip().to_string())
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// 启动后台清扫:过期的票据、PIN 挑战与会话按分钟清理。
    ///
    /// 不靠访问触发:隧道票据 TTL 只有两分钟,若没人再访问,过期条目会一直
    /// 留在表里 —— 表小但没必要留着。
    pub fn spawn_sweeper(self: &Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let swept = manager.sweep();
                if swept > 0 {
                    tracing::debug!(count = swept, "swept expired remote tickets/challenges/sessions");
                }
            }
        });
    }

    /// 过期票据 / 挑战 / 会话的定期清理。
    pub fn sweep(&self) -> usize {
        let now = audit::now_millis();
        self.tickets.purge_expired(now)
            + self.challenges.purge_expired(now)
            + self.sessions.sweep_expired(now)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn blocked_peers(&self) -> usize {
        self.limiter.blocked_count(audit::now_millis())
    }

    /// 选择局域网访问地址(状态页里的网卡切换)。不改绑定,只改生成链接
    /// 用的地址 —— 绑定 0.0.0.0 时所有网卡本来就都能连进来。
    pub fn select_lan_address(&self, address: &str) -> Result<(), String> {
        let parsed = address
            .parse::<std::net::IpAddr>()
            .map_err(|_| format!("不是合法的 IP 地址:{address}"))?;
        let listener = self.listener.lock().unwrap_or_else(|p| p.into_inner());
        if !listener.as_ref().is_some_and(|handle| handle.lan) {
            return Err("局域网连接尚未开启".to_string());
        }
        drop(listener);
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.lan_address = Some(parsed.to_string());
        Ok(())
    }

    /* ---- 内部 ---- */

    async fn spawn_listener(
        &self,
        bind: SocketAddr,
        lan: bool,
        tunnel_origin: bool,
    ) -> Result<SocketAddr, String> {
        let router = self
            .router
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| "远程 Router 尚未装配".to_string())?;

        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .map_err(|error| format!("绑定 {bind} 失败:{error}"))?;
        let actual = listener
            .local_addr()
            .map_err(|error| format!("读取绑定地址失败:{error}"))?;

        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let task = tokio::spawn(async move {
            let service = router.into_make_service_with_connect_info::<SocketAddr>();
            let served = axum::serve(listener, service).with_graceful_shutdown(async move {
                shutdown.cancelled().await;
            });
            if let Err(error) = served.await {
                tracing::error!(%error, "remote listener stopped unexpectedly");
            }
        });

        let mut slot = self.listener.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_some() {
            cancel.cancel();
            return Err("已有远程 listener 在运行".to_string());
        }
        *slot = Some(ListenerHandle {
            bind: actual,
            lan,
            tunnel_origin,
            cancel,
            task,
        });
        Ok(actual)
    }

    async fn lan_status(&self) -> Option<LanStatus> {
        self.status(true).await.lan
    }

    async fn tunnel_status(&self) -> Result<TunnelStatus, String> {
        self.status(true)
            .await
            .tunnel
            .ok_or_else(|| "隧道未运行".to_string())
    }

    fn publish(&self) {
        let _ = self.events.send(ServerEvent::RemoteUpdated);
    }

    /* ---- 测试辅助 ---- */

    /// 直接签一张票据(测试用:跳过真实 listener,直接拿到明文)。
    #[cfg(test)]
    pub fn issue_ticket_for_test(&self, via: Channel, base: &str, single_use: bool) -> String {
        let settings = self.config();
        let ttl = match via {
            Channel::Lan => settings.lan.ticket_ttl_seconds,
            Channel::Tunnel => settings.tunnel.ticket_ttl_seconds,
        };
        let require_pin = match via {
            Channel::Lan => settings.lan.require_pin,
            Channel::Tunnel => settings.tunnel.require_pin,
        };
        let token = self
            .tickets
            .issue(via, ttl, single_use, require_pin, audit::now_millis());
        format!("{base}/?ticket={token}")
            .rsplit_once("ticket=")
            .map(|(_, token)| token.to_string())
            .unwrap_or(token)
    }

    /// 设定通道 PIN 并返回它(测试用)。
    #[cfg(test)]
    pub fn set_pin_for_test(&self, via: Channel, pin: &str) -> String {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        match via {
            Channel::Lan => inner.lan_pin = Some(pin.to_string()),
            Channel::Tunnel => inner.tunnel_pin = Some(pin.to_string()),
        }
        pin.to_string()
    }

    /// 直接写入某个通道的票据展示槽(测试用:不起真实隧道也能验证两条
    /// 通道的链接互不串台)。
    #[cfg(test)]
    pub fn set_ticket_for_test(&self, via: Channel, url: &str) {
        let mut inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.set_ticket(
            via,
            Some(IssuedTicket {
                url: url.to_string(),
                expires_at: audit::now_millis() + 60_000,
            }),
        );
    }

    /// 某条通道当前展示的票据链接(测试用)。
    #[cfg(test)]
    pub fn ticket_for_test(&self, via: Channel) -> Option<String> {
        let inner = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        inner.ticket_for(via).map(|ticket| ticket.url.clone())
    }

    /// 起一个 listener 并返回实际端口(测试用:不必真的拉 cloudflared
    /// 也能构造"隧道已开"的状态)。
    #[cfg(test)]
    pub async fn spawn_listener_for_test(
        &self,
        bind: std::net::SocketAddr,
        lan: bool,
    ) -> Result<u16, String> {
        self.spawn_listener(bind, lan, !lan)
            .await
            .map(|addr| addr.port())
    }
}

/// 去掉 Host 头里的端口与方括号,统一小写。
fn normalize_host(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_port = if let Some(rest) = trimmed.strip_prefix('[') {
        // IPv6 字面量:`[::1]:3602` → `[::1]`
        match rest.split_once(']') {
            Some((host, _)) => format!("[{host}]"),
            None => return None,
        }
    } else {
        match trimmed.rsplit_once(':') {
            // 只有一段冒号才当端口(`a:b:c` 不是合法 host:port)。
            Some((host, port))
                if !host.contains(':')
                    && (port.is_empty() || port.chars().all(|c| c.is_ascii_digit())) =>
            {
                host.to_string()
            }
            _ => trimmed.to_string(),
        }
    };
    Some(without_port.to_ascii_lowercase())
}

fn candidate_views() -> Vec<CandidateView> {
    net::candidates()
        .into_iter()
        .map(|candidate| CandidateView {
            interface: candidate.interface,
            address: candidate.address.to_string(),
            recommended: candidate.recommended,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(raw: &str) -> Option<SocketAddr> {
        Some(raw.parse().unwrap())
    }

    #[test]
    fn normalizes_host_headers() {
        assert_eq!(normalize_host("127.0.0.1:3602").as_deref(), Some("127.0.0.1"));
        assert_eq!(normalize_host("Example.COM").as_deref(), Some("example.com"));
        assert_eq!(normalize_host("[::1]:3602").as_deref(), Some("[::1]"));
        assert_eq!(
            normalize_host("abc-def.trycloudflare.com").as_deref(),
            Some("abc-def.trycloudflare.com")
        );
        assert_eq!(normalize_host("   ").as_deref(), None);
        assert_eq!(normalize_host("").as_deref(), None);
        // 冒号结尾(无端口)不能把 host 削掉。
        assert_eq!(normalize_host("example.com:").as_deref(), Some("example.com"));
        assert_eq!(normalize_host("[::1]").as_deref(), Some("[::1]"));
        assert_eq!(normalize_host("[::1]:").as_deref(), Some("[::1]"));
        // 畸形方括号不猜。
        assert_eq!(normalize_host("[::1"), None);
    }

    #[test]
    fn classifies_requests_by_peer_and_cloudflare_headers() {
        assert_eq!(RemoteManager::classify(peer("127.0.0.1:5000"), false), Via::Local);
        assert_eq!(RemoteManager::classify(peer("127.0.0.1:5000"), true), Via::Tunnel);
        assert_eq!(RemoteManager::classify(peer("[::1]:5000"), true), Via::Tunnel);
        assert_eq!(RemoteManager::classify(peer("192.168.1.20:5000"), false), Via::Lan);
        // 局域网客户端伪造 CF 头也只能被当成隧道流量(会被 https 校验拦下)。
        assert_eq!(RemoteManager::classify(peer("192.168.1.20:5000"), true), Via::Lan);
        assert_eq!(RemoteManager::classify(None, false), Via::Lan);
        assert_eq!(RemoteManager::classify(None, true), Via::Lan);
    }

    #[test]
    fn cf_connecting_ip_only_trusted_for_tunnel_traffic() {
        // 经 cloudflared 回源:采信真实客户端 IP。
        assert_eq!(
            RemoteManager::effective_peer(peer("127.0.0.1:1"), Via::Tunnel, Some("203.0.113.9")),
            "203.0.113.9"
        );
        // 局域网直连伪造该头无效。
        assert_eq!(
            RemoteManager::effective_peer(peer("192.168.1.20:1"), Via::Lan, Some("203.0.113.9")),
            "192.168.1.20"
        );
        // 本机直通也不采信。
        assert_eq!(
            RemoteManager::effective_peer(peer("127.0.0.1:1"), Via::Local, Some("203.0.113.9")),
            "127.0.0.1"
        );
        // 多级代理链取第一个。
        assert_eq!(
            RemoteManager::effective_peer(
                peer("127.0.0.1:1"),
                Via::Tunnel,
                Some("203.0.113.9, 10.0.0.1")
            ),
            "203.0.113.9"
        );
        // 非法值退回 peer。
        assert_eq!(
            RemoteManager::effective_peer(peer("127.0.0.1:1"), Via::Tunnel, Some("not-an-ip")),
            "127.0.0.1"
        );
        assert_eq!(RemoteManager::effective_peer(None, Via::Lan, None), "unknown");
    }

    #[test]
    fn via_maps_to_channel() {
        assert_eq!(Via::Local.channel(), None);
        assert_eq!(Via::Lan.channel(), Some(Channel::Lan));
        assert_eq!(Via::Tunnel.channel(), Some(Channel::Tunnel));
        assert_eq!(Channel::Tunnel.as_str(), "tunnel");
        assert_eq!(Via::Local.as_str(), "local");
    }
}
