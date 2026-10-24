//! 下载器管理端点（对齐 `PBHDownloaderController`）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::AppState;

/// `PUT /api/downloaders`：新增下载器（body: `{id, config}`）。
pub async fn create(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let cfg = body.get("config").cloned().unwrap_or(body.clone());
    match state.backend.add_downloader(&cfg) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), json!({"success": true})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `POST /api/downloaders/scan`：本机下载器扫描（上游 `DownloaderDiscovery`）。
///
/// 枚举本机 LISTEN 端点并探测 qBittorrent / Transmission / BiglyBT / BitComet；
/// 已在配置中的端口标记为 `ADDED`。返回裸数组（上游 `List<DiscoveredDownloader>`）。
pub async fn scan(State(state): State<AppState>) -> Response {
    let ids: Vec<String> = state
        .downloaders
        .lock()
        .map(|list| list.iter().map(|status| status.id.clone()).collect())
        .unwrap_or_default();
    let mut added_ports = Vec::new();
    for id in ids {
        if let Some(meta) = state.backend.downloader_meta(&id) {
            if let Some(port) = endpoint_port(&meta.endpoint) {
                added_ports.push(port);
            }
        }
    }
    let found = pbh_downloader::discovery::scan(&added_ports).await;
    (StatusCode::OK, crate::std_resp(true, None, json!(found))).into_response()
}

fn endpoint_port(endpoint: &str) -> Option<u16> {
    let rest = endpoint.split("://").nth(1).unwrap_or(endpoint);
    let host_port = rest.split('/').next()?;
    host_port.rsplit_once(':')?.1.parse().ok()
}

/// `GET /api/downloaders/test`：测试配置连通性（body 为 `{config}`）。
pub async fn test(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let cfg = body.get("config").cloned().unwrap_or(body);
    match state.backend.test_downloader(&cfg) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), json!({"success": true})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `PATCH /api/downloaders/{id}`：更新下载器。
pub async fn update(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let cfg = body.get("config").cloned().unwrap_or(body);
    match state.backend.update_downloader(&id, &cfg) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), json!({"success": true})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `DELETE /api/downloaders/{id}`：移除下载器。
pub async fn remove(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.backend.remove_downloader(&id) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), json!({"success": true})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/downloaders/{id}/status`：单下载器详细状态。
///
/// 对齐上游 `DownloaderStatusDTO(lastStatus, lastStatusMessage, activeTorrents,
/// activePeers, config, paused)`：WebUI 首页卡片的状态文案读 `lastStatus` 枚举、
/// **编辑弹窗从 `config` 填充表单**（上游数据源为 `saveDownloaderJson()`）——
/// 缺失会让卡片显示「未知」、编辑弹窗全空。
pub async fn status(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let statuses = state
        .downloaders
        .lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    if let Some(status) = statuses.iter().find(|s| s.id == id) {
        let meta = state.backend.downloader_meta(&id);
        let (config, paused) = meta
            .as_ref()
            .map(|m| (m.config.clone(), m.paused))
            .unwrap_or((Value::Null, false));
        // 上游 DownloaderLastStatus 枚举：HEALTHY/PAUSED/NEED_TAKE_ACTION/ERROR/UNKNOWN
        let (last_status, last_status_message) = if paused {
            ("PAUSED", "PAUSED")
        } else if status.online {
            ("HEALTHY", "OK")
        } else {
            ("ERROR", "ERROR")
        };
        let body = json!({
            "id": status.id,
            "name": status.name,
            "type": status.kind,
            "online": status.online,
            "version": status.version,
            "lastStatus": last_status,
            "lastStatusMessage": last_status_message,
            "activeTorrents": status.torrents,
            "activePeers": status.peers,
            "config": config,
            "paused": paused,
            "endpoint": meta.as_ref().map(|m| m.endpoint.clone()).unwrap_or_default(),
        });
        (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), body),
        )
            .into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            crate::std_resp(false, Some("DOWNLOADER_NOT_FOUND"), Value::Null),
        )
            .into_response()
    }
}

/// 一条实时种子 → `TorrentWrapper`（对齐上游全 9 字段）。
///
/// WebUI「活动种子」的速度列直接读 `rtUploadSpeed` / `rtDownloadSpeed`，
/// 缺失会渲染成 `NaN undefined/s`（2026-10-11 生产缺陷，已由契约测试固化）。
/// 抽成函数以便单测字段集合，避免重演「只断言 200 不断言字段名」的漏洞。
pub(crate) fn torrent_data_json(t: &pbh_core::model::TorrentData) -> Value {
    json!({
        "id": t.hash,
        "name": t.name,
        "size": t.total_size,
        "completedSize": t.completed_size(),
        "hash": t.hash,
        "privateTorrent": t.is_private.unwrap_or(false),
        "progress": t.progress,
        "rtUploadSpeed": t.upspeed,
        "rtDownloadSpeed": t.dlspeed,
    })
}

/// 一条实时 peer → 上游 `PopulatedPeerDTO(peer, geo, ptrRecord)`。
///
/// WebUI 的活动种子 Peer 弹窗直接读 `record.peer.address.ip` / `record.peer.uploadSpeed`
/// / `record.geo.*`；返回扁平对象会让这些列全部空白。
fn peer_wrapper_json(
    p: &pbh_core::model::PeerData,
    geo: Value,
    ptr_record: Option<String>,
) -> Value {
    json!({
        "peer": {
            // 对齐 `PeerAddress` 的全部序列化字段（`address` 为 transient，不输出）
            "address": {
                "downloaderRawIp": p.raw_ip,
                "downloaderRawPort": p.port,
                "teredoClientIp": Value::Null,
                "teredoClientUdpPort": 0,
                "nattedClientIp": Value::Null,
                "nattedClientPort": 0,
                "ip": p.ip,
                "port": p.port,
                "natTranslated": false,
                "teredoTranslated": false,
            },
            "rawIp": p.raw_ip,
            "id": p.peer_id.clone().unwrap_or_default(),
            "clientName": p.client_name.clone().unwrap_or_default(),
            "downloaded": p.downloaded,
            "downloadSpeed": p.dl_speed,
            "uploaded": p.uploaded,
            "uploadSpeed": p.up_speed,
            "progress": p.progress,
            "flags": pbh_core::modules::monitor::lt_std_string_opt(p.flags.as_deref()),
        },
        "geo": geo,
        // PTR 仅在 `lookup.dns-reverse-lookup` 开启时解析（默认关闭 ⇒ null）
        "ptrRecord": ptr_record,
    })
}


/// `GET /api/downloaders/{id}/torrents`：实时种子列表。
///
/// 对齐上游 `TorrentWrapper(id, size, completedSize, name, hash, privateTorrent,
/// progress, rtUploadSpeed, rtDownloadSpeed)`：WebUI「活动种子」的速度列直接读
/// `rtUploadSpeed` / `rtDownloadSpeed`，缺失会渲染成 `NaN undefined/s`。
pub async fn torrents(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if state.backend.downloader(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            crate::std_resp(false, Some("DOWNLOADER_NOT_FOUND"), Value::Null),
        )
            .into_response();
    }
    // 上游读取 `getLivePeersSnapshot()`（最近一轮 wave），不再单独请求下载器
    let mut torrents = state.live_snapshot.torrents(&id);
    torrents.sort_by(|a, b| {
        b.upspeed
            .cmp(&a.upspeed)
            .then(b.dlspeed.cmp(&a.dlspeed))
    });
    let results = torrents.iter().map(torrent_data_json).collect::<Vec<_>>();
    (
        StatusCode::OK,
        crate::std_resp(true, Some("OK"), json!(results)),
    )
        .into_response()
}

/// `GET /api/downloaders/{id}/torrent/{torrent_id}/peers`：指定种子的实时 Peer 列表。
pub async fn peers(
    State(state): State<AppState>,
    Path((id, torrent_id)): Path<(String, String)>,
) -> Response {
    if state.backend.downloader(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            crate::std_resp(false, Some("DOWNLOADER_NOT_FOUND"), Value::Null),
        )
            .into_response();
    }
    // 上游从 live snapshot 过滤：下载器存在但 torrent 不在本轮快照时返回空数组
    let mut peers = state.live_snapshot.peers(&id, &torrent_id);
    // 上游按 `uploadSpeed` 倒序（`handlePeersInTorrentOnDownloader`）
    peers.sort_by_key(|peer| std::cmp::Reverse(peer.up_speed));
    let ptr_enabled = state.ptr.enabled();
    let results = peers
        .iter()
        .map(|p| {
            let ptr = ptr_enabled.then(|| state.ptr.lookup(&p.ip)).flatten();
            peer_wrapper_json(p, crate::api::query_geo(&state, &p.ip), ptr)
        })
        .collect::<Vec<_>>();
    (
        StatusCode::OK,
        crate::std_resp(true, Some("OK"), json!(results)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbh_core::model::PeerData;

    fn sample_peer() -> PeerData {
        PeerData {
            client_name: Some("qBittorrent 5.0".into()),
            peer_id: Some("-qB5000-".into()),
            dl_speed: 12,
            downloaded: 34,
            up_speed: 56,
            uploaded: 78,
            progress: 0.5,
            flags: Some("d U".into()),
            ip: "1.2.3.4".into(),
            port: 6881,
            raw_ip: "1.2.3.4:6881".into(),
            connection: None,
        }
    }

    /// WebUI 的活动种子 Peer 弹窗按 `record.peer.*` / `record.geo` 读取；
    /// 回归锁定嵌套结构，避免再次返回扁平对象导致整表空白。
    #[test]
    fn peer_wrapper_matches_populated_peer_dto_shape() {
        let value = peer_wrapper_json(
            &sample_peer(),
            serde_json::json!({ "country": { "iso": "US" } }),
            Some("host.example.com.".into()),
        );
        let peer = &value["peer"];
        assert_eq!(peer["address"]["ip"], "1.2.3.4");
        assert_eq!(peer["address"]["port"], 6881);
        assert_eq!(peer["address"]["downloaderRawIp"], "1.2.3.4:6881");
        assert_eq!(peer["rawIp"], "1.2.3.4:6881");
        assert_eq!(peer["id"], "-qB5000-");
        assert_eq!(peer["clientName"], "qBittorrent 5.0");
        assert_eq!(peer["downloadSpeed"], 12);
        assert_eq!(peer["uploadSpeed"], 56);
        assert_eq!(peer["progress"], 0.5);
        assert_eq!(peer["flags"], "d U");
        assert_eq!(value["geo"]["country"]["iso"], "US");
        assert_eq!(value["ptrRecord"], "host.example.com.");
    }
}
