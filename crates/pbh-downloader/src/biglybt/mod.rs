//! BiglyBT 适配器，忠实复刻上游 `BiglyBT`（需在 BiglyBT 侧安装 PBH 适配器插件）。
//!
//! 要点：
//! - 每个请求都带 `Content-Type: application/json` 与 `Authorization: Bearer <token>`
//!   （对齐上游 OkHttp interceptor）；`endpoint` 结尾 `/` 按上游 `Config.readFromYaml`
//!   去掉**一个**。
//! - 登录：`GET /metadata` → 校验 `pluginVersion >= 1.3.0` → `POST /setconnector`
//!   上报 `ConnectorData{"PeerBanHelper", version, abbrev}`（上游是异步 `enqueue` 且不检查
//!   状态码，只记录传输失败）。
//! - torrents：`GET /downloads?filter=4&filter=5&filter=8`（下载中 / 做种 / 错误），
//!   实时上下行速率同时为 0 的条目被丢弃；`ignore-private`（上游默认 false）为真时跳过私有种子。
//! - peers：`GET /download/{infoHash}/peers`；404 视为「种子已删除或种子错误」返回空列表
//!   （Java 要求可变列表，故不是不可变空表）。
//! - 封禁：增量 `POST /bans {"ips":[…]}`、全量 `PUT /bans {"replaceWith":[…],"includeNonPBHEntries":false}`；
//!   两者都用上游单参 `remapBanListAddress(addr)`（等价 `supportRangeBan = true`）重映射并去重。
//! - 特性标志（顺序同 `BiglyBT.getFeatureFlags()`）：`READ_PEER_PROTOCOLS` / `UNBAN_IP` /
//!   `TRAFFIC_STATS` / `LIVE_UPDATE_BT_PROTOCOL_PORT` / `RANGE_BAN_IP`。
//!
//! 不在本阶段接口内、因此未复刻的上游能力：
//! - `getSpeedLimiter` / `setSpeedLimiter`、`getBTProtocolPort` / `setBTProtocolPort`、
//!   `getTrackers` / `setTrackers`、`saveDownloader`：Rust `Downloader` 特性集里没有对应方法。
//! - `BiglyBTTorrent.trackers`：`TorrentData` 无 trackers 字段。
//! - `PeerImpl.handshaking`（上游为 `peer.state != 30 && peer.state != 40`）：`PeerData`
//!   无该字段，模型以速率推导 `is_handshaking()`。
//! - `AbstractDownloader.login()` 的告警/退避机制（`AlertManager`、Sentry、
//!   `failedLoginAttempts` 达 15 次后 30 分钟冷却）：本阶段下载器不持有告警管理器，
//!   与其它适配器（qB / Transmission / Deluge）保持一致。

pub mod dto;

use crate::http::{BoxFuture, HttpFetcher, HttpRequest, HttpResponse, ReqwestFetcher};
use crate::{BanEntry, Downloader, DownloaderFeature, DownloaderStatistics, LoginResult};
use dto::*;
use pbh_core::defaults::qb as qbcfg;
use pbh_core::i18n::{Param, TranslationComponent, Translator};
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::remap::{remap_ban_list_address, translate_peer_ip, RemapConfig};
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use tracing::{error, warn};

/// `BiglyBTDownloadStateConst.ST_DOWNLOADING`：`Download.getState()`
const ST_DOWNLOADING: i32 = 4;
/// `BiglyBTDownloadStateConst.ST_SEEDING`
const ST_SEEDING: i32 = 5;
/// `BiglyBTDownloadStateConst.ST_ERROR`
const ST_ERROR: i32 = 8;

/// `ConnectorData` 的软件名固定值（对齐 `new ConnectorData("PeerBanHelper", …)`）。
const CONNECTOR_SOFTWARE: &str = "PeerBanHelper";
/// 对齐 `Main.getMeta().getVersion()`：本实现无构建元数据，使用 crate 版本。
const CONNECTOR_VERSION: &str = env!("CARGO_PKG_VERSION");
/// 对齐 `Main.getMeta().getAbbrev()`（`BuildMeta` 的缺省值 "Unknown"，本实现无 git 提交信息）。
const CONNECTOR_ABBREV: &str = "Unknown";

/// `pluginVersion` 的最低支持版本（上游 `semver.isLowerThan("1.3.0")`）。
const MIN_ADAPTER_VERSION: &str = "1.3.0";

const MSG_PAUSED: &str = "DOWNLOADER_PAUSED";
const MSG_STATUS_OK: &str = "STATUS_TEXT_OK";
const MSG_INCORRECT_CRED: &str = "DOWNLOADER_LOGIN_INCORRECT_CRED";
const MSG_LOGIN_EXCEPTION: &str = "DOWNLOADER_LOGIN_EXCEPTION";
const MSG_LOGIN_IO_EXCEPTION: &str = "DOWNLOADER_LOGIN_IO_EXCEPTION";
const MSG_ADAPTER_VERSION: &str = "DOWNLOADER_BIGLYBT_INCORRECT_ADAPTER_VERSION";
const MSG_INCORRECT_RESPONSE: &str = "DOWNLOADER_BIGLYBT_INCORRECT_RESPONSE";
const MSG_PEERS_FAILED: &str = "DOWNLOADER_BIGLYBT_FAILED_REQUEST_PEERS_LIST_IN_TORRENT";
const MSG_INCREMENT_BAN_FAILED: &str = "DOWNLOADER_BIGLYBT_INCREAMENT_BAN_FAILED";
const MSG_SAVE_BANLIST_FAILED: &str = "DOWNLOADER_BIGLYBT_FAILED_SAVE_BANLIST";
const MSG_STATISTICS_FAILED: &str = "DOWNLOADER_FAILED_REQUEST_STATISTICS";

/// 对齐上游 `tlUI`：下载器不持有 locale，使用服务端默认文案语言。
const UI_LOCALE: &str = "zh_cn";

#[derive(Clone, Debug)]
pub struct BiglyBtConfig {
    pub id: String,
    pub name: String,
    /// 适配器插件 HTTP API 地址（上游 `Config` 无默认值，必填）
    pub endpoint: String,
    /// `Authorization: Bearer <token>`（上游 `Config.token`，默认空串）
    pub token: String,
    /// 是否使用增量封禁（上游 `increment-ban`，默认 true）
    pub increment_ban: bool,
    /// 是否校验 TLS 证书（上游 `verify-ssl`，默认 true）
    pub verify_ssl: bool,
    /// 是否忽略私有种子（上游 `ignore-private`，**默认 false**：BiglyBT 默认包含私有种子）
    pub ignore_private: bool,
    /// 启动即暂停（上游 `paused`，默认 false）
    pub paused: bool,
    /// `config.yml` 的 `banlist-remapping` / `ip-remapping` 配置
    pub remap: RemapConfig,
}

impl Default for BiglyBtConfig {
    fn default() -> Self {
        Self {
            id: "biglybt".into(),
            name: "BiglyBT".into(),
            // 上游 `Config.readFromYaml` 里 endpoint 无默认值（`section.getString("endpoint")`），
            // 插件由用户自行安装，故不留臆测的默认端口。
            endpoint: String::new(),
            token: String::new(),
            increment_ban: true,
            verify_ssl: true,
            ignore_private: false,
            paused: false,
            remap: RemapConfig::default(),
        }
    }
}

pub struct BiglyBtDownloader {
    config: BiglyBtConfig,
    /// 去掉结尾 `/` 后的 API 前缀（对齐 `Config.readFromYaml`）
    endpoint: String,
    http: Arc<dyn HttpFetcher>,
}

impl BiglyBtDownloader {
    pub fn new(config: BiglyBtConfig) -> anyhow::Result<Self> {
        let fetcher = Arc::new(ReqwestFetcher::new(
            config.verify_ssl,
            qbcfg::CONNECT_TIMEOUT_SECS,
            qbcfg::READ_TIMEOUT_SECS,
        )?);
        Self::with_fetcher(config, fetcher)
    }

    pub fn with_fetcher(config: BiglyBtConfig, http: Arc<dyn HttpFetcher>) -> anyhow::Result<Self> {
        // 对齐 `Config.readFromYaml`：`if (endpoint.endsWith("/")) substring(0, length - 1)`，
        // 即只去掉**一个**结尾斜杠。
        let endpoint = match config.endpoint.strip_suffix('/') {
            Some(stripped) => stripped.to_string(),
            None => config.endpoint.clone(),
        };
        Ok(Self {
            config,
            endpoint,
            http,
        })
    }

    /// 上游 OkHttp interceptor：给每个请求补 `Content-Type` 与 Bearer 令牌。
    fn authed(&self, req: HttpRequest) -> HttpRequest {
        req.with_header("Content-Type", "application/json")
            .with_header("Authorization", &format!("Bearer {}", self.config.token))
    }

    async fn get(&self, path: &str) -> anyhow::Result<HttpResponse> {
        let url = format!("{}{}", self.endpoint, path);
        self.http.execute(self.authed(HttpRequest::get(url))).await
    }

    async fn post_json(&self, path: &str, body: String) -> anyhow::Result<HttpResponse> {
        let url = format!("{}{}", self.endpoint, path);
        self.http
            .execute(self.authed(HttpRequest::post_json(url, body)))
            .await
    }

    async fn put_json(&self, path: &str, body: String) -> anyhow::Result<HttpResponse> {
        let url = format!("{}{}", self.endpoint, path);
        let mut req = HttpRequest::post_json(url, body);
        req.method = "PUT".to_string();
        self.http.execute(self.authed(req)).await
    }

    /// 对齐 `remapBanListAddress(addr)`（上游单参重载 → `supportRangeBan = true`）：
    /// 逐个地址重映射，按首次出现顺序去重（等价 Java 的 `flatMap(...).distinct()`）。
    fn remap_ips(&self, ips: impl Iterator<Item = String>) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for ip in ips {
            for mapped in remap_ban_list_address(&ip, true, &self.config.remap) {
                if seen.insert(mapped.clone()) {
                    out.push(mapped);
                }
            }
        }
        out
    }

    /// `GET /downloads`，对齐上游私有方法 `fetchTorrents(filtersUrlEncoded, includePrivate)`。
    async fn fetch_downloads(&self, filters: &[i32]) -> anyhow::Result<Vec<DownloadRecord>> {
        let query = filters
            .iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join("&filter=");
        let resp = self.get(&format!("/downloads?filter={query}")).await?;
        if !is_success(resp.status) {
            anyhow::bail!(
                "{}",
                tl(
                    MSG_INCORRECT_RESPONSE,
                    vec![
                        Param::Text(resp.status.to_string()),
                        Param::Text(resp.body.clone()),
                    ]
                )
            );
        }
        Ok(serde_json::from_str(&resp.body).unwrap_or_default())
    }

    /// 对齐 `setBanListIncrement`：`POST /bans`，载荷为重映射后的地址。
    async fn set_ban_list_increment(&self, peers: &[BanEntry]) -> anyhow::Result<()> {
        let ips = self.remap_ips(peers.iter().map(|p| p.ip.clone()));
        let body = serde_json::to_string(&BanBean { ips })?;
        let resp = match self.post_json("/bans", body).await {
            Ok(resp) => resp,
            Err(e) => {
                let (class, message) = exception_params(&e);
                error!(
                    "{}",
                    tl(
                        MSG_INCREMENT_BAN_FAILED,
                        vec![
                            Param::Text(self.config.name.clone()),
                            Param::Text(self.endpoint.clone()),
                            Param::Text("N/A".to_string()),
                            Param::Text(class),
                            Param::Text(message),
                        ]
                    )
                );
                return Err(e);
            }
        };
        if !is_success(resp.status) {
            error!(
                "{}",
                tl(
                    MSG_INCREMENT_BAN_FAILED,
                    vec![
                        Param::Text(self.config.name.clone()),
                        Param::Text(self.endpoint.clone()),
                        Param::Text(resp.status.to_string()),
                        Param::Text("HTTP ERROR".to_string()),
                        Param::Text(resp.body),
                    ]
                )
            );
            anyhow::bail!("Save BiglyBT banlist error: statusCode={}", resp.status);
        }
        Ok(())
    }

    /// 对齐 `setBanListFull`：`PUT /bans`，整份列表替换。
    async fn set_ban_list_full(&self, ips: &[String]) -> anyhow::Result<()> {
        let mapped = self.remap_ips(ips.iter().cloned());
        let body = serde_json::to_string(&BanListReplacementBean {
            replace_with: mapped,
            include_non_pbh_entries: false,
        })?;
        let resp = match self.put_json("/bans", body).await {
            Ok(resp) => resp,
            Err(e) => {
                let (class, message) = exception_params(&e);
                error!(
                    "{}",
                    tl(
                        MSG_SAVE_BANLIST_FAILED,
                        vec![
                            Param::Text(self.config.name.clone()),
                            Param::Text(self.endpoint.clone()),
                            Param::Text("N/A".to_string()),
                            Param::Text(class),
                            Param::Text(message),
                        ]
                    )
                );
                return Err(e);
            }
        };
        if !is_success(resp.status) {
            error!(
                "{}",
                tl(
                    MSG_SAVE_BANLIST_FAILED,
                    vec![
                        Param::Text(self.config.name.clone()),
                        Param::Text(self.endpoint.clone()),
                        Param::Text(resp.status.to_string()),
                        Param::Text("HTTP ERROR".to_string()),
                        Param::Text(resp.body),
                    ]
                )
            );
            anyhow::bail!("Save BiglyBT banlist error: statusCode={}", resp.status);
        }
        Ok(())
    }
}

impl Downloader for BiglyBtDownloader {
    fn id(&self) -> &str {
        &self.config.id
    }
    fn name(&self) -> &str {
        &self.config.name
    }
    fn downloader_type(&self) -> &'static str {
        "biglybt"
    }

    /// 对齐 `BiglyBT.getFeatureFlags()`：集合与顺序均与上游一致（恒含 `RANGE_BAN_IP`）。
    fn feature_flags(&self) -> Vec<String> {
        vec![
            DownloaderFeature::ReadPeerProtocols.name().to_string(),
            DownloaderFeature::UnbanIp.name().to_string(),
            DownloaderFeature::TrafficStats.name().to_string(),
            DownloaderFeature::LiveUpdateBtProtocolPort
                .name()
                .to_string(),
            DownloaderFeature::RangeBanIp.name().to_string(),
        ]
    }

    /// 对齐 `BiglyBT.login0()`（外层 `AbstractDownloader.login()` 的暂停短路一并复刻）。
    fn login<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<LoginResult>> {
        Box::pin(async move {
            if self.config.paused {
                return Ok(LoginResult::paused(tl(MSG_PAUSED, Vec::new()), ""));
            }
            let resp = match self.get("/metadata").await {
                Ok(resp) => resp,
                // 对齐最外层 `catch (Exception e)` → NETWORK_ERROR（不进入冷却）
                Err(e) => {
                    let (class, message) = exception_params(&e);
                    return Ok(LoginResult::network_error(tl(
                        MSG_LOGIN_IO_EXCEPTION,
                        vec![Param::Text(format!("{class}: {message}"))],
                    )));
                }
            };
            if !is_success(resp.status) {
                if resp.status == 403 {
                    return Ok(LoginResult::incorrect_credential(tl(
                        MSG_INCORRECT_CRED,
                        Vec::new(),
                    )));
                }
                return Ok(LoginResult::exception(tl(
                    MSG_LOGIN_EXCEPTION,
                    vec![Param::Text(format!("statusCode={}", resp.status))],
                )));
            }
            // 解析失败在 Java 里是 JsonSyntaxException，同样被最外层 catch 转成 NETWORK_ERROR
            let metadata: MetadataCallbackBean = match serde_json::from_str(&resp.body) {
                Ok(metadata) => metadata,
                Err(e) => {
                    return Ok(LoginResult::network_error(tl(
                        MSG_LOGIN_IO_EXCEPTION,
                        vec![Param::Text(format!("JsonSyntaxException: {e}"))],
                    )));
                }
            };
            // 上游对 null 的 pluginVersion 会 NPE（同样落入最外层 catch）
            let Some(plugin_version) = metadata.plugin_version.as_deref().map(str::trim) else {
                return Ok(LoginResult::network_error(tl(
                    MSG_LOGIN_IO_EXCEPTION,
                    vec![Param::Text(
                        "NullPointerException: pluginVersion is null".to_string(),
                    )],
                )));
            };
            // `new Semver(version)` 解析失败会抛 SemverException（→ NETWORK_ERROR）
            let Some(too_old) = adapter_version_lower_than_min(plugin_version, MIN_ADAPTER_VERSION)
            else {
                return Ok(LoginResult::network_error(tl(
                    MSG_LOGIN_IO_EXCEPTION,
                    vec![Param::Text(format!("SemverException: {plugin_version}"))],
                )));
            };
            if too_old {
                return Ok(LoginResult::require_take_actions(
                    tl(
                        MSG_ADAPTER_VERSION,
                        vec![Param::Text(MIN_ADAPTER_VERSION.to_string())],
                    ),
                    plugin_version,
                ));
            }
            // 上游 `enqueue` 后立即返回：不等待、不检查状态码，只记录传输失败。
            let payload = ConnectorData {
                software: CONNECTOR_SOFTWARE.to_string(),
                version: CONNECTOR_VERSION.to_string(),
                abbrev: CONNECTOR_ABBREV.to_string(),
            };
            match serde_json::to_string(&payload) {
                Ok(body) => {
                    if let Err(e) = self.post_json("/setconnector", body).await {
                        warn!("Unable to set connector for BiglyBT: {e}");
                    }
                }
                Err(e) => warn!("Unable to set connector for BiglyBT: {e}"),
            }
            Ok(LoginResult::success(
                tl(MSG_STATUS_OK, Vec::new()),
                plugin_version.to_string(),
            ))
        })
    }

    /// 对齐 `BiglyBT.getTorrents()`：`fetchTorrents([4,5,8], !ignorePrivate)` 后按速率过滤。
    fn fetch_torrents<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Vec<TorrentData>>> {
        Box::pin(async move {
            let records = self
                .fetch_downloads(&[ST_DOWNLOADING, ST_SEEDING, ST_ERROR])
                .await?;
            let include_private = !self.config.ignore_private;
            let mut out = Vec::new();
            for detail in &records {
                let torrent = detail.torrent.clone().unwrap_or_default();
                let stats = detail.stats.clone().unwrap_or_default();
                if !include_private && torrent.private_torrent {
                    continue;
                }
                let data = TorrentData {
                    // `BiglyBTTorrent.id` 与 `hash` 都取 `torrent.infoHash`
                    hash: torrent.info_hash.clone(),
                    name: detail.name.clone(),
                    // `completedInThousandNotation / 1000d`
                    progress: stats.completed_in_thousand_notation as f64 / 1000.0,
                    total_size: torrent.size,
                    // 上游直接用 stats 口径给出完成量，不按 piece 推算
                    piece_size: 0,
                    pieces_have: 0,
                    // 种子总大小 - 尚未下载大小（含未选择文件）= 已下载内容大小
                    completed_override: Some(torrent.size - stats.remaining_bytes),
                    dlspeed: stats.rt_download_speed,
                    upspeed: stats.rt_upload_speed,
                    is_private: Some(torrent.private_torrent),
                };
                if data.dlspeed > 0 || data.upspeed > 0 {
                    out.push(data);
                }
            }
            Ok(out)
        })
    }

    /// 对齐 `BiglyBT.getPeers(torrent)`。
    fn fetch_peers<'a>(
        &'a self,
        torrent: &'a TorrentData,
    ) -> BoxFuture<'a, anyhow::Result<Vec<PeerData>>> {
        Box::pin(async move {
            let resp = self
                .get(&format!("/download/{}/peers", torrent.id()))
                .await?;
            if resp.status == 404 {
                // 种子被删除或者种子错误时会返回 404；Java 返回可变空表
                return Ok(Vec::new());
            }
            if !is_success(resp.status) {
                anyhow::bail!(
                    "{}",
                    tl(
                        MSG_PEERS_FAILED,
                        vec![
                            Param::Text(resp.status.to_string()),
                            Param::Text(resp.body.clone()),
                        ]
                    )
                );
            }
            let manager: PeerManagerRecord = serde_json::from_str(&resp.body).unwrap_or_default();
            let mut out = Vec::new();
            for peer in manager.peers.as_deref().unwrap_or_default() {
                let stats = peer.stats.clone().unwrap_or_default();
                let raw_port = peer.port.clamp(0, u16::MAX as i32) as u16;
                // 上游先解码 peerId（`hexToByteArray(null)` 会 NPE），再判空 ip
                let peer_id = decode_peer_id_hex(peer.peer_id.as_deref().unwrap_or_default());
                let Some(ip) = peer.ip.as_deref() else {
                    continue;
                };
                // `ip == null || ip.isBlank()`（Java 的 isBlank 对纯空白串同样为真）
                if ip.trim().is_empty() {
                    continue;
                }
                // `startsWith("/") ? substring(1)`
                let raw_ip = ip.strip_prefix('/').unwrap_or(ip);
                // 对齐 `addressTranslate(new PeerAddress(ip, port, ip))`：
                // ip/port 为翻译后的地址，raw 部分保留下载器上报的原始 ip:port
                let (ip, port) =
                    translate_peer_ip(raw_ip, raw_port, &self.config.remap.ip_remapping);
                out.push(PeerData {
                    client_name: peer.client.clone(),
                    peer_id: Some(peer_id),
                    dl_speed: stats.rt_download_speed,
                    downloaded: stats.total_received,
                    up_speed: stats.rt_upload_speed,
                    uploaded: stats.total_sent,
                    progress: peer.percent_done_in_thousand_notation as f64 / 1000.0,
                    flags: peer_flag_string(peer),
                    ip,
                    port,
                    raw_ip: format!("{raw_ip}:{raw_port}"),
                    // 上游 `PeerImpl` 无连接类型字段
                    connection: None,
                });
            }
            Ok(out)
        })
    }

    /// 对齐 `setBanListIncrement`（上游在 `removed` 为空、开启增量且非全量重放时才走增量）。
    fn ban_peers<'a>(&'a self, peers: &'a [BanEntry]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move { self.set_ban_list_increment(peers).await })
    }

    /// 对齐 `setBanListFull`。
    fn replace_banned_ips<'a>(&'a self, ips: &'a [String]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move { self.set_ban_list_full(ips).await })
    }

    /// 对齐 `BiglyBT.getSpeedLimiter()`：`GET /speedlimiter`（单位 **bytes/s**）。
    /// 非 2xx 或解析失败 → 返回 `Err`（调用方 `ActiveMonitoringModule` 据此跳过本下载器）。
    fn get_speed_limiter<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<(i64, i64)>> {
        Box::pin(async move {
            let resp = self.get("/speedlimiter").await?;
            if resp.status != 200 {
                anyhow::bail!("BiglyBT getSpeedLimiter failed with status {}", resp.status);
            }
            let parsed: CurrentSpeedLimiterBean = serde_json::from_str(&resp.body)
                .map_err(|e| anyhow::anyhow!("BiglyBT getSpeedLimiter parse error: {e}"))?;
            Ok((parsed.upload, parsed.download))
        })
    }

    /// 对齐 `BiglyBT.setSpeedLimiter(...)`：`POST /speedlimiter`，负载 `SetSpeedLimiterBean`（单位 bytes/s）。
    /// 非 2xx → 返回 `Err`。
    fn set_speed_limiter<'a>(
        &'a self,
        upload: i64,
        download: i64,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let body = serde_json::to_string(&SetSpeedLimiterBean { upload, download })?;
            let resp = self.post_json("/speedlimiter", body).await?;
            if resp.status != 200 {
                anyhow::bail!("BiglyBT setSpeedLimiter failed with status {}", resp.status);
            }
            Ok(())
        })
    }

    /// 对齐 `BiglyBT.getStatistics()`；失败时对齐调用方语义（记日志 + 返回 0，不抛错）。
    fn statistics<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<DownloaderStatistics>> {
        Box::pin(async move {
            let resp = match self.get("/statistics").await {
                Ok(resp) => resp,
                Err(e) => {
                    let (_, message) = exception_params(&e);
                    error!(
                        "{}",
                        tl(
                            MSG_STATISTICS_FAILED,
                            vec![Param::Text("N/A".to_string()), Param::Text(message)]
                        )
                    );
                    return Ok(DownloaderStatistics::default());
                }
            };
            if !is_success(resp.status) {
                error!(
                    "{}",
                    tl(
                        MSG_STATISTICS_FAILED,
                        vec![Param::Text(resp.status.to_string()), Param::Text(resp.body)]
                    )
                );
                return Ok(DownloaderStatistics::default());
            }
            let Ok(record) = serde_json::from_str::<StatisticsRecord>(&resp.body) else {
                error!(
                    "{}",
                    tl(
                        MSG_STATISTICS_FAILED,
                        vec![
                            Param::Text("N/A".to_string()),
                            Param::Text("invalid statistics response".to_string()),
                        ]
                    )
                );
                return Ok(DownloaderStatistics::default());
            };
            Ok(DownloaderStatistics {
                all_time_upload: record.overall_data_bytes_sent.unwrap_or(0),
                all_time_download: record.overall_data_bytes_received.unwrap_or(0),
            })
        })
    }
}

/// 对齐 `BiglyBT.parseFlag(peer)` 读取的字段 + `PeerFlag.toString()` 的字符顺序。
///
/// 上游把 `PeerFlag` 对象交给 `PeerImpl`（下游可直接读 `isFromIncoming()` 等位）；本实现的
/// `PeerData` 只保留 libtorrent 风格字符串，故按 `toString()` 顺序渲染。
/// 上游硬编码为 `false` 的位不会出现在 `toString()` 输出里：`supportsExtensions`、
/// `onParole`、`uploadOnly`、`endGameMode`、`sslSocket`、`plainTextEncrypted`（"e"）、
/// `fromLSD`（"L"）、`fromResumeData`；`handshake`（`state == 20`，
/// `BiglyBTDownloadManagerStateConst.STATE_ALLOCATING`）与 `connecting`（`state == 10`，
/// `STATE_INITIALIZED`）虽被置位但 `toString()` 不输出；`fromTracker` 与 `fromIncoming`
/// 同样不输出，故字符串无法表达。
///
/// 上游 `parseFlag` 异常时返回 `null`（对应 `flags: None`）；本实现字段类型化后不会抛错，
/// 恒返回 `Some`。
fn peer_flag_string(peer: &PeerRecord) -> Option<String> {
    let interesting = peer.interesting;
    let choked = peer.choking;
    let remote_interested = peer.interested;
    let remote_choked = peer.choked;
    let optimistic_unchoke = peer.optimistic_unchoke;
    let snubbed = peer.snubbed;
    // `outgoingConnection` 与 `localConnection` 都取 `!peer.isIncoming()`
    let local_connection = !peer.incoming;
    let from_dht = peer.peer_source.as_deref() == Some("DHT");
    let from_pex = peer.peer_source.as_deref() == Some("PeerExchange");
    let rc4_encrypted = peer.use_crypto;
    let utp_socket = peer.protocol.as_deref() == Some("uTP");

    let mut parts: Vec<&str> = Vec::new();
    if interesting {
        parts.push(if remote_choked { "d" } else { "D" });
    }
    if remote_interested {
        parts.push(if choked { "u" } else { "U" });
    }
    if !remote_choked && !interesting {
        parts.push("K");
    }
    if !choked && !remote_interested {
        parts.push("?");
    }
    if optimistic_unchoke {
        parts.push("O");
    }
    if snubbed {
        parts.push("S");
    }
    if !local_connection {
        parts.push("I");
    }
    if from_dht {
        parts.push("H");
    }
    if from_pex {
        parts.push("X");
    }
    if rc4_encrypted {
        parts.push("E");
    }
    if utp_socket {
        parts.push("P");
    }
    Some(parts.join(" "))
}

/// 对齐 `new String(ByteUtil.hexToByteArray(peerId), ISO_8859_1)`：
/// 长度为奇数时**前置**补 `0`（`ByteUtil.hexToByteArray` 的行为），再两两解码为字节，
/// 逐字节按 ISO-8859-1 映射为字符（不做 8 字符截断，这一点与 Deluge 适配器不同）。
///
/// 上游遇到非法十六进制会抛 `NumberFormatException`；此处退回「已解码前缀」。
fn decode_peer_id_hex(hex: &str) -> String {
    let mut chars: Vec<char> = Vec::with_capacity(hex.chars().count() + 1);
    if hex.chars().count() % 2 == 1 {
        chars.push('0');
    }
    chars.extend(hex.chars());
    let mut out = String::with_capacity(chars.len() / 2);
    // 已预先补'0'保证长度恒为偶数，as_chunks 与 chunks_exact 等价且无 panic 风险
    for pair in chars.as_chunks::<2>().0 {
        match (pair[0].to_digit(16), pair[1].to_digit(16)) {
            (Some(hi), Some(lo)) => out.push(char::from((hi * 16 + lo) as u8)),
            _ => break,
        }
    }
    out
}

/// 对齐上游 `new Semver(version).isLowerThan(min)`（semver4j LOOSE 模式）：
/// 只比较 `major.minor.patch`（缺省分量按 0），相等时带预发布后缀的版本低于正式版
/// （如 `1.3.0-beta` < `1.3.0`）。
///
/// 版本串不可解析时返回 `None`：上游 `new Semver(...)` 会抛 `SemverException`，
/// 由最外层 catch 转成 NETWORK_ERROR。
fn adapter_version_lower_than_min(version: &str, min: &str) -> Option<bool> {
    let (current, current_pre) = parse_loose_semver(version)?;
    let (floor, _) = parse_loose_semver(min)?;
    Some(match current.cmp(&floor) {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Greater => false,
        // 相等：正式版 > 预发布版
        std::cmp::Ordering::Equal => current_pre.is_some_and(|pre| !pre.is_empty()),
    })
}

/// 解析 `major.minor.patch[-pre][+build]`，返回 ((major, minor, patch), 预发布后缀)。
fn parse_loose_semver(raw: &str) -> Option<((u64, u64, u64), Option<String>)> {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
    if trimmed.is_empty() {
        return None;
    }
    let (core, pre) = match trimmed.split_once('-') {
        Some((core, rest)) => (core, Some(rest.split('+').next().unwrap_or_default())),
        None => match trimmed.split_once('+') {
            Some((core, _)) => (core, None),
            None => (trimmed, None),
        },
    };
    let mut parts = core.split('.');
    let major = parse_num(parts.next()?)?;
    let minor = parse_num(parts.next().unwrap_or("0"))?;
    let patch = parse_num(parts.next().unwrap_or("0"))?;
    Some(((major, minor, patch), pre.map(|p| p.to_string())))
}

fn parse_num(raw: &str) -> Option<u64> {
    if raw.is_empty() {
        return None;
    }
    raw.parse::<u64>().ok()
}

/// OkHttp `Response.isSuccessful()`：200..=299
fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// `GET /downloads` 夹具：活跃公开 / 活跃私有 / 空闲公开。
    const DOWNLOADS: &str = r#"[
      {
        "state": 4,
        "name": "ubuntu.iso",
        "torrent": {
          "infoHash": "1111111111111111111111111111111111111111",
          "size": 1000,
          "privateTorrent": false
        },
        "stats": {
          "completedInThousandNotation": 425,
          "remainingBytes": 575,
          "rtDownloadSpeed": 20,
          "rtUploadSpeed": 10
        }
      },
      {
        "state": 5,
        "name": "private.bin",
        "torrent": {
          "infoHash": "2222222222222222222222222222222222222222",
          "size": 2000,
          "privateTorrent": true
        },
        "stats": {
          "completedInThousandNotation": 1000,
          "remainingBytes": 0,
          "rtDownloadSpeed": 5,
          "rtUploadSpeed": 0
        }
      },
      {
        "state": 4,
        "name": "idle.bin",
        "torrent": {
          "infoHash": "3333333333333333333333333333333333333333",
          "size": 3000,
          "privateTorrent": false
        },
        "stats": {
          "completedInThousandNotation": 0,
          "remainingBytes": 3000,
          "rtDownloadSpeed": 0,
          "rtUploadSpeed": 0
        }
      }
    ]"#;

    /// `GET /download/{infoHash}/peers` 夹具（最后两条为应被跳过的空 ip / null ip）。
    const PEERS: &str = r#"{
      "peers": [
        {
          "state": 10,
          "peerId": "2d5452313030302d6162636465666768696a",
          "ip": "/9.9.9.9",
          "port": 6881,
          "choking": true,
          "choked": true,
          "interested": true,
          "interesting": true,
          "seed": false,
          "snubbed": false,
          "incoming": false,
          "percentDoneInThousandNotation": 555,
          "client": "-TR1000-abcdefghij",
          "optimisticUnchoke": false,
          "peerSource": "DHT",
          "useCrypto": true,
          "protocol": "uTP",
          "stats": {
            "rtDownloadSpeed": 202,
            "rtUploadSpeed": 101,
            "totalSent": 303,
            "totalReceived": 404
          }
        },
        {
          "state": 40,
          "peerId": "2d5452",
          "ip": "2001:db8::1",
          "port": 7000,
          "choking": false,
          "choked": false,
          "interested": false,
          "interesting": false,
          "snubbed": true,
          "incoming": true,
          "optimisticUnchoke": true,
          "percentDoneInThousandNotation": 0,
          "client": null,
          "peerSource": "PeerExchange",
          "useCrypto": false,
          "protocol": "TCP",
          "stats": {}
        },
        {
          "state": 20,
          "peerId": "2d54523",
          "ip": "64:ff9b::102:304",
          "port": 7001,
          "choking": false,
          "choked": false,
          "interested": false,
          "interesting": false,
          "incoming": false,
          "percentDoneInThousandNotation": 100,
          "stats": {
            "rtDownloadSpeed": 1,
            "rtUploadSpeed": 2,
            "totalSent": 4,
            "totalReceived": 3
          }
        },
        { "state": 30, "peerId": "00", "ip": "", "port": 1, "stats": {} },
        { "state": 30, "peerId": "00", "ip": "   ", "port": 2, "stats": {} },
        { "state": 30, "peerId": "00", "ip": null, "port": 3, "stats": {} }
      ],
      "pendingPeers": [],
      "peerStats": {}
    }"#;

    const METADATA: &str = r#"{
      "pluginVersion": "1.3.0",
      "applicationVersion": "3.0.0.0",
      "applicationName": "BiglyBT",
      "azureusName": "BiglyBT"
    }"#;

    const STATISTICS: &str = r#"{
      "overallDataBytesReceived": 333444,
      "overallDataBytesSent": 111222
    }"#;

    /// 被记录到的请求。
    #[derive(Clone, Debug)]
    struct Recorded {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Option<String>,
    }

    /// 插件 HTTP API 的内存实现：按路径返回夹具并记录收到的请求。
    struct BiglyBtMock {
        requests: Mutex<Vec<Recorded>>,
        speedlimiter: Mutex<(u16, String)>,
        metadata: Mutex<(u16, String)>,
        downloads: Mutex<(u16, String)>,
        peers: Mutex<(u16, String)>,
        statistics: Mutex<(u16, String)>,
        bans_status: Mutex<u16>,
        /// 为 true 时所有请求都返回传输层错误
        transport_error: Mutex<bool>,
    }

    impl BiglyBtMock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                speedlimiter: Mutex::new((
                    200,
                    r#"{"upload":1048576,"download":2097152}"#.to_string(),
                )),
                metadata: Mutex::new((200, METADATA.to_string())),
                downloads: Mutex::new((200, DOWNLOADS.to_string())),
                peers: Mutex::new((200, PEERS.to_string())),
                statistics: Mutex::new((200, STATISTICS.to_string())),
                bans_status: Mutex::new(200),
                transport_error: Mutex::new(false),
            })
        }

        fn requests(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }

        /// 第一个 URL 以 `suffix` 结尾的请求
        fn request(&self, suffix: &str) -> Recorded {
            self.requests()
                .into_iter()
                .find(|r| r.url.ends_with(suffix))
                .unwrap_or_else(|| panic!("未收到以 {suffix} 结尾的请求"))
        }
    }

    impl HttpFetcher for BiglyBtMock {
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
                let (status, body) = if req.url.ends_with("/metadata") {
                    self.metadata.lock().unwrap().clone()
                } else if req.url.contains("/downloads?") {
                    self.downloads.lock().unwrap().clone()
                } else if req.url.ends_with("/peers") {
                    self.peers.lock().unwrap().clone()
                } else if req.url.ends_with("/statistics") {
                    self.statistics.lock().unwrap().clone()
                } else if req.url.ends_with("/bans") {
                    (*self.bans_status.lock().unwrap(), "{}".to_string())
                } else if req.url.ends_with("/setconnector") {
                    (200, "{}".to_string())
                } else if req.url.ends_with("/speedlimiter") {
                    self.speedlimiter.lock().unwrap().clone()
                } else {
                    (404, String::new())
                };
                Ok(HttpResponse::new(status, body))
            })
        }
    }

    fn downloader(mock: Arc<BiglyBtMock>, ignore_private: bool) -> Arc<BiglyBtDownloader> {
        let config = BiglyBtConfig {
            id: "biglybt-test".into(),
            name: "BiglyBT Test".into(),
            // 结尾斜杠用于验证「只去掉一个」
            endpoint: "http://mock.local/".into(),
            token: "test-token".into(),
            ignore_private,
            ..BiglyBtConfig::default()
        };
        Arc::new(BiglyBtDownloader::with_fetcher(config, mock as Arc<dyn HttpFetcher>).unwrap())
    }

    /// 对齐上游 OkHttp interceptor：每个请求都必须带这两个头。
    fn assert_authed(req: &Recorded) {
        let has = |name: &str, value: &str| {
            req.headers
                .iter()
                .any(|(k, v)| k.eq_ignore_ascii_case(name) && v == value)
        };
        assert!(
            has("Content-Type", "application/json"),
            "缺少 Content-Type: {:?}",
            req.headers
        );
        assert!(
            has("Authorization", "Bearer test-token"),
            "缺少 Authorization: {:?}",
            req.headers
        );
    }

    fn first_torrent(torrents: &[TorrentData]) -> &TorrentData {
        torrents
            .iter()
            .find(|t| t.hash == "1111111111111111111111111111111111111111")
            .expect("torrent-1")
    }

    #[tokio::test]
    async fn login_checks_adapter_version_and_sets_connector() {
        let mock = BiglyBtMock::new();
        let dl = downloader(mock.clone(), false);
        let login = dl.login().await.unwrap();
        assert!(login.success, "{}", login.message);
        assert_eq!(login.version, "1.3.0");

        let reqs = mock.requests();
        assert_eq!(
            reqs.iter()
                .map(|r| format!("{} {}", r.method, r.url))
                .collect::<Vec<_>>(),
            vec![
                "GET http://mock.local/metadata",
                "POST http://mock.local/setconnector"
            ]
        );
        reqs.iter().for_each(assert_authed);
        // ConnectorData 的字段与顺序（Gson 输出顺序）均与上游一致
        assert_eq!(
            reqs[1].body.as_deref().unwrap(),
            format!(
                r#"{{"software":"PeerBanHelper","version":"{}","abbrev":"Unknown"}}"#,
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[tokio::test]
    async fn login_reports_incorrect_credentials() {
        let mock = BiglyBtMock::new();
        *mock.metadata.lock().unwrap() = (403, String::new());
        let dl = downloader(mock.clone(), false);
        let login = dl.login().await.unwrap();
        assert!(!login.success);
        assert!(login.message.contains("凭据"), "{}", login.message);
        assert_eq!(mock.requests().len(), 1, "凭据错误时不应再发 setconnector");
    }

    #[tokio::test]
    async fn login_rejects_outdated_adapter() {
        let mock = BiglyBtMock::new();
        *mock.metadata.lock().unwrap() = (200, r#"{"pluginVersion":"1.2.9"}"#.to_string());
        let dl = downloader(mock.clone(), false);
        let login = dl.login().await.unwrap();
        assert!(!login.success);
        assert!(login.message.contains("适配器"), "{}", login.message);
        assert!(login.message.contains("1.3.0"), "{}", login.message);
        assert_eq!(login.version, "1.2.9");
        assert_eq!(
            mock.requests().len(),
            1,
            "版本不支持时不应再发 setconnector"
        );
    }

    #[tokio::test]
    async fn login_reports_server_error_and_network_failure() {
        let mock = BiglyBtMock::new();
        *mock.metadata.lock().unwrap() = (500, "boom".to_string());
        let dl = downloader(mock.clone(), false);
        let login = dl.login().await.unwrap();
        assert!(!login.success);
        assert!(
            login.message.contains("statusCode=500"),
            "{}",
            login.message
        );

        let mock = BiglyBtMock::new();
        *mock.transport_error.lock().unwrap() = true;
        let dl = downloader(mock.clone(), false);
        let login = dl.login().await.unwrap();
        assert!(!login.success);
        assert!(login.message.contains("网络错误"), "{}", login.message);
    }

    #[tokio::test]
    async fn torrents_are_mapped_and_speed_filtered() {
        let mock = BiglyBtMock::new();
        let dl = downloader(mock.clone(), false);
        let torrents = dl.fetch_torrents().await.unwrap();
        // 空闲条目被速率过滤；ignore-private 默认 false → 私有种子保留
        assert_eq!(torrents.len(), 2);

        let first = first_torrent(&torrents);
        assert_eq!(first.name, "ubuntu.iso");
        assert_eq!(first.progress, 0.425);
        assert_eq!(first.total_size, 1000);
        // 1000 - 575 = 425
        assert_eq!(first.completed_override, Some(425));
        assert_eq!(first.completed_size(), 425);
        assert_eq!(first.dlspeed, 20);
        assert_eq!(first.upspeed, 10);
        assert!(!first.is_private());

        let private = torrents
            .iter()
            .find(|t| t.hash == "2222222222222222222222222222222222222222")
            .expect("torrent-2");
        assert!(private.is_private());
        assert_eq!(private.progress, 1.0);
        assert_eq!(private.completed_override, Some(2000));

        let req = mock.request("/downloads?filter=4&filter=5&filter=8");
        assert_eq!(req.method, "GET");
        assert_eq!(
            req.url,
            "http://mock.local/downloads?filter=4&filter=5&filter=8"
        );
        assert_authed(&req);

        // ignore-private = true：私有种子被跳过
        let mock = BiglyBtMock::new();
        let dl = downloader(mock.clone(), true);
        let torrents = dl.fetch_torrents().await.unwrap();
        assert_eq!(torrents.len(), 1);
        assert_eq!(torrents[0].hash, "1111111111111111111111111111111111111111");
    }

    #[tokio::test]
    async fn speed_limiter_get_and_set() {
        let mock = BiglyBtMock::new();
        let dl = downloader(mock.clone(), false);
        // 对齐 `setSpeedLimiter(...)`：POST /speedlimiter（先调用，使首个 /speedlimiter 请求为 POST）
        dl.set_speed_limiter(0, 0).await.unwrap();
        let req = mock.request("/speedlimiter");
        assert_eq!(req.method, "POST");
        // 对齐 `getSpeedLimiter()`：GET /speedlimiter（bytes/s）
        let (up, dl_) = dl.get_speed_limiter().await.unwrap();
        assert_eq!(up, 1_048_576);
        assert_eq!(dl_, 2_097_152);
    }

    #[tokio::test]
    async fn peers_are_mapped_with_flags_and_hex_peer_id() {
        let mock = BiglyBtMock::new();
        let dl = downloader(mock.clone(), false);
        let torrents = dl.fetch_torrents().await.unwrap();
        let torrent = first_torrent(&torrents);
        let peers = dl.fetch_peers(torrent).await.unwrap();
        // 空串 / 纯空白 / null 的三条 ip 被跳过
        assert_eq!(peers.len(), 3);

        let by_ip: HashMap<&str, &PeerData> = peers.iter().map(|p| (p.ip.as_str(), p)).collect();

        // 前导 `/` 被去掉；peerId 十六进制解码后**不截断**（与 Deluge 适配器不同）
        let tr = by_ip.get("9.9.9.9").expect("peer 9.9.9.9");
        assert_eq!(tr.peer_id.as_deref(), Some("-TR1000-abcdefghij"));
        assert_eq!(tr.client_name.as_deref(), Some("-TR1000-abcdefghij"));
        assert_eq!(tr.dl_speed, 202);
        assert_eq!(tr.up_speed, 101);
        assert_eq!(tr.downloaded, 404);
        assert_eq!(tr.uploaded, 303);
        assert_eq!(tr.progress, 0.555);
        // interesting + remoteChoked → "d"；remoteInterested + choked → "u"；
        // 来源 DHT → "H"；useCrypto → "E"；uTP → "P"
        assert_eq!(tr.flags.as_deref(), Some("d u H E P"));
        assert_eq!(tr.port, 6881);
        assert_eq!(tr.raw_ip, "9.9.9.9:6881");
        assert!(tr.connection.is_none());

        // 入站连接 → "I"（`localConnection = !incoming`）；来源 PeerExchange → "X"
        let incoming = by_ip.get("2001:db8::1").expect("peer 2001:db8::1");
        assert_eq!(incoming.flags.as_deref(), Some("K ? O S I X"));
        assert_eq!(incoming.raw_ip, "2001:db8::1:7000");
        assert_eq!(incoming.peer_id.as_deref(), Some("-TR"));
        assert_eq!(incoming.client_name, None);

        // NAT64 翻译为 1.2.3.4，raw_ip 保留原地址；奇数长度 hex 前置补 0
        let nat64 = by_ip.get("1.2.3.4").expect("NAT64 peer");
        assert_eq!(nat64.raw_ip, "64:ff9b::102:304:7001");
        assert_eq!(nat64.port, 7001);
        assert_eq!(nat64.peer_id.as_deref(), Some("\u{2}\u{d5}E#"));
        assert_eq!(nat64.progress, 0.1);

        let req = mock.request("/peers");
        assert_eq!(
            req.url,
            "http://mock.local/download/1111111111111111111111111111111111111111/peers"
        );
        assert_authed(&req);
    }

    #[tokio::test]
    async fn peers_404_is_empty_and_other_errors_fail() {
        let mock = BiglyBtMock::new();
        *mock.peers.lock().unwrap() = (404, "Not Found".to_string());
        let dl = downloader(mock.clone(), false);
        let torrents = dl.fetch_torrents().await.unwrap();
        let torrent = first_torrent(&torrents);
        assert!(dl.fetch_peers(torrent).await.unwrap().is_empty());

        *mock.peers.lock().unwrap() = (500, "boom".to_string());
        let err = dl.fetch_peers(torrent).await.unwrap_err();
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[tokio::test]
    async fn incremental_ban_posts_remapped_ips() {
        let mock = BiglyBtMock::new();
        let dl = downloader(mock.clone(), false);
        dl.ban_peers(&[
            BanEntry {
                ip: "1.2.3.4".into(),
                port: 6881,
                raw_ip: "1.2.3.4:6881".into(),
            },
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

        let req = mock.request("/bans");
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "http://mock.local/bans");
        assert_authed(&req);
        // remapBanListAddress(addr) → supportRangeBan = true：去重 + IPv6 /52 网段
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(req.body.as_deref().unwrap()).unwrap(),
            serde_json::json!({ "ips": ["1.2.3.4", "::ffff:1.2.3.4", "2001:db8::1", "2001:db8::/52"] })
        );
    }

    #[tokio::test]
    async fn full_ban_replaces_ban_list() {
        let mock = BiglyBtMock::new();
        let dl = downloader(mock.clone(), false);
        dl.replace_banned_ips(&["1.2.3.4".to_string(), "2001:db8::1".to_string()])
            .await
            .unwrap();

        let req = mock.request("/bans");
        assert_eq!(req.method, "PUT");
        assert_eq!(req.url, "http://mock.local/bans");
        assert_authed(&req);
        // 字段与顺序对齐 `BanListReplacementBean` 的 Gson 输出
        assert_eq!(
            req.body.as_deref().unwrap(),
            r#"{"replaceWith":["1.2.3.4","::ffff:1.2.3.4","2001:db8::1","2001:db8::/52"],"includeNonPBHEntries":false}"#
        );
    }

    #[tokio::test]
    async fn ban_failures_return_error() {
        let mock = BiglyBtMock::new();
        *mock.bans_status.lock().unwrap() = 500;
        let dl = downloader(mock.clone(), false);
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
    }

    #[tokio::test]
    async fn statistics_are_read_and_failures_return_zero() {
        let mock = BiglyBtMock::new();
        let dl = downloader(mock.clone(), false);
        let stats = dl.statistics().await.unwrap();
        assert_eq!(stats.all_time_upload, 111_222);
        assert_eq!(stats.all_time_download, 333_444);
        let req = mock.request("/statistics");
        assert_eq!(req.method, "GET");
        assert_eq!(req.url, "http://mock.local/statistics");
        assert_authed(&req);

        // HTTP 失败：对齐上游调用方语义，记日志并返回 0（不返回 Err）
        let mock = BiglyBtMock::new();
        *mock.statistics.lock().unwrap() = (500, "boom".to_string());
        let dl = downloader(mock.clone(), false);
        let stats = dl.statistics().await.unwrap();
        assert_eq!(stats.all_time_upload, 0);
        assert_eq!(stats.all_time_download, 0);

        // 传输层失败同样返回 0
        let mock = BiglyBtMock::new();
        *mock.transport_error.lock().unwrap() = true;
        let dl = downloader(mock, false);
        let stats = dl.statistics().await.unwrap();
        assert_eq!(stats.all_time_upload, 0);
        assert_eq!(stats.all_time_download, 0);
    }

    #[test]
    fn feature_flags_match_upstream_and_order() {
        let mock = BiglyBtMock::new();
        let dl = downloader(mock, false);
        assert_eq!(
            dl.feature_flags(),
            vec![
                "READ_PEER_PROTOCOLS".to_string(),
                "UNBAN_IP".to_string(),
                "TRAFFIC_STATS".to_string(),
                "LIVE_UPDATE_BT_PROTOCOL_PORT".to_string(),
                "RANGE_BAN_IP".to_string(),
            ]
        );
        assert_eq!(dl.downloader_type(), "biglybt");
    }

    #[test]
    fn adapter_version_gate_matches_semver_rules() {
        assert_eq!(adapter_version_lower_than_min("1.2.9", "1.3.0"), Some(true));
        assert_eq!(
            adapter_version_lower_than_min("1.3.0", "1.3.0"),
            Some(false)
        );
        assert_eq!(
            adapter_version_lower_than_min("1.3.1", "1.3.0"),
            Some(false)
        );
        // LOOSE 模式：缺省分量按 0
        assert_eq!(adapter_version_lower_than_min("1.3", "1.3.0"), Some(false));
        assert_eq!(adapter_version_lower_than_min("1", "1.3.0"), Some(true));
        // 预发布版低于同号正式版
        assert_eq!(
            adapter_version_lower_than_min("1.3.0-beta1", "1.3.0"),
            Some(true)
        );
        assert_eq!(
            adapter_version_lower_than_min("1.4.0-beta1", "1.3.0"),
            Some(false)
        );
        // 构建元数据不影响比较
        assert_eq!(
            adapter_version_lower_than_min("1.3.0+build.5", "1.3.0"),
            Some(false)
        );
        // 不可解析 → 上游 Semver 构造抛异常
        assert_eq!(adapter_version_lower_than_min("abc", "1.3.0"), None);
        assert_eq!(adapter_version_lower_than_min("", "1.3.0"), None);
    }

    #[test]
    fn peer_id_hex_decode_follows_byte_util() {
        assert_eq!(
            decode_peer_id_hex("2d5452313030302d6162636465666768696a"),
            "-TR1000-abcdefghij"
        );
        // 奇数长度：前置补 0（`ByteUtil.hexToByteArray`）
        assert_eq!(decode_peer_id_hex("2d54523"), "\u{2}\u{d5}E#");
        assert_eq!(decode_peer_id_hex(""), "");
        // 非法十六进制：退回已解码前缀
        assert_eq!(decode_peer_id_hex("2d5g"), "-");
    }
}
