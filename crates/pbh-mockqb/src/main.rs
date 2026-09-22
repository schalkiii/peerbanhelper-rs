//! Mock qBittorrent WebUI 服务（对跑/性能基准基座）。
//!
//! 从一份 fixture JSON 提供 qB v2 WebAPI 的最小可用子集，使 PeerBanHelper 的 Rust 版与
//! Java 版能连接**同一份确定性输入**（torrents / peers），从而：
//! - 对跑（L5）：对比两者产出的封禁决策集合与理由；
//! - 性能基准：测量各自启动耗时、常驻内存（RSS）与单轮 ban wave 耗时。
//!
//! fixture 字段对齐 `crates/pbh-downloader/src/qbittorrent/dto.rs` 的 DTO。

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

/// qB `/api/v2/torrents/info` 与 `/torrents/properties` 的 torrent 视图。
#[derive(Debug, Clone, Default, Deserialize)]
struct Torrent {
    #[serde(default)]
    hash: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    progress: f64,
    #[serde(default, rename = "total_size")]
    total_size: i64,
    #[serde(default, rename = "piece_size")]
    piece_size: i64,
    #[serde(default, rename = "pieces_have")]
    pieces_have: i64,
    #[serde(default)]
    dlspeed: i64,
    #[serde(default)]
    upspeed: i64,
    #[serde(default, rename = "is_private")]
    is_private: Option<bool>,
}

/// qB `/sync/torrentPeers` 的单个 peer（键为 `ip:port`，此结构为值）。
#[derive(Debug, Clone, Default, Deserialize)]
struct Peer {
    #[serde(default)]
    client: Option<String>,
    #[serde(default, rename = "peer_id_client")]
    peer_id_client: Option<String>,
    #[serde(default)]
    dl_speed: i64,
    #[serde(default)]
    downloaded: i64,
    #[serde(default)]
    up_speed: i64,
    #[serde(default)]
    uploaded: i64,
    #[serde(default)]
    progress: f64,
    #[serde(default)]
    flags: Option<String>,
    #[serde(default)]
    ip: Option<String>,
    #[serde(default)]
    port: Option<i64>,
    #[serde(default)]
    connection: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct BuildInfo {
    #[serde(default)]
    libtorrent: Option<String>,
    #[serde(default)]
    qt: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Preferences {
    #[serde(default)]
    enable_multi_connections_from_same_ip: bool,
    #[serde(default)]
    up_limit: Option<i64>,
    #[serde(default)]
    dl_limit: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Fixture {
    #[serde(default = "default_version")]
    version: String,
    #[serde(default)]
    buildinfo: BuildInfo,
    #[serde(default)]
    preferences: Preferences,
    #[serde(default)]
    torrents: Vec<Torrent>,
    /// hash -> 该 torrent 的 peers 列表
    #[serde(default)]
    peers: HashMap<String, Vec<Peer>>,
}

fn default_version() -> String {
    "5.0.0".to_string()
}

#[derive(Clone)]
struct AppState {
    fixture: Arc<Fixture>,
}

#[derive(Parser, Debug)]
#[command(name = "mockqb", about = "Mock qBittorrent WebUI for PBH dual-run")]
struct Args {
    /// 监听端口
    #[arg(long, default_value_t = 8080)]
    port: u16,
    /// fixture JSON 路径
    #[arg(long, default_value = "fixtures/sample.json")]
    fixture: String,
}

fn text(body: &'static str) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

fn text_owned(body: String) -> Response {
    (
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; charset=utf-8"),
        )],
        body,
    )
        .into_response()
}

fn json_resp(value: Value) -> Response {
    Json(value).into_response()
}

async fn login() -> Response {
    // 上游以 body == "Ok." 判定登录成功
    text("Ok.")
}

async fn build_info(State(state): State<AppState>) -> Response {
    json_resp(json!({
        "libtorrent": state.fixture.buildinfo.libtorrent.clone().unwrap_or_else(|| "1.2.19.0".into()),
        "qt": state.fixture.buildinfo.qt.clone().unwrap_or_else(|| "6.5.0".into()),
    }))
}

async fn version(State(state): State<AppState>) -> Response {
    text_owned(state.fixture.version.clone())
}

async fn preferences(State(state): State<AppState>) -> Response {
    json_resp(json!({
        "enable_multi_connections_from_same_ip":
            state.fixture.preferences.enable_multi_connections_from_same_ip,
        "up_limit": state.fixture.preferences.up_limit.unwrap_or(0),
        "dl_limit": state.fixture.preferences.dl_limit.unwrap_or(0),
    }))
}

async fn set_preferences() -> Response {
    (StatusCode::OK, text("Ok.")).into_response()
}

// 封禁/限速下发端点：dry-run 模式下 Rust 不会调用，但作为完整 qB v2 契约的留白存根，
// 避免将来非 dry-run 对跑时因缺路由而 500。
async fn ban_peers_stub() -> Response {
    text("Ok.")
}

async fn torrents_info(State(state): State<AppState>) -> Response {
    let mut list = Vec::new();
    for t in &state.fixture.torrents {
        list.push(json!({
            "hash": t.hash,
            "name": t.name,
            "progress": t.progress,
            "total_size": t.total_size,
            "piece_size": t.piece_size,
            "pieces_have": t.pieces_have,
            "dlspeed": t.dlspeed,
            "upspeed": t.upspeed,
            "is_private": t.is_private.unwrap_or(false),
        }));
    }
    Json(list).into_response()
}

async fn torrent_properties(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let hash = params.get("hash").cloned().unwrap_or_default();
    let t = state.fixture.torrents.iter().find(|t| t.hash == hash);
    match t {
        Some(t) => json_resp(json!({
            "total_size": t.total_size,
            "piece_size": t.piece_size,
            "pieces_have": t.pieces_have,
            "is_private": t.is_private.unwrap_or(false),
        })),
        None => json_resp(json!({
            "total_size": 0,
            "piece_size": 0,
            "pieces_have": 0,
            "is_private": false,
        })),
    }
}

async fn maindata() -> Response {
    json_resp(json!({
        "rid": 1,
        "full_update": true,
        "torrents": {},
        "server_state": { "alltime_ul": 0, "alltime_dl": 0 },
    }))
}

async fn torrent_peers(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let hash = params.get("hash").cloned().unwrap_or_default();
    let empty = Vec::new();
    let peers = state.fixture.peers.get(&hash).unwrap_or(&empty);
    let mut map = serde_json::Map::new();
    for p in peers {
        let key = format!(
            "{}:{}",
            p.ip.clone().unwrap_or_default(),
            p.port.unwrap_or(6881)
        );
        let mut obj = serde_json::Map::new();
        if let Some(v) = &p.client {
            obj.insert("client".into(), json!(v));
        }
        if let Some(v) = &p.peer_id_client {
            obj.insert("peer_id_client".into(), json!(v));
        }
        obj.insert("dl_speed".into(), json!(p.dl_speed));
        obj.insert("downloaded".into(), json!(p.downloaded));
        obj.insert("up_speed".into(), json!(p.up_speed));
        obj.insert("uploaded".into(), json!(p.uploaded));
        obj.insert("progress".into(), json!(p.progress));
        if let Some(v) = &p.flags {
            obj.insert("flags".into(), json!(v));
        }
        if let Some(v) = &p.ip {
            obj.insert("ip".into(), json!(v));
        }
        if let Some(v) = p.port {
            obj.insert("port".into(), json!(v));
        }
        if let Some(v) = &p.connection {
            obj.insert("connection".into(), json!(v));
        }
        map.insert(key, Value::Object(obj));
    }
    json_resp(json!({ "peers": Value::Object(map) }))
}

async fn index() -> Response {
    Html("<h1>PBH mock qBittorrent</h1><p>dual-run / benchmark fixture server</p>").into_response()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let raw = std::fs::read_to_string(&args.fixture)
        .map_err(|e| anyhow::anyhow!("读取 fixture 失败 {}: {e}", args.fixture))?;
    let fixture: Fixture =
        serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("解析 fixture 失败: {e}"))?;
    let state = AppState {
        fixture: Arc::new(fixture),
    };
    println!(
        "[mockqb] 载入 fixture: {} 个 torrent, {} 个有 peers 的 torrent",
        state.fixture.torrents.len(),
        state.fixture.peers.len()
    );

    let app = Router::new()
        .route("/", get(index))
        .route("/api/v2/auth/login", post(login))
        .route("/api/v2/app/buildInfo", get(build_info))
        .route("/api/v2/app/version", get(version))
        .route("/api/v2/app/preferences", get(preferences))
        .route("/api/v2/app/setPreferences", post(set_preferences))
        .route("/api/v2/transfer/banPeers", post(ban_peers_stub))
        .route("/api/v2/torrents/info", get(torrents_info))
        .route("/api/v2/torrents/properties", get(torrent_properties))
        .route("/api/v2/sync/maindata", get(maindata))
        .route("/api/v2/sync/torrentPeers", get(torrent_peers))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], args.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("[mockqb] 监听 http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
