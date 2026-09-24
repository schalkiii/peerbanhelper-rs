//! axum Web 服务：统一响应、Token 鉴权、封禁历史 API、概要统计、静态 WebUI 托管
//! （SPEC 第 7 节；WebAPI 与上游 `webapi/**Controller` 逐端点对齐，详见 `api` 模块）。

pub mod api;
pub mod backend;
pub mod tasks;
pub use backend::{BuildMeta, LogEntry, ModuleRecord, ReloadEntry, RingLog, SubModule, WebBackend};
pub use tasks::{
    BackgroundTask, BackgroundTaskBarType, BackgroundTaskRegistry, BackgroundTaskStatus,
    GeoIpTaskAdapter,
};

use axum::{
    body::Body,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, Uri},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use pbh_core::i18n::{normalize_locale, TranslationComponent, Translator};
use pbh_core::modules::TrackedSwarmRow;
use pbh_db::{ms_to_rfc3339, Database};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::warn;

/// 运行期统计（由 ban wave 调度器增量更新）。
#[derive(Default, Debug, Clone)]
pub struct Metrics {
    pub downloader_count: usize,
    pub torrent_count: usize,
    pub peer_count: usize,
    pub banned_total: usize,
    /// 累计检查次数（对每个 peer 跑一遍 pipeline 计一次）
    pub checks: u64,
    /// 累计封禁次数
    pub peer_bans: u64,
    /// 累计解封次数
    pub peer_unbans: u64,
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    pub token: Arc<Mutex<String>>,
    pub metrics: Arc<Mutex<Metrics>>,
    pub started: Arc<Instant>,
    pub downloaders: Arc<Mutex<Vec<DownloaderStatus>>>,
    pub static_dir: Arc<Mutex<Option<PathBuf>>>,
    /// 内存封禁表（`/blocklist/*` 端点用；与 wave 共享）
    pub ban_list: Arc<Mutex<pbh_core::banlist::BanList>>,
    /// `banlist-remapping` 配置（blocklist 端点默认按支持范围封禁处理）
    pub remap: Arc<pbh_core::remap::RemapConfig>,
    /// 内嵌/覆盖文案表（与 ban wave 共用，支持 `data/lang` 覆盖）
    pub translator: Arc<Translator>,
    /// 服务端默认 locale（请求未指定 `locale` 时使用）
    pub locale: String,
    /// 主进程能力（配置读写、下载器管理、推送、手动封禁调度等）
    pub backend: Arc<dyn WebBackend>,
    /// 日志环形缓冲（`/api/logs/history` 与 SSE `/api/logs/live`）
    pub log_ring: Arc<RingLog>,
    /// 全局暂停开关（`PATCH /api/general/global` 与 wave 引擎共享）
    pub global_pause: Arc<AtomicBool>,
    /// 规则订阅模块（未启用为 `None`，`/api/sub/*` 返回 404）
    pub sub_module: Option<Arc<dyn SubModule>>,
    /// BTN 传输层句柄（`GET /api/peer/{ip}/btnQuery`；未启用时为空）。
    ///
    /// `BtnNetwork` 由 BTN 工作线程构造，这里只持有跨线程句柄
    /// （对齐上游 `PBHPeerController` 注入的 `BtnNetwork` 单例）。
    pub btn_network: pbh_core::btn_transport::SharedBtnNetwork,
    /// 后台任务注册表（`GET /api/tasks/live`；对齐上游 `BackgroundTaskManager`）
    pub tasks: Arc<BackgroundTaskRegistry>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DownloaderStatus {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub online: bool,
    pub version: String,
}

const PLACEHOLDER: &str = "<!doctype html><html><head><meta charset='utf-8'><title>PeerBanHelper-RS</title></head>\
<body style='font-family:sans-serif;text-align:center;margin-top:4rem'>\
<h2>PeerBanHelper-RS 正在运行</h2>\
<p>WebUI 静态资源未安装：将上游 webui/dist 放入 data/static 目录。</p>\
<p>API：<code>/api/metrics/general</code> · <code>/api/ban/list</code> · <code>/api/ban/logs</code></p>\
</body></html>";

/// 统一响应包装（`{success, message, data}`；对齐上游 `StdResp` 的紧凑 JSON）。
pub(crate) fn std_resp(success: bool, message: Option<&str>, data: Value) -> Json<Value> {
    Json(json!({ "success": success, "message": message, "data": data }))
}

pub fn build_router(state: AppState) -> Router {
    // 需要 Token 鉴权的 API（对齐上游 Role.USER_READ / USER_WRITE 分组）
    let api_authed = api::api_routes().layer(middleware::from_fn_with_state(
        state.clone(),
        auth_middleware,
    ));
    // 无需鉴权的 API（Role.ANYONE：登录、manifest、初始化状态）
    let api_public = api::public_routes();

    Router::new()
        .route("/health", get(health))
        // 封禁列表端点：下载器（Transmission 等）匿名拉取，不走 Token 鉴权（对齐上游 Role.ANYONE）
        .route("/blocklist/p2p-plain-format", get(blocklist_p2p_plain))
        .route("/blocklist/ip", get(blocklist_ip))
        .route("/blocklist/dat-emule", get(blocklist_dat_emule))
        .nest("/api", api_public)
        .nest("/api", api_authed)
        .fallback(static_handler)
        .with_state(state)
}

/// 生成随机规则名（对齐上游 `UUID.randomUUID().toString().replace("-", "")`）。
fn random_rule_name() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| char::from_digit(rng.gen_range(0..16), 16).unwrap())
        .collect()
}

/// 把（已重映射的）地址换算为 `start-end` 区间文本。
///
/// 输入可能是 CIDR，也可能是单个主机地址（`remap_ban_list_address` 会原样保留主机地址，
/// 对齐上游 `toPrefixBlock()` 把主机地址视为 /32 或 /128）。
fn range_bounds(ip: &str) -> Option<(String, String)> {
    let net = match ip.parse::<ipnet::IpNet>() {
        Ok(net) => net,
        Err(_) => {
            let addr = ip.parse::<std::net::IpAddr>().ok()?;
            let prefix = if addr.is_ipv4() { 32 } else { 128 };
            ipnet::IpNet::new(addr, prefix).ok()?
        }
    };
    let (start, end): (String, String) = match net {
        ipnet::IpNet::V4(v4) => {
            let base = u32::from(v4.network());
            let host_mask = u32::MAX.checked_shr(v4.prefix_len() as u32).unwrap_or(0);
            let broadcast = base | host_mask;
            (
                std::net::Ipv4Addr::from(base).to_string(),
                std::net::Ipv4Addr::from(broadcast).to_string(),
            )
        }
        ipnet::IpNet::V6(v6) => {
            let base = u128::from(v6.network());
            let host_mask = u128::MAX.checked_shr(v6.prefix_len() as u32).unwrap_or(0);
            let last = base | host_mask;
            (
                std::net::Ipv6Addr::from(base).to_string(),
                std::net::Ipv6Addr::from(last).to_string(),
            )
        }
    };
    Some((start, end))
}

/// 收集并重映射当前封禁列表（对齐上游 `banList.copyKeySet()` + `remapBanListAddress(ip)`，
/// 后者默认按「支持范围封禁」处理）。
fn remapped_bans(state: &AppState) -> Vec<String> {
    let ips = state
        .ban_list
        .lock()
        .map(|list| list.keys_sorted())
        .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for ip in ips {
        for mapped in pbh_core::remap::remap_ban_list_address(&ip, true, &state.remap) {
            if !out.contains(&mapped) {
                out.push(mapped);
            }
        }
    }
    out
}

/// `/blocklist/p2p-plain-format`：`<random-name>:<start>-<end>` 每行一条。
///
/// 列表为空且 User-Agent 以 `Transmission` 开头时返回占位条目（上游 workaround，
/// 否则 Transmission 会拒绝空 blocklist）。
pub fn build_p2p_plain(remapped: &[String], user_agent: Option<&str>) -> String {
    let mut out = String::new();
    for ip in remapped {
        if let Some((start, end)) = range_bounds(ip) {
            out.push_str(&format!("{}:{}-{}\n", random_rule_name(), start, end));
        }
    }
    if out.is_empty() && user_agent.is_some_and(|ua| ua.starts_with("Transmission")) {
        return "TransmissionWorkaround:127.127.127.127-127.127.127.127".to_string();
    }
    out
}

/// `/blocklist/ip`：每行一个 CIDR。
///
/// 对齐上游 `BlockListController`：`ipAddress.toPrefixBlock().toCompressedString()`，
/// 因此裸主机地址要补 `/32` / `/128`（下游按 CIDR 解析时会丢弃不带前缀的行）。
pub fn build_ip_list(remapped: &[String]) -> String {
    let mut out = String::new();
    for ip in remapped {
        let line = match ip.parse::<ipnet::IpNet>() {
            Ok(net) => net.to_string(),
            Err(_) => match ip.parse::<std::net::IpAddr>() {
                Ok(addr) => match ipnet::IpNet::new(addr, if addr.is_ipv4() { 32 } else { 128 }) {
                    Ok(net) => net.to_string(),
                    Err(_) => ip.clone(),
                },
                Err(_) => ip.clone(),
            },
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// `/blocklist/dat-emule`：`start - end , 000 , <cidr>` 每行一条。
pub fn build_dat_emule(remapped: &[String]) -> String {
    let mut out = String::new();
    for ip in remapped {
        if let Some((start, end)) = range_bounds(ip) {
            out.push_str(&format!("{start} - {end} , 000 , {ip}\n"));
        }
    }
    out
}

async fn blocklist_p2p_plain(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let user_agent = headers
        .get("User-Agent")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let body = build_p2p_plain(&remapped_bans(&state), user_agent.as_deref());
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
    )
}

async fn blocklist_ip(State(state): State<AppState>) -> impl IntoResponse {
    let body = build_ip_list(&remapped_bans(&state));
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
    )
}

async fn blocklist_dat_emule(State(state): State<AppState>) -> impl IntoResponse {
    let body = build_dat_emule(&remapped_bans(&state));
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
    )
}

async fn static_handler(State(state): State<AppState>, uri: Uri) -> Response {
    let dir = state.static_dir.lock().ok().and_then(|d| d.clone());
    let Some(dir) = dir else {
        return Html(PLACEHOLDER).into_response();
    };
    let rel = uri.path().trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };
    let safe = safe_join(&dir, rel);
    if let Some(path) = safe {
        if path.is_file() {
            if let Ok(bytes) = tokio::fs::read(&path).await {
                let ct = mime_for(&path);
                return Response::builder()
                    .status(StatusCode::OK)
                    .header("Content-Type", ct)
                    .body(Body::from(bytes))
                    .unwrap_or_else(|_| Html(PLACEHOLDER).into_response());
            }
        }
    }
    Html(PLACEHOLDER).into_response()
}

/// 防目录穿越的路径拼接。
fn safe_join(base: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = base.to_path_buf();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "map" => "application/json; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn health() -> Json<Value> {
    std_resp(true, None, json!({ "status": "ok" }))
}

async fn auth_middleware(
    State(state): State<AppState>,
    headers: HeaderMap,
    req: axum::http::Request<Body>,
    next: Next,
) -> Response {
    let token = state.token.lock().map(|t| t.clone()).unwrap_or_default();
    let header_ok = headers
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim_start_matches("Bearer ").trim() == token)
        .unwrap_or(false);
    let query_ok = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find(|p| p.starts_with("token=")))
        .map(|p| p.trim_start_matches("token=") == token)
        .unwrap_or(false);
    if token.is_empty() {
        // 上游 `JavalinWebContainer`：token 为空表示尚未完成初始化向导，
        // 此时任何鉴权 API 都重定向到 `/init`（`WEBAPI_NEED_INIT`）。
        // 放行会让出厂默认配置下的所有写接口完全裸奔。
        return (
            StatusCode::SEE_OTHER,
            [(axum::http::header::LOCATION, "/init")],
            std_resp(
                false,
                Some("WEBAPI_NEED_INIT"),
                json!({ "location": "/init" }),
            ),
        )
            .into_response();
    }
    if header_ok || query_ok {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            std_resp(false, Some("unauthorized"), Value::Null),
        )
            .into_response()
    }
}

#[derive(Deserialize)]
struct LogsQuery {
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_size")]
    size: i64,
    /// 请求期望的 locale（对齐上游 WebUI 的 `Accept-Language` / 请求参数）；
    /// 缺省时使用服务端默认 locale。API 据此用落库的 `rule_key`/`reason_key` 重新本地化。
    #[serde(default)]
    locale: Option<String>,
}
fn default_page() -> i64 {
    1
}
fn default_size() -> i64 {
    50
}

async fn ban_logs(State(state): State<AppState>, Query(q): Query<LogsQuery>) -> Response {
    let size = q.size.clamp(1, 500);
    let offset = (q.page.max(1) - 1) * size;
    // 请求的 locale：显式 > 服务端默认；归一化以便查表（zh-CN → zh_cn）
    let locale = normalize_locale(q.locale.as_deref().unwrap_or(&state.locale));
    match state.db.page_history(&[], size, offset) {
        Ok((logs, total)) => {
            let data = json!({
                "total": total,
                "page": q.page.max(1),
                "size": size,
                "results": logs.iter().map(|l| {
                    // 用落库的 `TranslationComponent`（JSON）按请求 locale 重新本地化；
                    // 解析失败时回退到原始字符串
                    let rule = render_keyed(&Some(l.rule.clone()), &l.rule, &state.translator, &locale);
                    let reason =
                        render_keyed(&Some(l.description.clone()), &l.description, &state.translator, &locale);
                    json!({
                        "id": l.id,
                        "downloaderId": l.downloader,
                        "torrentHash": l.torrent_info_hash.clone().unwrap_or_default(),
                        "torrentName": l.torrent_name.clone().unwrap_or_default(),
                        "ip": l.ip,
                        "port": l.port,
                        "peerId": l.peer_id.clone().unwrap_or_default(),
                        "clientName": l.peer_client_name.clone().unwrap_or_default(),
                        "module": l.module,
                        "rule": rule,
                        "reason": reason,
                        "banDuration": if l.unban_at > l.ban_at { l.unban_at - l.ban_at } else { 0 },
                        "createdAt": ms_to_rfc3339(l.ban_at),
                        "createdAtMs": l.ban_at,
                    })
                }).collect::<Vec<_>>()
            });
            (StatusCode::OK, std_resp(true, None, data)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}

/// 用落库的 `TranslationComponent`（JSON）按 locale 重新渲染；解析失败或 key 缺失时回退到已渲染文案。
pub(crate) fn render_keyed(
    stored: &Option<String>,
    fallback: &str,
    translator: &Translator,
    locale: &str,
) -> String {
    if let Some(json_str) = stored {
        if let Ok(component) = serde_json::from_str::<TranslationComponent>(json_str) {
            if !component.key.is_empty() {
                return translator.render(&component, locale);
            }
        }
    }
    fallback.to_string()
}

async fn ban_list(State(state): State<AppState>) -> Response {
    // 旧版路径 `/api/ban/list`：数据源为内存封禁表（含完整 `BanMetadata`）
    let locale = state.locale.clone();
    let records = state
        .ban_list
        .lock()
        .map(|list| list.records_sorted())
        .unwrap_or_default();
    let data = json!(records
        .iter()
        .map(|record| crate::api::bans::ban_dto(&state, &locale, &record.ip, &record.metadata))
        .collect::<Vec<_>>());
    (StatusCode::OK, std_resp(true, None, data)).into_response()
}

async fn general_metrics(State(state): State<AppState>) -> Response {
    let m = state.metrics.lock().map(|m| m.clone()).unwrap_or_default();
    let banned = state.ban_list.lock().map(|l| l.len()).unwrap_or(0);
    let uptime = state.started.elapsed().as_secs();
    let data = json!({
        "downloaders": m.downloader_count,
        "torrents": m.torrent_count,
        "peers": m.peer_count,
        "banned": banned.max(m.banned_total),
        "uptimeSeconds": uptime,
    });
    (StatusCode::OK, std_resp(true, None, data)).into_response()
}

async fn downloaders(State(state): State<AppState>) -> Response {
    let list = state
        .downloaders
        .lock()
        .map(|d| d.clone())
        .unwrap_or_default();
    (StatusCode::OK, std_resp(true, None, json!(list))).into_response()
}

// ===========================================================================
// 监控视图（`module.peer-analyse-service.*` / `active-monitoring` 的数据读取侧）
// ===========================================================================

/// `/api/modules/swarm-tracking`：对齐 `SwarmTrackingModule.handleWebAPI`
/// —— `{"trackedSwarmSize": trackedSwarmDao.count()}`，**裸 JSON、无 `StdResp` 包装**
/// （上游此处直接 `context.json(response)`）。
async fn swarm_tracking(State(state): State<AppState>) -> Response {
    match state.db.tracked_swarm_count() {
        Ok(count) => (StatusCode::OK, Json(json!({ "trackedSwarmSize": count }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}

/// `/api/modules/swarm-tracking/details`：对齐 `SwarmTrackingModule.handleDetails`
/// —— `Pageable(page, pageSize)` + `Orderable` + `PBHPage{page, size, total, results}`。
///
/// 与上游的差异：`pageSize` 额外夹到 `1..=500`（上游不设上限），避免一次拉全表。
async fn swarm_tracking_details(
    State(state): State<AppState>,
    uri: Uri,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let page = params
        .get("page")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    // 上游 `Pageable` 读的是 `pageSize`；这里兼容常见的 `size` 写法
    let size = params
        .get("pageSize")
        .or_else(|| params.get("size"))
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(10)
        .clamp(1, 500);
    let order_by = parse_order_by(uri.query());
    let offset = (page - 1) * size;
    match state.db.page_tracked_swarm(&order_by, size, offset) {
        Ok((rows, total)) => {
            let data = json!({
                "page": page,
                "size": size,
                "total": total,
                "results": rows.iter().map(tracked_swarm_json).collect::<Vec<_>>(),
            });
            (StatusCode::OK, std_resp(true, None, data)).into_response()
        }
        Err(e) => {
            warn!("swarm-tracking 分页查询失败: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                std_resp(false, Some(&e.to_string()), Value::Null),
            )
                .into_response()
        }
    }
}

/// 解析 `orderBy` 查询参数：对齐上游 `Orderable` 的 `field|asc` / `field|desc`
/// （缺省方向为 ASC）；可重复出现，按出现顺序作为主次排序键。
pub(crate) fn parse_order_by(query: Option<&str>) -> Vec<(String, bool)> {
    query
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            if key != "orderBy" {
                return None;
            }
            // 上游读的是框架已解码的查询参数，这里补上 `%XX` 解码
            let value = percent_decode(value);
            let mut parts = value.split('|');
            let field = parts.next().unwrap_or_default().to_string();
            let asc = match parts.next() {
                None => true,
                Some(direction) => {
                    !(direction.eq_ignore_ascii_case("desc")
                        || direction.eq_ignore_ascii_case("descend"))
                }
            };
            Some((field, asc))
        })
        .collect()
}

/// 极简 `%XX` 百分号解码（用于 `orderBy` 的 `|` 分隔符）。
pub(crate) fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Some(byte) = std::str::from_utf8(&bytes[index + 1..index + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `TrackedSwarmEntity` 的 JSON：字段名对齐 Gson（`OffsetDateTimeTypeAdapter` 把时间戳写成
/// epoch 毫秒；`dirty` 是 `transient` 字段，不会输出）。
fn tracked_swarm_json(row: &TrackedSwarmRow) -> Value {
    json!({
        "id": row.id,
        "ip": row.ip,
        "port": row.port,
        "infoHash": row.info_hash,
        "torrentIsPrivate": row.torrent_is_private,
        "torrentSize": row.torrent_size,
        "downloader": row.downloader,
        "downloaderProgress": row.downloader_progress,
        "peerId": row.peer_id,
        "clientName": row.client_name,
        "peerProgress": row.peer_progress,
        "uploaded": row.uploaded,
        "uploadedOffset": row.uploaded_offset,
        "uploadSpeed": row.upload_speed,
        "downloaded": row.downloaded,
        "downloadedOffset": row.downloaded_offset,
        "downloadSpeed": row.download_speed,
        "lastFlags": row.last_flags,
        "firstTimeSeen": row.first_time_seen_ms,
        "lastTimeSeen": row.last_time_seen_ms,
        "downloadSpeedMax": row.download_speed_max,
        "uploadSpeedMax": row.upload_speed_max,
    })
}

#[derive(Deserialize)]
struct AlertsQuery {
    /// 请求期望的 locale（与 `/api/ban/logs` 同一约定）；缺省用服务端默认 locale。
    #[serde(default)]
    locale: Option<String>,
}

/// `/api/alerts`：对齐 `PBHAlertController.handleListing` —— 未读告警列表，
/// `title` / `content` 按请求 locale 渲染（上游 `tl(locale(ctx), ...)`）。
///
/// 本端点只返回**未读**告警；读状态的三个写端点见 `api::alerts`
/// （`PATCH /api/alert/{id}/dismiss`、`POST /api/alert/dismissAll`、`DELETE /api/alert/{id}`）。
async fn alerts(State(state): State<AppState>, Query(q): Query<AlertsQuery>) -> Response {
    let locale = normalize_locale(q.locale.as_deref().unwrap_or(&state.locale));
    match state.db.list_unread_alerts() {
        Ok(alerts) => {
            let data = json!(alerts
                .iter()
                .map(|alert| json!({
                    "id": alert.id,
                    "createAt": alert.create_at_ms,
                    "readAt": alert.read_at_ms,
                    "level": alert.level,
                    "identifier": alert.identifier,
                    "title": render_stored_component(&alert.title, &state.translator, &locale),
                    "content": render_stored_component(&alert.content, &state.translator, &locale),
                }))
                .collect::<Vec<_>>());
            (StatusCode::OK, std_resp(true, None, data)).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}

/// 用落库的 `TranslationComponent`（JSON）按 locale 渲染；解析失败时原样返回落库文本。
fn render_stored_component(stored: &str, translator: &Translator, locale: &str) -> String {
    match serde_json::from_str::<TranslationComponent>(stored) {
        Ok(component) => translator.render(&component, locale),
        Err(_) => stored.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use pbh_core::modules::{AlertLevel, MonitorSink, TrackedSwarmRow};
    use pbh_db::DbMonitorSink;
    use pbh_downloader::Downloader;
    use std::sync::atomic::AtomicBool;
    use tower::ServiceExt;

    /// 端点测试用的 AppState（内存库 + 固定 Token + 内嵌文案表）。
    fn test_state(db: Arc<Database>) -> AppState {
        AppState {
            db,
            token: Arc::new(Mutex::new("test-token".to_string())),
            metrics: Arc::new(Mutex::new(Metrics::default())),
            started: Arc::new(Instant::now()),
            downloaders: Arc::new(Mutex::new(Vec::new())),
            static_dir: Arc::new(Mutex::new(None)),
            ban_list: Arc::new(Mutex::new(pbh_core::banlist::BanList::default())),
            remap: Arc::new(pbh_core::remap::RemapConfig::default()),
            translator: Arc::new(Translator::embedded()),
            locale: "zh_cn".to_string(),
            backend: Arc::new(NoopBackend),
            log_ring: Arc::new(RingLog::new(16)),
            global_pause: Arc::new(AtomicBool::new(false)),
            sub_module: None,
            btn_network: Default::default(),
            tasks: Arc::new(BackgroundTaskRegistry::new()),
        }
    }

    /// 测试用空实现：所有能力返回默认/空值（端点路由不依赖具体后端）。
    struct NoopBackend;
    impl crate::backend::WebBackend for NoopBackend {
        fn installation_id(&self) -> String {
            "test-install".into()
        }
        fn analytics_enabled(&self) -> bool {
            true
        }
        fn set_analytics(&self, _enabled: bool) -> Result<(), String> {
            Ok(())
        }
        fn modules(&self) -> Vec<ModuleRecord> {
            vec![]
        }
        fn global_paused(&self) -> bool {
            false
        }
        fn set_global_paused(&self, _paused: bool) -> Result<(), String> {
            Ok(())
        }
        fn reload(&self) -> Vec<ReloadEntry> {
            vec![]
        }
        fn read_config(&self, _name: &str) -> Result<Value, String> {
            Err("CONFIG_NOT_FOUND: test".into())
        }
        fn write_config(&self, _name: &str, _data: &Value) -> Result<(), String> {
            Err("CONFIG_NOT_FOUND: test".into())
        }
        fn ban_peers(&self, _ips: &[String]) -> Result<(), String> {
            Ok(())
        }
        fn unban_peers(&self, _ips: &[String]) -> Result<usize, String> {
            Ok(0)
        }
        fn downloaders(&self) -> Vec<Value> {
            vec![]
        }
        fn downloader(&self, _id: &str) -> Option<Arc<dyn Downloader>> {
            None
        }
        fn add_downloader(&self, _config: &Value) -> Result<(), String> {
            Err("TEST_ONLY".into())
        }
        fn update_downloader(&self, _id: &str, _config: &Value) -> Result<(), String> {
            Err("TEST_ONLY".into())
        }
        fn remove_downloader(&self, _id: &str) -> Result<(), String> {
            Err("TEST_ONLY".into())
        }
        fn test_downloader(&self, _config: &Value) -> Result<(), String> {
            Err("TEST_ONLY".into())
        }
        fn push_channels(&self) -> Vec<Value> {
            vec![]
        }
        fn add_push_channel(&self, _channel: &Value) -> Result<(), String> {
            Err("TEST_ONLY".into())
        }
        fn update_push_channel(&self, _name: &str, _channel: &Value) -> Result<(), String> {
            Err("TEST_ONLY".into())
        }
        fn remove_push_channel(&self, _name: &str) -> Result<(), String> {
            Err("TEST_ONLY".into())
        }
        fn test_push_channel(&self, _channel: &Value) -> Result<(), String> {
            Err("TEST_ONLY".into())
        }
    }

    /// 发一个带 Token 的 GET 请求，返回 (状态码, JSON)。
    async fn get_json(state: &AppState, uri: &str) -> (StatusCode, Value) {
        let request = Request::builder()
            .uri(uri)
            .header("Authorization", "Bearer test-token")
            .body(Body::empty())
            .expect("请求构造");
        let response = build_router(state.clone())
            .oneshot(request)
            .await
            .expect("路由调用");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("响应体");
        let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }

    fn swarm_row(address: &str, last_time_seen_ms: i64) -> TrackedSwarmRow {
        TrackedSwarmRow {
            id: None,
            ip: address.to_string(),
            port: 6881,
            info_hash: "abcdef0123456789".to_string(),
            torrent_is_private: Some(false),
            torrent_size: 1_000_000_000,
            downloader: "qb".to_string(),
            downloader_progress: 0.25,
            peer_id: "-qB4500-aaaaaaaa".to_string(),
            client_name: "qBittorrent/4.5.0".to_string(),
            peer_progress: 0.5,
            uploaded: 100,
            uploaded_offset: 100,
            upload_speed: 10,
            downloaded: 200,
            downloaded_offset: 200,
            download_speed: 20,
            last_flags: "d u".to_string(),
            first_time_seen_ms: 1000,
            last_time_seen_ms,
            download_speed_max: 30,
            upload_speed_max: 40,
            dirty: false,
        }
    }

    #[tokio::test]
    async fn swarm_tracking_endpoint_returns_bare_tracked_swarm_size() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let state = test_state(db.clone());
        let sink = DbMonitorSink::new(db);
        for index in 0..3 {
            let mut row = swarm_row(&format!("10.0.0.{index}"), 1000 + index);
            row.port = 6881 + index as u16;
            sink.upsert_tracked_swarm(&row);
        }

        // 上游 `handleWebAPI` 直接返回裸 JSON（没有 success/message/data 包装）
        let (status, body) = get_json(&state, "/api/modules/swarm-tracking").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({ "trackedSwarmSize": 3 }));
        assert!(body.get("data").is_none());

        // 清空后计数回落
        sink.reset_tracked_swarm();
        let (_, body) = get_json(&state, "/api/modules/swarm-tracking").await;
        assert_eq!(body, json!({ "trackedSwarmSize": 0 }));
    }

    #[tokio::test]
    async fn swarm_tracking_details_pages_and_orders() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let state = test_state(db.clone());
        let sink = DbMonitorSink::new(db);
        for index in 0..5 {
            let mut row = swarm_row(&format!("10.0.0.{index}"), 1000 + index);
            row.port = 6881 + index as u16;
            sink.upsert_tracked_swarm(&row);
        }

        // 默认 page=1 / pageSize=10
        let (status, body) = get_json(&state, "/api/modules/swarm-tracking/details").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], json!(true));
        assert_eq!(body["message"], Value::Null);
        assert_eq!(body["data"]["page"], json!(1));
        assert_eq!(body["data"]["size"], json!(10));
        assert_eq!(body["data"]["total"], json!(5));
        assert_eq!(body["data"]["results"].as_array().unwrap().len(), 5);
        let first = &body["data"]["results"][0];
        assert_eq!(first["ip"], json!("10.0.0.0"));
        assert_eq!(first["infoHash"], json!("abcdef0123456789"));
        assert_eq!(first["torrentIsPrivate"], json!(false));
        assert_eq!(first["peerProgress"], json!(0.5));
        assert_eq!(first["firstTimeSeen"], json!(1000), "时间戳为 epoch 毫秒");
        assert!(
            first.get("dirty").is_none(),
            "dirty 是 transient 字段，不输出"
        );

        // 分页：page=2&pageSize=2
        let (_, body) = get_json(
            &state,
            "/api/modules/swarm-tracking/details?page=2&pageSize=2",
        )
        .await;
        assert_eq!(body["data"]["page"], json!(2));
        assert_eq!(body["data"]["size"], json!(2));
        assert_eq!(body["data"]["total"], json!(5));
        assert_eq!(body["data"]["results"].as_array().unwrap().len(), 2);
        assert_eq!(body["data"]["results"][0]["lastTimeSeen"], json!(1002));

        // orderBy=last_time_seen|desc（对齐 `Orderable` 的 `field|desc`）
        let (_, body) = get_json(
            &state,
            "/api/modules/swarm-tracking/details?orderBy=last_time_seen%7Cdesc&pageSize=3",
        )
        .await;
        let seen: Vec<i64> = body["data"]["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["lastTimeSeen"].as_i64().unwrap())
            .collect();
        assert_eq!(seen, vec![1004, 1003, 1002]);

        // 非法排序列（对齐 `SQLHelper.checkSafeFieldName`）-> 500
        let (status, body) = get_json(
            &state,
            "/api/modules/swarm-tracking/details?orderBy=id%3Bdrop",
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["success"], json!(false));
    }

    #[tokio::test]
    async fn alerts_endpoint_lists_unread_alerts_localized_per_request() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let state = test_state(db.clone());
        let sink = DbMonitorSink::new(db);
        sink.publish_alert(
            true,
            AlertLevel::Warn,
            "dataTrafficCapping-1",
            &TranslationComponent::with_params(
                "MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_TITLE",
                vec!["2026-09-20".into()],
            ),
            &TranslationComponent::new("MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_DESCRIPTION"),
        );

        let (status, body) = get_json(&state, "/api/alerts").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["success"], json!(true));
        let alert = &body["data"][0];
        assert_eq!(alert["identifier"], json!("dataTrafficCapping-1"));
        assert_eq!(alert["level"], json!("WARN"));
        assert_eq!(alert["readAt"], Value::Null, "未读告警");
        assert!(alert["createAt"].as_i64().unwrap() > 0);
        assert_eq!(
            alert["title"],
            json!("下载器上行流量超限告警 (2026-09-20)"),
            "默认 locale（服务端 zh_cn）"
        );

        // `?locale=en_us` 用同一份落库数据重新本地化
        let (_, body) = get_json(&state, "/api/alerts?locale=en_us").await;
        assert_eq!(
            body["data"][0]["title"],
            json!("Download upload traffic reached threshold (2026-09-20)")
        );

        // 无 Token -> 401
        let request = Request::builder()
            .uri("/api/alerts")
            .body(Body::empty())
            .expect("请求构造");
        let response = build_router(state.clone())
            .oneshot(request)
            .await
            .expect("路由调用");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn p2p_plain_format_uses_start_end_ranges() {
        // 主机地址（无前缀）按 /32 与 /128 处理
        let bans = vec![
            "1.2.3.0/24".to_string(),
            "2001:db8::/32".to_string(),
            "1.2.3.4".to_string(),
            "2001:db8:1:2:3:4:5:6".to_string(),
        ];
        let text = build_p2p_plain(&bans, None);
        let lines: Vec<&str> = text.trim().lines().collect();
        assert_eq!(lines.len(), 4, "主机地址不能被丢弃: {text}");
        let (name, range) = lines[0].split_once(':').unwrap();
        assert_eq!(name.len(), 32, "规则名是 32 位十六进制");
        assert!(name.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(range, "1.2.3.0-1.2.3.255");
        assert!(lines[1].ends_with(":2001:db8::-2001:db8:ffff:ffff:ffff:ffff:ffff:ffff"));
        assert!(lines[2].ends_with(":1.2.3.4-1.2.3.4"));
        assert!(lines[3].ends_with(":2001:db8:1:2:3:4:5:6-2001:db8:1:2:3:4:5:6"));
    }

    #[test]
    fn p2p_plain_format_returns_transmission_workaround_when_empty() {
        let text = build_p2p_plain(&[], Some("Transmission 4.1.0"));
        assert_eq!(
            text,
            "TransmissionWorkaround:127.127.127.127-127.127.127.127"
        );
        // 其它客户端返回空
        assert_eq!(build_p2p_plain(&[], Some("qBittorrent/5.0")), "");
        assert_eq!(build_p2p_plain(&[], None), "");
    }

    #[test]
    fn ip_and_dat_emule_formats() {
        let bans = vec!["1.2.3.4/32".to_string()];
        assert_eq!(build_ip_list(&bans), "1.2.3.4/32\n");
        assert_eq!(
            build_dat_emule(&bans),
            "1.2.3.4 - 1.2.3.4 , 000 , 1.2.3.4/32\n"
        );
    }

    #[test]
    fn render_keyed_localizes_from_stored_component() {
        let t = Translator::embedded();
        let stored = serde_json::to_string(&TranslationComponent::with_params(
            "MODULE_IBL_MATCH_IP",
            vec!["1.2.3.0/24".into()],
        ))
        .unwrap();
        // 按请求 locale 重新本地化（同一份落库数据，两种语言）
        assert_eq!(
            render_keyed(&Some(stored.clone()), "fallback", &t, "zh_cn"),
            "匹配 IP 规则: 1.2.3.0/24"
        );
        assert_eq!(
            render_keyed(&Some(stored), "fallback", &t, "en_us"),
            "Match IP rule: 1.2.3.0/24"
        );
        // 缺 key 或不合法 JSON → 回退到落库已渲染文案
        assert_eq!(render_keyed(&None, "fallback", &t, "zh_cn"), "fallback");
        assert_eq!(
            render_keyed(&Some("not-json".to_string()), "fallback", &t, "zh_cn"),
            "fallback"
        );
        // 归一化：zh-CN → zh_cn
        let cn = render_keyed(
            &Some(serde_json::to_string(&TranslationComponent::new("Peer handshaking")).unwrap()),
            "x",
            &t,
            "zh-CN",
        );
        assert_eq!(cn, "Peer handshaking");
    }
}
