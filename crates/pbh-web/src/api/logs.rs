//! 日志端点（对齐 `PBHLogsController`：`/api/logs/history` + SSE `/api/logs/live`）。
//!
//! 说明：本环境 axum 0.8 缺少 `sse` feature（镜像包未启用），SSE 用
//! `Body::from_stream` + `text/event-stream` 响应头手动实现，协议等价。

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use std::convert::Infallible;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::AppState;

/// `GET /api/logs/history`：返回环形缓冲内全部日志（直接数组，非分页）。
pub async fn history(State(state): State<AppState>) -> Response {
    let entries = state.log_ring.snapshot();
    let data = serde_json::Value::Array(entries.iter().map(|e| e.to_web()).collect());
    (StatusCode::OK, crate::std_resp(true, None, data)).into_response()
}

/// `GET /api/logs/live`：SSE 流——先回放历史缓冲，再持续推送新日志。
///
/// 对齐上游 `handleLive`：连接建立后立即把 `ringDeque` 中的存量日志推送一遍，
/// 之后 `logger.pushStream()` 的新条目按 `WebUILogEntryDTO` JSON 逐条下发。
pub async fn live(State(state): State<AppState>) -> Response {
    let initial = futures_util::stream::iter(state.log_ring.snapshot().into_iter().map(|e| {
        Ok::<String, Infallible>(format!("data: {}\n\n", e.to_web()))
    }));
    let rx = state.log_ring.subscribe();
    let live = BroadcastStream::new(rx).filter_map(|result| match result {
        Ok(entry) => Some(Ok::<String, Infallible>(format!("data: {}\n\n", entry.to_web()))),
        Err(_) => None, // 落后于缓冲被跳过，与上游 PushStream 丢日志语义一致
    });
    let body = Body::from_stream(initial.chain(live));
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("X-Accel-Buffering", "no")
        .body(body)
        .unwrap()
}