//! 认证端点（对齐上游 `PBHAuthenticateController`）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::AppState;

#[derive(Deserialize)]
pub struct LoginBody {
    #[serde(default)]
    token: Option<String>,
}

/// `POST /api/auth/login`：校验 token；校验通过返回 200，失败返回 401。
///
/// 与上游的差异（未移植）：token 为空时上游抛出 `NeedInitException` 让前端跳转
/// `/init` 初始化向导页面；Rust 版默认总会生成 token，此处同样以 303 提示前端走初始化。
pub async fn login(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
    body: Option<Json<LoginBody>>,
) -> Response {
    let expected = state.token.lock().map(|t| t.clone()).unwrap_or_default();
    if expected.trim().is_empty() {
        return (StatusCode::SEE_OTHER, [("Location", "/init")]).into_response();
    }
    let given = body
        .and_then(|b| b.0.token)
        .or_else(|| query.get("token").cloned())
        .unwrap_or_default();
    if given.trim() == expected.trim() {
        crate::std_resp(true, Some("WEBAPI_AUTH_OK"), Value::Null).into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            crate::std_resp(false, Some("WEBAPI_AUTH_INVALID_TOKEN"), Value::Null),
        )
            .into_response()
    }
}

/// `POST /api/auth/logout`。
pub async fn logout() -> Response {
    crate::std_resp(true, Some("success"), json!("OK")).into_response()
}