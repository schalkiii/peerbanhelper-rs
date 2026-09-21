//! Deluge PBH-Adapter 插件的响应 DTO。
//!
//! 字段名逐字对齐插件返回的 snake_case 键（上游
//! `raccoonfink.deluge.responses.PBHActiveTorrentsResponse` / `PBHStatisticsResponse`），
//! 仅映射 `Deluge` / `DelugeTorrent` / `DelugePeer` 实际读取的字段。
//!
//! 注意：上游这些 DTO 字段都是包装类型（`Long` / `Double` / `Integer`），缺失时会在
//! 拆箱处 NPE（此异常不在 `catch (DelugeException)` 覆盖范围内）。本实现按字段类型的
//! 中性值（0 / 0.0 / false）处理，使单个缺字段不至于丢弃整份响应。

use serde::Deserialize;

/// `peerbanhelperadapter.get_active_torrents_info` 返回的单个 torrent。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ActiveTorrent {
    /// `DelugeTorrent.hash`（`info_hash`）
    #[serde(default, rename = "info_hash")]
    pub info_hash: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// 进度百分比（0-100），上游除以 100 后使用
    #[serde(default)]
    pub progress: Option<f64>,
    #[serde(default)]
    pub size: Option<i64>,
    /// `DelugeTorrent.completedSize`（已保存的数据量）
    #[serde(default, rename = "completed_size")]
    pub completed_size: Option<i64>,
    #[serde(default, rename = "upload_payload_rate")]
    pub upload_payload_rate: Option<i64>,
    #[serde(default, rename = "download_payload_rate")]
    pub download_payload_rate: Option<i64>,
    /// `priv`（JSON 键名保留，字段名避开 Rust 关键字）
    #[serde(default, rename = "priv")]
    pub is_private: Option<bool>,
    #[serde(default)]
    pub peers: Option<Vec<ActivePeer>>,
}

/// 单个 peer（`ActiveTorrentsResponseDTO.PeersDTO` 的所需子集）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ActivePeer {
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub port: Option<i64>,
    /// 十六进制 peer id（对齐 `StrUtil.toStringHex(peer.getPeerId())`）
    #[serde(default, rename = "peer_id")]
    pub peer_id: Option<String>,
    #[serde(default, rename = "client_name")]
    pub client_name: Option<String>,
    #[serde(default, rename = "payload_up_speed")]
    pub payload_up_speed: Option<i64>,
    #[serde(default, rename = "payload_down_speed")]
    pub payload_down_speed: Option<i64>,
    #[serde(default, rename = "total_upload")]
    pub total_upload: Option<i64>,
    #[serde(default, rename = "total_download")]
    pub total_download: Option<i64>,
    /// 进度百分比（0-100），上游除以 100 后使用
    #[serde(default)]
    pub progress: Option<f64>,
    /// libtorrent 连接位标志（`Deluge.parsePeerFlag` 的 `peerFlag`）
    #[serde(default)]
    pub flags: Option<i32>,
    /// peer 来源位标志（`Deluge.parsePeerFlag` 的 `sourceFlag`）
    #[serde(default)]
    pub source: Option<i32>,
}

/// `peerbanhelperadapter.get_session_totals` 返回的会话累计量。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SessionTotals {
    #[serde(default, rename = "total_payload_upload")]
    pub total_payload_upload: Option<i64>,
    #[serde(default, rename = "total_payload_download")]
    pub total_payload_download: Option<i64>,
}
