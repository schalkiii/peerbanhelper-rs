//! 第三方模块状态端点（`/api/modules/btn` 与 AutoSTUN 状态）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::AppState;

/// `GET /api/modules/btn`：BTN 模块状态（Rust 版未内置 BTN，返回未配置状态）。
pub async fn status(State(_): State<AppState>) -> Response {
    let data = json!({
        "abilities": [],
        "appId": "",
        "appSecret": "",
        "configSuccess": false,
        "configUrl": "",
        "configResult": "BTN is not available in this build",
    });
    (StatusCode::OK, crate::std_resp(true, Some("OK"), data)).into_response()
}

/// `GET /api/modules/auto-stun-port-forwarding/status`：AutoSTUN 状态。
pub async fn auto_stun_status(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let status = state.auto_stun_status();
    let _ = params;
    (StatusCode::OK, crate::std_resp(true, Some("OK"), json!(status))).into_response()
}

impl AppState {
    /// AutoSTUN 子服务状态（模块未加载时 `enabled=false`）。
    pub fn auto_stun_status(&self) -> Value {
        json!({
            "enabled": false,
            "reason": "auto-stun-port-forwarder module not loaded",
        })
    }
}