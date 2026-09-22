//! 统计端点（对齐 `PBHMetricsController` 与规则统计服务）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::AppState;

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn week_start_ms() -> i64 {
    now_ms() - 7 * 24 * 3600 * 1000
}

/// `GET /api/statistic/counter`：总体计数器（对齐上游 `BasicMetrics` + `HistoryService`）。
pub async fn counter(State(state): State<AppState>) -> Response {
    let metrics = state.metrics.lock().unwrap_or_else(|e| e.into_inner());
    let banned_ips = state.db.list_banned_ips().map(|l| l.len() as u64).unwrap_or(0);
    let peers = metrics.peer_count as u64;
    let data = json!({
        "checkCounter": metrics.checks,
        "peerBanCounter": metrics.peer_bans,
        "peerUnbanCounter": metrics.peer_unbans,
        "banlistCounter": banned_ips,
        "bannedIpCounter": banned_ips,
        "wastedTraffic": 0,
        "trackedSwarmCount": state.db.tracked_swarm_size().unwrap_or(0) as u64,
        "peerBlockRate": if peers > 0 { metrics.peer_bans as f64 / peers as f64 } else { 0.0 },
        "weeklySessions": state.db.peer_session_count_since(week_start_ms()).unwrap_or(0),
    });
    drop(metrics);
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `GET /api/statistic/analysis/field?type=count|sum&field=...&filter=0.01&downloader=...`
pub async fn field(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mode = params.get("type").cloned().unwrap_or_else(|| "count".into());
    let field = params.get("field").cloned().unwrap_or_default();
    let filter = params.get("filter").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let downloader = params.get("downloader").map(|s| s.to_string()).filter(|s| !s.is_empty());
    match state.db.field_stats(&field, &mode, filter, downloader.as_deref()) {
        Ok(rows) => {
            let results: Vec<Value> = rows
                .iter()
                .map(|(key, count, percent)| json!({ "key": key, "value": count, "percent": percent }))
                .collect();
            (StatusCode::OK, crate::std_resp(true, None, json!(results))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/statistic/analysis/banTrends?startAt&endAt&downloader`：封禁时间线。
pub async fn ban_trends(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let start = params.get("startAt").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let end = params.get("endAt").and_then(|v| v.parse::<i64>().ok()).unwrap_or(now_ms());
    let downloader = params.get("downloader").map(|s| s.to_string()).filter(|s| !s.is_empty());
    match state.db.ban_trends(start, end, downloader.as_deref()) {
        Ok(rows) => {
            let results: Vec<Value> = rows
                .iter()
                .map(|(key, value)| json!({ "key": key, "value": value }))
                .collect();
            (StatusCode::OK, crate::std_resp(true, None, json!(results))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/statistic/analysis/date?startAt&endAt&downloader`：按天聚合分布。
pub async fn date(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let start = params.get("startAt").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let end = params.get("endAt").and_then(|v| v.parse::<i64>().ok()).unwrap_or(now_ms());
    let downloader = params.get("downloader").map(|s| s.to_string()).filter(|s| !s.is_empty());
    match state.db.ban_trends(start, end, downloader.as_deref()) {
        Ok(rows) => {
            let mut buckets: std::collections::BTreeMap<i64, i64> = std::collections::BTreeMap::new();
            for (day_start, count) in rows {
                *buckets.entry((day_start / 86400000) * 86400000).or_insert(0) += count;
            }
            let total: i64 = buckets.values().sum();
            let results: Vec<Value> = buckets
                .iter()
                .map(|(ts, count)| {
                    json!({
                        "timestamp": ts,
                        "count": count,
                        "percent": if total > 0 { *count as f64 / total as f64 } else { 0.0 },
                    })
                })
                .collect();
            (StatusCode::OK, crate::std_resp(true, None, json!(results))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/statistic/rules`：规则命中统计（按 module + rule 聚合）。
pub async fn rules(State(state): State<AppState>) -> Response {
    match state.db.rule_stats() {
        Ok(rows) => {
            let results = rows
                .iter()
                .map(|(module, rule, count)| json!({ "module": module, "rule": rule, "count": count }))
                .collect::<Vec<_>>();
            (StatusCode::OK, crate::std_resp(true, None, json!(results))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}