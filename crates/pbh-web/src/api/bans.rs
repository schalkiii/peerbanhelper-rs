//! 封禁列表 / 封禁历史 / 排行端点（对齐 `PBHBanController`）。
//!
//! 数据源约定（与上游一致）：
//! - `GET /api/bans`：运行时封禁表（`banned_ips`），每条附最近一次封禁的上下文
//!   （module / rule / torrent / downloader）；
//! - `GET /api/bans/ranks`：`ban_logs` 历史 `GROUP BY ip`（对齐 `HistoryMapper.getBannedIps`）；
//! - `GET /api/bans/logs`：`ban_logs` 历史分页（对齐 `HistoryMapper.getBanLogs`）；
//! - `PUT/DELETE /api/bans`：手动封禁 / 解封（下一轮 wave 应用）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::AppState;

#[derive(Deserialize, Default)]
pub struct BansQuery {
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    filter: Option<String>,
    /// 旧版 feed 模式：只返回比该时间新的封禁
    #[serde(default, rename = "lastBanTime")]
    last_ban_time: Option<i64>,
    /// 旧版 feed 模式的页大小
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    page: Option<i64>,
    #[serde(default, rename = "pageSize")]
    page_size: Option<i64>,
    #[serde(default)]
    size: Option<i64>,
}

/// 把 `banned_ips` 一条记录组装为 BanList JSON（对齐前端 `BanList` 模型字段）。
fn banlist_json(state: &AppState, ip: &str, hit_count: i64, first_banned_at: i64, last_banned_at: i64, ban_until: i64) -> Value {
    let locale = state.locale.clone();
    let last = state.db.last_ban_log_by_ip(ip).ok().flatten();
    let (module, rule, description, torrent_hash, torrent_name, downloader, peer_id, client_name) = match &last {
        Some(log) => (
            log.module.clone(),
            crate::render_keyed(&log.rule_key, &log.rule, &state.translator, &locale),
            crate::render_keyed(&log.reason_key, &log.reason, &state.translator, &locale),
            log.torrent_hash.clone(),
            log.torrent_name.clone(),
            log.downloader_id.clone(),
            log.peer_id.clone(),
            log.client_name.clone(),
        ),
        None => (String::new(), String::new(), String::new(), String::new(), String::new(), String::new(), String::new(), String::new()),
    };
    json!({
        "address": ip,
        "banMetadata": {
            "context": "manual",
            "downloader": { "id": downloader, "name": downloader, "type": "" },
            "randomId": "",
            "banAt": first_banned_at,
            "unbanAt": if ban_until > 0 { json!(ban_until) } else { Value::Null },
            "torrent": { "id": "", "size": 0, "name": torrent_name, "hash": torrent_hash },
            "peer": {
                "address": { "port": 0, "ip": ip },
                "id": peer_id,
                "clientName": client_name,
                "downloaded": 0,
                "uploaded": 0,
                "progress": 0.0,
            },
            "rule": rule,
            "description": description,
            "reverseLookup": "",
            "hitCount": hit_count,
            "module": module,
            "lastBanTime": last_banned_at,
        },
        "ipGeoData": Value::Null,
    })
}

/// `GET /api/bans`：旧版 feed（`lastBanTime`+`limit`）与新分页（`page`+`pageSize`）。
pub async fn list(State(state): State<AppState>, Query(q): Query<BansQuery>) -> Response {
    let banned = match state.db.list_banned_ips() {
        Ok(list) => list,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::std_resp(false, Some(&e.to_string()), Value::Null),
            )
                .into_response()
        }
    };
    let keyword = q.search.as_deref().or(q.filter.as_deref()).unwrap_or("").trim();
    let mut rows: Vec<Value> = Vec::new();
    for item in banned {
        if !keyword.is_empty() && !item.ip.contains(keyword) {
            continue;
        }
        if let Some(last_ban_time) = q.last_ban_time {
            if item.last_banned_at <= last_ban_time {
                continue;
            }
        }
        rows.push(banlist_json(&state, &item.ip, item.hit_count, item.first_banned_at, item.last_banned_at, item.ban_until));
    }
    // 按最近封禁时间倒序（对齐上游 BanList 迭代顺序）
    rows.sort_by(|a, b| {
        let ta = a["banMetadata"]["lastBanTime"].as_i64().unwrap_or(0);
        let tb = b["banMetadata"]["lastBanTime"].as_i64().unwrap_or(0);
        tb.cmp(&ta)
    });
    let total = rows.len() as i64;
    let (page, size) = match (q.limit, q.page) {
        (Some(limit), _) => {
            let size = limit.clamp(1, 500);
            rows.truncate(size as usize);
            (1, size)
        }
        (None, ..) => {
            let page = q.page.unwrap_or(1).max(1);
            let size = q.page_size.or(q.size).unwrap_or(10).clamp(1, 500);
            let start = ((page - 1) * size) as usize;
            rows = rows.into_iter().skip(start).take(size as usize).collect();
            (page, size)
        }
    };
    let data = json!({ "page": page, "size": size, "total": total, "results": rows });
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `PUT /api/bans`：手动封禁一批 IP（body: `["1.2.3.4"]`）。
pub async fn add(State(state): State<AppState>, Json(body): Json<Vec<String>>) -> Response {
    match state.backend.ban_peers(&body) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), json!({ "count": body.len() })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `DELETE /api/bans`：解封批量 IP（`["*"]` 清空全部）。返回解封数量。
pub async fn remove(State(state): State<AppState>, Json(body): Json<Vec<String>>) -> Response {
    match state.backend.unban_peers(&body) {
        Ok(count) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), json!({ "count": count })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/bans/logs?page&pageSize&orderBy`：封禁历史分页（对齐 `BanLogDTO` 字段）。
pub async fn logs(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let page = params.get("page").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1).max(1);
    let size = params
        .get("pageSize")
        .or_else(|| params.get("size"))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(10)
        .clamp(1, 500);
    let order: Vec<(String, bool)> = params
        .iter()
        .filter(|(key, _)| key.as_str() == "orderBy" || key.as_str() == "sorter")
        .flat_map(|(_, value)| crate::parse_order_by(Some(value)))
        .collect();
    match state
        .db
        .page_ban_history(None, None, &order, size, (page - 1) * size)
    {
        Ok((rows, total)) => {
            let results = rows
                .iter()
                .map(|h| {
                    json!({
                        "id": h.id,
                        "banAt": h.ban_at,
                        "unbanAt": if h.unban_at > 0 { json!(h.unban_at) } else { json!(null) },
                        "peerIp": h.ip,
                        "peerPort": h.port,
                        "peerId": h.peer_id,
                        "peerClientName": h.peer_client_name,
                        "peerUploaded": 0,
                        "peerDownloaded": 0,
                        "peerProgress": 0.0,
                        "torrentsInfoHash": h.torrent_info_hash,
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

/// `GET /api/bans/ranks?page&pageSize&filter`：IP 封禁次数排行。
pub async fn ranks(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let page = params.get("page").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1).max(1);
    let size = params
        .get("pageSize")
        .or_else(|| params.get("size"))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(10)
        .clamp(1, 500);
    let filter = params
        .get("filter")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match state.db.page_ban_rank(filter.as_deref(), size, (page - 1) * size) {
        Ok((rankings, total)) => {
            let results = rankings
                .iter()
                .map(|(addr, count)| json!({ "address": addr, "count": count }))
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