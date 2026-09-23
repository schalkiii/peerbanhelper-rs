//! 后台任务端点（对齐 `PBHBackgroundTaskController`：SSE `GET /api/tasks/live`，Role.USER_READ）。
//!
//! 连接建立即推送当前任务全量快照（上游 `sendCurrentTasks`），之后随状态变化逐任务
//! 推送（上游 `broadcastTaskUpdate` 经 `backgroundTaskManager.addStatusListener`）。
//! 事件 JSON 与上游 `BackgroundTaskEvent` / `BackgroundTaskDTO` 同形：
//! `{type: "UPDATED", task: {id, title, statusText, status, barType, progress, current, max}}`，
//! `title` / `statusText` 按请求 locale 渲染（上游 `tl(lang, task.title)`）。
//!
//! 说明：本环境 axum 0.8 缺少 `sse` feature，SSE 用 `Body::from_stream` +
//! `text/event-stream` 手动实现（与 `/api/logs/live` 相同，协议等价）。

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::Response;
use pbh_core::i18n::Translator;
use std::convert::Infallible;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;

use crate::tasks::BackgroundTask;
use crate::AppState;

/// `GET /api/tasks/live`
pub async fn live(State(state): State<AppState>) -> Response {
    // 初始快照先于闭包消费 Arc/String（两个流各自持有一份渲染上下文）
    let initial_translator = state.translator.clone();
    let initial_locale = state.locale.clone();
    let initial = futures_util::stream::iter(
        state.tasks.task_list().into_iter().map(move |task| {
            Ok::<String, Infallible>(format!(
                "data: {}\n\n",
                task_event(&task, &initial_translator, &initial_locale)
            ))
        }),
    );
    let registry = state.tasks.clone();
    let live_translator = state.translator.clone();
    let live_locale = state.locale.clone();
    let live = BroadcastStream::new(registry.subscribe()).filter_map(move |result| match result {
        Ok(id) => registry.get_task(&id).map(|task| {
            Ok::<String, Infallible>(format!(
                "data: {}\n\n",
                task_event(&task, &live_translator, &live_locale)
            ))
        }),
        Err(_) => None, // 落后于广播缓冲被跳过
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

/// 上游 `BackgroundTaskEvent(UPDATED, BackgroundTaskDTO)` 的 JSON
/// （`title`/`statusText` 按 locale 渲染；`statusText` 可为 `null`）
pub fn task_event(
    task: &BackgroundTask,
    translator: &Translator,
    locale: &str,
) -> serde_json::Value {
    serde_json::json!({
        "type": "UPDATED",
        "task": {
            "id": task.id,
            "title": translator.render(&task.title, locale),
            "statusText": task.status_text.as_ref().map(|c| translator.render(c, locale)),
            "status": task.status.as_str(),
            "barType": task.bar_type.as_str(),
            "progress": task.progress(),
            "current": task.current,
            "max": task.max,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{
        BackgroundTaskBarType, BackgroundTaskRegistry, BackgroundTaskStatus, GeoIpTaskAdapter,
    };
    use pbh_core::geoip_update::GeoIpProgressStage;
    use std::sync::Arc;

    #[test]
    fn task_event_matches_upstream_dto_shape() {
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let adapter = Arc::new(GeoIpTaskAdapter::new(registry.clone()));
        let sink = adapter.sink();
        let url = "https://pbh-static.paulzzh.com/ipdb/GeoLite2-ASN.mmdb.xz";
        sink(pbh_core::geoip_update::GeoIpProgress {
            database: "GeoLite2-ASN".to_string(),
            target: "/tmp/GeoIP-ASN.mmdb".into(),
            stage: GeoIpProgressStage::DownloadStart,
            url: Some(url.to_string()),
        });
        sink(pbh_core::geoip_update::GeoIpProgress {
            database: "GeoLite2-ASN".to_string(),
            target: "/tmp/GeoIP-ASN.mmdb".into(),
            stage: GeoIpProgressStage::DownloadBytes {
                bytes: 4_000_000,
                total: Some(8_000_000),
            },
            url: Some(url.to_string()),
        });

        let task = registry.task_list().into_iter().next().unwrap();
        let event = task_event(&task, &Translator::embedded(), "en_us");
        assert_eq!(event["type"], "UPDATED");
        let dto = &event["task"];
        // 字段名与类型对齐上游 BackgroundTaskDTO（Gson/Jackson camelCase）
        assert_eq!(dto["id"], task.id.as_str());
        assert_eq!(dto["title"], "[GeoIPDB] Download database: {}");
        assert_eq!(dto["statusText"], format!("Download from remote server: {url}"));
        assert_eq!(dto["status"], "RUNNING");
        assert_eq!(dto["barType"], "DETERMINATE");
        assert_eq!(dto["progress"], 0.5);
        assert_eq!(dto["current"], 4_000_000);
        assert_eq!(dto["max"], 8_000_000);

        // statusText 缺省时为 null；状态字符串覆盖 BackgroundTaskStatus 全部枚举值
        let bare = BackgroundTask {
            id: "x".to_string(),
            title: pbh_core::i18n::TranslationComponent::new("X"),
            status_text: None,
            start_at_ms: 0,
            finished_at_ms: None,
            status: BackgroundTaskStatus::Preparing,
            bar_type: BackgroundTaskBarType::Hidden,
            max: 0,
            current: 0,
        };
        let event = task_event(&bare, &Translator::embedded(), "zh_cn");
        assert_eq!(event["task"]["statusText"], serde_json::Value::Null);
        assert_eq!(event["task"]["status"], "PREPARING");
        assert_eq!(event["task"]["barType"], "HIDDEN");
    }
}
