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

/// `POST /api/auth/login`：校验 token；校验通过返回 200 + 会话 cookie。
///
/// 上游为 Javalin 服务端会话（`sessionAttribute("authenticated", token)` +
/// JSESSIONID cookie 由浏览器自动携带）；WebUI 前端**没有任何手动凭据存储**
/// （无 localStorage/拦截器），登录后全靠 cookie 维持会话。Rust 版以无状态
/// cookie 等价实现：`PBH_SESSION=<token>`（HttpOnly，值即凭据，与 Bearer 同权），
/// 缺失会导致登录成功后所有 API 仍 401、WebUI 空白。
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
        let cookie = format!(
            "PBH_SESSION={}; Path=/; HttpOnly; SameSite=Lax; Max-Age=604800",
            expected.trim()
        );
        (
            StatusCode::OK,
            [("Set-Cookie", cookie.as_str())],
            crate::std_resp(true, Some("WEBAPI_AUTH_OK"), Value::Null),
        )
            .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            crate::std_resp(false, Some("WEBAPI_AUTH_INVALID_TOKEN"), Value::Null),
        )
            .into_response()
    }
}

/// `POST /api/auth/logout`：清除会话 cookie（对齐上游 `sessionAttribute(null)`）。
pub async fn logout() -> Response {
    let expired = "PBH_SESSION=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0";
    (
        StatusCode::OK,
        [("Set-Cookie", expired)],
        crate::std_resp(true, Some("success"), json!("OK")),
    )
        .into_response()
}
