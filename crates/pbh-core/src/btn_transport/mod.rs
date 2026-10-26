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


mod network;
mod pow;
mod types;
mod util;

pub use network::*;
pub use pow::*;
pub use types::*;
pub use util::{get_hashed_identifier, gzip_bytes, now_millis};
pub(crate) use util::{append_url, pseudo_random, url_encode_query};

#[cfg(test)]
mod tests;
