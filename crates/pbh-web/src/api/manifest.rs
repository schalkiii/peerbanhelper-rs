//! 元数据端点（对齐 `PBHMetadataController` 的 `/api/metadata/manifest`）。

use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

use crate::AppState;

/// `GET /api/metadata/manifest`（Role.ANYONE，无需鉴权）。
pub async fn manifest(State(state): State<AppState>) -> Response {
    let backend = state.backend.as_ref();
    let modules: Vec<Value> = backend
        .modules()
        .iter()
        .map(|m| json!({ "className": m.class_name, "configName": m.config_name }))
        .collect();
    let data = json!({
        "version": {
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "branch": option_env!("GIT_BRANCH").unwrap_or("main"),
            "commit": option_env!("GIT_COMMIT").unwrap_or("unknown"),
            "abbrev": option_env!("GIT_ABBREV").unwrap_or("unknown"),
        },
        "modules": modules,
        "installationId": backend.installation_id(),
        "analytics": backend.analytics_enabled(),
    });
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `GET /api/init/token` / `GET /api/oobe/status`：返回当前是否已初始化。
/// `POST /api/oobe/testDownloader`：OOBE 向导的下载器连通性测试
/// （对齐上游 `PBHOOBEController`；复用下载器管理层的测试实现）。
pub async fn oobe_test_downloader(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    match state.backend.test_downloader(&body) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, None, json!({ "success": true })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::OK,
            crate::std_resp(false, Some(&e), json!({ "success": false })),
        )
            .into_response(),
    }
}

/// `POST /api/oobe/scanDownloader`：局域网下载器扫描。
///
/// 本移植未实现上游的 `DownloaderDiscovery`（mDNS/端口探测），返回空结果——
/// OOBE 向导引导用户手动填写下载器地址（主路径不受影响）。
pub async fn oobe_scan_downloader() -> Response {
    // 上游返回 `List<DiscoveredDownloader>`；本移植未实现 mDNS/端口探测 ⇒ 空列表
    (
        StatusCode::OK,
        crate::std_resp(true, None, json!([])),
    )
        .into_response()
}

/// `POST /api/oobe/testDatabaseConfig`：数据库配置测试。
///
/// 本移植固定使用内置 SQLite（无外部数据库配置项），恒返回成功。
pub async fn oobe_test_database_config(Json(_body): Json<Value>) -> Response {
    (
        StatusCode::OK,
        crate::std_resp(true, None, json!({ "success": true, "driver": "sqlite" })),
    )
        .into_response()
}

/// `POST /api/oobe/init`：OOBE 向导提交初始化（`{token, downloader, btn, database}`）。
///
/// 该端点此前缺失，向导最后一步会拿到 `API_NOT_FOUND`。本移植的初始化大部分是
/// 自动的（访问令牌在首次启动时随机生成并写回配置、数据库固定内置 SQLite），
/// 因此这里只落实**有实际效果的两项**：访问令牌与下载器；`btn` / `database` 忽略。
pub async fn oobe_init(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    // 访问令牌：更新运行态 + 经后端写回 config.yml（重启后保持）
    if let Some(token) = body.get("token").and_then(|v| v.as_str()) {
        if !token.trim().is_empty() {
            if let Err(e) = state.backend.set_token(token) {
                return (
                    StatusCode::OK,
                    crate::std_resp(false, Some(&e), Value::Null),
                )
                    .into_response();
            }
            if let Ok(mut current) = state.token.lock() {
                *current = token.to_string();
            }
        }
    }
    // 下载器：交由后端持久化（对齐向导「添加一个下载器」的主路径）
    if let Some(downloader) = body.get("downloader") {
        if downloader.is_object() {
            if let Err(e) = state.backend.add_downloader(downloader) {
                return (
                    StatusCode::OK,
                    crate::std_resp(false, Some(&e), Value::Null),
                )
                    .into_response();
            }
        }
    }
    (
        StatusCode::OK,
        crate::std_resp(true, None, json!({ "success": true })),
    )
        .into_response()
}

/// `GET /api/init/token`：初始化状态。
pub async fn init_status(State(state): State<AppState>) -> Response {
    let initialized = !state
        .token
        .lock()
        .map(|t| t.trim().is_empty())
        .unwrap_or(true);
    (
        StatusCode::OK,
        crate::std_resp(true, None, json!({ "initialized": initialized })),
    )
        .into_response()
}
