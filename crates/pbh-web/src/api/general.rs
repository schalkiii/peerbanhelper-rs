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
    let uptime = state.started.elapsed().as_secs();
    // 编译时间：exe 修改时间（对齐上游「构建时间戳」展示语义；Rust 无稳定的
    // 编译期内嵌时间戳手段，运行时取二进制 mtime 等价——否则前端显示 1970）
    let compile_time = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::metadata(p).ok())
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let (china_ok, global_ok) = network_reachability();
    let memory = system_memory();
    let (_, btn_data) = crate::api::btn::btn_status_data(&state);
    let data = json!({
        "jvm": {
            "version": "Rust",
            "vendor": "rustc",
            "runtime": "rustc",
            "bitness": if std::mem::size_of::<usize>() == 8 { 64 } else { 32 },
            // WebUI 首页「堆内存信息」：无 JVM 堆概念，用系统物理内存近似
            // （空对象会让前端显示「0 Bytes 可用」红点）
            "memory": { "heap": memory, "non_heap": memory },
        },
        "system": {
            "os": std::env::consts::OS,
            "version": std::env::consts::ARCH,
            "architecture": std::env::consts::ARCH,
            "cores": std::thread::available_parallelism().map(|v| v.get()).unwrap_or(1),
            "load": 0,
            "network": {
                "internet_access": {
                    "accessToChinaNetwork": china_ok,
                    "accessToGlobalNetwork": global_ok,
                },
                "nat_type": "unknown",
                "use_proxy": false,
                "reverse_proxy": false,
                "client_ip": "",
            },
        },
        // 设置页「BTN 状态」直接读本段（与 /api/modules/btn 同源）
        "btn": btn_data,
        "peerbanhelper": {
            "version": env!("CARGO_PKG_VERSION"),
            "commit_id": option_env!("GIT_COMMIT").unwrap_or("unknown"),
            "compile_time": compile_time,
            "release": "Rust",
            "uptime": uptime,
            "token": "REDACTED",
        },
    });
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// 网络可达性探针（对齐上游 `HTTPUtil.getNetworkReachability` 的内外网语义）：
/// 国内探针 baidu.com:443、国际探针 cloudflare.com:443，短超时 TCP 连接测试。
fn network_reachability() -> (bool, bool) {
    use std::net::{TcpStream, ToSocketAddrs};
    use std::time::Duration;
    let probe = |host: &str| {
        (host, 443u16)
            .to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .and_then(|addr| TcpStream::connect_timeout(&addr, Duration::from_millis(900)).ok())
            .is_some()
    };
    (probe("www.baidu.com"), probe("www.cloudflare.com"))
}

/// 系统物理内存（字节）：`{max, committed, used, available, init}`。
/// WebUI 的「堆内存信息」直接消费（`available` 驱动「X 可用」显示）。
#[cfg(windows)]
fn system_memory() -> Value {
    use windows_sys::Win32::System::SystemInformation::{
        GlobalMemoryStatusEx, MEMORYSTATUSEX,
    };
    unsafe {
        let mut ms: MEMORYSTATUSEX = std::mem::zeroed();
        ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if GlobalMemoryStatusEx(&mut ms) != 0 {
            let total = ms.ullTotalPhys as u64;
            let available = ms.ullAvailPhys as u64;
            let used = total.saturating_sub(available);
            return json!({
                "max": total,
                "committed": used,
                "used": used,
                "available": available,
                "init": used,
            });
        }
    }
    json!({"max": 0, "committed": 0, "used": 0, "available": 0, "init": 0})
}

#[cfg(not(windows))]
fn system_memory() -> Value {
    json!({"max": 0, "committed": 0, "used": 0, "available": 0, "init": 0})
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
