//! Mock qBittorrent WebUI 服务（对跑/性能基准基座）。
//!
//! 从一份 fixture JSON 提供 qB v2 WebAPI 的最小可用子集，使 PeerBanHelper 的 Rust 版与
//! Java 版能连接**同一份确定性输入**（torrents / peers），从而：
//! - 对跑（L5）：对比两者产出的封禁决策集合与理由；
//! - 性能基准：测量各自启动耗时、常驻内存（RSS）与单轮 ban wave 耗时。
//!
//! fixture 字段对齐 `crates/pbh-downloader/src/qbittorrent/dto.rs` 的 DTO。

use axum::extract::{Form, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

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
    /// 多波演化（可选）：`waves[k]` 是第 `k+1` 次 `torrentPeers` 请求后的生效快照。
    ///
    /// 覆盖语义：`torrents` / `peers` 均可选，给出的部分**替换**基础段（peers 按 hash
    /// 整组替换）；未给出的沿用当前值。请求次数超过 waves 长度时恒用最后一份——
    /// 使「进度回退 → 稳定」之类的有状态场景（PCB / 多拨累计）在长跑中保持稳定。
    /// 无 `waves` 时与旧行为一致（恒返回基础段）。
    #[serde(default)]
    waves: Vec<Wave>,
}

/// 单个波次的覆盖快照。
#[derive(Debug, Clone, Default, Deserialize)]
struct Wave {
    #[serde(default)]
    torrents: Option<Vec<Torrent>>,
    #[serde(default)]
    peers: Option<HashMap<String, Vec<Peer>>>,
}

impl Fixture {
    /// 第 `request_no` 次（从 1 起）`torrentPeers` 请求时 `hash` 对应的 peers：
    /// 基础段依次被 `waves[0..min(request_no-1, len)]` 中给出该 hash 的波覆盖。
    fn peers_at(&self, hash: &str, request_no: u64) -> Option<&Vec<Peer>> {
        let mut current = self.peers.get(hash);
        let take = (request_no.saturating_sub(1) as usize).min(self.waves.len());
        for wave in &self.waves[..take] {
            if let Some(group) = wave.peers.as_ref().and_then(|m| m.get(hash)) {
                current = Some(group);
            }
        }
        current
    }

    /// 第 `request_no` 次（从 1 起）`torrents/info` 的生效种子列表（覆盖语义同上，
    /// 波内未给出 `torrents` 时沿用当前值；`None` 表示无种子）。
    fn torrents_at(&self, request_no: u64) -> &Vec<Torrent> {
        let take = (request_no.saturating_sub(1) as usize).min(self.waves.len());
        let mut current = &self.torrents;
        for wave in &self.waves[..take] {
            if let Some(ts) = wave.torrents.as_ref() {
                current = ts;
            }
        }
        current
    }
}

fn default_version() -> String {
    "5.0.0".to_string()
}

#[derive(Clone)]
struct AppState {
    fixture: Arc<Fixture>,
    /// 对跑录制文件：把收到的封禁下发（增量 `banPeers` 与全量 `banned_IPs`）
    /// 逐 IP 追加写入，使 Java 与 Rust 两版的封禁集合可直接做跨版本 diff。
    record: Option<Arc<Mutex<std::fs::File>>>,
    /// 每 hash 的 `torrentPeers` 请求次数（驱动 waves 波次轮转）
    wave_counter: Arc<Mutex<HashMap<String, u64>>>,
    /// `torrents/info` 的调用次数（与 wave 同频，驱动种子列表轮转）
    torrents_counter: Arc<Mutex<u64>>,
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
    /// 封禁下发录制文件路径（逐 IP 追加）；指定后用于跨版本封禁集合 diff
    #[arg(long)]
    record: Option<String>,
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

/// 全量下发：`json={"banned_IPs":"a\nb\nc"}`，按行拆分后逐条录制。
async fn set_preferences(
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(raw) = form.get("json") {
        if let Ok(value) = serde_json::from_str::<Value>(raw) {
            if let Some(list) = value.get("banned_IPs").and_then(|v| v.as_str()) {
                for ip in list.lines() {
                    record_ip(&state, ip);
                }
            }
        }
    }
    (StatusCode::OK, text("Ok.")).into_response()
}

/// 增量下发：`peers=a|b|c`，按 `|` 拆分后逐条录制。
async fn ban_peers(
    State(state): State<AppState>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    if let Some(joined) = form.get("peers") {
        for ip in joined.split('|') {
            record_ip(&state, ip);
        }
    }
    text("Ok.")
}

/// 把一条封禁地址追加到录制文件（去重交给后续 diff 阶段处理）。
fn record_ip(state: &AppState, ip: &str) {
    let ip = ip.trim();
    if ip.is_empty() {
        return;
    }
    if let Some(file) = &state.record {
        if let Ok(mut guard) = file.lock() {
            let _ = writeln!(guard, "{ip}");
            let _ = guard.flush();
        }
    }
}

async fn torrents_info(State(state): State<AppState>) -> Response {
    // 波次推进：PBH 每波调用一次 torrents/info，与 torrentPeers 的 per-hash 计数同频
    let request_no = {
        let mut counter = state
            .torrents_counter
            .lock()
            .expect("torrents_counter poisoned");
        *counter += 1;
        *counter
    };
    let mut list = Vec::new();
    for t in state.fixture.torrents_at(request_no) {
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
    // 波次推进：第 k 次请求返回 base ⊕ waves[0..min(k-1, len)]
    let request_no = {
        let mut counter = state
            .wave_counter
            .lock()
            .expect("wave_counter poisoned");
        let entry = counter.entry(hash.clone()).or_insert(0);
        *entry += 1;
        *entry
    };
    let empty = Vec::new();
    let peers = state.fixture.peers_at(&hash, request_no).unwrap_or(&empty);
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
    let record = match args.record.as_deref() {
        Some(path) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| anyhow::anyhow!("打开录制文件失败 {path}: {e}"))?;
            Some(Arc::new(Mutex::new(file)))
        }
        None => None,
    };
    let state = AppState {
        fixture: Arc::new(fixture),
        record,
        wave_counter: Arc::new(Mutex::new(HashMap::new())),
        torrents_counter: Arc::new(Mutex::new(0)),
    };
    println!(
        "[mockqb] 载入 fixture: {} 个 torrent, {} 个有 peers 的 torrent, {} 个波次",
        state.fixture.torrents.len(),
        state.fixture.peers.len(),
        state.fixture.waves.len()
    );

    let app = Router::new()
        .route("/", get(index))
        .route("/api/v2/auth/login", post(login))
        .route("/api/v2/app/buildInfo", get(build_info))
        .route("/api/v2/app/version", get(version))
        .route("/api/v2/app/preferences", get(preferences))
        .route("/api/v2/app/setPreferences", post(set_preferences))
        .route("/api/v2/transfer/banPeers", post(ban_peers))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waves_rotate_by_request_number_and_stick_to_last() {
        let f: Fixture = serde_json::from_str(
            r#"{
                "peers": { "H": [ {"ip":"1.1.1.1","port":1,"progress":0.9} ] },
                "waves": [
                    { "peers": { "H": [ {"ip":"1.1.1.1","port":1,"progress":0.45} ] } },
                    { "peers": { "H": [ {"ip":"1.1.1.1","port":1,"progress":0.45} ] } }
                ]
            }"#,
        )
        .unwrap();
        let prog = |n: u64| f.peers_at("H", n).unwrap()[0].progress;
        assert_eq!(prog(1), 0.9, "第 1 次请求返回基础段");
        assert_eq!(prog(2), 0.45, "第 2 次请求返回 waves[0]");
        assert_eq!(prog(3), 0.45, "第 3 次请求返回 waves[1]");
        assert_eq!(prog(9), 0.45, "超出波次后恒用最后一份");
    }

    #[test]
    fn missing_wave_entries_fall_through_to_current() {
        // waves[0] 未给出 hash "X" 的 peers -> 沿用当前值；waves[1] 给出 -> 覆盖
        let f: Fixture = serde_json::from_str(
            r#"{
                "peers": { "X": [ {"ip":"3.3.3.3","port":3} ] },
                "waves": [ {"peers": {"H": []}}, {"peers": {"X": [ {"ip":"4.4.4.4","port":4} ]}} ]
            }"#,
        )
        .unwrap();
        assert_eq!(f.peers_at("X", 1).unwrap()[0].ip.as_deref(), Some("3.3.3.3"));
        assert_eq!(f.peers_at("X", 2).unwrap()[0].ip.as_deref(), Some("3.3.3.3"));
        assert_eq!(f.peers_at("X", 3).unwrap()[0].ip.as_deref(), Some("4.4.4.4"));
    }

    #[test]
    fn no_waves_keeps_legacy_behavior() {
        let f: Fixture =
            serde_json::from_str(r#"{"peers": {"H": [{"ip":"1.1.1.1","port":1}]}}"#).unwrap();
        for n in 1..=5u64 {
            assert_eq!(f.peers_at("H", n).unwrap()[0].ip.as_deref(), Some("1.1.1.1"));
        }
    }
}
