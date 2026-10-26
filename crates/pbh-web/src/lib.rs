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
    /// 累计「浪费的流量」：每个被封禁 peer 的 `uploaded`（对齐上游
    /// `InMemoryMetrics#recordPeerBan` 的 `wastedTraffic += peer.uploaded`）
    pub wasted_traffic: u64,
}

#[derive(Clone)]
pub struct AppState {
    pub db: Arc<Database>,
    pub token: Arc<Mutex<String>>,
    /// 静默登录令牌（GUI 专用，对齐上游 `SILENT_LOGIN_TOKEN_FOR_GUI`）：
    /// `?silentLogin=` 匹配即放行并种会话 cookie，GUI 内免输入 token
    pub silent_login: String,
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
    /// GeoIP 数据源（`GET /api/chart/geoIpInfo`；未安装 IP 库时为 `None`）。
    ///
    /// 与 ban wave 的 `history.peer_geoip` 落库共用同一份 provider（对齐上游
    /// `PBHChartController` 注入的 `IPDBManager`）：上游对每个去重 IP **实时查询**
    /// IPDB 再聚合，而非读取落库 JSON——落库 JSON 只保留了精简字段，不足以还原
    /// city/province/isp/region 四个维度。
    pub geo: Option<Arc<dyn pbh_core::geoip::GeoIpProvider>>,
    /// PTR 反查器（`/api/peer/{ip}` 始终尝试；peers 列表受 `lookup.dns-reverse-lookup` 控制）
    pub ptr: Arc<pbh_core::ptr::PtrLookup>,
    /// 本轮 wave 的实时 torrent/peer 快照（下载器详情端点数据源）
    pub live_snapshot: pbh_core::live::LiveSnapshot,
    /// 自定义脚本目录（`GET/PUT/DELETE /api/expression-engine/*`）。
    ///
    /// 与 `expression-engine` 模块加载脚本用的是同一路径（见
    /// `pbh_core::modules::expression_engine::resolve_scripts_dir`）。
    pub scripts_dir: Option<PathBuf>,
    /// `laboratory.yml` 路径（实验性功能 `/api/laboratory/*`；对齐上游
    /// `Laboratory` 的 `<config_dir>/laboratory.yml`）。
    pub lab_path: Option<PathBuf>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DownloaderStatus {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub online: bool,
    pub version: String,
    /// 本轮 wave 该下载器的活跃种子/peer 计数（对齐上游 `DownloaderStatusDTO`
    /// 的 `activeTorrents` / `activePeers` 数据源）
    #[serde(default)]
    pub torrents: u64,
    #[serde(default)]
    pub peers: u64,
}

/// 下载器元数据：编辑表单与状态页的数据源。
/// 对齐上游 `DownloaderWrapperDTO.endpoint` 与 `DownloaderStatusDTO.config`
/// （`saveDownloaderJson()`）——WebUI 前端编辑下载器时读取 `config` 填充表单，
/// 缺失会导致编辑弹窗全空；状态显示依赖 `lastStatus` 枚举。
#[derive(Clone, Debug, serde::Serialize)]
pub struct DownloaderMeta {
    pub endpoint: String,
    pub paused: bool,
    pub config: serde_json::Value,
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

/// `/api` 未命中端点的 404（JSON——不得落回 SPA 的 index.html）。
async fn api_not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        std_resp(false, Some("API_NOT_FOUND"), Value::Null),
    )
        .into_response()
}

pub fn build_router(state: AppState) -> Router {
    build_router_with_cors(state, false)
}

/// 与 [`build_router`] 相同，`allow_cors = true` 时追加任意来源的 CORS 层
/// （对齐上游 `server.allow-cors` 的 `anyHost` 规则）。
pub fn build_router_with_cors(state: AppState, allow_cors: bool) -> Router {
    // 需要 Token 鉴权的 API（对齐上游 Role.USER_READ / USER_WRITE 分组）
    let api_authed = api::api_routes().layer(middleware::from_fn_with_state(
        state.clone(),
        auth_middleware,
    ));
    // 无需鉴权的 API（Role.ANYONE：登录、manifest、初始化状态）
    let api_public = api::public_routes();
    // /api 未命中端点必须返回 404 JSON——此前未命中会冒泡到 SPA fallback
    // 返回 index.html（假 200），前端拿到 HTML 当 JSON 解析失败，还会掩盖
    // 「后端缺端点」的事实（2026-10-07 生产教训）
    let api = api_public.merge(api_authed).fallback(api_not_found);

    let router = Router::new()
        .route("/health", get(health))
        // 封禁列表端点：下载器（Transmission 等）匿名拉取，不走 Token 鉴权（对齐上游 Role.ANYONE）
        .route("/blocklist/p2p-plain-format", get(blocklist_p2p_plain))
        .route("/blocklist/ip", get(blocklist_ip))
        .route("/blocklist/dat-emule", get(blocklist_dat_emule))
        .nest("/api", api)
        .fallback(static_handler)
        .with_state(state);
    if allow_cors {
        router.layer(tower_http::cors::CorsLayer::very_permissive())
    } else {
        router
    }
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

/// 占位页响应：`no-store` 防止浏览器/WebView 缓存占位内容——否则前端部署后
/// 普通刷新仍显示旧占位页（2026-10-07 生产教训）。
fn placeholder_response() -> Response {
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Html(PLACEHOLDER),
    )
        .into_response()
}

async fn static_handler(State(state): State<AppState>, uri: Uri) -> Response {
    // 静默登录入口（GUI 免登闭环）：窗口加载 `/?silentLogin=<token>`，此处校验
    // secret 并下发会话 cookie 后 302 到无参首页——页面内后续 API 请求由浏览器
    // 自动携带 cookie 认证。此前仅靠 /api middleware 的静默豁免无法闭环：
    // 文档请求不经过 /api 中间件，cookie 永远不会种下，GUI 始终 401 骨架屏。
    if let Some(q) = uri.query() {
        if let Some(s) = q
            .split('&')
            .find(|p| p.starts_with("silentLogin="))
            .map(|p| p["silentLogin=".len()..].trim())
        {
            let expected = state.silent_login.clone();
            if !expected.is_empty() && s == expected {
                let token = state
                    .token
                    .lock()
                    .map(|t| t.clone())
                    .unwrap_or_default();
                let cookie = format!(
                    "PBH_SESSION={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800",
                    token
                );
                return Response::builder()
                    .status(StatusCode::FOUND)
                    .header(axum::http::header::LOCATION, "/")
                    .header(axum::http::header::SET_COOKIE, cookie)
                    .body(Body::empty())
                    .unwrap_or_else(|_| placeholder_response());
            }
        }
    }
    let dir = state.static_dir.lock().ok().and_then(|d| d.clone());
    let Some(dir) = dir else {
        return placeholder_response();
    };
    let rel = uri.path().trim_start_matches('/');
    let rel = if rel.is_empty() { "index.html" } else { rel };

    // 1. 优先按请求路径精确服务静态文件
    if let Some(rsp) = try_serve_static(&dir, rel).await {
        return rsp;
    }

    // 2. 前端 dist 使用相对路径 `./assets/...` 与 `./favicon.ico`。当用户从
    //    非根路由（如 /banlist）直接刷新时，浏览器会请求 `/banlist/assets/...`，
    //    精确路径找不到就会回退 index.html，导致 JS/CSS 被当成 HTML 解析，
    //    页面白屏并报「无法解析接口返回数据」。把最后一个 `/assets/` 之后的
    //    路径映射到 static 根目录的 `assets/`，`*/favicon.ico` 映射到根 favicon。
    if let Some(idx) = rel.rfind("/assets/") {
        let assets_rel = &rel[idx + 1..];
        if let Some(rsp) = try_serve_static(&dir, assets_rel).await {
            return rsp;
        }
    }
    if rel.ends_with("/favicon.ico") {
        if let Some(rsp) = try_serve_static(&dir, "favicon.ico").await {
            return rsp;
        }
    }

    // 3. 某些前端 API 调用（如 manifest）会拼上当前子路径前缀，即使已注入
    //    <base href="/"> 仍请求 `/banlist/api/metadata/manifest`。将其 308 到
    //    `/api/...`，避免被 SPA fallback 吞掉导致前端报「无法解析接口返回数据」。
    if let Some(idx) = rel.rfind("/api/") {
        let api_path = &rel[idx + 1..];
        return Response::builder()
            .status(StatusCode::PERMANENT_REDIRECT)
            .header(axum::http::header::LOCATION, format!("/{}", api_path))
            .body(Body::empty())
            .unwrap_or_else(|_| placeholder_response());
    }

    // 4. SPA history 路由回退：WebUI 的前端子路由在磁盘上并无同名文件，
    //    刷新时必须回退到 index.html 交给前端路由接管。
    //    dist 里 index.html 使用相对路径引用 assets/api/favicon，从非根路由
    //    刷新时浏览器会把这些相对路径加上当前路径前缀（如 /banlist/assets/...），
    //    导致资源与 API 请求全部错位。注入 <base href="/"> 让所有相对路径以
    //    根为基准，一次性修正。
    if let Some(rsp) = serve_index_html(&dir).await {
        return rsp;
    }
    placeholder_response()
}

/// 尝试从 static 目录服务单个文件。返回 None 当文件不存在或读取失败。
async fn try_serve_static(dir: &std::path::Path, rel: &str) -> Option<Response> {
    let path = safe_join(dir, rel)?;
    if !path.is_file() {
        return None;
    }
    let bytes = tokio::fs::read(&path).await.ok()?;
    let ct = mime_for(&path);
    // SPA 缓存策略：入口 html 每次回源（部署更新立即生效，规避启发式缓存）；
    // 其余静态资源（assets 带内容哈希）日级缓存
    let cache = if ct.starts_with("text/html") {
        "no-cache"
    } else {
        "public, max-age=86400"
    };
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", ct)
        .header("Cache-Control", cache)
        .body(Body::from(bytes))
        .ok()
}

/// 服务 SPA 入口 `index.html`，并动态注入 `<base href="/">`。
///
/// 前端 dist 原样复用上游产物，其资源/API 引用采用相对路径。当用户从
/// `/banlist`、`/settings` 等子路由直接刷新时，浏览器会把 `assets/...`、
/// `api/...`、`favicon.ico` 解析为 `/banlist/assets/...`、`/banlist/api/...` 等，
/// 导致静态资源错配、API 请求被 SPA fallback 吞掉（前端拿到 HTML 当 JSON 解析，
/// 报「无法解析接口返回数据」）。注入 base href 后所有相对路径以根为基准，
/// 一次性解决该问题，同时保留 dist 文件不做修改。
async fn serve_index_html(dir: &std::path::Path) -> Option<Response> {
    let path = safe_join(dir, "index.html")?;
    let mut html = tokio::fs::read_to_string(&path).await.ok()?;
    if !html.contains("<base href=") {
        html = html.replacen("<head>", "<head>\n  <base href=\"/\">", 1);
    }
    Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "text/html; charset=utf-8")
        .header("Cache-Control", "no-cache")
        .body(Body::from(html))
        .ok()
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
    // 会话 cookie：对齐上游 Javalin session（WebUI 前端登录后仅靠浏览器自动携带
    // 的 cookie 维持会话，无手动凭据存储）——`POST /api/auth/login` 下发
    // `PBH_SESSION=<token>`，此处验证之
    let cookie_ok = headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| {
            c.split(';')
                .map(str::trim)
                .find(|p| p.starts_with("PBH_SESSION="))
                .map(|p| p["PBH_SESSION=".len()..].trim() == token)
        })
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
    // 静默登录（对齐上游 JavalinWebContainer accessManager 对 `?silentLogin=` 的豁免）：
    // GUI 打开 WebUI 时 URL 携带 `?silentLogin=<token>`（进程启动期生成、经
    // data/silent_login_token 文件传给 GUI）。首次放行并下发会话 cookie，
    // 页面内后续 API 请求由浏览器自动携带 cookie，前端零感知。
    let silent_ok = !state.silent_login.is_empty()
        && req
            .uri()
            .query()
            .and_then(|q| q.split('&').find(|p| p.starts_with("silentLogin=")))
            .map(|p| p["silentLogin=".len()..].trim() == state.silent_login)
            .unwrap_or(false);
    if header_ok || query_ok || cookie_ok {
        next.run(req).await
    } else if silent_ok {
        let mut resp = next.run(req).await;
        let cookie = format!(
            "PBH_SESSION={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800",
            token
        );
        if let Ok(v) = axum::http::HeaderValue::from_str(&cookie) {
            resp.headers_mut()
                .append(axum::http::header::SET_COOKIE, v);
        }
        resp
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
    // 对齐上游 `DownloaderWrapperDTO(id, name, endpoint, type, paused)`：
    // WebUI 卡片的编辑入口需要 endpoint，状态显示需要 paused
    let items: Vec<Value> = list
        .iter()
        .map(|s| {
            let meta = state.backend.downloader_meta(&s.id);
            json!({
                "id": s.id,
                "name": s.name,
                "type": s.kind,
                "online": s.online,
                "version": s.version,
                "endpoint": meta.as_ref().map(|m| m.endpoint.clone()).unwrap_or_default(),
                "paused": meta.as_ref().map(|m| m.paused).unwrap_or(false),
            })
        })
        .collect();
    (StatusCode::OK, std_resp(true, None, json!(items))).into_response()
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
mod tests;
