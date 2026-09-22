//! 告警写端点（对齐 `PBHAlertController` 的 dismiss / dismissAll / delete；
//! GET `/api/alerts` 仍在 `lib.rs`）。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::AppState;

/// `PATCH /api/alert/{id}/dismiss`：标记单条已读；目标不存在时返回 404。
pub async fn dismiss(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let exists = state.db.get_alert_by_id(id).ok().flatten().is_some();
    if !exists {
        return (
            StatusCode::NOT_FOUND,
            crate::std_resp(false, Some("ALERT_NOT_FOUND"), serde_json::Value::Null),
        )
            .into_response();
    }
    let _ = state.db.mark_alert_read(id);
    crate::std_resp(true, Some("OK"), serde_json::Value::Null).into_response()
}

/// `POST /api/alert/dismissAll`：全部已读。
pub async fn dismiss_all(State(state): State<AppState>) -> Response {
    let _ = state.db.mark_all_alerts_read();
    crate::std_resp(true, Some("OK!"), serde_json::Value::Null).into_response()
}

/// `DELETE /api/alert/{id}`：删除单条告警（幂等，目标不存在同样返回 OK）。
pub async fn delete_alert(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let _ = state.db.delete_alert_by_id(id);
    crate::std_resp(true, Some("OK"), serde_json::Value::Null).into_response()
}
