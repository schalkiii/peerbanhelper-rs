//! AutoSTUN 的 WebUI 管理端点（`/api/autostun/*`，对齐上游 `PBHAutoStunController`）。
//!
//! 隧道编排由应用层管理器负责（`crates/pbh/src/auto_stun_manager.rs`）：它每 5 秒读一次
//! [`AutoStunSettings`] 并维护隧道，所以这里的读写都是「改配置 + 触发重载」，与上游
//! `handleConfigPut`（写 `auto-stun.*` → `reloadModule()`）一致。
//!
//! 已知差异：`/api/autostun/tunnel/{id}/connections` 的分页结果恒为空——上游的
//! 每连接明细（建立时间 / 活跃时间 / 双向字节 / GeoIP）依赖转发器的逐连接记账，
//! 本移植的转发器只做地址反查（`translate`）与总量计数，未记录逐连接明细。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::AppState;

/// 配置写回后的提示（对齐上游 `Lang.AUTOSTUN_CONFIG_REAPPLIED`）。
const CONFIG_REAPPLIED: &str = "AUTOSTUN_CONFIG_REAPPLIED";

fn ok(data: Value) -> Response {
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

fn failure(status: StatusCode, message: &str) -> Response {
    (
        status,
        crate::std_resp(false, Some(message), Value::Null),
    )
        .into_response()
}

/// 隧道信息 DTO（字段对齐前端 `api/model/autostun.ts` 的 `TunnelInfo`）。
fn tunnel_json(tunnel: &pbh_core::auto_stun_forwarder::StunTcpTunnel) -> Value {
    let forwarder = &tunnel.forwarder;
    json!({
        "valid": tunnel.is_valid(),
        "startedAt": tunnel.started_at(),
        "lastSuccessHeartbeatAt": tunnel.last_success_heartbeat_at(),
        "connectionsHandled": forwarder.connection_handled(),
        "connectionsFailed": forwarder.connection_failed(),
        "connectionsBlocked": forwarder.connection_rejected(),
        "totalToDownstreamBytes": forwarder.total_to_downstream_bytes(),
        "totalToUpstreamBytes": forwarder.total_to_upstream_bytes(),
        "establishedConnections": forwarder.established_connections(),
        "proxyHost": forwarder.config().proxy_host,
        "proxyPort": tunnel.inter.port(),
        "upstreamHost": forwarder.config().upstream_host,
        "upstreamPort": forwarder.config().upstream_port,
    })
}

/// 下载器基本信息（对齐 `DownloaderBasicInfo`）。
fn downloader_json(state: &AppState, id: &str) -> Value {
    let list = state
        .downloaders
        .lock()
        .map(|list| list.clone())
        .unwrap_or_default();
    match list.iter().find(|status| status.id == id) {
        Some(status) => json!({"id": id, "name": status.name, "type": status.kind}),
        None => json!({"id": id, "name": id, "type": ""}),
    }
}

/// `GET /api/autostun/status`：AutoSTUN 状态。
pub async fn status(State(state): State<AppState>) -> Response {
    let settings = (state.auto_stun_settings)();
    let nat_type = state
        .auto_stun_registry
        .as_ref()
        .map(|registry| registry.nat_type().as_str())
        .unwrap_or("Unknown");
    let selected: Vec<Value> = settings
        .downloaders
        .iter()
        .map(|id| downloader_json(&state, id))
        .collect();
    ok(json!({
        "enabled": settings.enabled,
        "useFriendlyLoopbackMapping": settings.use_friendly_loopback_mapping,
        "selectedDownloaders": selected,
        "natType": nat_type,
        // 上游按「默认网关 172.* + 内核 release 含 docker」判断是否跑在 Docker 桥接网络里
        // （仅用于提示）。本移植未接内核探测，恒 false。
        "isBridgeNetDriver": false,
    }))
}

/// `PUT /api/autostun/config`：保存 AutoSTUN 配置并触发重载。
pub async fn config(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let enabled = body
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let friendly = body
        .get("useFriendlyLoopbackMapping")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let downloaders: Vec<String> = body
        .get("downloaders")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut view = match state.backend.read_config("config") {
        Ok(view) => view,
        Err(e) => return failure(StatusCode::INTERNAL_SERVER_ERROR, &e),
    };
    let Some(map) = view.as_object_mut() else {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, "config is not an object");
    };
    // 上游写 `auto-stun.{enabled,use-friendly-loopback-mapping,downloaders}`；
    // 配置视图里键名是 snake_case（保存时会还原成连字符）。
    map.insert(
        "auto_stun".into(),
        json!({
            "enabled": enabled,
            "use_friendly_loopback_mapping": friendly,
            "downloaders": downloaders,
        }),
    );
    if let Err(e) = state.backend.write_config("config", &view) {
        return failure(StatusCode::INTERNAL_SERVER_ERROR, &e);
    }
    state.backend.reload();
    (
        StatusCode::OK,
        crate::std_resp(true, Some(CONFIG_REAPPLIED), Value::Null),
    )
        .into_response()
}

/// `GET /api/autostun/tunnels`：当前隧道列表（`[{downloader, tunnel}]`）。
pub async fn tunnels(State(state): State<AppState>) -> Response {
    let Some(registry) = state.auto_stun_registry.as_ref() else {
        return ok(json!(Vec::<Value>::new()));
    };
    let items: Vec<Value> = registry
        .tunnels()
        .iter()
        .map(|(id, tunnel)| {
            json!({
                "downloader": downloader_json(&state, id),
                "tunnel": tunnel_json(tunnel),
            })
        })
        .collect();
    ok(json!(items))
}

/// `GET /api/autostun/tunnel/{id}/info`：单个下载器的隧道信息。
pub async fn tunnel_info(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(registry) = state.auto_stun_registry.as_ref() else {
        return failure(StatusCode::OK, "AUTOSTUN_NOT_SUPPORTED");
    };
    match registry
        .tunnels()
        .iter()
        .find(|(downloader_id, _)| downloader_id == &id)
        .map(|(_, tunnel)| tunnel_json(tunnel))
    {
        Some(info) => ok(info),
        None => failure(StatusCode::OK, "AUTOSTUN_TUNNEL_NOT_FOUND"),
    }
}

/// `GET /api/autostun/tunnel/{id}/connections`：隧道连接明细（分页）。
///
/// 暂未实现逐连接记账（见模块文档），返回结构完整的分页空页。
pub async fn connections(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
    Query(_params): Query<HashMap<String, String>>,
) -> Response {
    ok(json!({
        "page": 1,
        "size": 10,
        "total": 0,
        "results": Vec::<Value>::new(),
    }))
}

/// `POST /api/autostun/refreshNatType`：立即刷新 NAT 类型（上游立即返回、异步探测）。
pub async fn refresh_nat_type(State(state): State<AppState>) -> Response {
    if let Some(registry) = state.auto_stun_registry.clone() {
        // UDP 探测是阻塞的（每台服务器有超时），放到独立线程，接口立即返回
        std::thread::spawn(move || {
            let nat_type = registry.refresh_nat_type();
            tracing::info!("AutoSTUN NAT 类型已刷新: {}", nat_type.as_str());
        });
    }
    ok(json!({ "status": "Refreshing NAT Status" }))
}

/// `POST /api/autostun/restart`：重载配置（上游等价于 `reloadModule()`）。
pub async fn restart(State(state): State<AppState>) -> Response {
    state.backend.reload();
    ok(json!({ "status": "AutoSTUN restarted" }))
}
