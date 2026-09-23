//! 后台任务注册表（对齐上游 `util/backgroundtask/BackgroundTaskManager` + `BackgroundTask`）
//! 与 WebUI 消费端（对齐 `PBHBackgroundTaskController` 的 SSE `GET /api/tasks/live`，
//! Role.USER_READ；DTO 同 `BackgroundTaskDTO`）。
//!
//! 上游 `BackgroundTaskManager`：任务列表 `CopyOnWriteArrayList`、`addTaskAsync`
//! （QUEUED → RUNNING → `start` → `complete`/`fail`，状态变化通知监听器）、每 5 秒
//! `cleanTask`（COMPLETED 保留 1 分钟、其余终态保留 30 分钟）。本移植没有任务执行器
//! （任务执行体是 GeoIP 更新线程等调用方），注册表只承接生命周期登记、快照读取与
//! 更新广播；清理改为快照时惰性执行（唯一观察者是 [`BackgroundTaskRegistry::task_list`]
//! 与 SSE 端点，对外行为等价）。
//!
//! [`GeoIpTaskAdapter`] 把 GeoIP 更新器的 [`GeoIpProgress`] 事件流映射成注册表里的
//! 下载任务，逐字段对齐上游 `IPDB#updateMMDB` 的
//! `new FunctionalBackgroundTask(new TranslationComponent(Lang.IPDB_DOWNLOAD_MMDB), ...)`。

use pbh_core::geoip_update::{
    GeoIpProgress, GeoIpProgressStage, LANG_IPDB_DOWNLOAD_MMDB,
    LANG_IPDB_DOWNLOAD_MMDB_DESCRIPTION,
};
use pbh_core::i18n::TranslationComponent;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::broadcast;

/// 对齐上游 `BackgroundTaskStatus`（`active` 标志同上游枚举构造参数）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundTaskStatus {
    Queued,
    Preparing,
    Running,
    Completed,
    Failed,
    Paused,
    Cancelled,
}

impl BackgroundTaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Preparing => "PREPARING",
            Self::Running => "RUNNING",
            Self::Completed => "COMPLETED",
            Self::Failed => "FAILED",
            Self::Paused => "PAUSED",
            Self::Cancelled => "CANCELLED",
        }
    }

    /// 对齐上游 `BackgroundTaskStatus.isActive()`
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Preparing | Self::Running | Self::Paused)
    }
}

/// 对齐上游 `BackgroundTaskProgressBarType`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundTaskBarType {
    Hidden,
    Determinate,
    Indeterminate,
}

impl BackgroundTaskBarType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hidden => "HIDDEN",
            Self::Determinate => "DETERMINATE",
            Self::Indeterminate => "INDETERMINATE",
        }
    }
}

/// 一条后台任务（对齐上游 `BackgroundTask`；`title`/`statusText` 是未渲染的
/// `TranslationComponent`，由端点按请求 locale 渲染——上游 `tl(lang, task.title)`）
#[derive(Clone, Debug)]
pub struct BackgroundTask {
    pub id: String,
    pub title: TranslationComponent,
    pub status_text: Option<TranslationComponent>,
    pub start_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    pub status: BackgroundTaskStatus,
    pub bar_type: BackgroundTaskBarType,
    pub max: i64,
    pub current: i64,
}

impl BackgroundTask {
    /// 对齐上游 `getProgress()`：`max == 0` 时为 0.0
    pub fn progress(&self) -> f64 {
        if self.max == 0 {
            0.0
        } else {
            self.current as f64 / self.max as f64
        }
    }

    /// 对齐上游 `complete()`
    fn complete(&mut self) {
        self.finished_at_ms = Some(now_ms());
        if matches!(
            self.status,
            BackgroundTaskStatus::Queued | BackgroundTaskStatus::Running
        ) {
            self.status = BackgroundTaskStatus::Completed;
        }
    }

    /// 对齐上游 `fail()`
    fn fail(&mut self) {
        self.finished_at_ms = Some(now_ms());
        self.status = BackgroundTaskStatus::Failed;
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 生成任务 id（对齐上游 `UUID.randomUUID().toString()` 的 8-4-4-4-12 十六进制形态）
fn new_task_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let bytes: [u8; 16] = rng.gen();
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 后台任务注册表（上游 `BackgroundTaskManager` 的最小对应物）。
///
/// 更新广播用 `tokio::sync::broadcast`（SSE 端点订阅）；与上游状态监听器的差异是
/// 只广播任务 id，DTO 由端点按各自客户端的 locale 现场渲染（上游
/// `broadcastTaskUpdate` 同样是按客户端 locale 逐个渲染）。
pub struct BackgroundTaskRegistry {
    tasks: StdMutex<Vec<BackgroundTask>>,
    tx: broadcast::Sender<String>,
}

impl Default for BackgroundTaskRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BackgroundTaskRegistry {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            tasks: StdMutex::new(Vec::new()),
            tx,
        }
    }

    /// 对齐 `addTaskAsync` 的注册部分：QUEUED 入列并通知监听器（执行体由调用方驱动）
    pub fn add_task(&self, title: TranslationComponent) -> String {
        let id = new_task_id();
        let task = BackgroundTask {
            id: id.clone(),
            title,
            status_text: None,
            start_at_ms: now_ms(),
            finished_at_ms: None,
            status: BackgroundTaskStatus::Queued,
            bar_type: BackgroundTaskBarType::Indeterminate,
            max: 0,
            current: 0,
        };
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.push(task);
        }
        self.notify(&id);
        id
    }

    /// 对齐 `addTaskAsync` 开始执行时的 `status = RUNNING`
    pub fn mark_running(&self, id: &str) {
        self.update(id, |task| task.status = BackgroundTaskStatus::Running);
    }

    /// 对齐 `bgTask.setStatusText(...)`
    pub fn set_status_text(&self, id: &str, status_text: Option<TranslationComponent>) {
        self.update(id, |task| task.status_text = status_text);
    }

    /// 对齐 `bgTask.setBarType(...)`
    pub fn set_bar_type(&self, id: &str, bar_type: BackgroundTaskBarType) {
        self.update(id, |task| task.bar_type = bar_type);
    }

    /// 对齐 `bgTask.setMax(...)` + `bgTask.setCurrent(...)`
    pub fn set_progress(&self, id: &str, max: i64, current: i64) {
        self.update(id, |task| {
            task.max = max;
            task.current = current;
        });
    }

    /// 对齐 `complete()`
    pub fn complete_task(&self, id: &str) {
        self.update(id, BackgroundTask::complete);
    }

    /// 对齐 `fail()`
    pub fn fail_task(&self, id: &str) {
        self.update(id, BackgroundTask::fail);
    }

    /// 对齐 `getTaskList()`：活跃优先 → COMPLETED/FAILED 优先 → startAt 倒序
    pub fn task_list(&self) -> Vec<BackgroundTask> {
        self.retain_fresh();
        let mut list = self.tasks.lock().unwrap_or_else(|e| e.into_inner()).clone();
        list.sort_by(|a, b| {
            match (a.status.is_active(), b.status.is_active()) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => {
                    let a_done = matches!(
                        a.status,
                        BackgroundTaskStatus::Completed | BackgroundTaskStatus::Failed
                    );
                    let b_done = matches!(
                        b.status,
                        BackgroundTaskStatus::Completed | BackgroundTaskStatus::Failed
                    );
                    match (a_done, b_done) {
                        (true, false) => std::cmp::Ordering::Less,
                        (false, true) => std::cmp::Ordering::Greater,
                        _ => b.start_at_ms.cmp(&a.start_at_ms),
                    }
                }
            }
        });
        list
    }

    /// 按 id 取单个任务（SSE 增量推送用）
    pub fn get_task(&self, id: &str) -> Option<BackgroundTask> {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|task| task.id == id)
            .cloned()
    }

    /// 订阅更新（值为变更任务的 id）
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.tx.subscribe()
    }

    /// 对齐 `cleanTask()`（上游每 5 秒；这里在快照时惰性执行）：
    /// COMPLETED 保留 1 分钟、其余终态保留 30 分钟，活跃任务永不清理。
    /// 上游的 `isDisposalImmediatelyAfterComplete` 在本移植的用法（IPDB 下载）里没有
    /// 任务使用，未移植。
    fn retain_fresh(&self) {
        let now = now_ms();
        if let Ok(mut tasks) = self.tasks.lock() {
            tasks.retain(|task| {
                let Some(finished_at) = task.finished_at_ms else {
                    return true;
                };
                let retention = match task.status {
                    BackgroundTaskStatus::Completed => 60_000,
                    _ => 1_800_000,
                };
                now < finished_at + retention
            });
        }
    }

    fn update(&self, id: &str, apply: impl FnOnce(&mut BackgroundTask)) {
        if let Ok(mut tasks) = self.tasks.lock() {
            if let Some(task) = tasks.iter_mut().find(|task| task.id == id) {
                apply(task);
            }
        }
        self.notify(id);
    }

    fn notify(&self, id: &str) {
        // 无订阅者时发送失败属正常（对齐上游无 SSE 客户端时的空转）
        let _ = self.tx.send(id.to_string());
    }
}

/// 把 GeoIP 更新器的 [`GeoIpProgress`] 事件流映射为注册表里的下载任务。
///
/// 逐项对齐上游 `IPDB#updateMMDB` / `IPDB#downloadFile`：
/// - 任务标题 `Lang.IPDB_DOWNLOAD_MMDB`（一个数据库一个任务）；
/// - 每换一个镜像重设状态文本 `Lang.IPDB_DOWNLOAD_MMDB_DESCRIPTION`（参数 = 镜像 URL）；
/// - `contentLength > 0` ⇒ DETERMINATE + `max = contentLength`，否则 INDETERMINATE
///   （响应体整块到达，`current` 一次性设为已接收字节数，对齐上游逐块 `setCurrent`）；
/// - 解压/校验与落盘阶段切回 INDETERMINATE（上游下载完成后同样切回）；
/// - 结束时 `complete()` / `fail()`。
pub struct GeoIpTaskAdapter {
    registry: Arc<BackgroundTaskRegistry>,
    tasks: StdMutex<HashMap<String, String>>,
}

impl GeoIpTaskAdapter {
    pub fn new(registry: Arc<BackgroundTaskRegistry>) -> Self {
        Self {
            registry,
            tasks: StdMutex::new(HashMap::new()),
        }
    }

    /// 生成可直接交给
    /// [`GeoIpUpdater::with_progress_sink`](pbh_core::geoip_update::GeoIpUpdater::with_progress_sink)
    /// 的进度回调。
    pub fn sink(self: &Arc<Self>) -> Arc<dyn Fn(GeoIpProgress) + Send + Sync> {
        let this = self.clone();
        Arc::new(move |progress| this.handle(&progress))
    }

    fn handle(&self, progress: &GeoIpProgress) {
        let Some(task_id) = self.task_id(&progress.database) else {
            return;
        };
        match &progress.stage {
            GeoIpProgressStage::DownloadStart => {
                if let Some(url) = &progress.url {
                    self.registry.set_status_text(
                        &task_id,
                        Some(TranslationComponent::with_params(
                            LANG_IPDB_DOWNLOAD_MMDB_DESCRIPTION,
                            vec![url.clone().into()],
                        )),
                    );
                }
            }
            GeoIpProgressStage::DownloadBytes { bytes, total } => {
                let bar_type = if total.map(|size| size > 0).unwrap_or(false) {
                    BackgroundTaskBarType::Determinate
                } else {
                    BackgroundTaskBarType::Indeterminate
                };
                self.registry.set_bar_type(&task_id, bar_type);
                self.registry
                    .set_progress(&task_id, total.unwrap_or(0), *bytes);
            }
            GeoIpProgressStage::Validate | GeoIpProgressStage::Write => {
                self.registry
                    .set_bar_type(&task_id, BackgroundTaskBarType::Indeterminate);
            }
            GeoIpProgressStage::Finished { success } => {
                if *success {
                    self.registry.complete_task(&task_id);
                } else {
                    self.registry.fail_task(&task_id);
                }
                if let Ok(mut tasks) = self.tasks.lock() {
                    tasks.remove(&progress.database);
                }
            }
        }
    }

    /// 取（或创建）数据库对应的任务；创建即 QUEUED → RUNNING（对齐 `addTaskAsync`）
    fn task_id(&self, database: &str) -> Option<String> {
        let mut tasks = self.tasks.lock().ok()?;
        if let Some(id) = tasks.get(database) {
            return Some(id.clone());
        }
        let id = self
            .registry
            .add_task(TranslationComponent::new(LANG_IPDB_DOWNLOAD_MMDB));
        self.registry.mark_running(&id);
        tasks.insert(database.to_string(), id.clone());
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbh_core::i18n::Translator;

    fn render(component: &TranslationComponent) -> String {
        Translator::embedded().render(component, "zh_cn")
    }

    #[test]
    fn task_lifecycle_mirrors_upstream_state_machine() {
        let registry = BackgroundTaskRegistry::new();
        let id = registry.add_task(TranslationComponent::new("IPDB_DOWNLOAD_MMDB"));

        let queued = registry.get_task(&id).unwrap();
        assert_eq!(queued.status.as_str(), "QUEUED");
        assert!(!queued.status.is_active());
        assert_eq!(queued.bar_type.as_str(), "INDETERMINATE");
        assert_eq!(queued.progress(), 0.0, "max=0 ⇒ progress 0.0");

        registry.mark_running(&id);
        assert_eq!(registry.get_task(&id).unwrap().status.as_str(), "RUNNING");
        assert!(registry.get_task(&id).unwrap().status.is_active());

        registry.set_progress(&id, 100, 40);
        let running = registry.get_task(&id).unwrap();
        assert_eq!(running.max, 100);
        assert_eq!(running.current, 40);
        assert_eq!(running.progress(), 0.4);

        registry.set_bar_type(&id, BackgroundTaskBarType::Determinate);
        registry.set_status_text(
            &id,
            Some(TranslationComponent::with_params(
                "IPDB_DOWNLOAD_MMDB_DESCRIPTION",
                vec!["https://mirror.test/GeoLite2-City.mmdb.xz".to_string().into()],
            )),
        );
        let updated = registry.get_task(&id).unwrap();
        assert_eq!(updated.bar_type.as_str(), "DETERMINATE");
        assert_eq!(
            render(updated.status_text.as_ref().unwrap()),
            "从远程服务器下载: https://mirror.test/GeoLite2-City.mmdb.xz"
        );

        registry.complete_task(&id);
        let done = registry.get_task(&id).unwrap();
        assert_eq!(done.status.as_str(), "COMPLETED");
        assert!(!done.status.is_active());
        assert!(done.finished_at_ms.is_some());

        // fail() 与 complete() 的语义差异（对齐上游两个方法）
        let failed_id = registry.add_task(TranslationComponent::new("X"));
        registry.fail_task(&failed_id);
        assert_eq!(registry.get_task(&failed_id).unwrap().status.as_str(), "FAILED");
    }

    #[test]
    fn task_list_orders_active_first_then_start_at_desc() {
        let registry = BackgroundTaskRegistry::new();
        let first = registry.add_task(TranslationComponent::new("A"));
        registry.complete_task(&first);
        let second = registry.add_task(TranslationComponent::new("B"));
        registry.mark_running(&second);
        let third = registry.add_task(TranslationComponent::new("C"));
        registry.mark_running(&third);

        // now_ms 为毫秒精度，三个任务可能同一毫秒创建；显式错开 start_at 验证排序
        //（first 最旧 1000 < second 2000 < third 最新 3000）
        {
            let mut tasks = registry.tasks.lock().unwrap();
            tasks[0].start_at_ms = 1000;
            tasks[1].start_at_ms = 2000;
            tasks[2].start_at_ms = 3000;
        }

        let list = registry.task_list();
        let ids: Vec<String> = list.iter().map(|task| task.id.clone()).collect();
        // 活跃（RUNNING）任务在前、startAt 倒序；COMPLETED 任务还在 1 分钟保留窗口内，
        // 排在其余非活跃（含 QUEUED）之前
        assert_eq!(ids, vec![third, second, first]);
    }

    #[test]
    fn completed_tasks_are_cleaned_after_one_minute_failed_after_thirty() {
        let registry = BackgroundTaskRegistry::new();
        let completed = registry.add_task(TranslationComponent::new("A"));
        registry.complete_task(&completed);
        let failed = registry.add_task(TranslationComponent::new("B"));
        registry.fail_task(&failed);

        // 把完成时间回拨：COMPLETED 61 秒前 ⇒ 清理；FAILED 61 秒前 ⇒ 保留（30 分钟窗口）
        {
            let mut tasks = registry.tasks.lock().unwrap();
            let past = now_ms() - 61_000;
            for task in tasks.iter_mut() {
                task.finished_at_ms = Some(past);
            }
        }
        let list = registry.task_list();
        let remaining: Vec<&str> = list
            .iter()
            .map(|task| task.title.key.as_str())
            .collect();
        assert_eq!(remaining, vec!["B"], "COMPLETED 1 分钟后清理，FAILED 保留 30 分钟");

        // FAILED 31 分钟前 ⇒ 也被清理
        {
            let mut tasks = registry.tasks.lock().unwrap();
            let past = now_ms() - 31 * 60_000;
            for task in tasks.iter_mut() {
                task.finished_at_ms = Some(past);
            }
        }
        assert!(registry.task_list().is_empty());
    }

    #[test]
    fn geoip_adapter_maps_progress_stream_to_ipdb_tasks() {
        let registry = Arc::new(BackgroundTaskRegistry::new());
        let adapter = Arc::new(GeoIpTaskAdapter::new(registry.clone()));
        let sink = adapter.sink();

        let url = "https://github.com/PBH-BTN/GeoLite.mmdb/releases/latest/download/GeoLite2-City.mmdb.xz";
        // 上游 updateMMDB → downloadFile 的事件序列
        sink(GeoIpProgress {
            database: "GeoLite2-City".to_string(),
            target: "/tmp/GeoIP-City.mmdb".into(),
            stage: GeoIpProgressStage::DownloadStart,
            url: Some(url.to_string()),
        });
        sink(GeoIpProgress {
            database: "GeoLite2-City".to_string(),
            target: "/tmp/GeoIP-City.mmdb".into(),
            stage: GeoIpProgressStage::DownloadBytes {
                bytes: 6_000_000,
                total: Some(6_000_000),
            },
            url: Some(url.to_string()),
        });
        sink(GeoIpProgress {
            database: "GeoLite2-City".to_string(),
            target: "/tmp/GeoIP-City.mmdb".into(),
            stage: GeoIpProgressStage::Validate,
            url: None,
        });
        sink(GeoIpProgress {
            database: "GeoLite2-City".to_string(),
            target: "/tmp/GeoIP-City.mmdb".into(),
            stage: GeoIpProgressStage::Write,
            url: None,
        });
        sink(GeoIpProgress {
            database: "GeoLite2-City".to_string(),
            target: "/tmp/GeoIP-City.mmdb".into(),
            stage: GeoIpProgressStage::Finished { success: true },
            url: None,
        });

        let list = registry.task_list();
        assert_eq!(list.len(), 1, "一个数据库一个任务");
        let task = &list[0];
        // 标题 = 上游 Lang.IPDB_DOWNLOAD_MMDB（同键）
        assert_eq!(task.title.key, "IPDB_DOWNLOAD_MMDB");
        assert_eq!(
            render(&task.title),
            "[GeoIPDB] 下载数据库: {}",
            "标题无参数，模板原样"
        );
        assert_eq!(task.status.as_str(), "COMPLETED");
        // 终态 barType：Validate/Write 阶段已切回 INDETERMINATE（对齐上游下载完成后切回）
        assert_eq!(task.bar_type.as_str(), "INDETERMINATE");
        assert_eq!(task.max, 6_000_000);
        assert_eq!(task.current, 6_000_000);
        assert_eq!(task.progress(), 1.0);

        // 失败路径：同一数据库再次更新（新任务）且全部镜像耗尽
        sink(GeoIpProgress {
            database: "GeoLite2-City".to_string(),
            target: "/tmp/GeoIP-City.mmdb".into(),
            stage: GeoIpProgressStage::DownloadStart,
            url: Some(url.to_string()),
        });
        sink(GeoIpProgress {
            database: "GeoLite2-City".to_string(),
            target: "/tmp/GeoIP-City.mmdb".into(),
            stage: GeoIpProgressStage::Finished { success: false },
            url: None,
        });
        let list = registry.task_list();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].status.as_str(), "FAILED", "新任务在前（startAt 倒序）");
        assert_eq!(
            list[0].bar_type.as_str(),
            "INDETERMINATE",
            "未知长度（无 DownloadBytes）保持 INDETERMINATE"
        );
        assert_eq!(list[0].title.key, "IPDB_DOWNLOAD_MMDB");
    }
}
