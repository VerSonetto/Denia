//! 局域网网卡探测:枚举可作为访问入口的本机 IPv4 地址。
//!
//! 只收 IPv4:手机浏览器扫 `http://192.168.x.x:port/?ticket=…` 是最省事的
//! 形态,IPv6 链路本地地址要写 `%25` 转义且依赖网段配置,放进候选只会让
//! 用户对着一个连不通的二维码发呆。多网卡时全部列出,由用户选。

use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 一块可用于局域网访问的网卡地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanCandidate {
    /// 网卡名(Windows 上是"以太网"/"WLAN"这类可读名)。
    pub interface: String,
    pub address: Ipv4Addr,
    /// 是否为推荐项(私有网段 + 网卡 up)。推荐项在 UI 里默认选中。
    pub recommended: bool,
}

/// 枚举候选地址,推荐项排在前面。
///
/// 过滤规则:排除回环、排除链路本地(169.254/16,自动配置失败的残留)、
/// 排除未 up 的网卡。虚拟网卡(VPN/虚拟机的 198.18 段等)保留但降级为
/// 非推荐 —— 用户可能真的要从 VPN 网段连进来。
pub fn candidates() -> Vec<LanCandidate> {
    let mut out: Vec<LanCandidate> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|interface| {
            // 先取完需要借用整个 `interface` 的信息,再移动它的字段。
            let oper_up = interface.is_oper_up();
            match interface.addr {
                if_addrs::IfAddr::V4(v4) => {
                    let address = v4.ip;
                    if address.is_loopback() || address.is_link_local() || address.is_unspecified() {
                        return None;
                    }
                    Some(LanCandidate {
                        interface: interface.name,
                        address,
                        recommended: oper_up && is_private(address),
                    })
                }
                if_addrs::IfAddr::V6(_) => None,
            }
        })
        .collect();
    // 推荐项在前,其次按地址数值排序(同一网卡每次枚举顺序稳定)。
    out.sort_by_key(|c| (!c.recommended, u32::from(c.address), c.interface.clone()));
    out
}

/// 缓存窗口:同一窗口内重复问"本机有哪些地址"不再走 syscall。
const CACHE_TTL: Duration = Duration::from_secs(5);

static CACHE: Mutex<Option<CachedIfAddrs>> = Mutex::new(None);

struct CachedIfAddrs {
    at: Instant,
    /// 只留 Host 校验需要的东西:地址字符串集合。
    addresses: Vec<String>,
}

/// `candidates()` 的缓存版,专给"每个请求都要问一次"的路径用
/// (远程门的 Host 白名单校验)。
///
/// 枚举网卡是一次阻塞系统调用(Windows 上 `GetAdaptersAddresses`,Linux 上
/// 读 netlink),放在 async 中间件里逐请求付,等于给手机上每一次点击都加
/// 一截主机侧开销 —— 而且它还是项目规范里明确要求 `spawn_blocking` 的那类
/// 调用。网卡列表在秒级尺度上不会变,5 秒窗口足够:拔网线、切 Wi-Fi 这种
/// 变化的代价最多是"新地址 5 秒内还没进白名单",而那时用户本来就连不通。
///
/// 需要即时结果的调用方(开局域网监听、切换地址)仍用 `candidates()`。
pub fn cached_addresses() -> Vec<String> {
    let mut guard = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(cached) = guard.as_ref().filter(|cached| cached.at.elapsed() < CACHE_TTL) {
        return cached.addresses.clone();
    }
    let addresses: Vec<String> = candidates()
        .into_iter()
        .map(|candidate| candidate.address.to_string())
        .collect();
    *guard = Some(CachedIfAddrs {
        at: Instant::now(),
        addresses: addresses.clone(),
    });
    addresses
}

/// 测试/网卡变化后主动失效缓存。
pub fn invalidate_cache() {
    let mut guard = CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = None;
}

/// 私有网段判定(RFC 1918 + CGNAT):这些地址才可能真的在同一局域网里。
fn is_private(address: Ipv4Addr) -> bool {
    address.is_private() || address.octets()[0] == 100 && (64..128).contains(&address.octets()[1])
}

/// 没有候选时的兜底:至少让用户能手工填地址,不静默失败。
pub fn preferred(candidates: &[LanCandidate]) -> Option<Ipv4Addr> {
    candidates.first().map(|c| c.address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges_are_recognized() {
        assert!(is_private(Ipv4Addr::new(192, 168, 1, 10)));
        assert!(is_private(Ipv4Addr::new(10, 0, 0, 5)));
        assert!(is_private(Ipv4Addr::new(172, 16, 3, 4)));
        // CGNAT(100.64/10):运营商级 NAT 后的地址,局域网场景里可能真实存在。
        assert!(is_private(Ipv4Addr::new(100, 64, 0, 1)));
        assert!(!is_private(Ipv4Addr::new(8, 8, 8, 8)));
        assert!(!is_private(Ipv4Addr::new(172, 32, 0, 1)));
    }

    #[test]
    fn candidates_exclude_loopback_and_link_local() {
        for candidate in candidates() {
            assert!(!candidate.address.is_loopback(), "{candidate:?}");
            assert!(!candidate.address.is_link_local(), "{candidate:?}");
        }
    }

    #[test]
    fn preferred_picks_first() {
        let list = vec![LanCandidate {
            interface: "eth0".into(),
            address: Ipv4Addr::new(192, 168, 1, 2),
            recommended: true,
        }];
        assert_eq!(preferred(&list), Some(Ipv4Addr::new(192, 168, 1, 2)));
        assert_eq!(preferred(&[]), None);
    }
}
