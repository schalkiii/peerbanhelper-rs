//! L3：qBittorrent 适配器解析与过滤（SPEC 第 3 节 [GOLDEN]）。

mod common;

use common::{fixtures_dir, mock_qb, mock_qb_with, MockFetcher};
use pbh_downloader::qbittorrent::QBConfig;
use pbh_downloader::{BanEntry, Downloader};

#[tokio::test]
async fn login_version_and_health() {
    let f = MockFetcher::new(fixtures_dir());
    let qb = mock_qb(f.clone());
    let login = qb.login().await.unwrap();
    assert!(login.success, "{}", login.message);
    assert_eq!(login.version, "5.0.2");
    // 首次健康应关闭同 IP 多连接
    let prefs = f.set_prefs_payloads();
    assert!(prefs
        .iter()
        .any(|p| p.contains("enable_multi_connections_from_same_ip")));
}

#[tokio::test]
async fn private_torrent_filtered_and_fields_mapped() {
    let f = MockFetcher::new(fixtures_dir());
    let qb = mock_qb(f);
    let torrents = qb.fetch_torrents().await.unwrap();
    // 夹具中一个公开种子、一个私有种子；ignorePrivate=true 只保留公开
    assert_eq!(torrents.len(), 1);
    let t = &torrents[0];
    assert_eq!(t.hash, "0101010101010101010101010101010101010101");
    assert_eq!(t.total_size, 1_000_000_000);
    assert_eq!(t.completed_size(), 4_194_304 * 239);
    assert!(!t.is_private());
    assert!(t.is_seeding());
}

#[tokio::test]
async fn peers_filtered_and_mapped() {
    let f = MockFetcher::new(fixtures_dir());
    let qb = mock_qb(f);
    let t = qb.fetch_torrents().await.unwrap().remove(0);
    let peers = qb.fetch_peers(&t).await.unwrap();
    // 夹具 8 个：HTTP 连接、空 IP、.onion 各过滤一个 -> 5
    assert_eq!(peers.len(), 5, "filtered peer set");
    let by_ip: std::collections::HashMap<&str, _> =
        peers.iter().map(|p| (p.ip.as_str(), p)).collect();

    let normal = by_ip.get("8.8.8.8").expect("normal peer present");
    assert_eq!(normal.client_name.as_deref(), Some("qBittorrent v5.0.2"));
    assert_eq!(normal.peer_id.as_deref(), Some("-qB5020-xxxxxxxxxxxx"));
    assert!(!normal.is_handshaking());
    assert!(normal.peer_flag().is_some());
    assert_eq!(normal.raw_ip, "8.8.8.8:51413");

    // 被过滤的不应出现
    assert!(!by_ip.contains_key("5.5.5.5"), "HTTP connection filtered");
    assert!(!by_ip.contains_key("2.3.4.5"), ".onion filtered");
    assert!(peers.iter().all(|p| !p.ip.is_empty()), "empty ip filtered");
}

#[tokio::test]
async fn ban_and_replace_payloads() {
    let f = MockFetcher::new(fixtures_dir());
    let qb = mock_qb(f.clone());
    let entries = vec![
        BanEntry {
            ip: "1.1.1.1".into(),
            port: 1,
            raw_ip: "1.1.1.1:1".into(),
        },
        BanEntry {
            ip: "2.2.2.2".into(),
            port: 2,
            raw_ip: "2.2.2.2:2".into(),
        },
    ];
    qb.ban_peers(&entries).await.unwrap();
    let payloads = f.ban_payloads();
    assert_eq!(payloads.len(), 1);
    assert_eq!(payloads[0], "1.1.1.1:1|2.2.2.2:2");

    qb.replace_banned_ips(&["1.1.1.1".to_string(), "2.2.2.2".to_string()])
        .await
        .unwrap();
    let prefs = f.set_prefs_payloads();
    let last = prefs.last().unwrap();
    let v: serde_json::Value = serde_json::from_str(last).unwrap();
    // IPv4 封禁地址会附带 IPv4-mapped IPv6 变体（对齐上游 generateRemappedPairIfPossible），
    // 字符串化对齐 `toCompressedString()`（hex 压缩）；
    // 全量列表按字符串排序后 IPv4 在前、映射形式在后
    assert_eq!(
        v["banned_IPs"],
        "1.1.1.1\n2.2.2.2\n::ffff:101:101\n::ffff:202:202"
    );
}

#[tokio::test]
async fn statistics_mapped() {
    let f = MockFetcher::new(fixtures_dir());
    let qb = mock_qb(f);
    let s = qb.statistics().await.unwrap();
    assert_eq!(s.all_time_upload, 123_456_789);
    assert_eq!(s.all_time_download, 987_654_321);
}

/// 对齐 Java `login0` 开头的 `if (isLoggedIn()) return SUCCESS`：
/// 会话仍然有效时不得重复 POST `/auth/login`（每 5 秒一轮会白白多打一次登录请求）。
#[tokio::test]
async fn login_reuses_a_valid_session() {
    let f = MockFetcher::new(fixtures_dir());
    let qb = mock_qb(f.clone());

    let first = qb.login().await.unwrap();
    assert!(first.success, "{}", first.message);
    assert_eq!(f.call_count("/auth/login"), 1, "首次登录需要表单认证");

    let second = qb.login().await.unwrap();
    assert!(second.success, "{}", second.message);
    assert_eq!(f.call_count("/auth/login"), 1, "会话有效时不应再次登录");
}

/// 对齐 Java `getStatistics`：alltime_ul 与 alltime_dl 同时为 0 时视为未就绪并抛错。
#[tokio::test]
async fn statistics_errors_when_downloader_not_ready() {
    let f = MockFetcher::new(fixtures_dir());
    f.maindata_ready
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let qb = mock_qb(f);
    assert!(qb.statistics().await.is_err(), "alltime 统计全 0 应报错");
}

/// HTTP Basic Auth 对齐上游 OkHttp `Authenticator`：
/// **先不带凭据**发起请求，收到 401 后带 Basic 重试一次（而不是每个请求都预置凭据）。
#[tokio::test]
async fn basic_auth_retries_once_after_401() {
    let f = MockFetcher::new(fixtures_dir());
    f.basic_required
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let cfg = QBConfig {
        basic_auth: Some(("user".into(), "pass".into())),
        ..QBConfig::default()
    };
    let qb = mock_qb_with(f.clone(), cfg);
    let login = qb.login().await.unwrap();
    assert!(login.success, "{}", login.message);

    let buildinfo: Vec<_> = f
        .recorded()
        .into_iter()
        .filter(|c| c.url.contains("/app/buildInfo"))
        .collect();
    assert!(
        !buildinfo.first().expect("至少一次 buildInfo").basic,
        "首个请求不应预置 Basic 凭据（上游用 Authenticator 仅在 401 后重试）"
    );
    assert!(
        buildinfo.iter().any(|c| c.basic),
        "被 401 拒绝后必须带 Basic 重试"
    );
}

/// 下载器能力标志对齐 Java `AbstractQbittorrent.getFeatureFlags()`：
/// 恒定声明 `UNBAN_IP` / `TRAFFIC_STATS` / `LIVE_UPDATE_BT_PROTOCOL_PORT`；
/// 仅 qB >= 5.3.0（或 `5.2.0-beta1`）才声明 `RANGE_BAN_IP`。
#[tokio::test]
async fn feature_flags_follow_qb_version() {
    let f = MockFetcher::new(fixtures_dir());
    let qb = mock_qb(f.clone());
    // 未登录时没有版本信息 -> 不具备 RANGE_BAN_IP
    assert!(qb.feature_flags().iter().any(|f| f == "UNBAN_IP"));
    assert!(!qb.feature_flags().iter().any(|f| f == "RANGE_BAN_IP"));

    qb.login().await.unwrap();
    assert!(qb.feature_flags().iter().any(|f| f == "UNBAN_IP"));
    assert!(qb.feature_flags().iter().any(|f| f == "TRAFFIC_STATS"));
    assert!(qb
        .feature_flags()
        .iter()
        .any(|f| f == "LIVE_UPDATE_BT_PROTOCOL_PORT"));
    assert!(
        !qb.feature_flags().iter().any(|f| f == "RANGE_BAN_IP"),
        "5.0.2 不支持范围封禁"
    );

    // 5.3.0 起支持
    let f2 = MockFetcher::new(fixtures_dir());
    f2.set_version("v5.3.0");
    let qb2 = mock_qb(f2);
    qb2.login().await.unwrap();
    assert!(qb2.feature_flags().iter().any(|f| f == "RANGE_BAN_IP"));

    // 5.2.0-beta1 是上游白名单中的一个特例
    let f3 = MockFetcher::new(fixtures_dir());
    f3.set_version("v5.2.0-beta1");
    let qb3 = mock_qb(f3);
    qb3.login().await.unwrap();
    assert!(qb3.feature_flags().iter().any(|f| f == "RANGE_BAN_IP"));

    // 5.2.0 正式版不支持
    let f4 = MockFetcher::new(fixtures_dir());
    f4.set_version("v5.2.0");
    let qb4 = mock_qb(f4);
    qb4.login().await.unwrap();
    assert!(!qb4.feature_flags().iter().any(|f| f == "RANGE_BAN_IP"));
}

/// 全量封禁列表按下载器能力与 `banlist-remapping` 配置重映射：
/// 具备范围封禁能力的下载器收到 IPv6 `/52` 网段，否则只收到单个地址。
#[tokio::test]
async fn full_banlist_payload_is_remapped_by_capability() {
    let ipv6 = "2001:db8:1:2:3:4:5:6".to_string();

    let f = MockFetcher::new(fixtures_dir());
    f.set_version("v5.3.0");
    let qb = mock_qb(f.clone());
    qb.login().await.unwrap();
    qb.replace_banned_ips(std::slice::from_ref(&ipv6))
        .await
        .unwrap();
    let payload = f.set_prefs_payloads().pop().unwrap();
    let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
    let banned = v["banned_IPs"].as_str().unwrap().to_string();
    assert!(
        banned.contains("2001:db8:1::/52"),
        "应生成 /52 网段: {banned}"
    );
    assert!(banned.contains(&ipv6), "同时保留精确地址: {banned}");

    let f2 = MockFetcher::new(fixtures_dir());
    let qb2 = mock_qb(f2.clone());
    qb2.login().await.unwrap();
    qb2.replace_banned_ips(std::slice::from_ref(&ipv6))
        .await
        .unwrap();
    let payload = f2.set_prefs_payloads().pop().unwrap();
    let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(v["banned_IPs"], ipv6, "不支持范围封禁时只下发单个地址");
}

/// peer 地址翻译对齐 `AbstractDownloader.addressTranslate`：
/// NAT64 → 内嵌 IPv4；IPv4-mapped → IPv4；Teredo 默认不转换。
#[tokio::test]
async fn peers_are_address_translated() {
    let f = MockFetcher::new(fixtures_dir());
    f.peers_responses
        .lock()
        .unwrap()
        .push(std::fs::read_to_string(fixtures_dir().join("torrentPeers_nat64.json")).unwrap());
    let qb = mock_qb(f);
    let t = qb.fetch_torrents().await.unwrap().remove(0);
    let peers = qb.fetch_peers(&t).await.unwrap();
    assert_eq!(peers.len(), 3);
    let by_ip: std::collections::HashMap<&str, _> =
        peers.iter().map(|p| (p.ip.as_str(), p)).collect();

    assert!(by_ip.contains_key("1.2.3.4"), "NAT64 应翻译为内嵌 IPv4");
    assert!(by_ip.contains_key("5.6.7.8"), "IPv4-mapped 应归一为 IPv4");
    assert!(
        by_ip.contains_key("2001:0:4136:e378:8000:63bf:3fff:fdd2"),
        "Teredo 默认不翻译"
    );
    // 原始 ip:port 键保留，供增量封禁使用
    assert_eq!(
        by_ip.get("1.2.3.4").unwrap().raw_ip,
        "64:ff9b::102:304:7001"
    );
}
