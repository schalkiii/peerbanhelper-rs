//! Deluge 适配器，忠实复刻上游 `Deluge` + `raccoonfink.deluge.DelugeServer`。
//!
//! 要点：
//! - JSON-RPC 端点 `{endpoint}{rpc-url}`（默认 `/json`），请求体
//!   `{"id": <自增计数>, "method": ..., "params": [...]}`（对齐 `DelugeRequest.toPostData`）。
//! - 每条调用路径都先 `auth.login`（密码是唯一参数）再调用目标方法。登录成功还需
//!   `system.listMethods` 含 PBH-Adapter-Deluge 的 4 个方法，否则报「插件未安装」。
//! - torrents 与其 peers 来自同一次 `peerbanhelperadapter.get_active_torrents_info`
//!   （对齐 `Deluge.getTorrents` / `DelugeTorrent.peers`），不额外发 per-torrent RPC：
//!   peers 随响应缓存，由 `fetch_peers` 读出。
//! - 封禁：增量 `peerbanhelperadapter.ban_ips`、全量 `peerbanhelperadapter.replace_blocklist`，
//!   两者都按 `remapBanListAddress(addr)`（`supportRangeBan` 恒为 true）重映射后去重。
//! - RPC 失败时对齐上游：记日志后忽略（返回空列表 / 0 统计 / 不抛错）。
//! - 特性标志：上游 `Deluge` 未覆写 `getFeatureFlags()`，沿用 `AbstractDownloader` 默认集合。

pub mod dto;

use crate::http::{BoxFuture, HttpFetcher, HttpRequest, ReqwestFetcher};
use crate::{BanEntry, Downloader, DownloaderFeature, DownloaderStatistics, LoginResult};
use dto::*;
use pbh_core::defaults::qb as qbcfg;
use pbh_core::i18n::{Param, TranslationComponent, Translator};
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::remap::{remap_ban_list_address, translate_peer_ip, RemapConfig};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::error;

/// PBH-Adapter-Deluge 必须提供的方法（对齐 `Deluge.MUST_HAVE_METHODS`，顺序一致）。
const MUST_HAVE_METHODS: [&str; 4] = [
    "peerbanhelperadapter.replace_blocklist",
    "peerbanhelperadapter.unban_ips",
    "peerbanhelperadapter.get_active_torrents_info",
    "peerbanhelperadapter.ban_ips",
];

const M_AUTH_LOGIN: &str = "auth.login";
const M_LIST_METHODS: &str = "system.listMethods";
/// 对齐 `DelugeServer.getConfig()` 的 `core.get_config`
const M_GET_CONFIG: &str = "core.get_config";
/// 对齐 `DelugeServer.setConfig()` 的 `core.set_config`
const M_SET_CONFIG: &str = "core.set_config";

/// Deluge 的 `max_upload_speed` / `max_download_speed` 单位是 **KiB/s**（上游 ×1024 转 bytes/s）。
const SPEED_LIMIT_UNIT: i64 = 1024;
/// 活跃 torrent + peers（一次调用取全部）
const M_ACTIVE_TORRENTS: &str = "peerbanhelperadapter.get_active_torrents_info";
const M_BAN_IPS: &str = "peerbanhelperadapter.ban_ips";
const M_REPLACE_BLOCKLIST: &str = "peerbanhelperadapter.replace_blocklist";
const M_SESSION_TOTALS: &str = "peerbanhelperadapter.get_session_totals";

const MSG_PLUGIN_NOT_INSTALLED: &str = "DOWNLOADER_DELUGE_PLUGIN_NOT_INSTALLED";
const MSG_API_ERROR: &str = "DOWNLOADER_DELUGE_API_ERROR";
const MSG_INCORRECT_CRED: &str = "DOWNLOADER_LOGIN_INCORRECT_CRED";
const MSG_IO_EXCEPTION: &str = "DOWNLOADER_LOGIN_IO_EXCEPTION";

/// 对齐上游 `tlUI`：下载器不持有 locale，使用服务端默认文案语言。
const UI_LOCALE: &str = "zh_cn";

#[derive(Clone, Debug)]
pub struct DelugeConfig {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    /// Deluge Web 密码（`auth.login` 的唯一参数）
    pub password: String,
    pub verify_ssl: bool,
    /// RPC 路径（上游默认 `/json`）
    pub rpc_url: String,
    pub paused: bool,
    /// `config.yml` 的 `banlist-remapping` / `ip-remapping` 配置
    pub remap: RemapConfig,
}

impl Default for DelugeConfig {
    fn default() -> Self {
        Self {
            id: "deluge".into(),
            name: "Deluge".into(),
            // Deluge Web UI 默认端口
            endpoint: "http://127.0.0.1:8112".into(),
            password: String::new(),
            verify_ssl: true,
            rpc_url: "/json".into(),
            paused: false,
            remap: RemapConfig::default(),
        }
    }
}

pub struct DelugeDownloader {
    config: DelugeConfig,
    rpc_endpoint: String,
    http: Arc<dyn HttpFetcher>,
    /// JSON-RPC 请求 id（对齐 `DelugeServer.m_counter`：从 0 起自增）
    request_id: AtomicU64,
    /// `get_active_torrents_info` 带回来的 peers，按 torrent hash 缓存
    peers_cache: Mutex<HashMap<String, Vec<PeerData>>>,
}

impl DelugeDownloader {
    pub fn new(config: DelugeConfig) -> anyhow::Result<Self> {
        let fetcher = Arc::new(ReqwestFetcher::new(
            config.verify_ssl,
            qbcfg::CONNECT_TIMEOUT_SECS,
            qbcfg::READ_TIMEOUT_SECS,
        )?);
        Self::with_fetcher(config, fetcher)
    }

    pub fn with_fetcher(config: DelugeConfig, http: Arc<dyn HttpFetcher>) -> anyhow::Result<Self> {
        // 对齐 `Config.readFromYaml`：去掉 endpoint 结尾的 `/`，然后与 rpc-url 直接拼接
        let endpoint = config.endpoint.trim_end_matches('/').to_string();
        let rpc_endpoint = format!("{}{}", endpoint, config.rpc_url);
        Ok(Self {
            config,
            rpc_endpoint,
            http,
            request_id: AtomicU64::new(0),
            peers_cache: Mutex::new(HashMap::new()),
        })
    }

    /// 原始 JSON-RPC 调用，对齐 `DelugeServer.makeRequest`（不含自动登录）。
    async fn make_request(&self, method: &str, params: Vec<Value>) -> anyhow::Result<Value> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let body = json!({ "id": id, "method": method, "params": params }).to_string();
        let resp = self
            .http
            .execute(HttpRequest::post_json(self.rpc_endpoint.clone(), body))
            .await?;
        if resp.status != 200 {
            // 对齐 `new DelugeException(resp.code() + " - " + resp.body().string())`
            anyhow::bail!("{} - {}", resp.status, resp.body);
        }
        let json: Value = serde_json::from_str(&resp.body)?;
        // 对齐 makeRequest 的错误检查：`error` 非 null 即抛异常
        // （上游对非对象形式的 error 会抛 IllegalStateException，此处仅把对象当作错误）
        if let Some(err) = json.get("error").filter(|v| v.is_object()) {
            let message = err.get("message").and_then(|v| v.as_str());
            let code = err.get("code").and_then(|v| v.as_i64()).unwrap_or(-1);
            let mut builder = String::from("Error");
            if code >= 0 {
                builder.push_str(&format!(" {code}"));
            }
            if let Some(message) = message {
                builder.push_str(&format!(": {message}"));
            }
            anyhow::bail!(builder);
        }
        // 对齐 `DelugeResponse` 构造函数：缺少 `id` 字段即视为无效响应
        if json.get("id").is_none() {
            anyhow::bail!("Invalid 'id' field in JSON: {json}");
        }
        Ok(json)
    }

    /// `auth.login`（对齐 `DelugeServer.login`）。返回 `result` 布尔值。
    async fn login_session(&self) -> anyhow::Result<bool> {
        let resp = self
            .make_request(M_AUTH_LOGIN, vec![json!(self.config.password)])
            .await?;
        Ok(resp
            .get("result")
            .and_then(|v| v.as_bool())
            .unwrap_or(false))
    }

    /// 已登录的 RPC 调用：先 `auth.login`，再调用目标方法，返回其 `result` 字段。
    async fn call(&self, method: &str, params: Vec<Value>) -> anyhow::Result<Value> {
        if !self.login_session().await? {
            // 上游此处会因会话未建立而收到 RPC 错误；语义同样是「调用未生效」
            anyhow::bail!("auth.login 返回 false");
        }
        let resp = self.make_request(method, params).await?;
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// `system.listMethods`（对齐 `DelugeListMethodsResponse`）。
    async fn list_methods(&self) -> anyhow::Result<Vec<String>> {
        let resp = self.make_request(M_LIST_METHODS, Vec::new()).await?;
        Ok(resp
            .get("result")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn torrent_from(&self, active: &ActiveTorrent) -> TorrentData {
        TorrentData {
            hash: active.info_hash.clone().unwrap_or_default(),
            name: active.name.clone().unwrap_or_default(),
            // 对齐 `activeTorrent.getProgress() / 100.0d`
            progress: active.progress.unwrap_or(0.0) / 100.0,
            total_size: active.size.unwrap_or(0),
            // Deluge 不提供 piece 信息，完成量只能取插件给的 `completed_size`
            piece_size: 0,
            pieces_have: 0,
            // 对齐 `DelugeTorrent.completedSize`（直接取响应字段）
            completed_override: Some(active.completed_size.unwrap_or(0)),
            dlspeed: active.download_payload_rate.unwrap_or(0),
            upspeed: active.upload_payload_rate.unwrap_or(0),
            is_private: Some(active.is_private.unwrap_or(false)),
        }
    }

    fn peers_of(&self, active: &ActiveTorrent) -> Vec<PeerData> {
        let mut out = Vec::new();
        for peer in active.peers.as_deref().unwrap_or_default() {
            let raw_ip = peer.ip.clone().unwrap_or_default();
            let raw_port = peer.port.unwrap_or(0).clamp(0, u16::MAX as i64) as u16;
            // 对齐 `addressTranslate(new PeerAddress(ip, port, ip))`：
            // ip/port 为翻译后的地址，raw 部分保留下载器原始 ip:port
            let (ip, port) = translate_peer_ip(&raw_ip, raw_port, &self.config.remap.ip_remapping);
            // 对齐 `Deluge.getTorrents`：翻译后的 ip 为空则丢弃该 peer
            if ip.trim().is_empty() {
                continue;
            }
            out.push(PeerData {
                client_name: peer.client_name.clone(),
                // 对齐 `StrUtil.toStringHex(peer.getPeerId())` + 截断到 8 字符
                peer_id: Some(peer_id_from_hex(
                    peer.peer_id.as_deref().unwrap_or_default(),
                )),
                dl_speed: peer.payload_down_speed.unwrap_or(0),
                downloaded: peer.total_download.unwrap_or(0),
                up_speed: peer.payload_up_speed.unwrap_or(0),
                uploaded: peer.total_upload.unwrap_or(0),
                progress: peer.progress.unwrap_or(0.0) / 100.0,
                flags: Some(peer_flag_string(
                    peer.flags.unwrap_or(0),
                    peer.source.unwrap_or(0),
                )),
                ip,
                port,
                raw_ip: format!("{}:{}", raw_ip, raw_port),
                // Deluge 无连接类型概念
                connection: None,
            });
        }
        out
    }

    /// 对齐 `remapBanListAddress(addr)`：`supportRangeBan` 恒为 true（上游单参重载）。
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
}

impl Downloader for DelugeDownloader {
    fn id(&self) -> &str {
        &self.config.id
    }
    fn name(&self) -> &str {
        &self.config.name
    }
    fn downloader_type(&self) -> &'static str {
        "deluge"
    }

    /// 对齐 `AbstractDownloader.getFeatureFlags()`：`Deluge` 未覆写，故仅 `UNBAN_IP`。
    fn feature_flags(&self) -> Vec<String> {
        vec![DownloaderFeature::UnbanIp.name().to_string()]
    }

    fn login<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<LoginResult>> {
        Box::pin(async move {
            // 对齐 `AbstractDownloader.login()` 开头的 `isPaused()` 短路
            if self.config.paused {
                return Ok(LoginResult::paused("Deluge 已暂停", Self::VERSION));
            }
            let logged_in = match self.login_session().await {
                Ok(v) => v,
                Err(e) => {
                    // 上游 `catch (DelugeException)` → EXCEPTION（不进入冷却）
                    return Ok(LoginResult::exception(tl(
                        MSG_IO_EXCEPTION,
                        vec![exception_param(&e)],
                    )));
                }
            };
            if !logged_in {
                return Ok(LoginResult::incorrect_credential(tl(
                    MSG_INCORRECT_CRED,
                    Vec::new(),
                )));
            }
            let methods = match self.list_methods().await {
                Ok(v) => v,
                Err(e) => {
                    return Ok(LoginResult::exception(tl(
                        MSG_IO_EXCEPTION,
                        vec![exception_param(&e)],
                    )))
                }
            };
            // 对齐 `new HashSet<>(methods).containsAll(MUST_HAVE_METHODS)`
            if !MUST_HAVE_METHODS
                .iter()
                .all(|need| methods.iter().any(|m| m == need))
            {
                return Ok(LoginResult::missing_components(tl(
                    MSG_PLUGIN_NOT_INSTALLED,
                    vec![Param::Text(self.config.name.clone())],
                )));
            }
            Ok(LoginResult::success("OK", Self::VERSION))
        })
    }

    /// 对齐 `Deluge.getTorrents()`：一次 RPC 取出活跃 torrent 及其 peers；
    /// 上游不做私有种子过滤，也不做速率过滤（活跃性由适配器插件保证）。
    fn fetch_torrents<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Vec<TorrentData>>> {
        Box::pin(async move {
            let result = match self.call(M_ACTIVE_TORRENTS, Vec::new()).await {
                Ok(v) => v,
                Err(e) => {
                    // 对齐上游：`catch (DelugeException)` → 记日志并返回已有（空）列表
                    log_api_error(&e);
                    return Ok(Vec::new());
                }
            };
            let active: Vec<ActiveTorrent> = serde_json::from_value(result).unwrap_or_default();
            let mut cache = HashMap::new();
            let mut out = Vec::with_capacity(active.len());
            for torrent in &active {
                cache.insert(
                    torrent.info_hash.clone().unwrap_or_default(),
                    self.peers_of(torrent),
                );
                out.push(self.torrent_from(torrent));
            }
            if let Ok(mut guard) = self.peers_cache.lock() {
                *guard = cache;
            }
            Ok(out)
        })
    }

    /// 对齐 `Deluge.getPeers()`：直接返回 `DelugeTorrent` 自带的 peers（同一次响应）。
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

    /// 对齐 `setBanListIncrement`：`peerbanhelperadapter.ban_ips`，载荷为重映射后的地址。
    fn ban_peers<'a>(&'a self, peers: &'a [BanEntry]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let ips = self.remap_ips(peers.iter().map(|p| p.ip.clone()));
            if ips.is_empty() {
                return Ok(());
            }
            if let Err(e) = self.call(M_BAN_IPS, vec![json!(ips)]).await {
                log_api_error(&e);
            }
            Ok(())
        })
    }

    /// 对齐 `setBanListFull`：`peerbanhelperadapter.replace_blocklist`，整份列表替换。
    fn replace_banned_ips<'a>(&'a self, ips: &'a [String]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let mapped = self.remap_ips(ips.iter().cloned());
            if let Err(e) = self.call(M_REPLACE_BLOCKLIST, vec![json!(mapped)]).await {
                log_api_error(&e);
            }
            Ok(())
        })
    }

    /// 对齐 `Deluge.getStatistics()`：失败时记日志并返回 0（不向上抛错）。
    fn statistics<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<DownloaderStatistics>> {
        Box::pin(async move {
            let result = match self.call(M_SESSION_TOTALS, Vec::new()).await {
                Ok(v) => v,
                Err(e) => {
                    log_api_error(&e);
                    return Ok(DownloaderStatistics::default());
                }
            };
            let dto: SessionTotals = serde_json::from_value(result).unwrap_or_default();
            Ok(DownloaderStatistics {
                all_time_upload: dto.total_payload_upload.unwrap_or(0),
                all_time_download: dto.total_payload_download.unwrap_or(0),
            })
        })
    }

    /// 对齐 `Deluge.getSpeedLimiter()`：`core.get_config`，`max_upload_speed` /
    /// `max_download_speed`（**KiB/s**）×1024 -> bytes/s。
    ///
    /// 失败语义：`DelugeException` 时上游记日志并返回 `null`（调用方跳过）-> 本实现记同一
    /// 文案日志后返回 `Err`；`config` 或其限速字段缺失在上游是 NPE（不在 catch 范围内，继续
    /// 上抛）-> 本实现同样返回 `Err`。
    fn get_speed_limiter<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<(i64, i64)>> {
        Box::pin(async move {
            let result = match self.call(M_GET_CONFIG, Vec::new()).await {
                Ok(v) => v,
                Err(e) => {
                    log_api_error(&e);
                    anyhow::bail!("core.get_config failed: {e}");
                }
            };
            let config: CoreConfig = serde_json::from_value(result).map_err(|e| {
                anyhow::anyhow!("NullPointerException: core.get_config result is null: {e}")
            })?;
            let download_limit = config.max_download_speed.ok_or_else(|| {
                anyhow::anyhow!("NullPointerException: max_download_speed is null")
            })? * SPEED_LIMIT_UNIT;
            let upload_limit = config
                .max_upload_speed
                .ok_or_else(|| anyhow::anyhow!("NullPointerException: max_upload_speed is null"))?
                * SPEED_LIMIT_UNIT;
            Ok((upload_limit, download_limit))
        })
    }

    /// 对齐 `Deluge.setSpeedLimiter()`：`core.set_config`，载荷是
    /// `ConfigRequest.toRequestJSON()` 的一个对象参数（键序 `max_download_speed` 后
    /// `max_upload_speed`）；bytes/s 整除 1024 换成 **KiB/s**，`<= 0` 即
    /// `isUploadUnlimited()` / `isDownloadUnlimited()` 写 0（Deluge 的「不限制」）。
    ///
    /// 失败语义：上游 `catch (DelugeException)` 只记日志，不抛错 -> 本实现同样返回 `Ok(())`。
    fn set_speed_limiter<'a>(
        &'a self,
        upload: i64,
        download: i64,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let upload_limit = if upload <= 0 {
                0
            } else {
                upload / SPEED_LIMIT_UNIT
            };
            let download_limit = if download <= 0 {
                0
            } else {
                download / SPEED_LIMIT_UNIT
            };
            let config = json!({
                "max_download_speed": download_limit,
                "max_upload_speed": upload_limit,
            });
            if let Err(e) = self.call(M_SET_CONFIG, vec![config]).await {
                log_api_error(&e);
            }
            Ok(())
        })
    }
}

impl DelugeDownloader {
    /// 上游 Deluge 无版本上报能力（`DownloaderLoginResult` 也不含版本）。
    const VERSION: &'static str = "";
}

/// 对齐 `Deluge.parsePeerFlag` 的位定义 + `PeerFlag.toString()` 的 libtorrent 风格输出。
fn peer_flag_string(peer_flag: i32, source_flag: i32) -> String {
    let bit = |flags: i32, index: u32| (flags & (1 << index)) != 0;
    let interesting = bit(peer_flag, 0);
    let choked = bit(peer_flag, 1);
    let remote_interested = bit(peer_flag, 2);
    let remote_choked = bit(peer_flag, 3);
    let local_connection = bit(peer_flag, 6);
    let optimistic_unchoke = bit(peer_flag, 11);
    let snubbed = bit(peer_flag, 12);
    let utp_socket = bit(peer_flag, 17);
    let rc4_encrypted = bit(peer_flag, 19);
    let plaintext_encrypted = bit(peer_flag, 20);
    let from_dht = bit(source_flag, 1);
    let from_pex = bit(source_flag, 2);
    let from_lsd = bit(source_flag, 3);

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
    if from_lsd {
        parts.push("L");
    }
    if rc4_encrypted {
        parts.push("E");
    }
    if plaintext_encrypted {
        parts.push("e");
    }
    if utp_socket {
        parts.push("P");
    }
    parts.join(" ")
}

/// 对齐 `StrUtil.toStringHex`：每两个十六进制字符解码为一个字节，按 ISO-8859-1 转字符；
/// 随后 `if (peerId.length() > 8) peerId = peerId.substring(0, 8)`。
///
/// 上游遇到非法十六进制会抛 `NumberFormatException`，此处退回「已解码前缀」。
fn peer_id_from_hex(hex: &str) -> String {
    let bytes = hex.as_bytes();
    let mut out = String::with_capacity(bytes.len() / 2);
    // 上游 Hex 解码输入长度固定为偶数，`as_chunks` 与 chunks_exact 等价且无 panic 风险
    for pair in bytes.as_chunks::<2>().0 {
        let (hi, lo) = (pair[0] as char, pair[1] as char);
        match (hi.to_digit(16), lo.to_digit(16)) {
            (Some(hi), Some(lo)) => out.push(char::from((hi * 16 + lo) as u8)),
            _ => break,
        }
    }
    if out.chars().count() > 8 {
        out.chars().take(8).collect()
    } else {
        out
    }
}

/// 对齐上游 `log.error(tlUI(Lang.DOWNLOADER_DELUGE_API_ERROR), e)`。
fn log_api_error(e: &anyhow::Error) {
    error!("{}: {e}", tl(MSG_API_ERROR, Vec::new()));
}

/// 上游 `e.getClass().getName() + ": " + e.getMessage()`（Deluge 侧异常均为 `DelugeException`）。
fn exception_param(e: &anyhow::Error) -> Param {
    Param::Text(format!("DelugeException: {e}"))
}

/// 渲染服务端 UI 文案（对齐上游 `tlUI`）。
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

    /// 活跃 torrent 与 peers 的夹具（字段名对齐 PBH-Adapter-Deluge）。
    const ACTIVE_TORRENTS: &str = r#"[
      {
        "id": "torrent-1",
        "name": "ubuntu.iso",
        "info_hash": "1111111111111111111111111111111111111111",
        "progress": 42.5,
        "size": 1000,
        "completed_size": 425,
        "upload_payload_rate": 10,
        "download_payload_rate": 20,
        "priv": true,
        "peers": [
          {
            "ip": "9.9.9.9",
            "port": 6881,
            "peer_id": "2d5452313030302d6162636465666768696a",
            "client_name": "-TR1000-abcdefghij",
            "payload_up_speed": 101,
            "payload_down_speed": 202,
            "total_upload": 303,
            "total_download": 404,
            "progress": 55.5,
            "flags": 79,
            "source": 0
          },
          {
            "ip": "64:ff9b::102:304",
            "port": 7001,
            "peer_id": "2d5452",
            "client_name": null,
            "payload_up_speed": 0,
            "payload_down_speed": 0,
            "total_upload": 0,
            "total_download": 0,
            "progress": 0,
            "flags": 2049,
            "source": 14
          },
          { "ip": "", "port": 1, "peer_id": null, "flags": 0, "source": 0 }
        ]
      },
      {
        "id": "torrent-2",
        "name": "private.bin",
        "info_hash": "2222222222222222222222222222222222222222",
        "progress": 100,
        "size": 2000,
        "upload_payload_rate": 0,
        "download_payload_rate": 0,
        "priv": null,
        "peers": []
      }
    ]"#;

    /// PBH-Adapter-Deluge 的 JSON-RPC 内存实现。
    struct DelugeMock {
        /// 收到的请求（method, 完整请求体）
        pub requests: Mutex<Vec<(String, Value)>>,
        /// `system.listMethods` 的方法表
        pub list_methods: Mutex<Vec<String>>,
        /// `auth.login` 的 result
        pub login_ok: Mutex<bool>,
        /// 让 `peerbanhelperadapter.*` 返回 JSON-RPC error
        pub rpc_error: Mutex<bool>,
        /// `get_active_torrents_info` 的 result
        pub active_torrents: Mutex<String>,
        /// `core.get_config` 的 result（Deluge 的单位是 KiB/s）
        pub core_config: Mutex<Value>,
    }

    impl DelugeMock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                list_methods: Mutex::new(MUST_HAVE_METHODS.iter().map(|m| m.to_string()).collect()),
                login_ok: Mutex::new(true),
                rpc_error: Mutex::new(false),
                active_torrents: Mutex::new(ACTIVE_TORRENTS.to_string()),
                core_config: Mutex::new(json!({
                    "max_download_speed": 2048,
                    "max_upload_speed": 1024,
                })),
            })
        }

        fn methods(&self) -> Vec<String> {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .map(|(m, _)| m.clone())
                .collect()
        }

        /// 某个方法收到的第一个请求的 `params`
        fn params_of(&self, method: &str) -> Value {
            self.requests
                .lock()
                .unwrap()
                .iter()
                .find(|(m, _)| m == method)
                .map(|(_, body)| body["params"].clone())
                .unwrap_or(Value::Null)
        }
    }

    fn rpc_ok(id: &Value, result: Value) -> String {
        json!({ "id": id, "result": result, "error": Value::Null }).to_string()
    }

    fn rpc_err(id: &Value, message: &str) -> String {
        json!({ "id": id, "result": Value::Null, "error": { "code": 1, "message": message } })
            .to_string()
    }

    impl HttpFetcher for DelugeMock {
        fn execute<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, anyhow::Result<HttpResponse>> {
            Box::pin(async move {
                assert!(req.url.ends_with("/json"), "unexpected url {}", req.url);
                let body: Value =
                    serde_json::from_str(req.body.as_deref().unwrap_or_default()).unwrap();
                let method = body["method"].as_str().unwrap_or_default().to_string();
                let id = body["id"].clone();
                self.requests.lock().unwrap().push((method.clone(), body));
                let failed = *self.rpc_error.lock().unwrap();
                let json = match method.as_str() {
                    M_AUTH_LOGIN => rpc_ok(&id, json!(*self.login_ok.lock().unwrap())),
                    M_LIST_METHODS => rpc_ok(&id, json!(self.list_methods.lock().unwrap().clone())),
                    M_ACTIVE_TORRENTS => {
                        if failed {
                            rpc_err(&id, "adapter failed")
                        } else {
                            rpc_ok(&id, self.active_torrents.lock().unwrap().parse().unwrap())
                        }
                    }
                    M_GET_CONFIG => {
                        if failed {
                            rpc_err(&id, "adapter failed")
                        } else {
                            rpc_ok(&id, self.core_config.lock().unwrap().clone())
                        }
                    }
                    M_SET_CONFIG => {
                        if failed {
                            rpc_err(&id, "adapter failed")
                        } else {
                            rpc_ok(&id, json!(true))
                        }
                    }
                    M_BAN_IPS | M_REPLACE_BLOCKLIST => {
                        if failed {
                            rpc_err(&id, "adapter failed")
                        } else {
                            rpc_ok(&id, json!(true))
                        }
                    }
                    M_SESSION_TOTALS => {
                        if failed {
                            rpc_err(&id, "adapter failed")
                        } else {
                            rpc_ok(
                                &id,
                                json!({ "total_payload_upload": 111_222, "total_payload_download": 333_444 }),
                            )
                        }
                    }
                    other => rpc_err(&id, &format!("unknown method {other}")),
                };
                Ok(HttpResponse::ok(json))
            })
        }
    }

    fn mock_downloader(mock: Arc<DelugeMock>) -> Arc<DelugeDownloader> {
        let config = DelugeConfig {
            id: "deluge-test".into(),
            name: "Deluge Test".into(),
            endpoint: "http://mock.local".into(),
            password: "secret".into(),
            ..DelugeConfig::default()
        };
        Arc::new(DelugeDownloader::with_fetcher(config, mock as Arc<dyn HttpFetcher>).unwrap())
    }

    #[tokio::test]
    async fn login_requires_adapter_plugin_methods() {
        let mock = DelugeMock::new();
        let deluge = mock_downloader(mock.clone());
        let login = deluge.login().await.unwrap();
        assert!(login.success, "{}", login.message);
        // 先 auth.login 再 system.listMethods；密码作为唯一参数
        assert_eq!(mock.methods(), vec![M_AUTH_LOGIN, M_LIST_METHODS]);
        assert_eq!(mock.params_of(M_AUTH_LOGIN), json!(["secret"]));

        // 插件缺失：方法表不含 adapter 方法 -> 登录失败并给出插件提示
        let mock = DelugeMock::new();
        *mock.list_methods.lock().unwrap() = vec!["daemon.login".to_string()];
        let deluge = mock_downloader(mock);
        let login = deluge.login().await.unwrap();
        assert!(!login.success);
        assert!(
            login.message.contains("PBH-Adapter-Deluge"),
            "缺少插件时应提示适配器插件: {}",
            login.message
        );
    }

    #[tokio::test]
    async fn login_reports_incorrect_credentials() {
        let mock = DelugeMock::new();
        *mock.login_ok.lock().unwrap() = false;
        let deluge = mock_downloader(mock.clone());
        let login = deluge.login().await.unwrap();
        assert!(!login.success);
        assert!(login.message.contains("凭据"), "{}", login.message);
        // 凭据错误时不再查询方法表
        assert_eq!(mock.methods(), vec![M_AUTH_LOGIN]);
    }

    #[tokio::test]
    async fn torrents_and_peers_come_from_a_single_rpc() {
        let mock = DelugeMock::new();
        let deluge = mock_downloader(mock.clone());
        let torrents = deluge.fetch_torrents().await.unwrap();
        assert_eq!(torrents.len(), 2);

        let first = torrents
            .iter()
            .find(|t| t.hash == "1111111111111111111111111111111111111111")
            .expect("torrent-1");
        assert_eq!(first.name, "ubuntu.iso");
        assert_eq!(first.progress, 0.425);
        assert_eq!(first.total_size, 1000);
        // 对齐 DelugeTorrent.completedSize：直接取 completed_size
        assert_eq!(first.completed_override, Some(425));
        assert_eq!(first.completed_size(), 425);
        assert_eq!(first.dlspeed, 20);
        assert_eq!(first.upspeed, 10);
        assert!(first.is_private());

        let second = torrents
            .iter()
            .find(|t| t.hash == "2222222222222222222222222222222222222222")
            .expect("torrent-2");
        assert!(!second.is_private());
        assert_eq!(second.completed_override, Some(0));

        // 每个调用路径都先登录；get_active_torrents_info 只发一次（peers 随响应返回）
        assert_eq!(
            mock.methods(),
            vec![M_AUTH_LOGIN, M_ACTIVE_TORRENTS],
            "不应额外发 per-torrent peer RPC"
        );

        // fetch_peers 直接读缓存，不产生新请求
        let peers = deluge.fetch_peers(first).await.unwrap();
        assert_eq!(peers.len(), 2, "空 ip 的 peer 被丢弃");
        assert_eq!(mock.methods().len(), 2);
    }

    #[tokio::test]
    async fn peers_are_translated_and_flags_mapped() {
        let mock = DelugeMock::new();
        let deluge = mock_downloader(mock.clone());
        let torrents = deluge.fetch_torrents().await.unwrap();
        let first = torrents
            .iter()
            .find(|t| t.hash == "1111111111111111111111111111111111111111")
            .expect("torrent-1");
        let peers = deluge.fetch_peers(first).await.unwrap();
        let by_ip: HashMap<&str, &PeerData> = peers.iter().map(|p| (p.ip.as_str(), p)).collect();

        let normal = by_ip.get("9.9.9.9").expect("peer 9.9.9.9");
        // peer_id：十六进制解码后截断到 8 字符（"-TR1000-"）
        assert_eq!(normal.peer_id.as_deref(), Some("-TR1000-"));
        assert_eq!(normal.client_name.as_deref(), Some("-TR1000-abcdefghij"));
        assert_eq!(normal.dl_speed, 202);
        assert_eq!(normal.up_speed, 101);
        assert_eq!(normal.downloaded, 404);
        assert_eq!(normal.uploaded, 303);
        assert_eq!(normal.progress, 0.555);
        // flags=79（interesting+remoteChoked+remoteInterested+choked+localConnection）→ "d u"
        assert_eq!(normal.flags.as_deref(), Some("d u"));
        assert_eq!(normal.raw_ip, "9.9.9.9:6881");
        assert_eq!(normal.port, 6881);
        assert!(normal.connection.is_none());

        // NAT64 地址按 ip-remapping 翻译，raw_ip 保留下载器原始 ip:port
        let nat64 = by_ip.get("1.2.3.4").expect("NAT64 peer 翻译为 1.2.3.4");
        assert_eq!(nat64.port, 7001);
        assert_eq!(nat64.raw_ip, "64:ff9b::102:304:7001");
        assert_eq!(nat64.peer_id.as_deref(), Some("-TR"));
        // flags=2049（interesting+optimisticUnchoke）+ source=14（dht+pex+lsd）→ "D ? O I H X L"
        assert_eq!(nat64.flags.as_deref(), Some("D ? O I H X L"));
    }

    #[tokio::test]
    async fn incremental_ban_sends_remapped_addresses() {
        let mock = DelugeMock::new();
        let deluge = mock_downloader(mock.clone());
        deluge
            .ban_peers(&[
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

        assert_eq!(mock.methods(), vec![M_AUTH_LOGIN, M_BAN_IPS]);
        // remapBanListAddress(addr, supportRangeBan=true)：去重 + IPv6 /52 网段
        assert_eq!(
            mock.params_of(M_BAN_IPS),
            json!([["1.2.3.4", "::ffff:102:304", "2001:db8::1", "2001:db8::/52"]])
        );
    }

    #[tokio::test]
    async fn full_ban_replaces_blocklist() {
        let mock = DelugeMock::new();
        let deluge = mock_downloader(mock.clone());
        deluge
            .replace_banned_ips(&["1.2.3.4".to_string(), "2001:db8::1".to_string()])
            .await
            .unwrap();

        assert_eq!(mock.methods(), vec![M_AUTH_LOGIN, M_REPLACE_BLOCKLIST]);
        assert_eq!(
            mock.params_of(M_REPLACE_BLOCKLIST),
            json!([["1.2.3.4", "::ffff:102:304", "2001:db8::1", "2001:db8::/52"]])
        );
    }

    #[tokio::test]
    async fn ban_failure_is_logged_and_ignored() {
        let mock = DelugeMock::new();
        *mock.rpc_error.lock().unwrap() = true;
        let deluge = mock_downloader(mock);
        deluge
            .ban_peers(&[BanEntry {
                ip: "1.2.3.4".into(),
                port: 1,
                raw_ip: "1.2.3.4:1".into(),
            }])
            .await
            .unwrap();
        deluge
            .replace_banned_ips(&["1.2.3.4".to_string()])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn statistics_and_zero_on_failure() {
        let mock = DelugeMock::new();
        let deluge = mock_downloader(mock.clone());
        let stats = deluge.statistics().await.unwrap();
        assert_eq!(stats.all_time_upload, 111_222);
        assert_eq!(stats.all_time_download, 333_444);
        assert_eq!(mock.methods(), vec![M_AUTH_LOGIN, M_SESSION_TOTALS]);

        // RPC 失败：对齐上游，只记日志并返回 0（不返回 Err）
        let mock = DelugeMock::new();
        *mock.rpc_error.lock().unwrap() = true;
        let deluge = mock_downloader(mock.clone());
        let stats = deluge.statistics().await.unwrap();
        assert_eq!(stats.all_time_upload, 0);
        assert_eq!(stats.all_time_download, 0);

        // torrents 失败同样返回空列表
        let torrents = deluge.fetch_torrents().await.unwrap();
        assert!(torrents.is_empty());
    }

    /// 读取：`core.get_config` 的 KiB/s ×1024 -> bytes/s。
    #[tokio::test]
    async fn speed_limiter_is_read_in_kib() {
        let mock = DelugeMock::new();
        let deluge = mock_downloader(mock.clone());
        let (upload, download) = deluge.get_speed_limiter().await.unwrap();
        assert_eq!(upload, 1_048_576, "1024 KiB/s -> 1 MiB/s");
        assert_eq!(download, 2_097_152, "2048 KiB/s -> 2 MiB/s");
        assert_eq!(mock.methods(), vec![M_AUTH_LOGIN, M_GET_CONFIG]);
        assert_eq!(mock.params_of(M_GET_CONFIG), json!([]));
    }

    /// 下发 1 MiB/s：bytes/s ÷ 1024 -> KiB/s，载荷是 `ConfigRequest.toRequestJSON()` 的一个对象参数。
    #[tokio::test]
    async fn speed_limiter_is_written_in_kib() {
        let mock = DelugeMock::new();
        let deluge = mock_downloader(mock.clone());
        deluge
            .set_speed_limiter(1_048_576, 2_097_152)
            .await
            .unwrap();
        assert_eq!(mock.methods(), vec![M_AUTH_LOGIN, M_SET_CONFIG]);
        assert_eq!(
            mock.params_of(M_SET_CONFIG),
            json!([{ "max_download_speed": 2048, "max_upload_speed": 1024 }])
        );

        // 不限制：`isUploadUnlimited()` / `isDownloadUnlimited()`（<= 0）-> 0
        let mock = DelugeMock::new();
        let deluge = mock_downloader(mock.clone());
        deluge.set_speed_limiter(0, -1).await.unwrap();
        assert_eq!(
            mock.params_of(M_SET_CONFIG),
            json!([{ "max_download_speed": 0, "max_upload_speed": 0 }])
        );
    }

    /// 失败路径：RPC 失败时读取报错（上游返回 null）、下发只记日志；
    /// `config` 缺字段等价上游 NPE（同样报错）。
    #[tokio::test]
    async fn speed_limiter_failure_semantics_follow_upstream() {
        let mock = DelugeMock::new();
        *mock.rpc_error.lock().unwrap() = true;
        let deluge = mock_downloader(mock);
        assert!(deluge.get_speed_limiter().await.is_err());
        assert!(deluge.set_speed_limiter(1_048_576, 2_097_152).await.is_ok());

        let mock = DelugeMock::new();
        *mock.core_config.lock().unwrap() = json!({ "max_upload_speed": 1024 });
        let deluge = mock_downloader(mock);
        let err = deluge.get_speed_limiter().await.unwrap_err();
        assert!(err.to_string().contains("max_download_speed"), "{err}");

        let mock = DelugeMock::new();
        *mock.core_config.lock().unwrap() = Value::Null;
        let deluge = mock_downloader(mock);
        assert!(deluge.get_speed_limiter().await.is_err());
    }

    #[tokio::test]
    async fn feature_flags_are_abstract_downloader_defaults() {
        let mock = DelugeMock::new();
        let deluge = mock_downloader(mock);
        assert_eq!(deluge.feature_flags(), vec!["UNBAN_IP".to_string()]);
        assert_eq!(deluge.downloader_type(), "deluge");
    }

    #[test]
    fn peer_id_hex_decode_truncates_to_eight_chars() {
        // "-TR1000-" 的解码（长度恰好 8，不截断）
        assert_eq!(peer_id_from_hex("2d5452313030302d"), "-TR1000-");
        // 超长：截断到 8 个字符
        assert_eq!(
            peer_id_from_hex("2d5452313030302d6162636465666768696a"),
            "-TR1000-"
        );
        // 奇数长度：丢弃最后一个半字节（对齐 Java 的 `s.length() / 2`）
        assert_eq!(peer_id_from_hex("2d545"), "-T");
        // 非法十六进制：退回已解码前缀
        assert_eq!(peer_id_from_hex("2d5g"), "-");
        assert_eq!(peer_id_from_hex(""), "");
    }

    #[test]
    fn peer_flag_string_follows_peerflag_tostring() {
        // 空标志：!remoteChoked && !interesting -> "K"，!choked && !remoteInterested -> "?"，
        // !localConnection -> "I"
        assert_eq!(peer_flag_string(0, 0), "K ? I");
        // interesting+remoteChoked -> "d"；remoteInterested+choked -> "u"；
        // 未置位 localConnection（bit 6）-> "I"
        assert_eq!(peer_flag_string(0b1 | 0b1000 | 0b100 | 0b10, 0), "d u I");
        // 置位 localConnection 后不再输出 "I"
        assert_eq!(
            peer_flag_string(0b1 | 0b1000 | 0b100 | 0b10 | (1 << 6), 0),
            "d u"
        );
        // utp/加密 socket
        assert_eq!(
            peer_flag_string((1 << 17) | (1 << 19) | (1 << 20), 0),
            "K ? I E e P"
        );
    }
}
