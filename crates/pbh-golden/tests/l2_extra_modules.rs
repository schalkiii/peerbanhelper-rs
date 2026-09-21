//! L2：`auto-range-ban` 与 `multi-dialing-blocker` 两个默认启用模块的判定对齐。

use pbh_core::banlist::BanList;
use pbh_core::i18n::Translator;
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::module::{CheckContext, PeerAction, RuleModule};
use pbh_core::modules::anti_vampire::{AntiVampire, AntiVampireSettings};
use pbh_core::modules::auto_range_ban::AutoRangeBan;
use pbh_core::modules::idle_protection::{
    IdleConnectionDosProtection, IdleProtectionSettings, ProtectionMode,
};
use pbh_core::modules::ptr_blacklist::{PtrBlacklist, PtrCache};
use pbh_core::rule::RuleSet;
use pbh_core::modules::multi_dialing::{MultiDialingBlocker, MultiDialingSettings};
use std::sync::{Arc, Mutex};

fn peer(ip: &str, peer_id: &str) -> PeerData {
    PeerData {
        client_name: Some("qBittorrent/4.5.0".into()),
        peer_id: Some(peer_id.into()),
        dl_speed: 1000,
        downloaded: 1000,
        up_speed: 1000,
        uploaded: 1000,
        progress: 0.5,
        flags: Some("d u".into()),
        ip: ip.into(),
        port: 6881,
        raw_ip: format!("{ip}:6881"),
        connection: Some("uTP".into()),
    }
}

fn torrent() -> TorrentData {
    TorrentData {
        hash: "0101010101010101010101010101010101010101".into(),
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

fn ctx(now_ms: i64) -> CheckContext {
    CheckContext { now_ms, features: vec!["UNBAN_IP".into()] }
}

// ---------- anti-vampire ----------

fn anti_vampire() -> AntiVampire {
    // 上游 profile.yml 默认：ban-duration 14400000、presets.xunlei.enabled true
    AntiVampire::new(AntiVampireSettings { ban_duration_ms: 14_400_000, xunlei_preset: true })
}

fn torrent_with_progress(progress: f64) -> TorrentData {
    TorrentData { progress, ..torrent() }
}

#[test]
fn anti_vampire_xunlei_preset_rules() {
    let m = anti_vampire();
    let tr = Translator::embedded();
    let downloading = torrent_with_progress(0.5);
    let seeding = torrent_with_progress(1.0);

    // 迅雷 0019：下载中放行（它正常参与 swarm），做种时封禁
    let peer_0019 = peer("1.1.1.1", "-XL0019-abcdefghijkl");
    assert_eq!(m.check("d", &downloading, &peer_0019, &ctx(0)).action, PeerAction::NoAction);
    let r = m.check("d", &seeding, &peer_0019, &ctx(0));
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(r.ban_duration_ms, 14_400_000);
    assert_eq!(tr.render(r.rule_key.as_ref().unwrap(), "zh_cn"), "任务反吸血");
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "[对等反制] 已识别的拒绝做种的不良客户端（迅雷，0019 版本）：禁止连接到做种任务"
    );
    assert_eq!(r.data["xunleiType"], "0019");
    assert_eq!(r.data["seeding"], true);

    // 非 0019 迅雷：任何状态都封禁
    let old = peer("2.2.2.2", "-XL0018-abcdefghijkl");
    let r = m.check("d", &downloading, &old, &ctx(0));
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "[禁止] 已识别的拒绝上传的不良客户端（迅雷，非 0019 版本）：禁止连接到任何任务"
    );
    assert_eq!(r.data["xunleiType"], "non-0019");

    // clientName 命中：xunlei 开头
    let by_name = peer("3.3.3.3", "-qB4700-xxxxxxxxxxxx");
    let mut by_name = by_name;
    by_name.client_name = Some("Xunlei 0.0.1.9".into());
    assert_eq!(m.check("d", &downloading, &by_name, &ctx(0)).action, PeerAction::NoAction);
    assert_eq!(m.check("d", &seeding, &by_name, &ctx(0)).action, PeerAction::Ban);

    let mut old_by_name = peer("4.4.4.4", "-qB4700-xxxxxxxxxxxx");
    old_by_name.client_name = Some("Xunlei/1.0.0".into());
    assert_eq!(m.check("d", &downloading, &old_by_name, &ctx(0)).action, PeerAction::Ban);

    // 非迅雷客户端不受影响
    assert_eq!(m.check("d", &seeding, &peer("5.5.5.5", "-qB4700-xxxxxxxxxxxx"), &ctx(0)).action, PeerAction::NoAction);

    // 该模块不做握手判定（上游直接进入预设检查）
    let mut hs = peer("6.6.6.6", "-xl0018-handshaking");
    hs.up_speed = 0;
    hs.dl_speed = 0;
    assert_eq!(m.check("d", &downloading, &hs, &ctx(0)).action, PeerAction::Ban);
}

#[test]
fn anti_vampire_disabled_preset_never_bans() {
    let m = AntiVampire::new(AntiVampireSettings {
        ban_duration_ms: 14_400_000,
        xunlei_preset: false,
    });
    assert_eq!(
        m.check("d", &torrent(), &peer("1.1.1.1", "-XL0018-abcdefghijkl"), &ctx(0)).action,
        PeerAction::NoAction
    );
}

// ---------- ptr-blacklist ----------

fn ptr_blacklist(rule_json: &str, ptr: Option<&str>) -> PtrBlacklist {
    let rules = RuleSet::from_json_text(&[rule_json.to_string()]).unwrap();
    let cache = Arc::new(PtrCache::new());
    // 预热缓存（应用层会为 peer IP 预热，这里直接写入反向名）
    if let Some(name) = ptr {
        cache.insert("4.3.2.1.in-addr.arpa", Some(name.to_string()));
    }
    PtrBlacklist::new(rules, 259_200_000, cache)
}

#[test]
fn ptr_blacklist_matches_reverse_dns_against_rules() {
    let t = torrent_with_progress(0.5);
    let p = peer("1.2.3.4", "-qB4700-xxxxxxxxxxxx");
    let tr = Translator::embedded();

    // 默认规则 `{"method":"EQUALS","content":"example.com"}`
    let m = ptr_blacklist(r#"{"method":"EQUALS","content":"example.com"}"#, Some("example.com"));
    let r = m.check("d", &t, &p, &ctx(0));
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(r.ban_duration_ms, 259_200_000);
    assert_eq!(tr.render(r.rule_key.as_ref().unwrap(), "zh_cn"), "字符串匹配: example.com");
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "匹配 PTR 规则: 字符串匹配: example.com"
    );
    assert_eq!(r.data["rule"], "example.com");

    // ENDS_WITH 规则：命中的是 PTR 名
    let m = ptr_blacklist(
        r#"{"method":"ENDS_WITH","content":".example.com"}"#,
        Some("host.example.com"),
    );
    let r = m.check("d", &t, &p, &ctx(0));
    assert_eq!(r.action, PeerAction::Ban);
    assert!(
        Translator::embedded()
            .render(r.reason_key.as_ref().unwrap(), "zh_cn")
            .contains(".example.com")
    );

    // 未命中 -> pass
    let m = ptr_blacklist(r#"{"method":"CONTAINS","content":"evil"}"#, Some("host.example.com"));
    assert_eq!(m.check("d", &t, &p, &ctx(0)).action, PeerAction::NoAction);

    // 没有 PTR 记录 / 未预热 -> pass
    let m = ptr_blacklist(r#"{"method":"CONTAINS","content":"evil"}"#, None);
    assert_eq!(m.check("d", &t, &p, &ctx(0)).action, PeerAction::NoAction);

    // 握手中 -> handshaking
    let mut hs = peer("1.2.3.4", "-qB4700-xxxxxxxxxxxx");
    hs.up_speed = 0;
    hs.dl_speed = 0;
    let m = ptr_blacklist(r#"{"method":"EQUALS","content":"example.com"}"#, Some("example.com"));
    let r = m.check("d", &t, &hs, &ctx(0));
    assert_eq!(r.reason_key.as_ref().unwrap().key, "Peer handshaking");
}

#[test]
fn reverse_dns_names_follow_rfc() {
    use pbh_core::iputil::reverse_dns_name;
    assert_eq!(reverse_dns_name("1.2.3.4").as_deref(), Some("4.3.2.1.in-addr.arpa"));
    assert_eq!(
        reverse_dns_name("2001:db8::1").as_deref(),
        Some("1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.ip6.arpa")
    );
}

// ---------- idle-connection-dos-protection ----------

fn idle_settings() -> IdleProtectionSettings {
    // 上游 profile.yml 默认（enabled: false，这里直接测判定逻辑）
    IdleProtectionSettings {
        ban_duration_ms: 900_000,
        max_allowed_idle_time_ms: 300_000,
        idle_speed_threshold: 64,
        min_status_change_percentage: 0.001,
        reset_on_status_change: true,
        protect_mode: ProtectionMode::DeterminedByPeerFlags,
    }
}

/// 慢速 peer（进度恒为 0，避免被 `reset-on-status-change` 重置计时器）。
fn idle_peer() -> PeerData {
    let mut p = peer("10.0.0.1", "-qB4700-xxxxxxxxxxxx");
    p.up_speed = 0;
    p.dl_speed = 0;
    p.uploaded = 1000;
    p.downloaded = 1000;
    p.progress = 0.0;
    p
}

#[test]
fn idle_protection_bans_after_max_idle_time() {
    let m = IdleConnectionDosProtection::new(idle_settings());
    let seeding = torrent_with_progress(1.0);
    let p = idle_peer();

    // 做种任务：首次记录起点
    assert_eq!(m.check("d", &seeding, &p, &ctx(0)).action, PeerAction::NoAction);
    // 未超过最大空闲时间 -> 仍放行
    assert_eq!(
        m.check("d", &seeding, &p, &ctx(300_000)).action,
        PeerAction::NoAction,
        "等于阈值不封禁（上游用 `>`）"
    );
    // 超过阈值 -> 封禁
    let r = m.check("d", &seeding, &p, &ctx(300_001));
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(r.ban_duration_ms, 900_000);
    let tr = Translator::embedded();
    assert_eq!(tr.render(r.rule_key.as_ref().unwrap(), "zh_cn"), "空闲连接拒绝服务攻击保护");
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "Peer 10.0.0.1:6881 未在限定时间内进行任何有效的数据传输，封禁以释放下载器连接池资源"
    );
    assert_eq!(r.data["idle_type"], "timeout");
    assert_eq!(r.data["last_percentage"], 0.0);
    // 封禁后连接记录被清除
    assert_eq!(m.tracked_peers(), 0);
}

#[test]
fn idle_protection_resets_on_activity() {
    let m = IdleConnectionDosProtection::new(idle_settings());
    let seeding = torrent_with_progress(1.0);
    let p = idle_peer();
    assert_eq!(m.check("d", &seeding, &p, &ctx(0)).action, PeerAction::NoAction);

    // 速度超过阈值 -> 重置计时器
    let mut fast = p.clone();
    fast.up_speed = 1024;
    assert_eq!(m.check("d", &seeding, &fast, &ctx(100_000)).action, PeerAction::NoAction);
    assert_eq!(m.tracked_peers(), 0, "速度达标会移除空闲记录");
    // 重新开始计时，因此 300_001 时还不算超时
    assert_eq!(m.check("d", &seeding, &p, &ctx(300_001)).action, PeerAction::NoAction);
    assert_eq!(m.check("d", &seeding, &p, &ctx(600_002)).action, PeerAction::Ban);
}

/// 观察窗口内的平均速度超过阈值同样会重置计时器。
#[test]
fn idle_protection_resets_on_average_speed_within_window() {
    let m = IdleConnectionDosProtection::new(idle_settings());
    let seeding = torrent_with_progress(1.0);
    let p = idle_peer();
    assert_eq!(m.check("d", &seeding, &p, &ctx(0)).action, PeerAction::NoAction);

    // 1 秒内增长 1MiB -> 平均速度 1047 B/s > 64
    let mut uploaded_more = p.clone();
    uploaded_more.uploaded = 1000 + 1024 * 1024;
    assert_eq!(m.check("d", &seeding, &uploaded_more, &ctx(1_000)).action, PeerAction::NoAction);
    assert_eq!(m.tracked_peers(), 0, "平均速度达标会移除空闲记录");

    // 之后重新累计，从 1_001 起算
    assert_eq!(m.check("d", &seeding, &p, &ctx(1_001)).action, PeerAction::NoAction);
    assert_eq!(m.check("d", &seeding, &p, &ctx(301_003)).action, PeerAction::Ban);
}

/// 上游 `percentageChange = |progress * 100 - lastProgress|` 量纲不一致（×100 vs 未 ×100），
/// 因此只要 peer 汇报的进度非 0，`reset-on-status-change` 每次检查都会重置计时器——
/// 结果是进度非 0 的 peer 永远不会因空闲被封禁。此行为被忠实保留。
#[test]
fn idle_protection_status_change_uses_upstream_percentage_units() {
    let m = IdleConnectionDosProtection::new(idle_settings());
    let seeding = torrent_with_progress(1.0);
    let mut p = idle_peer();
    p.progress = 0.5;
    // 即使空闲远超阈值，进度非 0 也会被判定为“状态变化”而重置
    for ts in [0, 1_000_000, 2_000_000, 3_000_000] {
        assert_eq!(
            m.check("d", &seeding, &p, &ctx(ts)).action,
            PeerAction::NoAction,
            "ts={ts} 时被 reset-on-status-change 重置"
        );
    }
    assert_eq!(m.tracked_peers(), 0, "每次重置都会清空记录");
}

#[test]
fn idle_protection_respects_protect_mode_and_peer_flags() {
    let downloading = torrent_with_progress(0.5);
    let p = idle_peer();

    // 模式 0 + 下载任务 + 无 Peer Flags -> 不保护（直接放行）
    let no_flags = IdleConnectionDosProtection::new(IdleProtectionSettings {
        protect_mode: ProtectionMode::DeterminedByPeerFlags,
        ..idle_settings()
    });
    let mut p_no_flags = p.clone();
    p_no_flags.flags = None;
    assert_eq!(
        no_flags.check("d", &downloading, &p_no_flags, &ctx(0)).action,
        PeerAction::NoAction
    );
    assert_eq!(
        no_flags.check("d", &downloading, &p_no_flags, &ctx(1_000_000)).action,
        PeerAction::NoAction,
        "不支持 Peer Flags 的下载器不保护下载任务"
    );

    // 模式 0 + 下载任务 + 兴趣系统工作 -> 忽略
    let mut interested = p.clone();
    interested.flags = Some("d u".into());
    assert_eq!(
        no_flags.check("d", &downloading, &interested, &ctx(0)).action,
        PeerAction::NoAction
    );

    // 模式 1（仅保护做种） + 下载任务 -> 直接放行
    let seeding_only = IdleConnectionDosProtection::new(IdleProtectionSettings {
        protect_mode: ProtectionMode::AlwaysSeeding,
        ..idle_settings()
    });
    assert_eq!(
        seeding_only.check("d", &downloading, &p, &ctx(1_000_000)).action,
        PeerAction::NoAction
    );

    // 模式 2 -> 无论 flags 与否都保护下载任务
    let always = IdleConnectionDosProtection::new(IdleProtectionSettings {
        protect_mode: ProtectionMode::AlwaysSeedingAndDownloading,
        ..idle_settings()
    });
    let mut p2 = p.clone();
    p2.flags = None;
    assert_eq!(always.check("d", &downloading, &p2, &ctx(0)).action, PeerAction::NoAction);
    assert_eq!(always.check("d", &downloading, &p2, &ctx(400_000)).action, PeerAction::Ban);
}

#[test]
fn idle_protection_drops_peers_not_seen_for_five_checks() {
    let m = IdleConnectionDosProtection::new(idle_settings());
    let seeding = torrent_with_progress(1.0);
    let p = idle_peer();
    assert_eq!(m.check("d", &seeding, &p, &ctx(0)).action, PeerAction::NoAction);
    assert_eq!(m.tracked_peers(), 1);
    for _ in 0..6 {
        m.on_peers_retrieved(&[]);
    }
    assert_eq!(m.tracked_peers(), 0, "连续 5 次未出现则清理");
}

// ---------- auto-range-ban ----------

fn auto_range_ban(ban_list: &Arc<Mutex<BanList>>) -> AutoRangeBan {
    // 上游 profile.yml 默认：ipv4: 30、ipv6: 48、ban-duration: 604800000
    AutoRangeBan::new(30, 48, 604_800_000, ban_list.clone())
}

#[test]
fn auto_range_ban_chain_bans_peers_in_the_same_prefix() {
    let ban_list = Arc::new(Mutex::new(BanList::new()));
    ban_list
        .lock()
        .unwrap()
        .add("1.2.3.4", 0, "peer-id-blacklist", false);
    let m = auto_range_ban(&ban_list);
    let t = torrent();

    // 与已封禁的 1.2.3.4 同属 /30 -> 连锁封禁
    let r = m.check("d", &t, &peer("1.2.3.5", "-qB4700-xxxxxxxxxxxx"), &ctx(0));
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(r.ban_duration_ms, 604_800_000);
    let t9 = Translator::embedded();
    assert_eq!(t9.render(r.rule_key.as_ref().unwrap(), "zh_cn"), "IPv4/30");
    assert_eq!(
        t9.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "IP 地址 1.2.3.5 与另一个已封禁的 IP 地址 1.2.3.4 处于同一封禁区间 1.2.3.4/30 内，执行连锁封禁操作。"
    );
    assert_eq!(r.data["relatedBannedAddress"], "1.2.3.4");

    // 不同 /30 -> 不封
    assert_eq!(
        m.check("d", &t, &peer("1.2.3.8", "-qB4700-xxxxxxxxxxxx"), &ctx(0)).action,
        PeerAction::NoAction
    );
    // 自身已在封禁表中 -> 跳过（由其它模块负责）
    assert_eq!(
        m.check("d", &t, &peer("1.2.3.4", "-qB4700-xxxxxxxxxxxx"), &ctx(0)).action,
        PeerAction::NoAction
    );
    // 不同地址族 -> 不封
    assert_eq!(
        m.check("d", &t, &peer("2001:db8::1", "-qB4700-xxxxxxxxxxxx"), &ctx(0)).action,
        PeerAction::NoAction
    );
}

#[test]
fn auto_range_ban_ignores_disconnect_only_bans_and_handshaking_peers() {
    let ban_list = Arc::new(Mutex::new(BanList::new()));
    // PCB 快速测试的临时封禁不参与连锁
    ban_list
        .lock()
        .unwrap()
        .add("9.9.9.9", 0, "progress-cheat-blocker", true);
    let m = auto_range_ban(&ban_list);
    let t = torrent();
    assert_eq!(
        m.check("d", &t, &peer("9.9.9.8", "-qB4700-xxxxxxxxxxxx"), &ctx(0)).action,
        PeerAction::NoAction,
        "banForDisconnect 记录不参与连锁封禁"
    );

    // 握手中返回 pass（上游此处用的是 pass() 而不是 handshaking()）
    let mut hs = peer("1.2.3.5", "-qB4700-xxxxxxxxxxxx");
    hs.up_speed = 0;
    hs.dl_speed = 0;
    let r = m.check("d", &t, &hs, &ctx(0));
    assert_eq!(r.action, PeerAction::NoAction);
    assert_eq!(r.reason_key.as_ref().unwrap().key, "Check passed");
}

#[test]
fn auto_range_ban_uses_ipv6_prefix() {
    let ban_list = Arc::new(Mutex::new(BanList::new()));
    ban_list
        .lock()
        .unwrap()
        .add("2001:db8:1:2::1", 0, "peer-id-blacklist", false);
    let m = auto_range_ban(&ban_list);
    let t = torrent();
    // 同 /48 -> 连锁；注意上游把 /48 的第 4 段清零后为 2001:db8:1::/48
    let r = m.check("d", &t, &peer("2001:db8:1:2::9", "-qB4700-xxxxxxxxxxxx"), &ctx(0));
    assert_eq!(r.action, PeerAction::Ban);
    let t9 = Translator::embedded();
    assert_eq!(t9.render(r.rule_key.as_ref().unwrap(), "zh_cn"), "IPv6/48");
    // 不同 /48 -> 不封
    assert_eq!(
        m.check("d", &t, &peer("2001:db8:2::9", "-qB4700-xxxxxxxxxxxx"), &ctx(0)).action,
        PeerAction::NoAction
    );
}

// ---------- multi-dialing-blocker ----------

fn multi_dialing() -> MultiDialingBlocker {
    // 上游 profile.yml 默认值
    MultiDialingBlocker::new(MultiDialingSettings {
        ban_duration_ms: 1_296_000_000,
        subnet_mask_length: 24,
        subnet_mask_v6_length: 56,
        tolerate_num_ipv4: 2,
        tolerate_num_ipv6: 5,
        cache_lifespan_ms: 86_400_000,
        keep_hunting: false,
        keep_hunting_time_ms: 0,
    })
}

#[test]
fn multi_dialing_blocks_the_ip_that_exceeds_the_tolerance() {
    let m = multi_dialing();
    let t = torrent();
    // 同一 /24 内的第 1、2 个 IP 放行
    assert_eq!(
        m.check("d", &t, &peer("10.0.0.1", "-qB4700-aaaaaaaaaaaa"), &ctx(0)).action,
        PeerAction::NoAction
    );
    assert_eq!(
        m.check("d", &t, &peer("10.0.0.2", "-qB4700-bbbbbbbbbbbb"), &ctx(1_000)).action,
        PeerAction::NoAction
    );
    // 第 3 个 IP 超过 tolerate-num-ipv4(2) -> 封禁
    let r = m.check("d", &t, &peer("10.0.0.3", "-qB4700-cccccccccccc"), &ctx(2_000));
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(r.ban_duration_ms, 1_296_000_000);
    let tr = Translator::embedded();
    assert_eq!(tr.render(r.rule_key.as_ref().unwrap(), "zh_cn"), "检测到多拨下载");
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "发现多拨下载，请持续关注，子网：10.0.0.0/24，触发IP：10.0.0.3"
    );
    assert_eq!(r.data["subnetPeersSize"], 3);
}

#[test]
fn multi_dialing_counts_distinct_peers_per_subnet_and_torrent() {
    let m = multi_dialing();
    let t = torrent();
    // 同一 IP 重复上报只算一个
    for now in [0, 1_000, 2_000] {
        assert_eq!(
            m.check("d", &t, &peer("10.0.0.1", "-qB4700-aaaaaaaaaaaa"), &ctx(now)).action,
            PeerAction::NoAction
        );
    }
    // 另一个 /24 -> 独立计数
    assert_eq!(
        m.check("d", &t, &peer("10.0.1.1", "-qB4700-aaaaaaaaaaaa"), &ctx(3_000)).action,
        PeerAction::NoAction
    );
    assert_eq!(
        m.check("d", &t, &peer("10.0.1.2", "-qB4700-aaaaaaaaaaaa"), &ctx(4_000)).action,
        PeerAction::NoAction
    );

    // 超时后缓存过期：同一 /24 旧记录不再计数
    let later = 86_400_000 + 10_000;
    assert_eq!(
        m.check("d", &t, &peer("10.0.2.1", "-qB4700-aaaaaaaaaaaa"), &ctx(later)).action,
        PeerAction::NoAction
    );
    assert_eq!(
        m.check("d", &t, &peer("10.0.2.2", "-qB4700-aaaaaaaaaaaa"), &ctx(later + 1_000)).action,
        PeerAction::NoAction
    );
    assert_eq!(
        m.check("d", &t, &peer("10.0.2.3", "-qB4700-aaaaaaaaaaaa"), &ctx(later + 2_000)).action,
        PeerAction::Ban
    );
}

#[test]
fn multi_dialing_uses_ipv6_tolerance_and_prefix() {
    let m = multi_dialing();
    let t = torrent();
    // IPv6 /56 容忍 5 个
    for (i, ip) in ["2001:db8:1::1", "2001:db8:1::2", "2001:db8:1::3", "2001:db8:1::4", "2001:db8:1::5"]
        .iter()
        .enumerate()
    {
        assert_eq!(
            m.check("d", &t, &peer(ip, "-qB4700-aaaaaaaaaaaa"), &ctx(i as i64 * 1_000)).action,
            PeerAction::NoAction,
            "{ip} 应在容忍范围内"
        );
    }
    let r = m.check("d", &t, &peer("2001:db8:1::6", "-qB4700-aaaaaaaaaaaa"), &ctx(6_000));
    assert_eq!(r.action, PeerAction::Ban);
    let tr = Translator::embedded();
    assert!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn").contains("2001:db8:1::/56"),
        "IPv6 前缀长度应按 subnet-mask-v6-length"
    );
}

#[test]
fn multi_dialing_handshaking_peers_are_skipped() {
    let m = multi_dialing();
    let t = torrent();
    let mut hs = peer("10.0.0.1", "-qB4700-aaaaaaaaaaaa");
    hs.up_speed = 0;
    hs.dl_speed = 0;
    let r = m.check("d", &t, &hs, &ctx(0));
    assert_eq!(r.action, PeerAction::NoAction);
    assert_eq!(r.reason_key.as_ref().unwrap().key, "Peer handshaking");
}

/// `keep-hunting`：追猎窗口内，即使子网计数已因缓存过期归零也会继续封禁。
#[test]
fn multi_dialing_keep_hunting_bans_following_peers_within_window() {
    let m = MultiDialingBlocker::new(MultiDialingSettings {
        ban_duration_ms: 1_296_000_000,
        subnet_mask_length: 24,
        subnet_mask_v6_length: 56,
        tolerate_num_ipv4: 2,
        tolerate_num_ipv6: 5,
        cache_lifespan_ms: 100_000,
        keep_hunting: true,
        keep_hunting_time_ms: 600_000,
    });
    let t = torrent();
    for (i, ip) in ["10.0.0.1", "10.0.0.2"].iter().enumerate() {
        m.check("d", &t, &peer(ip, "-qB4700-aaaaaaaaaaaa"), &ctx(i as i64));
    }
    // 第 3 个 IP 触发多拨 -> 进入追猎名单
    let trigger = m.check("d", &t, &peer("10.0.0.3", "-qB4700-aaaaaaaaaaaa"), &ctx(0));
    assert_eq!(trigger.action, PeerAction::Ban);

    // 缓存过期后子网计数归零（<= 容忍值），但追猎窗口仍有效 -> 按追猎封禁
    let r = m.check("d", &t, &peer("10.0.0.9", "-qB4700-aaaaaaaaaaaa"), &ctx(100_001));
    assert_eq!(r.action, PeerAction::Ban);
    let tr = Translator::embedded();
    assert_eq!(tr.render(r.rule_key.as_ref().unwrap(), "zh_cn"), "多拨持续追踪");
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "触发多拨追猎名单，子网：10.0.0.0/24，触发IP：10.0.0.9"
    );

    // 追猎窗口过期（距上次追猎命中超过 keep-hunting-time）-> 不再因追猎封禁
    let expired = m.check("d", &t, &peer("10.0.0.77", "-qB4700-aaaaaaaaaaaa"), &ctx(100_001 + 600_000));
    assert_eq!(expired.action, PeerAction::NoAction);
}
