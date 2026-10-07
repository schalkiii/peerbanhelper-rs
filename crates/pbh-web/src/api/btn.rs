//! 第三方模块状态端点（`/api/modules/btn` 与 AutoSTUN 状态）。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::AppState;

/// `GET /api/modules/btn`：BTN 模块状态（对齐上游 `PBHBtnController.status`）。
///
/// 前端设置页按 `enabled` / `configSuccess` / `abilities` 渲染 BTN 页面；
/// 之前是硬编码的「未内置」占位，导致 BTN 实际运行时页面仍显示未启用。
pub async fn status(State(state): State<AppState>) -> Response {
    let Some(network) = state.btn_network.get() else {
        // 未启用：对齐上游「btnNetwork == null」分支（StdResp 的 success=false）
        let data = json!({
            "enabled": false,
            "configSuccess": false,
            "appId": "N/A",
            "appSecret": "N/A",
            "abilities": [],
            "configUrl": "BTN_NOT_ENABLE_AND_REQUIRE_RESTART",
        });
        return (
            StatusCode::OK,
            crate::std_resp(false, Some("BTN_NOT_ENABLE_AND_REQUIRE_RESTART"), data),
        )
            .into_response();
    };
    let abilities: Vec<Value> = network
        .abilities()
        .iter()
        .map(|a| {
            json!({
                // Rust 的 ability 无独立 display 字段：kind 调试名 + endpoint 兼作描述
                "name": format!("{:?}", a.kind),
                "displayName": format!("{:?}", a.kind),
                "description": a.endpoint,
                "lastSuccess": a.last_status,
                "lastMessage": if a.last_status { "OK" } else { "FAILED" },
                "lastUpdateAt": a.last_status_at_ms,
            })
        })
        .collect();
    let config = network.config();
    let config_result = match network.config_result() {
        pbh_core::btn_transport::BtnConfigStatus::Pending => "PENDING".to_string(),
        pbh_core::btn_transport::BtnConfigStatus::Success { .. } => "SUCCESS".to_string(),
        pbh_core::btn_transport::BtnConfigStatus::HttpError { status, .. } => {
            format!("HTTP {status}")
        }
        pbh_core::btn_transport::BtnConfigStatus::ClientTooOld { implemented, min } => {
            format!("CLIENT_TOO_OLD ({implemented} < {min})")
        }
        pbh_core::btn_transport::BtnConfigStatus::ServerTooOld { implemented, max } => {
            format!("SERVER_TOO_OLD ({implemented} > {max})")
        }
        pbh_core::btn_transport::BtnConfigStatus::Exception { .. } => "EXCEPTION".to_string(),
    };
    let app_secret = if config.app_secret.len() > 5 {
        format!("{}*******", &config.app_secret[..5])
    } else {
        config.app_secret.clone()
    };
    let data = json!({
        "enabled": true,
        "configSuccess": network.config_success(),
        "configResult": config_result,
        "abilities": abilities,
        "appId": config.app_id,
        "appSecret": app_secret,
        "configUrl": config.config_url,
    });
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `GET /api/modules/auto-stun-port-forwarding/status`：AutoSTUN 状态。
pub async fn auto_stun_status(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let status = state.auto_stun_status();
    let _ = params;
    (
        StatusCode::OK,
        crate::std_resp(true, Some("OK"), json!(status)),
    )
        .into_response()
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
