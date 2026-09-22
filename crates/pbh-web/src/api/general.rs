//! 通用配置 / 运行状态端点（对齐 `PBHGeneralController`）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::AppState;

/// `GET /api/general/status`：整体运行信息（含 JVM→运行时约定、内存由系统层填充）。
pub async fn status(State(state): State<AppState>) -> Response {
    let backend = state.backend.as_ref();
    let uptime = state.started.elapsed().as_secs();
    let data = json!({
        "jvm": {
            "version": "Rust ",
            "vendor": "rustc",
            "runtime": std::env::var("RUST_BACKTRACE").unwrap_or_default(),
            "bitness": if std::mem::size_of::<usize>() == 8 { 64 } else { 32 },
            "memory": { "heap": {}, "non_heap": {} },
        },
        "system": {
            "os": std::env::consts::OS,
            "version": std::env::consts::ARCH,
            "architecture": std::env::consts::ARCH,
            "cores": std::thread::available_parallelism().map(|v| v.get()).unwrap_or(1),
            "load": 0,
            "network": {
                "internet_access": { "accessToChinaNetwork": false, "accessToGlobalNetwork": false },
                "nat_type": "unknown",
            },
        },
        "peerbanhelper": {
            "version": env!("CARGO_PKG_VERSION"),
            "commit_id": option_env!("GIT_COMMIT").unwrap_or("unknown"),
            "compile_time": 0,
            "release": "Rust",
            "uptime": uptime,
            "token": "REDACTED",
        },
    });
    let _ = backend;
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `GET /api/general/global`：全局配置（暂停 / 匿名统计）。
pub async fn global_get(State(state): State<AppState>) -> Response {
    let backend = state.backend.as_ref();
    let data = json!({
        "globalPaused": backend.global_paused(),
        "analytics": backend.analytics_enabled(),
    });
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `PATCH /api/general/global`：更新全局配置。
pub async fn global_patch(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let backend = state.backend.as_ref();
    if let Some(paused) = body.get("globalPaused").and_then(|v| v.as_bool()) {
        if let Err(e) = backend.set_global_paused(paused) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::std_resp(false, Some(&e), Value::Null),
            )
                .into_response();
        }
    }
    if let Some(enabled) = body.get("analytics").and_then(|v| v.as_bool()) {
        if let Err(e) = backend.set_analytics(enabled) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                crate::std_resp(false, Some(&e), Value::Null),
            )
                .into_response();
        }
    }
    crate::std_resp(true, Some("OK"), Value::Null).into_response()
}

/// `POST /api/general/reload`：重载所有模块配置。
pub async fn reload(State(state): State<AppState>) -> Response {
    let results = state.backend.reload();
    let data = json!({
        "results": results.iter().map(|r| json!({
            "moduleName": r.module_name,
            "changes": r.results,
        })).collect::<Vec<_>>(),
    });
    (StatusCode::OK, crate::std_resp(true, Some("OK"), data)).into_response()
}

/// `GET /api/general/checkModuleAvailable?module=xxx`
pub async fn check_module_available(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let module = query.get("module").cloned().unwrap_or_default();
    let available = state
        .backend
        .modules()
        .iter()
        .any(|m| m.config_name == module);
    (
        StatusCode::OK,
        crate::std_resp(true, Some("OK"), json!(available)),
    )
        .into_response()
}

/// `GET /api/general/config` / `GET /api/general/profile`：读取配置文件。
pub async fn config_get(State(state): State<AppState>, uri: axum::http::Uri) -> Response {
    let name = if uri.path().ends_with("/profile") {
        "profile"
    } else {
        "config"
    };
    match state.backend.read_config(name) {
        Ok(value) => (StatusCode::OK, crate::std_resp(true, Some("OK"), value)).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `PUT /api/general/config` / `PUT /api/general/profile`：整文件保存。
pub async fn config_put(
    State(state): State<AppState>,
    uri: axum::http::Uri,
    Json(body): Json<Value>,
) -> Response {
    let name = if uri.path().ends_with("/profile") {
        "profile"
    } else {
        "config"
    };
    match state.backend.write_config(name, &body) {
        Ok(()) => crate::std_resp(true, Some("OK"), Value::Null).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `GET /api/general/heapdump`：下载运行摘要（Rust 无 JVM 堆，输出线程统计文本）。
pub async fn heapdump() -> Response {
    let body = format!(
        "PeerBanHelper-RS {}\nthreads: {}\n",
        env!("CARGO_PKG_VERSION"),
        std::thread::current().name().unwrap_or("main")
    );
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
    )
        .into_response()
}

/// `GET /api/general/stacktrace`：输出当前线程栈的快照。
pub async fn stacktrace() -> Response {
    let thread = std::thread::current();
    let name = thread.name().unwrap_or("unknown");
    let body = format!("{name}: {thread:?}\n");
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        body,
    )
        .into_response()
}
