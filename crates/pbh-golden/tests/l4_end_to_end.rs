//! L4：端到端黄金测试——夹具下载器 → 判定流水线 → 封禁集合/载荷，逐 peer 对齐上游。

mod common;

use common::{fixtures_dir, mock_qb, MockFetcher};
use pbh_core::pipeline::Decision;
use pbh_core::{default_pipeline, CheckContext};
use pbh_downloader::{BanEntry, Downloader};
use std::collections::HashSet;

/// 跑通：登录 → torrents → peers → 流水线判定，返回（封禁 rawIp 集合, ip->module, ip->rule, 跳过数）。
async fn run_wave(
    qb: &std::sync::Arc<pbh_downloader::QBittorrentDownloader>,
) -> (
    HashSet<String>,
    std::collections::HashMap<String, String>,
    std::collections::HashMap<String, String>,
    usize,
) {
    let pipeline = default_pipeline();
    let ctx = CheckContext {
        now_ms: 0,
        features: qb.feature_flags(),
    };
    let torrents = qb.fetch_torrents().await.unwrap();
    let mut banned = HashSet::new();
    let mut modules = std::collections::HashMap::new();
    let mut rules = std::collections::HashMap::new();
    let mut skipped = 0;
    for t in &torrents {
        for p in qb.fetch_peers(t).await.unwrap() {
            match pipeline.evaluate(qb.id(), t, &p, &ctx) {
                Decision::Ban(r) => {
                    banned.insert(p.raw_ip.clone());
                    modules.insert(p.ip.clone(), r.module.clone());
                    rules.insert(p.ip.clone(), r.rule.clone());
                }
                Decision::Skip(_) => skipped += 1,
                Decision::None => {}
            }
        }
    }
    (banned, modules, rules, skipped)
}

#[tokio::test]
async fn end_to_end_expected_bans() {
    let f = MockFetcher::new(fixtures_dir());
    let qb = mock_qb(f.clone());
    qb.login().await.unwrap();

    let (banned, modules, rules, skipped) = run_wave(&qb).await;

    // 期望封禁：吸血 peerId、吸血 client、进度作弊（6.6.6.6 上传量已达 10% 阈值，
    // 触发 PCB 快速测试 BAN_FOR_DISCONNECT，与上游默认 `fast-pcb-test-percentage: 0.1` 一致）
    let expected: HashSet<String> = ["9.9.9.9:1000", "7.7.7.7:2000", "6.6.6.6:7000"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(banned, expected, "ban set must match Java baseline");

    assert_eq!(
        modules.get("9.9.9.9").map(String::as_str),
        Some("peer-id-blacklist")
    );
    assert_eq!(
        modules.get("7.7.7.7").map(String::as_str),
        Some("client-name-blacklist")
    );
    assert_eq!(
        modules.get("6.6.6.6").map(String::as_str),
        Some("progress-cheat-blocker")
    );
    assert_eq!(
        rules.get("6.6.6.6").map(String::as_str),
        Some("fastPcbTest")
    );
    assert_eq!(rules.get("9.9.9.9").map(String::as_str), Some("rule"));

    // 局域网 peer 命中 bypass，被跳过
    assert_eq!(skipped, 1, "LAN peer skipped");

    // 正常 peer 不被封
    assert!(!banned.contains("8.8.8.8:51413"));

    // 下发增量封禁，载荷集合一致（顺序因 HashMap 不保证，按集合比较）
    let entries: Vec<BanEntry> = expected
        .iter()
        .map(|raw| {
            let (ip, port) = raw.rsplit_once(':').unwrap();
            BanEntry {
                ip: ip.to_string(),
                port: port.parse().unwrap(),
                raw_ip: raw.clone(),
            }
        })
        .collect();
    qb.ban_peers(&entries).await.unwrap();
    let payload = f.ban_payloads().pop().unwrap();
    let got: HashSet<String> = payload.split('|').map(|s| s.to_string()).collect();
    assert_eq!(got, expected);
}

#[tokio::test]
async fn lan_peer_never_banned_even_with_blacklisted_id() {
    let f = MockFetcher::new(fixtures_dir());
    let qb = mock_qb(f);
    let (banned, _, _, skipped) = run_wave(&qb).await;
    // 192.168.1.5 虽 peerId 命中 -hp，但 bypass 优先
    assert!(!banned.contains("192.168.1.5:3000"));
    assert!(skipped >= 1);
}
