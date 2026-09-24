//! L2：规则模块对单个 peer 的判定（SPEC 第 5 节 [GOLDEN]），含 PCB 多波状态机。

use pbh_core::model::{PeerData, TorrentData};
use pbh_core::module::{CheckContext, PeerAction, RuleModule};
use pbh_core::modules::progress_cheat::{PcbConfig, ProgressCheatBlocker};
use pbh_core::modules::{IpBlacklist, StringBlacklist};

fn peer(
    ip: &str,
    port: u16,
    peer_id: Option<&str>,
    client: Option<&str>,
    up_speed: i64,
    uploaded: i64,
    progress: f64,
) -> PeerData {
    PeerData {
        client_name: client.map(|s| s.to_string()),
        peer_id: peer_id.map(|s| s.to_string()),
        dl_speed: 1000,
        downloaded: 1000,
        up_speed,
        uploaded,
        progress,
        flags: Some("d u X".into()),
        ip: ip.into(),
        port,
        raw_ip: format!("{ip}:{port}"),
        connection: Some("uTP".into()),
    }
}

fn torrent(size: i64, piece_size: i64, pieces_have: i64) -> TorrentData {
    TorrentData {
        hash: "h".into(),
        name: "t".into(),
        progress: 1.0,
        total_size: size,
        piece_size,
        pieces_have,
        completed_override: None,
        dlspeed: 0,
        upspeed: 0,
        is_private: Some(false),
    }
}

fn ctx(now_ms: i64) -> CheckContext {
    CheckContext {
        now_ms,
        features: vec!["UNBAN_IP".into(), "BAN_IP".into()],
    }
}

#[test]
fn peer_id_module_bans_and_handshakes() {
    let m = StringBlacklist::peer_id();
    let t = torrent(1_000_000_000, 0, 0);
    let bad = peer("9.9.9.9", 1, Some("-hp001-x"), Some(""), 10, 10, 0.1);
    assert_eq!(m.check("d", &t, &bad, &ctx(0)).action, PeerAction::Ban);
    // 握手中（速度皆 0）且 peerId 为空 -> NO_ACTION
    let mut hs = bad.clone();
    hs.up_speed = 0;
    hs.dl_speed = 0;
    hs.peer_id = None;
    assert_eq!(m.check("d", &t, &hs, &ctx(0)).action, PeerAction::NoAction);
}

/// Java 的前置条件是 `isHandShaking(peer) && (id == null || id.isBlank())`：
/// **只有**「握手中且标识为空」才跳过。若早期实现只要握手中就跳过，
/// 吸血客户端只需把速度压成 0 即可绕过 PeerId/ClientName 黑名单。
#[test]
fn blacklist_modules_still_check_handshaking_peers_that_carry_an_id() {
    let t = torrent(1_000_000_000, 0, 0);
    let mut hs_bad = peer("9.9.9.9", 1, Some("-hp001-x"), Some(""), 0, 0, 0.0);
    hs_bad.dl_speed = 0;
    assert_eq!(
        StringBlacklist::peer_id()
            .check("d", &t, &hs_bad, &ctx(0))
            .action,
        PeerAction::Ban,
        "握手中但带 peerId：仍需判定"
    );

    let mut hs_client = peer(
        "7.7.7.7",
        2,
        Some("-qB5000-000000000000"),
        Some("xfplay/9.0"),
        0,
        0,
        0.0,
    );
    hs_client.dl_speed = 0;
    assert_eq!(
        StringBlacklist::client_name()
            .check("d", &t, &hs_client, &ctx(0))
            .action,
        PeerAction::Ban,
        "握手中但带 clientName：仍需判定"
    );

    // 空白字符视为 blank，同样走握手跳过
    let mut hs_blank = hs_bad.clone();
    hs_blank.peer_id = Some("   ".into());
    assert_eq!(
        StringBlacklist::peer_id()
            .check("d", &t, &hs_blank, &ctx(0))
            .action,
        PeerAction::NoAction
    );
}

#[test]
fn client_module_bans_xfplay() {
    let m = StringBlacklist::client_name();
    let t = torrent(1_000_000_000, 0, 0);
    let p = peer(
        "7.7.7.7",
        2,
        Some("-qB5000-000000000000"),
        Some("xfplay/9.0"),
        10,
        10,
        0.2,
    );
    let r = m.check("d", &t, &p, &ctx(0));
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(r.module, "client-name-blacklist");
}

#[test]
fn ip_blacklist_cidr_and_port() {
    let m = IpBlacklist::new(&["1.2.3.0/24".to_string()], &[4444], 0);
    let t = torrent(1_000_000_000, 0, 0);
    let cidr = peer("1.2.3.99", 100, Some("-qB-x"), Some("qB"), 10, 10, 0.1);
    assert_eq!(m.check("d", &t, &cidr, &ctx(0)).action, PeerAction::Ban);
    let port = peer("8.8.8.8", 4444, Some("-qB-x"), Some("qB"), 10, 10, 0.1);
    assert_eq!(m.check("d", &t, &port, &ctx(0)).action, PeerAction::Ban);
    let clean = peer("9.9.9.9", 5555, Some("-qB-x"), Some("qB"), 10, 10, 0.1);
    assert_eq!(
        m.check("d", &t, &clean, &ctx(0)).action,
        PeerAction::NoAction
    );
}

#[test]
fn pcb_excessive_download_banned_first_wave() {
    // 关闭快速 PCB 测试，隔离「过量下载」分支
    let pcb = ProgressCheatBlocker::new(PcbConfig {
        fast_pcb_test_percentage: 0.0,
        ..PcbConfig::default()
    });
    let t = torrent(1_000_000_000, 0, 0);
    // 上传量 20 亿 > 阈值 15 亿
    let p = peer(
        "6.6.6.6",
        7,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        2_000_000_000,
        0.05,
    );
    let r = pcb.check("d", &t, &p, &ctx(0));
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(r.rule, "excessiveMaxDownloadThreshold");
}

/// 上游 profile.yml 的 PCB 默认参数必须逐字对齐：
/// `fast-pcb-test-percentage: 0.1` / `fast-pcb-test-block-duration: 15000` / `max-wait-duration: 30000`
/// / `ipv4-prefix-length: 32` / `ipv6-prefix-length: 56` / `maximum-difference: 0.1` / `rewind-maximum-difference: 0.07`。
#[test]
fn pcb_defaults_match_upstream_profile() {
    let c = PcbConfig::default();
    assert_eq!(c.torrent_minimum_size, 50_000_000);
    assert_eq!(c.maximum_difference, 0.1);
    assert_eq!(c.rewind_maximum_difference, 0.07);
    assert!(c.block_excessive_clients);
    assert_eq!(c.excessive_threshold, 1.5);
    assert_eq!(c.ipv4_prefix_length, 32);
    assert_eq!(c.ipv6_prefix_length, 56);
    assert_eq!(c.ban_duration_ms, 2_592_000_000);
    assert_eq!(c.max_wait_duration_ms, 30_000);
    assert_eq!(c.fast_pcb_test_percentage, 0.1, "上游默认启用快速 PCB 测试");
    assert_eq!(c.fast_pcb_test_block_duration_ms, 15_000);
}

/// 快速 PCB 测试：达到 10% 上传量后主动断开一次（BAN_FOR_DISCONNECT），且只触发一次。
#[test]
fn pcb_fast_test_fires_once_then_stops() {
    let pcb = ProgressCheatBlocker::default();
    let t = torrent(1_000_000_000, 0, 0);
    // 已上传 20% >= 10% 阈值
    let p = peer(
        "6.6.6.6",
        20,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        200_000_000,
        0.20,
    );
    let r1 = pcb.check("d", &t, &p, &ctx(0));
    assert_eq!(r1.action, PeerAction::BanForDisconnect);
    assert_eq!(r1.rule, "fastPcbTest");
    assert_eq!(
        r1.ban_duration_ms, 15_000,
        "封禁时长取 fast-pcb-test-block-duration"
    );
    // 已测试过：不再触发快速测试
    let p2 = peer(
        "6.6.6.6",
        20,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        300_000_000,
        0.20,
    );
    let r2 = pcb.check("d", &t, &p2, &ctx(5_000));
    assert_eq!(r2.action, PeerAction::NoAction, "fastPcbTest 只执行一次");
}

/// 上游 `CacheKeyAddr(downloader, torrentId, peerAddressIp)` **不含端口**：
/// 同一 IP 的不同端口共用同一个 addr 实体，避免重复累加上传增量。
#[test]
fn pcb_shares_address_entity_across_ports_of_same_ip() {
    let pcb = ProgressCheatBlocker::new(PcbConfig {
        fast_pcb_test_percentage: 0.0,
        ..PcbConfig::default()
    });
    let t = torrent(1_000_000_000, 0, 0);
    // 同一 IP、两个不同端口，各自上报 9 亿上传量（90%）
    let a = peer(
        "6.6.6.6",
        1000,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        900_000_000,
        0.90,
    );
    let b = peer(
        "6.6.6.6",
        2000,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        900_000_000,
        0.90,
    );
    assert_eq!(pcb.check("d", &t, &a, &ctx(0)).action, PeerAction::NoAction);
    // 若端口被计入缓存键，第二个端口的增量会被重复累加（9e8+9e8=1.8e9 > 1.5e9 阈值）而误封
    let rb = pcb.check("d", &t, &b, &ctx(0));
    assert_eq!(
        rb.action,
        PeerAction::NoAction,
        "同 IP 不同端口必须共用 addr 实体，不得重复累加（rule={}）",
        rb.rule
    );
}

#[test]
fn pcb_desync_waits_one_window_then_bans() {
    let pcb = ProgressCheatBlocker::new(PcbConfig {
        fast_pcb_test_percentage: 0.0,
        ..PcbConfig::default()
    });
    // completed_size = -1（piece_size=0），避免过量分支干扰
    let t = torrent(1_000_000_000, 0, 0);
    // 实际已上传 40%（4 亿），客户端只报 20%，差值 20%
    let p = peer(
        "6.6.6.6",
        8,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        400_000_000,
        0.20,
    );
    // 第一次：开窗，不封
    let r1 = pcb.check("d", &t, &p, &ctx(0));
    assert_eq!(
        r1.action,
        PeerAction::NoAction,
        "first wave should open delay window"
    );
    // 窗口到期后（>30s）：封禁 deSyncDifference
    let r2 = pcb.check("d", &t, &p, &ctx(31_000));
    assert_eq!(r2.action, PeerAction::Ban);
    assert_eq!(r2.rule, "deSyncDifference");
}

#[test]
fn pcb_rewind_banned() {
    let pcb = ProgressCheatBlocker::new(PcbConfig {
        fast_pcb_test_percentage: 0.0,
        ..PcbConfig::default()
    });
    let t = torrent(1_000_000_000, 0, 0);
    // 第一波：仅上传 1%，客户端报 90%，通过；记录 lastReportProgress=0.9
    let p1 = peer(
        "6.6.6.6",
        9,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        10_000_000,
        0.90,
    );
    let r1 = pcb.check("d", &t, &p1, &ctx(0));
    assert_eq!(r1.action, PeerAction::NoAction);
    // 第二波：实际上传 50%（computed 0.5），客户端进度倒退到 45%（差值仅 5% 不触发 desync），
    // 但相对上次 90% 倒退 45% > 7% -> rewindProgress
    let p2 = peer(
        "6.6.6.6",
        9,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        500_000_000,
        0.45,
    );
    let r2 = pcb.check("d", &t, &p2, &ctx(5_000));
    assert_eq!(
        r2.action,
        PeerAction::Ban,
        "expected rewind ban, got {:?} {}",
        r2.action,
        r2.reason
    );
    assert_eq!(r2.rule, "rewindProgress");
}

#[test]
fn pcb_small_file_skipped() {
    let pcb = ProgressCheatBlocker::default();
    // 小于 minimum-size(50MB)
    let t = torrent(10_000_000, 0, 0);
    let p = peer(
        "6.6.6.6",
        10,
        Some("-qB4500-y"),
        Some("qB"),
        1000,
        9_000_000,
        0.0,
    );
    assert_eq!(pcb.check("d", &t, &p, &ctx(0)).action, PeerAction::NoAction);
}

#[test]
fn pcb_not_uploading_skipped() {
    let pcb = ProgressCheatBlocker::default();
    let t = torrent(1_000_000_000, 0, 0);
    // up_speed=0 且 uploaded=0 -> 未在向其上传
    let p = peer("6.6.6.6", 11, Some("-qB4500-y"), Some("qB"), 0, 0, 0.0);
    assert_eq!(pcb.check("d", &t, &p, &ctx(0)).action, PeerAction::NoAction);
}

#[test]
fn pcb_rewind_disabled_respected() {
    // 关闭倒退检测；同时关闭快速 PCB 测试以隔离该分支
    let cfg = PcbConfig {
        rewind_maximum_difference: -1.0,
        fast_pcb_test_percentage: 0.0,
        ..PcbConfig::default()
    };
    let pcb = ProgressCheatBlocker::new(cfg);
    let t = torrent(1_000_000_000, 0, 0);
    let p1 = peer(
        "6.6.6.6",
        12,
        Some("-qB4500-y"),
        Some("qB"),
        1000,
        10_000_000,
        0.90,
    );
    pcb.check("d", &t, &p1, &ctx(0));
    let p2 = peer(
        "6.6.6.6",
        12,
        Some("-qB4500-y"),
        Some("qB"),
        1000,
        500_000_000,
        0.45,
    );
    // computed 0.5 vs reported 0.45 差值 5% 不触发 desync；rewind 已关闭 -> 不封
    assert_eq!(
        pcb.check("d", &t, &p2, &ctx(5_000)).action,
        PeerAction::NoAction
    );
}

/// Java `DigestionSession.extractFromLastOrgan` 的聚合规则：
/// **PeerAction 等级更高者胜**（SKIP > BAN > BAN_FOR_DISCONNECT > NO_ACTION），
/// 等级相同时取**更长**的 ban 时长。因此 Rust 的「首个命中模块即短路」是错误的。
#[test]
fn pipeline_aggregates_by_action_rank_then_longest_duration() {
    let mut p = pbh_core::pipeline::Pipeline::default();
    // 故意把时长更短的 IP 黑名单（3 天）放在 PCB（30 天）之前
    p.add_module(Box::new(IpBlacklist::new(
        &["1.2.3.0/24".into()],
        &[],
        259_200_000,
    )));
    p.add_module(Box::new(ProgressCheatBlocker::new(PcbConfig {
        fast_pcb_test_percentage: 0.0,
        ..PcbConfig::default()
    })));
    let t = torrent(1_000_000_000, 0, 0);
    // 同时命中 IP 黑名单与「过量下载」
    let victim = peer(
        "1.2.3.9",
        100,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        2_000_000_000,
        0.05,
    );
    match p.evaluate("d", &t, &victim, &ctx(0)) {
        pbh_core::pipeline::Decision::Ban(r) => {
            assert_eq!(
                r.module, "progress-cheat-blocker",
                "等级相同取更长 ban 时长（30 天 > 3 天）"
            );
            assert_eq!(r.ban_duration_ms, 2_592_000_000);
        }
        other => panic!("expected Ban, got {other:?}"),
    }
}

/// 模块产出的文案键必须与上游 `Lang.*` 一致，且渲染结果逐字等于上游 UI 文案。
#[test]
fn module_results_carry_upstream_translation_keys() {
    use pbh_core::i18n::Translator;

    let t = Translator::embedded();
    let tr = torrent(1_000_000_000, 0, 0);

    // PeerId：rule = 命中规则的 matcherName()；reason = MODULE_CNB_MATCH_CLIENT_NAME(comment)
    // （上游 PeerIdBlacklist 复用 ClientName 的文案键，这里同样保留该行为）
    let bad = peer(
        "9.9.9.9",
        1,
        Some("-hp001-x"),
        Some("qBittorrent"),
        10,
        10,
        0.1,
    );
    let r = StringBlacklist::peer_id().check("d", &tr, &bad, &ctx(0));
    let rule = r.rule_key.clone().expect("rule_key");
    let reason = r.reason_key.clone().expect("reason_key");
    assert_eq!(rule.key, "MATCH_STRING_STARTS_WITH");
    assert_eq!(t.render(&rule, "zh_cn"), "字符串开头: -hp");
    assert_eq!(reason.key, "MODULE_CNB_MATCH_CLIENT_NAME");
    assert_eq!(
        t.render(&reason, "zh_cn"),
        "匹配 ClientName (UserAgent): 字符串开头: -hp"
    );

    // IP 黑名单：rule = IP_BLACKLIST_CIDR_RULE(命中的网段)
    let ip = IpBlacklist::new(&["1.2.3.0/24".to_string()], &[], 0);
    let hit = peer("1.2.3.99", 100, Some("-qB-x"), Some("qB"), 10, 10, 0.1);
    let r = ip.check("d", &tr, &hit, &ctx(0));
    assert_eq!(r.rule_key.as_ref().unwrap().key, "IP_BLACKLIST_CIDR_RULE");
    assert_eq!(
        t.render(r.rule_key.as_ref().unwrap(), "zh_cn"),
        "IP/CIDR 规则: 1.2.3.0/24"
    );
    assert_eq!(
        t.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "匹配 IP 规则: 1.2.3.0/24"
    );

    // PCB deSync：reason 参数按 DecimalFormat("0.00%") 格式化
    let pcb = ProgressCheatBlocker::new(PcbConfig {
        fast_pcb_test_percentage: 0.0,
        ..PcbConfig::default()
    });
    let p = peer(
        "6.6.6.6",
        8,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        400_000_000,
        0.20,
    );
    pcb.check("d", &tr, &p, &ctx(0));
    let r = pcb.check("d", &tr, &p, &ctx(31_000));
    assert_eq!(
        r.rule_key.as_ref().unwrap().key,
        "PCB_RULE_REACHED_MAX_DIFFERENCE"
    );
    assert_eq!(
        t.render(r.rule_key.as_ref().unwrap(), "zh_cn"),
        "已超过允许的进度差异最大值"
    );
    assert_eq!(
        t.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "客户端进度：20.00%，实际进度：40.00%，差值：20.00%"
    );
}

/// `StructuredData.rule` 必须是命中规则的 `metadata()`（规则串本身，如 `-hp`），
/// 而非 peer 的 peerId/clientName 值。早期实现错把 peer 值写进了 `data.rule`，
/// 导致下游 `history.rule_name` 字段与上游不一致。
#[test]
fn string_blacklist_data_rule_is_matched_rule_metadata() {
    let t = torrent(1_000_000_000, 0, 0);
    let bad = peer(
        "9.9.9.9",
        1,
        Some("-hp001-x"),
        Some("qBittorrent"),
        10,
        10,
        0.1,
    );
    let r = StringBlacklist::peer_id().check("d", &t, &bad, &ctx(0));
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(
        r.data["rule"].as_str(),
        Some("-hp"),
        "data.rule 应为命中规则的 metadata（规则串），不是 peer 的 peerId"
    );
    assert_ne!(r.data["rule"].as_str(), Some("-hp001-x"));
}

/// PCB 状态持久化（上游 `enable-persist: true`）：跨「进程重启」保留解封窗口与计数器。
///
/// 上游把 `PCBRangeEntity` / `PCBAddressEntity` 落库（`pcb_range` / `pcb_addr`），
/// 重启后加载，因此「已开窗等待」的 peer 在重启后仍然只有一次宽限，而不是重新开窗。
#[test]
fn pcb_state_survives_a_restart_via_persisted_rows() {
    use pbh_core::modules::progress_cheat::PcbPersistRow;

    let cfg = PcbConfig {
        fast_pcb_test_percentage: 0.0,
        ..PcbConfig::default()
    };
    let t = torrent(1_000_000_000, 0, 0);
    let p = peer(
        "6.6.6.6",
        4321,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        400_000_000,
        0.20,
    );

    let first = ProgressCheatBlocker::new(cfg.clone());
    // 首次：差值 20% -> 开窗等待，不封禁
    assert_eq!(
        first.check("d", &t, &p, &ctx(0)).action,
        PeerAction::NoAction
    );

    let rows: Vec<PcbPersistRow> = first.flush_dirty();
    assert_eq!(rows.len(), 2, "addr 与 range 各一行");
    assert!(
        rows.iter().all(|r| r.ban_delay_window_end_ms > 0),
        "窗口状态需要被持久化"
    );
    let addr_row = rows.iter().find(|r| r.kind.is_addr()).expect("addr row");
    assert_eq!(addr_row.key, "6.6.6.6");
    assert_eq!(addr_row.port, 4321, "port 取该 IP 首次出现时的端口");
    assert_eq!(addr_row.downloader_id, "d");
    assert_eq!(addr_row.torrent_id, "h");

    // 重启：新实例 + 载入持久化行 -> 窗口仍然有效，窗口到期后直接封禁
    let restored = ProgressCheatBlocker::new(cfg);
    restored.load_persisted(rows);
    let r = restored.check("d", &t, &p, &ctx(31_000));
    assert_eq!(r.action, PeerAction::Ban, "重启后不应重新开窗");
    assert_eq!(r.rule, "deSyncDifference");

    // 对照：未载入持久化状态的新实例只会再次开窗
    let fresh = ProgressCheatBlocker::new(PcbConfig {
        fast_pcb_test_percentage: 0.0,
        ..PcbConfig::default()
    });
    assert_eq!(
        fresh.check("d", &t, &p, &ctx(31_000)).action,
        PeerAction::NoAction
    );
}

/// 已经落库过的状态不会重复落库（对齐上游 `isDirty` 语义）。
#[test]
fn pcb_flush_only_returns_changed_entities() {
    let pcb = ProgressCheatBlocker::new(PcbConfig {
        fast_pcb_test_percentage: 0.0,
        ..PcbConfig::default()
    });
    let t = torrent(1_000_000_000, 0, 0);
    let p = peer(
        "6.6.6.6",
        1,
        Some("-qB4500-y"),
        Some("qB"),
        1000,
        100_000_000,
        0.5,
    );
    pcb.check("d", &t, &p, &ctx(0));
    assert_eq!(pcb.flush_dirty().len(), 2);
    assert!(pcb.flush_dirty().is_empty(), "未变更的实体不应重复落库");

    // 再次判定 -> 重新标记为脏
    pcb.check("d", &t, &p, &ctx(5_000));
    assert_eq!(pcb.flush_dirty().len(), 2);
}

/// 模块注册顺序对齐上游 `PeerBanHelper.registerModules()`：
/// `IPBlackList → PeerIdBlacklist → ClientNameBlacklist → (ExpressionRule) → ProgressCheatBlocker
/// → MultiDialingBlocker → AutoRangeBan`。
/// 该顺序是「等级与时长完全并列」时的最终裁决依据。
#[test]
fn default_pipeline_module_order_matches_upstream_registration() {
    let pipeline = pbh_core::default_pipeline();
    let names: Vec<&str> = pipeline.modules.iter().map(|m| m.config_name()).collect();
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
            "ip-address-blocker-rules",
            "anti-vampire"
        ]
    );
}

/// 等级不同时以等级为准：PCB 快速测试的 BAN_FOR_DISCONNECT 不得压过真正意义上的 BAN。
#[test]
fn pipeline_prefers_ban_over_ban_for_disconnect() {
    let mut p = pbh_core::pipeline::Pipeline::default();
    p.add_module(Box::new(ProgressCheatBlocker::default())); // 快速测试开启 -> BAN_FOR_DISCONNECT
    p.add_module(Box::new(IpBlacklist::new(
        &["1.2.3.0/24".into()],
        &[],
        259_200_000,
    )));
    let t = torrent(1_000_000_000, 0, 0);
    let victim = peer(
        "1.2.3.9",
        100,
        Some("-qB4500-y"),
        Some("qBittorrent/4.5.0"),
        1000,
        200_000_000,
        0.20,
    );
    match p.evaluate("d", &t, &victim, &ctx(0)) {
        pbh_core::pipeline::Decision::Ban(r) => {
            assert_eq!(r.module, "ip-address-blocker");
            assert_eq!(r.ban_duration_ms, 259_200_000);
        }
        other => panic!("expected Ban, got {other:?}"),
    }
}

// ---------- string_blacklist 细粒度（REGEX / LENGTH / data.type 区分） ----------

use pbh_core::rule::RuleSet;

/// REGEX 规则（Java `matcher.matches()` 整段匹配，锚定到首尾）：命中后 `data.type` 为
/// `peerId`，`data.rule` 为**原始正则模式**（非 peer 值）。
#[test]
fn string_blacklist_regex_mode_uses_rule_metadata() {
    let rules = RuleSet::from_json_text(&[r#"{"method":"REGEX","content":"-hp[0-9]+"}"#.to_string()])
        .expect("regex rule parse");
    let m = StringBlacklist::peer_id_with(rules, 123_000);
    let t = torrent(1_000_000_000, 0, 0);
    // `-hp[0-9]+` 整段匹配（无后缀），与上游 matches() 语义一致
    let p = peer(
        "9.9.9.9",
        1,
        Some("-hp001"),
        Some("qBittorrent"),
        10,
        10,
        0.1,
    );
    let r = m.check("d", &t, &p, &ctx(0));
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(r.data["type"], "peerId", "peer_id 模块的 data.type 必须是 peerId");
    assert_eq!(
        r.data["rule"].as_str(),
        Some("-hp[0-9]+"),
        "data.rule 为原始正则模式，不是 peer 值"
    );
    assert_ne!(r.data["rule"].as_str(), Some("-hp001"));

    // 带后缀的 peer_id 不满足整段匹配 -> 不命中
    let suffix = peer(
        "9.9.9.8",
        1,
        Some("-hp001-x"),
        Some("qBittorrent"),
        10,
        10,
        0.1,
    );
    assert_eq!(
        m.check("d", &t, &suffix, &ctx(0)).action,
        PeerAction::NoAction,
        "REGEX 为整段匹配，带后缀的 id 不命中"
    );
}

/// LENGTH 规则（按 UTF-16 码元，对齐 Java `String.length()`）：长度落在区间内才命中。
#[test]
fn string_blacklist_length_mode_bans_ids_in_range() {
    // 封禁长度在 [10, 100] 码元的 peer_id（含边界）
    let rules = RuleSet::from_json_text(&[r#"{"method":"LENGTH","min":10,"max":100}"#.to_string()])
        .expect("length rule parse");
    let m = StringBlacklist::peer_id_with(rules, 0);
    let t = torrent(1_000_000_000, 0, 0);
    // 20 个 ASCII 码元，落在 [10,100] -> 封禁
    let long = peer(
        "9.9.9.9",
        1,
        Some("-qB5000-000000000000"),
        Some("qB"),
        10,
        10,
        0.1,
    );
    assert_eq!(m.check("d", &t, &long, &ctx(0)).action, PeerAction::Ban);
    // 边界 10 码元 -> 封禁
    let boundary = peer("9.9.9.7", 1, Some("-qB5000-x1"), Some("qB"), 10, 10, 0.1);
    assert_eq!(
        m.check("d", &t, &boundary, &ctx(0)).action,
        PeerAction::Ban,
        "LENGTH 区间含上边界（min=10 应封禁）"
    );
    // 9 码元，落在区间外 -> 不封
    let short = peer("9.9.9.6", 1, Some("-qB5000-x"), Some("qB"), 10, 10, 0.1);
    assert_eq!(
        m.check("d", &t, &short, &ctx(0)).action,
        PeerAction::NoAction,
        "LENGTH 区间外（< min）不命中"
    );
}

/// client_name 模块与 peer_id 模块共享同一判定逻辑，但 `data.type` 必须为 `clientName`。
#[test]
fn client_name_module_reports_distinct_data_type() {
    let rules = RuleSet::from_json_text(&[r#"{"method":"CONTAINS","content":"xfplay"}"#.to_string()])
        .expect("contains rule parse");
    let m = StringBlacklist::client_name_with(rules, 0);
    let t = torrent(1_000_000_000, 0, 0);
    let p = peer(
        "7.7.7.7",
        2,
        Some("-qB5000-x"),
        Some("xfplay/9.0"),
        10,
        10,
        0.2,
    );
    let r = m.check("d", &t, &p, &ctx(0));
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(
        r.data["type"], "clientName",
        "client_name 模块的 data.type 必须是 clientName"
    );
    assert_eq!(r.data["rule"], "xfplay");
    // 反例：同样的 client_name 规则不会误伤 peer_id 字段
    let by_id = peer(
        "7.7.7.6",
        2,
        Some("xfplay-evil"),
        Some("qBittorrent"),
        10,
        10,
        0.2,
    );
    assert_eq!(
        m.check("d", &t, &by_id, &ctx(0)).action,
        PeerAction::NoAction,
        "client_name 规则只看 clientName 字段"
    );
}
