//! PTR 反查（对齐上游 `DNSLookupImpl` + `lookup.dns-reverse-lookup`）。
//!
//! 解析链：
//! 1. `resolvers.servers` 里的普通 DNS 服务器（IP / `IP:port`），按顺序查询；
//! 2. `resolvers.use-system: true`（默认）时用系统解析器（`getnameinfo`）。
//!
//! 单次查询上限 3 秒（对齐上游 `ExtendedResolver.setTimeout(3s)`）；DoH URL 暂不支持，
//! 构造时跳过并记录日志。未配置任何解析器时 [`PtrLookup::lookup`] 恒返回 `None`。

use crate::config::ResolversConfig;
use crate::iputil::reverse_dns_name;
use crate::modules::ptr_blacklist::{PtrResolver, SystemPtrResolver, PTR_LOOKUP_TIMEOUT};
use std::net::IpAddr;
use std::sync::mpsc;
use std::time::Duration;

/// PTR 查询器（可在多线程间共享）。
#[derive(Debug, Clone)]
pub struct PtrLookup {
    /// `lookup.dns-reverse-lookup`：仅控制「封禁元数据 / peers 列表」是否反查；
    /// `/api/peer/{ip}` 与上游一致，始终尝试解析。
    enabled: bool,
    use_system: bool,
    servers: Vec<IpAddr>,
    timeout: Duration,
}

impl PtrLookup {
    pub fn from_config(config: &ResolversConfig, enabled: bool) -> Self {
        let mut servers = Vec::new();
        for server in &config.servers {
            match parse_server(server) {
                Some(ip) => servers.push(ip),
                None if server.trim().starts_with("http") => {
                    tracing::warn!("不支持的 DoH 解析器（已跳过）：{server}");
                }
                None => tracing::warn!("无法解析 resolvers.servers 条目（已跳过）：{server}"),
            }
        }
        Self {
            enabled,
            use_system: config.use_system,
            servers,
            timeout: PTR_LOOKUP_TIMEOUT,
        }
    }

    /// 无解析器的空实现（测试 / 未配置 DNS 时使用）。
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            use_system: false,
            servers: Vec::new(),
            timeout: PTR_LOOKUP_TIMEOUT,
        }
    }

    /// `lookup.dns-reverse-lookup` 是否开启（封禁元数据、peers 列表使用）。
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 反查 IP 的 PTR 名；无记录 / 超时 / 未配置解析器时返回 `None`。
    pub fn lookup(&self, ip: &str) -> Option<String> {
        let reverse = reverse_dns_name(ip)?;
        if !self.servers.is_empty() {
            let resolver = SystemPtrResolver::with_servers(self.servers.clone());
            if let Some(name) = resolver.resolve(&reverse) {
                if !name.is_empty() {
                    return Some(ensure_fqdn(name));
                }
            }
        }
        if self.use_system {
            let address: IpAddr = crate::iputil::parse_addr(ip)?;
            return system_lookup(address, self.timeout).map(ensure_fqdn);
        }
        None
    }
}

/// 上游 dnsjava `PTRRecord.getTarget().toString()` 输出带尾点的绝对域名
/// （如 `dns.google.`）；系统 `getnameinfo` 通常省略尾点，这里统一补齐。
fn ensure_fqdn(name: String) -> String {
    if name.ends_with('.') {
        name
    } else {
        format!("{name}.")
    }
}

impl Default for PtrLookup {
    fn default() -> Self {
        Self::disabled()
    }
}

/// 系统解析器查询（`getnameinfo`）带等待上限：超时后返回 `None`，
/// 后台线程会在系统解析器自身超时后结束。
fn system_lookup(address: IpAddr, timeout: Duration) -> Option<String> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let result = dns_lookup::lookup_addr(&address);
        let _ = sender.send(result);
    });
    match receiver.recv_timeout(timeout) {
        Ok(Ok(name)) if !name.is_empty() => Some(name),
        _ => None,
    }
}

/// 解析 `resolvers.servers` 条目：支持 `1.1.1.1`、`1.1.1.1:53`、`[::1]:53`。
fn parse_server(value: &str) -> Option<IpAddr> {
    let value = value.trim();
    if value.is_empty() || value.starts_with("http") {
        return None;
    }
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Some(ip);
    }
    if let Some(rest) = value.strip_prefix('[') {
        return rest.split(']').next()?.parse().ok();
    }
    value.rsplit_once(':').and_then(|(host, _)| host.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_port_qualified_servers() {
        assert_eq!(parse_server("1.1.1.1"), Some("1.1.1.1".parse().unwrap()));
        assert_eq!(parse_server("8.8.8.8:53"), Some("8.8.8.8".parse().unwrap()));
        assert_eq!(parse_server("[2001:4860:4860::8888]:53"), Some("2001:4860:4860::8888".parse().unwrap()));
        assert_eq!(parse_server("https://dns.google/dns-query"), None);
        assert_eq!(parse_server("not-an-ip"), None);
    }

    #[test]
    fn fqdn_gets_a_trailing_dot_once() {
        assert_eq!(ensure_fqdn("dns.google".into()), "dns.google.");
        assert_eq!(ensure_fqdn("dns.google.".into()), "dns.google.");
    }

    #[test]
    fn disabled_lookup_returns_none() {
        let ptr = PtrLookup::disabled();
        assert!(!ptr.enabled());
        assert!(ptr.lookup("1.2.3.4").is_none());
    }
}
