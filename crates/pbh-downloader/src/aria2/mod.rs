//! Aria2Next 适配器，忠实复刻上游 `downloader/impl/aria2next/Aria2Next`。
//!
//! 要点：
//! - JSON-RPC 2.0 over HTTP：`{"jsonrpc":"2.0","id":<UUID>,"method":...,"params":[...]}`
//!   （对齐 `JsonRpcRequest`）。RPC 密钥有两处：OkHttp interceptor 给**每个**请求加
//!   `Content-Type: application/json` 与 `Authorization: token:<secret>`，
//!   且 `buildRpcRequest` 在 token 非空时把它作为 **`params` 的第一个元素**
//!   （`"token:" + token`）。两处都必须保留。
//! - 版本门槛：只支持 Aria2Next 分支——`aria2.getVersion` 的 `product` 必须是 `aria2-next`，
//!   否则登录失败并给出 `DOWNLOADER_ARIA2NEXT_INCORRECT_PRODUCT`（`MISSING_COMPONENTS`）。
//!   注意 `login0()` 不经过 `sendRpcRequest`：HTTP 非 2xx 时 401/403 报凭据错误、
//!   其余报 `DOWNLOADER_LOGIN_EXCEPTION("statusCode=" + code)`；HTTP 2xx 但 `result`
//!   或 `result.version` 缺失时同样走 `EXCEPTION`（`statusCode=` 仍是 200）。
//! - 类型宽松：aria2 把数字与布尔编码为字符串，DTO 用 Gson 风格的宽松转换（见 [`dto`]）。
//! - torrents：`fetch_torrents` 对齐 `getTorrents()`（**仅** `aria2.tellActive`），
//!   `fetch_all_torrents` 对齐 `getAllTorrents()`（`tellActive` + `tellWaiting` + `tellStopped`，
//!   失败时返回已收集到的部分结果）。两者都用 Java 里那一份 24 项的 `keys` 数组，
//!   并按 `bittorrent != null` 过滤掉非 BT 任务。注意上游调用 `tellWaiting`/`tellStopped`
//!   时**只**传了 `keys`（缺少 aria2 要求的 `offset`/`num`），真实 aria2 会返回参数错误，
//!   由上游的 catch 记日志并返回部分结果——本实现逐字保留该行为。
//! - `A2Task` 的映射：`id` 取 `gid`（peers 查询用）、`hash` 取 `infoHash`、
//!   `name` 取 `bittorrent.info.name`（缺失时回退 `files[0].path`，再回退 `"<METADATA> <gid>"`）、
//!   `progress = completedLength / totalLength`（`totalLength <= 0` 时为 0）、
//!   `size = totalLength`、完成量直接取 `completedLength`、速率取 `downloadSpeed`/`uploadSpeed`；
//!   `isPrivate()` 上游**恒为 false**（因此 `ignore-private` 配置在 Aria2Next 上不生效，
//!   上游也只是持久化该字段而从不读取）。
//! - peers：`aria2.getPeers` 的参数是 torrent 的 **gid**（`A2Task.getId()`），而 `TorrentData`
//!   只保留 infoHash，故 `fetch_torrents` 期间记录 infoHash→gid 映射（对齐上游 torrent 对象
//!   自己带着 gid）。`peerId` 经 `URLDecoder.decode(..., ISO_8859_1)` 解码（`%XX` 与 `+` 语义）；
//!   `ip`/`port` 走 `addressTranslate`（[`translate_peer_ip`]：Teredo / NAT64 / IPv4-mapped 归一），
//!   `raw_ip` 保留上游 `new PeerAddress(ip, port, ip)` 的「下载器原始 ip」（**不含端口**）。
//!   flags 由 `amChoking`/`amInterested`/`peerChoking`/`peerInterested`/`incoming`/`snubbed`
//!   组装，再按 `PeerFlag.toString()` 的顺序渲染为 libtorrent 风格字符串
//!   （`handshaking`/`seeder` 两个位无法由该字符串表达）。
//! - 封禁：Aria2Next **没有**增量封禁 API，`setBanList` 恒为整份替换
//!   （`aria2.setBtPeerBlocklist`，参数是「地址数组」这一个元素），地址逐个走
//!   `remapBanListAddress(addr, true)`（Aria2Next 声明 `RANGE_BAN_IP`）且**不去重**
//!   （上游此处没有 `distinct()`）。因此 [`Aria2Downloader::ban_peers`] 是显式的 no-op，
//!   `main.rs` 为该适配器固定 `increment_ban = false`。
//! - 特性标志（与 `getFeatureFlags()` 完全一致，含顺序）：`UNBAN_IP` /
//!   `LIVE_UPDATE_BT_PROTOCOL_PORT` / `RANGE_BAN_IP`（上游把 `TRAFFIC_STATS` 注释掉了）。
//! - 统计：Aria2Next **未覆写** `getStatistics()`，沿用 `AbstractDownloader` 的
//!   `new DownloaderStatistics(0, 0)`；bean 里的 `A2GlobalStat` 没有任何调用方。
//!
//! 不在本阶段接口内、因此未复刻的上游能力：
//! - `getTrackers` / `setTrackers`（`A2Task.bittorrent.announceList`）、
//!   `getSpeedLimiter` / `setSpeedLimiter`、`getBTProtocolPort` / `setBTProtocolPort`
//!   （`A2GlobalOptions` + `aria2.getGlobalOption` / `aria2.changeGlobalOption`）、
//!   `saveDownloader`：Rust `Downloader` 特性集里没有对应方法。
//! - `AbstractDownloader.login()` 的告警/退避机制（`AlertManager`、Sentry、
//!   `failedLoginAttempts` 达 15 次后 30 分钟冷却）与连接池
//!   `ConnectionPool(getMaxConcurrentPeerRequestSlots() + 10, …)`（并发槽由 ban wave 的
//!   全局信号量承担），与其它适配器保持一致。
//! - 上游 `sendRpcRequest` 在 `result` 为 `null` 时直接把它交给调用方（调用方随即 NPE）；
//!   本实现把该情况当作错误处理，由各调用路径记日志后返回空列表 / 忽略。

pub mod dto;

use crate::http::{BoxFuture, HttpFetcher, HttpRequest, ReqwestFetcher};
use crate::{BanEntry, Downloader, DownloaderFeature, DownloaderStatistics, LoginResult};
use dto::*;
use pbh_core::defaults::qb as qbcfg;
use pbh_core::i18n::{Param, TranslationComponent, Translator};
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::remap::{remap_ban_list_address, translate_peer_ip, RemapConfig};
use rand::RngCore;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tracing::{debug, error};

const M_GET_VERSION: &str = "aria2.getVersion";
const M_TELL_ACTIVE: &str = "aria2.tellActive";
const M_TELL_WAITING: &str = "aria2.tellWaiting";
const M_TELL_STOPPED: &str = "aria2.tellStopped";
const M_GET_PEERS: &str = "aria2.getPeers";
/// Aria2Next 的封禁列表扩展（aria2 原生没有封禁 API）。
const M_SET_BT_PEER_BLOCKLIST: &str = "aria2.setBtPeerBlocklist";
/// 对齐 `getSpeedLimiter()`：`aria2.getGlobalOption`（单位 bytes/s，字符串值）。
#[cfg(test)]
const M_GET_GLOBAL_OPTION: &str = "aria2.getGlobalOption";
/// 对齐 `setSpeedLimiter(...)`：`aria2.changeGlobalOption`。
#[cfg(test)]
const M_CHANGE_GLOBAL_OPTION: &str = "aria2.changeGlobalOption";

/// `Aria2Next.login0()` 要求的产品名（仅 Aria2Next 分支返回该值）。
const PRODUCT_ARIA2_NEXT: &str = "aria2-next";

/// 上游三处 `buildRpcRequest` 使用同一份 `keys` 数组（顺序与拼写逐字一致）。
const TELL_KEYS: [&str; 24] = [
    "gid",
    "status",
    "totalLength",
    "completedLength",
    "uploadLength",
    "bitfield",
    "downloadSpeed",
    "uploadSpeed",
    "infoHash",
    "numSeeders",
    "seeder",
    "pieceLength",
    "numPieces",
    "connections",
    "errorCode",
    "errorMessage",
    "followedBy",
    "following",
    "belongsTo",
    "dir",
    "bittorrent",
    "verifiedLength",
    "verifyIntegrityPending",
    "files",
];

/// `Lang` 的文案键（与上游逐一对应）。
const MSG_PAUSED: &str = "DOWNLOADER_PAUSED";
const MSG_STATUS_OK: &str = "STATUS_TEXT_OK";
const MSG_INCORRECT_PRODUCT: &str = "DOWNLOADER_ARIA2NEXT_INCORRECT_PRODUCT";
const MSG_JSONRPC_REQUEST_FAILED: &str = "DOWNLOADER_JSONRPC_REQUEST_FAILED";
const MSG_LOGIN_EXCEPTION: &str = "DOWNLOADER_LOGIN_EXCEPTION";
const MSG_LOGIN_INCORRECT_CRED: &str = "DOWNLOADER_LOGIN_INCORRECT_CRED";
const MSG_LOGIN_IO_EXCEPTION: &str = "DOWNLOADER_LOGIN_IO_EXCEPTION";

/// 对齐上游 `tlUI`：下载器不持有 locale，使用服务端默认文案语言。
const UI_LOCALE: &str = "zh_cn";

/// Aria2Next 下载器配置（对齐 `Aria2Next.Config`）。
#[derive(Clone, Debug)]
pub struct Aria2Config {
    pub id: String,
    pub name: String,
    /// RPC 端点（`http://host:port/jsonrpc`）
    pub endpoint: String,
    /// RPC 密钥；非空时既作为 `Authorization: token:<secret>` 头，也作为 params 首元素
    pub token: String,
    /// 是否校验 TLS 证书（上游 `verify-ssl`，默认 true）
    pub verify_ssl: bool,
    /// 上游 `ignore-private`（默认 false）。Aria2Next 仅持久化该字段，适配器逻辑从不读取
    /// （`A2Task.isPrivate()` 恒为 false）。
    pub ignore_private: bool,
    /// 启动即暂停（上游 `paused`，默认 false）
    pub paused: bool,
    /// `config.yml` 的 `banlist-remapping` / `ip-remapping` 配置
    pub remap: RemapConfig,
}

impl Default for Aria2Config {
    fn default() -> Self {
        Self {
            id: "aria2next".into(),
            name: "Aria2Next".into(),
            // 上游 `Config.readFromYaml` 未给 endpoint 默认值（`section.getString("endpoint")`）
            endpoint: String::new(),
            token: String::new(),
            verify_ssl: true,
            ignore_private: false,
            paused: false,
            remap: RemapConfig::default(),
        }
    }
}

pub struct Aria2Downloader {
    config: Aria2Config,
    /// 去掉结尾 `/` 后的 RPC 端点（对齐 `Config.readFromYaml`）
    endpoint: String,
    http: Arc<dyn HttpFetcher>,
    /// `aria2.getVersion` 协商到的版本（对齐上游 `lastSemver`：记录后上游从未读取）
    last_version: Mutex<String>,
    /// `aria2.getPeers` 需要 torrent 的 **gid**，而 `TorrentData` 只保留 infoHash：
    /// `fetch_torrents` 期间记录映射（对齐上游 `A2Task.getId()` 即 gid）。
    gid_by_hash: Mutex<HashMap<String, String>>,
}

impl Aria2Downloader {
    pub fn new(config: Aria2Config) -> anyhow::Result<Self> {
        let fetcher = Arc::new(ReqwestFetcher::new(
            config.verify_ssl,
            qbcfg::CONNECT_TIMEOUT_SECS,
            qbcfg::READ_TIMEOUT_SECS,
        )?);
        Self::with_fetcher(config, fetcher)
    }

    pub fn with_fetcher(config: Aria2Config, http: Arc<dyn HttpFetcher>) -> anyhow::Result<Self> {
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
            last_version: Mutex::new(String::new()),
            gid_by_hash: Mutex::new(HashMap::new()),
        })
    }

    /// 已协商到的 Aria2Next 版本（未登录时为空串）。
    pub fn version(&self) -> String {
        self.last_version.lock().map(|v| v.clone()).unwrap_or_default()
    }

    /// 对齐 `buildRpcRequest(method, customParams)`：token 前缀 + 随机 UUID id。
    ///
    /// OkHttp interceptor 的两个头（`Content-Type` / `Authorization: token:<secret>`）在此一并附加。
    fn build_rpc_request(&self, method: &str, custom_params: Vec<Value>) -> HttpRequest {
        let token = &self.config.token;
        let mut params: Vec<Value> = Vec::new();
        // `if (token != null && !token.isEmpty())`
        if !token.is_empty() {
            params.push(Value::String(format!("token:{token}")));
        }
        params.extend(custom_params);
        let body = serde_json::to_string(&JsonRpcRequest {
            jsonrpc: "2.0",
            id: rpc_id(),
            method: method.to_string(),
            params,
        })
        .unwrap_or_default();
        HttpRequest::post_json(self.endpoint.clone(), body)
            .with_header("Content-Type", "application/json")
            .with_header("Authorization", &format!("token:{token}"))
    }

    /// 对齐 `sendRpcRequest`：非 2xx 或 JSON-RPC `error` 非 null 时抛
    /// `DownloaderRequestException(tlUI(DOWNLOADER_JSONRPC_REQUEST_FAILED, …))`。
    async fn send_rpc_request<T: DeserializeOwned>(
        &self,
        method: &str,
        custom_params: Vec<Value>,
    ) -> anyhow::Result<T> {
        let resp = self
            .http
            .execute(self.build_rpc_request(method, custom_params))
            .await?;
        if !is_success(resp.status) {
            anyhow::bail!("{}", self.rpc_failed_message(resp.status, "N/A", &resp.body));
        }
        debug!("Aria2Next RPC Response: {}", resp.body);
        let parsed: JsonRpcResponse<T> = serde_json::from_str(&resp.body)?;
        // 「正确的 JsonRPC 判断标准：优先看 error 字段是否非空」
        if let Some(err) = &parsed.error {
            anyhow::bail!(
                "{}",
                self.rpc_failed_message(
                    resp.status,
                    &err.code.to_string(),
                    err.message.as_deref().unwrap_or("null"),
                )
            );
        }
        // 上游把 `null` 结果直接返回给调用方（调用方 NPE）；此处按错误处理
        parsed
            .result
            .ok_or_else(|| anyhow::anyhow!("JsonRpcResponse.result is null: method={method}"))
    }

    /// 对齐 `tlUI(Lang.DOWNLOADER_JSONRPC_REQUEST_FAILED, getName(), getEndpoint(), code, …)`。
    fn rpc_failed_message(&self, status: u16, code: &str, message: &str) -> String {
        tl(
            MSG_JSONRPC_REQUEST_FAILED,
            vec![
                Param::Text(self.config.name.clone()),
                Param::Text(self.config.endpoint.clone()),
                Param::Text(status.to_string()),
                Param::Text(code.to_string()),
                Param::Text(message.to_string()),
            ],
        )
    }

    /// `keys` 参数（`List.of(List.of(...))` → params 里的**一个数组元素**）。
    fn tell_keys_param() -> Value {
        Value::Array(TELL_KEYS.iter().map(|k| Value::String(k.to_string())).collect())
    }

    /// 对齐 `sendRpcRequest(buildRpcRequest(method, List.of(List.of(keys))), …)`。
    async fn tell(&self, method: &str) -> anyhow::Result<Vec<A2Task>> {
        self.send_rpc_request(method, vec![Self::tell_keys_param()]).await
    }

    /// 对齐 `A2Task.getName()`。
    fn task_name(task: &A2Task) -> String {
        if let Some(info) = task.bittorrent.as_ref().and_then(|b| b.info.as_ref()) {
            return info.name.clone().unwrap_or_default();
        }
        if let Some(first) = task.files.as_ref().and_then(|files| files.first()) {
            return first.path.clone().unwrap_or_default();
        }
        format!("<METADATA> {}", task.gid.clone().unwrap_or_default())
    }

    /// 对齐 `getTorrents()`/`getAllTorrents()` 里把 `A2Task` 映射为 `TorrentImpl` 的部分。
    fn torrent_from(&self, task: &A2Task) -> TorrentData {
        TorrentData {
            // `getHash()` → infoHash（`getId()` → gid，仅用于 peers 查询）
            hash: task.info_hash.clone().unwrap_or_default(),
            name: Self::task_name(task),
            // `getProgress()`：`totalLength <= 0` 时为 0.0
            progress: if task.total_length <= 0 {
                0.0
            } else {
                task.completed_length as f64 / task.total_length as f64
            },
            // `getSize()` → totalLength
            total_size: task.total_length,
            // A2Task 不提供 piece 信息，完成量直接用 `completedLength`
            piece_size: 0,
            pieces_have: 0,
            completed_override: Some(task.completed_length),
            dlspeed: task.download_speed,
            upspeed: task.upload_speed,
            // `A2Task.isPrivate()` 恒为 false
            is_private: Some(false),
        }
    }

    /// 对齐 `getPeers(torrent)` 的映射部分（`peek(peer -> peer.setPeerAddress(addressTranslate(...)))`）。
    fn peer_from(&self, peer: &A2Peer) -> PeerData {
        // 上游 `new PeerAddress(ip, port, ip)`：raw 部分就是下载器上报的 ip（不含端口）。
        // ip 缺失时上游会 NPE（`IPAddressUtil.getIPAddress(null)`），此处按空串继续映射。
        let raw_ip = peer.ip.clone().unwrap_or_default();
        let raw_port = peer.port.clamp(0, u16::MAX as i32) as u16;
        // 对齐 `addressTranslate(new PeerAddress(ip, port, ip))`
        let (ip, port) = translate_peer_ip(&raw_ip, raw_port, &self.config.remap.ip_remapping);
        PeerData {
            // `getClientName()`：null → ""
            client_name: Some(peer.peer_client_name.clone().unwrap_or_default()),
            // `getPeerId()`：null → ""，否则 `URLDecoder.decode(..., ISO_8859_1)`
            peer_id: Some(
                url_decode_iso_8859_1(peer.peer_id.as_deref().unwrap_or_default()),
            ),
            dl_speed: peer.download_speed,
            downloaded: peer.downloaded,
            up_speed: peer.upload_speed,
            uploaded: peer.uploaded,
            progress: peer.progress,
            // `getFlags()` 恒返回 PeerFlag（非 null）
            flags: Some(peer_flag_string(
                peer.am_choking,
                peer.am_interested,
                peer.peer_choking,
                peer.peer_interested,
                // `localConnection = !incoming`
                !peer.incoming,
                peer.snubbed,
            )),
            ip,
            port,
            raw_ip,
            // 上游 `A2Peer` 无连接类型字段
            connection: None,
        }
    }

    /// `fetch_peers` 需要的 gid（对齐 `torrent.getId()` = `A2Task.gid`）。
    fn gid_of(&self, torrent: &TorrentData) -> String {
        self.gid_by_hash
            .lock()
            .ok()
            .and_then(|map| map.get(&torrent.hash).cloned())
            .unwrap_or_else(|| torrent.id().to_string())
    }

    /// 记录 infoHash → gid（供 `fetch_peers` 使用）。
    fn cache_gids(&self, tasks: &[A2Task]) {
        let Ok(mut map) = self.gid_by_hash.lock() else {
            return;
        };
        for task in tasks {
            if let Some(gid) = &task.gid {
                map.insert(task.info_hash.clone().unwrap_or_default(), gid.clone());
            }
        }
    }

    /// 对齐 `getAllTorrents()`：`tellActive` + `tellWaiting` + `tellStopped`，
    /// 任一步失败只记日志并返回已收集到的部分结果。
    pub async fn fetch_all_torrents(&self) -> Vec<TorrentData> {
        let mut tasks: Vec<A2Task> = Vec::new();
        for method in [M_TELL_ACTIVE, M_TELL_WAITING, M_TELL_STOPPED] {
            match self.tell(method).await {
                Ok(batch) => {
                    let batch: Vec<A2Task> =
                        batch.into_iter().filter(|t| t.bittorrent.is_some()).collect();
                    self.cache_gids(&batch);
                    tasks.extend(batch);
                }
                Err(e) => {
                    error!("Error on request while getting all torrents: {e}");
                    break;
                }
            }
        }
        tasks.iter().map(|t| self.torrent_from(t)).collect()
    }
}

impl Downloader for Aria2Downloader {
    fn id(&self) -> &str {
        &self.config.id
    }
    fn name(&self) -> &str {
        &self.config.name
    }
    fn downloader_type(&self) -> &'static str {
        "aria2"
    }

    /// 对齐 `Aria2Next.getFeatureFlags()`：集合与顺序均与上游一致
    /// （`TRAFFIC_STATS` 在上游被注释掉，故不声明）。
    fn feature_flags(&self) -> Vec<String> {
        vec![
            DownloaderFeature::UnbanIp.name().to_string(),
            DownloaderFeature::LiveUpdateBtProtocolPort.name().to_string(),
            DownloaderFeature::RangeBanIp.name().to_string(),
        ]
    }

    /// 对齐 `login0()`（外层 `AbstractDownloader.login()` 的暂停短路一并复刻）。
    fn login<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<LoginResult>> {
        Box::pin(async move {
            if self.config.paused {
                return Ok(LoginResult {
                    success: false,
                    message: tl(MSG_PAUSED, Vec::new()),
                    version: self.version(),
                });
            }
            // `buildRpcRequest("aria2.getVersion", null)`
            let resp = match self
                .http
                .execute(self.build_rpc_request(M_GET_VERSION, Vec::new()))
                .await
            {
                Ok(resp) => resp,
                Err(e) => return Ok(login_io_exception(&e)),
            };
            if is_success(resp.status) {
                // 解析失败在 Java 里是 JsonSyntaxException，同样被最外层 catch 转成 NETWORK_ERROR
                let parsed: JsonRpcResponse<A2Version> = match serde_json::from_str(&resp.body) {
                    Ok(parsed) => parsed,
                    Err(e) => return Ok(login_io_exception(&anyhow::Error::from(e))),
                };
                if let Some(result) = parsed.result {
                    if let Some(version) = result.version.clone() {
                        // `if (!"aria2-next".equals(result.getProduct()))`：product 为 null 同样失败
                        if result.product.as_deref() != Some(PRODUCT_ARIA2_NEXT) {
                            debug!("Connected Aria2RPC server validation failure: {result:?}");
                            return Ok(LoginResult {
                                success: false,
                                message: tl(MSG_INCORRECT_PRODUCT, Vec::new()),
                                version,
                            });
                        }
                        // 对齐 `new Semver(version, LOOSE)`：不可解析时抛 SemverException
                        // （被最外层 catch 转成 NETWORK_ERROR）
                        if parse_loose_semver(&version).is_none() {
                            return Ok(LoginResult {
                                success: false,
                                message: tl(
                                    MSG_LOGIN_IO_EXCEPTION,
                                    vec![Param::Text(format!("SemverException: {version}"))],
                                ),
                                version,
                            });
                        }
                        if let Ok(mut last) = self.last_version.lock() {
                            *last = version.clone();
                        }
                        return Ok(LoginResult {
                            success: true,
                            message: tl(MSG_STATUS_OK, Vec::new()),
                            version,
                        });
                    }
                }
                // `result`/`version` 缺失：落到 login0 末尾的 EXCEPTION 分支
                return Ok(login_status_code_exception(resp.status, self.version()));
            }
            if resp.status == 401 || resp.status == 403 {
                return Ok(LoginResult {
                    success: false,
                    message: tl(MSG_LOGIN_INCORRECT_CRED, Vec::new()),
                    version: self.version(),
                });
            }
            Ok(login_status_code_exception(resp.status, self.version()))
        })
    }

    /// 对齐 `getTorrents()`：仅 `aria2.tellActive`；RPC 失败时记日志并返回空列表。
    fn fetch_torrents<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Vec<TorrentData>>> {
        Box::pin(async move {
            let tasks = match self.tell(M_TELL_ACTIVE).await {
                Ok(tasks) => tasks,
                Err(e) => {
                    error!("Error on request while getting torrents: {e}");
                    return Ok(Vec::new());
                }
            };
            // `.stream().filter(t -> t.getBittorrent() != null)`
            let tasks: Vec<A2Task> =
                tasks.into_iter().filter(|t| t.bittorrent.is_some()).collect();
            self.cache_gids(&tasks);
            Ok(tasks.iter().map(|t| self.torrent_from(t)).collect())
        })
    }

    /// 对齐 `getPeers(torrent)`：`aria2.getPeers` 的唯一参数是 gid；失败返回空列表。
    fn fetch_peers<'a>(
        &'a self,
        torrent: &'a TorrentData,
    ) -> BoxFuture<'a, anyhow::Result<Vec<PeerData>>> {
        Box::pin(async move {
            let gid = self.gid_of(torrent);
            let peers = match self
                .send_rpc_request::<Vec<A2Peer>>(M_GET_PEERS, vec![Value::String(gid)])
                .await
            {
                Ok(peers) => peers,
                Err(e) => {
                    error!("Error on request while getting peers: {e}");
                    return Ok(Vec::new());
                }
            };
            Ok(peers.iter().map(|p| self.peer_from(p)).collect())
        })
    }

    /// Aria2Next 没有增量封禁 API：`setBanList` 忽略 `added`/`removed`/`applyFullList`，
    /// 恒为整份列表替换。因此本方法是显式 no-op（`main.rs` 为该适配器固定
    /// `increment_ban = false`，该路径不会被选中），仅作为安全网保留。
    fn ban_peers<'a>(&'a self, _peers: &'a [BanEntry]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move { Ok(()) })
    }

    /// 对齐 `setBanList`：`aria2.setBtPeerBlocklist`，整份列表替换。
    ///
    /// 地址逐个走 `remapBanListAddress(i, true)`（Aria2Next 声明 `RANGE_BAN_IP`），
    /// **不做跨地址去重**（上游此处没有 `distinct()`）；失败只记日志。
    fn replace_banned_ips<'a>(&'a self, ips: &'a [String]) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let mut addresses: Vec<Value> = Vec::new();
            for ip in ips {
                for mapped in remap_ban_list_address(ip, true, &self.config.remap) {
                    addresses.push(Value::String(mapped));
                }
            }
            match self
                .send_rpc_request::<A2SetBtPeerBlocklist>(
                    M_SET_BT_PEER_BLOCKLIST,
                    vec![Value::Array(addresses)],
                )
                .await
            {
                Ok(result) => debug!(
                    "Aria2Next downloader {} now at revision {} with {} rows",
                    self.config.endpoint,
                    java_optional(result.revision),
                    java_optional(result.rule_count)
                ),
                Err(e) => error!("Error on request while setting banlist: {e}"),
            }
            Ok(())
        })
    }

    /// 对齐 `Aria2Next.getSpeedLimiter()`：`aria2.getGlobalOption`，单位 **bytes/s**（字符串形式）。
    fn get_speed_limiter<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<(i64, i64)>> {
        Box::pin(async move {
            let opts: HashMap<String, String> = self
                .send_rpc_request("aria2.getGlobalOption", Vec::new())
                .await?;
            let upload = opts
                .get("max-overall-upload-limit")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            let download = opts
                .get("max-overall-download-limit")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            Ok((upload, download))
        })
    }

    /// 对齐 `Aria2Next.setSpeedLimiter(...)`：`aria2.changeGlobalOption`，
    /// 负载 `{"max-overall-upload-limit": ..., "max-overall-download-limit": ...}`（字符串；0 = 不限制）。
    /// 返回非 `OK` → 抛错。
    fn set_speed_limiter<'a>(
        &'a self,
        upload: i64,
        download: i64,
    ) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let result: String = self
                .send_rpc_request(
                    "aria2.changeGlobalOption",
                    vec![serde_json::json!({
                        "max-overall-upload-limit": upload.to_string(),
                        "max-overall-download-limit": download.to_string(),
                    })],
                )
                .await?;
            if !result.eq_ignore_ascii_case("OK") {
                anyhow::bail!("Aria2Next setSpeedLimiter returned non-OK result: {result}");
            }
            Ok(())
        })
    }

    /// 对齐 `getStatistics()`：Aria2Next **未覆写**，沿用 `AbstractDownloader` 的
    /// `new DownloaderStatistics(0, 0)`（不发起任何 RPC）。
    fn statistics<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<DownloaderStatistics>> {
        Box::pin(async move { Ok(DownloaderStatistics::default()) })
    }
}

/// `UUID.randomUUID()` 的等价物（版本 4、变体 10 的随机 UUID 字符串，用作 JSON-RPC `id`）。
fn rpc_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 对齐 `URLDecoder.decode(peerId, ISO_8859_1)`：`%XX` 解码为字节、`+` 解码为空格，
/// 其余字符按字节原样保留，最后逐字节按 ISO-8859-1 映射为字符。
///
/// 上游遇到不完整的转义序列会抛 `IllegalArgumentException`（由调用链上层处理）；
/// 此处把非法 `%` 序列按字面字符保留。
fn url_decode_iso_8859_1(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let (hi, lo) = (bytes[i + 1] as char, bytes[i + 2] as char);
                match (hi.to_digit(16), lo.to_digit(16)) {
                    (Some(hi), Some(lo)) => {
                        out.push(char::from((hi * 16 + lo) as u8));
                        i += 3;
                    }
                    _ => {
                        out.push('%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(' ');
                i += 1;
            }
            byte => {
                out.push(char::from(byte));
                i += 1;
            }
        }
    }
    out
}

/// 对齐 `A2Peer.getFlags()` 构造的 `PeerFlag` + `PeerFlag.toString()` 的输出顺序。
///
/// 上游 `PeerFlag.builder()` 只显式赋了 `interesting`（`amInterested`）、`choked`（`amChoking`）、
/// `remoteInterested`（`peerInterested`）、`remoteChoked`（`peerChoking`）、`handshake`
/// （`handshaking`）、`localConnection`（`!incoming`）、`snubbed`、`seed`（`seeder`），
/// 其余位（含 `optimisticUnchoke`、`utpSocket`、`rc4Encrypted`、来源位）全为 `false`。
///
/// `handshake` 与 `seed` 在 `toString()` 里不输出，`PeerData` 只能保存该字符串，
/// 故这两位的语义无法表达（`PeerData::is_handshaking()` 改由速率推导，与其它适配器一致）。
fn peer_flag_string(
    am_choking: bool,
    am_interested: bool,
    peer_choking: bool,
    peer_interested: bool,
    local_connection: bool,
    snubbed: bool,
) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if am_interested {
        parts.push(if peer_choking { "d" } else { "D" });
    }
    if peer_interested {
        parts.push(if am_choking { "u" } else { "U" });
    }
    if !peer_choking && !am_interested {
        parts.push("K");
    }
    if !am_choking && !peer_interested {
        parts.push("?");
    }
    if snubbed {
        parts.push("S");
    }
    if !local_connection {
        parts.push("I");
    }
    parts.join(" ")
}

/// 宽松解析 `major[.minor[.patch]]`（对齐 semver4j 的 LOOSE 模式）：
/// 允许 `v` 前缀与预发布/构建后缀，多余分量忽略；不可解析返回 `None`。
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

/// 取字符串的前导数字（如 `1`、`37`）。
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

/// 对齐 `new TranslationComponent(Lang.DOWNLOADER_LOGIN_EXCEPTION, "statusCode=" + response.code())`。
fn login_status_code_exception(status: u16, version: String) -> LoginResult {
    LoginResult {
        success: false,
        message: tl(
            MSG_LOGIN_EXCEPTION,
            vec![Param::Text(format!("statusCode={status}"))],
        ),
        version,
    }
}

/// 对齐最外层 `catch (Exception e)` →
/// `new TranslationComponent(Lang.DOWNLOADER_LOGIN_IO_EXCEPTION, e.getClass().getName() + ": " + e.getMessage())`。
fn login_io_exception(e: &anyhow::Error) -> LoginResult {
    let (class, message) = exception_params(e);
    LoginResult {
        success: false,
        message: tl(
            MSG_LOGIN_IO_EXCEPTION,
            vec![Param::Text(format!("{class}: {message}"))],
        ),
        version: String::new(),
    }
}

/// 对齐上游 `e.getClass().getName()` 与 `e.getMessage()` 两个占位参数。
///
/// Rust 无法在运行时取得动态类型名（`dyn Error` 的 `type_name` 只会给出 trait 名），
/// 故类名位置固定填 `anyhow::Error`，消息位置取错误链根因的 `Display`。
fn exception_params(e: &anyhow::Error) -> (String, String) {
    ("anyhow::Error".to_string(), e.root_cause().to_string())
}

/// Java `Integer` 的 `String.valueOf` / 字符串拼接语义（`null` 输出 `null`）。
fn java_optional(value: Option<i32>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "null".to_string(),
    }
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

    /// `aria2.getVersion`：Aria2Next 分支（`product` = `aria2-next`）。
    const VERSION_NEXT: &str = r#"{
      "enabledFeatures": ["Async DNS", "BitTorrent", "Firefox3 Cookie"],
      "product": "aria2-next",
      "rpcVersion": "1.3.0",
      "version": "1.37.0"
    }"#;

    /// `aria2.tellActive`：BT 任务（数值/布尔都是字符串）+ 非 BT 任务。
    const ACTIVE: &str = r#"[
      {
        "gid": "gid-1",
        "status": "active",
        "totalLength": "1000",
        "completedLength": "425",
        "uploadLength": "500",
        "bitfield": "f0",
        "downloadSpeed": "20",
        "uploadSpeed": "10",
        "infoHash": "1111111111111111111111111111111111111111",
        "numSeeders": "2",
        "seeder": "false",
        "pieceLength": "262144",
        "numPieces": "4",
        "connections": "3",
        "dir": "/downloads",
        "bittorrent": {
          "announceList": [["http://tracker.example/announce"]],
          "creationDate": "1700000000",
          "info": { "name": "ubuntu.iso" },
          "magnetLink": "magnet:?xt=urn:btih:1111",
          "mode": "multi"
        }
      },
      {
        "gid": "gid-2",
        "status": "active",
        "totalLength": "2048",
        "completedLength": "0",
        "downloadSpeed": "0",
        "uploadSpeed": "0",
        "infoHash": "2222222222222222222222222222222222222222",
        "seeder": "true",
        "bittorrent": { "mode": "single", "info": null },
        "files": [
          { "index": "1", "path": "/downloads/unnamed.bin", "length": "2048",
            "completedLength": "0", "selected": "true", "uris": [] }
        ]
      },
      { "gid": "gid-3", "status": "active", "totalLength": "512", "files": [] }
    ]"#;

    /// `aria2.tellWaiting` / `aria2.tellStopped`：各一个 BT 任务。
    const WAITING: &str = r#"[
      { "gid": "gid-4", "status": "waiting", "totalLength": "4096",
        "completedLength": "1024", "infoHash": "4444444444444444444444444444444444444444",
        "bittorrent": { "info": { "name": "waiting.iso" } } }
    ]"#;
    const STOPPED: &str = r#"[
      { "gid": "gid-5", "status": "removed", "totalLength": "8192",
        "completedLength": "8192", "infoHash": "5555555555555555555555555555555555555555",
        "bittorrent": { "info": { "name": "stopped.iso" } } }
    ]"#;

    /// `aria2.getPeers`：普通 peer / NAT64 peer（`+` 出现在 peerId 中）。
    const PEERS: &str = r#"[
      {
        "amChoking": "false",
        "amInterested": "true",
        "bitfield": "ffff",
        "completedLength": "0",
        "downloadSpeed": "202",
        "downloaded": "404",
        "flags": "d?",
        "handshaking": "false",
        "incoming": "false",
        "ip": "9.9.9.9",
        "optimisticUnchoke": "false",
        "peerChoking": "true",
        "peerClientName": "-TR1000-abcdefghij",
        "peerId": "%2DTR1000%2Dabcdefghij",
        "peerInterested": "false",
        "port": "6881",
        "progress": "0.555",
        "seeder": "false",
        "snubbed": "false",
        "uploadSpeed": "101",
        "uploaded": "303"
      },
      {
        "amChoking": "true",
        "amInterested": "false",
        "incoming": "true",
        "ip": "64:ff9b::102:304",
        "peerChoking": "false",
        "peerClientName": null,
        "peerId": "a+b",
        "peerInterested": "true",
        "port": "7001",
        "progress": "0.1",
        "seeder": "true",
        "snubbed": "true",
        "downloadSpeed": "1",
        "uploadSpeed": "2",
        "downloaded": "3",
        "uploaded": "4"
      }
    ]"#;

    /// 被记录到的请求。
    #[derive(Clone, Debug)]
    struct Recorded {
        method: String,
        url: String,
        headers: Vec<(String, String)>,
        body: Value,
    }

    impl Recorded {
        fn header(&self, name: &str) -> String {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        }

        /// 该方法的 `params` 数组。
        fn params(&self) -> Value {
            self.body["params"].clone()
        }
    }

    /// 成功信封（`id` 固定；解析侧不依赖它，仅作 JSON-RPC 完整性）。
    fn ok(result: &str) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":"test","result":{result},"error":null}}"#)
    }

    /// JSON-RPC 错误信封。
    fn rpc_error(code: i64, message: &str) -> String {
        format!(
            r#"{{"jsonrpc":"2.0","id":"test","result":null,"error":{{"code":{code},"message":{}}}}}"#,
            serde_json::to_string(message).unwrap_or_default()
        )
    }

    /// Aria2Next RPC 的内存实现：按 method 返回夹具（完整响应体）并记录收到的请求。
    struct Aria2Mock {
        requests: Mutex<Vec<Recorded>>,
        version: Mutex<(u16, String)>,
        active: Mutex<(u16, String)>,
        waiting: Mutex<(u16, String)>,
        stopped: Mutex<(u16, String)>,
        peers: Mutex<(u16, String)>,
        blocklist: Mutex<(u16, String)>,
        /// 为 true 时所有请求都返回传输层错误
        transport_error: Mutex<bool>,
        /// 对齐 `getSpeedLimiter()`：`aria2.getGlobalOption` 的返回值。
        global_option: Mutex<(u16, String)>,
        /// 对齐 `setSpeedLimiter(...)`：`aria2.changeGlobalOption` 的返回值（一般为 "OK"）。
        set_global_option: Mutex<(u16, String)>,
    }

    impl Aria2Mock {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
                version: Mutex::new((200, ok(VERSION_NEXT))),
                active: Mutex::new((200, ok(ACTIVE))),
                waiting: Mutex::new((200, ok(WAITING))),
                stopped: Mutex::new((200, ok(STOPPED))),
                peers: Mutex::new((200, ok(PEERS))),
                blocklist: Mutex::new((
                    200,
                    ok(r#"{"disconnectedPeers":0,"removedPeers":1,"revision":3,"ruleCount":2}"#),
                )),
                transport_error: Mutex::new(false),
                global_option: Mutex::new((
                    200,
                    r#"{"result":{"max-overall-upload-limit":"1048576","max-overall-download-limit":"2097152"}}"#
                        .to_string(),
                )),
                set_global_option: Mutex::new((
                    200,
                    r#"{"result":"OK"}"#.to_string(),
                )),
            })
        }

        fn requests(&self) -> Vec<Recorded> {
            self.requests.lock().unwrap().clone()
        }

        fn methods(&self) -> Vec<String> {
            self.requests().into_iter().map(|r| r.method).collect()
        }

        /// 某个方法收到的第一个请求
        fn request(&self, method: &str) -> Recorded {
            self.requests()
                .into_iter()
                .find(|r| r.method == method)
                .unwrap_or_else(|| panic!("未收到 {method} 请求"))
        }
    }

    impl HttpFetcher for Aria2Mock {
        fn execute<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, anyhow::Result<HttpResponse>> {
            Box::pin(async move {
                if *self.transport_error.lock().unwrap() {
                    anyhow::bail!("error sending request for url ({})", req.url);
                }
                let body: Value =
                    serde_json::from_str(req.body.as_deref().unwrap_or_default()).unwrap();
                let method = body["method"].as_str().unwrap_or_default().to_string();
                self.requests.lock().unwrap().push(Recorded {
                    method: method.clone(),
                    url: req.url.clone(),
                    headers: req.headers.clone(),
                    body,
                });
                let (status, response) = match method.as_str() {
                    M_GET_VERSION => self.version.lock().unwrap().clone(),
                    M_TELL_ACTIVE => self.active.lock().unwrap().clone(),
                    M_TELL_WAITING => self.waiting.lock().unwrap().clone(),
                    M_TELL_STOPPED => self.stopped.lock().unwrap().clone(),
                    M_GET_PEERS => self.peers.lock().unwrap().clone(),
                    M_SET_BT_PEER_BLOCKLIST => self.blocklist.lock().unwrap().clone(),
                    M_GET_GLOBAL_OPTION => self.global_option.lock().unwrap().clone(),
                    M_CHANGE_GLOBAL_OPTION => self.set_global_option.lock().unwrap().clone(),
                    other => (404, rpc_error(-32601, &format!("unknown method {other}"))),
                };
                Ok(HttpResponse::new(status, response))
            })
        }
    }

    fn downloader(mock: Arc<Aria2Mock>) -> Arc<Aria2Downloader> {
        let config = Aria2Config {
            id: "aria2-test".into(),
            name: "Aria2Next Test".into(),
            // 结尾斜杠用于验证「只去掉一个」
            endpoint: "http://mock.local/".into(),
            token: "secret".into(),
            ..Aria2Config::default()
        };
        Arc::new(Aria2Downloader::with_fetcher(config, mock as Arc<dyn HttpFetcher>).unwrap())
    }

    /// 对齐上游 OkHttp interceptor：每个请求都必须带这两个头。
    fn assert_common_headers(req: &Recorded) {
        assert_eq!(req.url, "http://mock.local", "结尾斜杠应被去掉一个");
        assert_eq!(
            req.header("Content-Type"),
            "application/json",
            "缺少 Content-Type: {:?}",
            req.headers
        );
        assert_eq!(
            req.header("Authorization"),
            "token:secret",
            "缺少 Authorization: {:?}",
            req.headers
        );
    }

    fn torrent_by_hash<'a>(torrents: &'a [TorrentData], hash: &str) -> &'a TorrentData {
        torrents
            .iter()
            .find(|t| t.hash == hash)
            .unwrap_or_else(|| panic!("未找到 torrent {hash}"))
    }

    #[tokio::test]
    async fn login_succeeds_only_for_aria2next_product() {
        let mock = Aria2Mock::new();
        let dl = downloader(mock.clone());
        let login = dl.login().await.unwrap();
        assert!(login.success, "{}", login.message);
        assert_eq!(login.version, "1.37.0");
        assert_eq!(dl.version(), "1.37.0");
        assert!(login.message.contains("工作正常"), "{}", login.message);

        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert_common_headers(&requests[0]);
        assert_eq!(requests[0].method, M_GET_VERSION);
        // token 只作为 params 的首元素出现；`aria2.getVersion` 没有自定义参数
        assert_eq!(requests[0].params(), serde_json::json!(["token:secret"]));
        assert_eq!(
            requests[0].body["jsonrpc"], "2.0",
            "JSON-RPC 版本必须为 2.0"
        );
        assert!(
            requests[0].body["id"].as_str().unwrap_or_default().len() == 36,
            "id 应为 UUID 字符串: {}",
            requests[0].body["id"]
        );
    }

    #[tokio::test]
    async fn login_rejects_non_aria2next_products() {
        // 官方 aria2 / 其它分支：product 不是 aria2-next
        let mock = Aria2Mock::new();
        *mock.version.lock().unwrap() = (
            200,
            ok(r#"{"product":"aria2","rpcVersion":"1.3.0","version":"1.37.0"}"#),
        );
        let dl = downloader(mock.clone());
        let login = dl.login().await.unwrap();
        assert!(!login.success);
        assert!(login.message.contains("Aria2Next"), "{}", login.message);
        assert_eq!(login.version, "1.37.0");
        assert_eq!(dl.version(), "", "product 不对时不应记录版本");

        // product 缺失同样是失败
        let mock = Aria2Mock::new();
        *mock.version.lock().unwrap() = (200, ok(r#"{"version":"1.37.0"}"#));
        let dl = downloader(mock);
        let login = dl.login().await.unwrap();
        assert!(!login.success);
        assert!(login.message.contains("Aria2Next"), "{}", login.message);
    }

    #[tokio::test]
    async fn login_reports_missing_fields_http_and_network_failures() {
        // HTTP 2xx 但没有 result / version → EXCEPTION(statusCode=200)
        let mock = Aria2Mock::new();
        *mock.version.lock().unwrap() = (200, ok(r#"{"product":"aria2-next"}"#));
        let dl = downloader(mock.clone());
        let login = dl.login().await.unwrap();
        assert!(!login.success);
        assert!(login.message.contains("statusCode=200"), "{}", login.message);

        // 版本号不是合法 semver → 对齐 `new Semver(version, LOOSE)` 抛异常
        let mock = Aria2Mock::new();
        *mock.version.lock().unwrap() = (
            200,
            ok(r#"{"product":"aria2-next","version":"not-a-version"}"#),
        );
        let dl = downloader(mock);
        let login = dl.login().await.unwrap();
        assert!(!login.success);
        assert!(login.message.contains("网络错误"), "{}", login.message);
        assert!(login.message.contains("SemverException"), "{}", login.message);

        // 401 / 403 → 凭据错误
        for status in [401u16, 403] {
            let mock = Aria2Mock::new();
            *mock.version.lock().unwrap() = (status, "Unauthorized".to_string());
            let dl = downloader(mock);
            let login = dl.login().await.unwrap();
            assert!(!login.success, "status={status}");
            assert!(login.message.contains("凭据"), "{}", login.message);
        }

        // 其它非 2xx → DOWNLOADER_LOGIN_EXCEPTION(statusCode=500)
        let mock = Aria2Mock::new();
        *mock.version.lock().unwrap() = (500, "boom".to_string());
        let dl = downloader(mock);
        let login = dl.login().await.unwrap();
        assert!(!login.success);
        assert!(login.message.contains("statusCode=500"), "{}", login.message);

        // 传输层失败 → NETWORK_ERROR（DOWNLOADER_LOGIN_IO_EXCEPTION）
        let mock = Aria2Mock::new();
        *mock.transport_error.lock().unwrap() = true;
        let dl = downloader(mock);
        let login = dl.login().await.unwrap();
        assert!(!login.success);
        assert!(login.message.contains("网络错误"), "{}", login.message);
    }

    #[tokio::test]
    async fn login_is_short_circuited_when_paused() {
        let mock = Aria2Mock::new();
        let config = Aria2Config {
            endpoint: "http://mock.local".into(),
            paused: true,
            ..Aria2Config::default()
        };
        let dl = Aria2Downloader::with_fetcher(config, mock.clone() as Arc<dyn HttpFetcher>).unwrap();
        let login = dl.login().await.unwrap();
        assert!(!login.success);
        assert!(login.message.contains("暂停"), "{}", login.message);
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn torrents_are_mapped_from_string_encoded_values() {
        let mock = Aria2Mock::new();
        let dl = downloader(mock.clone());
        let torrents = dl.fetch_torrents().await.unwrap();
        // 非 BT 任务（没有 bittorrent 字段）被过滤
        assert_eq!(torrents.len(), 2);

        let first = torrent_by_hash(&torrents, "1111111111111111111111111111111111111111");
        assert_eq!(first.name, "ubuntu.iso");
        // completedLength / totalLength = 425 / 1000
        assert_eq!(first.progress, 0.425);
        assert_eq!(first.total_size, 1000);
        assert_eq!(first.completed_override, Some(425));
        assert_eq!(first.completed_size(), 425);
        assert_eq!(first.dlspeed, 20);
        assert_eq!(first.upspeed, 10);
        // `A2Task.isPrivate()` 恒为 false
        assert!(!first.is_private());

        // `bittorrent.info` 缺失 → 回退 `files[0].path`
        let second = torrent_by_hash(&torrents, "2222222222222222222222222222222222222222");
        assert_eq!(second.name, "/downloads/unnamed.bin");
        assert_eq!(second.progress, 0.0);
        assert_eq!(second.completed_override, Some(0));

        // 只请求 active；`params` 是 [token, keys]
        assert_eq!(mock.methods(), vec![M_TELL_ACTIVE]);
        let request = mock.request(M_TELL_ACTIVE);
        assert_common_headers(&request);
        let params = request.params();
        assert_eq!(params[0], "token:secret");
        assert_eq!(
            params[1],
            serde_json::json!([
                "gid", "status", "totalLength", "completedLength",
                "uploadLength", "bitfield", "downloadSpeed",
                "uploadSpeed", "infoHash", "numSeeders",
                "seeder", "pieceLength", "numPieces", "connections",
                "errorCode", "errorMessage", "followedBy", "following", "belongsTo",
                "dir", "bittorrent", "verifiedLength", "verifyIntegrityPending", "files"
            ])
        );
    }

    /// `token` 为空时：interceptor 仍加 `Authorization: token:` 头，
    /// 但 `buildRpcRequest` 不再往 params 里塞 `token:` 元素。
    #[tokio::test]
    async fn empty_token_keeps_header_without_params_element() {
        let mock = Aria2Mock::new();
        let config = Aria2Config {
            endpoint: "http://mock.local".into(),
            token: String::new(),
            ..Aria2Config::default()
        };
        let dl =
            Aria2Downloader::with_fetcher(config, mock.clone() as Arc<dyn HttpFetcher>).unwrap();
        dl.fetch_torrents().await.unwrap();

        let request = mock.request(M_TELL_ACTIVE);
        assert_eq!(request.header("Authorization"), "token:");
        let params = request.params();
        let params = params.as_array().unwrap();
        assert_eq!(params.len(), 1, "无 token 时 params 只剩 keys: {params:?}");
        assert_eq!(params[0].as_array().unwrap().len(), TELL_KEYS.len());
    }

    #[tokio::test]
    async fn all_torrents_merge_active_waiting_and_stopped() {
        let mock = Aria2Mock::new();
        let dl = downloader(mock.clone());
        let torrents = dl.fetch_all_torrents().await;
        // active 2 个（1 个非 BT 被过滤）+ waiting 1 + stopped 1
        assert_eq!(torrents.len(), 4);
        assert!(torrents.iter().any(|t| t.name == "waiting.iso"));
        assert!(torrents.iter().any(|t| t.name == "stopped.iso"));
        assert_eq!(
            mock.methods(),
            vec![M_TELL_ACTIVE, M_TELL_WAITING, M_TELL_STOPPED]
        );
        // 三处使用同一份 keys 数组
        let keys = mock.request(M_TELL_ACTIVE).params()[1].clone();
        for method in [M_TELL_WAITING, M_TELL_STOPPED] {
            assert_eq!(mock.request(method).params()[1], keys, "{method}");
        }

        // 上游对 waiting/stopped 只传 keys（缺少 offset/num），真实 aria2 会报参数错误：
        // 该错误被记日志后返回**已收集到的部分结果**
        let mock = Aria2Mock::new();
        *mock.waiting.lock().unwrap() = (200, rpc_error(1, "Invalid parameter"));
        let dl = downloader(mock.clone());
        // 夹具是合法的 JSON 对象 → 反序列化 `Vec<A2Task>` 失败 → 与上游异常路径等价
        let torrents = dl.fetch_all_torrents().await;
        assert_eq!(torrents.len(), 2, "只保留 tellActive 的结果");
    }

    #[tokio::test]
    async fn speed_limiter_get_and_set() {
        let mock = Aria2Mock::new();
        *mock.global_option.lock().unwrap() = (
            200,
            r#"{"result":{"max-overall-upload-limit":"1048576","max-overall-download-limit":"2097152"}}"#
                .to_string(),
        );
        *mock.set_global_option.lock().unwrap() = (200, r#"{"result":"OK"}"#.to_string());
        let dl = downloader(mock.clone());
        // 对齐 `getSpeedLimiter()`：aria2.getGlobalOption 字符串值解析为 bytes/s
        let (up, dl_) = dl.get_speed_limiter().await.unwrap();
        assert_eq!(up, 1_048_576);
        assert_eq!(dl_, 2_097_152);
        // 对齐 `setSpeedLimiter(...)`：aria2.changeGlobalOption 返回 "OK"
        dl.set_speed_limiter(0, 0).await.unwrap();
        assert_eq!(mock.methods(), vec![M_GET_GLOBAL_OPTION, M_CHANGE_GLOBAL_OPTION]);
    }

    #[tokio::test]
    async fn peers_are_mapped_with_raw_ip_and_translation() {
        let mock = Aria2Mock::new();
        let dl = downloader(mock.clone());
        let torrents = dl.fetch_torrents().await.unwrap();
        let torrent = torrent_by_hash(&torrents, "1111111111111111111111111111111111111111");
        let peers = dl.fetch_peers(torrent).await.unwrap();
        assert_eq!(peers.len(), 2);

        // `aria2.getPeers` 的唯一参数是 gid（来自 `A2Task.getId()`）
        let request = mock.request(M_GET_PEERS);
        assert_common_headers(&request);
        assert_eq!(request.params(), serde_json::json!(["token:secret", "gid-1"]));

        let by_raw: HashMap<&str, &PeerData> =
            peers.iter().map(|p| (p.raw_ip.as_str(), p)).collect();

        // peerId 的 `%2D` 解码为 `-`；flags 由布尔位组装
        let normal = by_raw.get("9.9.9.9").expect("peer 9.9.9.9");
        assert_eq!(normal.ip, "9.9.9.9");
        assert_eq!(normal.port, 6881);
        assert_eq!(normal.peer_id.as_deref(), Some("-TR1000-abcdefghij"));
        assert_eq!(normal.client_name.as_deref(), Some("-TR1000-abcdefghij"));
        assert_eq!(normal.dl_speed, 202);
        assert_eq!(normal.up_speed, 101);
        assert_eq!(normal.downloaded, 404);
        assert_eq!(normal.uploaded, 303);
        assert_eq!(normal.progress, 0.555);
        // amInterested + peerChoking → "d"；!amChoking && !peerInterested → "?"；
        // incoming=false → localConnection → 不输出 "I"
        assert_eq!(normal.flags.as_deref(), Some("d ?"));
        assert!(normal.connection.is_none());

        // NAT64 地址翻译为 IPv4，raw_ip 保留下载器上报的 ip（不含端口）；
        // `+` 按 URLDecoder 解码为空格；入站连接 → "I"
        let nat64 = by_raw.get("64:ff9b::102:304").expect("NAT64 peer");
        assert_eq!(nat64.ip, "1.2.3.4");
        assert_eq!(nat64.port, 7001);
        assert_eq!(nat64.peer_id.as_deref(), Some("a b"));
        assert_eq!(nat64.client_name.as_deref(), Some(""));
        assert_eq!(nat64.progress, 0.1);
        // peerInterested + amChoking → "u"；!peerChoking && !amInterested → "K"；
        // snubbed → "S"；incoming=true → "I"
        assert_eq!(nat64.flags.as_deref(), Some("u K S I"));
    }

    #[tokio::test]
    async fn peers_failure_returns_empty_list() {
        let mock = Aria2Mock::new();
        let dl = downloader(mock.clone());
        let torrents = dl.fetch_torrents().await.unwrap();
        let torrent = torrent_by_hash(&torrents, "1111111111111111111111111111111111111111");

        // JSON-RPC error（如 gid 不存在）→ 记日志并返回空列表
        *mock.peers.lock().unwrap() = (200, rpc_error(1, "GID not found"));
        assert!(dl.fetch_peers(torrent).await.unwrap().is_empty());

        // HTTP 500 → 同样返回空列表
        *mock.peers.lock().unwrap() = (500, "boom".to_string());
        assert!(dl.fetch_peers(torrent).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn torrents_failure_returns_empty_list() {
        let mock = Aria2Mock::new();
        *mock.active.lock().unwrap() = (500, "boom".to_string());
        let dl = downloader(mock.clone());
        assert!(dl.fetch_torrents().await.unwrap().is_empty());

        let mock = Aria2Mock::new();
        *mock.transport_error.lock().unwrap() = true;
        let dl = downloader(mock);
        assert!(dl.fetch_torrents().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn full_blocklist_is_posted_without_dedup() {
        let mock = Aria2Mock::new();
        let dl = downloader(mock.clone());
        dl.replace_banned_ips(&[
            "1.2.3.4".to_string(),
            "2001:db8::1".to_string(),
            // 重复地址：上游此处没有 `distinct()`，应原样保留
            "1.2.3.4".to_string(),
        ])
        .await
        .unwrap();

        let request = mock.request(M_SET_BT_PEER_BLOCKLIST);
        assert_common_headers(&request);
        // 地址数组是**一个**参数（`List.of(fullList)`）
        assert_eq!(
            request.params(),
            serde_json::json!([
                "token:secret",
                ["1.2.3.4", "::ffff:1.2.3.4", "2001:db8::1", "2001:db8::/52", "1.2.3.4", "::ffff:1.2.3.4"]
            ])
        );
    }

    #[tokio::test]
    async fn blocklist_failure_is_logged_and_ignored() {
        let mock = Aria2Mock::new();
        *mock.blocklist.lock().unwrap() = (500, "boom".to_string());
        let dl = downloader(mock.clone());
        dl.replace_banned_ips(&["1.2.3.4".to_string()]).await.unwrap();
        assert_eq!(mock.methods(), vec![M_SET_BT_PEER_BLOCKLIST]);
    }

    /// Aria2Next 没有增量封禁 API：`ban_peers` 是显式 no-op（`main.rs` 固定增量关闭）。
    #[tokio::test]
    async fn ban_peers_is_an_explicit_no_op() {
        let mock = Aria2Mock::new();
        let dl = downloader(mock.clone());
        dl.ban_peers(&[BanEntry {
            ip: "1.2.3.4".into(),
            port: 6881,
            raw_ip: "1.2.3.4".into(),
        }])
        .await
        .unwrap();
        assert!(mock.requests().is_empty());
    }

    /// 对齐 `AbstractDownloader.getStatistics()` 的默认值，且不发任何 RPC。
    #[tokio::test]
    async fn statistics_are_inherited_zero_default() {
        let mock = Aria2Mock::new();
        let dl = downloader(mock.clone());
        let stats = dl.statistics().await.unwrap();
        assert_eq!(stats.all_time_upload, 0);
        assert_eq!(stats.all_time_download, 0);
        assert!(mock.requests().is_empty());
    }

    #[test]
    fn feature_flags_match_upstream_and_order() {
        let mock = Aria2Mock::new();
        let dl = downloader(mock);
        assert_eq!(
            dl.feature_flags(),
            vec![
                "UNBAN_IP".to_string(),
                "LIVE_UPDATE_BT_PROTOCOL_PORT".to_string(),
                "RANGE_BAN_IP".to_string(),
            ],
            "上游 `TRAFFIC_STATS` 被注释掉，不应声明"
        );
        assert_eq!(dl.downloader_type(), "aria2");
        assert_eq!(dl.id(), "aria2-test");
        assert_eq!(dl.name(), "Aria2Next Test");
    }

    #[test]
    fn peer_id_decoding_follows_url_decoder() {
        assert_eq!(url_decode_iso_8859_1("%2DTR1000%2Dabcdefghij"), "-TR1000-abcdefghij");
        // `+` → 空格；非转义字符原样保留；非法转义按字面保留
        assert_eq!(url_decode_iso_8859_1("a+b"), "a b");
        assert_eq!(url_decode_iso_8859_1("plain"), "plain");
        assert_eq!(url_decode_iso_8859_1("%zz"), "%zz");
        assert_eq!(url_decode_iso_8859_1("%2"), "%2");
        // ISO-8859-1：`%FF` 单字节映射为 U+00FF
        assert_eq!(url_decode_iso_8859_1("%FF"), "\u{ff}");
        assert_eq!(url_decode_iso_8859_1(""), "");
    }

    #[test]
    fn peer_flag_string_follows_peerflag_tostring() {
        // 全部布尔为 false：!peerChoking && !amInterested → "K"；!amChoking && !peerInterested → "?"；
        // incoming=false → localConnection=true → 不输出 "I"
        assert_eq!(peer_flag_string(false, false, false, false, true, false), "K ?");
        // 入站连接 → "I"
        assert_eq!(peer_flag_string(false, false, false, false, false, false), "K ? I");
        assert_eq!(
            peer_flag_string(false, true, true, false, true, false),
            "d ?"
        );
        assert_eq!(
            peer_flag_string(true, false, false, true, true, true),
            "u K S"
        );
        // amInterested + !peerChoking → "D"；peerInterested + !amChoking → "U"
        assert_eq!(
            peer_flag_string(false, true, false, true, true, false),
            "D U"
        );
    }

    #[test]
    fn task_name_falls_back_to_metadata_placeholder() {
        let task: A2Task =
            serde_json::from_str(r#"{"gid":"gid-x","totalLength":"1","files":[]}"#).unwrap();
        assert_eq!(Aria2Downloader::task_name(&task), "<METADATA> gid-x");
        let task: A2Task =
            serde_json::from_str(r#"{"gid":"gid-x","bittorrent":{"info":{"name":"n.iso"}}}"#)
                .unwrap();
        assert_eq!(Aria2Downloader::task_name(&task), "n.iso");
    }

    #[test]
    fn loose_semver_parsing_matches_semver4j() {
        assert_eq!(parse_loose_semver("1.37.0"), Some((1, 37, 0)));
        assert_eq!(parse_loose_semver("v1.37"), Some((1, 37, 0)));
        assert_eq!(parse_loose_semver("1"), Some((1, 0, 0)));
        assert_eq!(parse_loose_semver("1.37.0-devel"), Some((1, 37, 0)));
        assert_eq!(parse_loose_semver(""), None);
        assert_eq!(parse_loose_semver("aria2"), None);
    }

    #[test]
    fn rpc_ids_are_unique_uuids() {
        let a = rpc_id();
        let b = rpc_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        assert_eq!(&a[8..9], "-");
        assert_eq!(&a[13..14], "-");
        assert_eq!(&a[18..19], "-");
        assert_eq!(&a[23..24], "-");
        // 对齐 `UUID.randomUUID()`：版本位 4、变体位 10
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"), "{}", &a[19..20]);
    }
}
