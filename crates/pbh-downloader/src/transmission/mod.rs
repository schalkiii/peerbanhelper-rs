//! Transmission 适配器，忠实复刻上游 `Transmission`（>= 4.1.0）。
//!
//! 要点：
//! - JSON-RPC over HTTP：`{endpoint}{rpc-url}`（默认 `/transmission/rpc`），
//!   首次请求返回 **409** 并从 `X-Transmission-Session-Id` 头取会话 id，随后重试（对齐 cordelia `TrClient`）。
//! - 登录 = `session-get` 校验版本（截断到 5 字符，要求 >= 4.1.0），并确保 blocklist 指向
//!   PBH 自己的 `/blocklist/p2p-plain-format` 端点，必要时 `session-set` + `blocklist-update`。
//! - peers 随 `torrent-get` 一起返回（上游同样如此），因此本适配器缓存每个 torrent 的 peers，
//!   由 `fetch_peers` 读取，避免额外的 RPC 调用。
//! - Transmission 只支持「整份 blocklist 更新」，不支持按 peer 增量封禁，
//!   因此 `ban_peers` 不做任何事（wave 会为本适配器关闭增量封禁，见 SPEC §3.8）。

pub mod dto;

use crate::http::{BoxFuture, HttpFetcher, HttpRequest, ReqwestFetcher};
use crate::{BanEntry, Downloader, DownloaderFeature, DownloaderStatistics, LoginResult};
use dto::*;
use pbh_core::defaults::qb as qbcfg;
use pbh_core::i18n::{Param, TranslationComponent, Translator};
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::remap::RemapConfig;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tracing::error;

const SESSION_HEADER: &str = "X-Transmission-Session-Id";

/// `RqSessionSet` 里与限速有关的字段（值逐字对齐上游 `@SerializedName`）。
const SESSION_SET_ALT_SPEED_ENABLED: &str = "alt-speed-enabled";
const SESSION_SET_ALT_SPEED_TIME_ENABLED: &str = "alt-speed-time-enabled";
const SESSION_SET_SPEED_LIMIT_DOWN: &str = "speed-limit-down";
const SESSION_SET_SPEED_LIMIT_DOWN_ENABLED: &str = "speed-limit-down-enabled";
const SESSION_SET_SPEED_LIMIT_UP: &str = "speed-limit-up";
const SESSION_SET_SPEED_LIMIT_UP_ENABLED: &str = "speed-limit-up-enabled";

/// `RqSessionGet` 请求的 `fields`（逐字对齐 cordelia `types.Fields` 的常量值：
/// `downloadLimit` / `downloadLimited` / `uploadLimit` / `uploadLimited`）。
const SESSION_GET_SPEED_FIELDS: [&str; 4] = [
    "downloadLimit",
    "downloadLimited",
    "uploadLimit",
    "uploadLimited",
];

/// Transmission 的 `speed-limit-*` 单位是 **KB/s**（response 乘 1024 -> bytes/s）。
const SPEED_LIMIT_UNIT: i64 = 1024;

const MSG_FAILED_RETRIEVE_SPEED_LIMITER: &str = "DOWNLOADER_FAILED_RETRIEVE_SPEED_LIMITER";
const MSG_FAILED_SET_SPEED_LIMITER: &str = "DOWNLOADER_FAILED_SET_SPEED_LIMITER";

/// 对齐上游 `tlUI`：下载器不持有 locale，使用服务端默认文案语言。
const UI_LOCALE: &str = "zh_cn";
const RPC_FIELDS: &[&str] = &[
    "id",
    "hashString",
    "name",
    "peersConnected",
    "status",
    "totalSize",
    "peers",
    "rateDownload",
    "rateUpload",
    "peerLimit",
    "percentDone",
    "sizeWhenDone",
    "trackerList",
    "trackerStats",
    "isPrivate",
];

#[derive(Clone, Debug)]
pub struct TRConfig {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub username: String,
    pub password: String,
    pub verify_ssl: bool,
    pub rpc_url: String,
    pub ignore_private: bool,
    pub paused: bool,
    /// PBH 自身 web 服务对外地址，用于生成 blocklist URL
    pub blocklist_url: String,
}

impl Default for TRConfig {
    fn default() -> Self {
        Self {
            id: "transmission".into(),
            name: "Transmission".into(),
            endpoint: "http://127.0.0.1:9091".into(),
            username: String::new(),
            password: String::new(),
            verify_ssl: true,
            rpc_url: "/transmission/rpc".into(),
            ignore_private: false,
            paused: false,
            blocklist_url: "http://127.0.0.1:9898/blocklist/p2p-plain-format".into(),
        }
    }
}

pub struct TransmissionDownloader {
    config: TRConfig,
    rpc_endpoint: String,
    http: Arc<dyn HttpFetcher>,
    /// `X-Transmission-Session-Id`（409 握手后写入）
    session_id: Mutex<String>,
    last_version: Mutex<String>,
    /// `torrent-get` 带回来的 peers，按 torrent hash 缓存
    peers_cache: Mutex<HashMap<String, Vec<PeerData>>>,
    remap: RemapConfig,
    healthy: Mutex<bool>,
}

impl TransmissionDownloader {
    pub fn new(config: TRConfig, remap: RemapConfig) -> anyhow::Result<Self> {
        let fetcher = Arc::new(ReqwestFetcher::new(
            config.verify_ssl,
            qbcfg::CONNECT_TIMEOUT_SECS,
            qbcfg::READ_TIMEOUT_SECS,
        )?);
        Self::with_fetcher(config, remap, fetcher)
    }

    pub fn with_fetcher(
        config: TRConfig,
        remap: RemapConfig,
        http: Arc<dyn HttpFetcher>,
    ) -> anyhow::Result<Self> {
        let endpoint = config.endpoint.trim_end_matches('/').to_string();
        let rpc_endpoint = format!("{}{}", endpoint, config.rpc_url);
        Ok(Self {
            config,
            rpc_endpoint,
            http,
            session_id: Mutex::new(String::new()),
            last_version: Mutex::new(String::new()),
            peers_cache: Mutex::new(HashMap::new()),
            remap,
            healthy: Mutex::new(false),
        })
    }

    /// 已协商到的版本（去掉尾部非数字部分前的 5 个字符）。
    pub fn version(&self) -> String {
        self.last_version
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    async fn post_json(&self, body: String) -> anyhow::Result<crate::http::HttpResponse> {
        let mut req = HttpRequest::post_json(self.rpc_endpoint.clone(), body);
        // 上游由 cordelia `TrClient` 决定是否带凭据：用户名或密码任一非空都要带
        //（反代常见「空用户名 + 密码」配置）
        if !self.config.username.is_empty() || !self.config.password.is_empty() {
            req.basic = Some((self.config.username.clone(), self.config.password.clone()));
        }
        let session = self
            .session_id
            .lock()
            .map(|s| s.clone())
            .unwrap_or_default();
        if !session.is_empty() {
            req = req.with_header(SESSION_HEADER, &session);
        }
        let resp = self.http.execute(req).await?;
        Ok(resp)
    }

    /// 执行一次 RPC：409 时取会话 id 重试一次（对齐 cordelia 的 session 握手）。
    async fn rpc<T: serde::de::DeserializeOwned + Default>(
        &self,
        method: &str,
        arguments: serde_json::Value,
    ) -> anyhow::Result<(String, T)> {
        let request = serde_json::json!({ "method": method, "arguments": arguments }).to_string();
        let mut resp = self.post_json(request.clone()).await?;
        if resp.status == 409 {
            let sid = resp.header(SESSION_HEADER).unwrap_or_default().to_string();
            if sid.is_empty() {
                anyhow::bail!("传输 RPC 409 但未返回 {SESSION_HEADER}");
            }
            if let Ok(mut guard) = self.session_id.lock() {
                *guard = sid.clone();
            }
            resp = self.post_json(request).await?;
            if resp.status == 409 {
                anyhow::bail!("传输 RPC 会话握手失败（仍然 409）");
            }
        }
        if resp.status != 200 {
            anyhow::bail!("传输 RPC 失败：HTTP {}", resp.status);
        }
        let parsed: TrResponse<T> = serde_json::from_str(&resp.body)?;
        Ok((parsed.result, parsed.arguments.unwrap_or_default()))
    }

    async fn set_blocklist_url(&self, url: &str) -> anyhow::Result<bool> {
        let (result, _) = self
            .rpc::<SessionGet>(
                "session-set",
                serde_json::json!({ "blocklist-url": url, "blocklist-enabled": true }),
            )
            .await?;
        Ok(result == "success")
    }

    async fn update_blocklist(&self) -> anyhow::Result<bool> {
        let (result, _) = self
            .rpc::<BlocklistUpdate>("blocklist-update", serde_json::json!({}))
            .await?;
        if result != "success" {
            // 对齐上游：把 blocklist URL 指向一个明显的“失败”地址，便于用户在 WebUI 发现
            let _ = self
                .set_blocklist_url("http://peerbanhelper-blocklist-update-failed.com/check-peerbanhelper-webui-prefix-settings")
                .await;
            return Ok(false);
        }
        Ok(true)
    }

    /// `version.length() > 5` 时截断到 5 字符（上游行为）
    fn normalize_version(raw: &str) -> String {
        let trimmed = raw.trim();
        if trimmed.len() > 5 {
            trimmed[..5].to_string()
        } else {
            trimmed.to_string()
        }
    }

    fn torrent_from(&self, backend: &TrTorrent) -> TorrentData {
        TorrentData {
            hash: backend.hash_string.clone(),
            name: backend.name.clone(),
            progress: backend.percent_done,
            total_size: backend.total_size,
            // Transmission 无 piece_size/pieces_have；完成量按 sizeWhenDone * percentDone 计算
            piece_size: 0,
            pieces_have: 0,
            completed_override: Some(self.completed_size(backend)),
            dlspeed: backend.rate_download,
            upspeed: backend.rate_upload,
            is_private: Some(backend.is_private),
        }
    }

    /// `Torrent.getCompletedSize()` 对 Transmission 的实现（覆盖 qB 口径）：
    /// `(long) (sizeWhenDone * percentDone)`。
    fn completed_size(&self, backend: &TrTorrent) -> i64 {
        (backend.size_when_done as f64 * backend.percent_done) as i64
    }

    fn peers_of(&self, backend: &TrTorrent) -> Vec<PeerData> {
        backend
            .peers
            .iter()
            .map(|p| {
                let (ip, port) = pbh_core::remap::translate_peer_ip(
                    &p.address,
                    p.port,
                    &self.remap.ip_remapping,
                );
                PeerData {
                    client_name: p.client_name.clone(),
                    peer_id: Some(decode_peer_id(p.peer_id.as_deref())),
                    dl_speed: p.rate_to_client.unwrap_or(-1),
                    downloaded: p.bytes_to_client.unwrap_or(-1),
                    up_speed: p.rate_to_peer.unwrap_or(-1),
                    uploaded: p.bytes_to_peer.unwrap_or(-1),
                    progress: p.progress,
                    flags: p.flag_str.clone(),
                    ip: ip.clone(),
                    port,
                    // Transmission 没有独立的原始 ip:port 键，使用 address:port
                    raw_ip: format!("{}:{}", p.address, p.port),
                    connection: None,
                }
            })
            .collect()
    }
}

impl Downloader for TransmissionDownloader {
    fn id(&self) -> &str {
        &self.config.id
    }
    fn name(&self) -> &str {
        &self.config.name
    }
    fn downloader_type(&self) -> &'static str {
        "transmission"
    }

    /// 对齐 `Transmission.getFeatureFlags()`：无 `RANGE_BAN_IP`
    fn feature_flags(&self) -> Vec<String> {
        vec![
            DownloaderFeature::UnbanIp.name().to_string(),
            DownloaderFeature::TrafficStats.name().to_string(),
            DownloaderFeature::LiveUpdateBtProtocolPort
                .name()
                .to_string(),
        ]
    }

    fn login<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<LoginResult>> {
        Box::pin(async move {
            if self.config.paused {
                return Ok(LoginResult::paused("Transmission 已暂停", self.version()));
            }
            // 上游 `login0` 无 try/catch：传输异常外抛给 `AbstractDownloader.login()` 计数
            let (result, session) = self
                .rpc::<SessionGet>("session-get", serde_json::json!({}))
                .await?;
            if result != "success" {
                return Err(anyhow::anyhow!("session-get 返回 {result}"));
            }
            let version = Self::normalize_version(&session.version);
            if let Ok(mut guard) = self.last_version.lock() {
                *guard = version.clone();
            }
            // 要求 >= 4.1.0
            let parts: Vec<u32> = version
                .split('.')
                .filter_map(|s| {
                    s.chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse()
                        .ok()
                })
                .collect();
            let (major, minor) = (
                parts.first().copied().unwrap_or(0),
                parts.get(1).copied().unwrap_or(0),
            );
            if major < 4 || (major == 4 && minor < 1) {
                // 上游返回 EXCEPTION 状态（不进入冷却）
                return Ok(LoginResult::exception(format!(
                    "Transmission 版本不支持（需 >= 4.1.0）：{version}"
                )));
            }

            // blocklist 必须指向 PBH 自身端点
            let expected = self.config.blocklist_url.clone();
            if !session.blocklist_enabled || !session.blocklist_url.starts_with(&expected) {
                let url = format!("{expected}?t={}", chrono_like_now_ms());
                if !self.set_blocklist_url(&url).await? {
                    return Ok(LoginResult::exception(
                        "设置 Transmission blocklist URL 失败",
                    ));
                }
                if !self.update_blocklist().await? {
                    return Ok(LoginResult::require_take_actions(
                        "Transmission blocklist 更新失败",
                        version,
                    ));
                }
            }
            if let Ok(mut guard) = self.healthy.lock() {
                *guard = true;
            }
            Ok(LoginResult::success("OK", version))
        })
    }

    fn fetch_torrents<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Vec<TorrentData>>> {
        Box::pin(async move {
            let fields: Vec<&str> = RPC_FIELDS.to_vec();
            let (_, args) = self
                .rpc::<TorrentGet>("torrent-get", serde_json::json!({ "fields": fields }))
                .await?;
            let mut out = Vec::new();
            let mut cache = HashMap::new();
            for backend in &args.torrents {
                cache.insert(backend.hash_string.clone(), self.peers_of(backend));
                // 活跃过滤：下载/上传速率 > 0 或有连接
                if !(backend.rate_download > 0
                    || backend.rate_upload > 0
                    || backend.peers_connected > 0)
                {
                    continue;
                }
                if self.config.ignore_private && backend.is_private {
                    continue;
                }
                let torrent = self.torrent_from(backend);
                out.push(torrent);
            }
            if let Ok(mut guard) = self.peers_cache.lock() {
                *guard = cache;
            }
            Ok(out)
        })
    }

    fn fetch_peers<'a>(
        &'a self,
        torrent: &'a TorrentData,
    ) -> BoxFuture<'a, anyhow::Result<Vec<PeerData>>> {
        Box::pin(async move {
            let peers = self
                .peers_cache
                .lock()
                .ok()
                .and_then(|c| c.get(&torrent.hash).cloned())
                .unwrap_or_default();
            Ok(peers)
        })
    }

    /// Transmission 不支持按 peer 增量封禁（只支持整份 blocklist 更新）
    fn ban_peers<'a>(&'a self, _peers: &'a [BanEntry]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move { Ok(()) })
    }

    /// 对齐 `Transmission.setBanList`：忽略传入列表，直接触发 `blocklist-update`
    /// （列表内容由 PBH 自己的 `/blocklist/p2p-plain-format` 端点提供）。
    fn replace_banned_ips<'a>(&'a self, _ips: &'a [String]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            // 上游 `setBanList` 在 `blocklist-update` 失败时**只**记
            // `DOWNLOADER_TR_INCORRECT_SET_BANLIST_API_RESP` 错误日志 + 发布告警，
            // 不抛异常（下一轮继续重试）；抛错会让本轮下发被整体跳过。
            match self.update_blocklist().await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::error!(
                        "Transmission blocklist-update 返回失败（blocklistUrl={}）",
                        self.config.blocklist_url
                    );
                }
                Err(e) => {
                    tracing::error!("Transmission blocklist-update 失败: {e:#}");
                }
            }
            Ok(())
        })
    }

    fn statistics<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<DownloaderStatistics>> {
        Box::pin(async move {
            let (_, stats) = self
                .rpc::<SessionStats>("session-stats", serde_json::json!({}))
                .await?;
            Ok(DownloaderStatistics {
                all_time_upload: stats.cumulative_stats.uploaded_bytes,
                all_time_download: stats.cumulative_stats.downloaded_bytes,
            })
        })
    }

    /// 对齐 `Transmission.getSpeedLimiter()`：`session-get` 带
    /// `fields = [downloadLimit, downloadLimited, uploadLimit, uploadLimited]`，
    /// 响应的 KB/s 值 **×1024** 转为 bytes/s；对应该方向未启用限速时归零（= 不限制）。
    ///
    /// 失败语义：`result != success` 时上游记日志并返回 `null`（调用方跳过）-> 本实现
    /// 记同一文案日志后返回 `Err`；RPC/传输层异常上游直接抛出 -> 本实现透传 `Err`。
    fn get_speed_limiter<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<(i64, i64)>> {
        Box::pin(async move {
            let (result, session) = self
                .rpc::<SessionGet>(
                    "session-get",
                    serde_json::json!({ "fields": SESSION_GET_SPEED_FIELDS }),
                )
                .await?;
            if result != "success" {
                error!(
                    "{}",
                    tl(
                        MSG_FAILED_RETRIEVE_SPEED_LIMITER,
                        vec![
                            Param::Text(self.config.name.clone()),
                            Param::Text(result.clone())
                        ]
                    )
                );
                anyhow::bail!("session-get 返回 {result}");
            }
            // `args.getSpeedLimitDown() * 1024L` / `args.getSpeedLimitUp() * 1024L`
            let mut download_limit = session.speed_limit_down * SPEED_LIMIT_UNIT;
            let mut upload_limit = session.speed_limit_up * SPEED_LIMIT_UNIT;
            if !session.speed_limit_down_enabled {
                download_limit = 0;
            }
            if !session.speed_limit_up_enabled {
                upload_limit = 0;
            }
            Ok((upload_limit, download_limit))
        })
    }

    /// 对齐 `Transmission.setSpeedLimiter()`：`session-set` 首字节序固定为
    /// `alt-speed-enabled` / `alt-speed-time-enabled` / `speed-limit-*`（先 down 后 up），
    /// 值按 **整数除法** 从 bytes/s 换成 KB/s；`isUploadUnlimited()` / `isDownloadUnlimited()`
    /// （`<= 0`）时该方向写 `max(1024, 值)/1024`（= 1）并把 `*-enabled` 置 false。
    ///
    /// 失败语义：上游只记 `DOWNLOADER_FAILED_SET_SPEED_LIMITER` 日志，不抛错（RPC 层异常除外）。
    fn set_speed_limiter<'a>(
        &'a self,
        upload: i64,
        download: i64,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let upload_unlimited = upload <= 0;
            let download_unlimited = download <= 0;
            let arguments = serde_json::json!({
                SESSION_SET_ALT_SPEED_ENABLED: false,
                SESSION_SET_ALT_SPEED_TIME_ENABLED: false,
                SESSION_SET_SPEED_LIMIT_DOWN: kb_per_second(download, download_unlimited),
                SESSION_SET_SPEED_LIMIT_UP: kb_per_second(upload, upload_unlimited),
                SESSION_SET_SPEED_LIMIT_UP_ENABLED: !upload_unlimited,
                SESSION_SET_SPEED_LIMIT_DOWN_ENABLED: !download_unlimited,
            });
            let (result, _) = self.rpc::<SessionGet>("session-set", arguments).await?;
            if result != "success" {
                error!(
                    "{}",
                    tl(
                        MSG_FAILED_SET_SPEED_LIMITER,
                        vec![Param::Text(self.config.name.clone()), Param::Text(result)]
                    )
                );
            }
            Ok(())
        })
    }
}

/// 对齐 `Transmission.setSpeedLimiter` 的换算：不限速时 `max(1024, 值) / 1024`（恒为 1），
/// 否则 `值 / 1024`（Java 侧是 `(int)` 截断，此处保留 i64 以避免溢出）。
fn kb_per_second(bytes_per_second: i64, unlimited: bool) -> i64 {
    if unlimited {
        1024i64.max(bytes_per_second) / SPEED_LIMIT_UNIT
    } else {
        bytes_per_second / SPEED_LIMIT_UNIT
    }
}

/// 对齐上游 `tlUI` 的渲染入口（下载器不持有 locale，与服务端默认文案语言一致）。
fn tl(key: &str, params: Vec<Param>) -> String {
    static TRANSLATOR: OnceLock<Translator> = OnceLock::new();
    TRANSLATOR
        .get_or_init(Translator::embedded)
        .render(&TranslationComponent::with_params(key, params), UI_LOCALE)
}

/// 生成 cache-busting 时间戳（毫秒）。避免为此引入 chrono 依赖。
fn chrono_like_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::HttpResponse;
    use serde_json::{json, Value};

    /// `session-get` 夹具：上传 1024 KB/s（1 MiB/s）、下载 2048 KB/s（2 MiB/s），两者都已启用。
    fn session_get_body(down_enabled: bool, up_enabled: bool) -> String {
        json!({
            "result": "success",
            "arguments": {
                "speed-limit-down": 2048,
                "speed-limit-down-enabled": down_enabled,
                "speed-limit-up": 1024,
                "speed-limit-up-enabled": up_enabled,
                "peer-port": 51413,
            }
        })
        .to_string()
    }

    /// JSON-RPC 的内存实现：记录请求体并按方法返回夹具。
    struct TrMock {
        requests: Mutex<Vec<Value>>,
        /// `(method, 完整响应体, HTTP 状态码)` 的固定应答
        session_get: Mutex<(String, u16)>,
        session_set: Mutex<(String, u16)>,
    }

    impl TrMock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                session_get: Mutex::new((session_get_body(true, true), 200)),
                session_set: Mutex::new((
                    json!({ "result": "success", "arguments": {} }).to_string(),
                    200,
                )),
            })
        }

        fn request(&self, method: &str) -> Value {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .find(|r| r["method"] == method)
                .cloned()
                .unwrap_or_else(|| panic!("未收到 {method} 请求"))
        }

        /// 该方法的 `arguments`
        fn arguments(&self, method: &str) -> Value {
            self.request(method)["arguments"].clone()
        }
    }

    impl HttpFetcher for TrMock {
        fn execute<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, anyhow::Result<HttpResponse>> {
            Box::pin(async move {
                assert!(
                    req.url.ends_with("/transmission/rpc"),
                    "unexpected url {}",
                    req.url
                );
                let body: Value =
                    serde_json::from_str(req.body.as_deref().unwrap_or_default()).unwrap();
                let method = body["method"].as_str().unwrap_or_default().to_string();
                self.requests.lock().unwrap().push(body);
                let (response, status) = match method.as_str() {
                    "session-get" => self.session_get.lock().unwrap().clone(),
                    "session-set" => self.session_set.lock().unwrap().clone(),
                    other => panic!("未预期的 RPC 方法 {other}"),
                };
                Ok(HttpResponse::new(status, response))
            })
        }
    }

    fn mock_downloader(mock: Arc<TrMock>) -> Arc<TransmissionDownloader> {
        let config = TRConfig {
            id: "tr-test".into(),
            name: "Transmission Test".into(),
            endpoint: "http://mock.local".into(),
            ..TRConfig::default()
        };
        Arc::new(
            TransmissionDownloader::with_fetcher(config, RemapConfig::default(), mock).unwrap(),
        )
    }

    /// 读取：`session-get` 的 KB/s ×1024 -> bytes/s；未启用该方向时归零。
    #[tokio::test]
    async fn speed_limiter_is_read_in_kib_and_zeroed_when_disabled() {
        let mock = TrMock::new();
        let dl = mock_downloader(mock.clone());
        let (upload, download) = dl.get_speed_limiter().await.unwrap();
        assert_eq!(upload, 1_048_576, "1024 KB/s -> 1 MiB/s");
        assert_eq!(download, 2_097_152, "2048 KB/s -> 2 MiB/s");
        // `fields` 用的是 cordelia `Fields` 的 camelCase 常量值
        assert_eq!(
            mock.arguments("session-get"),
            json!({ "fields": ["downloadLimit", "downloadLimited", "uploadLimit", "uploadLimited"] })
        );

        let mock = TrMock::new();
        *mock.session_get.lock().unwrap() = (session_get_body(false, false), 200);
        let dl = mock_downloader(mock);
        let (upload, download) = dl.get_speed_limiter().await.unwrap();
        assert_eq!((upload, download), (0, 0), "未启用限速 -> 0（不限制）");
    }

    /// 下发 1 MiB/s 上传 + 2 MiB/s 下载：bytes/s ÷ 1024 -> KB/s，并打开两个 enabled 开关。
    #[tokio::test]
    async fn speed_limiter_is_written_in_kib_per_second() {
        let mock = TrMock::new();
        let dl = mock_downloader(mock.clone());
        dl.set_speed_limiter(1_048_576, 2_097_152).await.unwrap();
        assert_eq!(
            mock.arguments("session-set"),
            json!({
                "alt-speed-enabled": false,
                "alt-speed-time-enabled": false,
                "speed-limit-down": 2048,
                "speed-limit-up": 1024,
                "speed-limit-up-enabled": true,
                "speed-limit-down-enabled": true,
            })
        );
    }

    /// 不限制：该方向写 `max(1024, 值)/1024 = 1` KB/s 且 `*-enabled = false`（对齐上游）。
    #[tokio::test]
    async fn unlimited_speed_limiter_disables_the_limits() {
        let mock = TrMock::new();
        let dl = mock_downloader(mock.clone());
        dl.set_speed_limiter(0, -1).await.unwrap();
        assert_eq!(
            mock.arguments("session-set"),
            json!({
                "alt-speed-enabled": false,
                "alt-speed-time-enabled": false,
                "speed-limit-down": 1,
                "speed-limit-up": 1,
                "speed-limit-up-enabled": false,
                "speed-limit-down-enabled": false,
            })
        );
    }

    /// 非整 KiB 的样本：整数除法截断（例如 1 MiB + 1 字节 -> 1024 KB/s）。
    #[tokio::test]
    async fn speed_limiter_truncates_partial_kib() {
        let mock = TrMock::new();
        let dl = mock_downloader(mock.clone());
        dl.set_speed_limiter(1_048_577, 1536).await.unwrap();
        let args = mock.arguments("session-set");
        assert_eq!(args["speed-limit-up"], json!(1024));
        assert_eq!(args["speed-limit-down"], json!(1));
    }

    /// 失败路径：`result != success` 时读取报错（上游返回 null），下发只记日志。
    #[tokio::test]
    async fn speed_limiter_failure_semantics_follow_upstream() {
        let mock = TrMock::new();
        *mock.session_get.lock().unwrap() = (
            json!({ "result": "invalid session id", "arguments": {} }).to_string(),
            200,
        );
        *mock.session_set.lock().unwrap() = (
            json!({ "result": "no such field", "arguments": {} }).to_string(),
            200,
        );
        let dl = mock_downloader(mock);
        assert!(
            dl.get_speed_limiter().await.is_err(),
            "session-get 失败 -> Err"
        );
        // 上游 setSpeedLimiter 只记日志，不抛错
        assert!(dl.set_speed_limiter(1_048_576, 2_097_152).await.is_ok());

        // RPC/HTTP 层失败：上游让异常透传（读取与下发都是）
        let mock = TrMock::new();
        *mock.session_get.lock().unwrap() = (String::new(), 500);
        *mock.session_set.lock().unwrap() = (String::new(), 500);
        let dl = mock_downloader(mock);
        assert!(dl.get_speed_limiter().await.is_err());
        assert!(dl.set_speed_limiter(1_048_576, 2_097_152).await.is_err());
    }
}
