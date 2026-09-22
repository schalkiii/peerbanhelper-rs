//! Peer 信息端点（对齐 `PBHPeerController`）。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::AppState;

fn parse_ip(ip: &str) -> String {
    // 兼容 `ip:port` 形式的输入，取 host 部分
    ip.split(':').next().unwrap_or(ip).to_string()
}

/// `GET /api/peer/{ip}?pageSize`：peer 概要（封禁数 / 访问数 / 首次与最近活跃）。
pub async fn info(
    State(state): State<AppState>,
    Path(ip): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let host = parse_ip(&ip);
    let ban_count: i64 = state
        .db
        .page_ban_history(Some(&host), None, &[], 1, 0)
        .map(|(_, total)| total)
        .unwrap_or(0);
    let access = state.db.peer_access_summary(&host).unwrap_or((0, 0, 0, 0, 0, 0));
    let found = access.0 > 0;
    let data = json!({
        "found": found,
        "ip": host,
        "banCount": ban_count,
        "torrentAccessCount": access.1,
        "firstTimeSeen": access.2,
        "lastTimeSeen": access.3,
        "uploadedToPeer": access.4,
        "downloadedFromPeer": access.5,
        "peerId": null,
        "geoData": null,
        "wastedTraffic": 0,
    });
    let _ = params;
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `GET /api/peer/{ip}/accessHistory`：访问记录分页。
pub async fn access_history(
    State(state): State<AppState>,
    Path(ip): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let host = parse_ip(&ip);
    let page = params.get("page").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0).max(0);
    let size = params
        .get("pageSize")
        .or_else(|| params.get("size"))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(1, 500);
    let order: Vec<(String, bool)> = params
        .iter()
        .filter(|(key, _)| key.as_str() == "orderBy" || key.as_str() == "sorter")
        .flat_map(|(_, value)| crate::parse_order_by(Some(value)))
        .collect();
    match state.db.query_access_history(Some(&host), None, &order, size, page * size) {
        Ok((rows, total)) => {
            let results = rows
                .iter()
                .map(|r| {
                    json!({
                        "id": r.id,
                        "address": r.peer_ip,
                        "port": r.port,
                        "torrent": {
                            "id": r.torrent_id,
                            "size": r.torrent_size,
                            "name": r.torrent_name,
                            "hash": r.torrent_info_hash,
                        },
                        "downloader": r.downloader,
                        "peerId": r.peer_id,
                        "clientName": r.client_name,
                        "uploaded": r.uploaded,
                        "downloaded": r.downloaded,
                        "uploadSpeed": r.upload_speed,
                        "downloadSpeed": r.download_speed,
                        "lastFlags": r.last_flags,
                        "firstTimeSeen": r.first_time_seen,
                        "lastTimeSeen": r.last_time_seen,
                    })
                })
                .collect::<Vec<_>>();
            let data = json!({
                "page": page + 1,
                "size": size,
                "total": total,
                "results": results,
            });
            (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/peer/{ip}/banHistory`：该 IP 的封禁历史（复用 `/api/bans/logs` 逻辑）。
pub async fn ban_history(
    State(state): State<AppState>,
    Path(ip): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let host = parse_ip(&ip);
    let page = params.get("page").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1).max(1);
    let size = params
        .get("pageSize")
        .or_else(|| params.get("size"))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(1, 500);
    let order: Vec<(String, bool)> = params
        .iter()
        .filter(|(key, _)| key.as_str() == "orderBy" || key.as_str() == "sorter")
        .flat_map(|(_, value)| crate::parse_order_by(Some(value)))
        .collect();
    match state.db.page_ban_history(Some(&host), None, &order, size, (page - 1) * size) {
        Ok((rows, total)) => {
            let results = rows
                .iter()
                .map(|h| {
                    json!({
                        "banAt": h.ban_at,
                        "unbanAt": if h.unban_at > 0 { json!(h.unban_at) } else { json!(null) },
                        "peerIp": h.ip,
                        "peerPort": h.port,
                        "peerId": h.peer_id,
                        "peerClientName": h.peer_client_name,
                        "peerUploaded": 0,
                        "peerDownloaded": 0,
                        "peerProgress": 0.0,
                        "torrentInfoHash": h.torrent_info_hash,
                        "torrentName": h.torrent_name,
                        "torrentSize": h.torrent_size,
                        "downloader": h.downloader,
                        "module": h.module,
                        "rule": h.rule,
                        "description": h.description,
                    })
                })
                .collect::<Vec<_>>();
            let data = json!({ "page": page, "size": size, "total": total, "results": results });
            (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/peer/{ip}/btnQuery`：BTN 信誉查询（Rust 版未集成 BTN 库，恒为不可用）。
pub async fn btn_query(Path(ip): Path<String>) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        crate::std_resp(false, Some("BTN is not available in this build"), json!({
            "ip": ip,
        })),
    )
        .into_response()
}