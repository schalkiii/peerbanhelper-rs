//! 规则订阅端点（对齐 `RuleSubController`）。
//!
//! 只有 `ip-blacklist-rules` 模块启用时由 `api/mod.rs` 挂载到 `/api/sub/*`
//! （对齐上游 `RuleSubController.onEnable` 的注册条件）；未启用时这些路由不存在，
//! 由 axum 返回 404。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::AppState;

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, crate::std_resp(false, Some("RULE_SUB_MODULE_DISABLED"), Value::Null)).into_response()
}

/// `GET /api/sub/`：订阅规则列表。
pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(module) = state.sub_module.as_ref() else {
        return not_found();
    };
    let rules = module.list_rules();
    let data = json!({ "rules": rules, "count": rules.len() });
    let _ = params;
    (StatusCode::OK, crate::std_resp(true, Some("OK"), data)).into_response()
}

/// `PUT /api/sub/rule`：新增规则（body 为规则 JSON）。
pub async fn add_rule(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(module) = state.sub_module.as_ref() else {
        return not_found();
    };
    match module.add_rule(&body) {
        Ok(()) => (StatusCode::OK, crate::std_resp(true, Some("OK"), json!({"success": true}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `PATCH /api/sub/rule/{id}`：更新规则。
pub async fn update_rule(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(module) = state.sub_module.as_ref() else {
        return not_found();
    };
    match module.update_rule(&id, &body) {
        Ok(()) => (StatusCode::OK, crate::std_resp(true, Some("OK"), json!({"success": true}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `DELETE /api/sub/rule/{id}`：删除规则。
pub async fn remove_rule(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(module) = state.sub_module.as_ref() else {
        return not_found();
    };
    match module.remove_rule(&id) {
        Ok(()) => (StatusCode::OK, crate::std_resp(true, Some("OK"), json!({"success": true}))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}

/// `POST /api/sub/rule/{id}/update`：立即刷新单条规则。
pub async fn refresh_rule(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(module) = state.sub_module.as_ref() else {
        return not_found();
    };
    let result = module.refresh_rule(Some(&id));
    (StatusCode::OK, crate::std_resp(true, Some("OK"), json!({"success": true, "result": result}))).into_response()
}

/// `POST /api/sub/rules/update`：刷新全部规则。
pub async fn refresh_all(State(state): State<AppState>) -> Response {
    let Some(module) = state.sub_module.as_ref() else {
        return not_found();
    };
    let result = module.refresh_rule(None);
    (StatusCode::OK, crate::std_resp(true, Some("OK"), json!({"success": true, "result": result}))).into_response()
}

/// `GET /api/sub/logs?page&pageSize`：规则内嵌日志。
pub async fn logs(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(module) = state.sub_module.as_ref() else {
        return not_found();
    };
    let page = params.get("page").and_then(|v| v.parse::<i64>().ok()).unwrap_or(1).max(1);
    let size = params
        .get("pageSize")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(1, 200);
    let (total, entries) = module.logs(page, size);
    (
        StatusCode::OK,
        crate::std_resp(
            true,
            Some("OK"),
            json!({ "page": page, "size": size, "total": total, "results": entries }),
        ),
    )
        .into_response()
}

/// `GET /api/sub/interval`：轮询间隔（毫秒）。
pub async fn interval_get(State(state): State<AppState>) -> Response {
    let Some(module) = state.sub_module.as_ref() else {
        return not_found();
    };
    let interval = module.interval();
    (StatusCode::OK, crate::std_resp(true, Some("OK"), json!({ "interval": interval }))).into_response()
}

/// `PATCH /api/sub/interval`：设置轮询间隔（body: `{interval: 毫秒}`）。
pub async fn interval_patch(State(state): State<AppState>, Json(body): Json<Value>) -> Response {
    let Some(module) = state.sub_module.as_ref() else {
        return not_found();
    };
    let interval = body.get("interval").and_then(|v| v.as_i64()).unwrap_or_default();
    match module.set_interval(interval) {
        Ok(()) => (StatusCode::OK, crate::std_resp(true, Some("OK"), json!({ "interval": interval }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e), Value::Null),
        )
            .into_response(),
    }
}