//! L2：`profile.yml` → 判定流水线的映射（模块开关、ban-duration、规则集、bypass 地址）。
//!
//! 对齐上游：
//! - `AbstractFeatureModule.shouldModuleEnabled()`：配置节缺失 / 无 `enabled` 键 → **禁用**该模块；
//! - 各模块 `reloadConfig()` 读取的键名与默认值；
//! - `ban-duration: 0`（或缺失）→ 使用全局 `ban-duration`。

use pbh_core::config::ProfileConfig;
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::module::{CheckContext, PeerAction};
use pbh_core::pipeline::Decision;

const PROFILE: &str = r#"
check-interval: 5000
ban-duration: 1209600000
ignore-peers-from-addresses:
  - "10.0.0.0/8"
  - "192.168.0.0/16"
module:
  peer-id-blacklist:
    enabled: true
    ban-duration: 1000
    banned-peer-id:
      - '{"method":"EQUALS","content":"evil"}'
  ip-address-blocker:
    enabled: true
    ban-duration: 2000
    ips:
      - "1.2.3.0/24"
    ports:
      - 4444
  progress-cheat-blocker:
    enabled: false
"#;

fn profile() -> ProfileConfig {
    serde_yaml::from_str(PROFILE).expect("profile yaml")
}

fn peer(ip: &str, port: u16, peer_id: &str, client: &str, uploaded: i64, progress: f64) -> PeerData {
    PeerData {
        client_name: Some(client.to_string()),
        peer_id: Some(peer_id.to_string()),
        dl_speed: 1000,
        downloaded: 1000,
        up_speed: 1000,
        uploaded,
        progress,
        flags: Some("d u".into()),
        ip: ip.into(),
        port,
        raw_ip: format!("{ip}:{port}"),
        connection: Some("uTP".into()),
    }
}

fn torrent() -> TorrentData {
    TorrentData {
        hash: "h".into(),
        name: "t".into(),
        progress: 1.0,
        total_size: 1_000_000_000,
        piece_size: 0,
        pieces_have: 0,
        completed_override: None,
        dlspeed: 0,
        upspeed: 0,
        is_private: Some(false),
    }
}

fn ctx() -> CheckContext {
    CheckContext { now_ms: 0, features: vec!["UNBAN_IP".into()] }
}

#[test]
fn profile_drives_module_set_and_order() {
    let p = profile().build_pipeline();
    let names: Vec<&str> = p.modules.iter().map(|m| m.config_name()).collect();
    // client-name-blacklist 未在 profile 中出现 -> 上游视为禁用；PCB 显式 enabled: false
    assert_eq!(names, vec!["ip-address-blocker", "peer-id-blacklist"]);
    assert_eq!(p.global_ban_duration_ms, 1_209_600_000);
}

#[test]
fn profile_rules_and_ban_durations_are_applied() {
    let p = profile().build_pipeline();
    let t = torrent();

    // 自定义 peer-id 规则 + 模块级 ban-duration
    match p.evaluate("d", &t, &peer("9.9.9.9", 1, "evil", "qBittorrent", 10, 0.1), &ctx()) {
        Decision::Ban(r) => {
            assert_eq!(r.module, "peer-id-blacklist");
            assert_eq!(r.ban_duration_ms, 1_000, "使用模块级 ban-duration");
        }
        other => panic!("expected ban, got {other:?}"),
    }

    // 配置的 CIDR 与端口黑名单
    match p.evaluate("d", &t, &peer("1.2.3.77", 1, "-qB5000-xxxxxxxxxxxx", "qBittorrent", 10, 0.1), &ctx()) {
        Decision::Ban(r) => {
            assert_eq!(r.module, "ip-address-blocker");
            assert_eq!(r.ban_duration_ms, 2_000);
        }
        other => panic!("expected ban, got {other:?}"),
    }
    match p.evaluate("d", &t, &peer("8.8.8.8", 4444, "-qB5000-xxxxxxxxxxxx", "qBittorrent", 10, 0.1), &ctx()) {
        Decision::Ban(r) => assert_eq!(r.module, "ip-address-blocker"),
        other => panic!("expected ban, got {other:?}"),
    }
}

#[test]
fn profile_bypass_addresses_come_from_config() {
    let p = profile().build_pipeline();
    let t = torrent();
    // 10.0.0.0/8 在配置的 bypass 列表中 -> SKIP
    match p.evaluate("d", &t, &peer("10.1.2.3", 1, "evil", "qBittorrent", 10, 0.1), &ctx()) {
        Decision::Skip(r) => {
            assert_eq!(r.action, PeerAction::Skip);
            assert_eq!(r.rule, "general-rule-ignored-address");
        }
        other => panic!("expected skip, got {other:?}"),
    }
    // 未在列表中的地址仍然参与判定（192.168.0.0/16 命中 -> SKIP）
    assert!(matches!(
        p.evaluate("d", &t, &peer("192.168.5.5", 1, "evil", "qBittorrent", 10, 0.1), &ctx()),
        Decision::Skip(_)
    ));
    assert!(matches!(
        p.evaluate("d", &t, &peer("8.8.4.4", 1, "evil", "qBittorrent", 10, 0.1), &ctx()),
        Decision::Ban(_)
    ));
}

#[test]
fn module_missing_ban_duration_falls_back_to_global() {
    // 模块未写 ban-duration（等价于 0）-> 使用全局 ban-duration
    let yaml = r#"
ban-duration: 1209600000
module:
  ip-address-blocker:
    enabled: true
    ips:
      - "1.2.3.0/24"
"#;
    let cfg: ProfileConfig = serde_yaml::from_str(yaml).unwrap();
    let p = cfg.build_pipeline();
    match p.evaluate("d", &torrent(), &peer("1.2.3.77", 1, "-qB-", "qB", 10, 0.1), &ctx()) {
        Decision::Ban(r) => assert_eq!(r.ban_duration_ms, 1_209_600_000, "回退到全局 ban-duration"),
        other => panic!("expected ban, got {other:?}"),
    }
}

/// 出厂默认配置必须与上游 `profile.yml` 等价：
/// 四个规则模块全部启用、顺序正确、规则集逐字一致、默认参数一致。
#[test]
fn shipped_default_config_is_equivalent_to_upstream_profile() {
    #[derive(serde::Deserialize)]
    struct Doc {
        profile: ProfileConfig,
        /// 主配置的 GeoIP 数据库段（对齐上游 `config.yml` 的 `ip-database`）
        #[serde(rename = "ip-database")]
        ip_database: pbh_core::config::IpDatabaseConfig,
    }
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../pbh/src/default-config.yml");
    let text = std::fs::read_to_string(&path).expect("crates/pbh/src/default-config.yml 必须存在");
    let doc: Doc = serde_yaml::from_str(&text).expect("默认配置必须可解析");

    let p = doc.profile.build_pipeline();
    let names: Vec<&str> = p.modules.iter().map(|m| m.config_name()).collect();
    assert_eq!(
        names,
        vec![
            "ip-address-blocker",
            "peer-id-blacklist",
            "client-name-blacklist",
            "expression-engine",
            "progress-cheat-blocker",
            "multi-dialing-blocker",
            "auto-range-ban",
            // 本轮接线新增：上游 `registerModules()` 中 BtnNetworkOnline 位于
            // AutoRangeBan 之后、IPBlackRuleList 之前（profile.yml 的 `module.btn` 默认启用）。
            // 未配置 BTN 传输层时该模块恒 pass()，不改变任何封禁决策。
            "btn",
            "ip-address-blocker-rules",
            "anti-vampire"
        ],
        "默认配置必须启用上游 profile.yml 中默认启用的全部规则模块（顺序一致）"
    );
    assert_eq!(doc.profile.check_interval, 5_000);
    assert_eq!(doc.profile.ban_duration, 1_209_600_000);
    assert_eq!(
        doc.profile.ignore_peers_from_addresses,
        pbh_core::defaults::DEFAULT_IGNORE_ADDRESSES
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );

    // 规则集逐字一致（防止 YAML 里写错 method/content）
    let peer_id = doc.profile.module.peer_id_blacklist.as_ref().unwrap();
    assert_eq!(
        peer_id.banned_peer_id,
        pbh_core::defaults::DEFAULT_BANNED_PEER_ID
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );
    let client = doc.profile.module.client_name_blacklist.as_ref().unwrap();
    assert_eq!(
        client.banned_client_name,
        pbh_core::defaults::DEFAULT_BANNED_CLIENT_NAME
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
    );

    // PCB 默认参数一致
    let pcb = doc.profile.module.progress_cheat_blocker.as_ref().unwrap();
    assert_eq!(pcb.minimum_size, 50_000_000);
    assert_eq!(pcb.maximum_difference, 0.1);
    assert_eq!(pcb.rewind_maximum_difference, 0.07);
    assert_eq!(pcb.excessive_threshold, 1.5);
    assert_eq!(pcb.ipv4_prefix_length, 32);
    assert_eq!(pcb.ipv6_prefix_length, 56);
    assert_eq!(pcb.ban_duration_ms, 2_592_000_000);
    assert_eq!(pcb.max_wait_duration_ms, 30_000);
    assert_eq!(pcb.fast_pcb_test_percentage, 0.1);
    assert_eq!(pcb.fast_pcb_test_block_duration_ms, 15_000);

    // 多拨与范围封禁默认值一致
    let mdb = doc.profile.module.multi_dialing_blocker.as_ref().unwrap();
    assert_eq!(mdb.ban_duration_ms, 1_296_000_000);
    assert_eq!(mdb.subnet_mask_length, 24);
    assert_eq!(mdb.subnet_mask_v6_length, 56);
    assert_eq!(mdb.tolerate_num_ipv4, 2);
    assert_eq!(mdb.tolerate_num_ipv6, 5);
    assert_eq!(mdb.cache_lifespan_secs, 86_400);
    assert!(!mdb.keep_hunting);
    assert_eq!(mdb.keep_hunting_time_secs, 2_592_000);

    let arb = doc.profile.module.auto_range_ban.as_ref().unwrap();
    assert_eq!(arb.ban_duration_ms, 604_800_000);
    assert_eq!(arb.ipv4, 30);
    assert_eq!(arb.ipv6, 48);

    // 反吸血（迅雷预设）默认值一致
    let av = doc.profile.module.anti_vampire.as_ref().unwrap();
    assert_eq!(av.ban_duration_ms, 14_400_000);
    assert!(av.presets.xunlei.enabled);

    // BTN（上游 profile.yml 第 264-267 行）：enabled: true / ban-duration: 259200000
    let btn = doc.profile.module.btn.as_ref().expect("默认配置必须包含 module.btn");
    assert!(matches!(btn.enabled, Some(true)));
    assert_eq!(btn.ban_duration_ms, 259_200_000);

    // `ip-address-blocker` 的 GeoIP 维度（逐字对齐上游 profile.yml 的随包值）
    let ipb = doc.profile.module.ip_address_blocker.as_ref().unwrap();
    assert_eq!(ipb.asns, vec![0], "上游写成字符串 \"0\"，按 getLongList 口径解析为整数");
    assert_eq!(ipb.regions, vec!["0".to_string()]);
    assert_eq!(ipb.cities, vec!["示例海南".to_string()]);
    assert!(
        ipb.net_type.to_tokens().is_empty(),
        "随包 net-type 的 9 个 kebab 开关全为 false ⇒ 迁移后没有任何 camelCase 标志"
    );

    // 主配置的 `ip-database` 段（上游 config.yml：auto-update: true + 三个数据库名）
    assert!(doc.ip_database.auto_update);
    assert_eq!(doc.ip_database.database_city, "GeoLite2-City");
    assert_eq!(doc.ip_database.database_asn, "GeoLite2-ASN");
    assert_eq!(doc.ip_database.database_geocn, "GeoCN");
    assert_eq!(doc.ip_database.account_id, "");
    assert_eq!(doc.ip_database.license_key, "");

    // 监控模块（上游 registerModules 顺序：ActiveMonitoring → SwarmTracking →
    // SessionAnalyse → PeerRecording），**不进入**判定流水线
    let active = doc.profile.module.active_monitoring.as_ref().expect("active-monitoring");
    assert!(matches!(active.enabled, Some(true)));
    let active_settings = active.to_settings();
    assert_eq!(active_settings.daily_traffic_capping, -1);
    assert!(!active_settings.use_traffic_sliding_capping);
    assert_eq!(active_settings.max_traffic_allowed_in_window_period, 53_687_091_200);
    assert_eq!(active_settings.traffic_sliding_capping_max_speed, 10_485_760);
    assert_eq!(active_settings.traffic_sliding_capping_min_speed, 0);

    let analyse = doc
        .profile
        .module
        .peer_analyse_service
        .as_ref()
        .expect("peer-analyse-service");
    let session = analyse.session_analyse.as_ref().unwrap();
    assert!(matches!(session.enabled, Some(true)));
    assert_eq!(session.data_flush_interval_ms, 3_600_000);
    assert_eq!(session.cleanup_interval_ms, 3_600_000);
    assert_eq!(session.data_retention_time_ms, 15_552_000_000);
    let swarm = analyse.swarm_tracking.as_ref().unwrap();
    assert_eq!(swarm.to_settings().data_flush_interval_ms, 3_600_000);
    let recording = analyse.peer_recording.as_ref().unwrap();
    let recording_settings = recording.to_settings();
    assert_eq!(recording_settings.data_flush_interval_ms, 900_000);
    assert_eq!(recording_settings.data_retention_time_ms, 5_184_000_000);
    assert_eq!(recording_settings.data_cleanup_interval_ms, 604_800_000);

    let monitor_modules = doc.profile.build_monitor_modules(std::sync::Arc::new(
        pbh_core::modules::InMemoryMonitorSink::new(),
    ));
    let monitor_names: Vec<&str> = monitor_modules.iter().map(|m| m.config_name()).collect();
    assert_eq!(
        monitor_names,
        vec![
            "active-monitoring",
            "peer-analyse-service.swarm-tracking",
            "peer-analyse-service.session-analyse",
            "peer-analyse-service.peer-recording"
        ]
    );
}

/// 出厂默认配置的地址重映射段必须与上游 `config.yml` 默认值一致。
#[test]
fn shipped_default_config_has_upstream_remap_defaults() {
    use pbh_core::remap::{BanlistRemapping, IpRemapConfig};

    #[derive(serde::Deserialize)]
    struct Doc {
        #[serde(rename = "banlist-remapping")]
        banlist_remapping: BanlistRemapping,
        #[serde(rename = "ip-remapping")]
        ip_remapping: IpRemapConfig,
    }

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../pbh/src/default-config.yml");
    let text = std::fs::read_to_string(&path).expect("crates/pbh/src/default-config.yml 必须存在");
    let doc: Doc = serde_yaml::from_str(&text).expect("默认配置必须可解析");

    assert!(!doc.banlist_remapping.ipv4.enabled, "IPv4 重映射默认关闭");
    assert_eq!(doc.banlist_remapping.ipv4.remap_range, 30);
    assert!(doc.banlist_remapping.ipv6.enabled, "IPv6 重映射默认开启");
    assert_eq!(doc.banlist_remapping.ipv6.remap_range, 52);

    assert!(!doc.ip_remapping.teredo, "Teredo 默认不转换");
    assert!(doc.ip_remapping.nat64.enabled);
    assert_eq!(doc.ip_remapping.nat64.prefix, vec!["64:ff9b::/96".to_string()]);

    // AutoSTUN（`ip-remapping.auto-stun`）：默认关闭 ⇒ 不挂载翻译表、翻译严格直通；
    // STUN 服务器列表对齐上游 config.yml 的 `stun.tcp-servers` / `stun.udp-servers`
    let auto_stun = &doc.ip_remapping.auto_stun;
    assert!(!auto_stun.enabled, "auto-stun 默认关闭（上游 `auto-stun.enabled: false`）");
    assert!(auto_stun.use_friendly_loopback_mapping, "友好回环映射默认开启");
    assert!(auto_stun.downloaders.is_empty(), "默认不为任何下载器启用隧道");
    assert_eq!(
        auto_stun.tcp_servers,
        vec![
            "turn.cloudflare.com:3478",
            "stun.nextcloud.com:3478",
            "stun.sipnet.com:3478"
        ]
    );
    assert_eq!(
        auto_stun.udp_servers,
        vec![
            "stun.cdnbye.com:3478",
            "stun.nextcloud.com:3478",
            "stun.miwifi.com:3478",
            "stun.syncthing.net:3478",
            "stun.l.google.com:3478"
        ]
    );
    assert!(
        doc.ip_remapping.auto_stun_registry.is_none(),
        "随包配置默认不挂载 AutoSTUN 映射表（应用层仅在 enabled=true 时 with_auto_stun）"
    );
}

#[test]
fn missing_module_section_disables_the_module() {
    let cfg: ProfileConfig = serde_yaml::from_str("ban-duration: 1000\n").unwrap();
    let p = cfg.build_pipeline();
    assert!(p.modules.is_empty(), "配置节缺失时上游视为禁用");
    // 默认 bypass 地址仍然生效
    assert!(matches!(
        p.evaluate("d", &torrent(), &peer("192.168.1.1", 1, "evil", "qB", 10, 0.1), &ctx()),
        Decision::Skip(_)
    ));
}
