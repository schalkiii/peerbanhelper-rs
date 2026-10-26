//! BTN 协议 DTO、能力与配置响应（字段名/结构对齐上游 Gson 序列化）。

use super::*;

// ------------------------------------------------------------ 配置端点响应

/// 上游 `btn/BtnNetwork.PowCaptchaData`（Gson 按字段名序列化）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PowCaptchaData {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "challengeBase64", default)]
    pub challenge_base64: String,
    #[serde(rename = "difficultyBits", default)]
    pub difficulty_bits: i32,
    #[serde(default)]
    pub algorithm: String,
    #[serde(rename = "expireAt", default)]
    pub expire_at: i64,
}

/// 配置端点返回的 `proof_of_work_captcha` 子对象。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PowCaptchaEndpoint {
    #[serde(default)]
    pub endpoint: String,
}

/// 配置端点返回的单个 ability 定义（`BtnAbilityRules` / `BtnAbilityIP*List` 构造函数读取的键）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnAbilitySpec {
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub interval: Option<i64>,
    #[serde(default)]
    pub random_initial_delay: Option<i64>,
    #[serde(default)]
    pub pow_captcha: Option<bool>,
    /// `BtnAbilityHeartBeat.multiIf` 等扩展开关（多余字段一律 `false`）
    #[serde(default)]
    pub multi_if: Option<bool>,
    /// `BtnAbilityReconfigure` 内置「当前版本」（须匹配端点响应的 `version`，
    /// 上游 `ability.get("version").getAsString()` ⇒ 字符串版本号）
    #[serde(default)]
    pub version: Option<String>,
}

/// 配置端点响应（`BtnNetwork.configBtnNetwork` 解析的 `JsonObject`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnConfigResponse {
    #[serde(default)]
    pub min_protocol_version: Option<i32>,
    #[serde(default)]
    pub max_protocol_version: Option<i32>,
    #[serde(default)]
    pub proof_of_work_captcha: Option<PowCaptchaEndpoint>,
    #[serde(default)]
    pub ability: BTreeMap<String, BtnAbilitySpec>,
}

// -------------------------------------------------------------- 上报协议 DTO

// 下列结构与上游 `btn/ping/**` 的 @SerializedName **逐字一致**（Rust 字段名即 wire 名）。
// Gson 的 `JsonUtil.standard()` 开启 `serializeNulls`：`Option::None` 字段必须输出 JSON
// `null`，因此**不得**加 `skip_serializing_if`。

/// `BtnBanPing`：`{"bans":[...]}`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnBanPing {
    pub bans: Vec<BtnBan>,
}

/// `BtnBan`：`submit_bans` 载荷条目（`HistoryEntity` + `TorrentEntityDTO` 快照）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnBan {
    /// epoch 毫秒（上游 `Timestamp` 序列化为数字）
    pub ban_at: i64,
    pub peer_ip: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_progress: f64,
    pub peer_flag: Option<String>,
    /// `InfoHashUtil.getHashedIdentifier(infoHash)` 的哈希值
    pub torrent_identifier: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub from_peer_traffic: i64,
    pub to_peer_traffic: i64,
    pub downloader_progress: f64,
    pub module: String,
    pub rule: String,
    pub description: String,
    pub structured_data: Option<String>,
}

/// `BtnSwarmPeerPing`：`{"swarms":[...]}`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnSwarmPeerPing {
    pub swarms: Vec<BtnSwarm>,
}

/// `BtnSwarm`：`submit_swarm` 载荷条目（上游 `TrackedSwarmEntity` 快照）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BtnSwarm {
    pub torrent_identifier: String,
    /// 可空（上游 `Boolean` 包装类 ⇒ `serializeNulls` 可输出 `null`）
    pub torrent_is_private: Option<bool>,
    pub torrent_size: i64,
    pub downloader: String,
    pub downloader_progress: f64,
    pub peer_ip: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_progress: f64,
    pub to_peer_traffic: i64,
    pub to_peer_traffic_offset: i64,
    pub from_peer_traffic: i64,
    pub from_peer_traffic_offset: i64,
    /// epoch 毫秒（上游 `OffsetDateTime` 序列化为数字）
    pub first_time_seen: i64,
    /// epoch 毫秒
    pub last_time_seen: i64,
    pub peer_last_flags: Option<String>,
    pub upload_speed: i64,
    pub download_speed: i64,
    pub download_speed_max: i64,
    pub upload_speed_max: i64,
}

/// `LegacyBtnPeerPing`：`{"populate_time":..., "peers":[...]}`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnPeerPing {
    pub populate_time: i64,
    pub peers: Vec<LegacyBtnPeer>,
}

/// `LegacyBtnPeer`：遗留协议 live peer 快照（`submit_peers`）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnPeer {
    pub ip_address: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    pub torrent_identifier: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub downloaded: i64,
    pub rt_download_speed: i64,
    pub uploaded: i64,
    pub rt_upload_speed: i64,
    pub peer_progress: f64,
    pub downloader_progress: f64,
    pub peer_flag: Option<String>,
}

/// `LegacyBtnPeerHistoryPing`：`{"populate_time":..., "peers":[...]}`（`submit_histories`）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnPeerHistoryPing {
    pub populate_time: i64,
    pub peers: Vec<LegacyBtnPeerHistory>,
}

/// `LegacyBtnPeerHistory`：遗留协议 peer 历史记录
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnPeerHistory {
    pub ip_address: String,
    pub port: u16,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    pub torrent_identifier: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub downloaded: i64,
    pub downloaded_offset: i64,
    pub uploaded: i64,
    pub uploaded_offset: i64,
    /// epoch 毫秒（上游 `Timestamp` 序列化为数字）
    pub first_time_seen: i64,
    /// epoch 毫秒
    pub last_time_seen: i64,
    pub peer_flag: Option<String>,
}

/// `LegacyBtnBanPing`：`{"populate_time":..., "bans":[...]}`（遗留 `submit_bans`）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnBanPing {
    pub populate_time: i64,
    pub bans: Vec<LegacyBtnBan>,
}

/// `LegacyBtnBan`：遗留协议单条封禁上报
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LegacyBtnBan {
    pub btn_ban: bool,
    pub ban_unique_id: String,
    pub module: String,
    pub rule: String,
    pub peer: LegacyBtnPeer,
    pub structured_data: Option<serde_json::Value>,
}

// -------------------------------------------------------------- 上报数据行

// 数据行是「离线重组所需的最小信息」：trait 实现方（`pbh-db` / `pbh`）负责把
// `HistoryEntity` / `TrackedSwarmEntity` / `PeerRecordEntity` 与对应 torrent 快照
// JOIN 成这些行；本文件只负责按上游 `*.from(...)` 的顺序转换 + 哈希 infohash。

/// `BtnAbilitySubmitBans` 的一行（`HistoryEntity` + `TorrentEntityDTO`）
#[derive(Debug, Clone, Default)]
pub struct BtnBanHistoryRow {
    /// `HistoryEntity.id`：游标
    pub id: i64,
    pub ban_at_ms: i64,
    pub peer_ip: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_progress: f64,
    pub peer_flag: Option<String>,
    /// 原始 infohash（转换时调 [`get_hashed_identifier`]）
    pub torrent_hash: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub from_peer_traffic: i64,
    pub to_peer_traffic: i64,
    pub downloader_progress: f64,
    pub module: String,
    pub rule: String,
    pub description: String,
    pub structured_data: Option<String>,
}

impl BtnBanHistoryRow {
    pub(crate) fn into_ping(self) -> BtnBan {
        BtnBan {
            ban_at: self.ban_at_ms,
            peer_ip: self.peer_ip,
            peer_port: self.peer_port,
            peer_id: self.peer_id,
            peer_client_name: self.peer_client_name,
            peer_progress: self.peer_progress,
            peer_flag: self.peer_flag,
            torrent_identifier: get_hashed_identifier(&self.torrent_hash),
            torrent_is_private: self.torrent_is_private,
            torrent_size: self.torrent_size,
            from_peer_traffic: self.from_peer_traffic,
            to_peer_traffic: self.to_peer_traffic,
            downloader_progress: self.downloader_progress,
            module: self.module,
            rule: self.rule,
            description: self.description,
            structured_data: self.structured_data,
        }
    }
}

/// `BtnAbilitySubmitSwarm` 的一行（`TrackedSwarmEntity`）
#[derive(Debug, Clone, Default)]
pub struct BtnSwarmHistoryRow {
    /// `TrackedSwarmEntity.id`：二元游标的第二分量
    pub id: i64,
    /// `TrackedSwarmEntity.lastTimeSeen`（epoch 毫秒）：二元游标的第一分量
    pub last_time_seen_ms: i64,
    /// 原始 infohash
    pub torrent_hash: String,
    pub torrent_is_private: Option<bool>,
    pub torrent_size: i64,
    pub downloader: String,
    pub downloader_progress: f64,
    pub peer_ip: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_progress: f64,
    pub to_peer_traffic: i64,
    pub to_peer_traffic_offset: i64,
    pub from_peer_traffic: i64,
    pub from_peer_traffic_offset: i64,
    pub first_time_seen_ms: i64,
    pub peer_last_flags: Option<String>,
    pub upload_speed: i64,
    pub download_speed: i64,
    pub download_speed_max: i64,
    pub upload_speed_max: i64,
}

impl BtnSwarmHistoryRow {
    pub(crate) fn into_ping(self) -> BtnSwarm {
        BtnSwarm {
            torrent_identifier: get_hashed_identifier(&self.torrent_hash),
            torrent_is_private: self.torrent_is_private,
            torrent_size: self.torrent_size,
            downloader: self.downloader,
            downloader_progress: self.downloader_progress,
            peer_ip: self.peer_ip,
            peer_port: self.peer_port,
            peer_id: self.peer_id,
            peer_client_name: self.peer_client_name,
            peer_progress: self.peer_progress,
            to_peer_traffic: self.to_peer_traffic,
            to_peer_traffic_offset: self.to_peer_traffic_offset,
            from_peer_traffic: self.from_peer_traffic,
            from_peer_traffic_offset: self.from_peer_traffic_offset,
            first_time_seen: self.first_time_seen_ms,
            last_time_seen: self.last_time_seen_ms,
            peer_last_flags: self.peer_last_flags,
            upload_speed: self.upload_speed,
            download_speed: self.download_speed,
            download_speed_max: self.download_speed_max,
            upload_speed_max: self.upload_speed_max,
        }
    }
}

/// `BtnAbilitySubmitHistory` 的一行（`PeerRecordEntity` + `TorrentEntityDTO`）
#[derive(Debug, Clone, Default)]
pub struct BtnPeerHistoryRow {
    pub ip_address: String,
    pub port: u16,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    /// 原始 infohash
    pub torrent_hash: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub downloaded: i64,
    pub downloaded_offset: i64,
    pub uploaded: i64,
    pub uploaded_offset: i64,
    pub first_time_seen_ms: i64,
    pub last_time_seen_ms: i64,
    pub peer_flag: Option<String>,
}

impl BtnPeerHistoryRow {
    pub(crate) fn into_ping(self) -> LegacyBtnPeerHistory {
        LegacyBtnPeerHistory {
            ip_address: self.ip_address,
            port: self.port,
            peer_id: self.peer_id,
            client_name: self.client_name,
            torrent_identifier: get_hashed_identifier(&self.torrent_hash),
            torrent_is_private: self.torrent_is_private,
            torrent_size: self.torrent_size,
            downloaded: self.downloaded,
            downloaded_offset: self.downloaded_offset,
            uploaded: self.uploaded,
            uploaded_offset: self.uploaded_offset,
            first_time_seen: self.first_time_seen_ms,
            last_time_seen: self.last_time_seen_ms,
            peer_flag: self.peer_flag,
        }
    }
}

/// `LegacyBtnAbilitySubmitPeers` 的一行（live peer 快照）
#[derive(Debug, Clone, Default)]
pub struct BtnLegacyPeerRow {
    pub ip_address: String,
    pub peer_port: u16,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    /// 原始 infohash
    pub torrent_hash: String,
    pub torrent_is_private: bool,
    pub torrent_size: i64,
    pub downloaded: i64,
    pub rt_download_speed: i64,
    pub uploaded: i64,
    pub rt_upload_speed: i64,
    pub peer_progress: f64,
    pub downloader_progress: f64,
    pub peer_flag: Option<String>,
}

impl BtnLegacyPeerRow {
    pub(crate) fn into_peer(self) -> LegacyBtnPeer {
        LegacyBtnPeer {
            ip_address: self.ip_address,
            peer_port: self.peer_port,
            peer_id: self.peer_id,
            client_name: self.client_name,
            torrent_identifier: get_hashed_identifier(&self.torrent_hash),
            torrent_is_private: self.torrent_is_private,
            torrent_size: self.torrent_size,
            downloaded: self.downloaded,
            rt_download_speed: self.rt_download_speed,
            uploaded: self.uploaded,
            rt_upload_speed: self.rt_upload_speed,
            peer_progress: self.peer_progress,
            downloader_progress: self.downloader_progress,
            peer_flag: self.peer_flag,
        }
    }
}

/// `LegacyBtnAbilitySubmitBans.generateBans` 的一行（`BanMetadata` 快照，已含 peer 信息）
#[derive(Debug, Clone, Default)]
pub struct BtnLegacyBanRow {
    pub ban_at_ms: i64,
    pub ban_unique_id: String,
    pub module: String,
    pub rule: String,
    pub btn_ban: bool,
    pub peer: BtnLegacyPeerRow,
    pub structured_data: Option<serde_json::Value>,
}

impl BtnLegacyBanRow {
    pub(crate) fn into_ping(self) -> LegacyBtnBan {
        LegacyBtnBan {
            btn_ban: self.btn_ban,
            ban_unique_id: self.ban_unique_id,
            module: self.module,
            rule: self.rule,
            peer: self.peer.into_peer(),
            structured_data: self.structured_data,
        }
    }
}

/// 上报类能力的数据源抽象（对应上游 `HistoryService` + `TorrentService` +
/// `TrackedSwarmService` + `PeerRecordService` + `DownloaderServer` 的只读部分）。
///
/// 所有方法都有「空」默认实现：`BtnNetwork` 未注入数据源（或实现方返回空表）时，
/// 提交行为等价于上游「没有任何待提交数据」——**不发请求**、状态置真
/// （`setLastStatus(true, BTN_LAST_REPORT_EMPTY)`）。
///
/// 实现方约定：分页方法须按游标语义返回**有序**的行（上游依赖 stable ordering，
/// 否则游标推进会漏数据）。
pub trait BtnSubmitSource: Send + Sync + std::fmt::Debug {
    /// `historyDao.page(id > cursor_id)` 的封禁历史（`BtnAbilitySubmitBans`，每页 100）。
    fn batch_ban_history(&self, cursor_id: i64, limit: usize) -> Vec<BtnBanHistoryRow> {
        let _ = (cursor_id, limit);
        Vec::new()
    }

    /// `swarmDao.page(lastTimeSeen >= x, id > y, orderByAsc)` 的 swarm（每页 1000）。
    fn batch_swarm_history(
        &self,
        last_time_seen_ms: i64,
        id_after: i64,
        limit: usize,
    ) -> Vec<BtnSwarmHistoryRow> {
        let _ = (last_time_seen_ms, id_after, limit);
        Vec::new()
    }

    /// `peerRecordsDao.getPendingSubmitPeerRecords(since)` 的 peer 历史（每页 5000）。
    fn batch_peer_history(&self, last_time_seen_ms: i64, limit: usize) -> Vec<BtnPeerHistoryRow> {
        let _ = (last_time_seen_ms, limit);
        Vec::new()
    }

    /// `server.getLivePeersSnapshot()`：遗留协议 live peer 全量快照。
    fn legacy_peer_snapshot(&self) -> Vec<BtnLegacyPeerRow> {
        Vec::new()
    }

    /// `server.getBanList()`：遗留协议封禁全量快照（`ban_at_ms` 由调用方过滤）。
    fn legacy_ban_snapshot(&self) -> Vec<BtnLegacyBanRow> {
        Vec::new()
    }
}

// ---------------------------------------------------- 心跳 / IpQuery 响应 DTO

/// 心跳响应（上游 `ServerResponse`，本移植只用到 `external_ip`；Gson 忽略未命名字段，
/// serde 同样忽略未知键）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BtnHeartbeatResponse {
    #[serde(default)]
    pub external_ip: Option<String>,
}

/// `BtnAbilityIpQuery.query` 的响应（上游嵌套类 `IpQueryResult` 的逐字段移植）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryResult {
    /// 风险等级：red / orange / green / gray
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub bans: Option<IpQueryResultBans>,
    #[serde(default)]
    pub swarms: Option<IpQueryResultSwarms>,
    #[serde(default)]
    pub traffic: Option<IpQueryTraffic>,
    #[serde(default)]
    pub torrents: Option<IpQueryTorrents>,
}

/// `IpQueryResult.IpQueryResultBans`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryResultBans {
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub records: Vec<IpQueryBanRecord>,
}

/// `IpQueryResult.BanHistoryDto`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryBanRecord {
    #[serde(default)]
    pub populate_time: Option<i64>,
    #[serde(default)]
    pub torrent: Option<String>,
    #[serde(default)]
    pub peer_ip: Option<String>,
    #[serde(default)]
    pub peer_port: Option<u16>,
    #[serde(default)]
    pub peer_id: Option<String>,
    #[serde(default)]
    pub peer_client_name: Option<String>,
    #[serde(default)]
    pub peer_progress: Option<f64>,
    #[serde(default)]
    pub peer_flags: Option<String>,
    #[serde(default)]
    pub reporter_progress: Option<f64>,
    #[serde(default)]
    pub to_peer_traffic: Option<i64>,
    #[serde(default)]
    pub from_peer_traffic: Option<i64>,
    #[serde(default)]
    pub module_name: Option<String>,
    #[serde(default)]
    pub rule: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub structured_data: Option<serde_json::Value>,
}

/// `IpQueryResult.IpQueryResultSwarms`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryResultSwarms {
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub total: u64,
    #[serde(default)]
    pub records: Vec<IpQuerySwarmRecord>,
    #[serde(default)]
    pub concurrent_download_torrents_count: u64,
    #[serde(default)]
    pub concurrent_seeding_torrents_count: u64,
}

/// `IpQueryResult.SwarmTrackerDto`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQuerySwarmRecord {
    #[serde(default)]
    pub torrent: Option<String>,
    #[serde(default)]
    pub peer_ip: Option<String>,
    #[serde(default)]
    pub peer_port: Option<u16>,
    #[serde(default)]
    pub peer_id: Option<String>,
    #[serde(default)]
    pub peer_client_name: Option<String>,
    #[serde(default)]
    pub peer_progress: Option<f64>,
    #[serde(default)]
    pub from_peer_traffic: Option<i64>,
    #[serde(default)]
    pub to_peer_traffic: Option<i64>,
    #[serde(default)]
    pub from_peer_traffic_offset: Option<i64>,
    #[serde(default)]
    pub to_peer_traffic_offset: Option<i64>,
    #[serde(default)]
    pub flags: Option<String>,
    #[serde(default)]
    pub first_time_seen: Option<i64>,
    #[serde(default)]
    pub last_time_seen: Option<i64>,
    #[serde(default)]
    pub user_progress: f64,
}

/// `IpQueryResult.IpQueryTraffic`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryTraffic {
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub to_peer_traffic: i64,
    #[serde(default)]
    pub from_peer_traffic: i64,
    #[serde(default)]
    pub share_ratio: f64,
}

/// `IpQueryResult.IpQueryTorrents`
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpQueryTorrents {
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub count: u64,
}

// --------------------------------------------------------------- abilities

/// 上游 `btn/ability/**` 的能力种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtnAbilityKind {
    /// `BtnAbilityRules`：现代键 `rule_peer_identity`，遗留键 `rules`
    Rules,
    /// `BtnAbilityIPAllowList`：现代键 `ip_allowlist`
    IpAllowList,
    /// `BtnAbilityIPDenyList`：现代键 `ip_denylist`
    IpDenyList,
    /// `BtnAbilitySubmitBans`：现代键 `submit_bans`
    SubmitBans,
    /// `BtnAbilitySubmitSwarm`：现代键 `submit_swarm`
    SubmitSwarm,
    /// `BtnAbilitySubmitHistory`：键 `submit_histories`（两种协议分支共有）
    SubmitHistories,
    /// `BtnAbilityHeartBeat`：键 `heartbeat`
    Heartbeat,
    /// `BtnAbilityReconfigure`：键 `reconfigure`
    Reconfigure,
    /// `BtnAbilityIpQuery`：键 `ip_query`（只注册，不主动调度；由被查询方触发）
    IpQuery,
    /// `LegacyBtnAbilitySubmitPeers`：遗留键 `submit_peers`（min < 20 时）
    LegacySubmitPeers,
    /// `LegacyBtnAbilitySubmitBans`：遗留键 `submit_bans`（min < 20 时）
    LegacySubmitBans,
    /// 已在配置端点出现、但本移植未实现的能力（见模块文档）
    Other,
}

impl BtnAbilityKind {
    /// 上游 `BtnNetwork.configBtnNetwork` 里 `ability.has(<key>)` 的键 → 种类。
    ///
    /// 注意遗留协议的 `submit_bans` 对应 `LegacyBtnAbilitySubmitBans`，与
    /// 现代协议的 `BtnAbilitySubmitBans` 是**不同**实现；注册时由
    /// [`BtnNetwork::apply_config_response`] 按 `min_protocol_version` 分支决定，这里只做无分支映射。
    pub fn from_json_key(key: &str) -> Self {
        match key {
            "rules" | "rule_peer_identity" => Self::Rules,
            "ip_allowlist" => Self::IpAllowList,
            "ip_denylist" => Self::IpDenyList,
            "submit_bans" => Self::SubmitBans,
            "submit_swarm" => Self::SubmitSwarm,
            "submit_histories" => Self::SubmitHistories,
            "heartbeat" => Self::Heartbeat,
            "reconfigure" => Self::Reconfigure,
            "ip_query" => Self::IpQuery,
            "submit_peers" => Self::LegacySubmitPeers,
            _ => Self::Other,
        }
    }

    /// 现代协议下的 JSON 键
    pub fn json_key(self) -> &'static str {
        match self {
            Self::Rules => "rule_peer_identity",
            Self::IpAllowList => "ip_allowlist",
            Self::IpDenyList => "ip_denylist",
            Self::SubmitBans => "submit_bans",
            Self::SubmitSwarm => "submit_swarm",
            Self::SubmitHistories => "submit_histories",
            Self::Heartbeat => "heartbeat",
            Self::Reconfigure => "reconfigure",
            Self::IpQuery => "ip_query",
            Self::LegacySubmitPeers => "submit_peers",
            Self::LegacySubmitBans => "submit_bans",
            Self::Other => "unknown",
        }
    }

    /// 上游 `gatherAndSolveCaptchaBlocking(request, type)` 的 `type` 参数。
    ///
    /// 与 JSON 键的唯一差异：`submit_histories` 能力的 PoW type 是**单数**
    /// `submit_history`（源码实参逐字确认，不是 `json_key()` 的复数形式）。
    pub fn pow_type(self) -> &'static str {
        match self {
            Self::SubmitHistories => "submit_history",
            _ => self.json_key(),
        }
    }

    /// 本移植是否实现了该能力
    pub fn is_implemented(self) -> bool {
        !matches!(self, Self::Other)
    }

    /// 上游是否会给该能力 `scheduleWithFixedDelay(...)`（`IpQuery` 不注册调度器，
    /// 只由被查询方调用；其余能力全部周期执行）
    pub fn is_scheduled(self) -> bool {
        !matches!(self, Self::IpQuery | Self::Other)
    }

    /// 上游 `AbstractBtnAbility` 子类名（日志/状态展示用）
    pub fn upstream_name(self) -> &'static str {
        match self {
            Self::Rules => "BtnAbilityRules",
            Self::IpAllowList => "BtnAbilityIPAllowList",
            Self::IpDenyList => "BtnAbilityIPDenyList",
            Self::SubmitBans => "BtnAbilitySubmitBans",
            Self::SubmitSwarm => "BtnAbilitySubmitSwarm",
            Self::SubmitHistories => "BtnAbilitySubmitHistory",
            Self::Heartbeat => "BtnAbilityHeartBeat",
            Self::Reconfigure => "BtnAbilityReconfigure",
            Self::IpQuery => "BtnAbilityIpQuery",
            Self::LegacySubmitPeers => "LegacyBtnAbilitySubmitPeers",
            Self::LegacySubmitBans => "LegacyBtnAbilitySubmitBans",
            Self::Other => "BtnAbility(Unsupported)",
        }
    }
}

/// 上游 `AbstractBtnAbility` + 具体 ability 的运行期状态。
#[derive(Clone, Debug)]
pub struct BtnAbility {
    pub kind: BtnAbilityKind,
    /// 配置端点里该 ability 的 JSON 键（遗留协议可能是 `rules` / `submit_peers`）
    pub key: String,
    pub endpoint: String,
    /// `interval` 毫秒（`scheduleWithFixedDelay` 的 period）
    pub interval_ms: i64,
    /// `random_initial_delay` 毫秒（上游 `ThreadLocalRandom.nextLong(randomInitialDelay)`）
    pub random_initial_delay_ms: i64,
    pub pow_captcha: bool,
    /// `BtnAbilityHeartBeat.multiIf`：对每个本机网卡地址各发一次心跳
    pub multi_if: bool,
    /// `BtnAbilityReconfigure` 本地保存的 `version`（端点版本变化时触发重新握手）
    pub reconfigure_version: Option<String>,
    /// `AbstractBtnAbility.lastStatus`
    pub last_status: bool,
    /// `AbstractBtnAbility.lastStatusAt`
    pub last_status_at_ms: i64,
    /// 上游调度器的下一次触发时刻（本移植无后台线程，由调用方驱动 [`BtnNetwork::sync_due`]）
    pub next_due_ms: i64,
}

impl BtnAbility {
    pub(crate) fn from_spec(key: &str, spec: &BtnAbilitySpec) -> Self {
        Self {
            kind: BtnAbilityKind::from_json_key(key),
            key: key.to_string(),
            endpoint: spec.endpoint.clone().unwrap_or_default(),
            interval_ms: spec.interval.unwrap_or(0),
            random_initial_delay_ms: spec.random_initial_delay.unwrap_or(0),
            pow_captcha: spec.pow_captcha.unwrap_or(false),
            multi_if: spec.multi_if.unwrap_or(false),
            reconfigure_version: spec.version.clone(),
            // 上游构造函数 `set_status(true, LAST_STATUS_STANDBY)`
            last_status: true,
            last_status_at_ms: 0,
            next_due_ms: i64::MAX,
        }
    }
}
