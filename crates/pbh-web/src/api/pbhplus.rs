//! PeerBanHelper Plus 状态端点（对齐上游 `PBHPlusController`）。
//!
//! 上游的 Plus 是付费许可体系；本移植没有许可校验，按用户要求**默认开启全部
//! 功能**：`enabledFeatures` 返回前端门控使用的功能名集合（`basic`/`paid`），
//! `licenses` 为空（无许可密钥）。设置页的 Plus 弹窗因此显示为已激活状态。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

use crate::AppState;

/// 前端门控使用的功能名（编译 JS 逆向证实）：`basic` 门控多数页面可用性、
/// `paid` 门控首页 Plus 按钮。
pub const ENABLED_FEATURES: &[&str] = &["basic", "paid"];

/// `GET /api/pbhplus/status`：Plus 订阅状态（默认全部功能开启）。
pub async fn status(State(_state): State<AppState>) -> Response {
    let data = json!({
        "enabledFeatures": ENABLED_FEATURES,
        "licenses": [],
    });
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `PUT /api/pbhplus/key`：添加许可密钥（Rust 版无许可校验，直接确认）。
pub async fn put_key(State(_state): State<AppState>, Json(_body): Json<Value>) -> Response {
    (
        StatusCode::OK,
        crate::std_resp(true, Some("OK"), json!({"activated": true})),
    )
        .into_response()
}

/// `DELETE /api/pbhplus/key`：移除许可密钥（无实际效果——功能默认开启）。
pub async fn delete_key(State(_state): State<AppState>) -> Response {
    (
        StatusCode::OK,
        crate::std_resp(true, Some("OK"), Value::Null),
    )
        .into_response()
}
