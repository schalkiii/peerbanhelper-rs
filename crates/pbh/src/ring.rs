//! tracing → Web 日志环形缓冲桥接（`/api/logs` 历史与 SSE live 共用同一份数据）。

use pbh_web::{LogEntry, RingLog};
use std::sync::Arc;

/// tracing Layer：每个事件写入环形缓冲（对齐上游 `LogAwareStream` 的 `handle`）。
pub struct RingLayer {
    ring: Arc<RingLog>,
}

impl RingLayer {
    pub fn new(ring: Arc<RingLog>) -> Self {
        Self { ring }
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for RingLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut msg = String::new();
        let mut extra = String::new();
        event.record(&mut RingVisitor {
            msg: &mut msg,
            extra: &mut extra,
        });
        let content = if extra.is_empty() {
            msg
        } else {
            format!("{msg} {extra}")
        };
        let level = event.metadata().level().as_str().to_uppercase();
        let time = chrono::Utc::now().timestamp_millis();
        let thread = std::thread::current()
            .name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "main".to_string());
        self.ring.push(LogEntry {
            time,
            thread,
            level,
            content,
            seq: self.ring.next_seq(),
        });
    }
}

/// tracing field 访问器：`message` 进正文，其余字段拼接为附加信息。
struct RingVisitor<'a> {
    msg: &'a mut String,
    extra: &'a mut String,
}

impl<'a> tracing::field::Visit for RingVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if field.name() == "message" {
            *self.msg = format!("{value:?}");
        } else if !field.name().starts_with("log.") && field.name() != "target" {
            let _ = write!(self.extra, "{}={:?} ", field.name(), value);
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        use std::fmt::Write;
        if field.name() == "message" {
            *self.msg = value.to_string();
        } else if !field.name().starts_with("log.") && field.name() != "target" {
            let _ = write!(self.extra, "{}={value} ", field.name());
        }
    }
}