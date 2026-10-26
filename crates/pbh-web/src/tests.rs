use super::*;
use axum::body::to_bytes;
use axum::http::Request;
use pbh_core::modules::{AlertLevel, MonitorSink, TrackedSwarmRow};
use pbh_db::DbMonitorSink;
use pbh_downloader::Downloader;
use std::sync::atomic::AtomicBool;
use tower::ServiceExt;

/// 端点测试用的 AppState（内存库 + 固定 Token + 内嵌文案表）。
fn test_state(db: Arc<Database>) -> AppState {
    AppState {
        db,
        token: Arc::new(Mutex::new("test-token".to_string())),
        silent_login: String::new(),
        metrics: Arc::new(Mutex::new(Metrics::default())),
        started: Arc::new(Instant::now()),
        downloaders: Arc::new(Mutex::new(Vec::new())),
        static_dir: Arc::new(Mutex::new(None)),
        ban_list: Arc::new(Mutex::new(pbh_core::banlist::BanList::default())),
        remap: Arc::new(pbh_core::remap::RemapConfig::default()),
        translator: Arc::new(Translator::embedded()),
        locale: "zh_cn".to_string(),
        backend: Arc::new(NoopBackend),
        log_ring: Arc::new(RingLog::new(16)),
        global_pause: Arc::new(AtomicBool::new(false)),
        sub_module: None,
        btn_network: Default::default(),
        tasks: Arc::new(BackgroundTaskRegistry::new()),
        geo: None,
        ptr: Arc::new(pbh_core::ptr::PtrLookup::disabled()),
        live_snapshot: pbh_core::live::new_live_snapshot(),
        scripts_dir: None,
        lab_path: None,
    }
}

/// 测试用空实现：所有能力返回默认/空值（端点路由不依赖具体后端）。
struct NoopBackend;
impl crate::backend::WebBackend for NoopBackend {
fn downloader_meta(&self, _id: &str) -> Option<DownloaderMeta> {
    None
}

    fn installation_id(&self) -> String {
        "test-install".into()
    }
    fn set_token(&self, _token: &str) -> Result<(), String> {
        Ok(())
    }
    fn analytics_enabled(&self) -> bool {
        true
    }
    fn set_analytics(&self, _enabled: bool) -> Result<(), String> {
        Ok(())
    }
    fn dry_run_enabled(&self) -> bool {
        false
    }
    fn set_dry_run(&self, _enabled: bool) -> Result<(), String> {
        Ok(())
    }
    fn modules(&self) -> Vec<ModuleRecord> {
        vec![]
    }
    fn global_paused(&self) -> bool {
        false
    }
    fn set_global_paused(&self, _paused: bool) -> Result<(), String> {
        Ok(())
    }
    fn reload(&self) -> Vec<ReloadEntry> {
        vec![]
    }
    fn read_config(&self, _name: &str) -> Result<Value, String> {
        // 最小 profile 结构：ipblacklist CRUD 契约测试依赖此段存在
        Ok(serde_json::json!({
            "module": {
                "ip-address-blocker": {
                    "ips": ["0.0.0.0"],
                    "ports": [0],
                    "asns": ["0"],
                    "regions": ["0"],
                    "cities": ["示例海南"],
                    "net_type": { "wideband": false },
                }
            }
        }))
    }
    fn write_config(&self, _name: &str, _data: &Value) -> Result<(), String> {
        Ok(())
    }
    fn ban_peers(&self, _ips: &[String]) -> Result<(), String> {
        Ok(())
    }
    fn unban_peers(&self, _ips: &[String]) -> Result<usize, String> {
        Ok(0)
    }
    fn downloaders(&self) -> Vec<Value> {
        vec![]
    }
    fn downloader(&self, _id: &str) -> Option<Arc<dyn Downloader>> {
        None
    }
    fn add_downloader(&self, _config: &Value) -> Result<(), String> {
        Ok(())
    }
    fn update_downloader(&self, _id: &str, _config: &Value) -> Result<(), String> {
        Err("TEST_ONLY".into())
    }
    fn remove_downloader(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }
    fn test_downloader(&self, _config: &Value) -> Result<(), String> {
        Ok(())
    }
    fn push_channels(&self) -> Vec<Value> {
        vec![]
    }
    fn add_push_channel(&self, _channel: &Value) -> Result<(), String> {
        Ok(())
    }
    fn update_push_channel(&self, _name: &str, _channel: &Value) -> Result<(), String> {
        Ok(())
    }
    fn remove_push_channel(&self, _name: &str) -> Result<(), String> {
        Ok(())
    }
    fn test_push_channel(&self, _channel: &Value) -> Result<(), String> {
        Ok(())
    }
}

/// 发一个带 Token 的 GET 请求，返回 (状态码, JSON)。
async fn get_json(state: &AppState, uri: &str) -> (StatusCode, Value) {
    let request = Request::builder()
        .uri(uri)
        .header("Authorization", "Bearer test-token")
        .body(Body::empty())
        .expect("请求构造");
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("路由调用");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("响应体");
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

/// 发一个带 Token 的任意方法请求（`body` 可为纯文本），返回 (状态码, JSON)。
///
/// 脚本端点 `PUT /api/expression-engine/{id}` 的 body 是**源码文本而非 JSON**，
/// 因此这里不强制 `application/json`。
async fn send_json(
    state: &AppState,
    method: axum::http::Method,
    uri: &str,
    body: Option<String>,
) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(uri)
        .header("Authorization", "Bearer test-token")
        // `Json` 提取器要求该头；`Bytes` 提取器（脚本源码）不校验
        .header("content-type", "application/json")
        .body(body.map(Body::from).unwrap_or_else(Body::empty))
        .expect("请求构造");
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("路由调用");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("响应体");
    let parsed: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, parsed)
}

/// 规则订阅测试替身：验证 `/api/sub/*` 的**响应形状**（不依赖真实规则文件/网络）。
struct TestSubModule;
impl crate::SubModule for TestSubModule {
    fn list_rules(&self) -> Vec<Value> {
        vec![json!({
            "ruleId": "all-in-one",
            "ruleName": "All",
            "subUrl": "http://x/all.txt",
            "enabled": true,
            "lastUpdate": 100,
            "entCount": 3,
        })]
    }
    fn add_rule(&self, _rule: &Value) -> Result<(), String> {
        Ok(())
    }
    fn update_rule(&self, _id: &str, _rule: &Value) -> Result<(), String> {
        Ok(())
    }
    fn remove_rule(&self, _id: &str) -> Result<(), String> {
        Ok(())
    }
    fn refresh_rule(&self, _id: Option<&str>) -> String {
        String::new()
    }
    fn logs(&self, _rule_id: Option<&str>, _page: i64, _size: i64) -> (i64, Vec<Value>) {
        (0, vec![])
    }
    fn interval(&self) -> i64 {
        14_400_000
    }
    fn set_interval(&self, _ms: i64) -> Result<(), String> {
        Ok(())
    }
}

fn swarm_row(address: &str, last_time_seen_ms: i64) -> TrackedSwarmRow {
    TrackedSwarmRow {
        id: None,
        ip: address.to_string(),
        port: 6881,
        info_hash: "abcdef0123456789".to_string(),
        torrent_is_private: Some(false),
        torrent_size: 1_000_000_000,
        downloader: "qb".to_string(),
        downloader_progress: 0.25,
        peer_id: "-qB4500-aaaaaaaa".to_string(),
        client_name: "qBittorrent/4.5.0".to_string(),
        peer_progress: 0.5,
        uploaded: 100,
        uploaded_offset: 100,
        upload_speed: 10,
        downloaded: 200,
        downloaded_offset: 200,
        download_speed: 20,
        last_flags: "d u".to_string(),
        first_time_seen_ms: 1000,
        last_time_seen_ms,
        download_speed_max: 30,
        upload_speed_max: 40,
        dirty: false,
    }
}

#[tokio::test]
async fn swarm_tracking_endpoint_returns_bare_tracked_swarm_size() {
    let db = Arc::new(Database::open_in_memory().unwrap());
    let state = test_state(db.clone());
    let sink = DbMonitorSink::new(db);
    for index in 0..3 {
        let mut row = swarm_row(&format!("10.0.0.{index}"), 1000 + index);
        row.port = 6881 + index as u16;
        sink.upsert_tracked_swarm(&row);
    }

    // 上游 `handleWebAPI` 直接返回裸 JSON（没有 success/message/data 包装）
    let (status, body) = get_json(&state, "/api/modules/swarm-tracking").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "trackedSwarmSize": 3 }));
    assert!(body.get("data").is_none());

    // 清空后计数回落
    sink.reset_tracked_swarm();
    let (_, body) = get_json(&state, "/api/modules/swarm-tracking").await;
    assert_eq!(body, json!({ "trackedSwarmSize": 0 }));
}

#[tokio::test]
async fn swarm_tracking_details_pages_and_orders() {
    let db = Arc::new(Database::open_in_memory().unwrap());
    let state = test_state(db.clone());
    let sink = DbMonitorSink::new(db);
    for index in 0..5 {
        let mut row = swarm_row(&format!("10.0.0.{index}"), 1000 + index);
        row.port = 6881 + index as u16;
        sink.upsert_tracked_swarm(&row);
    }

    // 默认 page=1 / pageSize=10
    let (status, body) = get_json(&state, "/api/modules/swarm-tracking/details").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], json!(true));
    assert_eq!(body["message"], Value::Null);
    assert_eq!(body["data"]["page"], json!(1));
    assert_eq!(body["data"]["size"], json!(10));
    assert_eq!(body["data"]["total"], json!(5));
    assert_eq!(body["data"]["results"].as_array().unwrap().len(), 5);
    let first = &body["data"]["results"][0];
    assert_eq!(first["ip"], json!("10.0.0.0"));
    assert_eq!(first["infoHash"], json!("abcdef0123456789"));
    assert_eq!(first["torrentIsPrivate"], json!(false));
    assert_eq!(first["peerProgress"], json!(0.5));
    assert_eq!(first["firstTimeSeen"], json!(1000), "时间戳为 epoch 毫秒");
    assert!(
        first.get("dirty").is_none(),
        "dirty 是 transient 字段，不输出"
    );

    // 分页：page=2&pageSize=2
    let (_, body) = get_json(
        &state,
        "/api/modules/swarm-tracking/details?page=2&pageSize=2",
    )
    .await;
    assert_eq!(body["data"]["page"], json!(2));
    assert_eq!(body["data"]["size"], json!(2));
    assert_eq!(body["data"]["total"], json!(5));
    assert_eq!(body["data"]["results"].as_array().unwrap().len(), 2);
    assert_eq!(body["data"]["results"][0]["lastTimeSeen"], json!(1002));

    // orderBy=last_time_seen|desc（对齐 `Orderable` 的 `field|desc`）
    let (_, body) = get_json(
        &state,
        "/api/modules/swarm-tracking/details?orderBy=last_time_seen%7Cdesc&pageSize=3",
    )
    .await;
    let seen: Vec<i64> = body["data"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["lastTimeSeen"].as_i64().unwrap())
        .collect();
    assert_eq!(seen, vec![1004, 1003, 1002]);

    // 非法排序列（对齐 `SQLHelper.checkSafeFieldName`）-> 500
    let (status, body) = get_json(
        &state,
        "/api/modules/swarm-tracking/details?orderBy=id%3Bdrop",
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body["success"], json!(false));
}

#[tokio::test]
async fn alerts_endpoint_lists_unread_alerts_localized_per_request() {
    let db = Arc::new(Database::open_in_memory().unwrap());
    let state = test_state(db.clone());
    let sink = DbMonitorSink::new(db);
    sink.publish_alert(
        true,
        AlertLevel::Warn,
        "dataTrafficCapping-1",
        &TranslationComponent::with_params(
            "MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_TITLE",
            vec!["2026-09-20".into()],
        ),
        &TranslationComponent::new("MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_DESCRIPTION"),
    );

    let (status, body) = get_json(&state, "/api/alerts").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], json!(true));
    let alert = &body["data"][0];
    assert_eq!(alert["identifier"], json!("dataTrafficCapping-1"));
    assert_eq!(alert["level"], json!("WARN"));
    assert_eq!(alert["readAt"], Value::Null, "未读告警");
    assert!(alert["createAt"].as_i64().unwrap() > 0);
    assert_eq!(
        alert["title"],
        json!("下载器上行流量超限告警 (2026-09-20)"),
        "默认 locale（服务端 zh_cn）"
    );

    // `?locale=en_us` 用同一份落库数据重新本地化
    let (_, body) = get_json(&state, "/api/alerts?locale=en_us").await;
    assert_eq!(
        body["data"][0]["title"],
        json!("Download upload traffic reached threshold (2026-09-20)")
    );

    // 无 Token -> 401
    let request = Request::builder()
        .uri("/api/alerts")
        .body(Body::empty())
        .expect("请求构造");
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("路由调用");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[test]
fn p2p_plain_format_uses_start_end_ranges() {
    // 主机地址（无前缀）按 /32 与 /128 处理
    let bans = vec![
        "1.2.3.0/24".to_string(),
        "2001:db8::/32".to_string(),
        "1.2.3.4".to_string(),
        "2001:db8:1:2:3:4:5:6".to_string(),
    ];
    let text = build_p2p_plain(&bans, None);
    let lines: Vec<&str> = text.trim().lines().collect();
    assert_eq!(lines.len(), 4, "主机地址不能被丢弃: {text}");
    let (name, range) = lines[0].split_once(':').unwrap();
    assert_eq!(name.len(), 32, "规则名是 32 位十六进制");
    assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
    assert_eq!(range, "1.2.3.0-1.2.3.255");
    assert!(lines[1].ends_with(":2001:db8::-2001:db8:ffff:ffff:ffff:ffff:ffff:ffff"));
    assert!(lines[2].ends_with(":1.2.3.4-1.2.3.4"));
    assert!(lines[3].ends_with(":2001:db8:1:2:3:4:5:6-2001:db8:1:2:3:4:5:6"));
}

#[test]
fn p2p_plain_format_returns_transmission_workaround_when_empty() {
    let text = build_p2p_plain(&[], Some("Transmission 4.1.0"));
    assert_eq!(
        text,
        "TransmissionWorkaround:127.127.127.127-127.127.127.127"
    );
    // 其它客户端返回空
    assert_eq!(build_p2p_plain(&[], Some("qBittorrent/5.0")), "");
    assert_eq!(build_p2p_plain(&[], None), "");
}

#[test]
fn ip_and_dat_emule_formats() {
    let bans = vec!["1.2.3.4/32".to_string()];
    assert_eq!(build_ip_list(&bans), "1.2.3.4/32\n");
    assert_eq!(
        build_dat_emule(&bans),
        "1.2.3.4 - 1.2.3.4 , 000 , 1.2.3.4/32\n"
    );
}

#[test]
fn render_keyed_localizes_from_stored_component() {
    let t = Translator::embedded();
    let stored = serde_json::to_string(&TranslationComponent::with_params(
        "MODULE_IBL_MATCH_IP",
        vec!["1.2.3.0/24".into()],
    ))
    .unwrap();
    // 按请求 locale 重新本地化（同一份落库数据，两种语言）
    assert_eq!(
        render_keyed(&Some(stored.clone()), "fallback", &t, "zh_cn"),
        "匹配 IP 规则: 1.2.3.0/24"
    );
    assert_eq!(
        render_keyed(&Some(stored), "fallback", &t, "en_us"),
        "Match IP rule: 1.2.3.0/24"
    );
    // 缺 key 或不合法 JSON → 回退到落库已渲染文案
    assert_eq!(render_keyed(&None, "fallback", &t, "zh_cn"), "fallback");
    assert_eq!(
        render_keyed(&Some("not-json".to_string()), "fallback", &t, "zh_cn"),
        "fallback"
    );
    // 归一化：zh-CN → zh_cn
    let cn = render_keyed(
        &Some(serde_json::to_string(&TranslationComponent::new("Peer handshaking")).unwrap()),
        "x",
        &t,
        "zh-CN",
    );
    assert_eq!(cn, "Peer handshaking");
}

// ===========================================================================
// WebUI 契约测试（生产事故回归集，2026-10-07）
//
// 背景：单元/黄金测试全绿，但实机部署后前端大面积不可用——缺陷全部集中在
// 「API 表面契约」：端点缺失/硬编码占位、响应结构与前端期望不符、认证流程
// 断裂、/api 未命中被 SPA fallback 兜成假 200。本组测试逐条断言该契约，
// 每条对应一个已发生的生产缺陷。
// ===========================================================================

/// 发起不带认证的请求（模拟首次访问/GUI 未登录）。
async fn raw_get(state: AppState, uri: &str) -> (StatusCode, axum::http::HeaderMap, Value) {
    // ConnectInfo extension：oneshot 测试请求绕过 serve 层，status 接口的
    // ConnectInfo<SocketAddr> extractor 需要 request 手动注入
    let request = Request::builder()
        .uri(uri)
        // 契约测试聚焦「响应结构」；认证流程由独立用例覆盖
        .header("Authorization", "Bearer test-token")
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            9898,
        ))))
        .body(Body::empty())
        .expect("请求构造");
    let response = build_router(state.clone()).oneshot(request).await.expect("路由调用");
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("响应体");
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, headers, body)
}

#[tokio::test]
async fn btn_status_contract() {
    // 缺陷回归：/api/modules/btn 曾是硬编码占位（无 enabled 字段），
    // BTN 实际运行时设置页仍显示「未启用」。
    let (status, _, body) = raw_get(test_state(Arc::new(Database::open_in_memory().unwrap())), "/api/modules/btn").await;
    assert_eq!(status, StatusCode::OK);
    let data = body.get("data").expect("data 字段");
    assert!(data.get("enabled").and_then(|v| v.as_bool()).is_some(), "必须含布尔 enabled 字段");
    assert!(data.get("abilities").and_then(|v| v.as_array()).is_some(), "必须含 abilities 数组");
    assert!(data.get("configSuccess").is_some(), "必须含 configSuccess");
}

#[tokio::test]
async fn general_status_contract() {
    // 缺陷回归：general/status 的 jvm.memory 为空对象（首页 0 Bytes 红点）、
    // compile_time=0（显示 1970）、无 btn 段（设置页显示 BTN 未启用）。
    let (status, _, body) = raw_get(test_state(Arc::new(Database::open_in_memory().unwrap())), "/api/general/status").await;
    assert_eq!(status, StatusCode::OK);
    let data = body.get("data").expect("data 字段");
    let available = data
        .pointer("/jvm/memory/heap/available")
        .and_then(|v| v.as_u64())
        .expect("jvm.memory.heap.available 必须为数字（驱动首页「X 可用」）");
    assert!(available > 0, "available 不应为 0");
    let compile_time = data
        .pointer("/peerbanhelper/compile_time")
        .and_then(|v| v.as_i64())
        .expect("compile_time 必须为秒级时间戳（上游 `generatePbhData` 语义）");
    assert!(compile_time > 1_600_000_000, "不得为 1970（占位 0）");
    let btn = data.get("btn").expect("必须含 btn 段（设置页 BTN 状态数据源）");
    assert!(btn.get("enabled").and_then(|v| v.as_bool()).is_some());
    let net = data.pointer("/system/network").expect("必须含 system.network");
    assert!(net.get("internet_access").is_some(), "必须含 internet_access");
    assert!(net.get("use_proxy").is_some(), "必须含 use_proxy");
}

#[tokio::test]
async fn pbhplus_status_contract() {
    // 缺陷回归：/api/pbhplus/* 完全缺失（被 SPA fallback 兜成假 200 的 HTML），
    // 前端以 enabledFeatures 门控页面功能，空列表导致各处显示升级卡。
    let (status, _, body) = raw_get(test_state(Arc::new(Database::open_in_memory().unwrap())), "/api/pbhplus/status").await;
    assert_eq!(status, StatusCode::OK);
    let data = body.get("data").expect("data 字段");
    let features = data
        .get("enabledFeatures")
        .and_then(|v| v.as_array())
        .expect("必须含 enabledFeatures 数组");
    let names: Vec<&str> = features.iter().filter_map(|v| v.as_str()).collect();
    assert!(names.contains(&"basic"), "必须含 basic（页面可用性门控）");
    assert!(names.contains(&"paid"), "必须含 paid（Plus 按钮门控）");
    assert!(data.get("licenses").and_then(|v| v.as_array()).is_some());
}

#[tokio::test]
async fn downloaders_list_contract() {
    // 缺陷回归：list 响应缺 endpoint/paused（对齐上游 DownloaderWrapperDTO），
    // 编辑下载器弹窗全空。
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    *state.downloaders.lock().unwrap() = vec![DownloaderStatus {
        id: "d1".to_string(),
        name: "qBittorrent".to_string(),
        kind: "qbittorrent".to_string(),
        online: true,
        version: "4.6.7".to_string(),
        torrents: 3,
        peers: 12,
    }];
    let (status, _, body) = raw_get(state, "/api/downloaders").await;
    assert_eq!(status, StatusCode::OK);
    let item = body
        .pointer("/data/0")
        .expect("至少一个下载器条目");
    for field in ["id", "name", "type", "online", "version", "endpoint", "paused"] {
        assert!(item.get(field).is_some(), "list 条目缺字段 {field}");
    }
}

#[tokio::test]
async fn downloader_status_contract() {
    // 缺陷回归：status 响应缺 lastStatus/activeTorrents/activePeers/config/paused
    // （对齐上游 DownloaderStatusDTO）——首页显示「未知」、编辑弹窗全空。
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    *state.downloaders.lock().unwrap() = vec![DownloaderStatus {
        id: "d1".to_string(),
        name: "qBittorrent".to_string(),
        kind: "qbittorrent".to_string(),
        online: true,
        version: "4.6.7".to_string(),
        torrents: 3,
        peers: 12,
    }];
    let (status, _, body) = raw_get(state, "/api/downloaders/d1/status").await;
    assert_eq!(status, StatusCode::OK);
    let data = body.get("data").expect("data 字段");
    for field in [
        "lastStatus",
        "lastStatusMessage",
        "activeTorrents",
        "activePeers",
        "config",
        "paused",
    ] {
        assert!(data.get(field).is_some(), "status 响应缺字段 {field}");
    }
    assert_eq!(
        data.get("activeTorrents").and_then(|v| v.as_u64()),
        Some(3),
        "activeTorrents 来自 wave 聚合计数"
    );
}

#[tokio::test]
async fn login_sets_session_cookie_and_authorizes() {
    // 缺陷回归：login 只校验不下发 cookie，登录成功后所有 API 仍 401
    // （前端仅靠浏览器自动携带的会话 cookie 维持会话）。
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let router = build_router(state.clone());
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"token":"test-token"}"#))
        .expect("请求构造");
    let response = router.clone().oneshot(request).await.expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK);
    let cookie = response
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .find_map(|v| v.to_str().ok()?.split(';').next().map(str::to_string))
        .expect("登录必须下发 PBH_SESSION cookie");
    assert!(cookie.starts_with("PBH_SESSION="), "cookie 名必须为 PBH_SESSION：{cookie}");
    // 会话 cookie 调受保护端点必须通过
    let request = Request::builder()
        .uri("/api/statistic/counter")
        .header("Cookie", &cookie)
        .body(Body::empty())
        .expect("请求构造");
    let response = router.oneshot(request).await.expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK, "会话 cookie 必须通过认证");
}

#[tokio::test]
async fn silent_login_flow_sets_cookie_via_document() {
    // 缺陷回归：静默登录豁免最初挂在 /api middleware 上，而文档请求（GET /）
    // 不经过它——cookie 永远种不下，GUI 骨架屏。闭环必须是：文档入口
    // 校验 ?silentLogin= → 302 + Set-Cookie → 页面 API 由浏览器携带 cookie。
    let mut state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    state.silent_login = "test-silent-token".to_string();
    let router = build_router(state.clone());
    let request = Request::builder()
        .uri("/?silentLogin=test-silent-token")
        .body(Body::empty())
        .expect("请求构造");
    let response = router.clone().oneshot(request).await.expect("路由调用");
    assert_eq!(response.status(), StatusCode::FOUND, "文档入口应 302");
    let cookie = response
        .headers()
        .get_all(axum::http::header::SET_COOKIE)
        .iter()
        .find_map(|v| v.to_str().ok()?.split(';').next().map(str::to_string))
        .expect("静默登录必须下发会话 cookie");
    // 种下的 cookie 调受保护端点必须通过
    let request = Request::builder()
        .uri("/api/statistic/counter")
        .header("Cookie", &cookie)
        .body(Body::empty())
        .expect("请求构造");
    let response = router.oneshot(request).await.expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK, "静默登录的会话必须通过认证");
}

#[tokio::test]
async fn spa_fallback_and_api_404() {
    // 缺陷回归（两向）：
    // ① SPA history 路由刷新曾落入「未安装」占位页 → 未命中路径必须回退 html；
    // ② /api 未命中端点曾同样兜成 index.html（假 200 掩盖缺失端点）→ 必须 404 JSON。
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let router = build_router(state);
    // ① SPA 回退：html（占位也是 html，content-type 断言足够区分 JSON）
    for path in ["/", "/some/spa/route"] {
        let request = Request::builder().uri(path).body(Body::empty()).expect("请求构造");
        let response = router.clone().oneshot(request).await.expect("路由调用");
        assert_eq!(response.status(), StatusCode::OK, "{path} 应回退 html");
        let ct = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(ct.starts_with("text/html"), "{path} 必须回退 html，实际 {ct}");
    }
    // ② /api 未命中：404 JSON（不得是 index.html 假 200）
    let request = Request::builder()
        .uri("/api/nonexistent-endpoint")
        .body(Body::empty())
        .expect("请求构造");
    let response = router.oneshot(request).await.expect("路由调用");
    assert_eq!(response.status(), StatusCode::NOT_FOUND, "/api 未命中必须 404");
    let ct = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(ct.starts_with("application/json"), "/api 404 必须是 JSON，实际 {ct}");
}

#[tokio::test]
async fn spa_fallback_injects_base_href() {
    // 回归（2026-10-11）：前端 dist 使用相对路径引用 assets/api/favicon。
    // 从 /banlist 等子路由直接刷新时，浏览器会请求 /banlist/assets/...、
    // /banlist/api/...，导致 JS/CSS 被当成 HTML 解析、API 请求被 fallback 吞掉
    // → 前端报「无法解析接口返回数据」。后端在回退的 index.html 里注入
    // <base href="/">，让所有相对路径以根为基准。
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    // test_state 默认 static_dir=None（回退到占位页），这里构造临时 dist 目录
    // 只验证注入逻辑，不依赖生产目录。
    let tmp = std::env::temp_dir().join(format!("pbh-web-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(
        tmp.join("index.html"),
        "<!DOCTYPE html><html><head><title>PeerBanHelper</title></head><body><div id=app></div></body></html>",
    )
    .unwrap();
    *state.static_dir.lock().unwrap() = Some(tmp.clone());
    let router = build_router(state);
    for path in ["/banlist", "/settings", "/some/spa/route"] {
        let request = Request::builder().uri(path).body(Body::empty()).expect("请求构造");
        let response = router.clone().oneshot(request).await.expect("路由调用");
        assert_eq!(response.status(), StatusCode::OK, "{path} 应回退 html");
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(
            html.contains("<base href=\"/\">"),
            "{path} 回退的 html 必须注入 <base href=\"/\">，否则子路由相对路径错位"
        );
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn spa_route_api_request_redirects_to_api_root() {
    // 回归（2026-10-11）：从 /banlist 刷新时前端可能请求 /banlist/api/metadata/manifest，
    // 后端若回退 index.html，前端拿到 HTML 当 JSON 解析失败。应 308 到 /api/...。
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let tmp = std::env::temp_dir().join(format!("pbh-web-api-redirect-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("index.html"), "<!DOCTYPE html><html><head></head><body></body></html>").unwrap();
    *state.static_dir.lock().unwrap() = Some(tmp.clone());
    let router = build_router(state);
    let request = Request::builder()
        .uri("/banlist/api/metadata/manifest")
        .body(Body::empty())
        .expect("请求构造");
    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::PERMANENT_REDIRECT);
    let loc = response
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(loc, "/api/metadata/manifest");
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test]
async fn counter_reads_accumulated_metrics() {
    // 缺陷回归：counter 读 Metrics 的累计字段（checks/peer_bans/peer_unbans），
    // 但 wave 从未累加——首页「共检查/封禁/解封」恒为 0。
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    *state.metrics.lock().unwrap() = Metrics {
        checks: 7,
        peer_bans: 3,
        peer_unbans: 1,
        ..Default::default()
    };
    let request = Request::builder()
        .uri("/api/statistic/counter")
        .header("Authorization", "Bearer test-token")
        .body(Body::empty())
        .expect("请求构造");
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("响应体");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let data = body.get("data").expect("data 字段");
    assert_eq!(data.get("checkCounter").and_then(|v| v.as_u64()), Some(7));
    assert_eq!(data.get("peerBanCounter").and_then(|v| v.as_u64()), Some(3));
    assert_eq!(data.get("peerUnbanCounter").and_then(|v| v.as_u64()), Some(1));
}

// ---- 契约测试第二批：覆盖矩阵 web 层剩余端点 ----

#[tokio::test]
async fn ipblacklist_crud_roundtrip() {
    // NoopBackend.read_config 返回最小 profile，CRUD 全链路可断言
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let (_, body) = get_json(&state, "/api/modules/ipblacklist/ip").await;
    assert_eq!(body.pointer("/data/ip/0").and_then(|v| v.as_str()), Some("0.0.0.0"));
    // PUT 追加
    let request = Request::builder()
        .method("PUT")
        .uri("/api/modules/ipblacklist/ip")
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"ip":"203.0.114.55"}"#))
        .expect("请求构造");
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("路由调用");
    assert_eq!(response.status(), StatusCode::CREATED, "PUT 应 201");
    // DELETE 移除
    let request = Request::builder()
        .method("DELETE")
        .uri("/api/modules/ipblacklist/ip")
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"ip":"203.0.114.55"}"#))
        .expect("请求构造");
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK, "DELETE 应 200");
    // netType 读（对象→数组推导）
    let (_, body) = get_json(&state, "/api/modules/ipblacklist/netType").await;
    assert!(body.pointer("/data/netType").and_then(|v| v.as_array()).is_some());
}

#[tokio::test]
async fn general_global_get_and_patch() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let (_, body) = get_json(&state, "/api/general/global").await;
    assert_eq!(body.pointer("/data/globalPaused").and_then(|v| v.as_bool()), Some(false));
    let request = Request::builder()
        .method("PATCH")
        .uri("/api/general/global")
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"globalPaused":true}"#))
        .expect("请求构造");
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK, "PATCH 应 200");
}

#[tokio::test]
async fn bans_logs_and_ranks_endpoints() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let (status, _) = get_json(&state, "/api/bans/logs").await;
    assert_eq!(status, StatusCode::OK, "bans/logs 必须 200");
    let (status, _) = get_json(&state, "/api/bans/ranks").await;
    assert_eq!(status, StatusCode::OK, "bans/ranks 必须 200");
}

/// `/api/bans?search=` 覆盖 peer id / client / torrent 名称，并返回实时 GeoIP。
#[tokio::test]
async fn bans_search_matches_metadata_and_returns_geo() {
    use pbh_core::banlist::{BanMetadata, BannedRecord};
    use pbh_core::geoip::{CountryData, IpGeoData, StaticGeoIp};

    let mut state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let mut metadata = BanMetadata::minimal("1.2.3.4", "test-module", 1_000, 0);
    metadata.peer.id = "peer-xyz".into();
    metadata.peer.client_name = "client-abc".into();
    metadata.torrent.name = "example-torrent".into();
    metadata.torrent.hash = "hash-1".into();
    state
        .ban_list
        .lock()
        .unwrap()
        .add_record(BannedRecord {
            ip: "1.2.3.4".into(),
            unban_at_ms: 0,
            module: "test-module".into(),
            ban_for_disconnect: false,
            metadata,
        });

    let mut geo = StaticGeoIp::new();
    geo.insert(
        "1.2.3.4".parse().unwrap(),
        IpGeoData {
            country: Some(CountryData {
                name: Some("United States".into()),
                iso: Some("US".into()),
            }),
            ..Default::default()
        },
    );
    state.geo = Some(Arc::new(geo));

    let (status, body) = get_json(&state, "/api/bans?search=peer-xyz").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["total"], 1);
    assert_eq!(body["data"]["results"][0]["address"], "1.2.3.4");
    assert_eq!(
        body["data"]["results"][0]["ipGeoData"]["country"]["iso"],
        "US"
    );

    let (_, body) = get_json(&state, "/api/bans?search=example-torrent").await;
    assert_eq!(body["data"]["total"], 1, "torrent 名称应可搜索");
    let (_, body) = get_json(&state, "/api/bans?search=no-such-peer").await;
    assert_eq!(body["data"]["total"], 0);
}

#[tokio::test]
async fn push_channels_endpoint() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let (status, body) = get_json(&state, "/api/push").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("data").is_some(), "push 响应必须含 data");
}

#[tokio::test]
async fn sub_rules_without_module_returns_404() {
    // sub_module=None 的契约：404（前端据此显示「未启用」而非空白）
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let (status, _) = get_json(&state, "/api/sub/rules").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn charts_endpoints_return_200() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    for path in [
        "/api/chart/geoIpInfo",
        "/api/chart/trend",
        "/api/chart/traffic",
        "/api/chart/sessionAnalyse",
    ] {
        let (status, _) = get_json(&state, path).await;
        assert_eq!(status, StatusCode::OK, "{path} 必须 200");
    }
}

#[tokio::test]
async fn statistics_endpoints_return_200() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    for path in [
        "/api/statistic/analysis/date",
        "/api/statistic/analysis/banTrends",
        "/api/statistic/rules",
    ] {
        let (status, _) = get_json(&state, path).await;
        assert_eq!(status, StatusCode::OK, "{path} 必须 200");
    }
}

// ===========================================================================
// 契约测试第三批：**字段级**契约（防「前端静默 0 / NaN undefined/s」复发）
//
// 背景：前两批对部分端点只断言 200，未断言字段名，导致三类生产缺陷连续漏网——
// ① `counter` 写成 `peerBlockRate` ⇒ 「恶意:正常 连接比例」恒 0.00%；
// ② `ranks` 用 `address` 而非 `peerIp`；③ `downloaders/{id}/torrents` 缺
// `rtUploadSpeed/rtDownloadSpeed` ⇒ 活动种子速度列 `NaN undefined/s`。
// 本批对「前端实际读取」的字段集合做**全等断言**（多/缺/错名都会失败）。
// ===========================================================================

/// 断言对象字段集合与期望**完全一致**（排序后逐项比较）。
fn assert_fields(obj: &Value, expected: &[&str], what: &str) {
    let map = obj
        .as_object()
        .unwrap_or_else(|| panic!("{what} 不是 JSON 对象: {obj}"));
    let mut keys: Vec<&str> = map.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut want: Vec<&str> = expected.to_vec();
    want.sort_unstable();
    assert_eq!(
        keys, want,
        "{what} 字段集合不符——字段名错配会让前端静默显示 0/空白/NaN"
    );
}

#[tokio::test]
async fn statistic_counter_field_contract() {
    // 回归：`peersBlockRate` 曾误写为 `peerBlockRate`，前端
    // `formatPercentage(undefined)` 使「恶意:正常 连接比例」恒为 0.00%。
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let (status, body) = get_json(&state, "/api/statistic/counter").await;
    assert_eq!(status, StatusCode::OK);
    let data = body.get("data").expect("data 字段");
    assert_fields(
        data,
        &[
            "banlistCounter",
            "bannedIpCounter",
            "checkCounter",
            "peerBanCounter",
            "peerUnbanCounter",
            "peersBlockRate",
            "trackedSwarmCount",
            "wastedTraffic",
            "weeklySessions",
        ],
        "statistic/counter",
    );
    // 旧字段名不得复现
    assert!(
        data.get("peerBlockRate").is_none(),
        "不得再出现旧字段名 peerBlockRate（前端读的是 peersBlockRate）"
    );
}

#[tokio::test]
async fn peer_info_field_contract() {
    // 对齐上游 `PeerInfoDTO`
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let (status, body) = get_json(&state, "/api/peer/1.2.3.4").await;
    assert_eq!(status, StatusCode::OK);
    let data = body.get("data").expect("data 字段");
    assert_fields(
        data,
        &[
            "address",
            "banCount",
            "btnQueryAvailable",
            "downloadedFromPeer",
            "firstTimeSeen",
            "found",
            "geo",
            "lastTimeSeen",
            "ptrLookup",
            "torrentAccessCount",
            "uploadedToPeer",
        ],
        "peer/{ip}",
    );
}

#[test]
fn downloader_torrent_field_contract() {
    // 回归（2026-10-11）：缺 rtUploadSpeed/rtDownloadSpeed ⇒ 活动种子速度列
    // 渲染成 `NaN undefined/s`。对齐上游 `TorrentWrapper` 全 9 字段。
    let t = pbh_core::model::TorrentData {
        hash: "abc".into(),
        name: "n".into(),
        progress: 0.5,
        total_size: 100,
        piece_size: 10,
        pieces_have: 4,
        completed_override: None,
        dlspeed: 7,
        upspeed: 9,
        is_private: Some(true),
    };
    let v = crate::api::downloaders::torrent_data_json(&t);
    assert_fields(
        &v,
        &[
            "completedSize",
            "hash",
            "id",
            "name",
            "privateTorrent",
            "progress",
            "rtDownloadSpeed",
            "rtUploadSpeed",
            "size",
        ],
        "downloaders/{id}/torrents 条目",
    );
    assert_eq!(v.get("rtUploadSpeed").and_then(|x| x.as_i64()), Some(9));
    assert_eq!(v.get("rtDownloadSpeed").and_then(|x| x.as_i64()), Some(7));
    // completedSize = piece_size * pieces_have（无 override 时）
    assert_eq!(v.get("completedSize").and_then(|x| x.as_i64()), Some(40));
}

#[tokio::test]
async fn paginated_endpoints_response_shape() {
    // 分页端点统一 {page,size,total,results}（前端按此渲染分页器）
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    for path in [
        "/api/bans/logs",
        "/api/bans/ranks",
        "/api/peer/1.2.3.4/banHistory",
        "/api/peer/1.2.3.4/accessHistory",
        "/api/torrent/query",
    ] {
        let (status, body) = get_json(&state, path).await;
        assert_eq!(status, StatusCode::OK, "{path} 必须 200");
        let data = body.get("data").expect("data 字段");
        for f in ["page", "size", "total", "results"] {
            assert!(data.get(f).is_some(), "{path} 缺分页字段 {f}");
        }
    }
}

#[tokio::test]
async fn chart_endpoints_response_shape() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    // geoIpInfo：四个维度必须是 **KV 数组**——前端 `ispPie.vue` 的
    // `onSuccess` 只判 `if (data.data)`，随后直接 `data.data[field].reduce(...)`，
    // 返回对象会让整页报 `.reduce is not a function`（2026-10-12 生产问题）。
    // 未安装 IP 库（geo=None）时返回四个空数组。
    let (_, body) = get_json(&state, "/api/chart/geoIpInfo").await;
    let data = body.get("data").expect("data 字段");
    for f in ["city", "isp", "province", "region"] {
        let Some(field) = data.get(f) else {
            panic!("geoIpInfo 缺 {f}")
        };
        assert!(
            field.as_array().is_some(),
            "geoIpInfo.{f} 必须是数组（前端要 .reduce），实际 {field}"
        );
    }
    // trend：两个趋势数组（trends.vue 读 connectedPeersTrend / bannedPeersTrend）
    let (_, body) = get_json(&state, "/api/chart/trend").await;
    let data = body.get("data").expect("data 字段");
    for f in ["connectedPeersTrend", "bannedPeersTrend"] {
        assert!(data.get(f).and_then(|v| v.as_array()).is_some(), "trend 缺 {f}");
    }
    // traffic / sessionAnalyse：直接是数组（前端 .map，非对象）
    for path in ["/api/chart/traffic", "/api/chart/sessionAnalyse"] {
        let (_, body) = get_json(&state, path).await;
        assert!(
            body.get("data").and_then(|v| v.as_array()).is_some(),
            "{path} 的 data 必须是数组"
        );
    }
}

/// 自定义脚本端点（`custom-script` 页面）：列表分页 / 内容往返 / 删除 / 非法 id 拒绝。
///
/// 这套端点此前**完全缺失**，页面会拿到 `/api` fallback 的 `API_NOT_FOUND` 且列表为空。
#[tokio::test]
async fn expression_engine_script_endpoints_roundtrip() {
    let dir = std::env::temp_dir().join(format!("pbh-scripts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    state.scripts_dir = Some(dir.clone());

    // 端点必须存在（缺失时 /api 的 fallback 返回 API_NOT_FOUND + data:null）
    let (status, body) = get_json(&state, "/api/expression-engine/scripts").await;
    assert_eq!(status, StatusCode::OK, "scripts 端点缺失: {body}");
    let data = body.get("data").expect("data 字段");
    for f in ["page", "size", "total", "results"] {
        assert!(data.get(f).is_some(), "scripts 缺分页字段 {f}");
    }
    assert!(data
        .get("results")
        .and_then(|v| v.as_array())
        .expect("results 必须是数组")
        .is_empty());

    // editable：目录可写
    let (status, body) = get_json(&state, "/api/expression-engine/editable").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["data"]["editable"].as_bool().unwrap_or(false),
        "临时目录应可写"
    );

    // PUT（body 为源码文本）→ GET 回读 → 列表出现 → DELETE
    let (status, _) = send_json(
        &state,
        axum::http::Method::PUT,
        "/api/expression-engine/demo.av",
        Some("return 1;".to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "PUT 脚本失败");
    let (_, body) = get_json(&state, "/api/expression-engine/demo.av").await;
    assert_eq!(body["data"].as_str(), Some("return 1;"), "脚本内容往返");

    let (_, body) = get_json(&state, "/api/expression-engine/scripts").await;
    let results = body["data"]["results"].as_array().unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["id"].as_str(), Some("demo.av"));
    assert_eq!(results[0]["name"].as_str(), Some("demo"));
    assert!(results[0]["version"].is_string(), "version 必填（前端渲染）");
    assert_eq!(body["data"]["total"], 1);

    let (status, _) = send_json(
        &state,
        axum::http::Method::DELETE,
        "/api/expression-engine/demo.av",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = get_json(&state, "/api/expression-engine/scripts").await;
    assert_eq!(body["data"]["total"], 0);

    // 非法 id 必须被拒绝（含 `..` 的路径穿越、非 .av 后缀）
    for bad in ["/api/expression-engine/..evil.av", "/api/expression-engine/evil.txt"] {
        let (status, _) = send_json(
            &state,
            axum::http::Method::PUT,
            bad,
            Some("x".to_string()),
        )
        .await;
        assert_ne!(status, StatusCode::OK, "{bad} 必须被拒绝");
    }
    assert!(
        !dir.join("..evil.av").exists() && !dir.join("evil.txt").exists(),
        "非法 id 不得落盘"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 实验性功能端点（`laboratory`）：清单结构 + 总开关/单项开关持久化。
///
/// 缺端点时「启用实验性功能」会返回 API_NOT_FOUND。
#[tokio::test]
async fn laboratory_endpoints_expose_and_toggle_experiments() {
    let dir = std::env::temp_dir().join(format!("pbh-lab-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let lab = dir.join("laboratory.yml");
    let mut state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    state.lab_path = Some(lab.clone());

    let (status, body) = get_json(&state, "/api/laboratory/experiments").await;
    assert_eq!(status, StatusCode::OK, "laboratory 端点缺失: {body}");
    let data = body["data"].clone();
    assert!(data.get("labEnabled").is_some(), "缺 labEnabled");
    let experiments = data
        .get("experiments")
        .and_then(|v| v.as_array())
        .expect("experiments 必须是数组");
    assert!(!experiments.is_empty(), "至少应注册一个实验");
    for item in experiments {
        for f in ["id", "title", "description", "activated"] {
            assert!(item.get(f).is_some(), "实验项缺 {f}");
        }
    }

    // 总开关关闭 -> 所有实验 activated 恒 false（对齐上游 `isExperimentActivated`）
    let (status, _) = send_json(
        &state,
        axum::http::Method::POST,
        "/api/laboratory/config",
        Some(r#"{"enabled":false}"#.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = get_json(&state, "/api/laboratory/experiments").await;
    assert_eq!(body["data"]["labEnabled"], false);
    for item in body["data"]["experiments"].as_array().unwrap() {
        assert_eq!(item["activated"], false, "总开关关闭时不得激活");
    }

    // 总开关打开 + 打开单个实验
    send_json(
        &state,
        axum::http::Method::POST,
        "/api/laboratory/config",
        Some(r#"{"enabled":true}"#.to_string()),
    )
    .await;
    let (status, _) = send_json(
        &state,
        axum::http::Method::PUT,
        "/api/laboratory/experiment/sqlite_vacuum",
        Some(r#"{"status":true}"#.to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "设置实验失败");
    let (_, body) = get_json(&state, "/api/laboratory/experiments").await;
    let sqlite = body["data"]["experiments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == "sqlite_vacuum")
        .expect("sqlite_vacuum 实验应注册");
    assert_eq!(sqlite["activated"], true);

    // 未知实验 id 必须报错而非静默成功
    let (status, _) = send_json(
        &state,
        axum::http::Method::PUT,
        "/api/laboratory/experiment/no-such-experiment",
        Some(r#"{"status":true}"#.to_string()),
    )
    .await;
    assert_ne!(status, StatusCode::OK, "未知实验 id 必须被拒绝");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 规则订阅端点：`rules` 的 data 必须是**数组**，`interval` 的 data 必须是**裸数字**。
///
/// 此前 `rules` 返回 `{rules,count}` 对象、字段名为 `id/name/url/entries`，
/// 与前端 `ruleBrief`（`ruleId/ruleName/subUrl/entCount/lastUpdate`）全不对，
/// 页面表现为「用 java 版配置打开也是空的」。
/// 上游仅后端使用、前端未调用的少量接口：至少保证路由存在且语义诚实。
#[tokio::test]
async fn parity_alias_routes_exist() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let (status, _) = send_json(
        &state,
        axum::http::Method::POST,
        "/api/general/refreshNatStatus",
        Some("{}".into()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "AutoSTUN 未接线");

    let (status, body) = get_json(&state, "/api/autostun/tunnel/dl-1/info").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["success"], false);
    assert_eq!(body["message"], "AUTOSTUN_NOT_SUPPORTED");
}

/// 彩蛋：无 Token 直接 GET `/api/egg` → 302 Found，Location 为上游链接列表之一。
#[tokio::test]
async fn easter_egg_redirects_without_auth() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let request = Request::builder()
        .uri("/api/egg")
        .body(Body::empty())
        .expect("请求构造");
    let response = build_router(state)
        .oneshot(request)
        .await
        .expect("路由调用");
    assert_eq!(response.status(), StatusCode::FOUND);
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .expect("Location 头");
    assert!(location.starts_with("https://"), "意外的跳转地址 {location}");
}

#[tokio::test]
async fn sub_endpoints_return_bare_array_and_number() {
    let mut state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    state.sub_module = Some(Arc::new(TestSubModule));

    let (status, body) = get_json(&state, "/api/sub/rules").await;
    assert_eq!(status, StatusCode::OK);
    let data = body.get("data").expect("data 字段");
    assert!(
        data.as_array().is_some(),
        "sub/rules 的 data 必须是数组（前端 CommonResponse<ruleBrief[]>），实际 {data}"
    );
    let rules = data.as_array().unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0]["ruleId"], "all-in-one");
    assert_eq!(rules[0]["ruleName"], "All");
    assert_eq!(rules[0]["subUrl"], "http://x/all.txt");
    assert_eq!(rules[0]["entCount"], 3);
    assert_eq!(rules[0]["lastUpdate"], 100);

    let (status, body) = get_json(&state, "/api/sub/interval").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["data"].as_i64(),
        Some(14_400_000),
        "sub/interval 的 data 必须是裸数字，实际 {}",
        body["data"]
    );

    // 上游 `/api/sub/logs/{ruleId}`：单条订阅日志分页
    for path in ["/api/sub/logs", "/api/sub/logs/all-in-one"] {
        let (status, body) = get_json(&state, path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert_eq!(body["data"]["page"], 1, "{path}");
        assert!(
            body["data"]["results"].as_array().is_some(),
            "{path} results 必须是数组"
        );
    }
}

/// WebUI 端点契约清单（从上游 `webui/src/service/*.ts` 提取后固化）。
///
/// 仓库内不含前端源码，无法在 CI 里扫描前端，因此把「前端实际调用的全部端点 +
/// data 形状」固化成清单作为测试源。前端升级后需重新提取刷新此文件。
const WEBUI_ENDPOINTS_JSON: &str = include_str!("webui_endpoints.json");

/// `/api` fallback 的 404（`api_not_found`）——**端点缺失**的判据。
///
/// 405（方法不匹配）也算路由存在，因此不能用 status 直接判断。
fn is_api_not_found(status: StatusCode, body: &Value) -> bool {
    status == StatusCode::NOT_FOUND
        && body.get("message").and_then(|v| v.as_str()) == Some("API_NOT_FOUND")
}

/// 按清单约定的形状校验 `data`。
/// 按点路径取字段（`banMetadata.peer` → 两级）；任一层缺失返回 None。
fn field_at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(value, |current, key| current.get(key))
}

/// 校验一组点路径字段全部存在（`null` 也算存在——前端读取的是键本身）。
fn check_fields(data: &Value, paths: &[&str], label: &str) -> Result<(), String> {
    for path in paths {
        if field_at(data, path).is_none() {
            return Err(format!("{label}缺字段 {path}：{data}"));
        }
    }
    Ok(())
}

fn check_contract_shape(shape: &str, data: &Value) -> Result<(), String> {
    let kind = |v: &Value| -> &'static str {
        if v.is_array() {
            "array"
        } else if v.is_object() {
            "object"
        } else if v.is_number() {
            "number"
        } else if v.is_string() {
            "string"
        } else {
            "null"
        }
    };
    match shape {
        "any" => Ok(()),
        "array" | "object" | "number" | "string" => {
            if kind(data) == shape {
                Ok(())
            } else {
                Err(format!("data 应为 {shape}，实际 {}：{data}", kind(data)))
            }
        }
        "page" => {
            let obj = data.as_object().ok_or_else(|| format!("data 应为对象：{data}"))?;
            for f in ["page", "size", "total", "results"] {
                if !obj.contains_key(f) {
                    return Err(format!("缺分页字段 {f}"));
                }
            }
            if !data["results"].is_array() {
                return Err(format!("results 必须是数组：{}", data["results"]));
            }
            Ok(())
        }
        "geo" => {
            for f in ["city", "isp", "province", "region"] {
                match data.get(f) {
                    Some(v) if v.is_array() => {}
                    _ => return Err(format!("{f} 必须是数组（前端要 .reduce）")),
                }
            }
            Ok(())
        }
        "lab" => {
            if !data.get("labEnabled").is_some() {
                return Err(format!("缺 labEnabled：{data}"));
            }
            if !data["experiments"].is_array() {
                return Err(format!("experiments 必须是数组：{data}"));
            }
            Ok(())
        }
        other => Err(format!("清单里未知的 shape：{other}")),
    }
}

/// 清单里可选的嵌套字段断言：`fields`（对象）/ `item_fields`（数组元素）/
/// `results_fields`（`page` 形状的 `results` 元素）。点路径支持一层以上嵌套。
///
/// 空数组直接判失败：本测试的职责是「字段名别改坏」，没有样本数据时
/// 静默跳过会让字段改名重新变成隐性回归（这正是契约清单要防的事故类型）。
fn check_contract_fields(entry: &Value, data: &Value) -> Result<(), String> {
    let list = |key: &str| -> Vec<String> {
        entry
            .get(key)
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let fields = list("fields");
    if !fields.is_empty() {
        let paths: Vec<&str> = fields.iter().map(String::as_str).collect();
        check_fields(data, &paths, "data ")?;
    }
    let item_fields = list("item_fields");
    if !item_fields.is_empty() {
        let items = data
            .as_array()
            .ok_or_else(|| format!("item_fields 需要数组 data：{data}"))?;
        if items.is_empty() {
            return Err("item_fields 无样本：data 是空数组，请先给测试注入种子数据".into());
        }
        let paths: Vec<&str> = item_fields.iter().map(String::as_str).collect();
        for item in items {
            check_fields(item, &paths, "数组元素")?;
        }
    }
    let results_fields = list("results_fields");
    if !results_fields.is_empty() {
        let results = data
            .get("results")
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("results_fields 需要 page 形状：{data}"))?;
        if results.is_empty() {
            return Err("results_fields 无样本：results 是空数组，请先给测试注入种子数据".into());
        }
        let paths: Vec<&str> = results_fields.iter().map(String::as_str).collect();
        for item in results {
            check_fields(item, &paths, "results 元素")?;
        }
    }
    Ok(())
}

/// 嵌套字段断言自身必须真的能抓住「字段改名」与「空样本静默跳过」。
#[test]
fn contract_field_checks_reject_renamed_or_empty_samples() {
    let object_entry = json!({ "fields": ["version.version"] });
    assert!(check_contract_fields(&object_entry, &json!({ "version": { "version": "9.5.1" } })).is_ok());
    assert!(
        check_contract_fields(&object_entry, &json!({ "version": { "ver": "9.5.1" } })).is_err(),
        "字段改名必须失败"
    );

    let page_entry = json!({ "results_fields": ["address", "banMetadata.peer"] });
    let page_ok = json!({ "results": [{ "address": "1.2.3.4", "banMetadata": { "peer": {} } }] });
    assert!(check_contract_fields(&page_entry, &page_ok).is_ok());
    let page_renamed =
        json!({ "results": [{ "address": "1.2.3.4", "banMetadata": { "peers": {} } }] });
    assert!(
        check_contract_fields(&page_entry, &page_renamed).is_err(),
        "嵌套字段改名必须失败"
    );
    assert!(
        check_contract_fields(&page_entry, &json!({ "results": [] })).is_err(),
        "空样本不得静默跳过"
    );

    let array_entry = json!({ "item_fields": ["id", "type"] });
    assert!(
        check_contract_fields(&array_entry, &json!([{ "id": "a", "type": "qbittorrent" }])).is_ok()
    );
    assert!(
        check_contract_fields(&array_entry, &json!([{ "id": "a", "kind": "qbittorrent" }])).is_err(),
        "数组元素字段改名必须失败"
    );
    assert!(check_contract_fields(&array_entry, &json!([])).is_err());
}

/// **前端契约全量校验**：清单里的每个端点都必须存在，且 `data` 形状与前端期望一致。
///
/// 这组测试存在的理由：此前 `mod tests` 里已有「WebUI 契约测试（生产事故回归集）」，
/// 但那是**手工挑选的事故点**——`/api/expression-engine/*`、`/api/laboratory/*`
/// 从未被任何测试请求过，于是「缺端点」一直没暴露。这里改为**以前端完整调用面
/// 为契约源**，一次性覆盖全部端点。
#[tokio::test]
async fn webui_contract_all_endpoints_exist_and_match_shape() {
    let dir = std::env::temp_dir().join(format!("pbh-webui-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    // 注入可注入的依赖，让「模块未启用」不至于掩盖「形状不符」
    state.sub_module = Some(Arc::new(TestSubModule));
    state.scripts_dir = Some(dir.join("scripts"));
    state.lab_path = Some(dir.join("laboratory.yml"));
    // 种子数据：列表端点为空数组时无法校验 item_fields/results_fields
    // （空样本会掩盖字段改名，因此上面两个断言遇到空数组直接失败）
    state
        .ban_list
        .lock()
        .unwrap()
        .add("5.6.7.8", i64::MAX, "test-module", false);
    state.downloaders.lock().unwrap().push(DownloaderStatus {
        id: "dl-seed".into(),
        name: "Seed".into(),
        kind: "qbittorrent".into(),
        online: true,
        version: "5.2.0".into(),
        torrents: 0,
        peers: 0,
    });

    let contract: Value = serde_json::from_str(WEBUI_ENDPOINTS_JSON).expect("清单 JSON");
    let endpoints = contract["endpoints"].as_array().expect("endpoints 数组");
    assert!(!endpoints.is_empty(), "清单不应为空");

    let mut failures: Vec<String> = Vec::new();
    for entry in endpoints {
        let method = entry["method"].as_str().unwrap_or("GET");
        let path = entry["path"].as_str().expect("path");
        let shape = entry["shape"].as_str().unwrap_or("any");
        // SSE 等无法在 oneshot 里请求的长连接端点显式跳过
        if entry.get("skip").is_some() {
            continue;
        }
        let http_method = axum::http::Method::from_bytes(method.as_bytes()).expect("method");
        // 写方法给空 body：具体字段缺失会 400，但仍证明路由存在（非 API_NOT_FOUND）
        let body = match method {
            "PUT" | "POST" | "PATCH" => Some("{}".to_string()),
            _ => None,
        };
        let (status, resp) = send_json(&state, http_method, path, body).await;
        if is_api_not_found(status, &resp) {
            failures.push(format!("{method} {path}：端点缺失（API_NOT_FOUND）"));
            continue;
        }
        // 只在成功响应上校验形状（模块未启用等返回 success=false 时跳过）
        if status == StatusCode::OK && resp.get("success").and_then(|v| v.as_bool()) == Some(true)
        {
            if let Some(data) = resp.get("data") {
                if let Err(e) = check_contract_shape(shape, data)
                    .and_then(|()| check_contract_fields(entry, data))
                {
                    failures.push(format!("{method} {path}：{e}"));
                }
            }
        }
    }
    assert!(
        failures.is_empty(),
        "WebUI 契约不符 {} 项：\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `geoIpInfo` 的真实聚合：对每个去重 IP 查库，四个维度输出 KV 数组（按 value 降序）。
#[tokio::test]
async fn geo_ip_aggregates_distinct_banned_ips() {
    use pbh_core::geoip::{
        CityData, CountryData, IpGeoData, NetworkData, StaticGeoIp,
    };
    use pbh_core::model::TorrentData;

    let db = Arc::new(Database::open_in_memory().unwrap());
    let record = |ip: &str, ban_at_ms: i64| pbh_db::HistoryRecord {
        ban_at_ms,
        unban_at_ms: 0,
        ip: ip.to_string(),
        port: 6881,
        peer_id: Some("peer".into()),
        peer_client_name: Some("client".into()),
        peer_uploaded: Some(1),
        peer_downloaded: Some(2),
        peer_progress: 0.1,
        downloader_progress: 0.2,
        torrent: TorrentData {
            hash: "hash-1".into(),
            name: "t".into(),
            progress: 1.0,
            total_size: 1_000_000,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: Some(false),
        },
        module_name: "peer-id-blacklist".into(),
        rule_name: r#"{"key":"R","params":[]}"#.into(),
        description: r#"{"key":"D","params":[]}"#.into(),
        flags: Some("U".into()),
        downloader: "qb".into(),
        structured_data: None,
        peer_geoip: None,
    };
    // 三个去重 IP：两个有地理信息，一个查不到（计为 N/A）
    for (ip, ms) in [("1.2.3.4", 1_000i64), ("5.6.7.8", 2_000), ("9.9.9.9", 3_000)] {
        db.insert_history(&record(ip, ms)).unwrap();
    }

    let mut geo = StaticGeoIp::new();
    geo.insert(
        "1.2.3.4".parse().unwrap(),
        IpGeoData {
            city: Some(CityData {
                name: Some("Beijing".into()),
                cn_province: Some("北京市".into()),
                cn_city: Some("北京".into()),
                ..Default::default()
            }),
            country: Some(CountryData {
                name: Some("中国".into()),
                iso: None,
            }),
            network: Some(NetworkData {
                isp: Some("电信".into()),
                net_type: None,
            }),
            as_data: None,
        },
    );
    geo.insert(
        "5.6.7.8".parse().unwrap(),
        IpGeoData {
            city: Some(CityData {
                cn_province: Some("上海市".into()),
                cn_city: Some("上海".into()),
                ..Default::default()
            }),
            country: Some(CountryData {
                name: Some("中国".into()),
                iso: None,
            }),
            network: Some(NetworkData {
                isp: Some("联通".into()),
                net_type: None,
            }),
            as_data: None,
        },
    );

    let mut state = test_state(db);
    state.geo = Some(Arc::new(geo));
    let (status, body) = get_json(
        &state,
        "/api/chart/geoIpInfo?bannedOnly=true&startAt=0&endAt=10000",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data = body["data"].clone();

    // region：中国 2 / N/A 1，按 value 降序
    let region = data["region"].as_array().expect("region 数组");
    assert_eq!(region.len(), 2);
    assert_eq!(region[0], json!({ "key": "中国", "value": 2 }));
    assert_eq!(region[1], json!({ "key": "N/A", "value": 1 }));

    // city：上游是 `cnProvince + " " + cnCity`
    let city = data["city"].as_array().expect("city 数组");
    let keys: Vec<&str> = city.iter().map(|kv| kv["key"].as_str().unwrap()).collect();
    assert!(keys.contains(&"北京市 北京"), "city 取值: {keys:?}");
    assert!(keys.contains(&"上海市 上海"));
    assert!(keys.contains(&"N/A"));

    let isp = data["isp"].as_array().expect("isp 数组");
    let isp_keys: Vec<&str> = isp.iter().map(|kv| kv["key"].as_str().unwrap()).collect();
    assert!(isp_keys.contains(&"电信") && isp_keys.contains(&"联通"));

    let province = data["province"].as_array().expect("province 数组");
    let p_keys: Vec<&str> = province.iter().map(|kv| kv["key"].as_str().unwrap()).collect();
    assert!(p_keys.contains(&"北京市") && p_keys.contains(&"上海市"));
}

#[tokio::test]
async fn logs_history_and_manifest() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let (status, body) = get_json(&state, "/api/logs/history").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("data").is_some());
    let (status, body) = get_json(&state, "/api/metadata/manifest").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("data").is_some());
}

#[tokio::test]
async fn oobe_status_and_logout() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let (_, body) = get_json(&state, "/api/init/token").await;
    assert_eq!(
        body.pointer("/data/initialized").and_then(|v| v.as_bool()),
        Some(true),
        "token 已配置时 initialized 必须为 true"
    );
    let request = Request::builder()
        .method("POST")
        .uri("/api/auth/logout")
        .header("Authorization", "Bearer test-token")
        .body(Body::empty())
        .expect("请求构造");
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn unauthenticated_requests_rejected() {
    // 认证契约：无凭据访问受保护端点必须 401（不得裸奔/不得 302 丢响应体）
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let request = Request::builder()
        .uri("/api/statistic/counter")
        .body(Body::empty())
        .expect("请求构造");
    let response = build_router(state).oneshot(request).await.expect("路由调用");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn downloader_missing_returns_404() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let (status, _) = get_json(&state, "/api/downloaders/no-such-id/status").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "不存在的下载器必须 404");
}

#[tokio::test]
async fn downloaders_put_delete_roundtrip() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let request = Request::builder()
        .method("PUT")
        .uri("/api/downloaders")
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"id":"new1","config":{"type":"qbittorrent","endpoint":"http://127.0.0.1:9091"}}"#))
        .expect("请求构造");
    let response = build_router(state.clone())
        .oneshot(request)
        .await
        .expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK, "PUT 下载器应 200");
    let request = Request::builder()
        .method("DELETE")
        .uri("/api/downloaders/new1")
        .header("Authorization", "Bearer test-token")
        .body(Body::empty())
        .expect("请求构造");
    let response = build_router(state)
        .oneshot(request)
        .await
        .expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK, "DELETE 下载器应 200");
}

#[tokio::test]
async fn push_crud_and_test() {
    // push CRUD 全链路（对齐上游 PBHPushController：PUT/GET/PATCH/DELETE/test）
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let router = build_router(state.clone());
    let put = Request::builder()
        .method("PUT")
        .uri("/api/push")
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"name":"ch1","type":"webhook","endpoint":"http://127.0.0.1:18080/pushhook"}"#))
        .expect("请求构造");
    let response = router.clone().oneshot(put).await.expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK, "PUT 渠道应 200");
    let (status, _) = get_json(&state, "/api/push").await;
    assert_eq!(status, StatusCode::OK, "GET 渠道列表应 200");
    let patch = Request::builder()
        .method("PATCH")
        .uri("/api/push/ch1")
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"name":"ch1","type":"webhook"}"#))
        .expect("请求构造");
    let response = router.clone().oneshot(patch).await.expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK, "PATCH 渠道应 200");
    let post = Request::builder()
        .method("POST")
        .uri("/api/push/test")
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{"name":"ch1","type":"webhook"}"#))
        .expect("请求构造");
    let response = router.clone().oneshot(post).await.expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK, "test 渠道应 200");
    let delete = Request::builder()
        .method("DELETE")
        .uri("/api/push/ch1")
        .header("Authorization", "Bearer test-token")
        .body(Body::empty())
        .expect("请求构造");
    let response = router.oneshot(delete).await.expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK, "DELETE 渠道应 200");
}

#[tokio::test]
async fn oobe_endpoints_contract() {
    // OOBE 向导端点（Role.ANYONE：向导在认证前运行——不带凭据直测）
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let router = build_router(state.clone());
    // `/api/oobe/scanDownloader` 会真实探测本机端口（8s+），这里不请求；
    // 端点存在性由契约清单（带 skip 原因）与 pbh-downloader 的探测测试覆盖。
    for (method, path, body) in [
        ("POST", "/api/oobe/testDownloader", r#"{"type":"qbittorrent","endpoint":"http://127.0.0.1:9091"}"#),
        ("POST", "/api/oobe/testDatabaseConfig", r#"{}"#),
    ] {
        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .expect("请求构造");
        let response = router.clone().oneshot(request).await.expect("路由调用");
        assert_eq!(response.status(), StatusCode::OK, "{path} 应 200");
        let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("响应体");
        let v: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|e| panic!("{path} 必须返回 JSON（SPA 假 200 回归）：{e}"));
        assert_eq!(v.get("success").and_then(|s| s.as_bool()), Some(true), "{path}");
    }
}

#[tokio::test]
#[ignore = "真实扫描本机 LISTEN 端口（8s+，依赖本机服务）；逻辑由 pbh-downloader::discovery 测试覆盖"]
async fn downloaders_scan_returns_empty_list() {
    let state = test_state(Arc::new(Database::open_in_memory().unwrap()));
    let request = Request::builder()
        .method("POST")
        .uri("/api/downloaders/scan")
        .header("Authorization", "Bearer test-token")
        .header("Content-Type", "application/json")
        .body(Body::from(r#"{}"#))
        .expect("请求构造");
    let response = build_router(state).oneshot(request).await.expect("路由调用");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), usize::MAX).await.expect("响应体");
    let body: Value = serde_json::from_slice(&bytes).expect("必须是 JSON");
    // 上游返回 `List<DiscoveredDownloader>`（裸数组），不是 `{downloaders: []}`
    assert!(body.get("data").and_then(Value::as_array).is_some());
}
