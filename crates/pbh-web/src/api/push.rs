//! 推送渠道端点（对齐 `PBHPushController`）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::AppState;

/// `GET /api/push`：推送渠道列表。
pub async fn list(State(state): State<AppState>) -> Response {
    let channels = state.backend.push_channels();
    (
        StatusCode::OK,
        crate::std_resp(true, Some("OK"), json!({ "channels": channels })),
    )
        .into_response()
}

/// `PUT /api/push`：新建推送渠道（body 为单个渠道配置，如 `{name, type, ...}`）。
pub async fn create(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    match state.backend.add_push_channel(&body) {
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

/// `PATCH /api/push/{name}`：更新推送渠道。
pub async fn update(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    match state.backend.update_push_channel(&name, &body) {
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

/// `DELETE /api/push/{name}`：删除推送渠道。
pub async fn remove(State(state): State<AppState>, Path(name): Path<String>) -> Response {
    match state.backend.remove_push_channel(&name) {
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

/// `POST /api/push/test`：测试推送（body 为渠道配置，不保存）。
pub async fn test(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    match state.backend.test_push_channel(&body) {
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
