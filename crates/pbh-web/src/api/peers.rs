//! Peer 信息端点（对齐 `PBHPeerController`）。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use pbh_core::i18n::{normalize_locale, TranslationComponent};
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
    let ban_count: i64 = state.db.history_count_by_ip(&host).unwrap_or(0);
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
    let (page, size) = crate::api::pagination(&params);
    let locale = normalize_locale(params.get("locale").map(String::as_str).unwrap_or(&state.locale));
    match state.db.history_by_ip(&host, size, (page - 1) * size) {
        Ok((rows, total)) => {
            let results = rows
                .iter()
                .map(|h| crate::api::bans::ban_log_json(&state, &locale, h))
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

/// `GET /api/peer/{ip}/btnQuery`：BTN 信誉查询（对齐上游 `handleBtnQuery`）。
///
/// - BTN 未启用（`BtnNetwork == null`）⇒ `success=false` + `BTN_NETWORK_NOT_ENABLED`；
/// - 服务端未下发 `ip_query` 能力 ⇒ `success=false` + `BTN_ABILITY_IP_QUERY_NOT_PROVIDED`；
/// - 查询成功 ⇒ `success=true` + `IpQueryResult`（认证与 PoW 在 `query_ip` 内完成）；
/// - HTTP/认证失败 ⇒ 500（上游此时抛 `IOException`）。
pub async fn btn_query(
    State(state): State<AppState>,
    Path(ip): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let locale = normalize_locale(params.get("locale").map(String::as_str).unwrap_or(&state.locale));
    let host = parse_ip(&ip);
    let Some(network) = state.btn_network.get() else {
        let message = state
            .translator
            .render(&TranslationComponent::new("BTN_NETWORK_NOT_ENABLED"), &locale);
        return (StatusCode::OK, crate::std_resp(false, Some(&message), Value::Null)).into_response();
    };
    // `ip_query` 是阻塞 HTTP（含 PoW）：放到阻塞线程池，避免卡住 tokio worker
    let queried = tokio::task::spawn_blocking(move || network.query_ip(&host)).await;
    match queried {
        Ok(Ok(Some(result))) => (
            StatusCode::OK,
            crate::std_resp(true, None, serde_json::to_value(result).unwrap_or(Value::Null)),
        )
            .into_response(),
        Ok(Ok(None)) => {
            let message = state.translator.render(
                &TranslationComponent::new("BTN_ABILITY_IP_QUERY_NOT_PROVIDED"),
                &locale,
            );
            (StatusCode::OK, crate::std_resp(false, Some(&message), Value::Null)).into_response()
        }
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&format!("btn query task failed: {e}")), Value::Null),
        )
            .into_response(),
    }
}