//! 自定义脚本端点（`/api/expression-engine/*`，对齐 WebUI `service/script.ts`）。
//!
//! WebUI 的 custom-script 页面依赖以下契约，**缺端点会落到 `/api` 的 JSON 404**
//! （页面表现为「API_NOT_FOUND」且列表为空）：
//! - `GET  /scripts?page&pageSize` → `CommonResponseWithPage<Script>`，即
//!   `{page, size, results, total}`（`results` 必须是数组）；
//! - `GET  /editable` → `CommonResponse<{editable, reason}>`；
//! - `GET  /{id}` → `CommonResponse<string>`（脚本源码，纯文本）；
//! - `PUT  /{id}` → body 为脚本源码文本（**非 JSON**）；
//! - `DELETE /{id}` → `CommonResponseWithoutData`。

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path as StdPath, PathBuf};

use crate::AppState;

fn disabled() -> Response {
    (
        StatusCode::NOT_FOUND,
        crate::std_resp(false, Some("EXPRESSION_ENGINE_DISABLED"), Value::Null),
    )
        .into_response()
}

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        crate::std_resp(false, Some(message), Value::Null),
    )
        .into_response()
}

/// 脚本文件名（含 `.av`）→ 目录内的文件路径，**拒绝任何路径穿越写法**。
///
/// 只接受不含分隔符的纯文件名，且必须以 `.av` 结尾——既防止 `../` 逃逸，
/// 也避免名为 `editable` 的脚本与静态路由撞名造成歧义。
fn script_path(dir: &StdPath, id: &str) -> Option<PathBuf> {
    if id.is_empty() || !id.ends_with(".av") {
        return None;
    }
    if id.contains('/') || id.contains('\\') || id.contains('\0') {
        return None;
    }
    let stem = &id[..id.len() - 3];
    if stem.is_empty() || stem == "." || stem == ".." || stem.contains("..") {
        return None;
    }
    Some(dir.join(id))
}

/// 列出目录下的 `.av` 脚本（按文件名排序，保证分页稳定）。
fn collect_scripts(dir: &StdPath) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".av"))
        .collect();
    names.sort();
    names
}

/// `GET /api/expression-engine/scripts`：分页脚本列表。
pub async fn list(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(dir) = state.scripts_dir.as_ref() else {
        return disabled();
    };
    let page = params
        .get("page")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(1)
        .max(1);
    let size = params
        .get("pageSize")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(10)
        .clamp(1, 200);
    let all = collect_scripts(dir);
    let total = all.len() as i64;
    // 上游 `Pageable` 的 page 是 1-based，`getZeroBasedPage() = page - 1`
    let start = ((page - 1) * size) as usize;
    let results: Vec<Value> = all
        .iter()
        .skip(start)
        .take(size as usize)
        .map(|name| {
            json!({
                "id": name,
                "name": name.strip_suffix(".av").unwrap_or(name).to_string(),
                "author": Value::Null,
                "version": "1.0",
                "cacheable": false,
                "threadSafe": false,
            })
        })
        .collect();
    (
        StatusCode::OK,
        crate::std_resp(
            true,
            None,
            json!({ "page": page, "size": size, "total": total, "results": results }),
        ),
    )
        .into_response()
}

/// `GET /api/expression-engine/editable`：脚本是否可编辑（目录可用且可写）。
pub async fn editable(State(state): State<AppState>) -> Response {
    let Some(dir) = state.scripts_dir.as_ref() else {
        return disabled();
    };
    // 目录不存在时先尝试创建：否则新建脚本必然失败，界面会一直是只读模式
    if !dir.exists() && std::fs::create_dir_all(dir).is_err() {
        return (
            StatusCode::OK,
            crate::std_resp(
                true,
                None,
                json!({ "editable": false, "reason": "scripts-dir-unavailable" }),
            ),
        )
            .into_response();
    }
    let probe = dir.join(".pbh-write-probe");
    let writable = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    (
        StatusCode::OK,
        crate::std_resp(
            true,
            None,
            json!({
                "editable": writable,
                "reason": if writable { "" } else { "read-only" },
            }),
        ),
    )
        .into_response()
}

/// `GET /api/expression-engine/{id}`：读取脚本源码。
pub async fn content(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(dir) = state.scripts_dir.as_ref() else {
        return disabled();
    };
    let Some(path) = script_path(dir, &id) else {
        return bad_request("INVALID_SCRIPT_ID");
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => (StatusCode::OK, crate::std_resp(true, None, json!(text))).into_response(),
        Err(_) => bad_request("SCRIPT_NOT_FOUND"),
    }
}

/// `PUT /api/expression-engine/{id}`：写入脚本源码（body 为纯文本）。
pub async fn upsert(State(state): State<AppState>, Path(id): Path<String>, body: Bytes) -> Response {
    let Some(dir) = state.scripts_dir.as_ref() else {
        return disabled();
    };
    let Some(path) = script_path(dir, &id) else {
        return bad_request("INVALID_SCRIPT_ID");
    };
    if !dir.exists() && std::fs::create_dir_all(dir).is_err() {
        return bad_request("SCRIPTS_DIR_UNAVAILABLE");
    }
    match std::fs::write(&path, &body) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), Value::Null),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}

/// `DELETE /api/expression-engine/{id}`：删除脚本。
pub async fn remove(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(dir) = state.scripts_dir.as_ref() else {
        return disabled();
    };
    let Some(path) = script_path(dir, &id) else {
        return bad_request("INVALID_SCRIPT_ID");
    };
    match std::fs::remove_file(&path) {
        Ok(()) => (
            StatusCode::OK,
            crate::std_resp(true, Some("OK"), Value::Null),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            crate::std_resp(false, Some(&e.to_string()), Value::Null),
        )
            .into_response(),
    }
}
