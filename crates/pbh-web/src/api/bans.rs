//! 封禁列表 / 封禁历史 / 排行端点（对齐 `PBHBanController`）。
//!
//! 数据源（与上游一致）：
//! - `GET /api/bans`：内存封禁表 `BanList<IPAddress, BanMetadata>` → `BanDTO`
//!   （`address` + `banMetadata`（已按请求 locale 烘焙 `rule`/`description`）+ `ipGeoData`）；
//! - `GET /api/bans/logs`：`history` 表分页（`HistoryService.getBanLogs`）→ `BanLogDTO`；
//! - `GET /api/bans/ranks`：`history` 表按 IP 聚合计数（`HistoryService.getBannedIps`）；
//! - `PUT/DELETE /api/bans`：手动封禁 / 解封（下一轮 wave 应用）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use pbh_core::banlist::{BanMetadata, DownloaderBasicInfo};
use pbh_core::i18n::{normalize_locale, TranslationComponent, Translator};
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
    /// 是否过滤 `ban-for-disconnect` 条目（上游默认 `true`）
    #[serde(default, rename = "ignoreBanForDisconnect")]
    ignore_ban_for_disconnect: Option<bool>,
    #[serde(default)]
    locale: Option<String>,
}

/// 渲染 `TranslationComponent`（对齐 `TextManager.tl(locale, component)`）。
pub(crate) fn render_component(
    component: &TranslationComponent,
    translator: &Translator,
    locale: &str,
) -> String {
    translator.render(component, locale)
}

/// 下载器信息（对齐 `DownloaderBasicInfo`）：从运行期状态表查名称/类型。
pub(crate) fn downloader_info(state: &AppState, id: &str) -> DownloaderBasicInfo {
    let list = state.downloaders.lock().map(|d| d.clone()).unwrap_or_default();
    match list.iter().find(|d| d.id == id) {
        Some(status) => DownloaderBasicInfo {
            id: id.to_string(),
            name: status.name.clone(),
            kind: status.kind.clone(),
        },
        None => DownloaderBasicInfo { id: id.to_string(), name: id.to_string(), kind: String::new() },
    }
}

/// 把一条内存封禁记录组装为 `BanDTO`（对齐 `new BanDTO(address, new BakedBanMetadata(locale, meta), geoIp)`）。
pub(crate) fn ban_dto(
    state: &AppState,
    locale: &str,
    ip: &str,
    metadata: &BanMetadata,
) -> Value {
    json!({
        "address": ip,
        "banMetadata": {
            // `BakedBanMetadata` 字段顺序/取值对齐上游：downloader/torrent/peer 原样透传，
            // `rule` / `description` 按请求 locale 渲染为文本
            "downloader": metadata.downloader,
            "torrent": metadata.torrent,
            "peer": metadata.peer,
            "reverseLookup": metadata.reverse_lookup,
            "context": metadata.context,
            "banAt": metadata.ban_at_ms,
            "unbanAt": if metadata.unban_at_ms > 0 { json!(metadata.unban_at_ms) } else { Value::Null },
            "rule": render_component(&metadata.rule, &state.translator, locale),
            "description": render_component(&metadata.description, &state.translator, locale),
        },
        // 上游从 IP 库现查；本移植的历史记录已带 `peer_geoip`，列表视图不额外查询
        "ipGeoData": Value::Null,
    })
}

/// `GET /api/bans`：旧版 feed（`lastBanTime`+`limit`）与新分页（`page`+`pageSize`）。
pub async fn list(State(state): State<AppState>, Query(q): Query<BansQuery>) -> Response {
    let locale = normalize_locale(q.locale.as_deref().unwrap_or(&state.locale));
    let ignore_disconnect = q.ignore_ban_for_disconnect.unwrap_or(true);
    let keyword = q.search.as_deref().or(q.filter.as_deref()).unwrap_or("").trim().to_string();

    let records = state
        .ban_list
        .lock()
        .map(|list| list.records_sorted())
        .unwrap_or_default();
    let mut rows: Vec<Value> = Vec::new();
    for record in records {
        if ignore_disconnect && record.ban_for_disconnect {
            continue;
        }
        if !keyword.is_empty() && !record.ip.contains(&keyword) {
            continue;
        }
        if let Some(last_ban_time) = q.last_ban_time {
            if record.metadata.ban_at_ms <= last_ban_time {
                continue;
            }
        }
        rows.push(ban_dto(&state, &locale, &record.ip, &record.metadata));
    }
    // 按封禁时间倒序（对齐上游 `BanList` 迭代顺序）
    rows.sort_by(|a, b| {
        let ta = a["banMetadata"]["banAt"].as_i64().unwrap_or(0);
        let tb = b["banMetadata"]["banAt"].as_i64().unwrap_or(0);
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
    let (page, size) = crate::api::pagination(&params);
    let locale = normalize_locale(params.get("locale").map(String::as_str).unwrap_or(&state.locale));
    // 排序：DTO 字段名 → `history` 列名（对齐上游 `Orderable.addRemapping`）
    let order: Vec<(String, bool)> = params
        .iter()
        .filter(|(key, _)| key.as_str() == "orderBy" || key.as_str() == "sorter")
        .flat_map(|(_, value)| crate::parse_order_by(Some(value)))
        .filter_map(|(field, asc)| {
            crate::api::bans::order_column(&field).map(|column| (column, asc))
        })
        .collect();
    match state.db.page_history(&order, size, (page - 1) * size) {
        Ok((rows, total)) => {
            let results = rows
                .iter()
                .map(|h| ban_log_json(&state, &locale, h))
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

/// `Orderable` 白名单：DTO 字段名 → `history` 列名。
pub(crate) fn order_column(field: &str) -> Option<String> {
    pbh_db::Database::history_order_column(field)
}

/// 一条 `history` 行 → `BanLogDTO`（对齐上游 `PBHBanController.handleLogs` 的输出字段）。
pub(crate) fn ban_log_json(
    state: &AppState,
    locale: &str,
    row: &pbh_db::HistoryRow,
) -> Value {
    json!({
        "banAt": row.ban_at,
        "unbanAt": if row.unban_at > 0 { json!(row.unban_at) } else { Value::Null },
        "peerIp": row.ip,
        "peerPort": row.port,
        "peerId": row.peer_id,
        "peerClientName": row.peer_client_name,
        "peerUploaded": row.peer_uploaded.unwrap_or(0),
        "peerDownloaded": row.peer_downloaded.unwrap_or(0),
        "peerProgress": row.peer_progress,
        "torrentInfoHash": row.torrent_info_hash.clone().unwrap_or_default(),
        "torrentName": row.torrent_name.clone().unwrap_or_default(),
        "torrentSize": row.torrent_size,
        "module": row.module,
        // `rule_name` / `description` 存 `TranslationComponent` JSON（对齐上游 TypeHandler），
        // 这里按请求 locale 渲染；解析失败时退回原始字符串
        "rule": crate::render_keyed(&Some(row.rule.clone()), &row.rule, &state.translator, locale),
        "description": crate::render_keyed(
            &Some(row.description.clone()),
            &row.description,
            &state.translator,
            locale,
        ),
        "downloader": downloader_info(state, &row.downloader),
    })
}

/// `GET /api/bans/ranks?page&pageSize&filter`：IP 封禁次数排行（`HistoryService.getBannedIps`）。
pub async fn ranks(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (page, size) = crate::api::pagination(&params);
    let filter = params
        .get("filter")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match state.db.page_history_rank(filter.as_deref(), size, (page - 1) * size) {
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
