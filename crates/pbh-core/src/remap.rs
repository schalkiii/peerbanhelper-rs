//! IP 地址重映射：封禁列表重映射 + peer 地址翻译。
//!
//! 对齐上游：
//! - `IPAddressUtil.remapBanListAddress` / `generateRemappedPairIfPossible`（封禁列表 → CIDR）
//! - `AbstractDownloader.addressTranslate`（内置 NAT / Teredo / NAT64 / IPv4-mapped 归一）
//!
//! 默认值来自上游 `config.yml`：`banlist-remapping.ipv4.enabled=false`（`remap-range=30`）、
//! `banlist-remapping.ipv6.enabled=true`（`remap-range=52`）、`ip-remapping.teredo=false`、
//! `ip-remapping.nat64.enabled=true`（`prefix=["64:ff9b::/96"]`）、`auto-stun.enabled=false`。
//!
//! 内置 NAT（AutoSTUN）翻译实现在 [`crate::auto_stun`]；默认 `auto-stun.enabled=false` 时
//! 注册表为空，`translate_peer_ip` 与移植前逐位一致（严格直通）。
//!
//! 关于输出格式：上游用 inet.ipaddr 的 `toCompressedString()`，IPv6 采用其压缩写法；
//! 本实现采用 RFC 5952（Rust `Ipv6Addr` 的 `Display`），两者的**实际封禁集合等价**。
//! 另外，上游会为 IPv4 地址附带其 IPv4-mapped IPv6 写法（`::ffff:a.b.c.d`），
//! 对下载器而言该写法不会命中真实 IPv4 peer，属于空操作，故本实现不生成（见 SPEC §3.4）。

pub use crate::auto_stun::{AutoStun, AutoStunConfig};
use crate::iputil::parse_addr;
use ipnet::{Ipv4Net, Ipv6Net};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::sync::Arc;

/// `config.yml` 中与地址重映射相关的配置段。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemapConfig {
    #[serde(rename = "banlist-remapping", default)]
    pub banlist_remapping: BanlistRemapping,
    #[serde(rename = "ip-remapping", default)]
    pub ip_remapping: IpRemapConfig,
}

/// `banlist-remapping:` 段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BanlistRemapping {
    #[serde(default = "default_ipv4_remap")]
    pub ipv4: RemapRange,
    #[serde(default = "default_ipv6_remap")]
    pub ipv6: RemapRange,
}

impl Default for BanlistRemapping {
    fn default() -> Self {
        Self {
            ipv4: default_ipv4_remap(),
            ipv6: default_ipv6_remap(),
        }
    }
}

/// 单个地址族的重映射设置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemapRange {
    #[serde(default)]
    pub enabled: bool,
    /// 前缀长度：主机位被置 0 后生成 CIDR
    #[serde(rename = "remap-range", default)]
    pub remap_range: u8,
}

fn default_ipv4_remap() -> RemapRange {
    RemapRange {
        enabled: false,
        remap_range: 30,
    }
}
fn default_ipv6_remap() -> RemapRange {
    RemapRange {
        enabled: true,
        remap_range: 52,
    }
}

/// `ip-remapping:` 段（`teredo` 默认 false，`nat64` 默认开启）。
///
/// `auto_stun` 对应上游**顶层** `auto-stun:` 段（默认 `enabled: false`）；
/// `auto_stun_registry` 是运行时映射表（对齐上游 Spring 单例 `NatAddressProviderRegistry`），
/// 不参与序列化，`None` ⇒ 内置 NAT 翻译恒直通。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpRemapConfig {
    /// Teredo 转换（上游默认关闭）
    #[serde(default)]
    pub teredo: bool,
    #[serde(rename = "nat64", default)]
    pub nat64: Nat64Config,
    /// `auto-stun:` 段（AutoSTUN 内置 NAT 翻译）
    #[serde(rename = "auto-stun", default)]
    pub auto_stun: AutoStunConfig,
    /// AutoSTUN 运行时映射表；由应用层用 [`AutoStunConfig::build`] 挂载
    #[serde(skip)]
    pub auto_stun_registry: Option<Arc<AutoStun>>,
}

impl IpRemapConfig {
    /// 挂载 AutoSTUN 运行时映射表（对齐上游 `BTStunManager.register` →
    /// `NatAddressProviderRegistry.add`）。未挂载或 `auto-stun.enabled=false` 时翻译恒直通。
    pub fn with_auto_stun(mut self, registry: Arc<AutoStun>) -> Self {
        self.auto_stun_registry = Some(registry);
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nat64Config {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_nat64_prefix")]
    pub prefix: Vec<String>,
}

impl Default for Nat64Config {
    fn default() -> Self {
        Self {
            enabled: true,
            prefix: default_nat64_prefix(),
        }
    }
}

fn default_true() -> bool {
    true
}
fn default_nat64_prefix() -> Vec<String> {
    vec!["64:ff9b::/96".to_string()]
}

impl Nat64Config {
    /// 从 `IpRemapConfig` 直接取用时的便捷访问（`nat64_enabled`）。
    fn extract(&self, v6: &Ipv6Addr) -> Option<Ipv4Addr> {
        if !self.enabled {
            return None;
        }
        for raw in &self.prefix {
            if let Ok(net) = Ipv6Net::from_str(raw.trim()) {
                if net.contains(v6) {
                    return Some(embedded_ipv4(v6));
                }
            }
        }
        None
    }
}

/// IPv6 低 32 位（`getEmbeddedIPv4Address` 语义）。
fn embedded_ipv4(v6: &Ipv6Addr) -> Ipv4Addr {
    Ipv4Addr::from((u128::from(*v6) & 0xFFFF_FFFF) as u32)
}

/// Teredo 前缀 `2001:0000::/32`（`IPv6Address.isTeredo()`）。
pub fn is_teredo(v6: &Ipv6Addr) -> bool {
    let s = v6.segments();
    s[0] == 0x2001 && s[1] == 0x0000
}

/// 解析 Teredo 内嵌的「客户端 IPv4 + UDP 端口」，对齐 `IPAddressUtil.extractTeredo`。
pub fn extract_teredo(v6: &Ipv6Addr) -> (Ipv4Addr, u16) {
    let s = v6.segments();
    let obfuscated = (u128::from(*v6) & 0xFFFF_FFFF) as u32;
    let client = Ipv4Addr::from(!obfuscated);
    let port = !s[5];
    (client, port)
}

/// Teredo 内嵌客户端 IPv4（仅地址）。
fn teredo_client_ip(v6: &Ipv6Addr) -> Ipv4Addr {
    extract_teredo(v6).0
}

fn prefix_of_v4(addr: Ipv4Addr, len: u8) -> Option<String> {
    Ipv4Net::new(addr, len.min(32))
        .ok()
        .map(|n| n.trunc().to_string())
}

fn prefix_of_v6(addr: Ipv6Addr, len: u8) -> Option<String> {
    Ipv6Net::new(addr, len.min(128))
        .ok()
        .map(|n| n.trunc().to_string())
}

/// 生成某个地址的「等价写法」集合，对齐上游 `generateRemappedPairIfPossible`
///（分支顺序与上游一致，IPv4 分支优先）：
/// - IPv4 → 附带 IPv4-mapped IPv6 写法（`::ffff:a.b.c.d`）。qB 的封禁名单按地址族匹配，
///   缺少该变体时，同一主机改走 IPv6 栈连入将不会被阻断。
/// - NAT64 → 附带内嵌 IPv4
/// - Teredo → 附带内嵌客户端 IPv4（上游此处**不检查** `ip-remapping.teredo` 开关）
fn equivalent_forms(addr: IpAddr, nat64: Option<Ipv4Addr>) -> Vec<IpAddr> {
    let mut out = Vec::new();
    if let IpAddr::V4(v4) = addr {
        out.push(IpAddr::V6(v4.to_ipv6_mapped()));
        return out;
    }
    if let Some(v4) = nat64 {
        out.push(IpAddr::V4(v4));
    } else if let IpAddr::V6(v6) = addr {
        if is_teredo(&v6) {
            out.push(IpAddr::V4(teredo_client_ip(&v6)));
        }
    }
    out
}

/// 封禁列表地址重映射，对齐 `IPAddressUtil.remapBanListAddress(address, supportRangeBan)`。
///
/// `support_range_ban` 为下载器是否声明了 `RANGE_BAN_IP` 能力（qB 需 >= 5.3.0）；
/// 为 false 时只输出单个地址，不生成任何 CIDR 网段。
pub fn remap_ban_list_address(ip: &str, support_range_ban: bool, cfg: &RemapConfig) -> Vec<String> {
    let Some(addr) = parse_addr(ip) else {
        return vec![ip.trim().to_string()];
    };
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };

    push(addr.to_string());

    // NAT64 提取（默认开启）
    let nat64 = match addr {
        IpAddr::V6(v6) => cfg.ip_remapping.nat64.extract(&v6),
        IpAddr::V4(_) => None,
    };
    if let Some(v4) = nat64 {
        push(v4.to_string());
    }

    let range = &cfg.banlist_remapping;
    // IPv4 重映射（上游默认关闭）
    if support_range_ban && range.ipv4.enabled {
        let target = match addr {
            IpAddr::V4(v4) => Some(v4),
            IpAddr::V6(_) => nat64,
        };
        if let Some(v4) = target {
            if let Some(p) = prefix_of_v4(v4, range.ipv4.remap_range) {
                push(p);
            }
        }
    }
    // IPv6 重映射（上游默认开启，/52）；NAT64 / Teredo 地址不参与
    if support_range_ban && range.ipv6.enabled {
        if let IpAddr::V6(v6) = addr {
            if nat64.is_none() && !is_teredo(&v6) {
                if let Some(p) = prefix_of_v6(v6, range.ipv6.remap_range) {
                    push(p);
                }
            }
        }
    }

    // generateRemappedPairIfPossible(banAddress)
    for extra in equivalent_forms(addr, nat64) {
        push(extra.to_string());
    }

    out
}

/// peer 地址翻译，对齐 `AbstractDownloader.addressTranslate`（步骤顺序不可调换）：
///
/// 1. 内置 NAT（`convertIfManagedByBuiltInNat`，AutoSTUN）：下载器报告的本地/内网地址 →
///    隧道发现到的 NAT 公网地址（[`crate::auto_stun`]）；
/// 2. Teredo（`ip-remapping.teredo`，命中后**同时**改写 IP 与端口）；
/// 3. NAT64（`ip-remapping.nat64.enabled`，取低 32 位内嵌 IPv4，端口不变）；
/// 4. IPv4-mapped（`::ffff:a.b.c.d`）归一到 IPv4。
///
/// 每一步都基于**上一步改写后**的地址继续（上游为就地 `peerAddress.applyNat/setIp`）。
/// 内置 NAT 步骤仅在本配置挂载了 `auto_stun_registry` 时生效；默认配置
/// （`auto-stun.enabled: false`）注册表为空，行为与移植前完全一致（严格直通）。
pub fn translate_peer_ip(ip: &str, port: u16, cfg: &IpRemapConfig) -> (String, u16) {
    let Some(addr) = parse_addr(ip) else {
        return (ip.trim().to_string(), port);
    };
    // 1. 内置 NAT（AutoSTUN）：未启用 / 未命中映射 ⇒ 原样直通，不阻塞、不报错
    let (addr, port) = match cfg
        .auto_stun_registry
        .as_deref()
        .and_then(|t| t.translate(addr, port))
    {
        Some((translated, translated_port)) => (translated, translated_port),
        None => (addr, port),
    };
    // 2. / 3. Teredo / NAT64（对改写后的地址判定）
    if let IpAddr::V6(v6) = addr {
        if cfg.teredo && is_teredo(&v6) {
            let (client, teredo_port) = extract_teredo(&v6);
            return (client.to_string(), teredo_port);
        }
        if let Some(v4) = cfg.nat64.extract(&v6) {
            return (v4.to_string(), port);
        }
    }
    // 4. IPv4-mapped 已在 `parse_addr` 中归一
    (addr.to_string(), port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nat64_extract_uses_low_32_bits() {
        let v6 = Ipv6Addr::from_str("64:ff9b::102:304").unwrap();
        assert_eq!(embedded_ipv4(&v6), Ipv4Addr::new(1, 2, 3, 4));
    }

    #[test]
    fn teredo_detection_and_port_decode() {
        let v6 = Ipv6Addr::from_str("2001:0:4136:e378:8000:63bf:3fff:fdd2").unwrap();
        assert!(is_teredo(&v6));
        assert_eq!(extract_teredo(&v6), (Ipv4Addr::new(192, 0, 2, 45), 40000));
        assert!(!is_teredo(&Ipv6Addr::from_str("2001:db8::1").unwrap()));
    }

    #[test]
    fn unparsable_address_is_passed_through() {
        let cfg = RemapConfig::default();
        assert_eq!(
            remap_ban_list_address("not-an-ip", true, &cfg),
            vec!["not-an-ip"]
        );
        assert_eq!(
            translate_peer_ip("not-an-ip", 1, &cfg.ip_remapping),
            ("not-an-ip".to_string(), 1)
        );
    }

    /// 回归：`auto-stun.enabled` 默认 false（且未挂载注册表）时，`translate_peer_ip`
    /// 的输出与移植 AutoSTUN 之前**完全一致**（上游 `NatAddressProviderRegistry` 为空 ⇒ 直通）。
    #[test]
    fn translate_peer_ip_is_unchanged_while_auto_stun_is_disabled() {
        let cfg = IpRemapConfig::default();
        assert!(!cfg.auto_stun.enabled, "auto-stun.enabled 默认 false");
        assert!(
            cfg.auto_stun_registry.is_none(),
            "默认不挂载 AutoSTUN 注册表"
        );
        for (ip, port, expected) in [
            ("192.168.1.5", 6881u16, ("192.168.1.5".to_string(), 6881u16)),
            ("8.8.8.8", 1, ("8.8.8.8".to_string(), 1)),
            ("2001:db8::1", 2, ("2001:db8::1".to_string(), 2)),
            ("::ffff:1.2.3.4", 3, ("1.2.3.4".to_string(), 3)),
            ("64:ff9b::1.2.3.4", 4, ("1.2.3.4".to_string(), 4)),
        ] {
            assert_eq!(translate_peer_ip(ip, port, &cfg), expected, "输入 {ip}");
        }
    }

    /// 回归：即便挂载了注册表，只要 `auto-stun.enabled=false`（上游不注册任何 provider），
    /// 翻译仍是直通。
    #[test]
    fn disabled_auto_stun_registry_keeps_passthrough() {
        let registry = AutoStunConfig::default().build();
        assert!(registry.insert_mapping("192.168.0.0/16", "203.0.113.9"));
        let cfg = IpRemapConfig::default().with_auto_stun(registry);
        assert_eq!(
            translate_peer_ip("192.168.1.5", 6881, &cfg),
            ("192.168.1.5".to_string(), 6881)
        );
    }

    #[test]
    fn auto_stun_rewrites_private_ip_to_discovered_public_ip() {
        let registry = AutoStunConfig {
            enabled: true,
            ..AutoStunConfig::default()
        }
        .build();
        assert!(registry.insert_mapping("192.168.0.0/16", "203.0.113.9"));
        let cfg = IpRemapConfig::default().with_auto_stun(registry);

        // 命中：改写为公网地址，端口不变
        assert_eq!(
            translate_peer_ip("192.168.1.5", 6881, &cfg),
            ("203.0.113.9".to_string(), 6881)
        );
        // 未命中（内网但不在映射网段）：直通
        assert_eq!(
            translate_peer_ip("10.1.2.3", 6881, &cfg),
            ("10.1.2.3".to_string(), 6881)
        );
        // 公网地址：直通
        assert_eq!(
            translate_peer_ip("8.8.8.8", 6881, &cfg),
            ("8.8.8.8".to_string(), 6881)
        );
        // IPv6：STUN 发现路径只登记 IPv4 网段 ⇒ 直通
        assert_eq!(
            translate_peer_ip("2001:db8::1", 6881, &cfg),
            ("2001:db8::1".to_string(), 6881)
        );
    }

    /// 链式顺序回归：内置 NAT 步骤先于 Teredo/NAT64，后续步骤读取**改写后**的地址
    /// （对齐 `addressTranslate` 的就地改写顺序）。
    #[test]
    fn built_in_nat_rewrite_precedes_teredo_and_nat64() {
        let registry = AutoStunConfig {
            enabled: true,
            ..AutoStunConfig::default()
        }
        .build();
        assert!(registry.insert_mapping("64:ff9b::/96", "203.0.113.9"));
        assert!(registry.insert_mapping("2001:0:4136:e378::/64", "198.51.100.7"));
        let cfg = IpRemapConfig::default().with_auto_stun(registry);

        // 内嵌 IPv4 的 NAT64 地址先被内置 NAT 改写为公网 IPv4，故不再走 NAT64 分支
        assert_eq!(
            translate_peer_ip("64:ff9b::1.2.3.4", 6881, &cfg),
            ("203.0.113.9".to_string(), 6881)
        );
        // Teredo 地址同理：改写后已不是 Teredo（端口取映射后的端口）
        assert_eq!(
            translate_peer_ip("2001:0:4136:e378:8000:63bf:3fff:fdd2", 6881, &cfg),
            ("198.51.100.7".to_string(), 6881)
        );
    }

    /// 回归：未挂载注册表但 `auto-stun.enabled=true`（例如 STUN 尚不可达）⇒ 直通。
    #[test]
    fn enabled_auto_stun_without_registry_is_passthrough() {
        let cfg = IpRemapConfig {
            auto_stun: AutoStunConfig {
                enabled: true,
                ..AutoStunConfig::default()
            },
            ..IpRemapConfig::default()
        };
        assert_eq!(
            translate_peer_ip("192.168.1.5", 6881, &cfg),
            ("192.168.1.5".to_string(), 6881)
        );
    }
}
