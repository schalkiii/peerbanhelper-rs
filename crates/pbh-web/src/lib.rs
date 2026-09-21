//! axum Web 服务：统一响应、Token 鉴权、封禁 API、概要统计、静态 WebUI 托管（SPEC 第 7 节）。

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
use pbh_db::{ms_to_rfc3339, Database};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// 运行期概要指标，由 ban wave 调度器更新。
#[derive(Default, Debug, Clone)]
pub struct Metrics {
    pub downloader_count: usize,
    pub torrent_count: usize,
    pub peer_count: usize,
    pub banned_total: usize,
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

fn std_resp(success: bool, message: Option<&str>, data: Value) -> Json<Value> {
    Json(json!({ "success": success, "message": message, "data": data }))
}

pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        .route("/ban/list", get(ban_list))
        .route("/ban/logs", get(ban_logs))
        .route("/metrics/general", get(general_metrics))
        .route("/downloaders", get(downloaders))
        .layer(middleware::from_fn_with_state(state.clone(), auth_middleware));

    Router::new()
        .route("/health", get(health))
        // 封禁列表端点：下载器（Transmission 等）匿名拉取，不走 Token 鉴权（对齐上游 Role.ANYONE）
        .route("/blocklist/p2p-plain-format", get(blocklist_p2p_plain))
        .route("/blocklist/ip", get(blocklist_ip))
        .route("/blocklist/dat-emule", get(blocklist_dat_emule))
        .nest("/api", api)
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
            (std::net::Ipv4Addr::from(base).to_string(), std::net::Ipv4Addr::from(broadcast).to_string())
        }
        ipnet::IpNet::V6(v6) => {
            let base = u128::from(v6.network());
            let host_mask = u128::MAX.checked_shr(v6.prefix_len() as u32).unwrap_or(0);
            let last = base | host_mask;
            (std::net::Ipv6Addr::from(base).to_string(), std::net::Ipv6Addr::from(last).to_string())
        }
    };
    Some((start, end))
}

/// 收集并重映射当前封禁列表（对齐上游 `banList.copyKeySet()` + `remapBanListAddress(ip)`，
/// 后者默认按「支持范围封禁」处理）。
fn remapped_bans(
    state: &AppState,
) -> Vec<String> {
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
pub fn build_ip_list(remapped: &[String]) -> String {
    let mut out = String::new();
    for ip in remapped {
        out.push_str(ip);
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
    ([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], body)
}

async fn blocklist_ip(State(state): State<AppState>) -> impl IntoResponse {
    let body = build_ip_list(&remapped_bans(&state));
    ([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], body)
}

async fn blocklist_dat_emule(State(state): State<AppState>) -> impl IntoResponse {
    let body = build_dat_emule(&remapped_bans(&state));
    ([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], body)
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
    if token.is_empty() || header_ok || query_ok {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, std_resp(false, Some("unauthorized"), Value::Null)).into_response()
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
    match state.db.list_ban_logs(size, offset) {
        Ok((logs, total)) => {
            let data = json!({
                "total": total,
                "page": q.page.max(1),
                "size": size,
                "results": logs.iter().map(|l| {
                    // 用落库的 `TranslationComponent` 按请求 locale 重新本地化；
                    // 缺 key 时回退到落库时已渲染的文案
                    let rule = render_keyed(&l.rule_key, &l.rule, &state.translator, &locale);
                    let reason = render_keyed(&l.reason_key, &l.reason, &state.translator, &locale);
                    json!({
                        "id": l.id,
                        "downloaderId": l.downloader_id,
                        "torrentHash": l.torrent_hash,
                        "torrentName": l.torrent_name,
                        "ip": l.ip,
                        "port": l.port,
                        "peerId": l.peer_id,
                        "clientName": l.client_name,
                        "module": l.module,
                        "rule": rule,
                        "reason": reason,
                        "ruleKey": l.rule_key,
                        "reasonKey": l.reason_key,
                        "banDuration": l.ban_duration,
                        "createdAt": ms_to_rfc3339(l.created_at),
                        "createdAtMs": l.created_at,
                    })
                }).collect::<Vec<_>>()
            });
            (StatusCode::OK, std_resp(true, None, data)).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, std_resp(false, Some(&e.to_string()), Value::Null))
                .into_response()
        }
    }
}

/// 用落库的 `TranslationComponent`（JSON）按 locale 重新渲染；解析失败或 key 缺失时回退到已渲染文案。
fn render_keyed(
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
    match state.db.list_banned_ips() {
        Ok(list) => {
            let data = json!(list
                .iter()
                .map(|b| json!({
                    "ip": b.ip,
                    "module": b.module,
                    "hitCount": b.hit_count,
                    "firstBannedAt": ms_to_rfc3339(b.first_banned_at),
                    "lastBannedAt": ms_to_rfc3339(b.last_banned_at),
                    "banUntil": if b.ban_until > 0 { ms_to_rfc3339(b.ban_until) } else { String::new() },
                }))
                .collect::<Vec<_>>());
            (StatusCode::OK, std_resp(true, None, data)).into_response()
        }
        Err(e) => {
            (StatusCode::INTERNAL_SERVER_ERROR, std_resp(false, Some(&e.to_string()), Value::Null))
                .into_response()
        }
    }
}

async fn general_metrics(State(state): State<AppState>) -> Response {
    let m = state.metrics.lock().map(|m| m.clone()).unwrap_or_default();
    let banned = state.db.list_banned_ips().map(|l| l.len()).unwrap_or(0);
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
    let list = state.downloaders.lock().map(|d| d.clone()).unwrap_or_default();
    (StatusCode::OK, std_resp(true, None, json!(list))).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(text, "TransmissionWorkaround:127.127.127.127-127.127.127.127");
        // 其它客户端返回空
        assert_eq!(build_p2p_plain(&[], Some("qBittorrent/5.0")), "");
        assert_eq!(build_p2p_plain(&[], None), "");
    }

    #[test]
    fn ip_and_dat_emule_formats() {
        let bans = vec!["1.2.3.4/32".to_string()];
        assert_eq!(build_ip_list(&bans), "1.2.3.4/32\n");
        assert_eq!(build_dat_emule(&bans), "1.2.3.4 - 1.2.3.4 , 000 , 1.2.3.4/32\n");
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
        assert_eq!(render_keyed(&Some("not-json".to_string()), "fallback", &t, "zh_cn"), "fallback");
        // 归一化：zh-CN → zh_cn
        let cn = render_keyed(&Some(serde_json::to_string(&TranslationComponent::new("Peer handshaking")).unwrap()), "x", &t, "zh-CN");
        assert_eq!(cn, "Peer handshaking");
    }
}
