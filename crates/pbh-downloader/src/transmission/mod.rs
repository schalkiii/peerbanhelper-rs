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
use crate::{
    BanEntry, Downloader, DownloaderFeature, DownloaderStatistics, LoginResult,
};
use dto::*;
use pbh_core::defaults::qb as qbcfg;
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::remap::RemapConfig;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const SESSION_HEADER: &str = "X-Transmission-Session-Id";
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
        self.last_version.lock().map(|v| v.clone()).unwrap_or_default()
    }

    async fn post_json(&self, body: String) -> anyhow::Result<crate::http::HttpResponse> {
        let mut req = HttpRequest::post_json(self.rpc_endpoint.clone(), body);
        if let (Some(user), password) = (Some(&self.config.username), &self.config.password) {
            if !user.is_empty() {
                req.basic = Some((user.clone(), password.clone()));
            }
        }
        let session = self.session_id.lock().map(|s| s.clone()).unwrap_or_default();
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
        let (result, _) = self.rpc::<BlocklistUpdate>("blocklist-update", serde_json::json!({})).await?;
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
                let (ip, port) =
                    pbh_core::remap::translate_peer_ip(&p.address, p.port, &self.remap.ip_remapping);
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
            DownloaderFeature::LiveUpdateBtProtocolPort.name().to_string(),
        ]
    }

    fn login<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<LoginResult>> {
        Box::pin(async move {
            if self.config.paused {
                return Ok(LoginResult {
                    success: false,
                    message: "Transmission 已暂停".into(),
                    version: self.version(),
                });
            }
            let (result, session) = match self.rpc::<SessionGet>("session-get", serde_json::json!({})).await {
                Ok(v) => v,
                Err(e) => {
                    return Ok(LoginResult {
                        success: false,
                        message: format!("Transmission 登录失败: {e}"),
                        version: String::new(),
                    })
                }
            };
            if result != "success" {
                return Ok(LoginResult {
                    success: false,
                    message: format!("session-get 返回 {result}"),
                    version: String::new(),
                });
            }
            let version = Self::normalize_version(&session.version);
            if let Ok(mut guard) = self.last_version.lock() {
                *guard = version.clone();
            }
            // 要求 >= 4.1.0
            let parts: Vec<u32> = version
                .split('.')
                .filter_map(|s| s.chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok())
                .collect();
            let (major, minor) = (parts.first().copied().unwrap_or(0), parts.get(1).copied().unwrap_or(0));
            if major < 4 || (major == 4 && minor < 1) {
                return Ok(LoginResult {
                    success: false,
                    message: format!("Transmission 版本不支持（需 >= 4.1.0）：{version}"),
                    version,
                });
            }

            // blocklist 必须指向 PBH 自身端点
            let expected = self.config.blocklist_url.clone();
            if !session.blocklist_enabled || !session.blocklist_url.starts_with(&expected) {
                let url = format!("{expected}?t={}", chrono_like_now_ms());
                if !self.set_blocklist_url(&url).await? {
                    return Ok(LoginResult {
                        success: false,
                        message: "设置 Transmission blocklist URL 失败".into(),
                        version,
                    });
                }
                if !self.update_blocklist().await? {
                    return Ok(LoginResult {
                        success: false,
                        message: "Transmission blocklist 更新失败".into(),
                        version,
                    });
                }
            }
            if let Ok(mut guard) = self.healthy.lock() {
                *guard = true;
            }
            Ok(LoginResult { success: true, message: "OK".into(), version })
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
                if !(backend.rate_download > 0 || backend.rate_upload > 0 || backend.peers_connected > 0) {
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
            if !self.update_blocklist().await? {
                anyhow::bail!("blocklist-update 返回失败");
            }
            Ok(())
        })
    }

    fn statistics<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<DownloaderStatistics>> {
        Box::pin(async move {
            let (_, stats) = self.rpc::<SessionStats>("session-stats", serde_json::json!({})).await?;
            Ok(DownloaderStatistics {
                all_time_upload: stats.cumulative_stats.uploaded_bytes,
                all_time_download: stats.cumulative_stats.downloaded_bytes,
            })
        })
    }
}

/// 生成 cache-busting 时间戳（毫秒）。避免为此引入 chrono 依赖。
fn chrono_like_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}
