//! 实机 AviatorScript 社区脚本的黄金测试。
//!
//! 夹具是用户实机部署（Java v9.5.1 + Aviator 5.4.x）`<data>/scripts/` 下的 PBH-BTN
//! 社区规则，**原样**（未改写）经 `avscript::transpile` 翻译后在 rhai 引擎执行。
//! 期望值依据上游语义推演（与用部署 JRE + aviator jar 的探针结果一致）：
//! - 返回字符串 → BAN（reason 为该字符串原文）；返回 `false` → pass；
//! - `isBlank(null 字段)`：上游对 null 抛异常 → 整脚本 pass；本移植把 null 字段
//!   暴露为空串 → `isBlank` 为 true → 脚本 `return false` → pass（结果一致）。

use pbh_core::module::{PeerAction, RuleModule};
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::modules::ExpressionEngine;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scripts")
}

/// 在独立临时目录放置单个脚本并构建模块（避免脚本间聚合互相干扰）。
fn load_single(script_file: &str) -> ExpressionEngine {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!(
        "pbh_av_golden_{}_{}",
        std::process::id(),
        n
    ));
    fs::create_dir_all(&dir).unwrap();
    let src = fs::read_to_string(fixture_dir().join(script_file)).unwrap();
    fs::write(dir.join(script_file), src).unwrap();
    ExpressionEngine::new(0, Some(dir.to_str().unwrap()))
}

fn peer(client_name: Option<&str>, peer_id: Option<&str>, ip: &str) -> PeerData {
    PeerData {
        client_name: client_name.map(|s| s.to_string()),
        peer_id: peer_id.map(|s| s.to_string()),
        dl_speed: 0,
        downloaded: 0,
        up_speed: 0,
        uploaded: 0,
        progress: 0.5,
        flags: Some("".to_string()),
        ip: ip.to_string(),
        port: 6881,
        raw_ip: format!("{ip}:6881"),
        connection: None,
    }
}

fn torrent() -> TorrentData {
    TorrentData {
        hash: "abc".to_string(),
        name: "t".to_string(),
        progress: 0.5,
        total_size: 1000,
        piece_size: 0,
        pieces_have: 0,
        completed_override: None,
        dlspeed: 0,
        upspeed: 0,
        is_private: Some(false),
    }
}

fn decide(m: &ExpressionEngine, p: &PeerData) -> PeerAction {
    m.check("qb", &torrent(), p, &Default::default()).action
}

fn ban_reason(m: &ExpressionEngine, p: &PeerData) -> String {
    let r = m.check("qb", &torrent(), p, &Default::default());
    assert_eq!(r.action, PeerAction::Ban);
    // USER_SCRIPT_RUN_RESULT 的第 2 个参数是脚本返回的原文（第 1 个是脚本名）
    match &r.reason_key {
        Some(k) => match k.params.get(1) {
            Some(pbh_core::i18n::Param::Text(s)) => s.clone(),
            _ => r.reason,
        },
        None => r.reason,
    }
}

// ---------------------------------------------------------------------------
// gopeed-random-peerid.av：Gopeed Dev 客户端 + 非法 PeerID
// ---------------------------------------------------------------------------

#[test]
fn gopeed_dev_with_random_peer_id_is_banned() {
    let m = load_single("gopeed-random-peerid.av");
    let p = peer(Some("Gopeed Dev 1.2.3"), Some("RANDOMID1234"), "1.2.3.4");
    assert_eq!(decide(&m, &p), PeerAction::Ban);
    assert_eq!(ban_reason(&m, &p), "Gopeed Dev 全随机 PeerID 检测");
}

#[test]
fn gopeed_dev_with_valid_prefix_passes() {
    let m = load_single("gopeed-random-peerid.av");
    assert_eq!(
        decide(&m, &peer(Some("Gopeed Dev 1.2.3"), Some("-gp0001-x"), "1.2.3.4")),
        PeerAction::NoAction
    );
}

#[test]
fn gopeed_other_clients_and_blank_fields_pass() {
    let m = load_single("gopeed-random-peerid.av");
    // 非 Gopeed 客户端
    assert_eq!(
        decide(&m, &peer(Some("qBittorrent/4.3.9"), Some("-qB4390-"), "1.2.3.4")),
        PeerAction::NoAction
    );
    // clientName 为空 / peerId 为空（上游对 null 会抛异常按 pass 兜底，结果一致）
    assert_eq!(decide(&m, &peer(None, Some("-gp0001-x"), "1.2.3.4")), PeerAction::NoAction);
    assert_eq!(decide(&m, &peer(Some("Gopeed Dev"), None, "1.2.3.4")), PeerAction::NoAction);
}

// ---------------------------------------------------------------------------
// name-id-verify.av：PeerID/ClientName 伪装检查（seq.map 插入序敏感）
// ---------------------------------------------------------------------------

#[test]
fn masquerade_is_banned_with_exact_reason() {
    let m = load_single("name-id-verify.av");
    // Deluge 客户端 + μTorrent PeerID → 伪装
    let p = peer(Some("Deluge 2.1.1"), Some("-UT2900-x"), "1.2.3.4");
    assert_eq!(decide(&m, &p), PeerAction::Ban);
    assert_eq!(
        ban_reason(&m, &p),
        "Peer reporting: PeerId=-ut2900-x, ClientName=deluge 2.1.1, But PBH excepted=-de"
    );
}

#[test]
fn table_matches_pass_and_non_table_clients_pass() {
    let m = load_single("name-id-verify.av");
    // 表内匹配：bitcomet→-bc、aria2explorer→-ae、qbittorrent→-qb
    assert_eq!(
        decide(&m, &peer(Some("BitComet 2.0"), Some("-BC0001-x"), "1.2.3.4")),
        PeerAction::NoAction
    );
    assert_eq!(
        decide(&m, &peer(Some("aria2explorer 1.0"), Some("-ae0001-x"), "1.2.3.4")),
        PeerAction::NoAction
    );
    assert_eq!(
        decide(&m, &peer(Some("qBittorrent/4.3.9"), Some("-qB4390-"), "1.2.3.4")),
        PeerAction::NoAction
    );
    // 不在映射表的客户端
    assert_eq!(
        decide(&m, &peer(Some("Xunlei 3.0"), Some("-XL0001-x"), "1.2.3.4")),
        PeerAction::NoAction
    );
}

// ---------------------------------------------------------------------------
// 2e0-61ff-fe.av：IPv6 特征段随机后缀识别
// ---------------------------------------------------------------------------

#[test]
fn ipv6_2e0_feature_segment_is_banned() {
    let m = load_single("2e0-61ff-fe.av");
    let p = peer(Some("aria2/1.36"), Some("-ae0001-x"), "::2e0:61ff:fe:1234");
    assert_eq!(decide(&m, &p), PeerAction::Ban);
    assert_eq!(ban_reason(&m, &p), "2e0:61ff:fe 特征段随机后缀 IPv6 识别");
}

#[test]
fn ipv6_without_feature_segment_passes() {
    let m = load_single("2e0-61ff-fe.av");
    assert_eq!(
        decide(&m, &peer(Some("aria2/1.36"), Some("-ae0001-x"), "2001:db8::5")),
        PeerAction::NoAction
    );
    assert_eq!(
        decide(&m, &peer(Some("aria2/1.36"), Some("-ae0001-x"), "1.2.3.4")),
        PeerAction::NoAction
    );
}

// ---------------------------------------------------------------------------
// dot-1-ipv6-tr296.av：Transmission 2.94 + IPv6 ::1 伪装多拨
// ---------------------------------------------------------------------------

#[test]
fn transmission_294_on_loopback6_is_banned() {
    let m = load_single("dot-1-ipv6-tr296.av");
    let p = peer(Some("Transmission 2.94"), Some("-TR2960-x"), "::1");
    assert_eq!(decide(&m, &p), PeerAction::Ban);
    assert_eq!(ban_reason(&m, &p), "Transmission 2.94 (IPV6 ::1) 多拨伪装吸血");
}

#[test]
fn transmission_other_versions_and_addresses_pass() {
    let m = load_single("dot-1-ipv6-tr296.av");
    // 非 ::1 地址
    assert_eq!(
        decide(&m, &peer(Some("Transmission 2.94"), Some("-TR2960-x"), "2001:db8::5")),
        PeerAction::NoAction
    );
    // 非 Transmission 2.94 客户端
    assert_eq!(
        decide(&m, &peer(Some("Transmission 3.00"), Some("-TR3000-x"), "::1")),
        PeerAction::NoAction
    );
    // clientName 为空
    assert_eq!(
        decide(&m, &peer(None, Some("-TR2960-x"), "::1")),
        PeerAction::NoAction
    );
}

// ---------------------------------------------------------------------------
// 元数据头：@NAME 仍作为展示名（对齐上游 compileScript 的头部解析）
// ---------------------------------------------------------------------------

#[test]
fn script_metadata_name_is_parsed() {
    let dir = std::env::temp_dir().join(format!("pbh_av_meta_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let src = fs::read_to_string(fixture_dir().join("gopeed-random-peerid.av")).unwrap();
    assert!(src.contains("## @NAME Gopeed 全随机检查"));
    fs::write(dir.join("gopeed-random-peerid.av"), src).unwrap();
    let m = ExpressionEngine::new(0, Some(dir.to_str().unwrap()));
    let p = peer(Some("Gopeed Dev 1.2.3"), Some("RANDOMID1234"), "1.2.3.4");
    let r = m.check("qb", &torrent(), &p, &Default::default());
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(r.data["script"], "Gopeed 全随机检查");
}
