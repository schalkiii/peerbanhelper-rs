//! 种子信息端点（对齐 `PBHTorrentController`）。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pbh_core::i18n::normalize_locale;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::AppState;

/// `GET /api/torrent/query?keyword&page&pageSize`：种子搜索列表。
pub async fn query(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let keyword = params.get("keyword").map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let page = params.get("page").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1).max(1);
    let size = params
        .get("pageSize")
        .or_else(|| params.get("size"))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(1, 500);
    match state.db.torrent_list(keyword.as_deref(), size, (page - 1) * size) {
        Ok((rows, total)) => {
            let results = rows
                .iter()
                .map(torrent_json)
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

fn torrent_json(t: &pbh_db::TorrentRow) -> Value {
    json!({
        "id": t.id,
        "infoHash": t.info_hash,
        "name": t.name,
        "size": t.size,
        "privateTorrent": false,
        "peerBanCount": t.peer_ban_count,
        "peerAccessCount": t.peer_access_count,
    })
}

/// `GET /api/torrent/{infoHash}`：单种子详情。
pub async fn details(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
) -> Response {
    match state.db.torrent_by_hash(&info_hash) {
        Ok(Some(t)) => {
            let data = json!({
                "found": true,
                "infoHash": t.info_hash,
                "name": t.name,
                "size": t.size,
                "privateTorrent": false,
                "peerBanCount": t.peer_ban_count,
                "peerAccessCount": t.peer_access_count,
            });
            (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
        }
        Ok(None) => (
            StatusCode::OK,
            crate::std_resp(false, Some("TORRENT_NOT_FOUND"), json!({ "found": false })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/torrent/{hash}/accessHistory`：访问该种子的 Peer 记录分页。
pub async fn access_history(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
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
    match state.db.query_access_history(None, Some(&info_hash), &order, size, (page - 1) * size) {
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
                        "firstTimeSeen": r.first_time_seen,
                        "lastTimeSeen": r.last_time_seen,
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

/// `GET /api/torrent/{hash}/banHistory`：该种子的封禁历史。
pub async fn ban_history(
    State(state): State<AppState>,
    Path(info_hash): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (page, size) = crate::api::pagination(&params);
    let locale = normalize_locale(params.get("locale").map(String::as_str).unwrap_or(&state.locale));
    match state.db.history_by_torrent(&info_hash, size, (page - 1) * size) {
        Ok((rows, total)) => {
            let results = rows
                .iter()
                .map(|h| crate::api::bans::ban_log_json(&state, &locale, h))
                .collect::<Vec<_>>();
            let data = json!({
                "page": page,
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