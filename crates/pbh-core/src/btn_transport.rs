//! BTN 网络传输层：忠实复刻上游 `btn/BtnNetwork` 与 `btn/ability/impl/*` 的联网部分。
//!
//! 上游对照关系（本文件 ≈ 上游下列类的并集）：
//! - [`BtnNetwork`] ≈ `btn/BtnNetwork`：配置端点握手（`configUrl` GET）、abilities 构造、
//!   `X-BTN-ContentVersion`、`metadataDao` 本地缓存、重试/调度（`nextConfigAttemptTime`）、
//!   `gatherAndSolveCaptchaBlocking(request, type)`；
//! - [`BtnAbility`] / [`BtnAbilityKind`] ≈ `btn/ability/AbstractBtnAbility` +
//!   `BtnAbilityRules` / `BtnAbilityIPAllowList` / `BtnAbilityIPDenyList`
//!   （`endpoint` / `interval` / `random_initial_delay` / `pow_captcha` / `lastStatus`）；
//! - [`BtnMetadataStore`] ≈ `MetadataService`（`metadataDao`），键名逐字一致：
//!   `btn.ability.rules.cache`、`btn.ability.ip_allowlist.cache.version|value`、
//!   `btn.ability.ip_denylist.cache.version|value`；
//! - [`PowCaptchaData`] + [`solve_pow`] ≈ `BtnNetwork.PowCaptchaData` + `util/pow/PoWClient`
//!   （Base64 challenge → 递增 8 字节 nonce → `<algorithm>` 摘要前 `difficultyBits` 位为零）；
//! - [`ReqwestBlockingHttpClient`] ≈ `HTTPUtil.newBuilder()` 的 OkHttp 客户端
//!   （`User-Agent` / `Content-Type` / `BTN-AppID` / `BTN-AppSecret` / `X-BTN-AppID` /
//!   `X-BTN-AppSecret` / `Authentication: Bearer <appId>@<appSecret>`、`callTimeout(1min)`）。
//!
//! 本层**不实现**任何判定逻辑：规则一律通过 [`BtnNetworkOnline`] 的既有注入入口
//! （`apply_ruleset_json` / `apply_ip_*_list_text` / `sync_from_transport`）落地，
//! 因此 [`BtnNetwork`] 实现了 [`BtnTransport`]，可直接交给
//! [`BtnNetworkOnline::sync_from_transport`]。
//!
//! 惰性保证（与上游 `btn.enabled=false` 等价）：
//! - [`BtnNetworkConfig::enabled`] 为 false 或 `config-url` 为空 ⇒ **一个请求都不发**；
//! - 所有失败路径都是「记日志并继续」，绝不 panic、绝不清空既有规则、绝不因此封禁 peer。
//!
//! 上报类能力（`submit_bans` / `submit_swarm` / `submit_histories` / 遗留 `submit_peers` /
//! `submit_bans`）通过 [`BtnSubmitSource`] 注入数据源，不直接依赖 DAO：数据源缺省时
//! 等价于「空库」（上游 `setLastStatus(true, BTN_LAST_REPORT_EMPTY)`：不发请求、状态置真）；
//! `BtnAbilityReconfigure` / `BtnAbilityHeartBeat` / `BtnAbilityIpQuery` 已实现
//! （心跳 `multi_if` 的本机网卡列表由 [`BtnNetwork::with_local_ips`] 注入，缺省为空）。
//! 另：`ModuleMatchCache` 未移植（本移植每轮重新判定，结果等价），
//! `BackgroundTaskManager` / `ScheduledExecutorService` 未移植（改为可注入时钟的显式驱动，
//! 本文件不创建任何线程）。

use crate::banlist::BanList;
use crate::modules::btn::{BtnIpAbility, BtnNetworkOnline, BtnRuleset, BtnTransport};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};

// ------------------------------------------------------------ 常量（对齐上游）

/// 上游 `Main.PBH_BTN_PROTOCOL_IMPL_VERSION = 20`；`useLegacyAbilities = min_protocol_version < 20`。
pub const BTN_PROTOCOL_IMPL_VERSION: i32 = 20;

/// 上游 `BtnNetwork.RETRY_PERIOD_SECONDS = 600`（配置握手失败后的重试间隔，秒）。
pub const RETRY_PERIOD_SECONDS: i64 = 600;

/// 上游 `BtnNetwork.setupHttpClient()` 的 `callTimeout(Duration.ofMinutes(1))`。
pub const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// 上游 `Main.getUserAgent()`；本移植无版本信息提供者，用与 `geoip_update` 一致的 UA。
pub const BTN_USER_AGENT: &str = "PeerBanHelper-RS";

/// 上游 `setupHttpClient()` 的拦截器：匿名账户（未配置 app-id/app-secret）时附加。
pub const HEADER_BTN_APP_ID: &str = "BTN-AppID";
pub const HEADER_BTN_APP_SECRET: &str = "BTN-AppSecret";
pub const HEADER_X_BTN_APP_ID: &str = "X-BTN-AppID";
pub const HEADER_X_BTN_APP_SECRET: &str = "X-BTN-AppSecret";
pub const HEADER_AUTHENTICATION: &str = "Authentication";
pub const HEADER_X_BTN_INSTALLATION_ID: &str = "X-BTN-InstallationID";
/// 上游 `BtnAbilityIP*List.updateRule`：`response.header("X-BTN-ContentVersion", "unknown")`
pub const HEADER_X_BTN_CONTENT_VERSION: &str = "X-BTN-ContentVersion";
/// 上游 `gatherAndSolveCaptchaBlocking` 写入的两个请求头
pub const HEADER_X_BTN_POW_ID: &str = "X-BTN-PowID";
pub const HEADER_X_BTN_POW_SOLUTION: &str = "X-BTN-PowSolution";

/// `BtnAbilityRules.loadCacheFile` / `updateRule` 的缓存键
pub const CACHE_KEY_RULES: &str = "btn.ability.rules.cache";
/// `BtnAbilityIPAllowList` 的缓存键
pub const CACHE_KEY_IP_ALLOWLIST_VERSION: &str = "btn.ability.ip_allowlist.cache.version";
pub const CACHE_KEY_IP_ALLOWLIST_VALUE: &str = "btn.ability.ip_allowlist.cache.value";
/// `BtnAbilityIPDenyList` 的缓存键
pub const CACHE_KEY_IP_DENYLIST_VERSION: &str = "btn.ability.ip_denylist.cache.version";
pub const CACHE_KEY_IP_DENYLIST_VALUE: &str = "btn.ability.ip_denylist.cache.value";

/// 上游 `Lang.MISSING_VERSION_PROTOCOL_FIELD`
pub const LANG_MISSING_VERSION_PROTOCOL_FIELD: &str = "MISSING_VERSION_PROTOCOL_FIELD";
/// 上游 `Lang.BTN_CONFIG_STATUS_UNSUCCESSFUL_INCOMPATIBLE_BTN_PROTOCOL_VERSION_CLIENT`
pub const LANG_BTN_PROTOCOL_VERSION_CLIENT: &str =
    "BTN_CONFIG_STATUS_UNSUCCESSFUL_INCOMPATIBLE_BTN_PROTOCOL_VERSION_CLIENT";
/// 上游 `Lang.BTN_CONFIG_STATUS_UNSUCCESSFUL_INCOMPATIBLE_BTN_PROTOCOL_VERSION_SERVER`
pub const LANG_BTN_PROTOCOL_VERSION_SERVER: &str =
    "BTN_CONFIG_STATUS_UNSUCCESSFUL_INCOMPATIBLE_BTN_PROTOCOL_VERSION_SERVER";
/// 上游 `Lang.BTN_CONFIGURE_SYNC_SERVER`（后台任务标题）
pub const LANG_BTN_CONFIGURE_SYNC_SERVER: &str = "BTN_CONFIGURE_SYNC_SERVER";
/// 上游 `Lang.BTN_CONFIG_FAILS`
pub const LANG_BTN_CONFIG_FAILS: &str = "BTN_CONFIG_FAILS";
/// 上游 `Lang.BTN_REQUEST_FAILS`
pub const LANG_BTN_REQUEST_FAILS: &str = "BTN_REQUEST_FAILS";
/// 上游 `Lang.BTN_POW_CAPTCHA_LOAD_FROM_REMOTE`
pub const LANG_BTN_POW_CAPTCHA_LOAD_FROM_REMOTE: &str = "BTN_POW_CAPTCHA_LOAD_FROM_REMOTE";
/// 上游 `Lang.BTN_POW_CAPTCHA_COMPUTING` / `BTN_POW_CAPTCHA_COMPUTE_COMPLETED`
pub const LANG_BTN_POW_CAPTCHA_COMPUTING: &str = "BTN_POW_CAPTCHA_COMPUTING";
pub const LANG_BTN_POW_CAPTCHA_COMPUTE_COMPLETED: &str = "BTN_POW_CAPTCHA_COMPUTE_COMPLETED";
/// 上游 `Lang.UNABLE_LOAD_BTN_ABILITY`
pub const LANG_UNABLE_LOAD_BTN_ABILITY: &str = "UNABLE_LOAD_BTN_ABILITY";
/// 上游 `Lang.BTN_RULES_LOADED_FROM_CACHE` / `BTN_RULES_LOADED_FROM_REMOTE`
pub const LANG_BTN_RULES_LOADED_FROM_CACHE: &str = "BTN_RULES_LOADED_FROM_CACHE";
pub const LANG_BTN_RULES_LOADED_FROM_REMOTE: &str = "BTN_RULES_LOADED_FROM_REMOTE";
pub const LANG_BTN_UPDATE_RULES_SUCCESSES: &str = "BTN_UPDATE_RULES_SUCCESSES";
/// 上游 `Lang.BTN_CONFIG_STATUS_SUCCESSFUL` / `..._READ_ONLY`
pub const LANG_BTN_CONFIG_STATUS_SUCCESSFUL: &str = "BTN_CONFIG_STATUS_SUCCESSFUL";
pub const LANG_BTN_CONFIG_STATUS_SUCCESSFUL_READ_ONLY: &str =
    "BTN_CONFIG_STATUS_SUCCESSFUL_READ_ONLY";
/// 上游 `Lang.BTN_CONFIG_STATUS_UNSUCCESSFUL_HTTP_REQUEST` / `BTN_CONFIG_STATUS_EXCEPTION`
pub const LANG_BTN_CONFIG_STATUS_UNSUCCESSFUL_HTTP_REQUEST: &str =
    "BTN_CONFIG_STATUS_UNSUCCESSFUL_HTTP_REQUEST";
pub const LANG_BTN_CONFIG_STATUS_EXCEPTION: &str = "BTN_CONFIG_STATUS_EXCEPTION";
/// 上游 `Lang.BTN_ABILITY_IP_ALLOWLIST_LOADED_FROM_REMOTE` / `..._DENYLIST_...`
pub const LANG_BTN_ABILITY_ALLOWLIST_LOADED_FROM_REMOTE: &str =
    "BTN_ABILITY_IP_ALLOWLIST_LOADED_FROM_REMOTE";
pub const LANG_BTN_ABILITY_DENYLIST_LOADED_FROM_REMOTE: &str =
    "BTN_ABILITY_IP_DENYLIST_LOADED_FROM_REMOTE";

/// `ruleVersion` 的初值（上游 `BtnAbilityIP*List` 字段初值与 `updateRule` 的 `requireNonNullElse`）
pub const INITIAL_REV: &str = "initial";

/// `BtnAbilitySubmitBans.getMemCursor` 的 metadata 键（`HistoryEntity.id` 单调游标）
pub const METADATA_KEY_BANS_CURSOR: &str = "BtnAbilitySubmitBans.cursor";
/// `BtnAbilitySubmitSwarm.getMemCursor` 的 metadata 键（`<lastTimeSeen>,<id>` 二元游标）
pub const METADATA_KEY_SWARM_CURSOR: &str = "BtnAbilitySubmitSwarm.cursor";
/// `BtnAbilitySubmitHistory.getLastSubmitAtTimestamp` 的 metadata 键（epoch 毫秒）
pub const METADATA_KEY_HISTORY_TIMESTAMP: &str = "btn.submithistory.timestamp";

// ------------------------------------------------------------- 配置（config.yml `btn:`）

/// 主配置 `config.yml` 的 `btn:` 段（≈ 上游 `Main.getMainConfig()` 在
/// `BtnNetwork.reloadConfig()` 里读取的键）。
///
/// [`Default`] 即上游 `config.yml` 的默认形态：**全部关闭**，因此缺省部署零网络请求。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnNetworkConfig {
    /// `btn.enabled`
    #[serde(default)]
    pub enabled: bool,
    /// `btn.config-url`（为空 ⇒ 无 BTN 服务器，整体惰性）
    #[serde(rename = "config-url", default)]
    pub config_url: String,
    /// `btn.submit`：上游决定 submit_* 能力是否构造（本移植无 submit 能力，仅保留语义）
    #[serde(default)]
    pub submit: bool,
    /// `btn.app-id`
    #[serde(rename = "app-id", default)]
    pub app_id: String,
    /// `btn.app-secret`
    #[serde(rename = "app-secret", default)]
    pub app_secret: String,
    /// `btn.allow-script-execute`（默认 false ⇒ 不编译/不执行 BTN 脚本规则）
    #[serde(rename = "allow-script-execute", default)]
    pub allow_script_execute: bool,
    /// `installation-id`：上游 `getInstallationId()`，匿名账户时随请求头发送
    #[serde(rename = "installation-id", default)]
    pub installation_id: String,
}

impl BtnNetworkConfig {
    /// 上游 `isEnableBTN()` + 「`configUrl` 为空则连请求都不构造」的惰性判定。
    pub fn is_active(&self) -> bool {
        self.enabled && !self.config_url.trim().is_empty()
    }

    /// 上游 `setupHttpClient()`：app-id / app-secret 缺失、空白或为占位值时视为匿名账户。
    fn is_anonymous(&self) -> bool {
        let id_ok = !self.app_id.trim().is_empty() && self.app_id != "example-app-id";
        let secret_ok =
            !self.app_secret.trim().is_empty() && self.app_secret != "example-app-secret";
        !(id_ok && secret_ok)
    }
}

// ------------------------------------------------------------- HTTP 抽象

/// HTTP 方法（上游 `Request.Builder` 默认为 GET；上报载荷一律 POST）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BtnHttpMethod {
    #[default]
    Get,
    Post,
}

/// 一次 BTN 请求（对齐 okhttp3 `Request`：URL + 方法 + 请求头 + gzip 请求体）。
///
/// `body` 由调用方按上游语义预先压缩为 gzip（`Content-Encoding: gzip` 由调用方自行设置，
/// 与上游 `createGzipRequestBody` 之后的 `Request` 一致）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BtnHttpRequest {
    pub url: String,
    pub method: BtnHttpMethod,
    pub headers: Vec<(String, String)>,
    /// POST 请求体（已 gzip 压缩的字节）；GET 时为 `None`。
    pub body: Option<Vec<u8>>,
}

impl BtnHttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: BtnHttpMethod::Get,
            headers: Vec::new(),
            body: None,
        }
    }

    /// 对齐上游 `createGzipRequestBody` + `POST`：body 应已通过 [`gzip_bytes`] 压缩。
    pub fn post(url: impl Into<String>, body: Vec<u8>) -> Self {
        Self {
            url: url.into(),
            method: BtnHttpMethod::Post,
            headers: Vec::new(),
            body: Some(body),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

/// BTN 响应（对齐 okhttp3 `Response`：`code()` / `body().string()` / `header(name, default)`）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BtnHttpResponse {
    pub status: u16,
    pub body: String,
    pub headers: BTreeMap<String, String>,
}

impl BtnHttpResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            headers: BTreeMap::new(),
        }
    }

    /// 上游 `response.isSuccessful()`（2xx）
    pub fn is_successful(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 上游 `response.code() == 204`（规则未变化）
    pub fn is_no_content(&self) -> bool {
        self.status == 204
    }

    /// 上游 `response.header(name, default)`：大小写不敏感（HTTP 头语义）
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// 阻塞式 HTTP 客户端（上游是同步 OkHttp 调用）。
pub trait BtnHttpClient: Send + Sync + std::fmt::Debug {
    fn execute(&self, request: BtnHttpRequest) -> anyhow::Result<BtnHttpResponse>;
}

/// 生产实现：`reqwest::blocking`（超时对齐 `callTimeout(Duration.ofMinutes(1))`）。
///
/// **不要在 async 上下文里直接调用**（`reqwest::blocking` 内部自建运行时）。
#[derive(Debug)]
pub struct ReqwestBlockingHttpClient {
    client: reqwest::blocking::Client,
}

impl ReqwestBlockingHttpClient {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_user_agent(BTN_USER_AGENT)
    }

    pub fn with_user_agent(user_agent: impl Into<String>) -> anyhow::Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(CALL_TIMEOUT)
            .user_agent(user_agent.into())
            .build()?;
        Ok(Self { client })
    }
}

impl BtnHttpClient for ReqwestBlockingHttpClient {
    fn execute(&self, request: BtnHttpRequest) -> anyhow::Result<BtnHttpResponse> {
        let mut builder = match request.method {
            BtnHttpMethod::Get => self.client.get(&request.url),
            BtnHttpMethod::Post => {
                let mut builder = self.client.post(&request.url);
                if let Some(body) = &request.body {
                    builder = builder.body(body.clone());
                }
                builder
            }
        };
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let response = builder.send()?;
        let status = response.status().as_u16();
        let mut headers = BTreeMap::new();
        for (name, value) in response.headers() {
            headers.insert(
                name.as_str().to_string(),
                value.to_str().unwrap_or_default().to_string(),
            );
        }
        let body = response.text()?;
        Ok(BtnHttpResponse {
            status,
            body,
            headers,
        })
    }
}

/// 闭包适配器（测试与自建 HTTP 栈用）。
pub struct ClosureHttpClient<F> {
    f: F,
}

impl<F> std::fmt::Debug for ClosureHttpClient<F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClosureHttpClient")
            .finish_non_exhaustive()
    }
}

impl<F> ClosureHttpClient<F> {
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

impl<F> BtnHttpClient for ClosureHttpClient<F>
where
    F: Fn(&BtnHttpRequest) -> anyhow::Result<BtnHttpResponse> + Send + Sync,
{
    fn execute(&self, request: BtnHttpRequest) -> anyhow::Result<BtnHttpResponse> {
        (self.f)(&request)
    }
}

// --------------------------------------------------------- 本地缓存（metadataDao）

/// `MetadataService`（`metadataDao`）的极小抽象：BTN 用它持久化规则缓存。
pub trait BtnMetadataStore: Send + Sync + std::fmt::Debug {
    fn get(&self, key: &str) -> Option<String>;
    fn set(&self, key: &str, value: &str);
}

/// 进程内实现（默认）：与上游一样「重启后仍在」，但只在本进程生命周期内。
#[derive(Debug, Default)]
pub struct InMemoryMetadataStore {
    inner: StdMutex<BTreeMap<String, String>>,
}

impl InMemoryMetadataStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl BtnMetadataStore for InMemoryMetadataStore {
    fn get(&self, key: &str) -> Option<String> {
        self.inner.lock().ok().and_then(|map| map.get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) {
        if let Ok(mut map) = self.inner.lock() {
            map.insert(key.to_string(), value.to_string());
        }
    }
}

// ------------------------------------------------------------ 配置端点响应

/// 上游 `btn/BtnNetwork.PowCaptchaData`（Gson 按字段名序列化）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PowCaptchaData {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "challengeBase64", default)]
    pub challenge_base64: String,
    #[serde(rename = "difficultyBits", default)]
    pub difficulty_bits: i32,
    #[serde(default)]
    pub algorithm: String,
    #[serde(rename = "expireAt", default)]
    pub expire_at: i64,
}

/// 配置端点返回的 `proof_of_work_captcha` 子对象。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PowCaptchaEndpoint {
    #[serde(default)]
    pub endpoint: String,
}

/// 配置端点返回的单个 ability 定义（`BtnAbilityRules` / `BtnAbilityIP*List` 构造函数读取的键）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnAbilitySpec {
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub interval: Option<i64>,
    #[serde(default)]
    pub random_initial_delay: Option<i64>,
    #[serde(default)]
    pub pow_captcha: Option<bool>,
    /// `BtnAbilityHeartBeat.multiIf` 等扩展开关（多余字段一律 `false`）
    #[serde(default)]
    pub multi_if: Option<bool>,
    /// `BtnAbilityReconfigure` 内置「当前版本」（须匹配端点响应的 `version`，
    /// 上游 `ability.get("version").getAsString()` ⇒ 字符串版本号）
    #[serde(default)]
    pub version: Option<String>,
}

/// 配置端点响应（`BtnNetwork.configBtnNetwork` 解析的 `JsonObject`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnConfigResponse {
    #[serde(default)]
    pub min_protocol_version: Option<i32>,
    #[serde(default)]
    pub max_protocol_version: Option<i32>,
    #[serde(default)]
    pub proof_of_work_captcha: Option<PowCaptchaEndpoint>,
    #[serde(default)]
    pub ability: BTreeMap<String, BtnAbilitySpec>,
}

// -------------------------------------------------------------- 上报协议 DTO

// 下列结构与上游 `btn/ping/**` 的 @SerializedName **逐字一致**（Rust 字段名即 wire 名）。
// Gson 的 `JsonUtil.standard()` 开启 `serializeNulls`：`Option::None` 字段必须输出 JSON
// `null`，因此**不得**加 `skip_serializing_if`。

/// `BtnBanPing`：`{"bans":[...]}`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnBanPing {
    pub bans: Vec<BtnBan>,
}

/// `BtnBan`：`submit_bans` 载荷条目（`HistoryEntity` + `TorrentEntityDTO` 快照）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnBan {
    /// epoch 毫秒（上游 `Timestamp` 序列化为数字）
    pub ban_at: i64,
    pub peer_ip: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_progress: f64,
    pub peer_flag: Option<String>,
    /// `InfoHashUtil.getHashedIdentifier(infoHash)` 的哈希值
    pub torrent_identifier: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub from_peer_traffic: i64,
    pub to_peer_traffic: i64,
    pub downloader_progress: f64,
    pub module: String,
    pub rule: String,
    pub description: String,
    pub structured_data: Option<String>,
}

/// `BtnSwarmPeerPing`：`{"swarms":[...]}`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnSwarmPeerPing {
    pub swarms: Vec<BtnSwarm>,
}

/// `BtnSwarm`：`submit_swarm` 载荷条目（上游 `TrackedSwarmEntity` 快照）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnSwarm {
    pub torrent_identifier: String,
    /// 可空（上游 `Boolean` 包装类 ⇒ `serializeNulls` 可输出 `null`）
    pub torrent_is_private: Option<bool>,
    pub torrent_size: i64,
    pub downloader: String,
    pub downloader_progress: f64,
    pub peer_ip: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_progress: f64,
    pub to_peer_traffic: i64,
    pub to_peer_traffic_offset: i64,
    pub from_peer_traffic: i64,
    pub from_peer_traffic_offset: i64,
    /// epoch 毫秒（上游 `OffsetDateTime` 序列化为数字）
    pub first_time_seen: i64,
    /// epoch 毫秒
    pub last_time_seen: i64,
    pub peer_last_flags: Option<String>,
    pub upload_speed: i64,
    pub download_speed: i64,
    pub download_speed_max: i64,
    pub upload_speed_max: i64,
}

/// `LegacyBtnPeerPing`：`{"populate_time":..., "peers":[...]}`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnPeerPing {
    pub populate_time: i64,
    pub peers: Vec<LegacyBtnPeer>,
}

/// `LegacyBtnPeer`：遗留协议 live peer 快照（`submit_peers`）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnPeer {
    pub ip_address: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    pub torrent_identifier: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub downloaded: i64,
    pub rt_download_speed: i64,
    pub uploaded: i64,
    pub rt_upload_speed: i64,
    pub peer_progress: f64,
    pub downloader_progress: f64,
    pub peer_flag: Option<String>,
}

/// `LegacyBtnPeerHistoryPing`：`{"populate_time":..., "peers":[...]}`（`submit_histories`）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnPeerHistoryPing {
    pub populate_time: i64,
    pub peers: Vec<LegacyBtnPeerHistory>,
}

/// `LegacyBtnPeerHistory`：遗留协议 peer 历史记录
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnPeerHistory {
    pub ip_address: String,
    pub port: u16,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    pub torrent_identifier: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub downloaded: i64,
    pub downloaded_offset: i64,
    pub uploaded: i64,
    pub uploaded_offset: i64,
    /// epoch 毫秒（上游 `Timestamp` 序列化为数字）
    pub first_time_seen: i64,
    /// epoch 毫秒
    pub last_time_seen: i64,
    pub peer_flag: Option<String>,
}

/// `LegacyBtnBanPing`：`{"populate_time":..., "bans":[...]}`（遗留 `submit_bans`）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnBanPing {
    pub populate_time: i64,
    pub bans: Vec<LegacyBtnBan>,
}

/// `LegacyBtnBan`：遗留协议单条封禁上报
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnBan {
    pub btn_ban: bool,
    pub ban_unique_id: String,
    pub module: String,
    pub rule: String,
    pub peer: LegacyBtnPeer,
    pub structured_data: Option<serde_json::Value>,
}

// -------------------------------------------------------------- 上报数据行

// 数据行是「离线重组所需的最小信息」：trait 实现方（`pbh-db` / `pbh`）负责把
// `HistoryEntity` / `TrackedSwarmEntity` / `PeerRecordEntity` 与对应 torrent 快照
// JOIN 成这些行；本文件只负责按上游 `*.from(...)` 的顺序转换 + 哈希 infohash。

/// `BtnAbilitySubmitBans` 的一行（`HistoryEntity` + `TorrentEntityDTO`）
#[derive(Debug, Clone, Default)]
pub struct BtnBanHistoryRow {
    /// `HistoryEntity.id`：游标
    pub id: i64,
    pub ban_at_ms: i64,
    pub peer_ip: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_progress: f64,
    pub peer_flag: Option<String>,
    /// 原始 infohash（转换时调 [`get_hashed_identifier`]）
    pub torrent_hash: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub from_peer_traffic: i64,
    pub to_peer_traffic: i64,
    pub downloader_progress: f64,
    pub module: String,
    pub rule: String,
    pub description: String,
    pub structured_data: Option<String>,
}

impl BtnBanHistoryRow {
    fn into_ping(self) -> BtnBan {
        BtnBan {
            ban_at: self.ban_at_ms,
            peer_ip: self.peer_ip,
            peer_port: self.peer_port,
            peer_id: self.peer_id,
            peer_client_name: self.peer_client_name,
            peer_progress: self.peer_progress,
            peer_flag: self.peer_flag,
            torrent_identifier: get_hashed_identifier(&self.torrent_hash),
            torrent_is_private: self.torrent_is_private,
            torrent_size: self.torrent_size,
            from_peer_traffic: self.from_peer_traffic,
            to_peer_traffic: self.to_peer_traffic,
            downloader_progress: self.downloader_progress,
            module: self.module,
            rule: self.rule,
            description: self.description,
            structured_data: self.structured_data,
        }
    }
}

/// `BtnAbilitySubmitSwarm` 的一行（`TrackedSwarmEntity`）
#[derive(Debug, Clone, Default)]
pub struct BtnSwarmHistoryRow {
    /// `TrackedSwarmEntity.id`：二元游标的第二分量
    pub id: i64,
    /// `TrackedSwarmEntity.lastTimeSeen`（epoch 毫秒）：二元游标的第一分量
    pub last_time_seen_ms: i64,
    /// 原始 infohash
    pub torrent_hash: String,
    pub torrent_is_private: Option<bool>,
    pub torrent_size: i64,
    pub downloader: String,
    pub downloader_progress: f64,
    pub peer_ip: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_progress: f64,
    pub to_peer_traffic: i64,
    pub to_peer_traffic_offset: i64,
    pub from_peer_traffic: i64,
    pub from_peer_traffic_offset: i64,
    pub first_time_seen_ms: i64,
    pub peer_last_flags: Option<String>,
    pub upload_speed: i64,
    pub download_speed: i64,
    pub download_speed_max: i64,
    pub upload_speed_max: i64,
}

impl BtnSwarmHistoryRow {
    fn into_ping(self) -> BtnSwarm {
        BtnSwarm {
            torrent_identifier: get_hashed_identifier(&self.torrent_hash),
            torrent_is_private: self.torrent_is_private,
            torrent_size: self.torrent_size,
            downloader: self.downloader,
            downloader_progress: self.downloader_progress,
            peer_ip: self.peer_ip,
            peer_port: self.peer_port,
            peer_id: self.peer_id,
            peer_client_name: self.peer_client_name,
            peer_progress: self.peer_progress,
            to_peer_traffic: self.to_peer_traffic,
            to_peer_traffic_offset: self.to_peer_traffic_offset,
            from_peer_traffic: self.from_peer_traffic,
            from_peer_traffic_offset: self.from_peer_traffic_offset,
            first_time_seen: self.first_time_seen_ms,
            last_time_seen: self.last_time_seen_ms,
            peer_last_flags: self.peer_last_flags,
            upload_speed: self.upload_speed,
            download_speed: self.download_speed,
            download_speed_max: self.download_speed_max,
            upload_speed_max: self.upload_speed_max,
        }
    }
}

/// `BtnAbilitySubmitHistory` 的一行（`PeerRecordEntity` + `TorrentEntityDTO`）
#[derive(Debug, Clone, Default)]
pub struct BtnPeerHistoryRow {
    pub ip_address: String,
    pub port: u16,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    /// 原始 infohash
    pub torrent_hash: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub downloaded: i64,
    pub downloaded_offset: i64,
    pub uploaded: i64,
    pub uploaded_offset: i64,
    pub first_time_seen_ms: i64,
    pub last_time_seen_ms: i64,
    pub peer_flag: Option<String>,
}

impl BtnPeerHistoryRow {
    fn into_ping(self) -> LegacyBtnPeerHistory {
        LegacyBtnPeerHistory {
            ip_address: self.ip_address,
            port: self.port,
            peer_id: self.peer_id,
            client_name: self.client_name,
            torrent_identifier: get_hashed_identifier(&self.torrent_hash),
            torrent_is_private: self.torrent_is_private,
            torrent_size: self.torrent_size,
            downloaded: self.downloaded,
            downloaded_offset: self.downloaded_offset,
            uploaded: self.uploaded,
            uploaded_offset: self.uploaded_offset,
            first_time_seen: self.first_time_seen_ms,
            last_time_seen: self.last_time_seen_ms,
            peer_flag: self.peer_flag,
        }
    }
}

/// `LegacyBtnAbilitySubmitPeers` 的一行（live peer 快照）
#[derive(Debug, Clone, Default)]
pub struct BtnLegacyPeerRow {
    pub ip_address: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    /// 原始 infohash
    pub torrent_hash: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub downloaded: i64,
    pub rt_download_speed: i64,
    pub uploaded: i64,
    pub rt_upload_speed: i64,
    pub peer_progress: f64,
    pub downloader_progress: f64,
    pub peer_flag: Option<String>,
}

impl BtnLegacyPeerRow {
    fn into_peer(self) -> LegacyBtnPeer {
        LegacyBtnPeer {
            ip_address: self.ip_address,
            peer_port: self.peer_port,
            peer_id: self.peer_id,
            client_name: self.client_name,
            torrent_identifier: get_hashed_identifier(&self.torrent_hash),
            torrent_is_private: self.torrent_is_private,
            torrent_size: self.torrent_size,
            downloaded: self.downloaded,
            rt_download_speed: self.rt_download_speed,
            uploaded: self.uploaded,
            rt_upload_speed: self.rt_upload_speed,
            peer_progress: self.peer_progress,
            downloader_progress: self.downloader_progress,
            peer_flag: self.peer_flag,
        }
    }
}

/// `LegacyBtnAbilitySubmitBans.generateBans` 的一行（`BanMetadata` 快照，已含 peer 信息）
#[derive(Debug, Clone, Default)]
pub struct BtnLegacyBanRow {
    pub ban_at_ms: i64,
    pub ban_unique_id: String,
    pub module: String,
    pub rule: String,
    pub btn_ban: bool,
    pub peer: BtnLegacyPeerRow,
    pub structured_data: Option<serde_json::Value>,
}

impl BtnLegacyBanRow {
    fn into_ping(self) -> LegacyBtnBan {
        LegacyBtnBan {
            btn_ban: self.btn_ban,
            ban_unique_id: self.ban_unique_id,
            module: self.module,
            rule: self.rule,
            peer: self.peer.into_peer(),
            structured_data: self.structured_data,
        }
    }
}

/// 上报类能力的数据源抽象（对应上游 `HistoryService` + `TorrentService` +
/// `TrackedSwarmService` + `PeerRecordService` + `DownloaderServer` 的只读部分）。
///
/// 所有方法都有「空」默认实现：`BtnNetwork` 未注入数据源（或实现方返回空表）时，
/// 提交行为等价于上游「没有任何待提交数据」——**不发请求**、状态置真
/// （`setLastStatus(true, BTN_LAST_REPORT_EMPTY)`）。
///
/// 实现方约定：分页方法须按游标语义返回**有序**的行（上游依赖 stable ordering，
/// 否则游标推进会漏数据）。
pub trait BtnSubmitSource: Send + Sync + std::fmt::Debug {
    /// `historyDao.page(id > cursor_id)` 的封禁历史（`BtnAbilitySubmitBans`，每页 100）。
    fn batch_ban_history(&self, cursor_id: i64, limit: usize) -> Vec<BtnBanHistoryRow> {
        let _ = (cursor_id, limit);
        Vec::new()
    }

    /// `swarmDao.page(lastTimeSeen >= x, id > y, orderByAsc)` 的 swarm（每页 1000）。
    fn batch_swarm_history(
        &self,
        last_time_seen_ms: i64,
        id_after: i64,
        limit: usize,
    ) -> Vec<BtnSwarmHistoryRow> {
        let _ = (last_time_seen_ms, id_after, limit);
        Vec::new()
    }

    /// `peerRecordsDao.getPendingSubmitPeerRecords(since)` 的 peer 历史（每页 5000）。
    fn batch_peer_history(&self, last_time_seen_ms: i64, limit: usize) -> Vec<BtnPeerHistoryRow> {
        let _ = (last_time_seen_ms, limit);
        Vec::new()
    }

    /// `server.getLivePeersSnapshot()`：遗留协议 live peer 全量快照。
    fn legacy_peer_snapshot(&self) -> Vec<BtnLegacyPeerRow> {
        Vec::new()
    }

    /// `server.getBanList()`：遗留协议封禁全量快照（`ban_at_ms` 由调用方过滤）。
    fn legacy_ban_snapshot(&self) -> Vec<BtnLegacyBanRow> {
        Vec::new()
    }
}

// ---------------------------------------------------- 心跳 / IpQuery 响应 DTO

/// 心跳响应（上游 `ServerResponse`，本移植只用到 `external_ip`；Gson 忽略未命名字段，
/// serde 同样忽略未知键）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BtnHeartbeatResponse {
    #[serde(default)]
    pub external_ip: Option<String>,
}

/// `BtnAbilityIpQuery.query` 的响应（上游嵌套类 `IpQueryResult` 的逐字段移植）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryResult {
    /// 风险等级：red / orange / green / gray
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub bans: Option<IpQueryResultBans>,
    #[serde(default)]
    pub swarms: Option<IpQueryResultSwarms>,
    #[serde(default)]
    pub traffic: Option<IpQueryTraffic>,
    #[serde(default)]
    pub torrents: Option<IpQueryTorrents>,
}

/// `IpQueryResult.IpQueryResultBans`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryResultBans {
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub records: Vec<IpQueryBanRecord>,
}

/// `IpQueryResult.BanHistoryDto`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryBanRecord {
    #[serde(default)]
    pub populate_time: Option<i64>,
    #[serde(default)]
    pub torrent: Option<String>,
    #[serde(default)]
    pub peer_ip: Option<String>,
    #[serde(default)]
    pub peer_port: Option<u16>,
    #[serde(default)]
    pub peer_id: Option<String>,
    #[serde(default)]
    pub peer_client_name: Option<String>,
    #[serde(default)]
    pub peer_progress: Option<f64>,
    #[serde(default)]
    pub peer_flags: Option<String>,
    #[serde(default)]
    pub reporter_progress: Option<f64>,
    #[serde(default)]
    pub to_peer_traffic: Option<i64>,
    #[serde(default)]
    pub from_peer_traffic: Option<i64>,
    #[serde(default)]
    pub module_name: Option<String>,
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub structured_data: Option<serde_json::Value>,
}

/// `IpQueryResult.IpQueryResultSwarms`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryResultSwarms {
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub records: Vec<IpQuerySwarmRecord>,
    #[serde(default)]
    pub concurrent_download_torrents_count: u64,
    #[serde(default)]
    pub concurrent_seeding_torrents_count: u64,
}

/// `IpQueryResult.SwarmTrackerDto`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQuerySwarmRecord {
    #[serde(default)]
    pub torrent: Option<String>,
    #[serde(default)]
    pub peer_ip: Option<String>,
    #[serde(default)]
    pub peer_port: Option<u16>,
    #[serde(default)]
    pub peer_id: Option<String>,
    #[serde(default)]
    pub peer_client_name: Option<String>,
    #[serde(default)]
    pub peer_progress: Option<f64>,
    #[serde(default)]
    pub from_peer_traffic: Option<i64>,
    #[serde(default)]
    pub to_peer_traffic: Option<i64>,
    #[serde(default)]
    pub from_peer_traffic_offset: Option<i64>,
    #[serde(default)]
    pub to_peer_traffic_offset: Option<i64>,
    #[serde(default)]
    pub flags: Option<String>,
    #[serde(default)]
    pub first_time_seen: Option<i64>,
    #[serde(default)]
    pub last_time_seen: Option<i64>,
    #[serde(default)]
    pub user_progress: f64,
}

/// `IpQueryResult.IpQueryTraffic`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryTraffic {
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub to_peer_traffic: i64,
    #[serde(default)]
    pub from_peer_traffic: i64,
    #[serde(default)]
    pub share_ratio: f64,
}

/// `IpQueryResult.IpQueryTorrents`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryTorrents {
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub count: u64,
}

// --------------------------------------------------------------- abilities

/// 上游 `btn/ability/**` 的能力种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtnAbilityKind {
    /// `BtnAbilityRules`：现代键 `rule_peer_identity`，遗留键 `rules`
    Rules,
    /// `BtnAbilityIPAllowList`：现代键 `ip_allowlist`
    IpAllowList,
    /// `BtnAbilityIPDenyList`：现代键 `ip_denylist`
    IpDenyList,
    /// `BtnAbilitySubmitBans`：现代键 `submit_bans`
    SubmitBans,
    /// `BtnAbilitySubmitSwarm`：现代键 `submit_swarm`
    SubmitSwarm,
    /// `BtnAbilitySubmitHistory`：键 `submit_histories`（两种协议分支共有）
    SubmitHistories,
    /// `BtnAbilityHeartBeat`：键 `heartbeat`
    Heartbeat,
    /// `BtnAbilityReconfigure`：键 `reconfigure`
    Reconfigure,
    /// `BtnAbilityIpQuery`：键 `ip_query`（只注册，不主动调度；由被查询方触发）
    IpQuery,
    /// `LegacyBtnAbilitySubmitPeers`：遗留键 `submit_peers`（min < 20 时）
    LegacySubmitPeers,
    /// `LegacyBtnAbilitySubmitBans`：遗留键 `submit_bans`（min < 20 时）
    LegacySubmitBans,
    /// 已在配置端点出现、但本移植未实现的能力（见模块文档）
    Other,
}

impl BtnAbilityKind {
    /// 上游 `BtnNetwork.configBtnNetwork` 里 `ability.has(<key>)` 的键 → 种类。
    ///
    /// 注意遗留协议的 `submit_bans` 对应 `LegacyBtnAbilitySubmitBans`，与
    /// 现代协议的 `BtnAbilitySubmitBans` 是**不同**实现；注册时由
    /// [`BtnNetwork::apply_config_response`] 按 `min_protocol_version` 分支决定，这里只做无分支映射。
    pub fn from_json_key(key: &str) -> Self {
        match key {
            "rules" | "rule_peer_identity" => Self::Rules,
            "ip_allowlist" => Self::IpAllowList,
            "ip_denylist" => Self::IpDenyList,
            "submit_bans" => Self::SubmitBans,
            "submit_swarm" => Self::SubmitSwarm,
            "submit_histories" => Self::SubmitHistories,
            "heartbeat" => Self::Heartbeat,
            "reconfigure" => Self::Reconfigure,
            "ip_query" => Self::IpQuery,
            "submit_peers" => Self::LegacySubmitPeers,
            _ => Self::Other,
        }
    }

    /// 现代协议下的 JSON 键
    pub fn json_key(self) -> &'static str {
        match self {
            Self::Rules => "rule_peer_identity",
            Self::IpAllowList => "ip_allowlist",
            Self::IpDenyList => "ip_denylist",
            Self::SubmitBans => "submit_bans",
            Self::SubmitSwarm => "submit_swarm",
            Self::SubmitHistories => "submit_histories",
            Self::Heartbeat => "heartbeat",
            Self::Reconfigure => "reconfigure",
            Self::IpQuery => "ip_query",
            Self::LegacySubmitPeers => "submit_peers",
            Self::LegacySubmitBans => "submit_bans",
            Self::Other => "unknown",
        }
    }

    /// 上游 `gatherAndSolveCaptchaBlocking(request, type)` 的 `type` 参数。
    ///
    /// 与 JSON 键的唯一差异：`submit_histories` 能力的 PoW type 是**单数**
    /// `submit_history`（源码实参逐字确认，不是 `json_key()` 的复数形式）。
    pub fn pow_type(self) -> &'static str {
        match self {
            Self::SubmitHistories => "submit_history",
            _ => self.json_key(),
        }
    }

    /// 本移植是否实现了该能力
    pub fn is_implemented(self) -> bool {
        !matches!(self, Self::Other)
    }

    /// 上游是否会给该能力 `scheduleWithFixedDelay(...)`（`IpQuery` 不注册调度器，
    /// 只由被查询方调用；其余能力全部周期执行）
    pub fn is_scheduled(self) -> bool {
        !matches!(self, Self::IpQuery | Self::Other)
    }

    /// 上游 `AbstractBtnAbility` 子类名（日志/状态展示用）
    pub fn upstream_name(self) -> &'static str {
        match self {
            Self::Rules => "BtnAbilityRules",
            Self::IpAllowList => "BtnAbilityIPAllowList",
            Self::IpDenyList => "BtnAbilityIPDenyList",
            Self::SubmitBans => "BtnAbilitySubmitBans",
            Self::SubmitSwarm => "BtnAbilitySubmitSwarm",
            Self::SubmitHistories => "BtnAbilitySubmitHistory",
            Self::Heartbeat => "BtnAbilityHeartBeat",
            Self::Reconfigure => "BtnAbilityReconfigure",
            Self::IpQuery => "BtnAbilityIpQuery",
            Self::LegacySubmitPeers => "LegacyBtnAbilitySubmitPeers",
            Self::LegacySubmitBans => "LegacyBtnAbilitySubmitBans",
            Self::Other => "BtnAbility(Unsupported)",
        }
    }
}

/// 上游 `AbstractBtnAbility` + 具体 ability 的运行期状态。
#[derive(Clone, Debug)]
pub struct BtnAbility {
    pub kind: BtnAbilityKind,
    /// 配置端点里该 ability 的 JSON 键（遗留协议可能是 `rules` / `submit_peers`）
    pub key: String,
    pub endpoint: String,
    /// `interval` 毫秒（`scheduleWithFixedDelay` 的 period）
    pub interval_ms: i64,
    /// `random_initial_delay` 毫秒（上游 `ThreadLocalRandom.nextLong(randomInitialDelay)`）
    pub random_initial_delay_ms: i64,
    pub pow_captcha: bool,
    /// `BtnAbilityHeartBeat.multiIf`：对每个本机网卡地址各发一次心跳
    pub multi_if: bool,
    /// `BtnAbilityReconfigure` 本地保存的 `version`（端点版本变化时触发重新握手）
    pub reconfigure_version: Option<String>,
    /// `AbstractBtnAbility.lastStatus`
    pub last_status: bool,
    /// `AbstractBtnAbility.lastStatusAt`
    pub last_status_at_ms: i64,
    /// 上游调度器的下一次触发时刻（本移植无后台线程，由调用方驱动 [`BtnNetwork::sync_due`]）
    pub next_due_ms: i64,
}

impl BtnAbility {
    fn from_spec(key: &str, spec: &BtnAbilitySpec) -> Self {
        Self {
            kind: BtnAbilityKind::from_json_key(key),
            key: key.to_string(),
            endpoint: spec.endpoint.clone().unwrap_or_default(),
            interval_ms: spec.interval.unwrap_or(0),
            random_initial_delay_ms: spec.random_initial_delay.unwrap_or(0),
            pow_captcha: spec.pow_captcha.unwrap_or(false),
            multi_if: spec.multi_if.unwrap_or(false),
            reconfigure_version: spec.version.clone(),
            // 上游构造函数 `set_status(true, LAST_STATUS_STANDBY)`
            last_status: true,
            last_status_at_ms: 0,
            next_due_ms: i64::MAX,
        }
    }
}

// --------------------------------------------------------------- PoW captcha

/// 上游 `PoWClient.solve` 的迭代上限。
///
/// 上游在 `resultFuture.get()` 上无限等待（多线程暴力搜索）；本移植为单线程且必须保证
/// 不会挂住调用方，故设置上界。达到上界时的失败路径与上游「求解异常」一致：
/// 记日志、不加 PoW 请求头、继续后续流程。
pub const POW_MAX_ITERATIONS: u64 = 20_000_000;

/// 上游 `PoWClient.hasLeadingZeroBits`：摘要的前 `bits` 位必须全为 0。
pub fn has_leading_zero_bits(hash: &[u8], bits: i32) -> bool {
    if bits <= 0 {
        return true;
    }
    let bits = bits as usize;
    let full_bytes = bits / 8;
    let remaining_bits = bits % 8;
    if hash.len() < full_bytes {
        return false;
    }
    if hash[..full_bytes].iter().any(|byte| *byte != 0) {
        return false;
    }
    if remaining_bits > 0 {
        let mask = 0xFFu8 << (8 - remaining_bits);
        hash.get(full_bytes)
            .map(|byte| byte & mask == 0)
            .unwrap_or(false)
    } else {
        true
    }
}

/// `PoWCaptchaData.algorithm` 支持的摘要算法（上游 `MessageDigest.getInstance(algorithm)`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowAlgorithm {
    Sha256,
    Sha512,
}

impl PowAlgorithm {
    /// 上游把服务端的算法名直接交给 `MessageDigest`；这里按 JCA 标准名（大小写/连字符不敏感）识别。
    pub fn parse(algorithm: &str) -> Option<Self> {
        let normalized: String = algorithm.chars().filter(|c| !c.is_whitespace()).collect();
        match normalized.to_ascii_uppercase().replace('-', "").as_str() {
            "SHA256" | "SHA_256" => Some(Self::Sha256),
            "SHA512" | "SHA_512" => Some(Self::Sha512),
            _ => None,
        }
    }

    fn digest(self, challenge: &[u8], nonce: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        match self {
            Self::Sha256 => {
                let mut hasher = sha2::Sha256::new();
                hasher.update(challenge);
                hasher.update(nonce);
                hasher.finalize().to_vec()
            }
            Self::Sha512 => {
                let mut hasher = sha2::Sha512::new();
                hasher.update(challenge);
                hasher.update(nonce);
                hasher.finalize().to_vec()
            }
        }
    }
}

/// 上游 `PoWClient.solve(challenge, difficultyBits, algorithm)`：
/// 从随机 8 字节 nonce 开始递增，直到 `<algorithm>(challenge || nonce)` 的前
/// `difficulty_bits` 位为零；返回该 nonce 的 8 字节大端表示（`ByteBuffer.putLong`）。
///
/// 算法名无法识别或达到 [`POW_MAX_ITERATIONS`] 时返回 `None`（调用方按上游
/// `catch (Throwable)` 的分支处理：记日志、跳过 PoW 请求头）。
pub fn solve_pow(challenge: &[u8], difficulty_bits: i32, algorithm: &str) -> Option<Vec<u8>> {
    let algorithm = PowAlgorithm::parse(algorithm)?;
    let mut nonce = random_nonce();
    for _ in 0..POW_MAX_ITERATIONS {
        let nonce_bytes = nonce.to_be_bytes();
        if has_leading_zero_bits(&algorithm.digest(challenge, &nonce_bytes), difficulty_bits) {
            return Some(nonce_bytes.to_vec());
        }
        nonce = nonce.wrapping_add(1);
    }
    None
}

/// 上游 `new SecureRandom().nextLong()`：非密码学强度的 64 位起点。
fn random_nonce() -> u64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut state = STATE.load(Ordering::Relaxed);
    if state == 0 {
        state = (now_millis() as u64) ^ 0x9E37_79B9_7F4A_7C15;
    }
    // xorshift64*
    state ^= state >> 12;
    state ^= state << 25;
    state ^= state >> 27;
    STATE.store(state, Ordering::Relaxed);
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// 上游 `Base64.getEncoder().encodeToString(...)`。
pub fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(chunk.get(1).copied().unwrap_or(0));
        let b2 = u32::from(chunk.get(2).copied().unwrap_or(0));
        let group = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(group >> 18) as usize & 63] as char);
        out.push(ALPHABET[(group >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(group >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[group as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 上游 `Base64.getDecoder().decode(...)`；非法输入返回 `None`（上游 `IllegalArgumentException`）。
pub fn base64_decode(input: &str) -> Option<Vec<u8>> {
    let value: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if !value.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(value.len() / 4 * 3);
    for chunk in value.chunks(4) {
        let mut group: u32 = 0;
        let mut available = 0usize;
        for (index, byte) in chunk.iter().enumerate() {
            let decoded = match byte {
                b'A'..=b'Z' => byte - b'A',
                b'a'..=b'z' => byte - b'a' + 26,
                b'0'..=b'9' => byte - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                b'=' => continue,
                _ => return None,
            } as u32;
            group |= decoded << (18 - index * 6);
            available += 1;
        }
        if available == 0 {
            continue;
        }
        out.push((group >> 16) as u8);
        if available > 2 {
            out.push((group >> 8) as u8);
        }
        if available > 3 {
            out.push(group as u8);
        }
    }
    Some(out)
}

// ------------------------------------------------------------- 握手结果

/// `BtnNetwork.configBtnNetwork` 的结束状态（`configSuccess` / `configResult`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BtnConfigStatus {
    /// 尚未尝试（`reloadConfig` 后的初值，等价于 `configSuccess=false`）
    Pending,
    /// `configSuccess=true`
    Success { submit: bool },
    /// HTTP 非 2xx（上游 `BTN_CONFIG_STATUS_UNSUCCESSFUL_HTTP_REQUEST`）
    HttpError { status: u16, body: String },
    /// 协议版本不兼容：客户端实现版本低于 `min_protocol_version`
    ClientTooOld { implemented: i32, min: i32 },
    /// 协议版本不兼容：服务端 `max_protocol_version` 低于客户端实现版本
    ServerTooOld { implemented: i32, max: i32 },
    /// 其它异常（缺字段 / JSON 非法 / 传输失败，上游 `BTN_CONFIG_STATUS_EXCEPTION`）
    Exception { kind: String, message: String },
}

impl BtnConfigStatus {
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }

    /// 上游 `configResult` 的 `Lang` 键
    pub fn lang_key(&self) -> &'static str {
        match self {
            Self::Pending => LANG_BTN_CONFIGURE_SYNC_SERVER,
            Self::Success { submit } => {
                if *submit {
                    LANG_BTN_CONFIG_STATUS_SUCCESSFUL
                } else {
                    LANG_BTN_CONFIG_STATUS_SUCCESSFUL_READ_ONLY
                }
            }
            Self::HttpError { .. } => LANG_BTN_CONFIG_STATUS_UNSUCCESSFUL_HTTP_REQUEST,
            Self::ClientTooOld { .. } => LANG_BTN_PROTOCOL_VERSION_CLIENT,
            Self::ServerTooOld { .. } => LANG_BTN_PROTOCOL_VERSION_SERVER,
            Self::Exception { .. } => LANG_BTN_CONFIG_STATUS_EXCEPTION,
        }
    }
}

/// [`BtnNetwork::sync_due`] 的报告（每个已到期的 ability 一条）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BtnAbilitySync {
    pub key: String,
    pub kind: BtnAbilityKind,
    /// 规则是否发生变化
    pub updated: bool,
    pub last_status: bool,
}

// -------------------------------------------------------------- BtnNetwork

/// 上游 `btn/BtnNetwork`：配置端点握手 + abilities 调度 + PoW captcha + 本地缓存。
///
/// 本移植**不创建任何线程**（上游有 `ScheduledExecutorService` 与 `BackgroundTaskManager`）；
/// 重试与到期判定都在 [`Self::check_if_need_retry_config`] / [`Self::sync_due`] 里以可注入
/// 时钟的方式暴露，由应用层按自己的定时器驱动。
pub struct BtnNetwork {
    config: RwLock<BtnNetworkConfig>,
    http: Arc<dyn BtnHttpClient>,
    metadata: Arc<dyn BtnMetadataStore>,
    pow_endpoint: RwLock<Option<String>>,
    abilities: RwLock<Vec<BtnAbility>>,
    config_success: AtomicBool,
    config_result: RwLock<BtnConfigStatus>,
    /// 上游 `nextConfigAttemptTime`（毫秒）
    next_config_attempt_ms: AtomicI64,
    /// 允许列表更新后需要解封的封禁表（上游 `btnNetwork.getServer().getBanList()`）
    ban_list: Option<Arc<StdMutex<BanList>>>,
    /// 本次同步是否更新了允许列表（触发 [`BtnNetworkOnline::on_rule_update`]）
    allowlist_changed: AtomicBool,
    /// 上报类能力的数据源（上游 `HistoryService` 等 DAO 的只读抽象；缺省 ⇒ 空库语义）
    submit_source: Option<Arc<dyn BtnSubmitSource>>,
    /// 心跳 `multi_if` 模式的本机网卡地址提供者（上游 `NetworkInterface.getNetworkInterfaces()`；
    /// 缺省为空列表 ⇒ 与上游「零可用网卡」等价，状态置 false）
    local_ips_provider: Option<Arc<dyn Fn() -> Vec<String> + Send + Sync>>,
    /// `BtnAbilitySubmitHistory` 的 `tryLock`（防止上一轮提交未完成时重入）
    submit_history_lock: StdMutex<()>,
    /// `LegacyBtnAbilitySubmitBans.lastReport`（epoch 毫秒；初值 0 ⇒ 首轮全量上报）
    legacy_bans_last_report_ms: AtomicI64,
}

impl std::fmt::Debug for BtnNetwork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BtnNetwork")
            .field("config", &self.config.read().ok().map(|c| c.clone()))
            .field(
                "config_success",
                &self.config_success.load(Ordering::Relaxed),
            )
            .finish_non_exhaustive()
    }
}

impl BtnNetwork {
    pub fn new(
        config: BtnNetworkConfig,
        http: Arc<dyn BtnHttpClient>,
        metadata: Arc<dyn BtnMetadataStore>,
    ) -> Self {
        // 上游 `reloadConfig()`：`nextConfigAttemptTime = System.currentTimeMillis()`（立即尝试）
        let next = if config.is_active() {
            i64::MIN
        } else {
            i64::MAX
        };
        Self {
            config: RwLock::new(config),
            http,
            metadata,
            pow_endpoint: RwLock::new(None),
            abilities: RwLock::new(Vec::new()),
            config_success: AtomicBool::new(false),
            config_result: RwLock::new(BtnConfigStatus::Pending),
            next_config_attempt_ms: AtomicI64::new(next),
            ban_list: None,
            allowlist_changed: AtomicBool::new(false),
            submit_source: None,
            local_ips_provider: None,
            submit_history_lock: StdMutex::new(()),
            legacy_bans_last_report_ms: AtomicI64::new(0),
        }
    }

    /// 绑定封禁表（[`DownloaderServer.getBanList()`]）；用于允许列表更新后的解封。
    pub fn with_ban_list(mut self, ban_list: Arc<StdMutex<BanList>>) -> Self {
        self.ban_list = Some(ban_list);
        self
    }

    /// 绑定上报数据源（[`BtnSubmitSource`]）；缺省 ⇒ 提交类能力按「空库」对待。
    pub fn with_submit_source(mut self, submit_source: Arc<dyn BtnSubmitSource>) -> Self {
        self.submit_source = Some(submit_source);
        self
    }

    /// 绑定心跳 `multi_if` 模式的本机网卡地址列表（上游 `NetworkInterface.getNetworkInterfaces()`；
    /// 平台无关实现由调用方提供，例如 `if-addrs` 或系统命令）。
    pub fn with_local_ips(mut self, provider: Arc<dyn Fn() -> Vec<String> + Send + Sync>) -> Self {
        self.local_ips_provider = Some(provider);
        self
    }

    // ------------------------------------------------------------ 配置

    pub fn config(&self) -> BtnNetworkConfig {
        self.config.read().map(|c| c.clone()).unwrap_or_default()
    }

    /// 上游 `reloadConfig()`：重载主配置、清空状态、重置 abilities 与重试时刻。
    pub fn reload_config(&self, config: BtnNetworkConfig) {
        let next = if config.is_active() {
            now_millis()
        } else {
            i64::MAX
        };
        if let Ok(mut slot) = self.config.write() {
            *slot = config;
        }
        self.config_success.store(false, Ordering::Relaxed);
        if let Ok(mut slot) = self.config_result.write() {
            *slot = BtnConfigStatus::Pending;
        }
        self.reset_abilities();
        self.next_config_attempt_ms.store(next, Ordering::Relaxed);
    }

    /// 上游 `resetAbilities()`：`abilities.values().forEach(unload); abilities.clear();`
    pub fn reset_abilities(&self) {
        if let Ok(mut slot) = self.pow_endpoint.write() {
            *slot = None;
        }
        if let Ok(mut slot) = self.abilities.write() {
            slot.clear();
        }
    }

    pub fn config_success(&self) -> bool {
        self.config_success.load(Ordering::Relaxed)
    }

    pub fn config_result(&self) -> BtnConfigStatus {
        self.config_result
            .read()
            .map(|s| s.clone())
            .unwrap_or(BtnConfigStatus::Pending)
    }

    pub fn abilities(&self) -> Vec<BtnAbility> {
        self.abilities.read().map(|a| a.clone()).unwrap_or_default()
    }

    /// 上游 `checkIfNeedRetryConfig()`：未成功且已过重试时刻 ⇒ 重新握手。
    ///
    /// `enabled=false` 时等价于上游「调度器为 null」：只把 `configSuccess` 置回 false，
    /// **不产生任何网络请求**。
    pub fn check_if_need_retry_config(&self, module: &BtnNetworkOnline) -> bool {
        self.check_if_need_retry_config_at(module, now_millis())
    }

    /// [`Self::check_if_need_retry_config`] 的可注入时钟版本。
    pub fn check_if_need_retry_config_at(&self, module: &BtnNetworkOnline, now_ms: i64) -> bool {
        if !self.config().is_active() {
            // 上游 `checkIfNeedRetryConfig` 的 else 分支
            self.config_success.store(false, Ordering::Relaxed);
            return false;
        }
        if self.config_success.load(Ordering::Relaxed) {
            return true;
        }
        if now_ms <= self.next_config_attempt_ms.load(Ordering::Relaxed) {
            return false;
        }
        self.config_btn_network_at(module, now_ms)
    }

    /// 上游 `configBtnNetwork()`：GET `configUrl` → 协议版本校验 → 构造 abilities → `load()`。
    ///
    /// 任何失败都记录 [`BtnConfigStatus`] 并把重试时刻推到 `now + 600s`（上游
    /// `RETRY_PERIOD_SECONDS`）；**不会**触碰既有规则。
    pub fn config_btn_network(&self, module: &BtnNetworkOnline) -> bool {
        self.config_btn_network_at(module, now_millis())
    }

    /// [`Self::config_btn_network`] 的可注入时钟版本。
    pub fn config_btn_network_at(&self, module: &BtnNetworkOnline, now_ms: i64) -> bool {
        let config = self.config();
        if !config.is_active() {
            return false;
        }
        // 上游 `btn.allow-script-execute`：`BtnNetworkOnline.reloadConfig()` 与
        // `BtnNetwork.reloadConfig()` 读的是同一个键，握手时同步给模块。
        module.set_allow_script(config.allow_script_execute);
        debug!("{LANG_BTN_CONFIGURE_SYNC_SERVER}: {}", config.config_url);
        let request = self.request(&config.config_url);
        match self.http.execute(request) {
            Ok(response) if response.is_successful() => {
                match self.apply_config_response(module, &response.body) {
                    Ok(()) => {
                        self.config_success.store(true, Ordering::Relaxed);
                        if let Ok(mut slot) = self.config_result.write() {
                            *slot = BtnConfigStatus::Success {
                                submit: config.submit,
                            };
                        }
                        true
                    }
                    Err(status) => {
                        self.fail_config(status, now_ms);
                        false
                    }
                }
            }
            Ok(response) => {
                self.fail_config(
                    BtnConfigStatus::HttpError {
                        status: response.status,
                        body: response.body,
                    },
                    now_ms,
                );
                false
            }
            Err(e) => {
                self.fail_config(
                    BtnConfigStatus::Exception {
                        kind: "TransportError".to_string(),
                        message: e.to_string(),
                    },
                    now_ms,
                );
                false
            }
        }
    }

    fn fail_config(&self, status: BtnConfigStatus, now_ms: i64) {
        error!(
            "{LANG_BTN_CONFIG_FAILS}: {} ({})",
            status.lang_key(),
            status_message(&status)
        );
        self.config_success.store(false, Ordering::Relaxed);
        if let Ok(mut slot) = self.config_result.write() {
            *slot = status;
        }
        self.next_config_attempt_ms
            .store(now_ms + RETRY_PERIOD_SECONDS * 1000, Ordering::Relaxed);
    }

    /// 解析配置端点响应并构造 abilities（上游 `configBtnNetwork` 的 try 块主体）。
    fn apply_config_response(
        &self,
        module: &BtnNetworkOnline,
        body: &str,
    ) -> Result<(), BtnConfigStatus> {
        let json: BtnConfigResponse =
            serde_json::from_str(body).map_err(|e| BtnConfigStatus::Exception {
                kind: "JsonSyntaxException".to_string(),
                message: e.to_string(),
            })?;
        let missing_field = || BtnConfigStatus::Exception {
            kind: "IllegalStateException".to_string(),
            message: LANG_MISSING_VERSION_PROTOCOL_FIELD.to_string(),
        };
        let min = json.min_protocol_version.ok_or_else(missing_field)?;
        if BTN_PROTOCOL_IMPL_VERSION < min {
            return Err(BtnConfigStatus::ClientTooOld {
                implemented: BTN_PROTOCOL_IMPL_VERSION,
                min,
            });
        }
        let max = json.max_protocol_version.ok_or_else(missing_field)?;
        if BTN_PROTOCOL_IMPL_VERSION > max {
            return Err(BtnConfigStatus::ServerTooOld {
                implemented: BTN_PROTOCOL_IMPL_VERSION,
                max,
            });
        }
        // 上游 `useLegacyAbilities = min_protocol_version < 20`：遗留协议没有 ip_* 列表能力
        let use_legacy_abilities = min < 20;
        self.reset_abilities();
        if let Some(pow) = &json.proof_of_work_captcha {
            if let Ok(mut slot) = self.pow_endpoint.write() {
                *slot = Some(pow.endpoint.clone());
            }
        }
        let submit = self.config().submit;
        let mut abilities: Vec<BtnAbility> = Vec::new();
        // 上游 `configBtnNetwork` 的注册顺序：遗留/现代分支 → 公共能力。
        // 注意遗留分支的 `submit_bans` 是 `LegacyBtnAbilitySubmitBans`，现代是 `BtnAbilitySubmitBans`。
        let mut register = |key: &str, kind: BtnAbilityKind| {
            if let Some(spec) = json.ability.get(key) {
                let mut ability = BtnAbility::from_spec(key, spec);
                ability.kind = kind;
                abilities.push(ability);
            }
        };
        if use_legacy_abilities {
            if submit {
                register("submit_peers", BtnAbilityKind::LegacySubmitPeers);
                register("submit_bans", BtnAbilityKind::LegacySubmitBans);
            }
            register("rules", BtnAbilityKind::Rules);
        } else {
            if submit {
                register("submit_bans", BtnAbilityKind::SubmitBans);
                register("submit_swarm", BtnAbilityKind::SubmitSwarm);
            }
            register("ip_denylist", BtnAbilityKind::IpDenyList);
            register("ip_allowlist", BtnAbilityKind::IpAllowList);
            register("rule_peer_identity", BtnAbilityKind::Rules);
        }
        if submit {
            register("submit_histories", BtnAbilityKind::SubmitHistories);
        }
        register("reconfigure", BtnAbilityKind::Reconfigure);
        register("heartbeat", BtnAbilityKind::Heartbeat);
        register("ip_query", BtnAbilityKind::IpQuery);
        let now = now_millis();
        for ability in &mut abilities {
            ability.next_due_ms =
                now + pseudo_random(&ability.key, ability.random_initial_delay_ms);
        }
        if let Ok(mut slot) = self.abilities.write() {
            *slot = abilities;
        }
        // 上游 `abilities.values().forEach(a -> { try { a.load(); } catch ... })`
        self.load_abilities(module);
        Ok(())
    }

    /// 上游各 ability 的 `load()`：从 `metadataDao` 读回缓存 → 注入模块。
    ///
    /// 允许列表从缓存加载后同样要解封（上游 `BtnAbilityIPAllowList.load()` 调
    /// `unbanAllowedBannedPeers()`）。
    pub fn load_abilities(&self, module: &BtnNetworkOnline) {
        let kinds: Vec<BtnAbilityKind> = self.abilities().iter().map(|a| a.kind).collect();
        for kind in kinds {
            self.load_ability(module, kind);
        }
        self.flush_allowlist_unban(module);
    }

    fn load_ability(&self, module: &BtnNetworkOnline, kind: BtnAbilityKind) {
        match kind {
            BtnAbilityKind::Rules => {
                if let Some(cache) = self.metadata.get(CACHE_KEY_RULES) {
                    if let Err(e) = module.apply_ruleset_json(&cache) {
                        warn!(
                            "{LANG_UNABLE_LOAD_BTN_ABILITY}: {} - {e}",
                            kind.upstream_name()
                        );
                    } else {
                        debug!("{LANG_BTN_RULES_LOADED_FROM_CACHE}");
                    }
                }
            }
            BtnAbilityKind::IpAllowList => {
                let version = self.metadata.get(CACHE_KEY_IP_ALLOWLIST_VERSION);
                if let Some(value) = self.metadata.get(CACHE_KEY_IP_ALLOWLIST_VALUE) {
                    let rev = version.as_deref().unwrap_or(INITIAL_REV);
                    let loaded = module.apply_ip_allowlist_text(&value, rev);
                    self.allowlist_changed.store(true, Ordering::Relaxed);
                    debug!("[BTN AllowList] 从缓存加载 {loaded} 条，版本 {rev}");
                }
            }
            BtnAbilityKind::IpDenyList => {
                let version = self.metadata.get(CACHE_KEY_IP_DENYLIST_VERSION);
                if let Some(value) = self.metadata.get(CACHE_KEY_IP_DENYLIST_VALUE) {
                    let rev = version.as_deref().unwrap_or(INITIAL_REV);
                    let loaded = module.apply_ip_denylist_text(&value, rev);
                    debug!("[BTN DenyList] 从缓存加载 {loaded} 条，版本 {rev}");
                }
            }
            // 上报类能力只有孤游标存于 metadata（无本地规则缓存），无需回灌
            _ => {}
        }
    }

    // ------------------------------------------------------- 请求构造 / PoW

    /// 上游 `setupHttpClient()` 的拦截器逻辑本体：给任意请求附加统一头部
    /// （`User-Agent` / `Content-Type` / `BTN-AppID` / `BTN-AppSecret` / `X-BTN-AppID` /
    /// `X-BTN-AppSecret` / `Authentication: Bearer <appId>@<appSecret>`；匿名账户加
    /// `X-BTN-InstallationID`）。上报类 POST 载荷与配置 GET 请求都走这里。
    fn apply_auth(&self, request: BtnHttpRequest) -> BtnHttpRequest {
        let config = self.config();
        let mut request = request
            .header("User-Agent", BTN_USER_AGENT)
            .header("Content-Type", "application/json")
            .header(HEADER_BTN_APP_ID, config.app_id.clone())
            .header(HEADER_BTN_APP_SECRET, config.app_secret.clone())
            .header(HEADER_X_BTN_APP_ID, config.app_id.clone())
            .header(HEADER_X_BTN_APP_SECRET, config.app_secret.clone())
            .header(
                HEADER_AUTHENTICATION,
                format!("Bearer {}@{}", config.app_id, config.app_secret),
            );
        if config.is_anonymous() {
            request = request.header(HEADER_X_BTN_INSTALLATION_ID, config.installation_id.clone());
        }
        request
    }

    /// 带认证头的 `GET` 请求（配置端点等）。
    fn request(&self, url: &str) -> BtnHttpRequest {
        self.apply_auth(BtnHttpRequest::get(url))
    }

    /// 上游 `gatherAndSolveCaptchaBlocking(requestBuilder, type)`：
    /// GET `<powCaptchaEndpoint>?type=<type>` → 求解 → 追加 `X-BTN-PowID` / `X-BTN-PowSolution`。
    ///
    /// 未配置 PoW 端点、求解失败或算法不支持时**不追加任何头**（与上游 `return;` /
    /// `catch (Throwable)` 的分支一致）。
    pub fn gather_and_solve_captcha(
        &self,
        request: BtnHttpRequest,
        pow_type: &str,
    ) -> BtnHttpRequest {
        let endpoint = match self.pow_endpoint.read() {
            Ok(slot) => match slot.as_ref() {
                Some(endpoint) if !endpoint.trim().is_empty() => endpoint.clone(),
                _ => return request,
            },
            Err(_) => return request,
        };
        let url = format!("{endpoint}?type={pow_type}");
        let response = match self.http.execute(self.request(&url)) {
            Ok(response) => response,
            Err(e) => {
                error!("Unable to gather or solve PoW Captcha: {e}");
                return request;
            }
        };
        if !response.is_successful() {
            error!(
                "{LANG_BTN_POW_CAPTCHA_LOAD_FROM_REMOTE}: {} {}",
                response.status, response.body
            );
            return request;
        }
        let data: PowCaptchaData = match serde_json::from_str(&response.body) {
            Ok(data) => data,
            Err(e) => {
                error!("Unable to gather or solve PoW Captcha: {e}");
                return request;
            }
        };
        let challenge = match base64_decode(&data.challenge_base64) {
            Some(challenge) => challenge,
            None => {
                error!("Unable to gather or solve PoW Captcha: invalid base64 challenge");
                return request;
            }
        };
        debug!("{LANG_BTN_POW_CAPTCHA_COMPUTING}");
        let started = now_millis();
        match solve_pow(&challenge, data.difficulty_bits, &data.algorithm) {
            Some(nonce) => {
                debug!(
                    "{LANG_BTN_POW_CAPTCHA_COMPUTE_COMPLETED}: {} ms",
                    now_millis() - started
                );
                request
                    .header(HEADER_X_BTN_POW_ID, data.id.clone())
                    .header(HEADER_X_BTN_POW_SOLUTION, base64_encode(&nonce))
            }
            None => {
                warn!("Unable to gather or solve PoW Captcha: solver gave up");
                request
            }
        }
    }

    // ------------------------------------------------------------- 同步

    /// 一次完整同步（上游各 ability 的 `updateRule()` 依次执行）。
    ///
    /// 返回是否有规则集被更新；允许列表变化后会按上游 `unbanAllowedBannedPeers()` 解封。
    pub fn sync(&self, module: &BtnNetworkOnline) -> bool {
        if !self.config().is_active() {
            return false;
        }
        self.allowlist_changed.store(false, Ordering::Relaxed);
        let updated = module.sync_from_transport(self);
        self.flush_allowlist_unban(module);
        updated
    }

    /// 上游 `scheduleWithFixedDelay(this::updateRule, random(0, randomInitialDelay), interval, MS)`：
    /// 只执行「已到期」的 ability，并把下一次触发时刻推进 `interval` 毫秒。
    pub fn sync_due(&self, module: &BtnNetworkOnline, now_ms: i64) -> Vec<BtnAbilitySync> {
        if !self.config().is_active() {
            return Vec::new();
        }
        self.allowlist_changed.store(false, Ordering::Relaxed);
        let mut due: Vec<(usize, BtnAbilityKind, i64)> = Vec::new();
        if let Ok(slot) = self.abilities.read() {
            for (index, ability) in slot.iter().enumerate() {
                if ability.kind.is_implemented() && now_ms >= ability.next_due_ms {
                    due.push((index, ability.kind, ability.interval_ms));
                }
            }
        }
        let mut report = Vec::with_capacity(due.len());
        for (index, kind, interval) in due {
            let (updated, reported_status) = match self.run_ability(module, kind, index, now_ms) {
                AbilityOutcome::Reported { updated, status } => (updated, Some(status)),
                AbilityOutcome::Skipped => (false, None),
            };
            let last_status = if let Ok(mut slot) = self.abilities.write() {
                if let Some(ability) = slot.get_mut(index) {
                    if let Some(status) = reported_status {
                        ability.last_status = status;
                        ability.last_status_at_ms = now_ms;
                    }
                    // 上游 fixed-delay：下一次触发 = 本次完成时刻 + interval
                    ability.next_due_ms = now_ms + interval;
                    ability.last_status
                } else {
                    true
                }
            } else {
                true
            };
            report.push(BtnAbilitySync {
                key: kind.json_key().to_string(),
                kind,
                updated,
                last_status,
            });
        }
        self.flush_allowlist_unban(module);
        report
    }

    /// 分发一个到期能力到唯一的执行入口（上游各 ability 的 `updateRule` / `submit` / 心跳回调）。
    fn run_ability(
        &self,
        module: &BtnNetworkOnline,
        kind: BtnAbilityKind,
        index: usize,
        now_ms: i64,
    ) -> AbilityOutcome {
        match kind {
            BtnAbilityKind::Rules => AbilityOutcome::Reported {
                updated: self.update_rules(module),
                status: true,
            },
            BtnAbilityKind::IpAllowList => {
                let rev = module.ip_list_version(true);
                AbilityOutcome::Reported {
                    updated: self.update_ip_list(module, BtnIpAbility::AllowList, &rev),
                    status: true,
                }
            }
            BtnAbilityKind::IpDenyList => {
                let rev = module.ip_list_version(false);
                AbilityOutcome::Reported {
                    updated: self.update_ip_list(module, BtnIpAbility::DenyList, &rev),
                    status: true,
                }
            }
            BtnAbilityKind::SubmitBans => self.run_submit_bans(index),
            BtnAbilityKind::SubmitSwarm => self.run_submit_swarm(index),
            BtnAbilityKind::SubmitHistories => self.run_submit_histories(index),
            BtnAbilityKind::LegacySubmitPeers => self.run_legacy_submit_peers(index),
            BtnAbilityKind::LegacySubmitBans => self.run_legacy_submit_bans(index),
            BtnAbilityKind::Heartbeat => {
                let status = self.run_heartbeat(index, now_ms);
                AbilityOutcome::Reported {
                    updated: false,
                    status,
                }
            }
            BtnAbilityKind::Reconfigure => {
                let status = self.run_reconfigure(module, index, now_ms);
                AbilityOutcome::Reported {
                    updated: false,
                    status,
                }
            }
            // `IpQuery` 不注册调度器（`is_scheduled() == false`），不会进入 due
            BtnAbilityKind::IpQuery | BtnAbilityKind::Other => AbilityOutcome::Reported {
                updated: false,
                status: true,
            },
        }
    }

    fn ability_at(&self, index: usize) -> Option<BtnAbility> {
        self.abilities
            .read()
            .ok()
            .and_then(|slot| slot.get(index).cloned())
    }

    fn set_last_status(&self, index: usize, status: bool, now_ms: i64) {
        if let Ok(mut slot) = self.abilities.write() {
            if let Some(ability) = slot.get_mut(index) {
                ability.last_status = status;
                ability.last_status_at_ms = now_ms;
            }
        }
    }

    fn metadata_i64(&self, key: &str, default: i64) -> i64 {
        self.metadata
            .get(key)
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(default)
    }

    // ---------------------------------------------------------- submit_bans

    fn run_submit_bans(&self, index: usize) -> AbilityOutcome {
        let Some(ability) = self.ability_at(index) else {
            return AbilityOutcome::Reported {
                updated: false,
                status: false,
            };
        };
        let Some(source) = self.submit_source.as_ref() else {
            debug!("BTN submit_bans：未注入数据源，按空库处理");
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        };
        let mut cursor = self.metadata_i64(METADATA_KEY_BANS_CURSOR, 0);
        let mut total = 0usize;
        loop {
            let rows = source.batch_ban_history(cursor, 100);
            if rows.is_empty() {
                break;
            }
            let ping = BtnBanPing {
                bans: rows.iter().map(|row| row.clone().into_ping()).collect(),
            };
            if let Err(error) = self.post_gzip_report(&ability, &ping) {
                warn!("{LANG_BTN_REQUEST_FAILS}: {error}");
                self.set_last_status(index, false, now_millis());
                return AbilityOutcome::Reported {
                    updated: false,
                    status: false,
                };
            }
            // 页提交成功后游标推进到本页最大 id（上游 `historyEntities.getLast().getId()`）
            cursor = rows.iter().map(|row| row.id).max().unwrap_or(cursor);
            self.metadata
                .set(METADATA_KEY_BANS_CURSOR, &cursor.to_string());
            total += rows.len();
            // 上游 `page.hasNext()`：本页不满 ⇒ 结束
            if rows.len() < 100 {
                break;
            }
        }
        debug!("BTN submit_bans：已上报 {total} 条");
        AbilityOutcome::Reported {
            updated: total > 0,
            status: true,
        }
    }

    // ---------------------------------------------------------- submit_swarm

    /// 上游 `getMemCursor`：`"<lastTimeSeen>,<id>"` 二元游标。
    fn swarm_cursor(&self) -> (i64, i64) {
        match self.metadata.get(METADATA_KEY_SWARM_CURSOR) {
            Some(value) => {
                let parts: Vec<&str> = value.splitn(2, ',').collect();
                let last = parts
                    .first()
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0);
                let id = parts
                    .get(1)
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0);
                (last, id)
            }
            None => (0, 0),
        }
    }

    fn run_submit_swarm(&self, index: usize) -> AbilityOutcome {
        let Some(ability) = self.ability_at(index) else {
            return AbilityOutcome::Reported {
                updated: false,
                status: false,
            };
        };
        let Some(source) = self.submit_source.as_ref() else {
            debug!("BTN submit_swarm：未注入数据源，按空库处理");
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        };
        let (mut last_seen, mut id) = self.swarm_cursor();
        let mut total = 0usize;
        loop {
            let rows = source.batch_swarm_history(last_seen, id, 1000);
            if rows.is_empty() {
                break;
            }
            let ping = BtnSwarmPeerPing {
                swarms: rows.iter().map(|row| row.clone().into_ping()).collect(),
            };
            if let Err(error) = self.post_gzip_report(&ability, &ping) {
                warn!("{LANG_BTN_REQUEST_FAILS}: {error}");
                self.set_last_status(index, false, now_millis());
                return AbilityOutcome::Reported {
                    updated: false,
                    status: false,
                };
            }
            // 排序保证最后一条即最大（上游 `getLast()`）
            last_seen = rows
                .last()
                .map(|row| row.last_time_seen_ms)
                .unwrap_or(last_seen);
            id = rows.last().map(|row| row.id).unwrap_or(id);
            self.metadata
                .set(METADATA_KEY_SWARM_CURSOR, &format!("{last_seen},{id}"));
            total += rows.len();
            if rows.len() < 1000 {
                break;
            }
        }
        debug!("BTN submit_swarm：已上报 {total} 条");
        AbilityOutcome::Reported {
            updated: total > 0,
            status: true,
        }
    }

    // ------------------------------------------------------- submit_histories

    /// 上游 `getLastSubmitAtTimestamp`：读取游标并做 30 天饱和（过期/缺失 ⇒ 重设为当前时刻）。
    fn history_timestamp(&self) -> i64 {
        let now = now_millis();
        let valid = self
            .metadata
            .get(METADATA_KEY_HISTORY_TIMESTAMP)
            .and_then(|value| value.parse::<i64>().ok())
            .is_some_and(|stored| stored >= now - 30 * 24 * 60 * 60 * 1000);
        if !valid {
            self.metadata
                .set(METADATA_KEY_HISTORY_TIMESTAMP, &now.to_string());
            return now;
        }
        self.metadata_i64(METADATA_KEY_HISTORY_TIMESTAMP, now)
    }

    fn run_submit_histories(&self, index: usize) -> AbilityOutcome {
        // 上游 `lock.tryLock()`：上一轮未结束则跳过本次，且不更新 lastStatus
        let Ok(_guard) = self.submit_history_lock.try_lock() else {
            debug!("BTN submit_histories：上一轮仍在提交，跳过本次");
            return AbilityOutcome::Skipped;
        };
        let Some(ability) = self.ability_at(index) else {
            return AbilityOutcome::Reported {
                updated: false,
                status: false,
            };
        };
        let Some(source) = self.submit_source.as_ref() else {
            debug!("BTN submit_histories：未注入数据源，按空库处理");
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        };
        let now = now_millis();
        let mut last_submit_at = self.history_timestamp();
        let mut total = 0usize;
        loop {
            let rows = source.batch_peer_history(last_submit_at, 5000);
            if rows.is_empty() {
                break;
            }
            let ping = LegacyBtnPeerHistoryPing {
                populate_time: now,
                peers: rows.iter().map(|row| row.clone().into_ping()).collect(),
            };
            if let Err(error) = self.post_gzip_report(&ability, &ping) {
                warn!("{LANG_BTN_REQUEST_FAILS}: {error}");
                self.set_last_status(index, false, now_millis());
                return AbilityOutcome::Reported {
                    updated: false,
                    status: false,
                };
            }
            total += rows.len();
            let mut last_record_at = rows
                .last()
                .map(|row| row.last_time_seen_ms)
                .unwrap_or(last_submit_at);
            if last_record_at >= last_submit_at {
                // 与上游一致：游标与最新记录相同时 +1，防止死循环
                last_record_at = if last_record_at == last_submit_at {
                    last_record_at + 1
                } else {
                    last_record_at
                };
                last_submit_at = last_record_at;
            } else {
                last_submit_at = now_millis();
            }
            self.metadata
                .set(METADATA_KEY_HISTORY_TIMESTAMP, &last_submit_at.to_string());
            if rows.len() < 5000 {
                break;
            }
        }
        debug!("BTN submit_histories：已上报 {total} 条");
        AbilityOutcome::Reported {
            updated: total > 0,
            status: true,
        }
    }

    // ------------------------------------------------------- legacy 上报

    /// 遗留 `submit_peers`：一次性全量 live peer 快照（无分页、无游标）。
    fn run_legacy_submit_peers(&self, index: usize) -> AbilityOutcome {
        let Some(ability) = self.ability_at(index) else {
            return AbilityOutcome::Reported {
                updated: false,
                status: false,
            };
        };
        let Some(source) = self.submit_source.as_ref() else {
            debug!("BTN submit_peers：未注入数据源，按空库处理");
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        };
        let peers: Vec<LegacyBtnPeer> = source
            .legacy_peer_snapshot()
            .iter()
            .map(|row| row.clone().into_peer())
            .collect();
        if peers.is_empty() {
            // 上游：空列表不发请求，`setLastStatus(true, BTN_LAST_REPORT_EMPTY)`
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        }
        let ping = LegacyBtnPeerPing {
            populate_time: now_millis(),
            peers,
        };
        match self.post_gzip_report(&ability, &ping) {
            Ok(()) => AbilityOutcome::Reported {
                updated: true,
                status: true,
            },
            Err(error) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: {error}");
                self.set_last_status(index, false, now_millis());
                AbilityOutcome::Reported {
                    updated: false,
                    status: false,
                }
            }
        }
    }

    /// 遗留 `submit_bans`：上报 `banAt > lastReport` 的新增封禁
    /// （上游按 `BanMetadata.getBanAt` 过滤，`lastReport` 为内存态）。
    fn run_legacy_submit_bans(&self, index: usize) -> AbilityOutcome {
        let Some(ability) = self.ability_at(index) else {
            return AbilityOutcome::Reported {
                updated: false,
                status: false,
            };
        };
        let Some(source) = self.submit_source.as_ref() else {
            debug!("BTN legacy submit_bans：未注入数据源，按空库处理");
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        };
        let now = now_millis();
        let last_report = self.legacy_bans_last_report_ms.load(Ordering::Relaxed);
        let rows: Vec<BtnLegacyBanRow> = source
            .legacy_ban_snapshot()
            .into_iter()
            .filter(|row| row.ban_at_ms > last_report)
            .collect();
        if rows.is_empty() {
            self.legacy_bans_last_report_ms
                .store(now, Ordering::Relaxed);
            return AbilityOutcome::Reported {
                updated: false,
                status: true,
            };
        }
        let ping = LegacyBtnBanPing {
            populate_time: now,
            bans: rows.iter().map(|row| row.clone().into_ping()).collect(),
        };
        match self.post_gzip_report(&ability, &ping) {
            Ok(()) => {
                let last_ban_at = rows.iter().map(|row| row.ban_at_ms).max().unwrap_or(now);
                self.legacy_bans_last_report_ms
                    .store(last_ban_at, Ordering::Relaxed);
                AbilityOutcome::Reported {
                    updated: true,
                    status: true,
                }
            }
            Err(error) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: {error}");
                self.set_last_status(index, false, now);
                AbilityOutcome::Reported {
                    updated: false,
                    status: false,
                }
            }
        }
    }

    // ------------------------------------------------------------- heartbeat

    fn run_heartbeat(&self, index: usize, now_ms: i64) -> bool {
        let Some(ability) = self.ability_at(index) else {
            return false;
        };
        let ok = if ability.multi_if {
            // 上游 `NetworkInterface.getNetworkInterfaces()`；本移植由注入方提供
            let ips: Vec<String> = self
                .local_ips_provider
                .as_ref()
                .map(|provider| provider())
                .unwrap_or_default();
            if ips.is_empty() {
                return false;
            }
            // 上游并发对每个地址各发一次（不能短路）；任一成功即整体成功
            let mut any_success = false;
            for ip in &ips {
                if self.send_heartbeat(&ability, ip) {
                    any_success = true;
                }
            }
            any_success
        } else {
            self.send_heartbeat(&ability, "default")
        };
        self.set_last_status(index, ok, now_ms);
        ok
    }

    /// 一次心跳（上游 `sendHeartBeat`）：`POST {"ifaddr":"<if>|default"}`，解析响应的
    /// `external_ip`；默认接口用**无认证**的裸 client（上游 `getHttpUtil()`），
    /// `multi_if` 的每个地址用带认证的 client。
    fn send_heartbeat(&self, ability: &BtnAbility, ifaddr: &str) -> bool {
        let body = serde_json::to_vec(&serde_json::json!({ "ifaddr": ifaddr })).unwrap_or_default();
        let mut request = BtnHttpRequest::post(&ability.endpoint, body)
            .header("User-Agent", BTN_USER_AGENT)
            .header("Content-Type", "application/json");
        if ifaddr != "default" {
            request = self.apply_auth(request);
        }
        if ability.pow_captcha {
            request = self.gather_and_solve_captcha(request, "heartbeat");
        }
        match self.http.execute(request) {
            Ok(response) if response.is_successful() => {
                let external_ip = serde_json::from_str::<BtnHeartbeatResponse>(&response.body)
                    .ok()
                    .and_then(|data| data.external_ip);
                match external_ip {
                    Some(ip) => debug!("BTN heartbeat `{ifaddr}` → 外部 IP: {ip}"),
                    None => debug!("BTN heartbeat `{ifaddr}` → 服务器未返回 external_ip"),
                }
                true
            }
            Ok(response) => {
                warn!(
                    "{LANG_BTN_REQUEST_FAILS}: heartbeat `{ifaddr}` → HTTP {} - {}",
                    response.status, response.body
                );
                false
            }
            Err(error) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: heartbeat `{ifaddr}` → {error}");
                false
            }
        }
    }

    // ---------------------------------------------------------- reconfigure

    fn run_reconfigure(&self, module: &BtnNetworkOnline, index: usize, now_ms: i64) -> bool {
        let Some(ability) = self.ability_at(index) else {
            return false;
        };
        let Some(local_version) = ability.reconfigure_version.as_deref() else {
            return true;
        };
        let response = match self.http.execute(self.request(&self.config().config_url)) {
            Ok(response) => response,
            Err(error) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: reconfigure → {error}");
                self.set_last_status(index, false, now_ms);
                return false;
            }
        };
        if !response.is_successful() {
            warn!(
                "{LANG_BTN_REQUEST_FAILS}: reconfigure → HTTP {} - {}",
                response.status, response.body
            );
            self.set_last_status(index, false, now_ms);
            return false;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(&response.body) else {
            warn!("{LANG_BTN_REQUEST_FAILS}: reconfigure → 响应不是合法 JSON");
            self.set_last_status(index, false, now_ms);
            return false;
        };
        let Some(remote) = json
            .pointer("/ability/reconfigure/version")
            .and_then(|value| value.as_str())
        else {
            // 服务端已移除 reconfigure ⇒ 视为禁用（上游 `BTN_RECONFIGURE_DISABLED_BY_SERVER`）
            return true;
        };
        if remote == local_version {
            return true;
        }
        info!("BTN reconfigure：服务端 {remote} != 本地 {local_version}，重新握手");
        let ok = self.config_btn_network_at(module, now_ms);
        if !ok {
            self.set_last_status(index, false, now_ms);
        }
        ok
    }

    // ------------------------------------------------------------ ip_query

    /// 上游 `BtnAbilityIpQuery.query(address)`：`GET endpoint?ip=<urlencoded>`（认证头 + 可选
    /// PoW `ip_query`）。未配置该能力时返回 `Ok(None)`；HTTP 失败抛出 `anyhow::Error`。
    pub fn query_ip(&self, address: &str) -> anyhow::Result<Option<IpQueryResult>> {
        let Some(ability) = self
            .abilities()
            .into_iter()
            .find(|ability| ability.kind == BtnAbilityKind::IpQuery)
        else {
            return Ok(None);
        };
        let url = append_url(&ability.endpoint, &[("ip", &url_encode_query(address))]);
        let mut request = self.request(&url);
        if ability.pow_captcha {
            request = self.gather_and_solve_captcha(request, "ip_query");
        }
        let response = self.http.execute(request)?;
        if !response.is_successful() {
            anyhow::bail!("HTTP {} - {}", response.status, response.body);
        }
        Ok(Some(serde_json::from_str(&response.body)?))
    }

    /// 上游 `createGzipRequestBody` + 认证头 + 可选 PoW：payload → gzip → POST。
    /// 非 2xx 视为失败（上游抛 `IllegalStateException`，游标不推进、状态置 false）。
    fn post_gzip_report(
        &self,
        ability: &BtnAbility,
        payload: &impl Serialize,
    ) -> anyhow::Result<()> {
        let json = serde_json::to_vec(payload)?;
        let body = gzip_bytes(&json)?;
        let request = BtnHttpRequest::post(&ability.endpoint, body)
            .header("User-Agent", BTN_USER_AGENT)
            .header("Content-Type", "application/json")
            .header("Content-Encoding", "gzip");
        let mut request = self.apply_auth(request);
        if ability.pow_captcha {
            request = self.gather_and_solve_captcha(request, ability.kind.pow_type());
        }
        let response = self.http.execute(request)?;
        if !response.is_successful() {
            anyhow::bail!("HTTP {} - {}", response.status, response.body);
        }
        Ok(())
    }

    fn update_rules(&self, module: &BtnNetworkOnline) -> bool {
        let rev = module
            .ruleset_version()
            .unwrap_or_else(|| INITIAL_REV.to_string());
        match self.fetch_ruleset(&rev) {
            Ok(Some(ruleset)) => match module.apply_ruleset(&ruleset) {
                Ok(()) => {
                    info!(
                        "{LANG_BTN_UPDATE_RULES_SUCCESSES}: {}",
                        module.ruleset_version().unwrap_or_default()
                    );
                    true
                }
                Err(e) => {
                    warn!("{LANG_BTN_REQUEST_FAILS}: {e}");
                    false
                }
            },
            Ok(None) => false,
            Err(e) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: {e}");
                false
            }
        }
    }

    fn update_ip_list(&self, module: &BtnNetworkOnline, kind: BtnIpAbility, rev: &str) -> bool {
        let allow = kind == BtnIpAbility::AllowList;
        match self.fetch_ip_list(kind, rev) {
            Ok(Some((text, version))) => {
                let loaded = if allow {
                    module.apply_ip_allowlist_text(&text, &version)
                } else {
                    module.apply_ip_denylist_text(&text, &version)
                };
                let lang = if allow {
                    LANG_BTN_ABILITY_ALLOWLIST_LOADED_FROM_REMOTE
                } else {
                    LANG_BTN_ABILITY_DENYLIST_LOADED_FROM_REMOTE
                };
                info!("{lang}: {version}, {loaded}");
                true
            }
            Ok(None) => false,
            Err(e) => {
                warn!("{LANG_BTN_REQUEST_FAILS}: {e}");
                false
            }
        }
    }

    /// 上游允许列表更新后（`@Subscribe onRuleUpdate` / `finally { unbanAllowedBannedPeers(); }`）
    /// 解封命中允许列表的地址。
    fn flush_allowlist_unban(&self, module: &BtnNetworkOnline) {
        if !self.allowlist_changed.swap(false, Ordering::Relaxed) {
            return;
        }
        if let Some(ban_list) = &self.ban_list {
            module.on_rule_update(ban_list);
        }
    }

    /// 上游 `URLUtil.appendUrl(endpoint, Map.of("rev", version))`。
    fn rev_url(&self, endpoint: &str, rev: &str) -> String {
        if endpoint.contains('?') {
            format!("{endpoint}&rev={rev}")
        } else {
            format!("{endpoint}?rev={rev}")
        }
    }
}

impl BtnTransport for BtnNetwork {
    /// 上游 `BtnAbilityRules.updateRule`：GET `<endpoint>?rev=<version>`；
    /// 204 ⇒ 无变化；非 2xx ⇒ 失败；2xx ⇒ 解析规则集并写入 `metadataDao` 缓存。
    fn fetch_ruleset(&self, rev: &str) -> anyhow::Result<Option<BtnRuleset>> {
        let Some(ability) = self
            .abilities()
            .into_iter()
            .find(|a| a.kind == BtnAbilityKind::Rules)
        else {
            // 上游：未构造该 ability ⇒ 不存在这次请求
            return Ok(None);
        };
        let url = self.rev_url(&ability.endpoint, rev);
        let mut request = self.request(&url);
        if ability.pow_captcha {
            request = self.gather_and_solve_captcha(request, ability.kind.pow_type());
        }
        let response = self.http.execute(request)?;
        if response.is_no_content() {
            return Ok(None);
        }
        if !response.is_successful() {
            anyhow::bail!("HTTP {} - {}", response.status, response.body);
        }
        let ruleset: BtnRuleset = serde_json::from_str(&response.body)?;
        self.metadata.set(CACHE_KEY_RULES, &response.body);
        Ok(Some(ruleset))
    }

    /// 上游 `BtnAbilityIP*List.updateRule`：同上，额外取 `X-BTN-ContentVersion` 头。
    fn fetch_ip_list(
        &self,
        kind: BtnIpAbility,
        rev: &str,
    ) -> anyhow::Result<Option<(String, String)>> {
        let wanted = match kind {
            BtnIpAbility::AllowList => BtnAbilityKind::IpAllowList,
            BtnIpAbility::DenyList => BtnAbilityKind::IpDenyList,
        };
        let Some(ability) = self.abilities().into_iter().find(|a| a.kind == wanted) else {
            return Ok(None);
        };
        let url = self.rev_url(&ability.endpoint, rev);
        let mut request = self.request(&url);
        if ability.pow_captcha {
            request = self.gather_and_solve_captcha(request, ability.kind.pow_type());
        }
        let response = self.http.execute(request)?;
        if response.is_no_content() {
            return Ok(None);
        }
        if !response.is_successful() {
            anyhow::bail!("HTTP {} - {}", response.status, response.body);
        }
        // 上游 `response.header("X-BTN-ContentVersion", "unknown")`
        let version = response
            .header(HEADER_X_BTN_CONTENT_VERSION)
            .unwrap_or("unknown")
            .to_string();
        let (version_key, value_key) = match kind {
            BtnIpAbility::AllowList => {
                (CACHE_KEY_IP_ALLOWLIST_VERSION, CACHE_KEY_IP_ALLOWLIST_VALUE)
            }
            BtnIpAbility::DenyList => (CACHE_KEY_IP_DENYLIST_VERSION, CACHE_KEY_IP_DENYLIST_VALUE),
        };
        self.metadata.set(version_key, &version);
        self.metadata.set(value_key, &response.body);
        if kind == BtnIpAbility::AllowList {
            self.allowlist_changed.store(true, Ordering::Relaxed);
        }
        Ok(Some((response.body, version)))
    }
}

// ------------------------------------------------------------------ 工具

/// 单个脉冲执行结果（由 `BtnNetwork::run_ability` 返回，`sync_due` 统一落账）。
enum AbilityOutcome {
    /// 本轮已执行：`updated` 是否有可上报变化；`status` 即上游 `lastStatus`。
    Reported { updated: bool, status: bool },
    /// 本轮被跳过（如 `tryLock` 未取到）：lastStatus 保持不变。
    Skipped,
}

/// 上游 `okio.GzipSink`：把载荷压缩成 gzip 字节（请求带 `Content-Encoding: gzip`）。
pub fn gzip_bytes(payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(payload)?;
    Ok(encoder.finish()?)
}

/// 上游 `InfoHashUtil.getHashedIdentifier`：infohash 小写 → CRC32 十六进制盐 →
/// SHA-256(明文 + 盐) 的小写十六进制。Guava（`Hashing.crc32().toString()` /
/// `Hashing.sha256().toString()`）输出与 `{:08x}` / `{:02x}` 一致。
pub fn get_hashed_identifier(torrent_info_hash: &str) -> String {
    use sha2::Digest;

    let handled = torrent_info_hash.to_lowercase();
    let salt = format!("{:08x}", crc32fast::hash(handled.as_bytes()));
    let mut hasher = sha2::Sha256::new();
    hasher.update(handled.as_bytes());
    hasher.update(salt.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// 上游 `URLEncoder.encode(str, UTF_8)` 的查询段编码（空格 → `+`，保留 `-_.*` 与字母数字）。
fn url_encode_query(input: &str) -> String {
    use std::fmt::Write;

    let mut encoded = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'*' => {
                encoded.push(byte as char);
            }
            b' ' => encoded.push('+'),
            _ => {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
    }
    encoded
}

/// 上游 `URLUtil.appendUrl(endpoint, params)`：按需用 `?` / `&` 追加密钥值对（已编码值）。
fn append_url(endpoint: &str, params: &[(&str, &str)]) -> String {
    let mut url = endpoint.to_string();
    for (key, value) in params {
        if url.contains('?') {
            url.push('&');
        } else {
            url.push('?');
        }
        url.push_str(key);
        url.push('=');
        url.push_str(value);
    }
    url
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 上游 `ThreadLocalRandom.current().nextLong(bound)`：由 ability 键推导的确定性伪随机
/// 初始延迟（避免测试中的不确定性）；`bound <= 0` 时为 0。
fn pseudo_random(seed: &str, bound: i64) -> i64 {
    if bound <= 0 {
        return 0;
    }
    let mut hash: u64 = 0xCBF2_9CE4_8422_2325;
    for byte in seed.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100_0000_01B3);
    }
    (hash % bound as u64) as i64
}

fn status_message(status: &BtnConfigStatus) -> String {
    match status {
        BtnConfigStatus::Pending => "pending".to_string(),
        BtnConfigStatus::Success { submit } => format!("submit={submit}"),
        BtnConfigStatus::HttpError { status, body } => format!("{status} - {body}"),
        BtnConfigStatus::ClientTooOld { implemented, min } => {
            format!("client {implemented} < min {min}")
        }
        BtnConfigStatus::ServerTooOld { implemented, max } => {
            format!("client {implemented} > max {max}")
        }
        BtnConfigStatus::Exception { kind, message } => format!("{kind}: {message}"),
    }
}

#[cfg(test)]
mod tests {
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
}
