//! BitComet 适配器，忠实复刻上游 `downloader/impl/bitcomet/BitComet`（要求 BitComet >= 2.18）。
//!
//! 要点：
//! - 会话：除登录外的业务请求都带 `Authorization: Bearer <deviceToken>`；上游 OkHttp interceptor
//!   还会给**每个**请求补 `Content-Type: application/json`、`Client-Type: BitComet WebUI`、
//!   `User-Agent: PeerBanHelper BitComet Adapter`（见 [`BitCometDownloader::with_common_headers`]）。
//! - 登录两步：`POST /api/webui/login`（AES 加密凭据，见 [`crypto`]）→ 版本门槛（>= 2.18）
//!   → `POST /api/device_token/get`（用 `invite_token` 换取 `device_token` 与服务端版本）。
//!   若 `GET /api/config/ipfilter/get` 显示 IP 过滤器未启用或不是 `blacklist` 模式，登录时自动改为
//!   「启用 + 黑名单」（对齐 `enableIpFilter`）。注意上游 `isLoggedIn()` 只看该探测请求是否成功，
//!   **不检查**返回值本身。
//! - 种子：`POST /api_v2/task_list/get`（`state_group=ACTIVE`）取任务列表，再对每个 `type == "BT"`
//!   的任务 `POST /api/task/summary/get` 取详情；概要请求失败只记日志并跳过该任务。
//! - peers：`POST /api/task/peers/get`（`groups=["peers_connected","ltseeds_connected"]`、
//!   `max_count=0` 取全量）。BitComet 只上报 `ip[:port]` 文本，故先按 `HostAndPort` 规则拆解
//!   （无端口时回退 `remote_port`），再走 `addressTranslate`（Teredo / NAT64 / IPv4-mapped）；
//!   `PeerData.raw_ip` 保存**未翻译**的下载器原始 `ip:port`。上游给 `PeerImpl` 传的 `flags`
//!   为 `null`，故本实现 `flags = None`；`handshaking` 由 `dl_rate <= 0 && up_rate <= 0` 推导，
//!   与 `PeerData::is_handshaking()` 定义一致。
//! - 封禁：BitComet 的 ipfilter 是「文件导入」语义，非显而易见的 API——
//!   `POST /api/config/ipfilter/upload` + `{"import_type": ..., "data_type": "data_file",
//!   "content_base64": Base64(地址按 "\n" 连接)}`。
//!   * 全量替换（`replace_banned_ips` → 上游 `setBanListFull`）：`import_type = "replace"`，
//!     地址逐个走 `remapBanListAddress`（BitComet 声明 `RANGE_BAN_IP` → `supportRangeBan = true`）。
//!   * 增量新增（`ban_peers` → 上游 ≤ v9.0.0 的 `setBanListIncrement`）：同一接口的
//!     `import_type = "merge"`，地址去重后按 `"\n"` 连接。
//!     注意：上游 v9.5.1 的 BitComet **已删除**增量分支——`setBanListIncrement` 原本就被
//!     `is211Newer()`（BitComet >= 2.11 不支持 merge 导入）挡住，而登录门槛是 2.18，故该分支不可达；
//!     因此 `main.rs` 给 BitComet 固定 `increment_ban = false`，生产路径恒为整份替换。
//! - 特性标志（顺序同 `getFeatureFlags()`）：`UNBAN_IP` / `LIVE_UPDATE_BT_PROTOCOL_PORT` /
//!   [`TRAFFIC_STATS`（服务端 >= 2.20）] / `RANGE_BAN_IP`。
//! - 统计：`POST /api/statistics_list/get`；BitComet 2.21 起令牌去掉前导 `$` 且 2.20 起返回字节数
//!   （更早的版本返回 `"2.33 GB"` 这类人类可读值，由 [`read_human_readable_value`] 换算）。
//!   失败时对齐上游：`warn` 后抛出（本实现返回 `Err`）。
//!
//! 不在本阶段接口内、因此未复刻的上游能力：
//! - `getTrackers` / `setTrackers`、`getSpeedLimiter` / `setSpeedLimiter`、
//!   `getBTProtocolPort` / `setBTProtocolPort`、`getAllTorrents`、`saveDownloader`：Rust
//!   `Downloader` 特性集里没有对应方法。
//! - `AbstractDownloader.login()` 的告警/退避机制（`AlertManager`、Sentry、
//!   `failedLoginAttempts` 达 15 次后 30 分钟冷却）与 `getMaxConcurrentPeerRequestSlots() = 4`
//!   （并发槽由 ban wave 的全局信号量承担）。
//! - `setBanList` 中对 `removed` 的解封调用（`POST /api/task/peers/unban_peers`）：trait 无解封入口，
//!   保留为公有方法 [`BitCometDownloader::unban_peers`]（语义已由整份替换覆盖）。

pub mod crypto;
pub mod dto;

use crate::http::{BoxFuture, HttpFetcher, HttpRequest, HttpResponse, ReqwestFetcher};
use crate::{
    BanEntry, Downloader, DownloaderFeature, DownloaderStatistics, LoginResult, LoginStatus,
};
use crypto::CLIENT_ID;
use dto::*;
use pbh_core::defaults::qb as qbcfg;
use pbh_core::i18n::{Param, TranslationComponent, Translator};
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::remap::{remap_ban_list_address, translate_peer_ip, RemapConfig};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{error, info, warn};

/// 登录时要求的最低 BitComet 版本（上游 `DOWNLOADER_BC_VERSION_UNACCEPTABLE` 的参数）。
const MIN_VERSION: &str = "2.18";
/// `BitComet.peerGroupsFilter`：只取「已连接」与「长连接做种」两组 peer。
const PEER_GROUPS_FILTER: [&str; 2] = ["peers_connected", "ltseeds_connected"];
/// `setBanListFull` 使用的导入模式（`operateBanListLegacy("replace", ...)`）。
const IMPORT_REPLACE: &str = "replace";
/// `setBanListIncrement` 使用的导入模式（上游 ≤ v9.0.0）。
const IMPORT_MERGE: &str = "merge";
/// `operateBanListLegacy` 的 `data_type`。
const DATA_TYPE_FILE: &str = "data_file";
/// `getStatistics()` 使用 `>= 2.20` 的字节数令牌；`>= 2.21` 起令牌去掉前导 `$`。
const TRAFFIC_STATS_VERSION: (u64, u64) = (2, 20);
const TOKEN_WITHOUT_DOLLAR_VERSION: (u64, u64) = (2, 21);
/// `queryNeedReConfigureIpFilter` 判定的黑名单模式值。
const IPFILTER_MODE_BLACKLIST: &str = "blacklist";
/// `unbanPeers` 的解封范围。
const UNBAN_RANGE_ALL_TASKS: &str = "unban_peers_in_all_tasks";
/// `getDeviceToken` 请求里的设备名（上游字面量）。
const DEVICE_NAME: &str = "PeerBanHelper - BitComet Adapter";

const MSG_PAUSED: &str = "DOWNLOADER_PAUSED";
const MSG_STATUS_OK: &str = "STATUS_TEXT_OK";
const MSG_LOGIN_EXCEPTION: &str = "DOWNLOADER_LOGIN_EXCEPTION";
const MSG_LOGIN_INCORRECT_CRED: &str = "DOWNLOADER_LOGIN_INCORRECT_CRED";
const MSG_LOGIN_IO_EXCEPTION: &str = "DOWNLOADER_LOGIN_IO_EXCEPTION";
const MSG_VERSION_UNACCEPTABLE: &str = "DOWNLOADER_BC_VERSION_UNACCEPTABLE";
const MSG_FAILED_TORRENT_LIST: &str = "DOWNLOADER_BC_FAILED_REQUEST_TORRENT_LIST";
const MSG_UNABLE_FETCH_TASK_SUMMARY: &str = "DOWNLOADER_BITCOMET_UNABLE_FETCH_TASK_SUMMARY";
const MSG_FAILED_PEERS_LIST: &str = "DOWNLOADER_BC_FAILED_REQUEST_PEERS_LIST_IN_TORRENT";
const MSG_FAILED_SAVE_BANLIST: &str = "DOWNLOADER_BC_FAILED_SAVE_BANLIST";
const MSG_FAILED_STATISTICS_LIST: &str = "DOWNLOADER_BC_FAILED_REQUEST_STATISTICS_LIST";
const MSG_IP_FILTER_SUCCESS: &str = "DOWNLOADER_BC_CONFIG_IP_FILTER_SUCCESS";
const MSG_IP_FILTER_FAILED: &str = "DOWNLOADER_BC_CONFIG_IP_FILTER_FAILED";

/// 对齐上游 `tlUI`：下载器不持有 locale，使用服务端默认文案语言。
const UI_LOCALE: &str = "zh_cn";

/// BitComet 下载器配置（对齐 `BitComet.Config`）。
#[derive(Clone, Debug)]
pub struct BitCometConfig {
    pub id: String,
    pub name: String,
    /// WebUI 地址（上游 `Config.readFromYaml` 无默认值，必填）
    pub endpoint: String,
    /// WebUI 用户名
    pub username: String,
    /// WebUI 密码
    pub password: String,
    /// 是否使用增量封禁（上游 `increment-ban`，默认 true；见模块文档：v9.5.1 恒走整份替换）
    pub increment_ban: bool,
    /// 是否校验 TLS 证书（上游 `verify-ssl`，默认 true）
    pub verify_ssl: bool,
    /// 是否忽略私有种子（上游 `ignore-private`，**默认 false**：默认包含私有种子）
    pub ignore_private: bool,
    /// 启动即暂停（上游 `paused`，默认 false）
    pub paused: bool,
    /// `config.yml` 的 `banlist-remapping` / `ip-remapping` 配置
    pub remap: RemapConfig,
}

impl Default for BitCometConfig {
    fn default() -> Self {
        Self {
            id: "bitcomet".into(),
            name: "BitComet".into(),
            // 上游 `Config.readFromYaml` 未给 endpoint 默认值（端口由用户在 BitComet 侧配置）
            endpoint: String::new(),
            username: String::new(),
            password: String::new(),
            increment_ban: true,
            verify_ssl: true,
            ignore_private: false,
            paused: false,
            remap: RemapConfig::default(),
        }
    }
}

pub struct BitCometDownloader {
    config: BitCometConfig,
    /// 去掉一个结尾 `/` 后的 API 前缀（对齐 `Config.readFromYaml`）
    endpoint: String,
    http: Arc<dyn HttpFetcher>,
    /// `GET_DEVICE_TOKEN` 换取的设备令牌
    device_token: Mutex<String>,
    /// 登录后协商到的服务端版本（决定能力标志与统计令牌）
    server_version: Mutex<String>,
    /// `TorrentData` 只保留 infohash，而 peers 查询需要 BitComet 的数字 `task_id`：
    /// `fetch_torrents` 期间记录映射（对齐上游 `torrent.getId()` 即 task_id）。
    task_ids: Mutex<HashMap<String, String>>,
}

/// `POST /api/webui/login` 的请求体（对齐 `loginJsonObject`）。
#[derive(Serialize)]
struct LoginRequest {
    authentication: String,
    client_id: String,
}

/// `POST /api/webui/login` 之前先序列化的凭据明文（对齐 `loginAttemptCred`）。
#[derive(Serialize)]
struct LoginAttemptCred<'a> {
    username: &'a str,
    password: &'a str,
}

/// `POST /api/device_token/get` 的请求体（对齐 `inviteTokenRetrievePayload`）。
#[derive(Serialize)]
struct DeviceTokenRequest<'a> {
    device_id: &'a str,
    device_name: &'a str,
    invite_token: &'a str,
    platform: &'a str,
}

/// `IP_FILTER_UPLOAD` 的请求体（对齐 `operateBanListLegacy`）。
#[derive(Serialize)]
struct BanListImport<'a> {
    import_type: &'a str,
    data_type: &'a str,
    content_base64: String,
}

/// `TASK_UNBAN_PEERS` 的请求体（对齐 `unbanPeers`）。
#[derive(Serialize)]
struct UnbanPeersRequest<'a> {
    ip_list: &'a [String],
    unban_range: &'a str,
}

impl BitCometDownloader {
    pub fn new(config: BitCometConfig) -> anyhow::Result<Self> {
        let fetcher = Arc::new(ReqwestFetcher::new(
            config.verify_ssl,
            qbcfg::CONNECT_TIMEOUT_SECS,
            qbcfg::READ_TIMEOUT_SECS,
        )?);
        Self::with_fetcher(config, fetcher)
    }

    pub fn with_fetcher(
        config: BitCometConfig,
        http: Arc<dyn HttpFetcher>,
    ) -> anyhow::Result<Self> {
        // 对齐 `Config.readFromYaml`：`if (endpoint.endsWith("/")) substring(0, length - 1)`，
        // 即只去掉**一个**结尾斜杠（浏览器复制地址的 workaround）。
        let endpoint = match config.endpoint.strip_suffix('/') {
            Some(stripped) => stripped.to_string(),
            None => config.endpoint.clone(),
        };
        Ok(Self {
            config,
            endpoint,
            http,
            device_token: Mutex::new(String::new()),
            server_version: Mutex::new(String::new()),
            task_ids: Mutex::new(HashMap::new()),
        })
    }

    /// 当前设备令牌（未登录时为空串，等价 Java 的 `null` → `Bearer null`）。
    fn device_token(&self) -> String {
        self.device_token
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default()
    }

    /// 当前协商到的服务端版本（未登录时为空串）。
    pub fn server_version(&self) -> String {
        self.server_version
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    /// 对齐 `serverVersion.isGreaterThanOrEqualTo(...)`（未登录时上游会 NPE，此处按 false）。
    fn server_version_at_least(&self, major: u64, minor: u64) -> bool {
        version_at_least(&self.server_version(), major, minor, 0)
    }

    /// 上游 OkHttp interceptor 的等价物：给每个请求补三个固定头。
    ///
    /// `Content-Type` 由 [`HttpRequest::post_json`] 设置，这里再补 `Client-Type` 与 `User-Agent`。
    fn with_common_headers(req: HttpRequest) -> HttpRequest {
        req.with_header("Content-Type", "application/json")
            .with_header("Client-Type", "BitComet WebUI")
            .with_header("User-Agent", "PeerBanHelper BitComet Adapter")
    }

    /// 已登录请求：`Authorization: Bearer <deviceToken>`。
    async fn post_authed(&self, path: &str, body: String) -> anyhow::Result<HttpResponse> {
        let url = format!("{}{}", self.endpoint, path);
        let token = self.device_token();
        let req = Self::with_common_headers(HttpRequest::post_json(url, body))
            .with_header("Authorization", &format!("Bearer {token}"));
        self.http.execute(req).await
    }

    /// 未认证请求（仅 `USER_LOGIN` 使用；上游不给该请求加 `Authorization`）。
    async fn post_plain(&self, path: &str, body: String) -> anyhow::Result<HttpResponse> {
        let url = format!("{}{}", self.endpoint, path);
        self.http
            .execute(Self::with_common_headers(HttpRequest::post_json(url, body)))
            .await
    }

    /// 指定令牌的请求（`GET_DEVICE_TOKEN` 用 `invite_token`）。
    async fn post_with_token(
        &self,
        path: &str,
        body: String,
        token: &str,
    ) -> anyhow::Result<HttpResponse> {
        let url = format!("{}{}", self.endpoint, path);
        let req = Self::with_common_headers(HttpRequest::post_json(url, body))
            .with_header("Authorization", &format!("Bearer {token}"));
        self.http.execute(req).await
    }

    /// 对齐 `queryNeedReConfigureIpFilter()`：返回是否需要修正 IP 过滤器配置。
    ///
    /// 非 2xx 时对齐上游抛 `IllegalStateException`；`ip_filter_config` / `enable_ipfilter`
    /// 缺失在 Java 里是 NPE（Gson 允许字段为 null），这里同样按错误处理。
    async fn query_need_reconfigure_ip_filter(&self) -> anyhow::Result<bool> {
        let resp = self
            .post_authed(GET_IP_FILTER_CONFIG, "{}".to_string())
            .await?;
        if !is_success(resp.status) {
            anyhow::bail!("Not a excepted statusCode while query the IPFilter status");
        }
        let parsed: BCIpFilterResponse = serde_json::from_str(&resp.body)?;
        let config = parsed
            .ip_filter_config
            .ok_or_else(|| anyhow::anyhow!("NullPointerException: ip_filter_config is null"))?;
        let enabled = config
            .enable_ipfilter
            .ok_or_else(|| anyhow::anyhow!("NullPointerException: enable_ipfilter is null"))?;
        // `"blacklist".equals(resp.getIpFilterConfig().getFilterMode())`：filterMode 为 null 即非黑名单
        let is_blacklist_mode = config.ipfilter_mode.as_deref() == Some(IPFILTER_MODE_BLACKLIST);
        Ok(!enabled || !is_blacklist_mode)
    }

    /// 对齐 `enableIpFilter()`：把 IP 过滤器改成「启用 + 黑名单」。
    async fn enable_ip_filter(&self) -> anyhow::Result<()> {
        // Gson 序列化 HashMap 的顺序不确定，这里按上游键名固定输出
        let body = serde_json::json!({
            "ip_filter_config": {
                "enable_ipfilter": true,
                "ipfilter_mode": IPFILTER_MODE_BLACKLIST,
            }
        })
        .to_string();
        let resp = self.post_authed(SET_IP_FILTER_CONFIG, body).await?;
        if !is_success(resp.status) {
            error!("{}", tl(MSG_IP_FILTER_FAILED, Vec::new()));
            return Ok(());
        }
        let parsed: BCConfigSetResponse = serde_json::from_str(&resp.body)?;
        if parsed
            .error_code
            .as_deref()
            .is_some_and(|code| code.eq_ignore_ascii_case("ok"))
        {
            info!("{}", tl(MSG_IP_FILTER_SUCCESS, Vec::new()));
        } else {
            error!("{}", tl(MSG_IP_FILTER_FAILED, Vec::new()));
        }
        Ok(())
    }

    /// 对齐 `isLoggedIn()`：探测 IP 过滤器接口，只关心是否抛异常（不关心返回值）。
    async fn is_logged_in(&self) -> bool {
        self.query_need_reconfigure_ip_filter().await.is_ok()
    }

    /// `fetch_peers` 需要的数字 `task_id`（对齐 `torrent.getId()`）。
    fn task_id_of(&self, torrent: &TorrentData) -> String {
        self.task_ids
            .lock()
            .ok()
            .and_then(|map| map.get(&torrent.hash).cloned())
            .unwrap_or_else(|| torrent.id().to_string())
    }

    /// 对齐 `fetchTorrents` 中把 `BCTaskTorrentResponse` 映射为 `TorrentImpl` 的部分。
    fn torrent_from(&self, response: &BCTaskTorrentResponse) -> TorrentData {
        let detail = response.task_detail.clone().unwrap_or_default();
        let status = response.task_status.clone().unwrap_or_default();
        let task = response.task.clone().unwrap_or_default();
        TorrentData {
            // `infohash != null ? infohash : infohashV2`
            hash: detail
                .infohash
                .clone()
                .or_else(|| detail.infohash_v2.clone())
                .unwrap_or_default(),
            // 名称取 `task.task_name`（而非 task_detail.task_name）
            name: task.task_name.clone().unwrap_or_default(),
            // `taskStatus.downloadPermillage / 1000.0d`
            progress: status.download_permillage as f64 / 1000.0,
            total_size: detail.total_size,
            // BitComet 不提供 piece 信息，完成量取 `task.selectedDownloadedSize`
            piece_size: 0,
            pieces_have: 0,
            completed_override: Some(task.selected_downloaded_size),
            dlspeed: task.download_rate,
            upspeed: task.upload_rate,
            // 上游把 `Boolean torrent_private` 拆箱为 `boolean`（null 会 NPE），这里按 false
            is_private: Some(detail.torrent_private.unwrap_or(false)),
        }
    }

    /// 对齐 `getPeers(torrent)` 的映射部分。
    fn peers_from(&self, response: &BCTaskPeersResponse) -> Vec<PeerData> {
        // 上游：`peers == null` 时返回空列表
        let Some(peers) = response.peers.as_deref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for peer in peers {
            // `peerGroupsFilter.contains(dto.getGroup())`：group 缺失或不在集合内都被丢弃
            if !PEER_GROUPS_FILTER.contains(&peer.group.as_deref().unwrap_or_default()) {
                continue;
            }
            let remote_port = peer.remote_port.clamp(0, u16::MAX as i32) as u16;
            let (raw_ip, raw_port) =
                parse_address(peer.ip.as_deref().unwrap_or_default(), remote_port);
            // 对齐 `addressTranslate(new PeerAddress(ip, port, ip))`
            let (ip, port) = translate_peer_ip(&raw_ip, raw_port, &self.config.remap.ip_remapping);
            out.push(PeerData {
                client_name: peer.client_type.clone(),
                // `ByteUtil.hexToByteArray(peerId)` + `new String(bytes, ISO_8859_1)`
                peer_id: Some(decode_peer_id_hex(
                    peer.peer_id.as_deref().unwrap_or_default(),
                )),
                dl_speed: peer.dl_rate,
                // `dl_size` / `up_size` 可能为 null（上游旧版按 -1 兼容 BitComet 2.10）
                downloaded: peer.dl_size.unwrap_or(-1),
                up_speed: peer.up_rate,
                uploaded: peer.up_size.unwrap_or(-1),
                progress: peer.permillage as f64 / 1000.0,
                // 上游给 `PeerImpl` 传的 `PeerFlag` 为 null
                flags: None,
                ip,
                port,
                raw_ip: format!("{raw_ip}:{raw_port}"),
                // 上游 `PeerImpl` 无连接类型字段
                connection: None,
            });
        }
        out
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

    /// 对齐 `operateBanListLegacy(mode, content)`：把地址列表作为「数据文件」Base64 上传。
    async fn operate_ban_list(&self, mode: &str, content: &str) -> anyhow::Result<()> {
        use base64::Engine as _;
        let body = serde_json::to_string(&BanListImport {
            import_type: mode,
            data_type: DATA_TYPE_FILE,
            content_base64: base64::engine::general_purpose::STANDARD.encode(content.as_bytes()),
        })?;
        let resp = match self.post_authed(IP_FILTER_UPLOAD, body).await {
            Ok(resp) => resp,
            Err(e) => {
                let (class, message) = exception_params(&e);
                error!(
                    "{}",
                    tl(
                        MSG_FAILED_SAVE_BANLIST,
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
                    MSG_FAILED_SAVE_BANLIST,
                    vec![
                        Param::Text(self.config.name.clone()),
                        Param::Text(self.endpoint.clone()),
                        Param::Text(resp.status.to_string()),
                        Param::Text("HTTP ERROR".to_string()),
                        Param::Text(resp.body),
                    ]
                )
            );
            anyhow::bail!("Save BitComet banlist error: statusCode={}", resp.status);
        }
        Ok(())
    }

    /// 对齐 `unbanPeers(peerAddresses)`：解封给定地址列表。
    ///
    /// 上游 `setBanList` 在 `removed` 非空时调用它，随后仍会整份替换封禁列表；
    /// Rust `Downloader` trait 没有解封入口，故保留为公有方法。
    ///
    /// 注意：上游传入的是 `meta.getPeer().getAddress().toString()`（Lombok 生成的
    /// `PeerAddress(...)` 文本）；本实现由调用方给出 `ip[:port]` 文本。
    pub async fn unban_peers(&self, peer_addresses: &[String]) -> anyhow::Result<()> {
        let body = serde_json::to_string(&UnbanPeersRequest {
            ip_list: peer_addresses,
            unban_range: UNBAN_RANGE_ALL_TASKS,
        })?;
        let resp = match self.post_authed(TASK_UNBAN_PEERS, body).await {
            Ok(resp) => resp,
            Err(e) => {
                let (class, message) = exception_params(&e);
                error!(
                    "{}",
                    tl(
                        MSG_FAILED_SAVE_BANLIST,
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
                    MSG_FAILED_SAVE_BANLIST,
                    vec![
                        Param::Text(self.config.name.clone()),
                        Param::Text(self.endpoint.clone()),
                        Param::Text(resp.status.to_string()),
                        Param::Text("HTTP ERROR (unban_peers)".to_string()),
                        Param::Text(resp.body),
                    ]
                )
            );
            anyhow::bail!("Save BitComet banlist error: statusCode={}", resp.status);
        }
        Ok(())
    }
}

impl Downloader for BitCometDownloader {
    fn id(&self) -> &str {
        &self.config.id
    }
    fn name(&self) -> &str {
        &self.config.name
    }
    fn downloader_type(&self) -> &'static str {
        "bitcomet"
    }

    /// 对齐 `BitComet.getFeatureFlags()`：集合与顺序均与上游一致。
    fn feature_flags(&self) -> Vec<String> {
        let mut flags = vec![
            DownloaderFeature::UnbanIp.name().to_string(),
            DownloaderFeature::LiveUpdateBtProtocolPort
                .name()
                .to_string(),
        ];
        // `serverVersion.isGreaterThanOrEqualTo("2.20")`（未登录时上游 NPE，这里视为不满足）
        if self.server_version_at_least(TRAFFIC_STATS_VERSION.0, TRAFFIC_STATS_VERSION.1) {
            flags.push(DownloaderFeature::TrafficStats.name().to_string());
        }
        flags.push(DownloaderFeature::RangeBanIp.name().to_string());
        flags
    }

    /// 对齐 `login0()`（外层 `AbstractDownloader.login()` 的暂停短路一并复刻）。
    fn login<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<LoginResult>> {
        Box::pin(async move {
            if self.config.paused {
                return Ok(LoginResult::paused(
                    tl(MSG_PAUSED, Vec::new()),
                    self.server_version(),
                ));
            }
            // `if (isLoggedIn()) return SUCCESS; // 重用 Session 会话`
            if self.is_logged_in().await {
                return Ok(LoginResult::success(
                    tl(MSG_STATUS_OK, Vec::new()),
                    self.server_version(),
                ));
            }

            // 1) AES 加密凭据并登录
            let login_attempt = serde_json::to_string(&LoginAttemptCred {
                username: &self.config.username,
                password: &self.config.password,
            })?;
            let login_request = serde_json::to_string(&LoginRequest {
                authentication: crypto::credential(&login_attempt, CLIENT_ID),
                client_id: CLIENT_ID.to_string(),
            })?;
            let login_resp = match self.post_plain(USER_LOGIN, login_request).await {
                Ok(resp) => resp,
                Err(e) => return Ok(login_io_exception(&e)),
            };
            if !is_success(login_resp.status) {
                return Ok(LoginResult::exception(tl(
                    MSG_LOGIN_EXCEPTION,
                    vec![Param::Text(format!(
                        "{} {}",
                        login_resp.status, login_resp.body
                    ))],
                )));
            }
            let login_response: BCLoginResponse = match serde_json::from_str(&login_resp.body) {
                Ok(parsed) => parsed,
                Err(e) => return Ok(login_io_exception(&anyhow::Error::from(e))),
            };
            let error_code = login_response.error_code.clone().unwrap_or_default();
            if error_code.eq_ignore_ascii_case("PASSWORD_ERROR") {
                return Ok(LoginResult::incorrect_credential(tl(
                    MSG_LOGIN_INCORRECT_CRED,
                    // 上游把响应对象本身作为占位参数（Gson/Lombok toString）
                    vec![Param::Text(login_response.to_string())],
                )));
            }
            if !error_code.eq_ignore_ascii_case("ok") {
                return Ok(LoginResult::exception(tl(
                    MSG_LOGIN_EXCEPTION,
                    vec![Param::Text(login_response.to_string())],
                )));
            }

            // 2) 版本检查：major > 2 或 (major == 2 && minor >= 18)
            let raw_version = login_response.version.clone().unwrap_or_default();
            let Some((major, minor, _)) = parse_loose_semver(&raw_version) else {
                // 上游 `new Semver(...)` 抛 SemverException（null 则为 NPE），均落入外层 catch
                return Ok(LoginResult::exception(tl(
                    MSG_LOGIN_IO_EXCEPTION,
                    vec![Param::Text(format!("SemverException: {raw_version}"))],
                )));
            };
            if !(major > 2 || (major == 2 && minor >= 18)) {
                // 上游版本不达标 → MISSING_COMPONENTS（不进入冷却）
                return Ok(LoginResult {
                    success: false,
                    status: LoginStatus::MissingComponents,
                    message: tl(
                        MSG_VERSION_UNACCEPTABLE,
                        vec![Param::Text(MIN_VERSION.to_string())],
                    ),
                    version: raw_version,
                });
            }

            // 3) 用 invite_token 换 device_token
            let invite_token = login_response.invite_token.clone().unwrap_or_default();
            let device_request = serde_json::to_string(&DeviceTokenRequest {
                device_id: CLIENT_ID,
                device_name: DEVICE_NAME,
                invite_token: &invite_token,
                platform: "webui",
            })?;
            // 该请求的令牌是 invite_token（不是 device_token）
            let device_resp = match self
                .post_with_token(GET_DEVICE_TOKEN, device_request, &invite_token)
                .await
            {
                Ok(resp) => resp,
                Err(e) => return Ok(login_io_exception(&e)),
            };
            if !is_success(device_resp.status) {
                return Ok(LoginResult::exception(tl(
                    MSG_LOGIN_EXCEPTION,
                    vec![Param::Text(format!(
                        "{} {}",
                        device_resp.status, device_resp.body
                    ))],
                )));
            }
            let device_token_result: BCDeviceTokenResult =
                match serde_json::from_str(&device_resp.body) {
                    Ok(parsed) => parsed,
                    Err(e) => return Ok(login_io_exception(&anyhow::Error::from(e))),
                };
            if let Ok(mut token) = self.device_token.lock() {
                *token = device_token_result.device_token.clone().unwrap_or_default();
            }
            let server_version = device_token_result.version.clone().unwrap_or_default();
            if let Ok(mut version) = self.server_version.lock() {
                *version = server_version.clone();
            }

            // 4) IP 过滤器配置自检（对齐 `if (queryNeedReConfigureIpFilter()) enableIpFilter();`）
            match self.query_need_reconfigure_ip_filter().await {
                Ok(true) => {
                    if let Err(e) = self.enable_ip_filter().await {
                        return Ok(login_io_exception(&e));
                    }
                }
                Ok(false) => {}
                Err(e) => return Ok(login_io_exception(&e)),
            }

            Ok(LoginResult::success(
                tl(MSG_STATUS_OK, Vec::new()),
                server_version,
            ))
        })
    }

    /// 对齐 `getTorrents()` + `fetchTorrents(requirements, includePrivate)`。
    fn fetch_torrents<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Vec<TorrentData>>> {
        Box::pin(async move {
            // 对齐 `getTorrents()` 的 requirements；`limit = Integer.MAX_VALUE - 1`
            let requirements = serde_json::json!({
                "state_group": "ACTIVE",
                "sort_key": "",
                "sort_order": "unsorted",
                "tag_filter": "ALL",
                "task_type": "ALL",
                "start": 0,
                "limit": i32::MAX - 1,
            });
            let resp = self
                .post_authed(GET_TASK_LIST, requirements.to_string())
                .await?;
            if !is_success(resp.status) {
                anyhow::bail!(
                    "{}",
                    tl(
                        MSG_FAILED_TORRENT_LIST,
                        vec![
                            Param::Text(resp.status.to_string()),
                            Param::Text(resp.body.clone()),
                        ]
                    )
                );
            }
            // `tasks` 缺失在 Java 里是 NPE（无 catch），这里按空列表处理
            let task_list: BCTaskListResponse =
                serde_json::from_str(&resp.body).unwrap_or_default();
            // `includePrivate = !config.isIgnorePrivate()`
            let include_private = !self.config.ignore_private;
            let mut task_ids = HashMap::new();
            let mut out = Vec::new();
            for task in task_list
                .tasks
                .iter()
                .filter(|t| t.kind.as_deref() == Some("BT"))
            {
                let task_id = task.task_id.to_string();
                // `taskIds.put("task_id", String.valueOf(torrent.getTaskId()))`
                let body = serde_json::json!({ "task_id": task_id }).to_string();
                let resp = match self.post_authed(GET_TASK_SUMMARY, body).await {
                    Ok(resp) => resp,
                    Err(e) => {
                        // 上游只记日志并跳过该任务（`DOWNLOADER_BITCOMET_UNABLE_FETCH_TASK_SUMMARY`）
                        warn!("{}: {e}", tl(MSG_UNABLE_FETCH_TASK_SUMMARY, Vec::new()));
                        continue;
                    }
                };
                if !is_success(resp.status) {
                    warn!("{}", tl(MSG_UNABLE_FETCH_TASK_SUMMARY, Vec::new()));
                    continue;
                }
                let summary: BCTaskTorrentResponse = match serde_json::from_str(&resp.body) {
                    Ok(summary) => summary,
                    Err(e) => {
                        warn!("{}: {e}", tl(MSG_UNABLE_FETCH_TASK_SUMMARY, Vec::new()));
                        continue;
                    }
                };
                let torrent = self.torrent_from(&summary);
                if !include_private && torrent.is_private() {
                    continue;
                }
                task_ids.insert(torrent.hash.clone(), task_id);
                out.push(torrent);
            }
            if let Ok(mut cache) = self.task_ids.lock() {
                *cache = task_ids;
            }
            Ok(out)
        })
    }

    /// 对齐 `getPeers(torrent)`：`groups` 为固定的两组、`max_count = 0` 取全量。
    fn fetch_peers<'a>(
        &'a self,
        torrent: &'a TorrentData,
    ) -> BoxFuture<'a, anyhow::Result<Vec<PeerData>>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "groups": PEER_GROUPS_FILTER,
                "task_id": self.task_id_of(torrent),
                "max_count": 0,
            })
            .to_string();
            let resp = self.post_authed(GET_TASK_PEERS, body).await?;
            if !is_success(resp.status) {
                anyhow::bail!(
                    "{}",
                    tl(
                        MSG_FAILED_PEERS_LIST,
                        vec![
                            Param::Text(resp.status.to_string()),
                            Param::Text(resp.body.clone()),
                        ]
                    )
                );
            }
            let peers: BCTaskPeersResponse = serde_json::from_str(&resp.body)?;
            Ok(self.peers_from(&peers))
        })
    }

    /// 增量新增：对齐上游 ≤ v9.0.0 的 `setBanListIncrement`（`import_type = "merge"`）。
    ///
    /// 注意上游 v9.5.1 的 BitComet 已删除该分支（BitComet >= 2.11 不支持 merge 导入，
    /// 而登录门槛为 2.18），因此生产路径由 `main.rs` 固定为整份替换。
    fn ban_peers<'a>(&'a self, peers: &'a [BanEntry]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            if peers.is_empty() {
                return Ok(());
            }
            let ips = self.remap_ips(peers.iter().map(|p| p.ip.clone()));
            self.operate_ban_list(IMPORT_MERGE, &ips.join("\n")).await
        })
    }

    /// 整份替换：对齐 `setBanListFull`（`import_type = "replace"`）。
    fn replace_banned_ips<'a>(&'a self, ips: &'a [String]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            // 上游 `setBanListFull` 不做去重：按入参顺序 `flatMap` 后以 `\n` 连接
            let mut joined = Vec::new();
            for ip in ips {
                joined.extend(remap_ban_list_address(ip, true, &self.config.remap));
            }
            self.operate_ban_list(IMPORT_REPLACE, &joined.join("\n"))
                .await
        })
    }

    /// 对齐 `BitComet.getSpeedLimiter()`：`POST /api/config/connection_config/get`（单位 **bytes/s**）。
    /// 上游异常时返回 `null` ⇒ 调用方跳过；这里返回 `Err` 等价。
    fn get_speed_limiter<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<(i64, i64)>> {
        Box::pin(async move {
            let resp = match self
                .post_authed(GET_CONNECTION_CONFIG, "{}".to_string())
                .await
            {
                Ok(resp) => resp,
                Err(e) => return Err(e),
            };
            if !is_success(resp.status) {
                anyhow::bail!(
                    "BitComet getSpeedLimiter failed with status {}",
                    resp.status
                );
            }
            let parsed: BCConnectionConfigResponse = serde_json::from_str(&resp.body)
                .map_err(|e| anyhow::anyhow!("BitComet getSpeedLimiter parse error: {e}"))?;
            if parsed
                .error_code
                .as_deref()
                .is_some_and(|c| !c.eq_ignore_ascii_case("ok"))
            {
                anyhow::bail!(
                    "BitComet getSpeedLimiter error: {}",
                    parsed.error_message.unwrap_or_default()
                );
            }
            let cfg = parsed.connection_config.unwrap_or_default();
            Ok((
                cfg.max_upload_speed.unwrap_or(0),
                cfg.max_download_speed.unwrap_or(0),
            ))
        })
    }

    /// 对齐 `BitComet.setSpeedLimiter(...)`：`POST /api/config/connection_config/set`，
    /// 负载 `{"connection_config": {"max_upload_speed": ..., "max_download_speed": ...}}`（0 = 不限制）。
    fn set_speed_limiter<'a>(
        &'a self,
        upload: i64,
        download: i64,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let body = serde_json::json!({
                "connection_config": {
                    "max_upload_speed": if upload <= 0 { 0 } else { upload },
                    "max_download_speed": if download <= 0 { 0 } else { download },
                }
            })
            .to_string();
            let resp = self.post_authed(SET_CONNECTION_CONFIG, body).await?;
            if !is_success(resp.status) {
                anyhow::bail!(
                    "BitComet setSpeedLimiter failed with status {}",
                    resp.status
                );
            }
            let parsed: BCConfigSetResponse = serde_json::from_str(&resp.body)
                .map_err(|e| anyhow::anyhow!("BitComet setSpeedLimiter parse error: {e}"))?;
            if parsed
                .error_code
                .as_deref()
                .is_some_and(|c| !c.eq_ignore_ascii_case("ok"))
            {
                anyhow::bail!(
                    "BitComet setSpeedLimiter error: {}",
                    parsed.error_message.unwrap_or_default()
                );
            }
            Ok(())
        })
    }

    /// 对齐 `BitComet.getStatistics()`；失败时对齐上游：`warn` 后抛出。
    fn statistics<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<DownloaderStatistics>> {
        Box::pin(async move {
            match self.fetch_statistics().await {
                Ok(stats) => Ok(stats),
                Err(e) => {
                    // `log.warn("Failed to fetch BitComet statistics", e)`（上游用英文固定串）
                    warn!("Failed to fetch BitComet statistics: {e}");
                    // `throw new IllegalStateException(tlUI(..., "N/A", "N/A", e))`：
                    // 上游把内层异常对象作为第三个占位参数，这里按其 toString 语义拼出类名
                    Err(anyhow::anyhow!(
                        "{}",
                        tl(
                            MSG_FAILED_STATISTICS_LIST,
                            vec![
                                Param::Text("N/A".to_string()),
                                Param::Text("N/A".to_string()),
                                Param::Text(format!("anyhow::Error: {e}")),
                            ]
                        )
                    ))
                }
            }
        })
    }
}

impl BitCometDownloader {
    /// `getStatistics()` 的主体（失败原因按 Java 的异常语义向上抛）。
    async fn fetch_statistics(&self) -> anyhow::Result<DownloaderStatistics> {
        // BitComet 2.21 起令牌去掉前导 `$`（上游注释：偷偷删一个 `$` 炸飞第三方应用）：
        // `${TOKEN}` → `{TOKEN}`
        let open = if self.server_version_at_least(
            TOKEN_WITHOUT_DOLLAR_VERSION.0,
            TOKEN_WITHOUT_DOLLAR_VERSION.1,
        ) {
            "{"
        } else {
            "${"
        };
        let token_list: Vec<String> = [
            "G_TOTAL_DOWNLOAD_AUTO",
            "G_TOTAL_UPLOAD_AUTO",
            "G_SESSION_DOWNLOAD_AUTO",
            "G_SESSION_UPLOAD_AUTO",
            "G_TOTAL_DOWNLOAD_BYTE",
            "G_TOTAL_UPLOAD_BYTE",
            "G_SESSION_DOWNLOAD_BYTE",
            "G_SESSION_UPLOAD_BYTE",
        ]
        .iter()
        .map(|token| format!("{open}{token}}}"))
        .collect();
        let body = serde_json::json!({ "token_list": token_list }).to_string();
        let resp = self.post_authed(GET_STATISTICS_LIST, body).await?;
        if !is_success(resp.status) {
            // 上游先取 body 再判状态码，失败时同样落入下面的 catch
            anyhow::bail!(
                "{}",
                tl(
                    MSG_FAILED_STATISTICS_LIST,
                    vec![
                        Param::Text(resp.status.to_string()),
                        Param::Text(resp.body.clone()),
                    ]
                )
            );
        }
        let value_list: BCStatisticsValueListResponse = serde_json::from_str(&resp.body)?;
        let byte_tokens =
            self.server_version_at_least(TRAFFIC_STATS_VERSION.0, TRAFFIC_STATS_VERSION.1);
        let mut total_uploaded = 0i64;
        let mut total_downloaded = 0i64;
        for value in &value_list.value_list {
            // Java 对 null token 的 `switch` 会 NPE（落入外层 catch）
            let token = value
                .token
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("NullPointerException: token is null"))?;
            // Java：`Long.parseLong(null)` / `String.split` 对 null 都会抛异常
            let raw_value = value
                .value
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("NullPointerException: value is null"))?;
            if byte_tokens {
                match token {
                    "${G_TOTAL_DOWNLOAD_BYTE}" | "{G_TOTAL_DOWNLOAD_BYTE}" => {
                        total_downloaded = raw_value.parse::<i64>()?
                    }
                    "${G_TOTAL_UPLOAD_BYTE}" | "{G_TOTAL_UPLOAD_BYTE}" => {
                        total_uploaded = raw_value.parse::<i64>()?
                    }
                    _ => {}
                }
            } else {
                match token {
                    "${G_TOTAL_DOWNLOAD_AUTO}" | "{G_TOTAL_DOWNLOAD_AUTO}" => {
                        total_downloaded = read_human_readable_value(raw_value)
                    }
                    "${G_TOTAL_UPLOAD_AUTO}" | "{G_TOTAL_UPLOAD_AUTO}" => {
                        total_uploaded = read_human_readable_value(raw_value)
                    }
                    _ => {}
                }
            }
        }
        Ok(DownloaderStatistics {
            all_time_upload: total_uploaded,
            all_time_download: total_downloaded,
        })
    }
}

/// 对齐 `readHumanReadableValue("2.33 GB")`：按空格拆成「数值 + 单位」再换算为字节。
///
/// 拆不出两段或数值非法时返回 0（与上游一致），未知单位同样返回 0。
fn read_human_readable_value(value: &str) -> i64 {
    let mut parts = value.split(' ');
    let (Some(number), Some(unit), None) = (parts.next(), parts.next(), parts.next()) else {
        return 0;
    };
    let Ok(number) = number.parse::<f64>() else {
        return 0;
    };
    match unit.to_uppercase().as_str() {
        "B" => number as i64,
        "KB" => (number * 1024.0) as i64,
        "MB" => (number * 1024.0 * 1024.0) as i64,
        "GB" => (number * 1024.0 * 1024.0 * 1024.0) as i64,
        "TB" => (number * 1024.0 * 1024.0 * 1024.0 * 1024.0) as i64,
        "PB" => (number * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0) as i64,
        "EB" => (number * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0) as i64,
        "ZB" => (number * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0) as i64,
        _ => 0,
    }
}

/// 对齐 `parseAddress(address, port, listenPort)` 中的 `HostAndPort` 取用方式：
/// - `[v6]:port` / `host:port` → 显式端口（IPv6 必须带方括号，与 Guava 一致）
/// - 裸 IPv6 或单个主机名 → 回退 `remote_port`
///
/// 第二个参数 `listen_port` 在上游同样未被使用（`getPortOrDefault(port)` 用的是 `remote_port`）。
/// 端口段不是合法数字时上游会抛 `NumberFormatException`（整个 `getPeers` 失败），
/// 这里退回默认分支：把整串视为主机名并沿用 `remote_port`。
fn parse_address(address: &str, remote_port: u16) -> (String, u16) {
    let trimmed = address.trim();
    if let Some(rest) = trimmed.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            let host = rest[..end].to_string();
            let tail = &rest[end + 1..];
            if let Some(port) = tail.strip_prefix(':') {
                if let Ok(port) = port.parse::<u16>() {
                    return (host, port);
                }
            }
            // 未给出（或非法）端口：`getPortOrDefault(port)` 回退 remote_port
            return (host, remote_port);
        }
        return (trimmed.to_string(), remote_port);
    }
    // 恰好一个冒号 → host:port；0 个或 2 个以上（裸 IPv6）→ 整个串是主机名
    if trimmed.matches(':').count() == 1 {
        if let Some((host, port)) = trimmed.split_once(':') {
            if let Ok(port) = port.parse::<u16>() {
                return (host.to_string(), port);
            }
        }
    }
    (trimmed.to_string(), remote_port)
}

/// 对齐 `new String(ByteUtil.hexToByteArray(peerId), ISO_8859_1)`：
/// 长度为奇数时**前置**补 `0`（`ByteUtil.hexToByteArray` 的行为），再两两解码为字节，
/// 逐字节按 ISO-8859-1 映射为字符。
///
/// 上游遇到非法十六进制会抛 `NumberFormatException`（进而让整个 `getPeers` 失败）；
/// 此处退回「已解码前缀」。
fn decode_peer_id_hex(hex: &str) -> String {
    let mut chars: Vec<char> = Vec::with_capacity(hex.chars().count() + 1);
    if hex.chars().count() % 2 == 1 {
        chars.push('0');
    }
    chars.extend(hex.chars());
    let mut out = String::with_capacity(chars.len() / 2);
    // 已刻意补'0'保证长度恒为偶数，`as_chunks` 与 chunks_exact 等价且无 panic 风险
    for pair in chars.as_chunks::<2>().0 {
        match (pair[0].to_digit(16), pair[1].to_digit(16)) {
            (Some(hi), Some(lo)) => out.push(char::from((hi * 16 + lo) as u8)),
            _ => break,
        }
    }
    out
}

/// 对齐 `new Semver(version, LOOSE).isGreaterThanOrEqualTo(...)`：
/// 只比较 `major.minor.patch`（缺省分量按 0，多余分量忽略）；不可解析返回 false。
fn version_at_least(version: &str, major: u64, minor: u64, patch: u64) -> bool {
    match parse_loose_semver(version) {
        Some(parsed) => parsed >= (major, minor, patch),
        None => false,
    }
}

/// 宽松解析 `major[.minor[.patch]]`（对齐 semver4j 的 LOOSE 模式）：
/// 允许 `v` 前缀与预发布/构建后缀，多余分量忽略。
fn parse_loose_semver(raw: &str) -> Option<(u64, u64, u64)> {
    let trimmed = raw.trim();
    let trimmed = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
    if trimmed.is_empty() {
        return None;
    }
    let core = trimmed.split(['-', '+']).next().unwrap_or_default();
    let mut parts = core.split('.');
    let major = parse_numeric_component(parts.next().unwrap_or_default())?;
    let minor = match parts.next() {
        Some(part) => parse_numeric_component(part)?,
        None => 0,
    };
    let patch = match parts.next() {
        Some(part) => parse_numeric_component(part)?,
        None => 0,
    };
    Some((major, minor, patch))
}

/// 取字符串的前导数字（如 `2`、`21`、`5`；`5.28` 已被 `split('.')` 拆开）。
fn parse_numeric_component(raw: &str) -> Option<u64> {
    let digits: String = raw.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// OkHttp `Response.isSuccessful()`：200..=299
fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// 对齐 `new DownloaderLoginResult(Status.EXCEPTION, new TranslationComponent(Lang.DOWNLOADER_LOGIN_IO_EXCEPTION, e.getClass().getName() + ": " + e.getMessage()))`。
fn login_io_exception(e: &anyhow::Error) -> LoginResult {
    let (class, message) = exception_params(e);
    LoginResult::exception(tl(
        MSG_LOGIN_IO_EXCEPTION,
        vec![Param::Text(format!("{class}: {message}"))],
    ))
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
                    return respond(status, body);
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

        // 请求载荷（`getTorrents()` 的 requirements）
        let list_request = mock.request(dto::GET_TASK_LIST);
        assert_eq!(
            list_request.json(),
            json!({
                "state_group": "ACTIVE",
                "sort_key": "",
                "sort_order": "unsorted",
                "tag_filter": "ALL",
                "task_type": "ALL",
                "start": 0,
                "limit": 2147483646
            })
        );
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

        // 任务列表整体失败 → Err（DOWNLOADER_BC_FAILED_REQUEST_TORRENT_LIST）
        let mock = mock_with_version("2.21.5", IPFILTER_OK);
        *mock.task_list.lock().unwrap() = (500, "boom".to_string());
        let dl = downloader(mock.clone(), false);
        logged_in(&dl).await;
        let err = dl.fetch_torrents().await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("Torrents"), "{text}");
        assert!(text.contains("500"), "{text}");
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
            "1.2.3.4\n::ffff:1.2.3.4\n2001:db8::1\n2001:db8::/52"
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
            "1.2.3.4\n::ffff:1.2.3.4\n2001:db8::1\n2001:db8::/52"
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
}
