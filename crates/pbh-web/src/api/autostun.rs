//! AutoSTUN 的 WebUI 管理端点（`/api/autostun/*`，对齐 WebUI `service/autostun.ts`）。
//!
//! 核心 NAT 翻译 / TCP 隧道 / NAT 类型探测已实现（`pbh_core::auto_stun`，
//! 由 `ip-remapping.auto-stun.enabled` 门控），但**这组管理端点尚未接线**：
//! Web 层不持有运行中的 AutoSTUN 实例，启停/刷新也不会写回配置。
//!
//! 端点必须存在——缺失会让前端 AutoSTUN 页面拿到 `/api` fallback 的
//! `API_NOT_FOUND` 而整体不可用。因此这里返回**结构完整、语义诚实**的响应：
//! - 读操作给出「未启用」状态与空集合（页面正常渲染为未启用，而非报错/空白）；
//! - 写操作（启用/刷新/重启）明确返回失败并说明原因，而不是假装生效。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::AppState;

const NOT_SUPPORTED: &str = "AUTOSTUN_NOT_SUPPORTED";

/// 未支持的操作：返回失败但**不是** `API_NOT_FOUND`（端点确实存在）。
fn unsupported() -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        crate::std_resp(false, Some(NOT_SUPPORTED), Value::Null),
    )
        .into_response()
}

/// `GET /api/autostun/status`：AutoSTUN 状态。
///
/// 字段对齐前端 `AutoSTUNStatus`（`enabled` / `useFriendlyLoopbackMapping` /
/// `selectedDownloaders` / `natType` / `isBridgeNetDriver`）。
pub async fn status(State(_state): State<AppState>) -> Response {
    let data = json!({
        "enabled": false,
        "useFriendlyLoopbackMapping": false,
        "selectedDownloaders": [],
        "natType": "Unknown",
        "isBridgeNetDriver": false,
    });
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `PUT /api/autostun/config`：保存 AutoSTUN 配置（未实现 → 明确失败）。
pub async fn config(State(_state): State<AppState>, Json(_body): Json<Value>) -> Response {
    unsupported()
}

/// `GET /api/autostun/tunnels`：隧道列表（未启用 → 空数组）。
pub async fn tunnels(State(_state): State<AppState>) -> Response {
    (
        StatusCode::OK,
        crate::std_resp(true, None, json!(Vec::<Value>::new())),
    )
        .into_response()
}

/// `GET /api/autostun/tunnel/{id}/info`：单隧道信息。
///
/// Web 侧 AutoSTUN 未接线（见模块头注释），按上游「隧道不存在」分支返回
/// `success=false`，而不是伪造一份全零 TunnelInfoDTO。
pub async fn tunnel_info(Path(_id): Path<String>) -> Response {
    (
        StatusCode::OK,
        crate::std_resp(false, Some(NOT_SUPPORTED), Value::Null),
    )
        .into_response()
}

/// `GET /api/autostun/tunnel/{id}/connections`：隧道连接（未启用 → 空分页）。
pub async fn connections(
    State(_state): State<AppState>,
    Path(_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (page, size) = crate::api::pagination(&params);
    (
        StatusCode::OK,
        crate::std_resp(
            true,
            None,
            json!({ "page": page, "size": size, "total": 0, "results": [] }),
        ),
    )
        .into_response()
}

/// `POST /api/autostun/refreshNatType`：重新探测 NAT 类型（未实现 → 明确失败）。
pub async fn refresh_nat_type(State(_state): State<AppState>) -> Response {
    unsupported()
}

/// `POST /api/autostun/restart`：重启 AutoSTUN（未实现 → 明确失败）。
pub async fn restart(State(_state): State<AppState>) -> Response {
    unsupported()
}
