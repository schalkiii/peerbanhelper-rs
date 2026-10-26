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
mod tests;
