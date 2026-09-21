//! qBittorrent Web API 响应 DTO（仅映射本阶段所需字段）。

use serde::Deserialize;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct QBittorrentPeer {
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default, rename = "peer_id_client")]
    pub peer_id_client: Option<String>,
    #[serde(default)]
    pub dl_speed: i64,
    #[serde(default)]
    pub downloaded: i64,
    #[serde(default)]
    pub up_speed: i64,
    #[serde(default)]
    pub uploaded: i64,
    #[serde(default)]
    pub progress: f64,
    #[serde(default)]
    pub flags: Option<String>,
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default)]
    pub port: Option<i64>,
    #[serde(default)]
    pub connection: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct QBittorrentTorrent {
    #[serde(default)]
    pub hash: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub progress: f64,
    #[serde(default, rename = "total_size")]
    pub total_size: i64,
    #[serde(default, rename = "piece_size")]
    pub piece_size: i64,
    #[serde(default, rename = "pieces_have")]
    pub pieces_have: i64,
    #[serde(default)]
    pub dlspeed: i64,
    #[serde(default)]
    pub upspeed: i64,
    #[serde(default, rename = "is_private")]
    pub is_private: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TorrentProperties {
    #[serde(default, rename = "is_private")]
    pub is_private: Option<bool>,
    #[serde(default, rename = "piece_size")]
    pub piece_size: i64,
    #[serde(default, rename = "pieces_have")]
    pub pieces_have: i64,
    #[serde(default, rename = "total_size")]
    pub total_size: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TorrentPeersResponse {
    #[serde(default)]
    pub peers: HashMap<String, QBittorrentPeer>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BuildInfo {
    #[serde(default)]
    pub libtorrent: Option<String>,
    #[serde(default)]
    pub qt: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerState {
    #[serde(default, rename = "alltime_ul")]
    pub alltime_ul: i64,
    #[serde(default, rename = "alltime_dl")]
    pub alltime_dl: i64,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MainData {
    #[serde(default, rename = "server_state")]
    pub server_state: Option<ServerState>,
}

/// `GET /api/v2/app/preferences` 的所需子集（对齐 `QBittorrentPreferences` 的限速字段）。
///
/// 上游字段是装箱 `Long`：qB 未返回该键时 Java 在 `getDlLimit()` 处 NPE
/// （被 `catch (Exception e) { throw new IllegalStateException(e); }` 包成异常）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct QBittorrentPreferences {
    /// 上传限速，单位 bytes/s，0 = 不限制
    #[serde(default, rename = "up_limit")]
    pub up_limit: Option<i64>,
    /// 下载限速，单位 bytes/s，0 = 不限制
    #[serde(default, rename = "dl_limit")]
    pub dl_limit: Option<i64>,
}
