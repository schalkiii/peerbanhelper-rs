//! BiglyBT 适配器插件 HTTP API 的 DTO。
//!
//! 字段名逐字对齐上游 wrapper（`DownloadRecord` / `TorrentRecord` / `DownloadStatsRecord` /
//! `PeerManagerRecord` / `PeerRecord` / `PeerStatsRecord` / `StatisticsRecord` /
//! `MetadataCallbackBean`）的 JSON 键名：上游用 `JsonUtil.getGson()`（默认 Gson，
//! 无字段命名策略）直接反序列化这些 record，因此 JSON 键名 == Java 字段名（camelCase）。
//!
//! 只映射本阶段实际读取的字段；上游 wrapper 里的其余字段（`categoryName` / `tags` /
//! `flags` / `pendingPeers` / `peerSupportedMessages` / `handshakeReservedBytes` 等）不被读取。
//!
//! 类型映射注意：上游 record 的 `int` / `long` / `boolean` 为原始类型（字段缺失即 0/false），
//! 引用类型（`Long` / `String`）可为 null 并在拆箱处 NPE。本实现按同样的中性值处理
//! （0 / false / 空串），使单个缺失字段不至于丢弃整份响应。

use serde::{Deserialize, Serialize};

/// `GET /metadata` 响应（上游 `MetadataCallbackBean`，仅需插件版本）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct MetadataCallbackBean {
    /// 插件版本号，登录时做 `>= 1.3.0` 校验
    #[serde(default, rename = "pluginVersion")]
    pub plugin_version: Option<String>,
}

/// `GET /downloads` 响应元素（上游 `DownloadRecord`）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DownloadRecord {
    /// `Download.getState()`（见 `BiglyBTDownloadStateConst`）
    #[serde(default)]
    pub state: i32,
    /// `Download.getTorrent()`（上游此处为 null 会 NPE）
    #[serde(default)]
    pub torrent: Option<TorrentRecord>,
    /// `Download.getName()`
    #[serde(default)]
    pub name: String,
    /// `Download.getStats()`（上游此处为 null 会 NPE）
    #[serde(default)]
    pub stats: Option<DownloadStatsRecord>,
}

/// `Download.getTorrent()`（上游 `TorrentRecord`）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TorrentRecord {
    /// 十六进制 info hash，同时用作 `BiglyBTTorrent.id`
    #[serde(default, rename = "infoHash")]
    pub info_hash: String,
    /// 种子总大小（含未选中文件）
    #[serde(default)]
    pub size: i64,
    /// 私有种子标记（`ignore-private` 依据）
    #[serde(default, rename = "privateTorrent")]
    pub private_torrent: bool,
}

/// `Download.getStats()`（上游 `DownloadStatsRecord`）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct DownloadStatsRecord {
    /// 完成度（千分比）
    #[serde(default, rename = "completedInThousandNotation")]
    pub completed_in_thousand_notation: i32,
    /// 尚未下载的字节数（含未选中文件）
    #[serde(default, rename = "remainingBytes")]
    pub remaining_bytes: i64,
    /// 实时下载速度
    #[serde(default, rename = "rtDownloadSpeed")]
    pub rt_download_speed: i64,
    /// 实时上传速度
    #[serde(default, rename = "rtUploadSpeed")]
    pub rt_upload_speed: i64,
}

/// `GET /download/{infoHash}/peers` 响应（上游 `PeerManagerRecord`）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PeerManagerRecord {
    /// 已连接 peer（上游此处为 null 会 NPE）
    #[serde(default)]
    pub peers: Option<Vec<PeerRecord>>,
}

/// 单个 peer（上游 `PeerRecord`）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PeerRecord {
    /// `Peer.getState()`（`BiglyBTDownloadManagerStateConst`）
    #[serde(default)]
    pub state: i32,
    /// 十六进制 peer id（`ByteUtil.hexToByteArray` 的输入）
    #[serde(default, rename = "peerId")]
    pub peer_id: Option<String>,
    /// peer 地址（可能带前导 `/`）
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub port: i32,
    /// 我们正在 choke 对方（`PeerFlag.remoteChoked`）
    #[serde(default)]
    pub choked: bool,
    /// 对方正在 choke 我们（`PeerFlag.choked`）
    #[serde(default)]
    pub choking: bool,
    /// 对方对我们感兴趣（`PeerFlag.remoteInterested`）
    #[serde(default)]
    pub interested: bool,
    /// 我们对对方感兴趣（`PeerFlag.interesting`）
    #[serde(default)]
    pub interesting: bool,
    #[serde(default)]
    pub seed: bool,
    #[serde(default)]
    pub snubbed: bool,
    #[serde(default, rename = "optimisticUnchoke")]
    pub optimistic_unchoke: bool,
    /// 入站连接（`PeerFlag.localConnection = !incoming`）
    #[serde(default)]
    pub incoming: bool,
    /// 对方进度（千分比）
    #[serde(default, rename = "percentDoneInThousandNotation")]
    pub percent_done_in_thousand_notation: i32,
    /// 客户端名称
    #[serde(default)]
    pub client: Option<String>,
    /// peer 来源：`Tracker` / `DHT` / `PeerExchange` / `HolePunch` / …
    #[serde(default, rename = "peerSource")]
    pub peer_source: Option<String>,
    /// 是否使用加密传输（`PeerFlag.rc4Encrypted`）
    #[serde(default, rename = "useCrypto")]
    pub use_crypto: bool,
    /// 传输协议（`uTP` 时为 `PeerFlag.utpSocket`）
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub stats: Option<PeerStatsRecord>,
}

/// `Peer.getStats()`（上游 `PeerStatsRecord`）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct PeerStatsRecord {
    #[serde(default, rename = "rtDownloadSpeed")]
    pub rt_download_speed: i64,
    #[serde(default, rename = "rtUploadSpeed")]
    pub rt_upload_speed: i64,
    #[serde(default, rename = "totalSent")]
    pub total_sent: i64,
    #[serde(default, rename = "totalReceived")]
    pub total_received: i64,
}

/// `GET /statistics` 响应（上游 `StatisticsRecord`，字段为 `Long` 包装类型）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct StatisticsRecord {
    #[serde(default, rename = "overallDataBytesReceived")]
    pub overall_data_bytes_received: Option<i64>,
    #[serde(default, rename = "overallDataBytesSent")]
    pub overall_data_bytes_sent: Option<i64>,
}

/// `POST /setconnector` 请求体（上游 `ConnectorData`，字段顺序即 Gson 输出顺序）。
#[derive(Debug, Clone, Serialize)]
pub struct ConnectorData {
    pub software: String,
    pub version: String,
    pub abbrev: String,
}

/// `POST /bans` 请求体（上游 `BanBean`）。
#[derive(Debug, Clone, Serialize)]
pub struct BanBean {
    pub ips: Vec<String>,
}

/// `PUT /bans` 请求体（上游 `BanListReplacementBean`）。
#[derive(Debug, Clone, Serialize)]
pub struct BanListReplacementBean {
    #[serde(rename = "replaceWith")]
    pub replace_with: Vec<String>,
    #[serde(rename = "includeNonPBHEntries")]
    pub include_non_pbh_entries: bool,
}

/// `GET /speedlimiter` 响应（上游 `CurrentSpeedLimiterBean`，字段为原始 `long`）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CurrentSpeedLimiterBean {
    #[serde(default)]
    pub upload: i64,
    #[serde(default)]
    pub download: i64,
}

/// `POST /speedlimiter` 请求体（上游 `SetSpeedLimiterBean`）。
#[derive(Debug, Clone, Serialize)]
pub struct SetSpeedLimiterBean {
    pub upload: i64,
    pub download: i64,
}
