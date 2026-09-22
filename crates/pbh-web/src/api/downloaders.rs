//! 下载器管理端点（对齐 `PBHDownloaderController`）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::AppState;

/// `PUT /api/downloaders`：新增下载器（body: `{id, config}`）。
pub async fn create(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let cfg = body.get("config").cloned().unwrap_or(body.clone());
    match state.backend.add_downloader(&cfg) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), json!({"success": true})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/downloaders/test`：测试配置连通性（body 为 `{config}`）。
pub async fn test(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let cfg = body.get("config").cloned().unwrap_or(body);
    match state.backend.test_downloader(&cfg) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), json!({"success": true})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `PATCH /api/downloaders/{id}`：更新下载器。
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let cfg = body.get("config").cloned().unwrap_or(body);
    match state.backend.update_downloader(&id, &cfg) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), json!({"success": true})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `DELETE /api/downloaders/{id}`：移除下载器。
pub async fn remove(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.backend.remove_downloader(&id) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), json!({"success": true})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/downloaders/{id}/status`：单下载器详细状态。
pub async fn status(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let statuses = state
        .downloaders
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    if let Some(status) = statuses.iter().find(|s| s.id == id) {
        (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), serde_json::to_value(status).unwrap_or(Value::Null)),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            crate::std_resp(false, Some("DOWNLOADER_NOT_FOUND"), Value::Null),
        )
            .into_response()
    }
}

/// `GET /api/downloaders/{id}/torrents`：实时种子列表。
pub async fn torrents(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(downloader) = state.backend.downloader(&id) else {
        return (
            StatusCode::NOT_FOUND,
            crate::std_resp(false, Some("DOWNLOADER_NOT_FOUND"), Value::Null),
        )
            .into_response();
    };
    match downloader.fetch_torrents().await {
        Ok(torrents) => {
            let results = torrents
                .iter()
                .map(|t| {
                    json!({
                        "id": t.hash,
                        "name": t.name,
                        "size": t.total_size,
                        "progress": t.progress,
                    })
                })
                .collect::<Vec<_>>();
            (
                StatusCode::OK,
                crate::std_resp(true, Some("OK"), json!(results)),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/downloaders/{id}/torrent/{torrent_id}/peers`：指定种子的实时 Peer 列表。
pub async fn peers(
    State(state): State<AppState>,
    Path((id, torrent_id)): Path<(String, String)>,
) -> Response {
    let Some(downloader) = state.backend.downloader(&id) else {
        return (
            StatusCode::NOT_FOUND,
            crate::std_resp(false, Some("DOWNLOADER_NOT_FOUND"), Value::Null),
        )
            .into_response();
    };
    let torrents = match downloader.fetch_torrents().await {
        Ok(torrents) => torrents,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                crate::std_resp(false, Some(&e.to_string()), Value::Null),
            )
                .into_response()
        }
    };
    let Some(torrent) = torrents.iter().find(|t| t.hash == torrent_id) else {
        return (
            StatusCode::NOT_FOUND,
            crate::std_resp(false, Some("TORRENT_NOT_FOUND"), Value::Null),
        )
            .into_response();
    };
    match downloader.fetch_peers(torrent).await {
        Ok(peers) => {
            let results = peers
                .iter()
                .map(|p| {
                    json!({
                        "address": p.ip,
                        "port": p.port,
                        "peerId": p.peer_id,
                        "clientName": p.client_name,
                        "uploadSpeed": p.up_speed,
                        "downloadSpeed": p.dl_speed,
                    })
                })
                .collect::<Vec<_>>();
            (
                StatusCode::OK,
                crate::std_resp(true, Some("OK"), json!(results)),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}