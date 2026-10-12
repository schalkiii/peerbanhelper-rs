//! 图表端点（对齐 `PBHChartController`）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::AppState;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `counter` → `[{key, value}]`，按 **value 降序**（对齐上游 `.sorted(Integer.compare(o2.value(), o1.value()))`）。
fn sorted_kv(counter: BTreeMap<String, i64>) -> Vec<serde_json::Value> {
    let mut rows: Vec<(String, i64)> = counter.into_iter().collect();
    rows.sort_by_key(|(_, value)| std::cmp::Reverse(*value));
    rows.into_iter()
        .map(|(key, value)| json!({ "key": key, "value": value }))
        .collect()
}

/// 空 GeoIP 结构：四个维度都是 **数组**。
///
/// 前端 `ispPie.vue` 的 `onSuccess` 只判 `if (data.data)`，随后直接
/// `data.data[field].reduce(...)`——**四个维度必须是 `KV<string, number>[]`**；
/// 返回 `{}`（对象）会让 `.reduce is not a function` 直接崩掉整个统计页。
fn empty_geo() -> serde_json::Value {
    json!({
        "city": Vec::<serde_json::Value>::new(),
        "isp": Vec::<serde_json::Value>::new(),
        "province": Vec::<serde_json::Value>::new(),
        "region": Vec::<serde_json::Value>::new(),
    })
}

/// `GET /api/chart/geoIpInfo?startAt&endAt&bannedOnly&downloader`：IP 地理位置分布。
///
/// 对齐上游 `handleGeoIP`：对每个去重 IP **实时查询 IPDB** 再计数（落库的精简
/// `peer_geoip` JSON 不含 province/city 全名，无法还原四个维度）。维度取值：
/// - `isp`：`network.isp` 优先，其次 `as.organization`，缺省 `N/A`；
/// - `province`：`city.cn_province`；`city`：`cn_province + " " + cn_city`，无 `cn_city` 时取 `city.name`；
/// - `region`：`country.name`。
///
/// IP 库不可用时对齐上游 `IPDB_NEED_INIT` 语义返回失败，但 data 仍是四个空数组
/// （保证前端图表渲染为空而不是抛异常）。
pub async fn geo_ip(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(geo) = state.geo.as_ref() else {
        return (
            StatusCode::OK,
            crate::std_resp(false, Some("IPDB_NEED_INIT"), empty_geo()),
        )
            .into_response();
    };
    let start = params
        .get("startAt")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let end = params
        .get("endAt")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(now_ms());
    let banned_only = params
        .get("bannedOnly")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let downloader = params
        .get("downloader")
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty());
    let ips = state
        .db
        .distinct_ips(banned_only, start, end, downloader.as_deref())
        .unwrap_or_default();

    let mut isp_counter: BTreeMap<String, i64> = BTreeMap::new();
    let mut province_counter: BTreeMap<String, i64> = BTreeMap::new();
    let mut city_counter: BTreeMap<String, i64> = BTreeMap::new();
    let mut region_counter: BTreeMap<String, i64> = BTreeMap::new();
    for ip in ips {
        if ip.trim().is_empty() {
            continue;
        }
        let Some(addr) = pbh_core::iputil::parse_addr(&ip) else {
            continue;
        };
        // 上游 `IPDB#query` 返回的对象**字段可能为 null**（而非 null 对象），
        // 此时四个维度都记 `N/A` 并计数；这里用 default（全空字段）等价还原，
        // 不能在查询落空时直接跳过——否则会丢掉 `N/A` 这一档统计。
        let data = geo.query(addr).unwrap_or_default();
        // 上游先取 `as.organization`，再被 `network.isp` 覆盖（后者优先级更高）
        let isp = data
            .network
            .as_ref()
            .and_then(|n| n.isp.clone())
            .or_else(|| {
                data.as_data
                    .as_ref()
                    .and_then(|a| a.organization.clone())
            })
            .unwrap_or_else(|| "N/A".to_string());
        let region = data
            .country
            .as_ref()
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| "N/A".to_string());
        let (province, city) = match data.city.as_ref() {
            None => ("N/A".to_string(), "N/A".to_string()),
            Some(city_data) => {
                let province = city_data
                    .cn_province
                    .clone()
                    .unwrap_or_else(|| "N/A".to_string());
                let city = match city_data.cn_city.as_ref() {
                    // 上游：`city = cnProvince + " " + cnCity`
                    Some(cn_city) => format!(
                        "{} {cn_city}",
                        city_data.cn_province.as_deref().unwrap_or_default()
                    ),
                    None => city_data
                        .name
                        .clone()
                        .unwrap_or_else(|| "N/A".to_string()),
                };
                (province, city)
            }
        };
        *isp_counter.entry(isp).or_insert(0) += 1;
        *province_counter.entry(province).or_insert(0) += 1;
        *city_counter.entry(city).or_insert(0) += 1;
        *region_counter.entry(region).or_insert(0) += 1;
    }

    let data = json!({
        "city": sorted_kv(city_counter),
        "isp": sorted_kv(isp_counter),
        "province": sorted_kv(province_counter),
        "region": sorted_kv(region_counter),
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
