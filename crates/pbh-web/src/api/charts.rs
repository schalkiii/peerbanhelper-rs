//! 图表端点（对齐 `PBHChartController`）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::AppState;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `GET /api/chart/geoIpInfo?startAt&endAt&bannedOnly&downloader`：IP 地理位置分布。
///
/// Rust 版未内置 IP 库（与上游默认 `ip.db` 一致需付费库），统一返回空结构；
/// 前端数据展示为空表而非报错。
pub async fn geo_ip(
    State(_): State<AppState>,
    Query(_): Query<HashMap<String, String>>,
) -> Response {
    let data = json!({
        "city": {},
        "isp": {},
        "province": {},
        "region": {},
    });
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `GET /api/chart/trend?startAt&endAt&downloader`：连接/封禁趋势（按天归组）。
pub async fn trend(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let start = params
        .get("startAt")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let end = params
        .get("endAt")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(now_ms());
    let downloader = params
        .get("downloader")
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    // 连接趋势：peer_records 的 first_time_seen 窗口内按天去重计数（近似 tracked swarm 统计）
    let connected = state
        .db
        .peer_first_seen_trend(start, end, downloader.as_deref())
        .unwrap_or_default();
    // 封禁趋势：ban_logs
    let banned = state
        .db
        .ban_trends(start, end, downloader.as_deref())
        .unwrap_or_default();
    let data = json!({
        "connectedPeersTrend": connected.iter().map(|(k, v)| json!({ "key": k, "value": v })).collect::<Vec<_>>(),
        "bannedPeersTrend": banned.iter().map(|(k, v)| json!({ "key": k, "value": v })).collect::<Vec<_>>(),
    });
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `GET /api/chart/traffic?startAt&endAt&downloader`：流量曲线（traffic_journal_v3）。
pub async fn traffic(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let start = params
        .get("startAt")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let end = params
        .get("endAt")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(now_ms());
    let downloader = params
        .get("downloader")
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let rows = state
        .db
        .traffic_trend(start, end, downloader.as_deref())
        .unwrap_or_default();
    // 对齐上游 `PBHChartController#handleTraffic`：返回数组，每项为
    // { timestamp, dataOverallDownloaded, dataOverallUploaded }（`traffic.vue` 按此读取）
    let data = rows
        .iter()
        .map(|(ts, (down, up))| {
            json!({
                "timestamp": ts,
                "dataOverallDownloaded": down,
                "dataOverallUploaded": up,
            })
        })
        .collect::<Vec<_>>();
    (StatusCode::OK, crate::std_resp(true, None, json!(data))).into_response()
}

/// `GET /api/chart/sessionAnalyse?startAt&endAt&downloader`：会话时段分析。
pub async fn session_analyse(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let start = params
        .get("startTime")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let end = params
        .get("endTime")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(now_ms());
    let downloader = params
        .get("downloader")
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let rows = state
        .db
        .connection_metrics_trend(start, end, downloader.as_deref())
        .unwrap_or_default();
    // 对齐上游 `PeerConnectionMetricsDTO`：字段名用驼峰且包含全部 18 个字段。
    // Rust 仅落库并聚合 `totalConnections`/`incomingConnections`，其余按 0 占位
    // （待 metrics 写入层补齐后可移除占位）。
    let data = rows
        .iter()
        .map(|(ts, row)| {
            json!({
                "key": ts,
                "totalConnections": row[0],
                "incomingConnections": row[1],
                "remoteRefuseTransferToClient": 0,
                "remoteAcceptTransferToClient": 0,
                "localRefuseTransferToPeer": 0,
                "localAcceptTransferToPeer": 0,
                "localNotInterested": 0,
                "questionStatus": 0,
                "optimisticUnchoke": 0,
                "fromDHT": 0,
                "fromPEX": 0,
                "fromLSD": 0,
                "fromTrackerOrOther": 0,
                "rc4Encrypted": 0,
                "plainTextEncrypted": 0,
                "utpSocket": 0,
                "tcpSocket": 0,
            })
        })
        .collect::<Vec<_>>();
    (StatusCode::OK, crate::std_resp(true, None, json!(data))).into_response()
}
