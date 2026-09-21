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
