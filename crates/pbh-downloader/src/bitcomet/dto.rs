//! BitComet WebUI API 端点与响应 DTO。
//!
//! 端点常量与上游 `BCEndpoint` 枚举逐字对应（仅列出本阶段 Rust `Downloader` 接口用到的部分）；
//! 响应 DTO 对齐上游 `impl/bitcomet/resp/*`，字段名与其 `@SerializedName` 一致。

use serde::Deserialize;
use std::fmt;

/// `BCEndpoint.USER_LOGIN`
pub const USER_LOGIN: &str = "/api/webui/login";
/// `BCEndpoint.GET_DEVICE_TOKEN`
pub const GET_DEVICE_TOKEN: &str = "/api/device_token/get";
/// `BCEndpoint.GET_TASK_LIST`
pub const GET_TASK_LIST: &str = "/api_v2/task_list/get";
/// `BCEndpoint.GET_TASK_SUMMARY`
pub const GET_TASK_SUMMARY: &str = "/api/task/summary/get";
/// `BCEndpoint.GET_TASK_PEERS`
pub const GET_TASK_PEERS: &str = "/api/task/peers/get";
/// `BCEndpoint.GET_IP_FILTER_CONFIG`
pub const GET_IP_FILTER_CONFIG: &str = "/api/config/ipfilter/get";
/// `BCEndpoint.SET_IP_FILTER_CONFIG`
pub const SET_IP_FILTER_CONFIG: &str = "/api/config/ipfilter/set";
/// `BCEndpoint.IP_FILTER_UPLOAD`
pub const IP_FILTER_UPLOAD: &str = "/api/config/ipfilter/upload";
/// `BCEndpoint.TASK_UNBAN_PEERS`
pub const TASK_UNBAN_PEERS: &str = "/api/task/peers/unban_peers";
/// `BCEndpoint.GET_STATISTICS_LIST`
pub const GET_STATISTICS_LIST: &str = "/api/statistics_list/get";
/// `BCEndpoint.GET_CONNECTION_CONFIG`
pub const GET_CONNECTION_CONFIG: &str = "/api/config/connection_config/get";
/// `BCEndpoint.SET_CONNECTION_CONFIG`
pub const SET_CONNECTION_CONFIG: &str = "/api/config/connection_config/set";

/// 对齐 `BCLoginResponse`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCLoginResponse {
    #[serde(default, rename = "error_code")]
    pub error_code: Option<String>,
    #[serde(default, rename = "error_message")]
    pub error_message: Option<String>,
    #[serde(default, rename = "invite_token")]
    pub invite_token: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// 对齐 Lombok `@Data` 生成的 `toString()`（字段按声明顺序、`null` 原样输出）：
/// 上游 `DOWNLOADER_LOGIN_EXCEPTION` 的占位参数就是该对象的 `toString()`。
impl fmt::Display for BCLoginResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn opt(value: &Option<String>) -> &str {
            value.as_deref().unwrap_or("null")
        }
        write!(
            f,
            "BCLoginResponse(errorCode={}, errorMessage={}, inviteToken={}, version={})",
            opt(&self.error_code),
            opt(&self.error_message),
            opt(&self.invite_token),
            opt(&self.version)
        )
    }
}

/// 对齐 `BCDeviceTokenResult`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCDeviceTokenResult {
    #[serde(default, rename = "device_token")]
    pub device_token: Option<String>,
    #[serde(default, rename = "server_id")]
    pub server_id: Option<String>,
    #[serde(default, rename = "server_name")]
    pub server_name: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// 对齐 `BCIpFilterResponse`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCIpFilterResponse {
    #[serde(default, rename = "ip_filter_config")]
    pub ip_filter_config: Option<BCIpFilterConfig>,
    #[serde(default)]
    pub version: Option<String>,
}

/// 对齐 `BCIpFilterResponse.IpFilterConfigDTO`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCIpFilterConfig {
    #[serde(default, rename = "enable_ipfilter")]
    pub enable_ipfilter: Option<bool>,
    #[serde(default, rename = "ipfilter_mode")]
    pub ipfilter_mode: Option<String>,
    #[serde(default, rename = "loaded_record_count")]
    pub loaded_record_count: Option<i64>,
}

/// 对齐 `BCConfigSetResponse`（`set` 系列接口的通用返回）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCConfigSetResponse {
    #[serde(default, rename = "error_code")]
    pub error_code: Option<String>,
    #[serde(default, rename = "error_message")]
    pub error_message: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
}

/// 对齐 `BCStatisticsValueListResponse`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCStatisticsValueListResponse {
    #[serde(default, rename = "value_list")]
    pub value_list: Vec<BCStatisticsValue>,
}

/// 对齐 `BCStatisticsValueListResponse.ValueListDTO`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCStatisticsValue {
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

/// 对齐 `BCTaskListResponse`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCTaskListResponse {
    #[serde(default)]
    pub tasks: Vec<BCListTask>,
}

/// 对齐 `BCTaskListResponse.TasksDTO`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCListTask {
    #[serde(default, rename = "task_id")]
    pub task_id: i64,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

/// 对齐 `BCTaskTorrentResponse`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCTaskTorrentResponse {
    #[serde(default, rename = "error_code")]
    pub error_code: Option<String>,
    #[serde(default, rename = "task_detail")]
    pub task_detail: Option<BCTaskDetail>,
    #[serde(default, rename = "task_status")]
    pub task_status: Option<BCTaskStatus>,
    #[serde(default, rename = "task")]
    pub task: Option<BCTaskSummary>,
}

/// 对齐 `BCTaskTorrentResponse.TaskDetailDTO`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCTaskDetail {
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// 是否私有种子。上游直接拆箱为 `boolean`（null 会 NPE），本实现按 `false` 处理。
    #[serde(default, rename = "torrent_private")]
    pub torrent_private: Option<bool>,
    /// BTv1 infohash；为空时用 `infohash_v2`。
    #[serde(default)]
    pub infohash: Option<String>,
    #[serde(default, rename = "infohash_v2")]
    pub infohash_v2: Option<String>,
    #[serde(default, rename = "task_name")]
    pub task_name: Option<String>,
    #[serde(default, rename = "total_size")]
    pub total_size: i64,
}

/// 对齐 `BCTaskTorrentResponse.TaskStatusDTO`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCTaskStatus {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, rename = "dl_size")]
    pub dl_size: i64,
    #[serde(default, rename = "up_size")]
    pub up_size: i64,
    #[serde(default, rename = "total_size")]
    pub total_size: i64,
    /// 千分比进度（`/ 1000.0` 归一化）
    #[serde(default, rename = "download_permillage")]
    pub download_permillage: i16,
}

/// 对齐 `BCTaskTorrentResponse.TaskDTO`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCTaskSummary {
    #[serde(default, rename = "task_id")]
    pub task_id: i64,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default, rename = "task_name")]
    pub task_name: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, rename = "total_size")]
    pub total_size: i64,
    /// 已选择下载量（完成量口径）
    #[serde(default, rename = "selected_downloaded_size")]
    pub selected_downloaded_size: i64,
    #[serde(default, rename = "download_rate")]
    pub download_rate: i64,
    #[serde(default, rename = "upload_rate")]
    pub upload_rate: i64,
    #[serde(default)]
    pub permillage: i16,
}

/// 对齐 `BCTaskPeersResponse`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCTaskPeersResponse {
    #[serde(default, rename = "error_code")]
    pub error_code: Option<String>,
    #[serde(default, rename = "peer_count")]
    pub peer_count: Option<BCPeerCount>,
    /// `null` 表示该任务当前没有任何 peer（上游按空列表处理）
    #[serde(default)]
    pub peers: Option<Vec<BCPeer>>,
    #[serde(default, rename = "task")]
    pub task: Option<BCTaskSummary>,
}

/// 对齐 `BCTaskPeersResponse.PeerCountDTO`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCPeerCount {
    #[serde(default, rename = "peers_connected")]
    pub peers_connected: i64,
    #[serde(default, rename = "peers_connecting")]
    pub peers_connecting: i64,
}

/// 对齐 `BCTaskPeersResponse.PeersDTO`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCPeer {
    #[serde(default)]
    pub ip: Option<String>,
    #[serde(default, rename = "client_type")]
    pub client_type: Option<String>,
    #[serde(default)]
    pub flag: Option<String>,
    #[serde(default, rename = "remote_port")]
    pub remote_port: i32,
    #[serde(default, rename = "listen_port")]
    pub listen_port: i32,
    #[serde(default)]
    pub permillage: i16,
    #[serde(default, rename = "dl_rate")]
    pub dl_rate: i64,
    #[serde(default, rename = "up_rate")]
    pub up_rate: i64,
    /// 某些版本可能返回 `null`（上游注释：`may null in some version, we need check it`）
    #[serde(default, rename = "dl_size")]
    pub dl_size: Option<i64>,
    #[serde(default, rename = "up_size")]
    pub up_size: Option<i64>,
    #[serde(default, rename = "peer_id")]
    pub peer_id: Option<String>,
    #[serde(default)]
    pub group: Option<String>,
}

/// 对齐 `BCConnectionConfigResponse`（`GET_CONNECTION_CONFIG` 的返回）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCConnectionConfigResponse {
    #[serde(default, rename = "error_code")]
    pub error_code: Option<String>,
    #[serde(default, rename = "error_message")]
    pub error_message: Option<String>,
    #[serde(default, rename = "connection_config")]
    pub connection_config: Option<BCConnectionConfig>,
}

/// 对齐 `BCConnectionConfigResponse.ConnectionConfigDTO`。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BCConnectionConfig {
    #[serde(default, rename = "max_upload_speed")]
    pub max_upload_speed: Option<i64>,
    #[serde(default, rename = "max_download_speed")]
    pub max_download_speed: Option<i64>,
}
