//! 元数据端点（对齐 `PBHMetadataController` 的 `/api/metadata/manifest`）。

use axum::extract::State;
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
pub async fn init_status(State(state): State<AppState>) -> Response {
    let initialized = !state.token.lock().map(|t| t.trim().is_empty()).unwrap_or(true);
    (
        StatusCode::OK,
        crate::std_resp(true, None, json!({ "initialized": initialized })),
    )
        .into_response()
}