//! Transmission RPC 响应 DTO（仅映射本阶段所需字段）。
//!
//! 字段名对齐 cordelia `rpc.types` 使用的 JSON 名；对同义拼写使用 `alias` 兼容
//! （`bytes_to_client` / `bytesToClient` 等）。

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// JSON-RPC 响应信封：`{"result": "success", "arguments": {...}}`
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TrResponse<T> {
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub arguments: Option<T>,
}

impl<T> TrResponse<T> {
    pub fn is_success(&self) -> bool {
        self.result == "success"
    }
}

/// 由 `tag` 关联的响应（cordelia 用 tag 匹配请求，这里只需顺序执行，保留解析）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TrTagged<T> {
    #[serde(default)]
    pub result: String,
    #[serde(default)]
    pub arguments: Option<T>,
}

/// `session-get` 参数（仅所需字段）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SessionGet {
    #[serde(default)]
    pub version: String,
    #[serde(rename = "blocklist-enabled", default)]
    pub blocklist_enabled: bool,
    #[serde(rename = "blocklist-url", default)]
    pub blocklist_url: String,
    #[serde(rename = "speed-limit-down", default)]
    pub speed_limit_down: i64,
    #[serde(rename = "speed-limit-down-enabled", default)]
    pub speed_limit_down_enabled: bool,
    #[serde(rename = "speed-limit-up", default)]
    pub speed_limit_up: i64,
    #[serde(rename = "speed-limit-up-enabled", default)]
    pub speed_limit_up_enabled: bool,
    #[serde(rename = "peer-port", default)]
    pub peer_port: i64,
}

/// `session-stats` 参数。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct SessionStats {
    #[serde(rename = "cumulative-stats", default)]
    pub cumulative_stats: CumulativeStats,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct CumulativeStats {
    #[serde(rename = "uploadedBytes", alias = "uploaded_bytes", default)]
    pub uploaded_bytes: i64,
    #[serde(rename = "downloadedBytes", alias = "downloaded_bytes", default)]
    pub downloaded_bytes: i64,
}

/// `blocklist-update` 参数。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct BlocklistUpdate {
    #[serde(rename = "blocklist-size", default)]
    pub blocklist_size: i64,
}

/// `torrent-get` 参数。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TorrentGet {
    #[serde(default)]
    pub torrents: Vec<TrTorrent>,
}

/// 单个 torrent（字段名与 `torrent-get` 的 `fields` 数组一致）。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TrTorrent {
    #[serde(default)]
    pub id: i64,
    #[serde(rename = "hashString", default)]
    pub hash_string: String,
    #[serde(default)]
    pub name: String,
    #[serde(rename = "peersConnected", default)]
    pub peers_connected: i64,
    #[serde(rename = "totalSize", default)]
    pub total_size: i64,
    #[serde(rename = "rateDownload", default)]
    pub rate_download: i64,
    #[serde(rename = "rateUpload", default)]
    pub rate_upload: i64,
    #[serde(rename = "percentDone", default)]
    pub percent_done: f64,
    #[serde(rename = "sizeWhenDone", default)]
    pub size_when_done: i64,
    #[serde(rename = "isPrivate", default)]
    pub is_private: bool,
    #[serde(default)]
    pub peers: Vec<TrPeer>,
}

/// Transmission 的 peer 条目。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct TrPeer {
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub port: u16,
    #[serde(rename = "clientName", alias = "client_name", default)]
    pub client_name: Option<String>,
    #[serde(rename = "flagStr", alias = "flag_str", default)]
    pub flag_str: Option<String>,
    /// base64 编码的 20 字节 peer id（Transmission 的 `peer_id` 字段）
    #[serde(rename = "peer_id", alias = "peerId", default)]
    pub peer_id: Option<String>,
    #[serde(default)]
    pub progress: f64,
    #[serde(rename = "rateToClient", alias = "rate_to_client", default)]
    pub rate_to_client: Option<i64>,
    #[serde(rename = "rateToPeer", alias = "rate_to_peer", default)]
    pub rate_to_peer: Option<i64>,
    #[serde(rename = "bytes_to_client", alias = "bytesToClient", default)]
    pub bytes_to_client: Option<i64>,
    #[serde(rename = "bytes_to_peer", alias = "bytesToPeer", default)]
    pub bytes_to_peer: Option<i64>,
}

/// Transmission 的 peer id 是 base64（20 字节），解码为 ISO-8859-1 文本
/// （对齐上游 `new String(Base64.getDecoder().decode(...), ISO_8859_1)`）。
pub fn decode_peer_id(raw: Option<&str>) -> String {
    use base64::Engine;
    let Some(raw) = raw else {
        return String::new();
    };
    match base64::engine::general_purpose::STANDARD.decode(raw) {
        Ok(bytes) => bytes.iter().map(|b| *b as char).collect(),
        Err(_) => String::new(),
    }
}

/// 供测试与状态展示：`session-set` 请求体构造。
#[derive(Debug, Clone, Serialize)]
pub struct SessionSetBody {
    pub method: String,
    pub arguments: HashMap<String, serde_json::Value>,
}
