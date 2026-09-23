//! Web 后端抽象：把「主进程才能回答的问题」（配置读写、下载器管理、封禁调度、
//! 推送渠道、规则订阅、指标清单等）收敛为一个 trait，由 `pbh` 二进制提供实现，
//! `pbh-web` 只需面向该 trait 编程，避免依赖倒置。
//!
//! 同时提供 `RingLog`：供 `/api/logs/history` 与 SSE `/api/logs/live` 使用的
//! 内存环形日志缓冲（对齐上游 `logger` 的 `ringDeque` + `PushStream`）。
//!
//! 下载器实时数据（`/torrents` `/peers`）通过 `pbh_downloader::Downloader` 的
//! boxed-future 接口在 handler 内直接 await，无需 async trait。

use pbh_downloader::Downloader;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

/// 一条 WebUI 日志（对齐 `WebUILogEntryDTO`：`timestamp, thread, level, content, seq`；
/// 前端 `Log` 模型读 `content/offset/thread/time/level`，服务端输出两者可兼容的字段）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct LogEntry {
    pub time: i64,
    pub thread: String,
    pub level: String,
    pub content: String,
    pub seq: u64,
}

impl LogEntry {
    pub fn to_web(&self) -> Value {
        json!({
            "time": self.time,
            "thread": self.thread,
            "level": self.level,
            "content": self.content,
            "seq": self.seq,
        })
    }
}

/// 内存环形日志缓冲 + broadcast 订阅（对齐 `logger` 的 `ringDeque` 与 `stream`）。
pub struct RingLog {
    capacity: usize,
    entries: Mutex<VecDeque<LogEntry>>,
    seq: AtomicU64,
    tx: broadcast::Sender<LogEntry>,
}

impl RingLog {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(256);
        Self {
            capacity,
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            seq: AtomicU64::new(0),
            tx,
        }
    }

    /// 追加一条日志；超过容量时丢弃最老的。
    pub fn push(&self, entry: LogEntry) {
        {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries.push_back(entry.clone());
            while entries.len() > self.capacity {
                entries.pop_front();
            }
        }
        // 广播失败仅代表当前无订阅者，忽略
        let _ = self.tx.send(entry);
    }

    /// 当前全量快照（按时间正序）。
    pub fn snapshot(&self) -> Vec<LogEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// 订阅后续新日志。
    pub fn subscribe(&self) -> broadcast::Receiver<LogEntry> {
        self.tx.subscribe()
    }

    /// 取下一个序号并自增。
    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }
}

/// 模块记录（`/api/metadata/manifest` 的 `modules[]`，对齐 `ModuleRecordDTO`）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct ModuleRecord {
    pub class_name: String,
    pub config_name: String,
}

/// `reload` 的结果行（对齐上游 `ReloadResult`：`moduleName + results[]`）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct ReloadEntry {
    pub module_name: String,
    pub results: Vec<String>,
}

/// 规则订阅模块句柄（`/api/sub/*` 端点；未启用时为 `None`）。
pub trait SubModule: Send + Sync {
    /// 订阅规则列表（`[{id, name, url, enabled, ...}]`）。
    fn list_rules(&self) -> Vec<Value>;
    /// 新增规则（body 与上游 `RuleSubscribe` 实体同形）。
    fn add_rule(&self, rule: &Value) -> Result<(), String>;
    /// 更新规则。
    fn update_rule(&self, id: &str, rule: &Value) -> Result<(), String>;
    /// 删除规则。
    fn remove_rule(&self, id: &str) -> Result<(), String>;
    /// 刷新单条（`id=None` 时刷新全部）并返回刷新结果摘要。
    fn refresh_rule(&self, id: Option<&str>) -> String;
    /// 刷新日志（`(total, [entries])`）。
    fn logs(&self, page: i64, size: i64) -> (i64, Vec<Value>);
    /// 当前轮询间隔（毫秒）。
    fn interval(&self) -> i64;
    /// 设置轮询间隔。
    fn set_interval(&self, interval_ms: i64) -> Result<(), String>;
}

/// 运行版本（对齐 `BuildMeta`）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct BuildMeta {
    pub version: String,
    pub os: String,
    pub branch: String,
    pub commit: String,
    pub abbrev: String,
    pub compile_time: i64,
}

/// Web 后端能力接口。
///
/// 所有方法均为同步：主进程内部已有自己的 Runtime 或阻塞线程，这里不引入异步边界，
/// 便于 `axum` handler 与测试直接调用。下载器实时数据（torrents/peers）由
/// `downloaders_async` 提供异步句柄（`Downloader` trait 本身就是 boxed future 风格）。
pub trait WebBackend: Send + Sync {
    // ---- metadata / general ----
    /// 安装 ID（首次启动生成并持久化）。
    fn installation_id(&self) -> String;
    /// 匿名统计开关。
    fn analytics_enabled(&self) -> bool;
    /// 当前启用模块清单（manifest + checkModuleAvailable）。
    fn modules(&self) -> Vec<ModuleRecord>;
    /// 全局暂停状态。
    fn global_paused(&self) -> bool;
    /// 设置全局暂停。
    fn set_global_paused(&self, paused: bool) -> Result<(), String>;
    /// 匿名统计开关持久化。
    fn set_analytics(&self, enabled: bool) -> Result<(), String>;
    /// 重新加载配置；返回每个模块的重载结果。
    fn reload(&self) -> Vec<ReloadEntry>;
    /// 读取 config.yml / profile.yml 等配置文件的 YAML 解析结果。
    fn read_config(&self, name: &str) -> Result<Value, String>;
    /// 写回 config.yml / profile.yml（整文件覆盖式合并语义由主进程处理）。
    fn write_config(&self, name: &str, data: &Value) -> Result<(), String>;

    // —— 手动封禁 / 解封（`PUT/DELETE /api/bans`）--
    /// 把 peer 加入封禁清单（调度器在下一轮 wave 应用）。
    fn ban_peers(&self, ips: &[String]) -> Result<(), String>;
    /// 从封禁清单移除（`*` 表示清空）；返回实际解封条数。
    fn unban_peers(&self, ips: &[String]) -> Result<usize, String>;

    // —— 下载器管理 ----
    /// 下载器列表（含运行时状态，供 `/api/downloaders`）。
    fn downloaders(&self) -> Vec<Value>;
    /// 按 id 取下载器 live 句柄（供 `/torrents` `/peers` 实时查询）。
    fn downloader(&self, id: &str) -> Option<Arc<dyn Downloader>>;
    /// 创建下载器（写入配置并热加载）。
    fn add_downloader(&self, config: &Value) -> Result<(), String>;
    /// 更新下载器（id 不变则热更新）。
    fn update_downloader(&self, id: &str, config: &Value) -> Result<(), String>;
    /// 删除下载器。
    fn remove_downloader(&self, id: &str) -> Result<(), String>;
    /// 测试下载器配置（仅校验，不保存）。
    fn test_downloader(&self, config: &Value) -> Result<(), String>;

    // —— 推送渠道 ----
    /// 推送渠道列表（`/api/push`）。
    fn push_channels(&self) -> Vec<Value>;
    /// 新建推送渠道。
    fn add_push_channel(&self, channel: &Value) -> Result<(), String>;
    /// 更新推送渠道（按 name 定位）。
    fn update_push_channel(&self, name: &str, channel: &Value) -> Result<(), String>;
    /// 删除推送渠道。
    fn remove_push_channel(&self, name: &str) -> Result<(), String>;
    /// 测试推送渠道（仅构造并发送测试消息，不保存）。
    fn test_push_channel(&self, channel: &Value) -> Result<(), String>;
}
