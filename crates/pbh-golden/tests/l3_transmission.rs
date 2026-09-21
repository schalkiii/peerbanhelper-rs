//! L3：Transmission 适配器（JSON-RPC、409 会话握手、blocklist 配置、字段映射）。

use pbh_core::remap::RemapConfig;
use pbh_downloader::http::{BoxFuture, HttpFetcher, HttpRequest, HttpResponse};
use pbh_downloader::transmission::{TRConfig, TransmissionDownloader};
use pbh_downloader::Downloader;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const SESSION_ID: &str = "abcdef0123456789";

struct TrMock {
    pub requests: Mutex<Vec<(String, String)>>,
    pub handshakes: AtomicUsize,
    pub version: Mutex<String>,
    pub blocklist_enabled: Mutex<bool>,
    pub blocklist_url: Mutex<String>,
    pub update_fails: Mutex<bool>,
}

impl TrMock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            handshakes: AtomicUsize::new(0),
            version: Mutex::new("4.1.0 (abcdef)".into()),
            blocklist_enabled: Mutex::new(false),
            blocklist_url: Mutex::new(String::new()),
            update_fails: Mutex::new(false),
        })
    }

    fn methods(&self) -> Vec<String> {
        self.requests.lock().unwrap().iter().map(|(m, _)| m.clone()).collect()
    }

    fn arguments_of(&self, method: &str) -> Option<serde_json::Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .find(|(m, _)| m == method)
            .and_then(|(_, body)| serde_json::from_str::<serde_json::Value>(body).ok())
            .map(|v| v.get("arguments").cloned().unwrap_or(serde_json::Value::Null))
    }
}

impl HttpFetcher for TrMock {
    fn execute<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, anyhow::Result<HttpResponse>> {
        Box::pin(async move {
            assert!(req.url.ends_with("/transmission/rpc"), "unexpected url {}", req.url);
            // 409 会话握手
            if req.header("X-Transmission-Session-Id").is_none() {
                self.handshakes.fetch_add(1, Ordering::SeqCst);
                return Ok(HttpResponse::new(409, "").with_header("X-Transmission-Session-Id", SESSION_ID));
            }
            assert_eq!(req.header("X-Transmission-Session-Id"), Some(SESSION_ID));
            let body = req.body.clone().unwrap_or_default();
            let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
            let method = parsed["method"].as_str().unwrap().to_string();
            self.requests.lock().unwrap().push((method.clone(), body));

            let json = match method.as_str() {
                "session-get" => format!(
                    r#"{{"arguments":{{"version":"{}","blocklist-enabled":{},"blocklist-url":"{}"}},"result":"success"}}"#,
                    self.version.lock().unwrap(),
                    self.blocklist_enabled.lock().unwrap(),
                    self.blocklist_url.lock().unwrap()
                ),
                "session-set" => {
                    if let Some(args) = parsed.get("arguments") {
                        if let Some(url) = args.get("blocklist-url").and_then(|v| v.as_str()) {
                            // 模拟服务端保存
                            let _ = url;
                        }
                    }
                    r#"{"arguments":{},"result":"success"}"#.to_string()
                }
                "blocklist-update" => {
                    if *self.update_fails.lock().unwrap() {
                        r#"{"arguments":{},"result":"failure"}"#.to_string()
                    } else {
                        include_str!("fixtures/transmission_ok.json").to_string()
                    }
                }
                "torrent-get" => include_str!("fixtures/transmission_torrent_get.json").to_string(),
                "session-stats" => {
                    include_str!("fixtures/transmission_session_stats.json").to_string()
                }
                other => format!(r#"{{"arguments":{{}},"result":"unknown method {other}"}}"#),
            };
            Ok(HttpResponse::new(200, json))
        })
    }
}

fn downloader(mock: Arc<TrMock>) -> Arc<TransmissionDownloader> {
    let cfg = TRConfig {
        id: "tr-test".into(),
        name: "TR Test".into(),
        endpoint: "http://mock.local".into(),
        blocklist_url: "http://127.0.0.1:9898/blocklist/p2p-plain-format".into(),
        ..TRConfig::default()
    };
    Arc::new(
        TransmissionDownloader::with_fetcher(cfg, RemapConfig::default(), mock as Arc<dyn HttpFetcher>)
            .unwrap(),
    )
}

#[tokio::test]
async fn login_handshakes_session_and_configures_blocklist() {
    let mock = TrMock::new();
    let tr = downloader(mock.clone());
    let login = tr.login().await.unwrap();
    assert!(login.success, "{}", login.message);
    assert_eq!(login.version, "4.1.0", "版本截断到 5 个字符");
    assert_eq!(mock.handshakes.load(Ordering::SeqCst), 1, "首发 409 后重试");

    let methods = mock.methods();
    assert_eq!(methods, vec!["session-get", "session-set", "blocklist-update"]);
    let args = mock.arguments_of("session-set").unwrap();
    assert_eq!(args["blocklist-enabled"], true);
    assert!(
        args["blocklist-url"]
            .as_str()
            .unwrap()
            .starts_with("http://127.0.0.1:9898/blocklist/p2p-plain-format?t="),
        "URL 需带 cache-busting 参数: {args}"
    );
}

#[tokio::test]
async fn login_skips_blocklist_configuration_when_already_set() {
    let mock = TrMock::new();
    *mock.blocklist_enabled.lock().unwrap() = true;
    *mock.blocklist_url.lock().unwrap() =
        "http://127.0.0.1:9898/blocklist/p2p-plain-format?t=1".into();
    let tr = downloader(mock.clone());
    assert!(tr.login().await.unwrap().success);
    assert_eq!(mock.methods(), vec!["session-get"], "已配置好则不再设置/更新");
}

#[tokio::test]
async fn login_rejects_version_below_4_1_0() {
    let mock = TrMock::new();
    *mock.version.lock().unwrap() = "4.0.6".into();
    let tr = downloader(mock.clone());
    let login = tr.login().await.unwrap();
    assert!(!login.success);
    assert!(login.message.contains("4.1.0"), "{}", login.message);
    assert_eq!(mock.methods(), vec!["session-get"]);
}

#[tokio::test]
async fn login_fails_when_blocklist_update_fails_and_restores_failed_url() {
    let mock = TrMock::new();
    *mock.update_fails.lock().unwrap() = true;
    let tr = downloader(mock.clone());
    let login = tr.login().await.unwrap();
    assert!(!login.success);
    // 上游会额外把 URL 指向明显的失败地址，方便用户在 WebUI 发现
    let methods = mock.methods();
    assert!(methods.contains(&"blocklist-update".to_string()));
    let urls: Vec<String> = mock
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(m, _)| m == "session-set")
        .filter_map(|(_, body)| {
            serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| v["arguments"]["blocklist-url"].as_str().map(|s| s.to_string()))
        })
        .collect();
    assert!(
        urls.iter().any(|u| u.contains("peerbanhelper-blocklist-update-failed")),
        "失败后应写入失败占位 URL: {urls:?}"
    );
}

#[tokio::test]
async fn torrents_are_filtered_and_mapped() {
    let mock = TrMock::new();
    let tr = downloader(mock.clone());
    let torrents = tr.fetch_torrents().await.unwrap();
    // 活跃过滤：无速度且无连接的种子被排除；默认 ignore-private=false（上游默认），私有种子保留
    assert_eq!(torrents.len(), 2);
    let t = torrents
        .iter()
        .find(|t| t.hash == "0202020202020202020202020202020202020202")
        .expect("active public torrent");
    assert_eq!(t.name, "active-public.iso");
    assert_eq!(t.total_size, 800_000_000);
    assert_eq!(t.progress, 0.25);
    assert!(!t.is_private());
    // Transmission 的完成量口径：sizeWhenDone * percentDone
    assert_eq!(t.completed_size(), 200_000_000);
    assert!(
        torrents.iter().all(|t| t.hash != "0404040404040404040404040404040404040404"),
        "无速度无连接的种子不应出现"
    );

    let fields = mock.arguments_of("torrent-get").unwrap();
    assert!(fields["fields"].as_array().unwrap().iter().any(|f| f == "peers"));
}

#[tokio::test]
async fn private_torrents_are_filtered_when_ignore_private_enabled() {
    let mock = TrMock::new();
    let cfg = TRConfig {
        id: "tr-test".into(),
        name: "TR Test".into(),
        endpoint: "http://mock.local".into(),
        ignore_private: true,
        blocklist_url: "http://127.0.0.1:9898/blocklist/p2p-plain-format".into(),
        ..TRConfig::default()
    };
    let tr = Arc::new(
        TransmissionDownloader::with_fetcher(cfg, RemapConfig::default(), mock as Arc<dyn HttpFetcher>)
            .unwrap(),
    );
    let torrents = tr.fetch_torrents().await.unwrap();
    assert_eq!(torrents.len(), 1);
    assert!(!torrents[0].is_private());
}

#[tokio::test]
async fn peers_come_from_torrent_get_and_decode_base64_peer_id() {
    let mock = TrMock::new();
    let tr = downloader(mock.clone());
    let torrents = tr.fetch_torrents().await.unwrap();
    let peers = tr.fetch_peers(&torrents[0]).await.unwrap();
    assert_eq!(peers.len(), 2, "peers 来自 torrent-get 响应（不额外发 RPC）");

    let by_ip: std::collections::HashMap<&str, _> =
        peers.iter().map(|p| (p.ip.as_str(), p)).collect();
    let normal = by_ip.get("9.9.9.9").expect("peer 9.9.9.9");
    assert_eq!(normal.peer_id.as_deref(), Some("-TR4100-000000000000"));
    assert_eq!(normal.dl_speed, 3000);
    assert_eq!(normal.up_speed, 100);
    assert_eq!(normal.downloaded, 20000);
    assert_eq!(normal.uploaded, 4000);
    assert_eq!(normal.progress, 0.5);
    assert_eq!(normal.flags.as_deref(), Some("DU"));
    assert_eq!(normal.raw_ip, "9.9.9.9:1000");
    // peer_id 为 null -> 空字符串
    assert_eq!(by_ip.get("7.7.7.7").unwrap().peer_id.as_deref(), Some(""));

    // 只发出一次 torrent-get
    assert_eq!(mock.methods().iter().filter(|m| *m == "torrent-get").count(), 1);
}

#[tokio::test]
async fn statistics_and_feature_flags() {
    let mock = TrMock::new();
    let tr = downloader(mock.clone());
    let stats = tr.statistics().await.unwrap();
    assert_eq!(stats.all_time_upload, 111_222_333);
    assert_eq!(stats.all_time_download, 444_555_666);

    let flags = tr.feature_flags();
    assert!(flags.contains(&"UNBAN_IP".to_string()));
    assert!(flags.contains(&"TRAFFIC_STATS".to_string()));
    assert!(flags.contains(&"LIVE_UPDATE_BT_PROTOCOL_PORT".to_string()));
    assert!(!flags.contains(&"RANGE_BAN_IP".to_string()), "Transmission 不支持范围封禁");
}

#[tokio::test]
async fn increment_ban_is_noop_and_full_ban_triggers_blocklist_update() {
    let mock = TrMock::new();
    let tr = downloader(mock.clone());
    // 增量封禁：Transmission 不支持 -> 不产生请求
    tr.ban_peers(&[pbh_downloader::BanEntry {
        ip: "1.2.3.4".into(),
        port: 6881,
        raw_ip: "1.2.3.4:6881".into(),
    }])
    .await
    .unwrap();
    assert!(mock.methods().is_empty());

    // 全量：触发 blocklist-update（内容由 PBH 自己的端点提供）
    tr.replace_banned_ips(&["1.2.3.4".to_string()]).await.unwrap();
    assert_eq!(mock.methods(), vec!["blocklist-update"]);
}
