//! CIDR 重映射的 property/golden 测试：用独立计算的期望值与实现逐项比对。
//!
//! 随机源为确定性 LCG，种子可通过 `PBH_PROP_SEED` 覆盖并在日志中打印，
//! 失败时可用同一种子复现。输入限定为「普通地址」——NAT64/Teredo/IPv4-mapped
//! 有额外转换分支，由 `remap_golden.rs` 的定点用例覆盖。
//!
//! 期望值不调用被测的 remap 逻辑：
//! - IPv4 映射写法由 octets 手工拼装；
//! - IPv4 /30 网段用 `ipnet` 计算；
//! - IPv6 /52 网段用 128 位掩码计算。

use pbh_core::remap::{remap_ban_list_address, RemapConfig};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

struct Lcg(u64);

impl Lcg {
    fn next_u64(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn next_u128(&mut self) -> u128 {
        ((self.next_u64() as u128) << 64) | self.next_u64() as u128
    }
}

fn seed() -> u64 {
    let seed = std::env::var("PBH_PROP_SEED")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(20_261_025);
    eprintln!("PBH_PROP_SEED={seed}");
    seed
}

fn mapped_v4(address: Ipv4Addr) -> String {
    let octets = address.octets();
    format!(
        "::ffff:{:x}:{:x}",
        u16::from_be_bytes([octets[0], octets[1]]),
        u16::from_be_bytes([octets[2], octets[3]])
    )
}

fn assert_valid_outputs(outputs: &HashSet<String>, context: &str) {
    for output in outputs {
        assert!(
            output.parse::<IpAddr>().is_ok() || output.parse::<ipnet::IpNet>().is_ok(),
            "{context}: 非法地址/网段 {output}"
        );
    }
}

#[test]
fn ipv4_remap_matches_independent_golden() {
    let seed = seed();
    let mut rng = Lcg(seed);
    let mut config = RemapConfig::default();
    for iteration in 0..300 {
        let address = Ipv4Addr::from(rng.next_u64() as u32);
        let got: HashSet<String> =
            remap_ban_list_address(&address.to_string(), true, &config)
                .into_iter()
                .collect();
        let expected: HashSet<String> = [address.to_string(), mapped_v4(address)]
            .into_iter()
            .collect();
        assert_eq!(
            got, expected,
            "seed={seed} iteration={iteration} address={address}"
        );
        assert_valid_outputs(&got, &format!("seed={seed} address={address}"));
    }

    config.banlist_remapping.ipv4.enabled = true;
    let prefix = config.banlist_remapping.ipv4.remap_range;
    for iteration in 0..200 {
        let address = Ipv4Addr::from(rng.next_u64() as u32);
        let network = ipnet::Ipv4Net::new(address, prefix)
            .expect("valid prefix")
            .network();
        let got: HashSet<String> =
            remap_ban_list_address(&address.to_string(), true, &config)
                .into_iter()
                .collect();
        let expected: HashSet<String> = [
            address.to_string(),
            format!("{network}/{prefix}"),
            mapped_v4(address),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "seed={seed} iteration={iteration} address={address} prefix={prefix}"
        );
        assert_valid_outputs(&got, &format!("seed={seed} address={address}"));
    }
}

fn is_special_v6(address: &Ipv6Addr) -> bool {
    let segments = address.segments();
    let nat64 = segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2..6].iter().all(|segment| *segment == 0);
    let teredo = segments[0] == 0x2001 && segments[1] == 0;
    address.to_ipv4_mapped().is_some() || nat64 || teredo
}

#[test]
fn ipv6_remap_matches_independent_golden() {
    let seed = seed();
    let mut rng = Lcg(seed);
    let config = RemapConfig::default();
    let prefix = config.banlist_remapping.ipv6.remap_range;
    let mut checked = 0;
    while checked < 300 {
        let address = Ipv6Addr::from(rng.next_u128());
        if is_special_v6(&address) {
            continue;
        }
        let mask = if prefix == 0 {
            0
        } else {
            u128::MAX << (128 - u32::from(prefix))
        };
        let network = Ipv6Addr::from(u128::from(address) & mask);
        let got: HashSet<String> =
            remap_ban_list_address(&address.to_string(), true, &config)
                .into_iter()
                .collect();
        let expected: HashSet<String> = [
            address.to_string(),
            format!("{network}/{prefix}"),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            got, expected,
            "seed={seed} iteration={checked} address={address} prefix={prefix}"
        );
        assert_valid_outputs(&got, &format!("seed={seed} address={address}"));
        checked += 1;
    }

    for _ in 0..100 {
        let address = Ipv6Addr::from(rng.next_u128());
        if is_special_v6(&address) {
            continue;
        }
        let got = remap_ban_list_address(&address.to_string(), false, &config);
        assert_eq!(
            got,
            vec![address.to_string()],
            "不支持范围封禁时只下发单地址：seed={seed} address={address}"
        );
    }
}
