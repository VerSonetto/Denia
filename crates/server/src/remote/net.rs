//! 局域网网卡探测:枚举可作为访问入口的本机 IPv4 地址。
//!
//! 只收 IPv4:手机浏览器扫 `http://192.168.x.x:port/?ticket=…` 是最省事的
//! 形态,IPv6 链路本地地址要写 `%25` 转义且依赖网段配置,放进候选只会让
//! 用户对着一个连不通的二维码发呆。多网卡时全部列出,由用户选。

use std::net::Ipv4Addr;

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
