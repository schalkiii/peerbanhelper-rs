//! IP/CIDR 工具：前缀聚合与包含判定。

use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// 一组 CIDR 网络，用于 bypass / IP 黑名单包含判定。
#[derive(Clone, Debug, Default)]
pub struct IpSet {
    nets: Vec<IpNet>,
}

impl IpSet {
    pub fn from_cidrs<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut nets = Vec::new();
        for s in items {
            if let Some(net) = parse_net(s.as_ref()) {
                nets.push(net);
            }
        }
        Self { nets }
    }

    /// 对齐 Java `ra.contains(pa) || ra.equals(pa)`
    pub fn contains(&self, ip: &str) -> bool {
        let Ok(addr) = IpAddr::from_str(ip.trim()) else {
            // 可能带端口或为压缩 IPv6，尝试剥离
            return match split_host_port(ip) {
                Some(host) => IpAddr::from_str(&host).is_ok_and(|a| self.contains_addr(&a)),
                None => false,
            };
        };
        self.contains_addr(&addr)
    }

    pub fn contains_addr(&self, addr: &IpAddr) -> bool {
        self.nets.iter().any(|n| n.contains(addr))
    }

    /// 返回命中的网段的字符串形式（对齐 Java `IPBlackList` 中的 `ra.toString()`）。
    pub fn matching(&self, ip: &str) -> Option<String> {
        let addr = match IpAddr::from_str(ip.trim()) {
            Ok(a) => a,
            Err(_) => split_host_port(ip).and_then(|h| IpAddr::from_str(&h).ok())?,
        };
        self.nets.iter().find(|n| n.contains(&addr)).map(|n| n.to_string())
    }

    pub fn is_empty(&self) -> bool {
        self.nets.is_empty()
    }
}

/// 解析单个 IP 并归一化：去方括号、IPv4-mapped IPv6（`::ffff:a.b.c.d`）视为 IPv4。
///
/// 对齐 `IPAddressUtil.getIPAddress` 的归一化部分（不伪造占位地址，解析失败返回 None）。
pub fn parse_addr(ip: &str) -> Option<IpAddr> {
    let trimmed = ip.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    match IpAddr::from_str(inner).ok()? {
        IpAddr::V6(v6) => Some(match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        }),
        v4 => Some(v4),
    }
}

/// 解析单 IP 或 CIDR（单 IP 按 /32、/128 处理）。
pub fn parse_net(s: &str) -> Option<IpNet> {
    let s = s.trim();
    if let Ok(net) = IpNet::from_str(s) {
        return Some(net);
    }
    if let Ok(ip) = IpAddr::from_str(s) {
        return match ip {
            IpAddr::V4(v4) => Some(IpNet::V4(Ipv4Net::new(v4, 32).ok()?)),
            IpAddr::V6(v6) => Some(IpNet::V6(Ipv6Net::new(v6, 128).ok()?)),
        };
    }
    None
}

/// 计算 peer IP 的聚合前缀字符串（对齐 toPrefixBlock）。
pub fn prefix_block(ip: &str, v4_len: u8, v6_len: u8) -> Option<String> {
    let addr = IpAddr::from_str(ip.trim()).ok()?;
    Some(match addr {
        IpAddr::V4(v4) => {
            let net = Ipv4Net::new(v4, v4_len.min(32)).ok()?;
            net.trunc().to_string()
        }
        IpAddr::V6(v6) => {
            let net = Ipv6Net::new(v6, v6_len.min(128)).ok()?;
            net.trunc().to_string()
        }
    })
}

/// 从 `ip:port` / `[v6]:port` 中取出 host 部分。
pub fn split_host_port(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.starts_with('[') {
        let end = raw.find(']')?;
        return Some(raw[1..end].to_string());
    }
    if let Some(idx) = raw.rfind(':') {
        // 仅当冒号后是端口（数字）
        if raw[idx + 1..].chars().all(|c| c.is_ascii_digit()) {
            return Some(raw[..idx].to_string());
        }
    }
    Some(raw.to_string())
}

/// 反向 DNS 查询名（PTR 名），对齐 inet.ipaddr 的 `toReverseDNSLookupString()`：
/// - IPv4：`4.3.2.1.in-addr.arpa`
/// - IPv6：32 个半字节逆序 + `.ip6.arpa`
pub fn reverse_dns_name(ip: &str) -> Option<String> {
    let addr = parse_addr(ip)?;
    Some(match addr {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            format!("{}.{}.{}.{}.in-addr.arpa", o[3], o[2], o[1], o[0])
        }
        IpAddr::V6(v6) => {
            let hex = format!("{:032x}", u128::from(v6));
            let mut reversed: String = String::with_capacity(32 * 2 + 9);
            for ch in hex.chars().rev() {
                reversed.push(ch);
                reversed.push('.');
            }
            reversed.push_str("ip6.arpa");
            reversed
        }
    })
}

/// 判断是否 IPv4（含 IPv4-mapped IPv6）。
pub fn is_ipv4(ip: &str) -> bool {
    matches!(IpAddr::from_str(ip.trim()), Ok(IpAddr::V4(_)))
}

#[allow(dead_code)]
pub fn v4_unspecified() -> Ipv4Addr {
    Ipv4Addr::UNSPECIFIED
}
#[allow(dead_code)]
pub fn v6_unspecified() -> Ipv6Addr {
    Ipv6Addr::UNSPECIFIED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_contains() {
        let set = IpSet::from_cidrs(["192.168.0.0/16", "10.0.0.0/8", "fe80::/10"]);
        assert!(set.contains("192.168.1.23"));
        assert!(set.contains("10.255.255.255"));
        assert!(!set.contains("8.8.8.8"));
        assert!(set.contains("fe80::1"));
        assert!(!set.contains("2001:4860:4860::8888"));
    }

    #[test]
    fn single_ip_and_prefix() {
        let set = IpSet::from_cidrs(["1.2.3.4"]);
        assert!(set.contains("1.2.3.4"));
        assert!(!set.contains("1.2.3.5"));
        assert_eq!(prefix_block("1.2.3.4", 32, 56).as_deref(), Some("1.2.3.4/32"));
        assert_eq!(prefix_block("2001:db8::1234", 32, 56).as_deref(), Some("2001:db8::/56"));
    }

    #[test]
    fn host_port_split() {
        assert_eq!(split_host_port("1.2.3.4:51413").as_deref(), Some("1.2.3.4"));
        assert_eq!(split_host_port("[2001:db8::1]:51413").as_deref(), Some("2001:db8::1"));
    }
}
