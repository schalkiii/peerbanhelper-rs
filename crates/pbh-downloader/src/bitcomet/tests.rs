use super::*;
use crate::http::HttpResponse;
use base64::Engine as _;
use serde_json::{json, Value};

/// `GET_IP_FILTER_CONFIG`：IP 过滤器已启用且为黑名单模式（无需修正）。
const IPFILTER_OK: &str = r#"{
  "ip_filter_config": {
    "enable_ipfilter": true,
    "ipfilter_mode": "blacklist",
    "loaded_record_count": 12
  },
  "version": "2.21.5"
}"#;

/// 同上，但 IP 过滤器未启用 → 登录时需要自动修正。
const IPFILTER_DISABLED: &str = r#"{
  "ip_filter_config": {
    "enable_ipfilter": false,
    "ipfilter_mode": "blacklist"
  }
}"#;

/// `GET_TASK_LIST`：两个 BT 任务（11 / 12）与一个非 BT 任务（13）。
const TASK_LIST: &str = r#"{
  "tasks": [
    { "task_id": 11, "type": "BT" },
    { "task_id": 12, "type": "BT" },
    { "task_id": 13, "type": "HTTP" }
  ]
}"#;

/// 任务 11：公开种子，infohash v1 与 v2 都有。
const SUMMARY_11: &str = r#"{
  "error_code": "ok",
  "task_detail": {
    "type": "BT",
    "torrent_private": false,
    "infohash": "1111111111111111111111111111111111111111",
    "infohash_v2": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "task_name": "ubuntu.iso",
    "total_size": 1000
  },
  "task_status": { "status": "working", "total_size": 1000, "download_permillage": 425 },
  "task_summary": { "tags": "" },
  "task": {
    "task_id": 11,
    "type": "BT",
    "task_name": "ubuntu.iso",
    "status": "working",
    "total_size": 1000,
    "selected_downloaded_size": 425,
    "download_rate": 20,
    "upload_rate": 10,
    "permillage": 425
  }
}"#;

/// 任务 12：私有种子，且只有 infohash v2（`infohash` 回退分支）。
const SUMMARY_12: &str = r#"{
  "error_code": "ok",
  "task_detail": {
    "type": "BT",
    "torrent_private": true,
    "infohash": null,
    "infohash_v2": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    "task_name": "private.bin",
    "total_size": 2000
  },
  "task_status": { "status": "working", "download_permillage": 1000 },
  "task": {
    "task_id": 12,
    "type": "BT",
    "task_name": "private.bin",
    "selected_downloaded_size": 2000,
    "download_rate": 0,
    "upload_rate": 5
  }
}"#;

/// 任务 11 的 peers：4 条有效（两组） + 2 条应被 group 过滤。
const PEERS: &str = r#"{
  "error_code": "ok",
  "peer_count": { "peers_connected": 3, "peers_connecting": 1 },
  "peers": [
    {
      "ip": "9.9.9.9",
      "client_type": "-TR1000-abcdefghij",
      "flag": "K",
      "remote_port": 6881,
      "listen_port": 0,
      "permillage": 555,
      "dl_rate": 202,
      "up_rate": 101,
      "dl_size": 404,
      "up_size": 303,
      "peer_id": "2d5452313030302d6162636465666768696a",
      "group": "peers_connected"
    },
    {
      "ip": "1.2.3.4:7000",
      "client_type": null,
      "remote_port": 1,
      "permillage": 0,
      "dl_rate": 0,
      "up_rate": 0,
      "dl_size": null,
      "up_size": null,
      "peer_id": "2d5452",
      "group": "ltseeds_connected"
    },
    {
      "ip": "64:ff9b::102:304",
      "remote_port": 7001,
      "permillage": 100,
      "dl_rate": 1,
      "up_rate": 2,
      "dl_size": 3,
      "up_size": 4,
      "peer_id": "2d54523",
      "group": "peers_connected"
    },
    {
      "ip": "[2001:db8::1]:7002",
      "remote_port": 1111,
      "permillage": 0,
      "dl_rate": 0,
      "up_rate": 0,
      "peer_id": "00",
      "group": "peers_connected"
    },
    { "ip": "5.5.5.5", "remote_port": 7003, "peer_id": "00", "group": "peers_connecting" },
    { "ip": "6.6.6.6", "remote_port": 7004, "peer_id": "00" }
  ],
  "task": { "task_id": 11, "type": "BT", "task_name": "ubuntu.iso" }
}"#;

/// `GET_STATISTICS_LIST`（>= 2.20：字节数令牌）。
const STATISTICS_BYTES: &str = r#"{
  "value_list": [
    { "token": "{G_TOTAL_DOWNLOAD_BYTE}", "value": "333444" },
    { "token": "{G_TOTAL_UPLOAD_BYTE}", "value": "111222" },
    { "token": "{G_SESSION_UPLOAD_BYTE}", "value": "1" }
  ]
}"#;

/// `GET_STATISTICS_LIST`（< 2.20：人类可读值令牌）。
const STATISTICS_HUMAN: &str = r#"{
  "value_list": [
    { "token": "${G_TOTAL_DOWNLOAD_AUTO}", "value": "2.33 GB" },
    { "token": "${G_TOTAL_UPLOAD_AUTO}", "value": "1.5 MB" }
  ]
}"#;

/// 被记录到的请求。
#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    url: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
}

impl Recorded {
    fn header(&self, name: &str) -> String {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    }
    fn json(&self) -> Value {
        serde_json::from_str(self.body.as_deref().unwrap_or_default()).unwrap_or(Value::Null)
    }
}

/// BitComet WebUI 的内存实现：按路径返回夹具并记录收到的请求。
struct BitCometMock {
    requests: Mutex<Vec<Recorded>>,
    /// `GET_IP_FILTER_CONFIG` 的应答
    ipfilter: Mutex<(u16, String)>,
    /// 需要携带的设备令牌；不匹配时该接口返回 401（模拟未登录状态）
    ipfilter_token: Mutex<Option<String>>,
    /// `USER_LOGIN` 的应答
    login: Mutex<(u16, String)>,
    /// `GET_DEVICE_TOKEN` 的应答
    device_token: Mutex<(u16, String)>,
    task_list: Mutex<(u16, String)>,
    /// 按 `task_id` 索引的 `GET_TASK_SUMMARY` 应答
    summaries: Mutex<HashMap<String, (u16, String)>>,
    peers: Mutex<(u16, String)>,
    statistics: Mutex<(u16, String)>,
    /// `IP_FILTER_UPLOAD` 的状态码
    upload_status: Mutex<u16>,
    /// `TASK_UNBAN_PEERS` 的状态码
    unban_status: Mutex<u16>,
    /// 为 true 时所有请求都返回传输层错误
    transport_error: Mutex<bool>,
}

impl BitCometMock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            ipfilter: Mutex::new((200, IPFILTER_OK.to_string())),
            ipfilter_token: Mutex::new(None),
            login: Mutex::new((
                200,
                json!({
                    "error_code": "ok",
                    "error_message": "",
                    "invite_token": "invite-1",
                    "version": "2.21.5"
                })
                .to_string(),
            )),
            device_token: Mutex::new((
                200,
                json!({
                    "device_token": "device-token-1",
                    "server_id": "server-1",
                    "server_name": "BitComet",
                    "version": "2.21.5"
                })
                .to_string(),
            )),
            task_list: Mutex::new((200, TASK_LIST.to_string())),
            summaries: Mutex::new(HashMap::new()),
            peers: Mutex::new((200, PEERS.to_string())),
            statistics: Mutex::new((200, STATISTICS_BYTES.to_string())),
            upload_status: Mutex::new(200),
            unban_status: Mutex::new(200),
            transport_error: Mutex::new(false),
        })
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn requests_to(&self, suffix: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.url.ends_with(suffix))
            .collect()
    }

    /// 第一个 URL 以 `suffix` 结尾的请求
    fn request(&self, suffix: &str) -> Recorded {
        self.requests_to(suffix)
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("未收到以 {suffix} 结尾的请求"))
    }
}

impl HttpFetcher for BitCometMock {
    fn execute<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, anyhow::Result<HttpResponse>> {
        Box::pin(async move {
            if *self.transport_error.lock().unwrap() {
                anyhow::bail!("error sending request for url ({})", req.url);
            }
            self.requests.lock().unwrap().push(Recorded {
                method: req.method.clone(),
                url: req.url.clone(),
                headers: req.headers.clone(),
                body: req.body.clone(),
            });
            let url = req.url.clone();
            let respond = |status: u16, body: String| Ok(HttpResponse::new(status, body));
            if url.ends_with(dto::GET_IP_FILTER_CONFIG) {
                let required = self.ipfilter_token.lock().unwrap().clone();
                if let Some(required) = required {
                    if req.header("Authorization").unwrap_or_default()
                        != format!("Bearer {required}")
                    {
                        return respond(401, r#"{"error_code":"UNAUTHORIZED"}"#.to_string());
                    }
                }
                let (status, body) = self.ipfilter.lock().unwrap().clone();
                return respond(status, body);
            }
            if url.ends_with(dto::SET_IP_FILTER_CONFIG) {
                return respond(200, r#"{"error_code":"ok","version":"2.21.5"}"#.to_string());
            }
            if url.ends_with(dto::GET_CONNECTION_CONFIG) {
                return respond(
                    200,
                    r#"{"error_code":"ok","connection_config":{"max_upload_speed":1048576,"max_download_speed":2097152}}"#
                        .to_string(),
                );
            }
            if url.ends_with(dto::SET_CONNECTION_CONFIG) {
                return respond(200, r#"{"error_code":"ok","version":"2.21.5"}"#.to_string());
            }
            if url.ends_with(dto::USER_LOGIN) {
                let (status, body) = self.login.lock().unwrap().clone();
                return respond(status, body);
            }
            if url.ends_with(dto::GET_DEVICE_TOKEN) {
                let (status, body) = self.device_token.lock().unwrap().clone();
                return respond(status, body);
            }
            if url.ends_with(dto::GET_TASK_LIST) {
                let (status, body) = self.task_list.lock().unwrap().clone();
                if status != 200 {
                    return respond(status, body);
                }
                // 模拟 BitComet 2.23 的分页语义：按请求里的 start/limit 切片
                let request: Value = req
                    .body
                    .as_deref()
                    .and_then(|b| serde_json::from_str(b).ok())
                    .unwrap_or(Value::Null);
                let (Some(start), Some(limit)) = (
                    request["start"].as_i64(),
                    request["limit"].as_i64().filter(|limit| *limit > 0),
                ) else {
                    return respond(status, body);
                };
                let mut parsed: Value = match serde_json::from_str(&body) {
                    Ok(parsed) => parsed,
                    Err(_) => return respond(status, body),
                };
                let tasks = parsed["tasks"].as_array().cloned().unwrap_or_default();
                let start = (start.max(0) as usize).min(tasks.len());
                let window = &tasks[start..tasks.len().min(start + limit as usize)];
                parsed["tasks"] = json!(window);
                return respond(status, parsed.to_string());
            }
            if url.ends_with(dto::GET_TASK_SUMMARY) {
                let body: Value = req
                    .body
                    .as_deref()
                    .and_then(|b| serde_json::from_str(b).ok())
                    .unwrap_or(Value::Null);
                let task_id = body["task_id"].as_str().unwrap_or_default().to_string();
                let (status, body) = self
                    .summaries
                    .lock()
                    .unwrap()
                    .get(&task_id)
                    .cloned()
                    .unwrap_or((404, String::new()));
                return respond(status, body);
            }
            if url.ends_with(dto::GET_TASK_PEERS) {
                let (status, body) = self.peers.lock().unwrap().clone();
                return respond(status, body);
            }
            if url.ends_with(dto::GET_STATISTICS_LIST) {
                let (status, body) = self.statistics.lock().unwrap().clone();
                return respond(status, body);
            }
            if url.ends_with(dto::IP_FILTER_UPLOAD) {
                return respond(*self.upload_status.lock().unwrap(), "{}".to_string());
            }
            if url.ends_with(dto::TASK_UNBAN_PEERS) {
                return respond(*self.unban_status.lock().unwrap(), "{}".to_string());
            }
            respond(404, String::new())
        })
    }
}

/// 组装一个「探测接口要求 device-token-1」的 mock：第一次 `login()` 会走完整握手。
fn mock_with_version(version: &str, ipfilter_body: &str) -> Arc<BitCometMock> {
    let mock = BitCometMock::new();
    *mock.ipfilter_token.lock().unwrap() = Some("device-token-1".to_string());
    *mock.ipfilter.lock().unwrap() = (200, ipfilter_body.to_string());
    *mock.login.lock().unwrap() = (
        200,
        json!({
            "error_code": "ok",
            "error_message": "",
            "invite_token": "invite-1",
            "version": version
        })
        .to_string(),
    );
    *mock.device_token.lock().unwrap() = (
        200,
        json!({
            "device_token": "device-token-1",
            "server_id": "server-1",
            "server_name": "BitComet",
            "version": version
        })
        .to_string(),
    );
    *mock.summaries.lock().unwrap() = HashMap::from([
        ("11".to_string(), (200, SUMMARY_11.to_string())),
        ("12".to_string(), (200, SUMMARY_12.to_string())),
    ]);
    mock
}

fn downloader(mock: Arc<BitCometMock>, ignore_private: bool) -> Arc<BitCometDownloader> {
    let config = BitCometConfig {
        id: "bitcomet-test".into(),
        name: "BitComet Test".into(),
        // 结尾斜杠用于验证「只去掉一个」
        endpoint: "http://mock.local/".into(),
        username: "admin".into(),
        password: "adminadmin".into(),
        ignore_private,
        ..BitCometConfig::default()
    };
    Arc::new(BitCometDownloader::with_fetcher(config, mock as Arc<dyn HttpFetcher>).unwrap())
}

/// 登录并在失败时直接 panic（多数用例的前置条件）。
async fn logged_in(dl: &BitCometDownloader) {
    let login = dl.login().await.expect("login 调用不应返回 Err");
    assert!(login.success, "登录失败: {}", login.message);
}

/// 对齐上游 OkHttp interceptor：每个请求都必须带这三个头。
fn assert_common_headers(req: &Recorded) {
    assert_eq!(
        req.method, "POST",
        "BitComet WebUI 只会收到 POST: {}",
        req.url
    );
    assert_eq!(
        req.header("Content-Type"),
        "application/json",
        "缺少 Content-Type: {:?}",
        req.headers
    );
    assert_eq!(
        req.header("Client-Type"),
        "BitComet WebUI",
        "缺少 Client-Type"
    );
    assert_eq!(
        req.header("User-Agent"),
        "PeerBanHelper BitComet Adapter",
        "缺少 User-Agent"
    );
}

fn torrent_by_hash<'a>(torrents: &'a [TorrentData], hash: &str) -> &'a TorrentData {
    torrents
        .iter()
        .find(|t| t.hash == hash)
        .unwrap_or_else(|| panic!("未找到 torrent {hash}"))
}

#[tokio::test]
async fn login_performs_full_handshake_and_fixes_ip_filter() {
    let mock = mock_with_version("2.21.5", IPFILTER_DISABLED);
    let dl = downloader(mock.clone(), false);
    let login = dl.login().await.unwrap();
    assert!(login.success, "{}", login.message);
    assert_eq!(login.version, "2.21.5");
    assert_eq!(dl.server_version(), "2.21.5");

    let requests = mock.requests();
    assert_eq!(
        requests
            .iter()
            .map(|r| format!("{} {}", r.method, r.url))
            .collect::<Vec<_>>(),
        vec![
            format!("POST http://mock.local{}", dto::GET_IP_FILTER_CONFIG),
            format!("POST http://mock.local{}", dto::USER_LOGIN),
            format!("POST http://mock.local{}", dto::GET_DEVICE_TOKEN),
            format!("POST http://mock.local{}", dto::GET_IP_FILTER_CONFIG),
            format!("POST http://mock.local{}", dto::SET_IP_FILTER_CONFIG),
        ]
    );
    requests.iter().for_each(assert_common_headers);

    // 第一次探测尚未持有设备令牌；登录后的探测带上新令牌；登录请求本身不带令牌
    assert_eq!(requests[0].header("Authorization"), "Bearer ");
    assert_eq!(requests[1].header("Authorization"), "");
    assert_eq!(requests[3].header("Authorization"), "Bearer device-token-1");

    // 登录载荷：固定 client_id + AES 凭据（03 01 | t(8) | r(8) | iv(16) | 密文 | hmac(32)）
    let login_body = requests[1].json();
    assert_eq!(login_body["client_id"], crypto::CLIENT_ID);
    let raw = base64::engine::general_purpose::STANDARD
        .decode(login_body["authentication"].as_str().unwrap())
        .unwrap();
    assert_eq!(&raw[..2], &[3, 1]);
    assert_eq!(
        (raw.len() - 2 - 8 - 8 - 16 - 32) % 16,
        0,
        "密文必须是 16 字节块"
    );
    assert!(!raw.windows(12).any(|w| w == b"adminadmin"), "凭据必须加密");

    // device_token：用 invite_token 换取设备令牌
    let device_body = requests[2].json();
    assert_eq!(
        device_body,
        json!({
            "device_id": crypto::CLIENT_ID,
            "device_name": "PeerBanHelper - BitComet Adapter",
            "invite_token": "invite-1",
            "platform": "webui"
        })
    );
    assert_eq!(requests[2].header("Authorization"), "Bearer invite-1");

    // IP 过滤器自动修正为「启用 + 黑名单」
    assert_eq!(
        requests[4].json(),
        json!({ "ip_filter_config": { "enable_ipfilter": true, "ipfilter_mode": "blacklist" } })
    );
}

#[tokio::test]
async fn login_reuses_existing_session_when_probe_succeeds() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    let dl = downloader(mock.clone(), false);
    // 第一轮：完整握手（探测 401 → 登录 → 换令牌 → 探测成功，配置本就是黑名单模式）
    let first = dl.login().await.unwrap();
    assert!(first.success, "{}", first.message);
    assert_eq!(mock.requests().len(), 4);

    // 第二轮：探测成功 → 直接复用会话
    let second = dl.login().await.unwrap();
    assert!(second.success, "{}", second.message);
    assert!(second.message.contains("工作正常"), "{}", second.message);
    let requests = mock.requests();
    assert_eq!(requests.len(), 5);
    assert_eq!(
        requests[4].url,
        format!("http://mock.local{}", dto::GET_IP_FILTER_CONFIG)
    );
}

#[tokio::test]
async fn login_is_short_circuited_when_paused() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    let config = BitCometConfig {
        endpoint: "http://mock.local".into(),
        paused: true,
        ..BitCometConfig::default()
    };
    let dl =
        BitCometDownloader::with_fetcher(config, mock.clone() as Arc<dyn HttpFetcher>).unwrap();
    let login = dl.login().await.unwrap();
    assert!(!login.success);
    assert!(login.message.contains("暂停"), "{}", login.message);
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn login_reports_incorrect_credentials() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    *mock.login.lock().unwrap() = (
        200,
        json!({
            "error_code": "PASSWORD_ERROR",
            "error_message": "password error",
            "version": "2.21.5"
        })
        .to_string(),
    );
    let dl = downloader(mock.clone(), false);
    let login = dl.login().await.unwrap();
    assert!(!login.success);
    assert!(login.message.contains("凭据"), "{}", login.message);
    // 凭据错误时不再换 device token（探测 + 登录 = 2 次请求）
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test]
async fn login_rejects_old_version_and_odd_error_code() {
    // 2.17.9 < 2.18
    let mock = mock_with_version("2.17.9", IPFILTER_OK);
    let dl = downloader(mock.clone(), false);
    let login = dl.login().await.unwrap();
    assert!(!login.success);
    assert!(login.message.contains("2.18"), "{}", login.message);
    assert_eq!(login.version, "2.17.9");
    assert_eq!(
        mock.requests().len(),
        2,
        "版本不支持时不应继续换 device token"
    );

    // 其它错误码 → DOWNLOADER_LOGIN_EXCEPTION，并把响应对象的 toString 作为参数
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    *mock.login.lock().unwrap() = (
        200,
        json!({ "error_code": "TOKEN_EXPIRED", "error_message": "expired" }).to_string(),
    );
    let dl = downloader(mock.clone(), false);
    let login = dl.login().await.unwrap();
    assert!(!login.success);
    assert!(
        login.message.contains("无法连接到下载器"),
        "{}",
        login.message
    );
    assert!(login.message.contains("TOKEN_EXPIRED"), "{}", login.message);
}

#[tokio::test]
async fn login_reports_server_and_network_failures() {
    // 登录接口 5xx：错误信息里带状态码与响应体
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    *mock.login.lock().unwrap() = (500, "boom".to_string());
    let dl = downloader(mock.clone(), false);
    let login = dl.login().await.unwrap();
    assert!(!login.success);
    assert!(login.message.contains("500"), "{}", login.message);
    assert!(login.message.contains("boom"), "{}", login.message);

    // device_token 接口 5xx
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    *mock.device_token.lock().unwrap() = (500, "boom".to_string());
    let dl = downloader(mock.clone(), false);
    let login = dl.login().await.unwrap();
    assert!(!login.success);
    assert!(login.message.contains("500"), "{}", login.message);

    // 传输层错误 → DOWNLOADER_LOGIN_IO_EXCEPTION
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    *mock.transport_error.lock().unwrap() = true;
    let dl = downloader(mock.clone(), false);
    let login = dl.login().await.unwrap();
    assert!(!login.success);
    assert!(login.message.contains("网络错误"), "{}", login.message);
}

#[tokio::test]
async fn torrents_are_mapped_and_private_filtered() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;

    let torrents = dl.fetch_torrents().await.unwrap();
    // 非 BT 任务（13）没有概要应答，因此无 404 之外的条目
    assert_eq!(torrents.len(), 2);

    let first = torrent_by_hash(&torrents, "1111111111111111111111111111111111111111");
    assert_eq!(first.name, "ubuntu.iso");
    // download_permillage 425 → 0.425
    assert_eq!(first.progress, 0.425);
    assert_eq!(first.total_size, 1000);
    assert_eq!(first.completed_override, Some(425));
    assert_eq!(first.completed_size(), 425);
    assert_eq!(first.dlspeed, 20);
    assert_eq!(first.upspeed, 10);
    assert!(!first.is_private());

    // infohash 为空 → 回退 infohash_v2；私有种子默认包含（ignore-private = false）
    let second = torrent_by_hash(
        &torrents,
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    );
    assert_eq!(second.name, "private.bin");
    assert!(second.is_private());
    assert_eq!(second.progress, 1.0);
    assert_eq!(second.upspeed, 5);

    // 请求载荷（`getTorrents()` 的 requirements；9.5.2 起按 50 条分页并带 2.23 字段）
    let list_request = mock.request(dto::GET_TASK_LIST);
    assert_eq!(
        list_request.json(),
        json!({
            "state_group": "ACTIVE",
            "sort_key": "",
            "sort_order": "unsorted",
            "tag_filter": "ALL",
            "task_type": "ALL",
            "keyword": "",
            "query_mode": "stateless",
            "validate_task_ids": [],
            "start": 0,
            "limit": 50
        })
    );
    // 第 2 页（start=50）为空 → 上游语义：取回 0 条即结束分页
    let list_requests = mock.requests_to(dto::GET_TASK_LIST);
    assert_eq!(list_requests.len(), 2, "空页后应停止分页");
    assert_eq!(list_requests[1].json()["start"], 50);
    let summaries = mock.requests_to(dto::GET_TASK_SUMMARY);
    assert_eq!(summaries.len(), 2, "只应为 BT 任务请求概要");
    assert_eq!(summaries[0].json(), json!({ "task_id": "11" }));
    assert_eq!(summaries[1].json(), json!({ "task_id": "12" }));

    // ignore-private = true：私有种子被跳过
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    let dl = downloader(mock.clone(), true);
    logged_in(&dl).await;
    let torrents = dl.fetch_torrents().await.unwrap();
    assert_eq!(torrents.len(), 1);
    assert_eq!(torrents[0].hash, "1111111111111111111111111111111111111111");
}

#[tokio::test]
async fn speed_limiter_get_and_set() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;
    // 对齐 `getSpeedLimiter()`：GET_CONNECTION_CONFIG（bytes/s）
    let (up, dl_) = dl.get_speed_limiter().await.unwrap();
    assert_eq!(up, 1_048_576);
    assert_eq!(dl_, 2_097_152);
    // 对齐 `setSpeedLimiter(...)`：SET_CONNECTION_CONFIG
    dl.set_speed_limiter(0, 0).await.unwrap();
}

#[tokio::test]
async fn torrent_summary_failures_are_skipped() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    mock.summaries
        .lock()
        .unwrap()
        .insert("11".to_string(), (500, "boom".to_string()));
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;

    let torrents = dl.fetch_torrents().await.unwrap();
    assert_eq!(torrents.len(), 1, "概要请求失败的任务被跳过");
    assert_eq!(
        torrents[0].hash,
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    );

    // 任务列表整体失败：上游 9.5.2 改为「记日志 + 跳过该页继续下一页」，
    // 因此不再返回 Err，而是返回已取到的（这里是空）列表；分页有防御上界。
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    *mock.task_list.lock().unwrap() = (500, "boom".to_string());
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;
    let torrents = dl.fetch_torrents().await.expect("非 2xx 不得让 fetch 失败");
    assert!(torrents.is_empty());
    assert_eq!(
        mock.requests_to(dto::GET_TASK_LIST).len(),
        crate::bitcomet::MAX_TASK_LIST_PAGES,
        "持续失败时必须在防御上界处停止，而不是无限分页"
    );
}

#[tokio::test]
async fn task_list_paginates_and_accumulates_across_pages() {
    // 60 个 BT 任务 > 默认分页 50：应连续翻页并累计，第三页空 → 结束
    let tasks: Vec<Value> = (1..=60_i64).map(|id| json!({ "task_id": id, "type": "BT" })).collect();
    let mock = mock_with_version("2.23.0", IPFILTER_OK);
    *mock.task_list.lock().unwrap() = (200, json!({ "tasks": tasks }).to_string());
    {
        let mut summaries = mock.summaries.lock().unwrap();
        for id in 1..=60_i64 {
            let hash = format!("{id:040x}");
            summaries.insert(
                id.to_string(),
                (
                    200,
                    json!({
                        "error_code": "ok",
                        "task_detail": {
                            "type": "BT",
                            "torrent_private": false,
                            "infohash": hash,
                            "task_name": format!("t{id}"),
                            "total_size": 1000
                        },
                        "task_status": { "status": "working", "total_size": 1000, "download_permillage": 500 },
                        "task": {
                            "task_id": id,
                            "type": "BT",
                            "task_name": format!("t{id}"),
                            "status": "working",
                            "total_size": 1000,
                            "selected_downloaded_size": 500,
                            "download_rate": 1,
                            "upload_rate": 2,
                            "permillage": 500
                        }
                    })
                    .to_string(),
                ),
            );
        }
    }
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;
    let torrents = dl.fetch_torrents().await.unwrap();
    assert_eq!(torrents.len(), 60, "两页任务都应返回");

    let list_requests = mock.requests_to(dto::GET_TASK_LIST);
    let starts: Vec<i64> = list_requests
        .iter()
        .map(|request| request.json()["start"].as_i64().unwrap_or_default())
        .collect();
    assert_eq!(starts, vec![0, 50, 100], "应按 limit=50 递增 start 直到空页");
    assert!(
        list_requests
            .iter()
            .all(|request| request.json()["limit"] == json!(50)),
        "每页都必须带分页大小"
    );
}

#[tokio::test]
async fn peers_are_mapped_with_raw_ip_and_translation() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;
    let torrents = dl.fetch_torrents().await.unwrap();
    let torrent = torrent_by_hash(&torrents, "1111111111111111111111111111111111111111");
    let peers = dl.fetch_peers(torrent).await.unwrap();
    // 两组之外的 peer（peers_connecting / group 缺失）被丢弃
    assert_eq!(peers.len(), 4);

    // peers 请求：groups 固定两组、task_id 取 fetch_torrents 缓存的数字 id、max_count = 0
    let request = mock.request(dto::GET_TASK_PEERS);
    assert_eq!(
        request.json(),
        json!({
            "groups": ["peers_connected", "ltseeds_connected"],
            "task_id": "11",
            "max_count": 0
        })
    );

    // 按下载器原始 `ip:port` 取（NAT64 翻译后与纯 IPv4 peer 的 `ip` 会重合）
    let by_raw: HashMap<&str, &PeerData> =
        peers.iter().map(|p| (p.raw_ip.as_str(), p)).collect();

    // 普通 peer：peer_id 十六进制解码（不截断），flags 恒为 None（上游传 null）
    let normal = by_raw.get("9.9.9.9:6881").expect("peer 9.9.9.9");
    assert_eq!(normal.ip, "9.9.9.9");
    assert_eq!(normal.raw_ip, "9.9.9.9:6881");
    assert_eq!(normal.port, 6881);
    assert_eq!(normal.peer_id.as_deref(), Some("-TR1000-abcdefghij"));
    assert_eq!(normal.client_name.as_deref(), Some("-TR1000-abcdefghij"));
    assert_eq!(normal.dl_speed, 202);
    assert_eq!(normal.up_speed, 101);
    assert_eq!(normal.downloaded, 404);
    assert_eq!(normal.uploaded, 303);
    assert_eq!(normal.progress, 0.555);
    assert!(normal.flags.is_none());
    assert!(normal.connection.is_none());
    assert!(!normal.is_handshaking());

    // ip 字段自带端口时优先于 remote_port；dl_size/up_size 为 null 按 -1 兼容
    let explicit = by_raw.get("1.2.3.4:7000").expect("peer 1.2.3.4:7000");
    assert_eq!(explicit.ip, "1.2.3.4");
    assert_eq!(explicit.port, 7000);
    assert_eq!(explicit.downloaded, -1);
    assert_eq!(explicit.uploaded, -1);
    assert_eq!(explicit.peer_id.as_deref(), Some("-TR"));
    assert_eq!(explicit.client_name, None);
    assert!(explicit.is_handshaking());

    // NAT64 → 翻译为 IPv4，raw_ip 保留下载器原始 ip:port；奇数长度 hex 前置补 0
    let translated = by_raw.get("64:ff9b::102:304:7001").expect("NAT64 peer");
    assert_eq!(translated.ip, "1.2.3.4");
    assert_eq!(translated.port, 7001);
    assert_eq!(translated.peer_id.as_deref(), Some("\u{2}\u{d5}E#"));
    assert_eq!(translated.progress, 0.1);

    // 带方括号的 IPv6：解析出显式端口，地址不再重写
    let v6 = by_raw.get("2001:db8::1:7002").expect("IPv6 peer");
    assert_eq!(v6.ip, "2001:db8::1");
    assert_eq!(v6.port, 7002);
    assert_eq!(v6.peer_id.as_deref(), Some("\0"));
}

#[tokio::test]
async fn peers_null_is_empty_and_errors_are_propagated() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;
    let torrents = dl.fetch_torrents().await.unwrap();
    let torrent = torrent_by_hash(&torrents, "1111111111111111111111111111111111111111");

    // `peers: null` → 空列表（上游返回空列表而非报错）
    *mock.peers.lock().unwrap() = (200, r#"{"error_code":"ok","peers":null}"#.to_string());
    assert!(dl.fetch_peers(torrent).await.unwrap().is_empty());

    // 非 2xx → Err（DOWNLOADER_BC_FAILED_REQUEST_PEERS_LIST_IN_TORRENT）
    *mock.peers.lock().unwrap() = (500, "boom".to_string());
    let err = dl.fetch_peers(torrent).await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("Peers"), "{text}");
    assert!(text.contains("500"), "{text}");
}

#[tokio::test]
async fn incremental_ban_posts_merge_import() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;

    dl.ban_peers(&[
        BanEntry {
            ip: "1.2.3.4".into(),
            port: 6881,
            raw_ip: "1.2.3.4:6881".into(),
        },
        // 同一地址重复出现 → 去重（对齐 `distinct()`）
        BanEntry {
            ip: "1.2.3.4".into(),
            port: 7000,
            raw_ip: "1.2.3.4:7000".into(),
        },
        BanEntry {
            ip: "2001:db8::1".into(),
            port: 1,
            raw_ip: "[2001:db8::1]:1".into(),
        },
    ])
    .await
    .unwrap();

    let request = mock.request(dto::IP_FILTER_UPLOAD);
    let body = request.json();
    // BitComet 的封禁是「文件导入」：import_type + data_type + Base64 文件内容
    assert_eq!(body["import_type"], "merge");
    assert_eq!(body["data_type"], "data_file");
    let content = base64::engine::general_purpose::STANDARD
        .decode(body["content_base64"].as_str().unwrap())
        .unwrap();
    // remapBanListAddress(addr) → supportRangeBan = true：去重 + IPv6 /52 网段
    assert_eq!(
        String::from_utf8(content).unwrap(),
        "1.2.3.4\n::ffff:102:304\n2001:db8::1\n2001:db8::/52"
    );
}

#[tokio::test]
async fn full_ban_replaces_import_file() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;

    dl.replace_banned_ips(&["1.2.3.4".to_string(), "2001:db8::1".to_string()])
        .await
        .unwrap();

    let request = mock.request(dto::IP_FILTER_UPLOAD);
    let body = request.json();
    assert_eq!(body["import_type"], "replace");
    assert_eq!(body["data_type"], "data_file");
    let content = base64::engine::general_purpose::STANDARD
        .decode(body["content_base64"].as_str().unwrap())
        .unwrap();
    assert_eq!(
        String::from_utf8(content).unwrap(),
        "1.2.3.4\n::ffff:102:304\n2001:db8::1\n2001:db8::/52"
    );
}

#[tokio::test]
async fn ban_failures_return_error_and_unban_follows_upstream() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    *mock.upload_status.lock().unwrap() = 500;
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;

    let err = dl
        .ban_peers(&[BanEntry {
            ip: "1.2.3.4".into(),
            port: 1,
            raw_ip: "1.2.3.4:1".into(),
        }])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("statusCode=500"), "{err}");
    let err = dl
        .replace_banned_ips(&["1.2.3.4".to_string()])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("statusCode=500"), "{err}");

    // 解封接口（上游 `unbanPeers`）：`ip_list` + `unban_range`
    dl.unban_peers(&["1.2.3.4:6881".to_string()]).await.unwrap();
    assert_eq!(
        mock.request(dto::TASK_UNBAN_PEERS).json(),
        json!({ "ip_list": ["1.2.3.4:6881"], "unban_range": "unban_peers_in_all_tasks" })
    );

    *mock.unban_status.lock().unwrap() = 500;
    let err = dl
        .unban_peers(&["1.2.3.4:6881".to_string()])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("statusCode=500"), "{err}");
}

#[tokio::test]
async fn statistics_reads_byte_tokens_and_human_readable_values() {
    // >= 2.21：令牌去掉前导 `$`，2.20 起返回字节数
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;
    let stats = dl.statistics().await.unwrap();
    assert_eq!(stats.all_time_upload, 111_222);
    assert_eq!(stats.all_time_download, 333_444);
    assert_eq!(
        mock.request(dto::GET_STATISTICS_LIST).json(),
        json!({
            "token_list": [
                "{G_TOTAL_DOWNLOAD_AUTO}",
                "{G_TOTAL_UPLOAD_AUTO}",
                "{G_SESSION_DOWNLOAD_AUTO}",
                "{G_SESSION_UPLOAD_AUTO}",
                "{G_TOTAL_DOWNLOAD_BYTE}",
                "{G_TOTAL_UPLOAD_BYTE}",
                "{G_SESSION_DOWNLOAD_BYTE}",
                "{G_SESSION_UPLOAD_BYTE}"
            ]
        })
    );

    // < 2.20：令牌带 `$`，值为人类可读
    let mock = mock_with_version("2.19.0", IPFILTER_OK);
    *mock.statistics.lock().unwrap() = (200, STATISTICS_HUMAN.to_string());
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;
    let stats = dl.statistics().await.unwrap();
    assert_eq!(stats.all_time_upload, 1_572_864); // 1.5 MB
    assert_eq!(stats.all_time_download, 2_501_818_449); // 2.33 GB
    let token_list = mock.request(dto::GET_STATISTICS_LIST).json()["token_list"].clone();
    assert_eq!(token_list[0], "${G_TOTAL_DOWNLOAD_AUTO}");
    assert_eq!(token_list[4], "${G_TOTAL_DOWNLOAD_BYTE}");
}

#[tokio::test]
async fn statistics_failure_returns_error_like_upstream() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    *mock.statistics.lock().unwrap() = (500, "boom".to_string());
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;

    let err = dl.statistics().await.unwrap_err();
    let text = err.to_string();
    // 上游：warn 后抛 IllegalStateException(tl(..., "N/A", "N/A", e))
    assert!(text.contains("N/A"), "{text}");
    assert!(text.contains("500"), "{text}");
    assert!(text.contains("boom"), "{text}");
}

#[tokio::test]
async fn feature_flags_match_upstream_and_order() {
    let mock = mock_with_version("2.21.5", IPFILTER_OK);
    let dl = downloader(mock.clone(), false);
    // 未登录时上游会 NPE；本实现按「版本未知」处理（无 TRAFFIC_STATS）
    assert_eq!(
        dl.feature_flags(),
        vec![
            "UNBAN_IP".to_string(),
            "LIVE_UPDATE_BT_PROTOCOL_PORT".to_string(),
            "RANGE_BAN_IP".to_string(),
        ]
    );

    logged_in(&dl).await;
    assert_eq!(
        dl.feature_flags(),
        vec![
            "UNBAN_IP".to_string(),
            "LIVE_UPDATE_BT_PROTOCOL_PORT".to_string(),
            "TRAFFIC_STATS".to_string(),
            "RANGE_BAN_IP".to_string(),
        ]
    );
    assert_eq!(dl.downloader_type(), "bitcomet");
    assert_eq!(dl.id(), "bitcomet-test");
    assert_eq!(dl.name(), "BitComet Test");

    // 2.19.0 < 2.20 → 无 TRAFFIC_STATS
    let mock = mock_with_version("2.19.0", IPFILTER_OK);
    let dl = downloader(mock.clone(), false);
    logged_in(&dl).await;
    assert_eq!(
        dl.feature_flags(),
        vec![
            "UNBAN_IP".to_string(),
            "LIVE_UPDATE_BT_PROTOCOL_PORT".to_string(),
            "RANGE_BAN_IP".to_string(),
        ]
    );
}

#[test]
fn address_parsing_follows_host_and_port_rules() {
    assert_eq!(
        parse_address("9.9.9.9", 6881),
        ("9.9.9.9".to_string(), 6881)
    );
    assert_eq!(
        parse_address("  9.9.9.9  ", 6881),
        ("9.9.9.9".to_string(), 6881)
    );
    // 恰好一个冒号 → host:port
    assert_eq!(
        parse_address("1.2.3.4:7000", 1),
        ("1.2.3.4".to_string(), 7000)
    );
    // 方括号 IPv6
    assert_eq!(
        parse_address("[2001:db8::1]:7002", 1),
        ("2001:db8::1".to_string(), 7002)
    );
    assert_eq!(
        parse_address("[2001:db8::1]", 5),
        ("2001:db8::1".to_string(), 5)
    );
    // 裸 IPv6（2 个以上冒号）→ 整体视为主机名，回退 remote_port
    assert_eq!(
        parse_address("2001:db8::1", 7001),
        ("2001:db8::1".to_string(), 7001)
    );
    // 端口非法：上游会抛 NumberFormatException，这里退回「整串为主机名」
    assert_eq!(
        parse_address("1.2.3.4:abc", 5),
        ("1.2.3.4:abc".to_string(), 5)
    );
}

#[test]
fn peer_id_hex_decode_follows_byte_util() {
    assert_eq!(
        decode_peer_id_hex("2d5452313030302d6162636465666768696a"),
        "-TR1000-abcdefghij"
    );
    // 奇数长度：前置补 0
    assert_eq!(decode_peer_id_hex("2d54523"), "\u{2}\u{d5}E#");
    assert_eq!(decode_peer_id_hex("00"), "\0");
    assert_eq!(decode_peer_id_hex(""), "");
    // 非法十六进制：退回已解码前缀
    assert_eq!(decode_peer_id_hex("2d5g"), "-");
}

#[test]
fn human_readable_values_match_upstream_switch() {
    assert_eq!(read_human_readable_value("1 B"), 1);
    assert_eq!(read_human_readable_value("1 KB"), 1024);
    assert_eq!(read_human_readable_value("1 mb"), 1_048_576);
    assert_eq!(read_human_readable_value("2 GB"), 2_147_483_648);
    assert_eq!(read_human_readable_value("1 TB"), 1_099_511_627_776);
    // 拆不出两段 / 数值非法 / 未知单位 → 0
    assert_eq!(read_human_readable_value("2.33"), 0);
    assert_eq!(read_human_readable_value("2.33  GB"), 0);
    assert_eq!(read_human_readable_value("abc GB"), 0);
    assert_eq!(read_human_readable_value("1 XB"), 0);
}

#[test]
fn loose_semver_comparison_matches_semver4j() {
    assert!(version_at_least("2.21.5.28", 2, 21, 0));
    assert!(version_at_least("2.20", 2, 20, 0));
    assert!(!version_at_least("2.19.9", 2, 20, 0));
    assert!(version_at_least("3.0.0", 2, 20, 0));
    assert!(!version_at_least("", 2, 20, 0));
    assert_eq!(parse_loose_semver("v2.18"), Some((2, 18, 0)));
    assert_eq!(parse_loose_semver("2.18"), Some((2, 18, 0)));
    assert_eq!(parse_loose_semver("2.18.1-beta1"), Some((2, 18, 1)));
    assert_eq!(parse_loose_semver("2.18.1+build.5"), Some((2, 18, 1)));
    assert_eq!(parse_loose_semver("abc"), None);
}
