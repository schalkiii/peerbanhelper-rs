//! 地址重映射的黄金测试，对齐上游 `IPAddressUtil.remapBanListAddress` /
//! `AbstractDownloader.addressTranslate`（v9.5.1 默认配置）。
//!
//! 默认配置（上游 `config.yml`）：
//! - `banlist-remapping.ipv4.enabled: false`、`remap-range: 30`
//! - `banlist-remapping.ipv6.enabled: true`、`remap-range: 52`
//! - `ip-remapping.teredo: false`、`ip-remapping.nat64.enabled: true`、`prefix: ["64:ff9b::/96"]`

use pbh_core::remap::{
    remap_ban_list_address, translate_peer_ip, IpRemapConfig, Nat64Config, RemapConfig,
};

fn cfg() -> RemapConfig {
    RemapConfig::default()
}

#[test]
fn ipv4_always_pairs_with_its_ipv4_mapped_ipv6_form() {
    // 上游 `generateRemappedPairIfPossible` 对 IPv4 恒附带 IPv6 映射写法，
    // 使下载器在 IPv6 栈上也能阻断同一主机；与是否开启网段重映射无关
    assert_eq!(
        remap_ban_list_address("1.2.3.4", true, &cfg()),
        vec!["1.2.3.4", "::ffff:1.2.3.4"]
    );
    // 显式开启 IPv4 重映射后额外生成 /30 网段
    let mut c = cfg();
    c.banlist_remapping.ipv4.enabled = true;
    assert_eq!(
        remap_ban_list_address("1.2.3.4", true, &c),
        vec!["1.2.3.4", "1.2.3.4/30", "::ffff:1.2.3.4"]
    );
}

#[test]
fn ipv6_is_remapped_to_the_configured_prefix_length() {
    // 上游默认 ipv6.enabled=true / remap-range=52
    // /52 保留前 3 个段（48 位）+ 第 4 段的高 4 位；0x0002 的高 4 位为 0，故第 4 段被置 0
    let mut got = remap_ban_list_address("2001:db8:1:2:3:4:5:6", true, &cfg());
    got.sort();
    assert_eq!(got, vec!["2001:db8:1:2:3:4:5:6", "2001:db8:1::/52"]);
}

#[test]
fn remote_downloader_without_range_ban_capability_gets_single_addresses() {
    // 下载器未声明 RANGE_BAN_IP 时，`remapBanListAddress(..., false)` 不生成任何网段
    let _ = cfg();
    let got = remap_ban_list_address("2001:db8:1:2:3:4:5:6", false, &cfg());
    assert_eq!(got, vec!["2001:db8:1:2:3:4:5:6"]);
}

#[test]
fn ipv4_mapped_ipv6_is_normalized_to_ipv4_then_paired_back() {
    // `getIPAddress` 先把 IPv4-mapped IPv6 归一为 IPv4，
    // 再由 `generateRemappedPairIfPossible` 把映射写法补回来
    assert_eq!(
        remap_ban_list_address("::ffff:1.2.3.4", true, &cfg()),
        vec!["1.2.3.4", "::ffff:1.2.3.4"]
    );
}

#[test]
fn nat64_address_also_bans_the_embedded_ipv4() {
    let mut got = remap_ban_list_address("64:ff9b::1.2.3.4", true, &cfg());
    got.sort();
    assert_eq!(got, vec!["1.2.3.4", "64:ff9b::102:304"]);
}

#[test]
fn teredo_address_also_bans_the_embedded_client_ipv4() {
    // RFC 4380 示例地址：client 192.0.2.45 / UDP 40000
    let mut got = remap_ban_list_address("2001:0:4136:e378:8000:63bf:3fff:fdd2", true, &cfg());
    got.sort();
    assert_eq!(
        got,
        vec!["192.0.2.45", "2001:0:4136:e378:8000:63bf:3fff:fdd2"]
    );
}

#[test]
fn peer_address_translation_follows_upstream_order() {
    let c = IpRemapConfig::default();
    // IPv4-mapped IPv6 -> IPv4
    assert_eq!(
        translate_peer_ip("::ffff:1.2.3.4", 6881, &c),
        ("1.2.3.4".to_string(), 6881)
    );
    // NAT64 -> 内嵌 IPv4（端口不变）
    assert_eq!(
        translate_peer_ip("64:ff9b::1.2.3.4", 6881, &c),
        ("1.2.3.4".to_string(), 6881)
    );
    // 普通地址原样返回
    assert_eq!(
        translate_peer_ip("8.8.8.8", 6881, &c),
        ("8.8.8.8".to_string(), 6881)
    );
    assert_eq!(
        translate_peer_ip("2001:db8::1", 6881, &c),
        ("2001:db8::1".to_string(), 6881)
    );

    // Teredo 默认关闭
    assert_eq!(
        translate_peer_ip("2001:0:4136:e378:8000:63bf:3fff:fdd2", 6881, &c),
        ("2001:0:4136:e378:8000:63bf:3fff:fdd2".to_string(), 6881)
    );
    // 显式开启后按 Teredo 内嵌信息改写 IP 与端口
    let teredo_on = IpRemapConfig {
        teredo: true,
        ..IpRemapConfig::default()
    };
    assert_eq!(
        translate_peer_ip("2001:0:4136:e378:8000:63bf:3fff:fdd2", 6881, &teredo_on),
        ("192.0.2.45".to_string(), 40000)
    );
}

#[test]
fn nat64_extraction_respects_enabled_flag_and_prefix_list() {
    let off = IpRemapConfig {
        nat64: Nat64Config {
            enabled: false,
            ..Nat64Config::default()
        },
        ..IpRemapConfig::default()
    };
    assert_eq!(
        translate_peer_ip("64:ff9b::1.2.3.4", 6881, &off),
        ("64:ff9b::102:304".to_string(), 6881)
    );

    let custom = IpRemapConfig {
        nat64: Nat64Config {
            enabled: true,
            prefix: vec!["2001:db8:64::/96".to_string()],
        },
        ..IpRemapConfig::default()
    };
    // 不在自定义前缀网段内 -> 不转换
    assert_eq!(
        translate_peer_ip("64:ff9b::1.2.3.4", 6881, &custom),
        ("64:ff9b::102:304".to_string(), 6881)
    );
    assert_eq!(
        translate_peer_ip("2001:db8:64::1.2.3.4", 6881, &custom),
        ("1.2.3.4".to_string(), 6881)
    );
}
