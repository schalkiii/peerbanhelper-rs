//! qBittorrent 下载器适配器，忠实复刻 AbstractQbittorrent（SPEC 第 3 节 [GOLDEN]）。

pub mod dto;

use crate::http::{BoxFuture, HttpFetcher, HttpRequest, HttpResponse, ReqwestFetcher};
use crate::{BanEntry, Downloader, DownloaderFeature, DownloaderStatistics, LoginResult};
use dto::*;
use pbh_core::defaults::qb as qbcfg;
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::remap::{remap_ban_list_address, translate_peer_ip, RemapConfig};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

    #[test]
    fn version_compare() {
        assert!(version_at_least("4.5.0", 4, 5, 0));
        assert!(version_at_least("5.2.1", 4, 5, 0));
        assert!(!version_at_least("4.4.9", 4, 5, 0));
        assert!(version_at_least("5.0.0-beta1", 4, 5, 0));
    }
}
