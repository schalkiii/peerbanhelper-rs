use super::*;
use crate::i18n::TranslationComponent;
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, PeerAction, RuleModule};

// ---------------------------------------------------------- 测试脚手架

/// 脚本化 mock（URL 精确匹配；未命中 ⇒ 404）
#[derive(Debug, Default)]
struct MockHttpClient {
    scripts: StdMutex<Vec<(String, Script)>>,
    requests: StdMutex<Vec<BtnHttpRequest>>,
}

#[derive(Clone, Debug)]
enum Script {
    Body(u16, String),
    Headers(u16, String, Vec<(String, String)>),
    Transport(String),
}

/// 200 + body
fn ok(body: impl Into<String>) -> Script {
    Script::Body(200, body.into())
}

impl MockHttpClient {
    fn serve(self, url: &str, script: Script) -> Self {
        self.scripts.lock().unwrap().push((url.to_string(), script));
        self
    }

    fn call_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    fn urls(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.url.clone())
            .collect()
    }

    fn headers_of(&self, url: &str) -> Vec<(String, String)> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.url == url)
            .map(|r| r.headers.clone())
            .unwrap_or_default()
    }

    fn body_of(&self, url: &str) -> Vec<u8> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.url == url)
            .and_then(|r| r.body.clone())
            .unwrap_or_default()
    }
}

/// 内存数据源（测试用）：按游标语义返回行
#[derive(Debug, Default)]
struct StubSubmitSource {
    bans: StdMutex<Vec<BtnBanHistoryRow>>,
    swarms: StdMutex<Vec<BtnSwarmHistoryRow>>,
    peers: StdMutex<Vec<BtnPeerHistoryRow>>,
    legacy_peers: StdMutex<Vec<BtnLegacyPeerRow>>,
    legacy_bans: StdMutex<Vec<BtnLegacyBanRow>>,
}

impl BtnSubmitSource for StubSubmitSource {
    fn batch_ban_history(&self, cursor_id: i64, limit: usize) -> Vec<BtnBanHistoryRow> {
        self.bans
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.id > cursor_id)
            .take(limit)
            .cloned()
            .collect()
    }

    fn batch_swarm_history(
        &self,
        last_time_seen_ms: i64,
        id_after: i64,
        limit: usize,
    ) -> Vec<BtnSwarmHistoryRow> {
        self.swarms
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.last_time_seen_ms >= last_time_seen_ms && row.id > id_after)
            .take(limit)
            .cloned()
            .collect()
    }

    fn batch_peer_history(
        &self,
        last_time_seen_ms: i64,
        limit: usize,
    ) -> Vec<BtnPeerHistoryRow> {
        self.peers
            .lock()
            .unwrap()
            .iter()
            .filter(|row| row.last_time_seen_ms >= last_time_seen_ms)
            .take(limit)
            .cloned()
            .collect()
    }

    fn legacy_peer_snapshot(&self) -> Vec<BtnLegacyPeerRow> {
        self.legacy_peers.lock().unwrap().clone()
    }

    fn legacy_ban_snapshot(&self) -> Vec<BtnLegacyBanRow> {
        self.legacy_bans.lock().unwrap().clone()
    }
}

/// 读取第 `index` 个能力的状态副本（测试断言用）。
fn abi_state(network: &BtnNetwork, index: usize) -> BtnAbility {
    network
        .abilities
        .read()
        .unwrap()
        .get(index)
        .cloned()
        .expect("能力存在")
}

/// 解压 gzip（`flate2` 读取端），用于校验上报载荷。
fn gunzip(data: &[u8]) -> Vec<u8> {
    use flate2::read::GzDecoder;
    use std::io::Read;

    let mut decoded = Vec::new();
    GzDecoder::new(data)
        .read_to_end(&mut decoded)
        .expect("gzip 载荷可解压");
    decoded
}

impl BtnHttpClient for MockHttpClient {
    fn execute(&self, request: BtnHttpRequest) -> anyhow::Result<BtnHttpResponse> {
        self.requests.lock().unwrap().push(request.clone());
        let scripts = self.scripts.lock().unwrap();
        match scripts
            .iter()
            .find(|(url, _)| *url == request.url)
            .map(|(_, s)| s)
        {
            Some(Script::Body(status, body)) => Ok(BtnHttpResponse::new(*status, body.clone())),
            Some(Script::Headers(status, body, headers)) => Ok(BtnHttpResponse {
                status: *status,
                body: body.clone(),
                headers: headers.iter().cloned().collect(),
            }),
            Some(Script::Transport(message)) => Err(anyhow::anyhow!(message.clone())),
            None => Ok(BtnHttpResponse::new(404, "not found")),
        }
    }
}

const CONFIG_URL: &str = "https://btn.test/config";
const RULES_URL: &str = "https://btn.test/rules";
const ALLOW_URL: &str = "https://btn.test/allowlist";
const DENY_URL: &str = "https://btn.test/denylist";
const POW_URL: &str = "https://btn.test/pow";

fn active_config() -> BtnNetworkConfig {
    BtnNetworkConfig {
        enabled: true,
        config_url: CONFIG_URL.to_string(),
        ..Default::default()
    }
}

fn config_body() -> String {
    serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "ability": {
            "rule_peer_identity": {
                "endpoint": RULES_URL,
                "interval": 86400000,
                "random_initial_delay": 3600000,
                "pow_captcha": false
            },
            "ip_allowlist": { "endpoint": ALLOW_URL, "interval": 86400000, "random_initial_delay": 0 },
            "ip_denylist": { "endpoint": DENY_URL, "interval": 86400000, "random_initial_delay": 0 }
        }
    })
    .to_string()
}

fn ruleset_body(version: &str) -> String {
    serde_json::json!({
        "version": version,
        "peer_id": { "xunlei": ["{\"method\":\"STARTS_WITH\",\"content\":\"-hp\"}"] }
    })
    .to_string()
}

fn make_network(config: BtnNetworkConfig, http: Arc<dyn BtnHttpClient>) -> BtnNetwork {
    BtnNetwork::new(config, http, Arc::new(InMemoryMetadataStore::new()))
}

fn peer(ip: &str, port: u16, peer_id: &str, client: &str) -> PeerData {
    PeerData {
        client_name: Some(client.to_string()),
        peer_id: Some(peer_id.to_string()),
        dl_speed: 1000,
        downloaded: 1000,
        up_speed: 1000,
        uploaded: 1000,
        progress: 0.5,
        flags: Some("d u".to_string()),
        ip: ip.to_string(),
        port,
        raw_ip: format!("{ip}:{port}"),
        connection: Some("uTP".to_string()),
    }
}

fn torrent() -> TorrentData {
    TorrentData {
        hash: "h".to_string(),
        name: "t".to_string(),
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

fn check(module: &BtnNetworkOnline, peer: &PeerData) -> crate::module::CheckResult {
    module.check("qbittorrent", &torrent(), peer, &CheckContext::default())
}

// ---------------------------------------------------------------- 测试

/// 未配置 BTN（`enabled=false` / `config-url` 为空）⇒ 零网络请求，模块恒 pass
#[test]
fn unconfigured_btn_issues_no_requests() {
    let http = Arc::new(MockHttpClient::default().serve(CONFIG_URL, ok(config_body())));
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);

    for config in [
        BtnNetworkConfig::default(),
        BtnNetworkConfig {
            enabled: true,
            ..Default::default()
        },
        BtnNetworkConfig {
            config_url: CONFIG_URL.to_string(),
            ..Default::default()
        },
    ] {
        let network = make_network(config, http.clone());
        assert!(!network.check_if_need_retry_config_at(&module, 1_000));
        assert!(!network.sync(&module));
        assert!(network.sync_due(&module, 1_000).is_empty());
    }
    assert_eq!(http.call_count(), 0, "未配置时一个请求都不能发");
    assert!(!module.is_manager_initialized());
    let result = check(
        &module,
        &peer("1.2.3.4", 51413, "-hp001-abcdefghijkl", "Xunlei"),
    );
    assert_eq!(result.action, PeerAction::NoAction);
    assert_eq!(result.data["status"], "pass");
}

/// 配置端点握手成功 ⇒ abilities 就位 ⇒ 规则集被拉取并注入 ⇒ 命中即封禁
#[test]
fn config_fetch_then_ruleset_is_applied() {
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(config_body()))
            .serve(&format!("{RULES_URL}?rev=initial"), ok(ruleset_body("v1")))
            .serve(
                &format!("{ALLOW_URL}?rev=initial"),
                Script::Body(204, String::new()),
            )
            .serve(
                &format!("{DENY_URL}?rev=initial"),
                ok("2.2.2.0/24 # 恶意\n"),
            ),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());

    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    assert!(network.config_result().is_success());
    assert_eq!(network.abilities().len(), 3);
    // 上游注册顺序：ip_denylist → ip_allowlist → rule_peer_identity
    assert_eq!(network.abilities()[0].kind, BtnAbilityKind::IpDenyList);
    assert_eq!(network.abilities()[1].kind, BtnAbilityKind::IpAllowList);
    assert_eq!(network.abilities()[2].kind, BtnAbilityKind::Rules);

    assert!(network.sync(&module));
    assert_eq!(module.ruleset_version().as_deref(), Some("v1"));
    assert_eq!(
        check(
            &module,
            &peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "qBittorrent")
        )
        .action,
        PeerAction::Ban
    );
    // 黑名单同样注入
    assert_eq!(
        check(
            &module,
            &peer("2.2.2.9", 12345, "-qb0000-abcdefghijkl", "qBittorrent")
        )
        .action,
        PeerAction::Ban
    );
    assert!(http.urls().contains(&format!("{RULES_URL}?rev=initial")));
}

/// `rev` 未变（服务端 204）⇒ 不重新注入；`X-BTN-ContentVersion` 作为下一次 rev
#[test]
fn unchanged_content_version_uses_cache() {
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(config_body()))
            .serve(&format!("{RULES_URL}?rev=initial"), ok(ruleset_body("v1")))
            .serve(
                &format!("{RULES_URL}?rev=v1"),
                Script::Body(204, String::new()),
            )
            .serve(
                &format!("{ALLOW_URL}?rev=initial"),
                Script::Headers(
                    200,
                    "3.3.3.3 # 白\n".to_string(),
                    vec![(HEADER_X_BTN_CONTENT_VERSION.to_string(), "cv-1".to_string())],
                ),
            )
            .serve(
                &format!("{ALLOW_URL}?rev=cv-1"),
                Script::Body(204, String::new()),
            )
            .serve(
                &format!("{DENY_URL}?rev=initial"),
                Script::Body(204, String::new()),
            ),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    assert!(network.sync(&module));

    let calls_after_first = http.call_count();
    // 第二次同步：`rev` 已变为 v1 / cv-1 ⇒ 服务端 204 ⇒ 不重新注入
    assert!(!network.sync(&module));
    assert_eq!(module.ruleset_version().as_deref(), Some("v1"));
    assert!(http.urls().contains(&format!("{RULES_URL}?rev=v1")));
    assert!(http.urls().contains(&format!("{ALLOW_URL}?rev=cv-1")));
    assert!(http.call_count() > calls_after_first);
    // 允许列表命中 ⇒ SKIP
    let result = check(
        &module,
        &peer("3.3.3.3", 51413, "-hp001-abcdefghijkl", "Xunlei"),
    );
    assert_eq!(result.action, PeerAction::Skip);
}

/// 服务端不可达 ⇒ 模块仍 pass、不崩溃，并按 600s 重试
#[test]
fn unreachable_server_keeps_module_inert() {
    let http = Arc::new(MockHttpClient::default().serve(
        CONFIG_URL,
        Script::Transport("connection refused".to_string()),
    ));
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());

    assert!(!network.check_if_need_retry_config_at(&module, 1_000));
    assert!(matches!(
        network.config_result(),
        BtnConfigStatus::Exception { .. }
    ));
    assert!(!network.sync(&module));
    let result = check(
        &module,
        &peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "Xunlei"),
    );
    assert_eq!(result.action, PeerAction::NoAction);
    assert_eq!(result.data["status"], "pass");

    // 重试间隔（600s）内不再发起请求
    assert!(!network.check_if_need_retry_config_at(&module, 1_000 + 1_000));
    assert_eq!(http.call_count(), 1);
    // 超过重试间隔后再次尝试
    assert!(!network
        .check_if_need_retry_config_at(&module, 1_000 + RETRY_PERIOD_SECONDS * 1000 + 1));
    assert_eq!(http.call_count(), 2);
}

/// HTTP 500 / 缺字段 / 版本不兼容 ⇒ 记录上游文案键且不封禁
#[test]
fn config_failures_are_reported_with_upstream_keys() {
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);

    // HTTP 500
    let http = Arc::new(
        MockHttpClient::default().serve(CONFIG_URL, Script::Body(500, "boom".to_string())),
    );
    let network = make_network(active_config(), http.clone());
    assert!(!network.check_if_need_retry_config_at(&module, 1_000));
    assert!(matches!(
        network.config_result(),
        BtnConfigStatus::HttpError { status: 500, .. }
    ));
    assert_eq!(
        network.config_result().lang_key(),
        LANG_BTN_CONFIG_STATUS_UNSUCCESSFUL_HTTP_REQUEST
    );
    assert!(!module.is_manager_initialized());

    // 缺 min_protocol_version
    let http = Arc::new(MockHttpClient::default().serve(CONFIG_URL, ok("{}")));
    let network = make_network(active_config(), http.clone());
    assert!(!network.check_if_need_retry_config_at(&module, 1_000));
    assert!(matches!(
        network.config_result(),
        BtnConfigStatus::Exception { .. }
    ));

    // 服务端协议过新（客户端过旧）
    let body = serde_json::json!({ "min_protocol_version": 21, "max_protocol_version": 21, "ability": {} })
        .to_string();
    let http = Arc::new(MockHttpClient::default().serve(CONFIG_URL, ok(body)));
    let network = make_network(active_config(), http.clone());
    assert!(!network.check_if_need_retry_config_at(&module, 1_000));
    assert_eq!(
        network.config_result(),
        BtnConfigStatus::ClientTooOld {
            implemented: BTN_PROTOCOL_IMPL_VERSION,
            min: 21
        }
    );

    // 服务端协议过旧
    let body = serde_json::json!({ "min_protocol_version": 1, "max_protocol_version": 19, "ability": {} })
        .to_string();
    let http = Arc::new(MockHttpClient::default().serve(CONFIG_URL, ok(body)));
    let network = make_network(active_config(), http.clone());
    assert!(!network.check_if_need_retry_config_at(&module, 1_000));
    assert!(matches!(
        network.config_result(),
        BtnConfigStatus::ServerTooOld { .. }
    ));

    // 遗留协议（`min < 20`）只构造 rules 能力，`ip_*` 不构造
    // （`max` 必须 >= 实现版本，否则会先命中上面的 SERVER_TOO_OLD 分支）
    let body = serde_json::json!({
        "min_protocol_version": 10,
        "max_protocol_version": 20,
        "ability": { "rules": { "endpoint": RULES_URL, "interval": 1000, "random_initial_delay": 0 } }
    })
    .to_string();
    let http = Arc::new(MockHttpClient::default().serve(CONFIG_URL, ok(body)));
    let network = make_network(active_config(), http.clone());
    // 遗留协议本身是**握手成功**（只是不含 ip_* 能力）⇒ `check_if_need_retry_config_at` 返回 true
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    assert_eq!(network.abilities().len(), 1);
    assert_eq!(network.abilities()[0].kind, BtnAbilityKind::Rules);
    assert_eq!(network.abilities()[0].key, "rules");
}

/// 未实现的 ability（submit_* 等）不构造、不调度、不发请求
#[test]
#[allow(non_snake_case)]
fn submit_disabled_by_default_not_registered() {
    // 上游 `if (submit)` 分支：`btn.submit` 缺省 false ⇒ submit_* 能力不注册
    let body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "ability": {
            "submit_bans": { "endpoint": "https://btn.test/submit-bans", "interval": 1000, "random_initial_delay": 0 },
            "rule_peer_identity": { "endpoint": RULES_URL, "interval": 1000, "random_initial_delay": 0 }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body))
            .serve(&format!("{RULES_URL}?rev=initial"), ok(ruleset_body("v1"))),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    assert_eq!(
        network.abilities().len(),
        1,
        "submit=false ⇒ 仅注册 rule 能力"
    );
    assert!(network.sync(&module));
    assert_eq!(module.ruleset_version().as_deref(), Some("v1"));
    assert!(!http
        .urls()
        .iter()
        .any(|u| u.contains("submit-bans") || u.contains("/hb")));
}

/// 认证请求头与匿名账户分支（对齐 `setupHttpClient` 的拦截器）
#[test]
fn request_headers_match_upstream_interceptor() {
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let http = Arc::new(MockHttpClient::default().serve(CONFIG_URL, ok(config_body())));
    let config = BtnNetworkConfig {
        app_id: "app".to_string(),
        app_secret: "secret".to_string(),
        ..active_config()
    };
    let network = make_network(config, http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    let headers = http.headers_of(CONFIG_URL);
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(get("User-Agent"), BTN_USER_AGENT);
    assert_eq!(get("Content-Type"), "application/json");
    assert_eq!(get(HEADER_BTN_APP_ID), "app");
    assert_eq!(get(HEADER_X_BTN_APP_SECRET), "secret");
    assert_eq!(get(HEADER_AUTHENTICATION), "Bearer app@secret");
    assert!(!headers
        .iter()
        .any(|(k, _)| k == HEADER_X_BTN_INSTALLATION_ID));

    // 匿名账户（占位值）⇒ 附加 installation-id
    let http = Arc::new(MockHttpClient::default().serve(CONFIG_URL, ok(config_body())));
    let config = BtnNetworkConfig {
        app_id: "example-app-id".to_string(),
        app_secret: "example-app-secret".to_string(),
        installation_id: "inst-1".to_string(),
        ..active_config()
    };
    let network = make_network(config, http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    let headers = http.headers_of(CONFIG_URL);
    assert!(headers
        .iter()
        .any(|(k, v)| k == HEADER_X_BTN_INSTALLATION_ID && v == "inst-1"));
}

/// PoW captcha：求解成功时追加 `X-BTN-PowID` / `X-BTN-PowSolution`
#[test]
fn pow_captcha_is_solved_and_attached() {
    let challenge = b"challenge-bytes";
    let pow_body = serde_json::json!({
        "id": "pow-1",
        "challengeBase64": base64_encode(challenge),
        "difficultyBits": 8,
        "algorithm": "SHA-256",
        "expireAt": 0
    })
    .to_string();
    let config_body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "proof_of_work_captcha": { "endpoint": POW_URL },
        "ability": {
            "rule_peer_identity": {
                "endpoint": RULES_URL,
                "interval": 1000,
                "random_initial_delay": 0,
                "pow_captcha": true
            }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(config_body))
            .serve(&format!("{POW_URL}?type=rule_peer_identity"), ok(pow_body))
            .serve(&format!("{RULES_URL}?rev=initial"), ok(ruleset_body("v1"))),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    assert!(network.sync(&module));

    let headers = http.headers_of(&format!("{RULES_URL}?rev=initial"));
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(get(HEADER_X_BTN_POW_ID), "pow-1");
    let solution = base64_decode(&get(HEADER_X_BTN_POW_SOLUTION)).expect("base64 解出 nonce");
    assert_eq!(solution.len(), 8, "上游 `ByteBuffer.putLong` ⇒ 8 字节");
    assert!(has_leading_zero_bits(
        &PowAlgorithm::Sha256.digest(challenge, &solution),
        8
    ));
}

/// PoW 端点不可用 / 算法不支持 ⇒ 不加头、继续拉取（上游 `catch (Throwable)` 分支）
#[test]
fn pow_captcha_failure_skips_headers_but_continues() {
    let config_body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "proof_of_work_captcha": { "endpoint": POW_URL },
        "ability": {
            "rule_peer_identity": { "endpoint": RULES_URL, "interval": 1000, "random_initial_delay": 0, "pow_captcha": true }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(config_body))
            .serve(
                &format!("{POW_URL}?type=rule_peer_identity"),
                Script::Body(500, "nope".to_string()),
            )
            .serve(&format!("{RULES_URL}?rev=initial"), ok(ruleset_body("v1"))),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    assert!(network.sync(&module), "PoW 失败不得阻断规则拉取");
    let headers = http.headers_of(&format!("{RULES_URL}?rev=initial"));
    assert!(!headers.iter().any(|(k, _)| k == HEADER_X_BTN_POW_ID));
    assert_eq!(module.ruleset_version().as_deref(), Some("v1"));

    // 算法不支持：同样只是跳过 PoW 头
    assert!(solve_pow(b"x", 8, "NOT-A-DIGEST").is_none());
}

/// 本地缓存：握手时从 `metadataDao` 回灌规则（`BtnAbility*.load()`）
#[test]
fn cache_is_loaded_on_handshake_and_written_on_update() {
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(config_body()))
            .serve(&format!("{RULES_URL}?rev=initial"), ok(ruleset_body("v1")))
            .serve(&format!("{DENY_URL}?rev=initial"), ok("2.2.2.0/24\n"))
            .serve(
                &format!("{ALLOW_URL}?rev=initial"),
                Script::Body(204, String::new()),
            ),
    );
    let metadata = Arc::new(InMemoryMetadataStore::new());
    let network = BtnNetwork::new(active_config(), http.clone(), metadata.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    assert!(network.sync(&module));
    assert!(metadata.get(CACHE_KEY_RULES).is_some(), "规则集写入缓存");
    assert_eq!(
        metadata.get(CACHE_KEY_IP_DENYLIST_VALUE).as_deref(),
        Some("2.2.2.0/24\n")
    );

    // 新进程（新的 BtnNetwork + 新的模块）复用同一份 metadata：握手即回灌缓存
    let fresh_module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let http2 = Arc::new(MockHttpClient::default().serve(CONFIG_URL, ok(config_body())));
    let network2 = BtnNetwork::new(active_config(), http2.clone(), metadata.clone());
    assert!(network2.check_if_need_retry_config_at(&fresh_module, 1_000));
    assert_eq!(
        fresh_module.ruleset_version().as_deref(),
        Some("v1"),
        "缓存回灌规则集"
    );
    assert_eq!(
        check(
            &fresh_module,
            &peer("2.2.2.9", 12345, "-qb0000-abcdefghijkl", "qBittorrent")
        )
        .action,
        PeerAction::Ban,
        "缓存回灌黑名单"
    );
    assert_eq!(http2.call_count(), 1, "只发了配置端点一个请求");
}

/// 允许列表更新后解封（上游 `unbanAllowedBannedPeers` / `@Subscribe onRuleUpdate`）
#[test]
fn allowlist_update_unbans_banned_peers() {
    let ban_list = Arc::new(StdMutex::new(BanList::new()));
    {
        let mut list = ban_list.lock().unwrap();
        list.add("3.3.3.3", 0, "btn", false);
        list.add("9.9.9.9", 0, "btn", false);
    }
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(config_body()))
            .serve(
                &format!("{ALLOW_URL}?rev=initial"),
                Script::Headers(
                    200,
                    "3.3.3.3\n".to_string(),
                    vec![(HEADER_X_BTN_CONTENT_VERSION.to_string(), "cv-1".to_string())],
                ),
            )
            .serve(
                &format!("{DENY_URL}?rev=initial"),
                Script::Body(204, String::new()),
            )
            .serve(
                &format!("{RULES_URL}?rev=initial"),
                Script::Body(204, String::new()),
            ),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone()).with_ban_list(ban_list.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    network.sync(&module);
    let list = ban_list.lock().unwrap();
    assert_eq!(list.len(), 1);
    assert!(list.contains("9.9.9.9"));
}

/// ability 到期调度：`sync_due` 只跑到期的 ability 并推进下一次触发时刻
#[test]
fn sync_due_only_runs_due_abilities() {
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(config_body()))
            .serve(&format!("{RULES_URL}?rev=initial"), ok(ruleset_body("v1")))
            .serve(
                &format!("{DENY_URL}?rev=initial"),
                Script::Body(204, String::new()),
            )
            .serve(
                &format!("{ALLOW_URL}?rev=initial"),
                Script::Body(204, String::new()),
            ),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));

    // ability 的 `next_due_ms` 在配置握手时按**系统时钟** + 随机初始延迟计算，
    // 因此这里必须用真实时钟（rules 的 `random_initial_delay` 为 3600000 ⇒ 通常尚未到期）
    let now = now_millis();
    let rules_due = network
        .abilities()
        .iter()
        .any(|a| a.kind == BtnAbilityKind::Rules && now >= a.next_due_ms);
    let report = network.sync_due(&module, now);
    assert_eq!(report.len(), if rules_due { 3 } else { 2 });
    assert!(report.iter().all(|r| r.last_status));
    // 下一次触发 = now + interval（上游 fixed-delay）
    let abilities = network.abilities();
    let deny = abilities
        .iter()
        .find(|a| a.kind == BtnAbilityKind::IpDenyList)
        .unwrap();
    assert_eq!(deny.next_due_ms, now + 86_400_000);
    // 未到期 ⇒ 不再触发
    assert!(network.sync_due(&module, now + 1).is_empty());
}

/// `allow-script-execute` 由传输层写入模块（默认 false ⇒ 不编译/不执行）
#[test]
fn allow_script_execute_is_propagated_to_module() {
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    assert!(!module.allow_script());
    let http = Arc::new(MockHttpClient::default().serve(CONFIG_URL, ok(config_body())));
    let config = BtnNetworkConfig {
        allow_script_execute: true,
        ..active_config()
    };
    let network = make_network(config, http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    assert!(module.allow_script());
}

/// 规则集解析失败（JSON 非法）⇒ 保留旧规则、不封禁新 peer
#[test]
fn malformed_ruleset_body_keeps_previous_rules() {
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(config_body()))
            .serve(&format!("{RULES_URL}?rev=initial"), ok(ruleset_body("v1")))
            .serve(&format!("{RULES_URL}?rev=v1"), ok("not a json"))
            .serve(
                &format!("{DENY_URL}?rev=initial"),
                Script::Body(204, String::new()),
            )
            .serve(
                &format!("{ALLOW_URL}?rev=initial"),
                Script::Body(204, String::new()),
            ),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    assert!(network.sync(&module));
    assert_eq!(module.ruleset_version().as_deref(), Some("v1"));
    assert!(!network.sync(&module));
    assert_eq!(
        module.ruleset_version().as_deref(),
        Some("v1"),
        "失败时保留旧规则集"
    );
    assert_eq!(
        check(
            &module,
            &peer("9.9.9.9", 51413, "-hp001-abcdefghijkl", "qBittorrent")
        )
        .action,
        PeerAction::Ban
    );
}

/// `reload_config` 清空状态并按上游重置重试时刻
#[test]
fn reload_config_resets_state() {
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(config_body()))
            .serve(&format!("{RULES_URL}?rev=initial"), ok(ruleset_body("v1")))
            .serve(
                &format!("{DENY_URL}?rev=initial"),
                Script::Body(204, String::new()),
            )
            .serve(
                &format!("{ALLOW_URL}?rev=initial"),
                Script::Body(204, String::new()),
            ),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    assert!(network.sync(&module));

    network.reload_config(BtnNetworkConfig::default());
    assert!(!network.config_success());
    assert!(network.abilities().is_empty());
    assert!(matches!(network.config_result(), BtnConfigStatus::Pending));
    // 关闭后不再有任何请求
    let before = http.call_count();
    assert!(!network.check_if_need_retry_config_at(&module, 1_000_000));
    assert!(!network.sync(&module));
    assert!(network.sync_due(&module, 1_000_000).is_empty());
    assert_eq!(http.call_count(), before);
    // 模块既有规则保持不变（`BtnNetwork.resetAbilities()` 不清模块规则）
    assert_eq!(module.ruleset_version().as_deref(), Some("v1"));
}

/// 文案键与常量（对齐上游 `Lang` 与协议版本）
/// 上游 9.5.2 修复「部分 Linux 发行版系统代号含非 ASCII ⇒ HTTP 请求行为异常」。
/// 本移植的出站 UA 是固定 ASCII 常量，且没有任何请求头携带 OS 派生文本，
/// 因此该缺陷无从发生——此用例钉住前提（将来若把系统信息写进 UA，必须先净化）。
#[test]
fn outbound_user_agent_is_header_safe_ascii() {
    assert!(
        BTN_USER_AGENT
            .bytes()
            .all(|byte| (0x20..=0x7e).contains(&byte)),
        "BTN UA 必须是可见 ASCII，不得含控制字符或非 ASCII：{BTN_USER_AGENT:?}"
    );
}

#[test]
fn constants_match_upstream() {
    assert_eq!(BTN_PROTOCOL_IMPL_VERSION, 20);
    assert_eq!(RETRY_PERIOD_SECONDS, 600);
    assert_eq!(CALL_TIMEOUT, Duration::from_secs(60));
    assert_eq!(CACHE_KEY_RULES, "btn.ability.rules.cache");
    assert_eq!(
        CACHE_KEY_IP_ALLOWLIST_VERSION,
        "btn.ability.ip_allowlist.cache.version"
    );
    assert_eq!(
        CACHE_KEY_IP_DENYLIST_VALUE,
        "btn.ability.ip_denylist.cache.value"
    );
    assert_eq!(
        BtnAbilityKind::from_json_key("rules"),
        BtnAbilityKind::Rules
    );
    assert_eq!(
        BtnAbilityKind::from_json_key("rule_peer_identity"),
        BtnAbilityKind::Rules
    );
    assert_eq!(
        BtnAbilityKind::from_json_key("ip_allowlist"),
        BtnAbilityKind::IpAllowList
    );
    assert_eq!(
        BtnAbilityKind::from_json_key("ip_denylist"),
        BtnAbilityKind::IpDenyList
    );
    assert_eq!(
        BtnAbilityKind::from_json_key("submit_bans"),
        BtnAbilityKind::SubmitBans
    );
    assert_eq!(
        BtnAbilityKind::from_json_key("submit_peers"),
        BtnAbilityKind::LegacySubmitPeers
    );
    assert!(BtnAbilityKind::SubmitBans.is_implemented());
    assert!(BtnAbilityKind::LegacySubmitBans.is_implemented());
    assert!(!BtnAbilityKind::Other.is_implemented());
    assert_eq!(BtnAbilityKind::Rules.pow_type(), "rule_peer_identity");
    assert_eq!(BtnAbilityKind::SubmitBans.pow_type(), "submit_bans");
    assert_eq!(BtnAbilityKind::LegacySubmitBans.pow_type(), "submit_bans");
    // `submit_history` 是单数（上游 `gatherAndSolveCaptchaBlocking` 实参逐字确认）
    assert_eq!(BtnAbilityKind::SubmitHistories.pow_type(), "submit_history");
    assert_eq!(
        BtnAbilityKind::IpAllowList.upstream_name(),
        "BtnAbilityIPAllowList"
    );
    assert_eq!(
        BtnAbilityKind::LegacySubmitBans.upstream_name(),
        "LegacyBtnAbilitySubmitBans"
    );
}

// ------------------------------------------------------------ 工具函数

#[test]
fn gzip_bytes_roundtrip() {
    let raw = br#"{"hello":"world","pad":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}"#;
    let compressed = gzip_bytes(raw).expect("gzip 可以压缩");
    assert!(!compressed.is_empty());
    assert_eq!(gunzip(&compressed), raw);
}

/// `InfoHashUtil.getHashedIdentifier`：小写 + crc32 盐 + sha256 hex
#[test]
fn hashed_identifier_matches_upstream_format() {
    let hash = get_hashed_identifier("ABCDEF0123456789ABCDEF0123456789ABCDEF01");
    assert_eq!(hash.len(), 64);
    assert!(hash
        .chars()
        .all(|c| c.is_ascii_hexdigit() && (c.is_ascii_lowercase() || c.is_ascii_digit())));
    assert_eq!(
        hash,
        get_hashed_identifier("abcdef0123456789abcdef0123456789abcdef01"),
        "infohash 先转小写"
    );
    assert_ne!(get_hashed_identifier("a"), get_hashed_identifier("b"));
}

/// `URLEncoder.encode`：空格 → `+`；保留 `-_.*`
#[test]
fn url_encode_query_matches_java_url_encoder() {
    assert_eq!(url_encode_query("1.2.3.4"), "1.2.3.4");
    assert_eq!(url_encode_query("a b+c/d"), "a+b%2Bc%2Fd");
    assert_eq!(url_encode_query("~字符"), "%7E%E5%AD%97%E7%AC%A6");
}

#[test]
fn append_url_uses_question_mark_then_ampersand() {
    assert_eq!(
        append_url("https://x.test/q", &[("ip", "1.2.3.4")]),
        "https://x.test/q?ip=1.2.3.4"
    );
    assert_eq!(
        append_url("https://x.test/q?a=1", &[("ip", "1.2.3.4")]),
        "https://x.test/q?a=1&ip=1.2.3.4"
    );
}

// ------------------------------------------------------- submit 能力

#[test]
fn submit_bans_posts_gzip_and_advances_cursor() {
    const SUBMIT_BANS_URL: &str = "https://btn.test/submit-bans";
    let source = Arc::new(StubSubmitSource {
        bans: StdMutex::new(vec![
            BtnBanHistoryRow {
                id: 1,
                ban_at_ms: 1_614_000_000_000,
                peer_ip: "1.2.3.4".to_string(),
                peer_port: 51413,
                peer_id: Some("-hp001-abcdefghijkl".to_string()),
                peer_client_name: Some("Xunlei".to_string()),
                peer_progress: 0.5,
                peer_flag: Some("d u".to_string()),
                torrent_hash: "AA12".to_string(),
                torrent_is_private: true,
                torrent_size: 1024,
                from_peer_traffic: 5000,
                to_peer_traffic: 6000,
                downloader_progress: 0.8,
                module: "module-x".to_string(),
                rule: "rule-a".to_string(),
                description: "desc-a".to_string(),
                structured_data: Some("{\"k\":1}".to_string()),
            },
            BtnBanHistoryRow {
                id: 3,
                ban_at_ms: 1_700_000_000_100,
                module: "module-x".to_string(),
                rule: "rule-a".to_string(),
                ..Default::default()
            },
        ]),
        ..Default::default()
    });
    let body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "submit": true,
        "ability": {
            "submit_bans": { "endpoint": SUBMIT_BANS_URL, "interval": 86400000, "random_initial_delay": 0, "pow_captcha": false }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body))
            .serve(SUBMIT_BANS_URL, ok(r#"{"status":"ok"}"#)),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(
        BtnNetworkConfig {
            submit: true,
            ..active_config()
        },
        http.clone(),
    )
    .with_submit_source(source);
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    // ability 到期时间按系统时钟计算（上游随机延迟 0 ⇒ 立即到期）
    let now = now_millis();
    let report = network.sync_due(&module, now);
    assert_eq!(report.len(), 1);
    assert_eq!(report[0].kind, BtnAbilityKind::SubmitBans);
    assert!(report[0].updated);
    assert!(report[0].last_status);

    let headers = http.headers_of(SUBMIT_BANS_URL);
    assert!(headers
        .iter()
        .any(|(k, v)| k == "Content-Encoding" && v == "gzip"));
    assert!(headers
        .iter()
        .any(|(k, v)| k == "Content-Type" && v == "application/json"));

    let payload: serde_json::Value =
        serde_json::from_slice(&gunzip(&http.body_of(SUBMIT_BANS_URL))).expect("gzip 可解压");
    let bans = payload["bans"].as_array().expect("bans 数组");
    assert_eq!(bans.len(), 2);
    assert_eq!(bans[0]["ban_at"], 1_614_000_000_000_i64);
    assert_eq!(bans[0]["peer_ip"], "1.2.3.4");
    assert_eq!(
        bans[0]["torrent_identifier"],
        get_hashed_identifier("AA12").as_str()
    );
    assert_eq!(bans[0]["torrent_is_private"], true);
    assert_eq!(bans[0]["module"], "module-x");
    assert_eq!(bans[0]["structured_data"], "{\"k\":1}");
    assert!(bans[0].get("id").is_none(), "行 id 是游标，不出现在载荷中");

    // 游标推进到本页最大 id（上游 `getLast().getId()`）
    match network.metadata.get(METADATA_KEY_BANS_CURSOR) {
        Some(value) => assert_eq!(value, "3"),
        None => panic!("submit_bans 游标未持久化"),
    }
    // 第二轮无新数据 ⇒ 不发请求（interval 86400000 尚未到期）
    let before = http.call_count();
    let report = network.sync_due(&module, now + 20_000);
    assert!(report.is_empty());
    assert_eq!(http.call_count(), before);
}

#[test]
fn submit_swarm_uses_binary_cursor_and_posts() {
    const SWARM_URL: &str = "https://btn.test/submit-swarm";
    let source = Arc::new(StubSubmitSource {
        swarms: StdMutex::new(vec![
            BtnSwarmHistoryRow {
                id: 5,
                last_time_seen_ms: 7000,
                torrent_hash: "aaa".to_string(),
                ..Default::default()
            },
            BtnSwarmHistoryRow {
                id: 10,
                last_time_seen_ms: 7000,
                torrent_hash: "bbb".to_string(),
                ..Default::default()
            },
        ]),
        ..Default::default()
    });
    let body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "submit": true,
        "ability": {
            "submit_swarm": { "endpoint": SWARM_URL, "interval": 86400000, "random_initial_delay": 0, "pow_captcha": false }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body))
            .serve(SWARM_URL, ok(r#"{"status":"ok"}"#)),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(
        BtnNetworkConfig {
            submit: true,
            ..active_config()
        },
        http.clone(),
    )
    .with_submit_source(source);
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    let now = now_millis();
    let report = network.sync_due(&module, now);
    assert_eq!(report.len(), 1);
    assert_eq!(report[0].kind, BtnAbilityKind::SubmitSwarm);

    let payload: serde_json::Value =
        serde_json::from_slice(&gunzip(&http.body_of(SWARM_URL))).expect("gzip 可解压");
    let swarms = payload["swarms"].as_array().expect("swarms 数组");
    assert_eq!(swarms.len(), 2);
    assert_eq!(
        swarms[1]["torrent_identifier"],
        get_hashed_identifier("bbb").as_str()
    );
    // `torrent_is_private` 可空 ⇒ `serializeNulls` 输出 null
    assert!(swarms[0]["torrent_is_private"].is_null());

    // 二元游标 `lastTimeSeen,id`
    match network.metadata.get(METADATA_KEY_SWARM_CURSOR) {
        Some(value) => assert_eq!(value, "7000,10"),
        None => panic!("submit_swarm 游标未持久化"),
    }
}

#[test]
fn submit_histories_advances_cursor_and_skips_on_lock() {
    const HIST_URL: &str = "https://btn.test/submit-histories";
    // 时间戳取相对当前时刻（上游 30 天饱和：远古时间片会被丢弃重置）
    let base = now_millis();
    let source = Arc::new(StubSubmitSource {
        peers: StdMutex::new(vec![
            BtnPeerHistoryRow {
                ip_address: "1.2.3.4".to_string(),
                port: 51413,
                torrent_hash: "ccc".to_string(),
                first_time_seen_ms: base - 20_000,
                last_time_seen_ms: base - 10_000,
                ..Default::default()
            },
            BtnPeerHistoryRow {
                ip_address: "5.6.7.8".to_string(),
                port: 1024,
                torrent_hash: "ccc".to_string(),
                first_time_seen_ms: base - 15_000,
                last_time_seen_ms: base - 5_000,
                ..Default::default()
            },
        ]),
        ..Default::default()
    });
    let body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "submit": true,
        "ability": {
            "submit_histories": { "endpoint": HIST_URL, "interval": 86400000, "random_initial_delay": 0, "pow_captcha": false }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body))
            .serve(HIST_URL, ok(r#"{"status":"ok"}"#)),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(
        BtnNetworkConfig {
            submit: true,
            ..active_config()
        },
        http.clone(),
    )
    .with_submit_source(source);
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    let now = now_millis();
    // 无游标 ⇒ 30 天饱和：从「当前时刻」开始 ⇒ 空批次不出请求
    let report = network.sync_due(&module, now);
    assert_eq!(report.len(), 1);
    assert!(!report[0].updated);
    assert_eq!(
        http.urls()
            .iter()
            .filter(|url| url.as_str() == HIST_URL)
            .count(),
        0,
        "无历史游标时不应发请求"
    );
    match network.metadata.get(METADATA_KEY_HISTORY_TIMESTAMP) {
        Some(_) => {}
        None => panic!("未写入历史游标"),
    }

    // 预置旧游标 ⇒ 发一页（populate_time 为请求时刻；游标推进到本页最大 last_time_seen）
    network
        .metadata
        .set(METADATA_KEY_HISTORY_TIMESTAMP, &(base - 30_000).to_string());
    let report = network.sync_due(&module, now + 86_400_000);
    assert_eq!(report.len(), 1);
    assert!(report[0].updated);
    let payload: serde_json::Value =
        serde_json::from_slice(&gunzip(&http.body_of(HIST_URL))).expect("gzip 可解压");
    assert_eq!(payload["peers"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        payload["peers"][0]["torrent_identifier"],
        get_hashed_identifier("ccc").as_str()
    );
    assert!(payload["populate_time"].as_i64().is_some());
    match network.metadata.get(METADATA_KEY_HISTORY_TIMESTAMP) {
        Some(value) => assert_eq!(value, (base - 5_000).to_string()),
        None => panic!("历史游标未写入"),
    }

    // tryLock 未获得 ⇒ 跳过一整轮（Skipped），lastStatus 不变
    let _locked = network.submit_history_lock.lock().unwrap();
    let before = http.call_count();
    let report = network.sync_due(&module, now + 86_400_000 * 2);
    assert_eq!(report.len(), 1, "到期但执行被跳过仍会出现在报告中");
    assert_eq!(http.call_count(), before, "跳过不得发请求");
    assert!(abi_state(&network, 0).last_status, "状态保持不变");
}

/// 上游 `LegacyBtnAbilitySubmitBans`：只上报 `banAt > lastReport`；空则推进时间不上报
#[test]
fn legacy_submit_bans_filters_by_last_report() {
    const LEGACY_BANS_URL: &str = "https://btn.test/lb";
    let source = Arc::new(StubSubmitSource {
        legacy_bans: StdMutex::new(vec![
            BtnLegacyBanRow {
                ban_at_ms: 1000,
                ban_unique_id: "uid-1".to_string(),
                module: "module".to_string(),
                rule: "rule".to_string(),
                btn_ban: true,
                peer: BtnLegacyPeerRow {
                    ip_address: "9.9.9.9".to_string(),
                    torrent_hash: "ddd".to_string(),
                    ..Default::default()
                },
                structured_data: None,
            },
            BtnLegacyBanRow {
                ban_at_ms: 500,
                ban_unique_id: "uid-0".to_string(),
                ..Default::default()
            },
        ]),
        ..Default::default()
    });
    let body = serde_json::json!({
        "min_protocol_version": 19,
        "max_protocol_version": 20,
        "submit": true,
        "ability": {
            "submit_bans": { "endpoint": LEGACY_BANS_URL, "interval": 86400000, "random_initial_delay": 0, "pow_captcha": false },
            "submit_peers": { "endpoint": "https://btn.test/lp", "interval": 86400000, "random_initial_delay": 0 }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body))
            .serve(LEGACY_BANS_URL, ok(r#"{"status":"ok"}"#))
            .serve("https://btn.test/lp", ok(r#"{"status":"ok"}"#)),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(
        BtnNetworkConfig {
            submit: true,
            ..active_config()
        },
        http.clone(),
    )
    .with_submit_source(source);
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    // 模拟上一次上报推进到了 500ms：再早的封禁应被 lastReport 过滤
    network
        .legacy_bans_last_report_ms
        .store(500, Ordering::Relaxed);
    // 主协议 min=19 ⇒ 遗留分支：legacy submit_bans + submit_peers
    assert_eq!(network.abilities().len(), 2);
    let now = now_millis();
    let report = network.sync_due(&module, now);
    assert!(
        report
            .iter()
            .any(|sync| sync.kind == BtnAbilityKind::LegacySubmitBans && sync.updated),
        "上报 1000ms 的新封禁"
    );
    let payload: serde_json::Value =
        serde_json::from_slice(&gunzip(&http.body_of(LEGACY_BANS_URL))).expect("gzip 可解压");
    let bans = payload["bans"].as_array().expect("bans 数组");
    assert_eq!(bans.len(), 1, "500ms 的旧封禁被 lastReport 过滤");
    assert_eq!(bans[0]["ban_unique_id"], "uid-1");
    assert_eq!(
        network.legacy_bans_last_report_ms.load(Ordering::Relaxed),
        1000
    );

    // 无新封禁 ⇒ EMPTY：不发请求、状态置真、lastReport 推进到当前
    let before = http.call_count();
    let report = network.sync_due(&module, now + 86_400_000);
    assert!(
        network.legacy_bans_last_report_ms.load(Ordering::Relaxed) >= now,
        "推进到当前时刻"
    );
    assert_eq!(
        http.call_count(),
        before,
        "EMPTY 分支（legacy bans + legacy peers）均不发请求"
    );
    assert!(
        report
            .iter()
            .find(|sync| sync.kind == BtnAbilityKind::LegacySubmitBans)
            .is_some_and(|sync| sync.last_status),
        "空批次状态为 true"
    );
}

// ------------------------------------------------------------- heartbeat

#[test]
fn heartbeat_default_uses_bare_client_multi_if_auth_and_failure_marks_down() {
    const HB_URL: &str = "https://btn.test/hb";
    let body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "submit": false,
        "ability": {
            "heartbeat": { "endpoint": HB_URL, "interval": 60000, "random_initial_delay": 0, "pow_captcha": false }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body))
            .serve(HB_URL, ok(r#"{"external_ip":"203.0.113.1"}"#)),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    let report = network.sync_due(&module, now_millis());
    assert_eq!(report[0].kind, BtnAbilityKind::Heartbeat);
    assert!(report[0].last_status);
    assert_eq!(http.body_of(HB_URL), br#"{"ifaddr":"default"}"#);
    // 默认接口 ⇒ 裸 client（无认证头）
    assert!(!http
        .headers_of(HB_URL)
        .iter()
        .any(|(k, _)| k == HEADER_BTN_APP_ID));

    // multi_if：每个地址一次（带认证头），任一成功即状态为 true
    const HB2_URL: &str = "https://btn.test/hb2";
    let body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "ability": {
            "heartbeat": { "endpoint": HB2_URL, "interval": 60000, "random_initial_delay": 0, "multi_if": true }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body.clone()))
            .serve(HB2_URL, ok(r#"{"external_ip":"203.0.113.2"}"#)),
    );
    let network = make_network(active_config(), http.clone())
        .with_submit_source(Arc::new(StubSubmitSource::default()))
        .with_local_ips(Arc::new(|| {
            vec!["192.168.1.5".to_string(), "10.0.0.2".to_string()]
        }));
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    let report = network.sync_due(&module, now_millis());
    assert_eq!(
        http.urls().iter().filter(|u| u.as_str() == HB2_URL).count(),
        2,
        "两个网卡各一次"
    );
    assert!(report[0].last_status);
    assert!(
        http.headers_of(HB2_URL)
            .iter()
            .any(|(k, _)| k == HEADER_BTN_APP_ID),
        "multi_if 心跳带认证头"
    );
    assert_eq!(http.body_of(HB2_URL), br#"{"ifaddr":"192.168.1.5"}"#);

    // 未注入网卡提供者 ⇒ 零地址 ⇒ 状态 false
    let http2 = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body.clone()))
            .serve(HB2_URL, ok(r#"{"external_ip":"203.0.113.2"}"#)),
    );
    let network = make_network(active_config(), http2.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    let report = network.sync_due(&module, now_millis());
    assert!(!report[0].last_status);
}

// ---------------------------------------------------------- reconfigure

#[test]
fn reconfigure_version_change_triggers_rehandshake() {
    const RECONFIGURE_URL: &str = "https://btn.test/reconfigure";
    let config_v1 = |version: &str| {
        serde_json::json!({
            "min_protocol_version": 20,
            "max_protocol_version": 20,
            "ability": {
                "reconfigure": {
                    "endpoint": RECONFIGURE_URL,
                    "interval": 60000,
                    "random_initial_delay": 0,
                    "version": version,
                    "pow_captcha": false
                }
            }
        })
        .to_string()
    };
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(config_v1("v1")))
            .serve(RECONFIGURE_URL, ok(r#"{"status":"ok"}"#)),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    assert_eq!(
        network.abilities()[0].reconfigure_version.as_deref(),
        Some("v1")
    );
    let config_requests_after_handshake = http.call_count();
    let now = now_millis();

    // 服务端更新版本 ⇒ 重新握手
    http.scripts
        .lock()
        .unwrap()
        .insert(0, (CONFIG_URL.to_string(), ok(config_v1("v2"))));
    let report = network.sync_due(&module, now + 60_000);
    assert!(report
        .iter()
        .any(|sync| sync.kind == BtnAbilityKind::Reconfigure));
    assert_eq!(
        http.call_count(),
        config_requests_after_handshake + 2,
        "reconfigure GET 一次 + 重新握手 GET 一次"
    );
    assert_eq!(
        network.abilities()[0].reconfigure_version.as_deref(),
        Some("v2")
    );

    // 版本一致 ⇒ 不重新握手：仅做一次版本检查 GET（上游每周期都会 GET 检查）
    let before = http.call_count();
    network.sync_due(&module, now + 60_000 * 2);
    assert_eq!(
        http.call_count(),
        before + 1,
        "版本一致时仅检查一次，不重新握手"
    );
    assert_eq!(
        network.abilities()[0].reconfigure_version.as_deref(),
        Some("v2")
    );
}

// -------------------------------------------------------------- ip_query

#[test]
fn query_ip_builds_authed_url_and_parses() {
    const QUERY_URL: &str = "https://btn.test/ip-query";
    let body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "ability": {
            "ip_query": { "endpoint": QUERY_URL, "interval": 60000, "random_initial_delay": 0, "pow_captcha": false }
        }
    })
    .to_string();
    let response_json = serde_json::json!({
        "color": "red",
        "labels": ["扫描行为"],
        "bans": { "duration": 1, "total": 2, "records": [] },
        "swarms": { "duration": 1, "total": 0, "records": [], "concurrent_download_torrents_count": 0, "concurrent_seeding_torrents_count": 0 },
        "traffic": { "duration": 1, "to_peer_traffic": 10, "from_peer_traffic": 20, "share_ratio": 0.5 },
        "torrents": { "duration": 1, "count": 3 }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body))
            .serve(&format!("{QUERY_URL}?ip=1.2.3.4"), ok(response_json)),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http.clone());
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    // `ip_query` 不注册调度器（is_scheduled == false）
    assert!(network.abilities().is_empty() || !network.abilities()[0].kind.is_scheduled());

    let result = network
        .query_ip("1.2.3.4")
        .expect("查询成功")
        .expect("有结果");
    assert_eq!(result.color.as_deref(), Some("red"));
    assert_eq!(result.torrents.as_ref().map(|t| t.count), Some(3));
    assert_eq!(result.traffic.as_ref().map(|t| t.to_peer_traffic), Some(10));
}
#[test]
fn rev_url_appends_rev_like_upstream() {
    let network = make_network(
        BtnNetworkConfig::default(),
        Arc::new(MockHttpClient::default()),
    );
    assert_eq!(
        network.rev_url("https://x.test/r", "v1"),
        "https://x.test/r?rev=v1"
    );
    assert_eq!(
        network.rev_url("https://x.test/r?a=b", "v1"),
        "https://x.test/r?a=b&rev=v1"
    );
}

/// PoW：`has_leading_zero_bits` 与 `solve_pow` 的边界；base64 往返
#[test]
fn pow_helpers_match_upstream_semantics() {
    assert!(has_leading_zero_bits(&[0x00, 0x01], 0));
    assert!(has_leading_zero_bits(&[0x00, 0x01], 8));
    assert!(!has_leading_zero_bits(&[0x01], 8));
    assert!(has_leading_zero_bits(&[0x00, 0x7F], 9));
    assert!(!has_leading_zero_bits(&[0x00, 0x80], 9));
    assert!(!has_leading_zero_bits(&[], 8));

    let nonce = solve_pow(b"challenge", 8, "SHA-256").expect("8 位难度必然可解");
    assert_eq!(nonce.len(), 8);
    assert!(has_leading_zero_bits(
        &PowAlgorithm::Sha256.digest(b"challenge", &nonce),
        8
    ));
    assert!(solve_pow(b"challenge", 0, "sha256").is_some());
    assert_eq!(PowAlgorithm::parse("SHA-256"), Some(PowAlgorithm::Sha256));
    assert_eq!(PowAlgorithm::parse("sha512"), Some(PowAlgorithm::Sha512));
    assert_eq!(PowAlgorithm::parse("MD5"), None);

    assert_eq!(base64_encode(b""), "");
    assert_eq!(base64_encode(b"f"), "Zg==");
    assert_eq!(base64_encode(b"fo"), "Zm8=");
    assert_eq!(base64_encode(b"foo"), "Zm9v");
    assert_eq!(base64_decode("Zm9v").as_deref(), Some(b"foo".as_slice()));
    assert_eq!(base64_decode("Zm8=").as_deref(), Some(b"fo".as_slice()));
    assert_eq!(base64_decode("Zg==").as_deref(), Some(b"f".as_slice()));
    assert_eq!(base64_decode("!!!"), None);
}

/// 响应头的 `X-BTN-ContentVersion` 取值（大小写不敏感 + 缺省 `unknown`）
#[test]
fn content_version_header_lookup() {
    let mut response = BtnHttpResponse::new(200, "body");
    response
        .headers
        .insert("x-btn-contentversion".to_string(), "cv-9".to_string());
    assert_eq!(response.header(HEADER_X_BTN_CONTENT_VERSION), Some("cv-9"));
    assert_eq!(
        BtnHttpResponse::new(200, "").header(HEADER_X_BTN_CONTENT_VERSION),
        None
    );
    assert!(BtnHttpResponse::new(204, "").is_no_content());
    assert!(BtnHttpResponse::new(200, "").is_successful());
    assert!(!BtnHttpResponse::new(500, "").is_successful());
}

/// 未配置 app-id/app-secret ⇒ 匿名账户（上游 `example-app-id` 判定）
#[test]
fn anonymous_account_detection() {
    let mut config = BtnNetworkConfig::default();
    assert!(config.is_anonymous());
    config.app_id = "example-app-id".to_string();
    config.app_secret = "x".to_string();
    assert!(config.is_anonymous());
    config.app_id = "a".to_string();
    config.app_secret = "example-app-secret".to_string();
    assert!(config.is_anonymous());
    config.app_secret = "b".to_string();
    assert!(!config.is_anonymous());
    assert!(!BtnNetworkConfig::default().is_active());
    assert!(active_config().is_active());
}

/// `ruleVersion` 初值（上游 `Objects.requireNonNullElse(ruleVersion, "initial")`）
#[test]
fn initial_rev_is_used_when_no_rules_loaded() {
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    assert_eq!(module.ip_list_version(true), INITIAL_REV);
    assert_eq!(module.ip_list_version(false), INITIAL_REV);
    assert_eq!(
        check(
            &module,
            &peer("1.2.3.4", 51413, "-hp001-abcdefghijkl", "Xunlei")
        )
        .reason_key,
        Some(TranslationComponent::new("Check passed"))
    );
}
/// 提交失败：游标不得推进，能力状态置 false，下一轮重试同一批数据。
#[test]
fn submit_bans_failure_keeps_cursor_and_reports_false() {
    const SUBMIT_BANS_URL: &str = "https://btn.test/submit-bans-fail";
    let source = Arc::new(StubSubmitSource {
        bans: StdMutex::new(vec![BtnBanHistoryRow {
            id: 7,
            ..Default::default()
        }]),
        ..Default::default()
    });
    let body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "submit": true,
        "ability": {
            "submit_bans": {
                "endpoint": SUBMIT_BANS_URL,
                "interval": 86400000,
                "random_initial_delay": 0,
                "pow_captcha": false
            }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body))
            .serve(SUBMIT_BANS_URL, Script::Body(500, "boom".to_string())),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(
        BtnNetworkConfig {
            submit: true,
            ..active_config()
        },
        http,
    )
    .with_submit_source(source);
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    let report = network.sync_due(&module, now_millis());
    assert_eq!(report.len(), 1);
    assert!(!report[0].updated);
    assert!(!report[0].last_status);
    assert!(
        network.metadata.get(METADATA_KEY_BANS_CURSOR).is_none(),
        "提交失败不得推进 bans 游标"
    );
}

/// 二元游标同理：失败保留原游标（不写入新值）。
#[test]
fn submit_swarm_failure_keeps_binary_cursor() {
    const SWARM_URL: &str = "https://btn.test/submit-swarm-fail";
    let source = Arc::new(StubSubmitSource {
        swarms: StdMutex::new(vec![BtnSwarmHistoryRow {
            id: 10,
            last_time_seen_ms: 7000,
            torrent_hash: "aaa".to_string(),
            ..Default::default()
        }]),
        ..Default::default()
    });
    let body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "submit": true,
        "ability": {
            "submit_swarm": {
                "endpoint": SWARM_URL,
                "interval": 86400000,
                "random_initial_delay": 0,
                "pow_captcha": false
            }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body))
            .serve(SWARM_URL, Script::Body(500, "boom".to_string())),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(
        BtnNetworkConfig {
            submit: true,
            ..active_config()
        },
        http,
    )
    .with_submit_source(source);
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    let report = network.sync_due(&module, now_millis());
    assert_eq!(report.len(), 1);
    assert!(!report[0].last_status);
    assert!(
        network.metadata.get(METADATA_KEY_SWARM_CURSOR).is_none(),
        "提交失败不得写入 swarm 二元游标"
    );
}

/// `ip_query` 非 2xx 必须返回 Err（Web 层据此返回 500），不得当作空结果。
#[test]
fn query_ip_http_failure_returns_error() {
    const QUERY_URL: &str = "https://btn.test/ip-query-fail";
    let body = serde_json::json!({
        "min_protocol_version": 20,
        "max_protocol_version": 20,
        "ability": {
            "ip_query": {
                "endpoint": QUERY_URL,
                "interval": 60000,
                "random_initial_delay": 0,
                "pow_captcha": false
            }
        }
    })
    .to_string();
    let http = Arc::new(
        MockHttpClient::default()
            .serve(CONFIG_URL, ok(body))
            .serve(
                &format!("{QUERY_URL}?ip=1.2.3.4"),
                Script::Body(500, "boom".to_string()),
            ),
    );
    let module = BtnNetworkOnline::new(crate::modules::btn::BTN_BAN_DURATION_MS);
    let network = make_network(active_config(), http);
    assert!(network.check_if_need_retry_config_at(&module, 1_000));
    let error = network
        .query_ip("1.2.3.4")
        .expect_err("HTTP 500 必须上抛");
    assert!(error.to_string().contains("HTTP 500"), "{error}");
}
