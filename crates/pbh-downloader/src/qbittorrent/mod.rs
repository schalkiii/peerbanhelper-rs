//! qBittorrent 下载器适配器，忠实复刻 AbstractQbittorrent（SPEC 第 3 节 [GOLDEN]）。

pub mod dto;

use crate::http::{BoxFuture, HttpFetcher, HttpRequest, HttpResponse, ReqwestFetcher};
use crate::{BanEntry, Downloader, DownloaderFeature, DownloaderStatistics, LoginResult};
use dto::*;
use pbh_core::defaults::qb as qbcfg;
use pbh_core::i18n::{Param, TranslationComponent, Translator};
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::remap::{remap_ban_list_address, translate_peer_ip, RemapConfig};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tracing::error;

/// 上游 `Lang` 文案键（`DOWNLOADER_QB_FAILED_SAVE_SPEED_LIMITER` 的 5 个占位参数：
/// 下载器名 / apiEndpoint / 状态码 / `"HTTP ERROR"` / 响应体）。
const MSG_QB_FAILED_SAVE_SPEED_LIMITER: &str = "DOWNLOADER_QB_FAILED_SAVE_SPEED_LIMITER";

/// 对齐上游 `tlUI`：下载器不持有 locale，使用服务端默认文案语言。
const UI_LOCALE: &str = "zh_cn";

#[derive(Clone, Debug)]
pub struct QBConfig {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    pub username: String,
    pub password: String,
    pub api_key: Option<String>,
    pub basic_auth: Option<(String, String)>,
    pub verify_tls: bool,
    pub ignore_private: bool,
    pub increment_ban: bool,
    pub disable_same_ip_multi_connection: bool,
    pub max_concurrent_slots: usize,
    /// `config.yml` 的 `banlist-remapping` / `ip-remapping` 配置
    pub remap: RemapConfig,
}

impl Default for QBConfig {
    fn default() -> Self {
        Self {
            id: "qbittorrent".into(),
            name: "qBittorrent".into(),
            endpoint: "http://127.0.0.1:8080".into(),
            username: "admin".into(),
            password: String::new(),
            api_key: None,
            basic_auth: None,
            verify_tls: true,
            ignore_private: true,
            increment_ban: true,
            disable_same_ip_multi_connection: true,
            max_concurrent_slots: qbcfg::MAX_CONCURRENT_SLOTS,
            remap: RemapConfig::default(),
        }
    }
}

struct PropsCacheEntry {
    props: TorrentProperties,
    at: Instant,
}

pub struct QBittorrentDownloader {
    config: QBConfig,
    api_base: String,
    http: Arc<dyn HttpFetcher>,
    healthy: AtomicBool,
    props_cache: Mutex<HashMap<String, PropsCacheEntry>>,
    props_ttl: Duration,
    /// 协商得到的下载器版本（去掉前导 `v`），决定能力标志（对齐上游 `lastSemver`）
    last_version: Mutex<String>,
}

impl QBittorrentDownloader {
    pub fn new(config: QBConfig) -> anyhow::Result<Self> {
        let fetcher = Arc::new(ReqwestFetcher::new(
            config.verify_tls,
            qbcfg::CONNECT_TIMEOUT_SECS,
            qbcfg::READ_TIMEOUT_SECS,
        )?);
        Self::with_fetcher(config, fetcher, Duration::from_secs(65))
    }

    pub fn with_fetcher(
        config: QBConfig,
        fetcher: Arc<dyn HttpFetcher>,
        props_ttl: Duration,
    ) -> anyhow::Result<Self> {
        let api_base = format!("{}/api/v2", config.endpoint.trim_end_matches('/'));
        Ok(Self {
            config,
            api_base,
            http: fetcher,
            healthy: AtomicBool::new(false),
            props_cache: Mutex::new(HashMap::new()),
            props_ttl,
            last_version: Mutex::new(String::new()),
        })
    }

    /// 下载器是否声明 `RANGE_BAN_IP` 能力。
    ///
    /// 对齐上游 `AbstractQbittorrent.getFeatureFlags()`：
    /// `lastSemver >= 5.3.0 || lastSemver >= 5.3.0-alpha1 || lastSemver == 5.2.0-beta1`，
    /// 且外部开关 `pbh.downloader.qBittorrent.enableRangeBanIp`（默认 true）打开。
    /// 本实现不提供外部开关机制，视为始终为 true。
    pub fn supports_range_ban(&self) -> bool {
        let v = self.last_version.lock().map(|v| v.clone()).unwrap_or_default();
        if v.trim().is_empty() {
            return false;
        }
        version_at_least(&v, 5, 3, 0) || v.trim().eq_ignore_ascii_case("5.2.0-beta1")
    }

    /// 附加认证：Bearer（API Key）不用于 auth/login、auth/logout。
    ///
    /// Basic 凭据**不预置**，只在收到 401 后由 [`Self::send`] 重试时附加
    /// （对齐上游 OkHttp `Authenticator`：`responseCount <= 1` 才重试一次）。
    fn auth(&self, mut req: HttpRequest) -> HttpRequest {
        if let Some(key) = &self.config.api_key {
            if !req.url.contains("/auth/login") && !req.url.contains("/auth/logout") {
                req.bearer = Some(key.clone());
            }
        }
        req
    }

    async fn send(&self, req: HttpRequest) -> anyhow::Result<HttpResponse> {
        let has_basic = self.config.basic_auth.is_some();
        if !has_basic {
            return self.http.execute(self.auth(req)).await;
        }
        let resp = self.http.execute(self.auth(req.clone())).await?;
        if resp.status == 401 {
            if let Some((u, p)) = &self.config.basic_auth {
                let mut retry = req;
                retry.basic = Some((u.clone(), p.clone()));
                return self.http.execute(retry).await;
            }
        }
        Ok(resp)
    }

    async fn get(&self, path_and_query: &str) -> anyhow::Result<HttpResponse> {
        self.send(HttpRequest::get(format!("{}{}", self.api_base, path_and_query))).await
    }

    async fn post_form(&self, path: &str, form: Vec<(String, String)>) -> anyhow::Result<HttpResponse> {
        self.send(HttpRequest::post_form(format!("{}{}", self.api_base, path), form)).await
    }

    fn set_preferences(&self, json: serde_json::Value) -> Vec<(String, String)> {
        vec![("json".to_string(), json.to_string())]
    }

    /// 会话校验：`GET /app/buildInfo` 返回 200 且 `libtorrent` 非空（对齐 Java `isLoggedIn`）。
    async fn session_valid(&self) -> anyhow::Result<bool> {
        let build = self.get("/app/buildInfo").await?;
        if build.status != 200 {
            return Ok(false);
        }
        let info: BuildInfo = serde_json::from_str(&build.body).unwrap_or_default();
        Ok(!info.libtorrent.as_deref().unwrap_or("").is_empty())
    }

    /// 登录成功后的收尾：版本校验 + 首次健康时关闭「同 IP 多连接」。
    async fn finish_login(&self) -> anyhow::Result<LoginResult> {
        let version_resp = self.get("/app/version").await?;
        let version = version_resp.body.trim().trim_start_matches('v').to_string();
        if let Ok(mut last) = self.last_version.lock() {
            *last = version.clone();
        }
        // API Key 认证要求 qB >= 5.2.0；口令认证要求 >= 4.5.0
        let (min_major, min_minor, min_patch) = if self
            .config
            .api_key
            .as_deref()
            .is_some_and(|k| !k.trim().is_empty())
        {
            (5, 2, 0)
        } else {
            (4, 5, 0)
        };
        if !version_at_least(&version, min_major, min_minor, min_patch) {
            return Ok(LoginResult {
                success: false,
                message: format!(
                    "unsupported qBittorrent version: {version} (require >= {min_major}.{min_minor}.{min_patch})"
                ),
                version,
            });
        }

        let first_healthy = !self.healthy.swap(true, Ordering::SeqCst);
        if first_healthy && self.config.disable_same_ip_multi_connection {
            let form =
                self.set_preferences(serde_json::json!({ "enable_multi_connections_from_same_ip": false }));
            self.post_form("/app/setPreferences", form).await?;
        }

        Ok(LoginResult { success: true, message: "OK".into(), version })
    }

    async fn fetch_properties(&self, hash: &str) -> anyhow::Result<TorrentProperties> {
        if let Some(entry) = self.props_cache.lock().unwrap().get(hash) {
            if entry.at.elapsed() < self.props_ttl {
                return Ok(entry.props.clone());
            }
        }
        let resp = self
            .get(&format!("/torrents/properties?hash={}", urlencoding(hash)))
            .await?;
        let props: TorrentProperties = serde_json::from_str(&resp.body).unwrap_or_default();
        self.props_cache.lock().unwrap().insert(
            hash.to_string(),
            PropsCacheEntry { props: props.clone(), at: Instant::now() },
        );
        Ok(props)
    }
}

impl Downloader for QBittorrentDownloader {
    fn id(&self) -> &str {
        &self.config.id
    }
    fn name(&self) -> &str {
        &self.config.name
    }
    fn downloader_type(&self) -> &'static str {
        "qbittorrent"
    }
    /// 对齐 `AbstractQbittorrent.getFeatureFlags()`：
    /// `UNBAN_IP` / `TRAFFIC_STATS` / `LIVE_UPDATE_BT_PROTOCOL_PORT` 恒定声明，
    /// `RANGE_BAN_IP` 仅在 qB >= 5.3.0（或 5.2.0-beta1）时声明。
    fn feature_flags(&self) -> Vec<String> {
        let mut flags = vec![
            DownloaderFeature::UnbanIp.name().to_string(),
            DownloaderFeature::TrafficStats.name().to_string(),
            DownloaderFeature::LiveUpdateBtProtocolPort.name().to_string(),
        ];
        if self.supports_range_ban() {
            flags.push(DownloaderFeature::RangeBanIp.name().to_string());
        }
        flags
    }

    fn login<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<LoginResult>> {
        Box::pin(async move {
            // 1. 复用已有会话（对齐 Java `login0` 开头的 `if (isLoggedIn()) return SUCCESS`）。
            //    API Key 认证同样走此路径（无状态，无需 /auth/login）。
            if self.session_valid().await? {
                return self.finish_login().await;
            }
            // API Key 无效时不再尝试表单登录（上游直接返回凭据错误）
            if self.config.api_key.as_deref().is_some_and(|k| !k.trim().is_empty()) {
                return Ok(LoginResult {
                    success: false,
                    message: "API Key authentication failed".into(),
                    version: String::new(),
                });
            }

            // 2. 表单登录
            let resp = self
                .send(HttpRequest::post_form(
                    format!("{}/auth/login", self.api_base),
                    vec![
                        ("username".to_string(), self.config.username.clone()),
                        ("password".to_string(), self.config.password.clone()),
                    ],
                ))
                .await?;
            let body = resp.body.trim();
            if !body.eq_ignore_ascii_case("Ok.") {
                return Ok(LoginResult {
                    success: false,
                    message: format!("login failed: {body}"),
                    version: String::new(),
                });
            }

            // 3. 会话校验：buildInfo 的 libtorrent 非空
            if !self.session_valid().await? {
                return Ok(LoginResult {
                    success: false,
                    message: "session invalid: empty libtorrent".into(),
                    version: String::new(),
                });
            }
            self.finish_login().await
        })
    }

    fn fetch_torrents<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Vec<TorrentData>>> {
        Box::pin(async move {
            let mut by_hash: HashMap<String, QBittorrentTorrent> = HashMap::new();
            let page = qbcfg::PAGE_SIZE;
            let mut offset = 0u32;
            loop {
                let resp = self
                    .get(&format!("/torrents/info?filter=active&limit={page}&offset={offset}"))
                    .await?;
                let batch: Vec<QBittorrentTorrent> =
                    serde_json::from_str(&resp.body).unwrap_or_default();
                let batch_len = batch.len();
                let mut added = 0usize;
                for t in batch {
                    if by_hash.insert(t.hash.clone(), t).is_none() {
                        added += 1;
                    }
                }
                if added == 0 || (batch_len as u32) < page {
                    break;
                }
                offset += page;
            }

            let mut out = Vec::with_capacity(by_hash.len());
            for t in by_hash.into_values() {
                let mut is_private = t.is_private;
                let mut piece_size = t.piece_size;
                let mut pieces_have = t.pieces_have;
                // 对齐 Java `fillTorrentProperties`：
                //   - ignorePrivate 且字段缺失时，用 properties API 补全 is_private
                //   - piece_size / pieces_have **任一 <= 0** 时，两者都用 properties API 补全
                let need_private = self.config.ignore_private && is_private.is_none();
                let need_pieces = piece_size <= 0 || pieces_have <= 0;
                if need_private || need_pieces {
                    if let Ok(props) = self.fetch_properties(&t.hash).await {
                        if need_private {
                            is_private = props.is_private;
                        }
                        if need_pieces {
                            piece_size = props.piece_size;
                            pieces_have = props.pieces_have;
                        }
                    }
                }
                let torrent = TorrentData {
                    hash: t.hash,
                    name: t.name,
                    progress: t.progress,
                    total_size: t.total_size,
                    piece_size,
                    pieces_have,
                    completed_override: None,
                    dlspeed: t.dlspeed,
                    upspeed: t.upspeed,
                    is_private,
                };
                if self.config.ignore_private && torrent.is_private() {
                    continue;
                }
                out.push(torrent);
            }
            Ok(out)
        })
    }

    fn fetch_peers<'a>(
        &'a self,
        torrent: &'a TorrentData,
    ) -> BoxFuture<'a, anyhow::Result<Vec<PeerData>>> {
        Box::pin(async move {
            let resp = self
                .get(&format!("/sync/torrentPeers?hash={}", urlencoding(&torrent.hash)))
                .await?;
            let parsed: TorrentPeersResponse =
                serde_json::from_str(&resp.body).unwrap_or_default();
            let mut out = Vec::new();
            for (raw_ip, p) in parsed.peers {
                if let Some(conn) = &p.connection {
                    let c = conn.to_ascii_lowercase();
                    if c == "http" || c == "https" || c == "web" {
                        continue;
                    }
                }
                let ip = match p.ip.as_deref().filter(|s| !s.is_empty()) {
                    Some(ip) => ip.to_string(),
                    None => continue,
                };
                let key_lower = raw_ip.to_ascii_lowercase();
                if key_lower.contains(".onion") || key_lower.contains(".i2p") {
                    continue;
                }
                let port = p.port.unwrap_or(0).clamp(0, u16::MAX as i64) as u16;
                // 对齐 `AbstractQbittorrent.getPeers` 的 `addressTranslate`：
                // Teredo（配置开关）→ NAT64 → IPv4-mapped 归一；
                // `raw_ip` 保留下载器原始 ip:port，供增量封禁使用。
                let (ip, port) = translate_peer_ip(&ip, port, &self.config.remap.ip_remapping);
                out.push(PeerData {
                    client_name: p.client,
                    peer_id: p.peer_id_client,
                    dl_speed: p.dl_speed,
                    downloaded: p.downloaded,
                    up_speed: p.up_speed,
                    uploaded: p.uploaded,
                    progress: p.progress,
                    flags: p.flags,
                    ip,
                    port,
                    raw_ip: raw_ip.clone(),
                    connection: p.connection,
                });
            }
            Ok(out)
        })
    }

    fn ban_peers<'a>(&'a self, peers: &'a [BanEntry]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            if peers.is_empty() {
                return Ok(());
            }
            let payload: Vec<String> = peers.iter().map(|p| p.raw_ip.clone()).collect();
            let payload = dedupe_join(&payload, "|");
            self.post_form("/transfer/banPeers", vec![("peers".to_string(), payload)])
                .await?;
            Ok(())
        })
    }

    /// 全量下发封禁列表，对齐 `AbstractQbittorrent.setBanListFull`：
    /// 按 `supportRangeBan` 做 `remapBanListAddress`，去重后用 `\n` 连接。
    fn replace_banned_ips<'a>(&'a self, ips: &'a [String]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let support_range_ban = self.supports_range_ban();
            let mut seen = HashSet::new();
            let mut joined: Vec<String> = Vec::new();
            for ip in ips {
                for mapped in remap_ban_list_address(ip, support_range_ban, &self.config.remap) {
                    if seen.insert(mapped.clone()) {
                        joined.push(mapped);
                    }
                }
            }
            joined.sort();
            let form =
                self.set_preferences(serde_json::json!({ "banned_IPs": joined.join("\n") }));
            self.post_form("/app/setPreferences", form).await?;
            Ok(())
        })
    }

    fn statistics<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<DownloaderStatistics>> {
        Box::pin(async move {
            let resp = self.get("/sync/maindata").await?;
            if resp.status != 200 {
                anyhow::bail!("statistics request failed: statusCode={}", resp.status);
            }
            let data: MainData = serde_json::from_str(&resp.body).unwrap_or_default();
            let s = data.server_state.unwrap_or_default();
            // 对齐 Java `getStatistics`：alltime_ul/dl 同时为 0 视为下载器尚未就绪
            if s.alltime_ul == 0 && s.alltime_dl == 0 {
                anyhow::bail!("statistics not ready (alltimeUl=0, alltimeDl=0)");
            }
            Ok(DownloaderStatistics {
                all_time_upload: s.alltime_ul,
                all_time_download: s.alltime_dl,
            })
        })
    }

    /// 对齐 `AbstractQbittorrent.getSpeedLimiter()`：`GET /app/preferences`，
    /// 单位为 **bytes/s**（qB 原生单位，`0` = 不限制），无需换算。
    ///
    /// 失败语义：上游任何异常（含非 2xx 与 `up_limit`/`dl_limit` 缺失导致的 NPE）
    /// 都被包成 `IllegalStateException` 抛出 -> 本实现返回 `Err`。
    fn get_speed_limiter<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<(i64, i64)>> {
        Box::pin(async move {
            let resp = self.get("/app/preferences").await?;
            if resp.status != 200 {
                anyhow::bail!("Request failed with code: {}", resp.status);
            }
            let preferences: QBittorrentPreferences =
                serde_json::from_str(&resp.body).unwrap_or_default();
            // `preferences.getDlLimit()` / `getUpLimit()`（装箱 `Long`，缺失即 NPE）
            let download_limit = preferences
                .dl_limit
                .ok_or_else(|| anyhow::anyhow!("NullPointerException: dl_limit is null"))?;
            let upload_limit = preferences
                .up_limit
                .ok_or_else(|| anyhow::anyhow!("NullPointerException: up_limit is null"))?;
            Ok((upload_limit, download_limit))
        })
    }

    /// 对齐 `AbstractQbittorrent.setSpeedLimiter()`：`POST /app/setPreferences`，表单字段
    /// `json` 内含 `up_limit` / `dl_limit` / `alt_up_limit` / `alt_dl_limit` 以及
    /// `limit_utp_rate` / `limit_lan_peers` / `scheduler_enabled` 三个固定开关。
    ///
    /// 单位为 **bytes/s**（qB 原生单位）；`isUploadUnlimited()` / `isDownloadUnlimited()`
    /// 即 `<= 0` 时下发 `0`（qB 的「不限制」）。失败语义同上游：记
    /// `DOWNLOADER_QB_FAILED_SAVE_SPEED_LIMITER` 日志后抛异常 -> 本实现返回 `Err`。
    fn set_speed_limiter<'a>(
        &'a self,
        upload: i64,
        download: i64,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let upload_limit = if upload <= 0 { 0 } else { upload };
            let download_limit = if download <= 0 { 0 } else { download };
            let request_param = serde_json::json!({
                "up_limit": upload_limit,
                "dl_limit": download_limit,
                "alt_up_limit": upload_limit,
                "alt_dl_limit": download_limit,
                "limit_utp_rate": true,
                "limit_lan_peers": true,
                "scheduler_enabled": false,
            });
            let form = self.set_preferences(request_param);
            let resp = match self.post_form("/app/setPreferences", form).await {
                Ok(resp) => resp,
                Err(e) => {
                    error!(
                        "{}",
                        tl(
                            MSG_QB_FAILED_SAVE_SPEED_LIMITER,
                            vec![
                                Param::Text(self.config.name.clone()),
                                Param::Text(self.api_base.clone()),
                                Param::Text("N/A".to_string()),
                                Param::Text(exception_params(&e).0),
                                Param::Text(exception_params(&e).1),
                            ]
                        )
                    );
                    return Err(e);
                }
            };
            if resp.status != 200 {
                error!(
                    "{}",
                    tl(
                        MSG_QB_FAILED_SAVE_SPEED_LIMITER,
                        vec![
                            Param::Text(self.config.name.clone()),
                            Param::Text(self.api_base.clone()),
                            Param::Text(resp.status.to_string()),
                            Param::Text("HTTP ERROR".to_string()),
                            Param::Text(resp.body.clone()),
                        ]
                    )
                );
                // 上游此处文案是 `"Save qBittorrent shadow banlist error: statusCode="`（原文照抄）
                anyhow::bail!("Save qBittorrent shadow banlist error: statusCode={}", resp.status);
            }
            Ok(())
        })
    }
}

/// 对齐上游 `e.getClass().getName()` 与 `e.getMessage()` 两个占位参数。
///
/// Rust 无法在运行时取得动态类型名（`dyn Error` 的 `type_name` 只会给出 trait 名），
/// 故类名位置固定填 `anyhow::Error`，消息位置取错误链根因的 `Display`。
fn exception_params(e: &anyhow::Error) -> (String, String) {
    ("anyhow::Error".to_string(), e.root_cause().to_string())
}

/// 对齐上游 `tlUI` 的渲染入口（下载器不持有 locale，与服务端默认文案语言一致）。
fn tl(key: &str, params: Vec<Param>) -> String {
    static TRANSLATOR: OnceLock<Translator> = OnceLock::new();
    TRANSLATOR
        .get_or_init(Translator::embedded)
        .render(&TranslationComponent::with_params(key, params), UI_LOCALE)
}

fn dedupe_join(items: &[String], sep: &str) -> String {
    let mut seen = HashSet::new();
    items
        .iter()
        .filter(|s| seen.insert((*s).clone()))
        .cloned()
        .collect::<Vec<_>>()
        .join(sep)
}

/// 极简 URL 编码（hash 为十六进制，通常无需编码；保留对特殊字符的处理）。
fn urlencoding(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') {
                c.to_string()
            } else {
                let mut buf = [0u8; 4];
                c.encode_utf8(&mut buf).bytes().map(|b| format!("%{b:02X}")).collect()
            }
        })
        .collect()
}

/// LOOSE 版本比较：解析前三个数字分量，非数字后缀忽略。
pub fn version_at_least(v: &str, major: u32, minor: u32, patch: u32) -> bool {
    let parts: Vec<u32> = v
        .split(['.', '-', '_', '+'])
        .filter_map(|s| {
            s.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .ok()
        })
        .collect();
    let get = |i: usize| parts.get(i).copied().unwrap_or(0);
    (get(0), get(1), get(2)) >= (major, minor, patch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn version_compare() {
        assert!(version_at_least("4.5.0", 4, 5, 0));
        assert!(version_at_least("5.2.1", 4, 5, 0));
        assert!(!version_at_least("4.4.9", 4, 5, 0));
        assert!(version_at_least("5.0.0-beta1", 4, 5, 0));
    }

    /// 被记录到的请求。
    #[derive(Clone, Debug)]
    struct Recorded {
        method: String,
        url: String,
        form: Option<Vec<(String, String)>>,
    }

    impl Recorded {
        /// 表单字段 `json` 解析后的对象（`setPreferences` 的载荷）。
        fn json_form(&self) -> Value {
            let raw = self
                .form
                .as_ref()
                .and_then(|form| form.iter().find(|(k, _)| k == "json"))
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            serde_json::from_str(&raw).unwrap_or(Value::Null)
        }
    }

    /// Web API 的内存实现：`/app/preferences` 返回夹具，`/app/setPreferences` 按状态码返回。
    struct QbMock {
        requests: Mutex<Vec<Recorded>>,
        preferences: Mutex<(u16, String)>,
        set_preferences_status: Mutex<u16>,
    }

    impl QbMock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                // `up_limit` / `dl_limit` 单位就是 bytes/s
                preferences: Mutex::new((
                    200,
                    json!({ "up_limit": 1_048_576, "dl_limit": 2_097_152 }).to_string(),
                )),
                set_preferences_status: Mutex::new(200),
            })
        }

        fn requests(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }

        fn request(&self, suffix: &str) -> Recorded {
            self.requests()
                .into_iter()
                .find(|r| r.url.ends_with(suffix))
                .unwrap_or_else(|| panic!("未收到以 {suffix} 结尾的请求"))
        }
    }

    impl HttpFetcher for QbMock {
        fn execute<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, anyhow::Result<HttpResponse>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(Recorded {
                    method: req.method.clone(),
                    url: req.url.clone(),
                    form: req.form.clone(),
                });
                if req.url.ends_with("/app/preferences") {
                    let (status, body) = self.preferences.lock().unwrap().clone();
                    return Ok(HttpResponse::new(status, body));
                }
                if req.url.ends_with("/app/setPreferences") {
                    let status = *self.set_preferences_status.lock().unwrap();
                    return Ok(HttpResponse::new(status, String::new()));
                }
                Ok(HttpResponse::new(404, String::new()))
            })
        }
    }

    fn mock_downloader(mock: Arc<QbMock>) -> Arc<QBittorrentDownloader> {
        let config = QBConfig {
            id: "qb-test".into(),
            name: "qBittorrent Test".into(),
            endpoint: "http://mock.local".into(),
            ..QBConfig::default()
        };
        Arc::new(
            QBittorrentDownloader::with_fetcher(
                config,
                mock as Arc<dyn HttpFetcher>,
                Duration::from_secs(65),
            )
            .unwrap(),
        )
    }

    /// 读取：`GET /api/v2/app/preferences`，单位 bytes/s（无换算）。
    #[tokio::test]
    async fn speed_limiter_is_read_from_preferences_in_bytes_per_second() {
        let mock = QbMock::new();
        let dl = mock_downloader(mock.clone());
        let (upload, download) = dl.get_speed_limiter().await.unwrap();
        assert_eq!(upload, 1_048_576, "up_limit 原样返回（bytes/s）");
        assert_eq!(download, 2_097_152, "dl_limit 原样返回（bytes/s）");

        let req = mock.request("/app/preferences");
        assert_eq!(req.method, "GET");
        assert_eq!(req.url, "http://mock.local/api/v2/app/preferences");
        assert!(req.form.is_none());
    }

    /// 下发 1 MiB/s：bytes 原样进入 `up_limit` / `dl_limit`（含 alt 与三个固定开关）。
    #[tokio::test]
    async fn speed_limiter_is_written_as_bytes_per_second() {
        let mock = QbMock::new();
        let dl = mock_downloader(mock.clone());
        dl.set_speed_limiter(1_048_576, 2_097_152).await.unwrap();

        let req = mock.request("/app/setPreferences");
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "http://mock.local/api/v2/app/setPreferences");
        assert_eq!(
            req.json_form(),
            json!({
                "up_limit": 1_048_576,
                "dl_limit": 2_097_152,
                "alt_up_limit": 1_048_576,
                "alt_dl_limit": 2_097_152,
                "limit_utp_rate": true,
                "limit_lan_peers": true,
                "scheduler_enabled": false,
            })
        );
        // 表单只有一个 `json` 字段（对齐 `FormBody.Builder().add("json", …)`）
        assert_eq!(req.form.as_ref().map(|f| f.len()), Some(1));
    }

    /// `<= 0` 即 `isUploadUnlimited()` / `isDownloadUnlimited()` -> 下发 0（qB 的「不限制」）。
    #[tokio::test]
    async fn unlimited_speed_limiter_is_written_as_zero() {
        let mock = QbMock::new();
        let dl = mock_downloader(mock.clone());
        dl.set_speed_limiter(0, -1).await.unwrap();
        assert_eq!(
            mock.request("/app/setPreferences").json_form(),
            json!({
                "up_limit": 0,
                "dl_limit": 0,
                "alt_up_limit": 0,
                "alt_dl_limit": 0,
                "limit_utp_rate": true,
                "limit_lan_peers": true,
                "scheduler_enabled": false,
            })
        );
    }

    /// 失败路径：读取非 2xx（上游 `IllegalStateException`）与下发非 2xx（上游记日志后抛异常）。
    #[tokio::test]
    async fn speed_limiter_failures_follow_upstream() {
        let mock = QbMock::new();
        *mock.preferences.lock().unwrap() = (500, String::new());
        let dl = mock_downloader(mock.clone());
        let err = dl.get_speed_limiter().await.unwrap_err();
        assert!(err.to_string().contains("Request failed with code: 500"), "{err}");

        // `up_limit` / `dl_limit` 缺失等价上游装箱 `Long` 的 NPE（被包成异常）
        *mock.preferences.lock().unwrap() = (200, "{}".to_string());
        let err = dl.get_speed_limiter().await.unwrap_err();
        assert!(err.to_string().contains("dl_limit"), "{err}");

        let mock = QbMock::new();
        *mock.set_preferences_status.lock().unwrap() = 500;
        let dl = mock_downloader(mock);
        let err = dl.set_speed_limiter(1_048_576, 1_048_576).await.unwrap_err();
        assert!(
            err.to_string().contains("Save qBittorrent shadow banlist error: statusCode=500"),
            "{err}"
        );
    }
}
