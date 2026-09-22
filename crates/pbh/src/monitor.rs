//! 监控模块的应用层宿主（对齐上游各监控模块的 `onEnable` / `onPeersRetrieved` / `onDisable`）。
//!
//! 上游四个监控模块都不是 `RuleFeatureModule`（`RunCheckModuleOrgan` 只对后者调 `shouldBanPeer`），
//! 因此它们**不进判定流水线**：pbh-core 的 `ProfileConfig::build_monitor_modules` 只负责构造，
//! 调度由本宿主承担（对齐 `registerScheduledTask` 的 fixed-delay 语义，首次 delay 0 立即执行一次）：
//!
//! | 上游 | 本移植 |
//! | --- | --- |
//! | `ActiveMonitoringModule.updateTrafficStatus` 每 1 分钟 | [`MonitorHost::run_scheduled`]（[`TRAFFIC_TICK_INTERVAL`]） |
//! | `SessionAnalyseServiceModule.flushData` 每 `data-flush-interval` | [`MonitorHost::run_scheduled`] |
//! | `SessionAnalyseServiceModule.cleanup` 每 `cleanup-interval` | [`MonitorHost::run_scheduled`] |
//! | `PeerRecordingServiceModule.flush` 每 `data-flush-interval` | [`MonitorHost::run_scheduled`] |
//! | `PeerRecordingServiceModule.cleanup` 每 `data-cleanup-interval` | [`MonitorHost::run_scheduled`] |
//! | `SwarmTrackingModule.onEnable`（`resetTable`）+ `flushAll` | [`MonitorHost::new`] + [`MonitorHost::run_scheduled`] |
//! | 各模块 `onPeersRetrieved` | [`MonitorHost::on_peers_retrieved`]（在 wave 拉完 peers 后调用） |
//! | 各模块 `onDisable` | [`MonitorHost::shutdown`] |
//!
//! 落点由应用层注入：生产路径是 `pbh-db` 的 `DbMonitorSink`（与其它持久化共用同一个
//! `Database`，五张表逐条对齐上游）；测试可用 pbh-core 的 `InMemoryMonitorSink`。
//! WebUI 的监控视图（`/api/modules/swarm-tracking`、`/api/alerts`）由 `pbh-web` 直接读库。
//!
//! ## 限速落地
//!
//! `Downloader` trait 已暴露 `getSpeedLimiter()` / `setSpeedLimiter()`（bytes/s，<=0 为不限制），
//! 六个适配器均按上游端点实现。`traffic-sliding-capping` 的滑动窗口限速现已**真正下发**：
//! [`collect_traffic_stats`] 取当前限速、[`MonitorHost::run_scheduled`] 在
//! [`ActiveMonitoringModule::on_tick`] 算出新限速后调用 `set_speed_limiter` 落地。
//! 未实现限速（返回 `Err`）的下载器对齐上游 `getSpeedLimiter() == null` ⇒ 跳过。
//! 上游默认关闭该功能（`traffic-sliding-capping.enabled: false`），默认配置下无行为差异。

use pbh_core::config::ProfileConfig;
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::module::RuleModule;
use pbh_core::modules::{
    ActiveMonitoringModule, DownloaderTrafficStats, MonitorSink, PeerRecordingServiceModule,
    SessionAnalyseServiceModule, SpeedLimitChange, SpeedLimiter, SwarmTrackingModule,
    TrafficMonitoringAlert,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

use crate::push::AlertManager;
use crate::wave::DownloaderEntry;

/// 上游 `registerScheduledTask(this::updateTrafficStatus, 0, 1, TimeUnit.MINUTES)`。
pub const TRAFFIC_TICK_INTERVAL: Duration = Duration::from_secs(60);

/// `MonitorSink` 之上的一次「是否到期」判断。
///
/// 对齐 `scheduleWithFixedDelay(task, 0, interval)`：**首次立即执行**，之后每 `interval` 执行一次。
/// `interval_ms <= 0` 视为禁用（上游对非正间隔会抛 `IllegalArgumentException`，
/// 随包配置从不出现该形态，这里保守跳过而不是崩溃）。
fn due(slot: &mut Option<Instant>, interval_ms: i64) -> bool {
    if interval_ms <= 0 {
        return false;
    }
    let now = Instant::now();
    match *slot {
        None => {
            *slot = Some(now);
            true
        }
        Some(last) => {
            if now.duration_since(last) >= Duration::from_millis(interval_ms as u64) {
                *slot = Some(now);
                true
            } else {
                false
            }
        }
    }
}

/// 各定时任务的上次执行时间。
#[derive(Debug, Default)]
struct ScheduleState {
    traffic_tick: Option<Instant>,
    session_flush: Option<Instant>,
    session_cleanup: Option<Instant>,
    peer_recording_flush: Option<Instant>,
    peer_recording_cleanup: Option<Instant>,
    swarm_flush: Option<Instant>,
}

/// 监控模块宿主：持有 `build_monitor_modules` 构造出的模块。
///
/// 注入的 [`MonitorSink`] 由各模块持有（构造时 clone），宿主只负责调度；
/// 应用层若需要读取 sink（例如 WebUI 视图），保留自己那份 `Arc` 即可。
pub struct MonitorHost {
    /// `ActiveMonitoring → SwarmTracking → SessionAnalyse → PeerRecording`（上游注册顺序）
    modules: Vec<Box<dyn RuleModule>>,
    schedule: Mutex<ScheduleState>,
    /// 推送渠道（上游 `AlertManagerImpl.publishAlert(push=true)` 的推送分支）：
    /// `active-monitoring` 的日流量阈值告警除落库外，还要经它分发到各推送渠道。
    /// `None`（未接线，如部分测试）⇒ 只落库、不推送。
    alert_manager: Option<Arc<AlertManager>>,
}

impl MonitorHost {
    /// 按 `profile.yml` 的 `module.active-monitoring` / `module.peer-analyse-service` 段构造模块。
    ///
    /// `swarm-tracking` 的 `onEnable`（`trackedSwarmDao.resetTable()`）在构造时立即执行一次：
    /// 临时表随本次运行会话存在，重启即清空。
    pub fn new(profile: &ProfileConfig, sink: Arc<dyn MonitorSink>) -> Self {
        let modules = profile.build_monitor_modules(sink);
        let host = Self {
            modules,
            schedule: Mutex::new(ScheduleState::default()),
            alert_manager: None,
        };
        if let Some(swarm) = host.swarm_tracking() {
            swarm.on_enable();
        }
        host
    }

    /// 接线推送渠道：日流量阈值告警（`publishAlert(push=true)`）会经它分发
    /// （渲染规则与 alert 表完全一致，见 [`AlertManager::publish_alert`]）。
    pub fn with_alert_manager(mut self, alert_manager: Arc<AlertManager>) -> Self {
        self.alert_manager = Some(alert_manager);
        self
    }

    /// 已构造的监控模块名（`build_monitor_modules` 的顺序，供启动日志）。
    pub fn config_names(&self) -> Vec<&str> {
        self.modules.iter().map(|m| m.config_name()).collect()
    }

    fn module<T: 'static>(&self, config_name: &str) -> Option<&T> {
        self.modules
            .iter()
            .find(|m| m.config_name() == config_name)
            .and_then(|m| m.as_any().downcast_ref::<T>())
    }

    pub fn active_monitoring(&self) -> Option<&ActiveMonitoringModule> {
        self.module("active-monitoring")
    }

    pub fn session_analyse(&self) -> Option<&SessionAnalyseServiceModule> {
        self.module("peer-analyse-service.session-analyse")
    }

    pub fn peer_recording(&self) -> Option<&PeerRecordingServiceModule> {
        self.module("peer-analyse-service.peer-recording")
    }

    pub fn swarm_tracking(&self) -> Option<&SwarmTrackingModule> {
        self.module("peer-analyse-service.swarm-tracking")
    }

    /// 各模块的 `onPeersRetrieved`：wave 拉完某个 torrent 的 peers 后立即调用
    /// （对齐上游在 `DownloaderServerImpl` 的 peer 拉取回调里逐个模块派发）。
    pub fn on_peers_retrieved(
        &self,
        downloader_id: &str,
        torrent: &TorrentData,
        peers: &[PeerData],
        now_ms: i64,
    ) {
        if let Some(session) = self.session_analyse() {
            session.sync_peers(downloader_id, torrent, peers, now_ms);
        }
        if let Some(recording) = self.peer_recording() {
            recording.on_peers_retrieved(downloader_id, torrent, peers, now_ms);
        }
        if let Some(swarm) = self.swarm_tracking() {
            swarm.sync_peers(downloader_id, torrent, peers, now_ms);
        }
    }

    /// 一轮定时任务（由 ban wave 循环每个 tick 调用，内部按各模块的间隔判断是否到期）。
    ///
    /// `dry_run` 为 true 时只计算限速变更并记日志，不向下载器下发（`--dry-run` 演练模式）。
    pub async fn run_scheduled(
        &self,
        entries: &[DownloaderEntry],
        now_ms: i64,
        dry_run: bool,
    ) -> Vec<SpeedLimitChange> {
        let mut changes = Vec::new();

        // 1) `updateTrafficStatus`：先判断到期（持锁），再在锁外做下载器网络 I/O
        let traffic_due = match self.schedule.lock() {
            Ok(mut schedule) => self.active_monitoring().is_some_and(|_| {
                due(
                    &mut schedule.traffic_tick,
                    TRAFFIC_TICK_INTERVAL.as_millis() as i64,
                )
            }),
            Err(_) => false,
        };
        if traffic_due {
            if let Some(active) = self.active_monitoring() {
                let stats = collect_traffic_stats(entries).await;
                let (tick_changes, alert) = active.on_tick(&stats, now_ms);
                changes = tick_changes;
                // 对齐 `AlertManagerImpl.publishAlert(push=true)`：告警已落库，此处补推送分支
                self.dispatch_push_alert(alert).await;
                for change in &changes {
                    info!(
                        "active-monitoring: 下载器 {} 上传限速调整为 {} bytes/s（下载 {} bytes/s）",
                        change.downloader_name, change.limiter.upload, change.limiter.download
                    );
                    // 对齐 `downloader.setSpeedLimiter(...)`：把滑动窗口算出的新限速真正下发。
                    // 找不到匹配下载器或下发失败都只记日志，不影响其它下载器。
                    if dry_run {
                        info!(
                            "[dry-run] 跳过向下载器 {} 下发限速（上传 {} / 下载 {} bytes/s）",
                            change.downloader_id, change.limiter.upload, change.limiter.download
                        );
                        continue;
                    }
                    if let Some(entry) = entries
                        .iter()
                        .find(|e| e.downloader.id() == change.downloader_id)
                    {
                        if let Err(e) = entry
                            .downloader
                            .set_speed_limiter(change.limiter.upload, change.limiter.download)
                            .await
                        {
                            warn!(
                                "active-monitoring: 下载器 {} 限速下发失败: {e}",
                                change.downloader_id
                            );
                        }
                    }
                }
            }
        }

        // 2) flush / cleanup：全部是本地缓存与 sink 调用
        self.run_periodic(now_ms);
        changes
    }

    /// `flush` / `cleanup` 类定时任务（全部为内存或 sink 调用，无需等待网络）。
    fn run_periodic(&self, now_ms: i64) {
        let Ok(mut schedule) = self.schedule.lock() else {
            return;
        };
        if let Some(session) = self.session_analyse() {
            if due(
                &mut schedule.session_flush,
                session.settings.data_flush_interval_ms,
            ) {
                let summary = session.flush_data(now_ms);
                debug!(
                    "session-analyse flushData: track 行 {} / {}，删除 {} 条，缓存 {} 条",
                    summary.aggregated_rows_out_of_day,
                    summary.aggregated_rows_in_day,
                    summary.deleted,
                    session.cache_len()
                );
            }
            if due(
                &mut schedule.session_cleanup,
                session.settings.cleanup_interval_ms,
            ) {
                let deleted = session.cleanup(now_ms);
                if deleted > 0 {
                    info!("session-analyse 清理过期聚合数据 {deleted} 条");
                }
            }
        }
        if let Some(recording) = self.peer_recording() {
            if due(
                &mut schedule.peer_recording_flush,
                recording.settings.data_flush_interval_ms,
            ) {
                let cached = recording.cache_len();
                recording.flush();
                debug!("peer-recording flush: 写库 {cached} 条（缓存清空 dirty 标记）");
            }
            if due(
                &mut schedule.peer_recording_cleanup,
                recording.settings.data_cleanup_interval_ms,
            ) {
                // `data-retention-time <= 0` 时上游直接返回（`cleanup` 返回 None 表示未执行）
                if let Some(deleted) = recording.cleanup(now_ms) {
                    if deleted > 0 {
                        info!("peer-recording 清理过期记录 {deleted} 条");
                    }
                }
            }
        }
        if let Some(swarm) = self.swarm_tracking() {
            if due(
                &mut schedule.swarm_flush,
                swarm.settings.data_flush_interval_ms,
            ) {
                swarm.flush_all();
                debug!("swarm-tracking flushAll: 当前跟踪 {}", swarm.count());
            }
        }
    }

    /// 各模块 `onDisable`：`active-monitoring` 再跑一次 `updateTrafficStatus`、
    /// `session-analyse` 跑 `flushData`、`peer-recording` 跑 `flush`；
    /// `swarm-tracking` 只关缓存（内存实现无需动作，数据本就随进程结束丢弃）。
    pub async fn shutdown(&self, entries: &[DownloaderEntry], now_ms: i64) {
        if let Some(active) = self.active_monitoring() {
            let stats = collect_traffic_stats(entries).await;
            let (_, alert) = active.on_tick(&stats, now_ms);
            // 对齐 `onDisable` → `updateTrafficStatus`：同样会走一次完整的告警发布
            self.dispatch_push_alert(alert).await;
        }
        if let Some(session) = self.session_analyse() {
            session.flush_data(now_ms);
        }
        if let Some(recording) = self.peer_recording() {
            recording.flush();
        }
    }

    /// 日流量阈值告警的推送分支（对齐 `AlertManagerImpl.publishAlert(push=true)`）：
    /// 复用与 alert 表完全一致的 title/content 渲染路径
    /// （[`AlertManager::publish_alert`]）；推送失败只在内部记日志，绝不打断监控 tick。
    async fn dispatch_push_alert(&self, alert: Option<TrafficMonitoringAlert>) {
        let Some(alert) = alert else {
            return;
        };
        if !alert.alert.push {
            return;
        }
        if let Some(alert_manager) = &self.alert_manager {
            alert_manager
                .publish_alert(
                    true,
                    alert.alert.level.into(),
                    &alert.alert.identifier,
                    &alert.alert.title,
                    &alert.alert.description,
                )
                .await;
        }
    }
}

/// 对齐 `updateTrafficStatus()` 的取值口径：
/// `login()` 失败（抛异常或不 success）→ 跳过；`getStatistics()` 抛异常 → 记日志并跳过。
///
/// `getSpeedLimiter()` 在支持限速的下载器上取当前上传/下载限速；返回 `Err`（未实现/不支持）
/// 时对齐上游 `getSpeedLimiter() == null` ⇒ 该下载器不参与滑动窗口限速；
/// 特性标志按上游 `DownloaderFeatureFlag.TRAFFIC_STATS` 的名字比较。
pub async fn collect_traffic_stats(entries: &[DownloaderEntry]) -> Vec<DownloaderTrafficStats> {
    let mut stats = Vec::with_capacity(entries.len());
    for entry in entries {
        let downloader = entry.downloader.clone();
        let login = match downloader.login().await {
            Ok(login) => login,
            Err(e) => {
                debug!(
                    "active-monitoring: 下载器 {} 登录失败，跳过本轮流量统计: {e}",
                    downloader.id()
                );
                continue;
            }
        };
        if !login.success {
            continue;
        }
        let statistics = match downloader.statistics().await {
            Ok(statistics) => statistics,
            Err(e) => {
                warn!("active-monitoring: 无法写入小时级流量日志: {e}");
                continue;
            }
        };
        let traffic_stats_feature = downloader
            .feature_flags()
            .iter()
            .any(|flag| flag == "TRAFFIC_STATS");
        // `getSpeedLimiter()`：未实现（返回 Err）的下载器对齐上游 `getSpeedLimiter() == null` ⇒ 跳过限速。
        let speed_limiter = match downloader.get_speed_limiter().await {
            Ok((upload, download)) => Some(SpeedLimiter { upload, download }),
            Err(e) => {
                debug!(
                    "active-monitoring: 下载器 {} 不支持 getSpeedLimiter，跳过限速: {e}",
                    downloader.id()
                );
                None
            }
        };
        stats.push(DownloaderTrafficStats::new(
            downloader.id(),
            downloader.name(),
            true,
            statistics.all_time_download,
            statistics.all_time_upload,
            speed_limiter,
            traffic_stats_feature,
        ));
    }
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbh_core::modules::InMemoryMonitorSink;

    fn profile() -> ProfileConfig {
        serde_yaml::from_str(
            r#"
module:
  active-monitoring:
    enabled: true
    traffic-monitoring:
      daily: -1
  peer-analyse-service:
    session-analyse:
      enabled: true
      data-flush-interval: 3600000
      cleanup-interval: 3600000
      data-retention-time: 15552000000
    swarm-tracking:
      enabled: true
      data-flush-interval: 3600000
    peer-recording:
      enabled: true
      data-flush-interval: 900000
      data-retention-time: 5184000000
      data-cleanup-interval: 604800000
"#,
        )
        .expect("profile yaml")
    }

    fn torrent() -> TorrentData {
        TorrentData {
            hash: "hash-1".into(),
            name: "示例种子".into(),
            progress: 0.5,
            total_size: 1_000_000_000,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: Some(false),
        }
    }

    fn peer(ip: &str) -> PeerData {
        PeerData {
            client_name: Some("qBittorrent/4.5.0".into()),
            peer_id: Some("-qB4500-xxxxxxxxxxxx".into()),
            dl_speed: 1024,
            downloaded: 4096,
            up_speed: 2048,
            uploaded: 8192,
            progress: 0.25,
            flags: Some("d u".into()),
            ip: ip.into(),
            port: 6881,
            raw_ip: format!("{ip}:6881"),
            connection: Some("uTP".into()),
        }
    }

    #[tokio::test]
    async fn host_constructs_modules_in_upstream_order_and_resets_swarm_table() {
        let sink = Arc::new(InMemoryMonitorSink::new());
        let host = MonitorHost::new(&profile(), sink.clone());
        assert_eq!(
            host.config_names(),
            vec![
                "active-monitoring",
                "peer-analyse-service.swarm-tracking",
                "peer-analyse-service.session-analyse",
                "peer-analyse-service.peer-recording"
            ]
        );
        assert!(host.active_monitoring().is_some());
        assert!(host.session_analyse().is_some());
        assert!(host.peer_recording().is_some());
        assert!(host.swarm_tracking().is_some());

        // `onEnable` 的 `resetTable()`：临时表随本次运行会话存在，重启即清空
        host.on_peers_retrieved("qb", &torrent(), &[peer("1.2.3.4")], 0);
        host.run_scheduled(&[], 0, false).await;
        assert_eq!(sink.tracked_swarm().len(), 1);
        let restarted = MonitorHost::new(&profile(), sink.clone());
        assert!(restarted.swarm_tracking().is_some());
        assert!(
            sink.tracked_swarm().is_empty(),
            "resetTable -> 数据随重启丢弃"
        );
    }

    #[test]
    fn disabled_sections_do_not_construct_modules() {
        let cfg: ProfileConfig =
            serde_yaml::from_str("module:\n  active-monitoring:\n    enabled: false\n").unwrap();
        let host = MonitorHost::new(&cfg, Arc::new(InMemoryMonitorSink::new()));
        assert!(host.config_names().is_empty());
        assert!(host.active_monitoring().is_none());
    }

    /// `onPeersRetrieved` → 定时 `flush` 全链路（内存 sink）：
    /// peer-recording 与 swarm-tracking 都应落库，session-analyse 的 track 行也应被聚合。
    #[tokio::test]
    async fn peers_retrieved_then_scheduled_flush_reaches_the_sink() {
        let sink = Arc::new(InMemoryMonitorSink::new());
        let host = MonitorHost::new(&profile(), sink.clone());
        let torrent = torrent();
        let peers = vec![peer("1.2.3.4"), peer("5.6.7.8")];

        host.on_peers_retrieved("qb", &torrent, &peers, 0);
        assert_eq!(host.peer_recording().unwrap().cache_len(), 2);
        assert_eq!(host.swarm_tracking().unwrap().cache_len(), 2);
        assert_eq!(host.session_analyse().unwrap().cache_len(), 2);
        assert!(sink.peer_records().is_empty(), "回调本身不落库");

        host.run_scheduled(&[], 0, false).await;
        assert_eq!(
            host.peer_recording().unwrap().cache_len(),
            2,
            "flush 不清空缓存"
        );
        assert_eq!(sink.peer_records().len(), 2, "peer-recording.flush 写库");
        assert_eq!(
            sink.tracked_swarm().len(),
            2,
            "swarm-tracking.flushAll 写库"
        );
        assert_eq!(
            host.swarm_tracking().unwrap().count(),
            2,
            "count = trackedSwarmDao.count()"
        );
        // session-analyse 的 track 行先 upsert、再按「是否今天」聚合进 metrics 后删除
        assert_eq!(sink.connection_metrics().len(), 1, "flushData 聚合落库");
        assert!(sink.metrics_tracks().is_empty(), "聚合后的 track 行被删除");
    }

    /// 生产落点（`DbMonitorSink`）下的同一链路：`onPeersRetrieved` → 定时 flush → SQLite
    /// （归档缺口 #1 的端到端证据：四个模块都真正写进了上游那五张表）。
    #[tokio::test]
    async fn db_sink_persists_the_whole_pipeline_into_sqlite() {
        let db = Arc::new(pbh_db::Database::open_in_memory().expect("内存库"));
        let sink: Arc<dyn MonitorSink> = Arc::new(pbh_db::DbMonitorSink::new(db.clone()));
        let host = MonitorHost::new(&profile(), sink.clone());
        let now = 1_700_000_000_000;

        host.on_peers_retrieved("qb", &torrent(), &[peer("1.2.3.4"), peer("5.6.7.8")], now);
        assert_eq!(db.tracked_swarm_count().unwrap(), 0, "回调本身不落库");

        host.run_scheduled(&[], now, false).await;
        assert_eq!(
            db.tracked_swarm_count().unwrap(),
            2,
            "swarm-tracking.flushAll 写库"
        );
        assert_eq!(
            host.swarm_tracking().unwrap().count(),
            2,
            "count = trackedSwarmDao.count()"
        );
        // `peer_records` / `peer_connection_metrics` 没有读取接口，用清理接口反查写入条数
        assert_eq!(
            sink.remove_peer_records_before(i64::MAX),
            2,
            "peer-recording.flush 写库"
        );
        assert_eq!(
            sink.remove_connection_metrics_before(i64::MAX),
            1,
            "session-analyse.flushData 聚合落库"
        );
        assert!(sink.list_metrics_tracks_at(i64::MAX).is_empty());

        // 重启（`onEnable` 的 resetTable）-> swarm 数据随本次运行会话丢弃
        sink.reset_tracked_swarm();
        assert_eq!(db.tracked_swarm_count().unwrap(), 0);
    }

    /// fixed-delay 语义：首次立即执行，间隔未到时不再执行。
    #[tokio::test]
    async fn scheduled_tasks_run_immediately_then_respect_the_interval() {
        let sink = Arc::new(InMemoryMonitorSink::new());
        let host = MonitorHost::new(&profile(), sink.clone());
        let torrent = torrent();
        host.on_peers_retrieved("qb", &torrent, &[peer("1.2.3.4")], 0);

        host.run_scheduled(&[], 0, false).await;
        assert_eq!(sink.tracked_swarm().len(), 1);
        // 第二次调用（间隔远未到）不应重复刷写：缓存行数不变且无新增行
        host.run_scheduled(&[], 1_000, false).await;
        assert_eq!(sink.tracked_swarm().len(), 1);
    }

    #[tokio::test]
    async fn shutdown_flushes_pending_data() {
        let sink = Arc::new(InMemoryMonitorSink::new());
        let host = MonitorHost::new(&profile(), sink.clone());
        let torrent = torrent();
        host.on_peers_retrieved("qb", &torrent, &[peer("1.2.3.4")], 0);
        assert!(sink.peer_records().is_empty());

        host.shutdown(&[], 5_000).await;
        assert_eq!(sink.peer_records().len(), 1, "onDisable 的 flush()");
        assert!(
            !sink.connection_metrics().is_empty(),
            "onDisable 的 flushData()"
        );
    }

    // ---------------- 日流量阈值告警的推送分发（publishAlert(push=true)） ----------------

    /// 记录请求的 mock fetcher（与 push.rs 测试同款样式）。
    struct RecordingFetcher {
        requests: Mutex<Vec<pbh_downloader::http::HttpRequest>>,
    }

    impl RecordingFetcher {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                requests: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<pbh_downloader::http::HttpRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl pbh_downloader::http::HttpFetcher for RecordingFetcher {
        fn execute<'a>(
            &'a self,
            req: pbh_downloader::http::HttpRequest,
        ) -> pbh_downloader::http::BoxFuture<'a, anyhow::Result<pbh_downloader::http::HttpResponse>>
        {
            Box::pin(async move {
                self.requests.lock().unwrap().push(req);
                Ok(pbh_downloader::http::HttpResponse::new(
                    200,
                    "{}".to_string(),
                ))
            })
        }
    }

    fn alert_manager_with(fetcher: Arc<RecordingFetcher>) -> Arc<crate::push::AlertManager> {
        let section: crate::config::PushSection =
            serde_yaml::from_str("a:\n  type: bark\n  device_key: \"k\"\n")
                .expect("测试用 push 配置应可解析");
        let push_manager = crate::push::PushManager::from_config(&section, fetcher);
        Arc::new(crate::push::AlertManager::new(
            Arc::new(push_manager),
            Arc::new(pbh_core::i18n::Translator::embedded()),
            "zh_cn",
        ))
    }

    fn traffic_profile(daily: i64) -> ProfileConfig {
        serde_yaml::from_str(&format!(
            r#"
module:
  active-monitoring:
    enabled: true
    traffic-monitoring:
      daily: {daily}
"#
        ))
        .expect("profile yaml")
    }

    /// 当日上传量达到阈值：告警落库（alert 表）**且**经推送渠道分发，
    /// 推送渲染与 alert 表一致（`"[PeerBanHelper/<LEVEL>] " + title` / 渲染后的 content，
    /// 对齐 `AlertManagerImpl.publishAlert(push=true)` → `PushManagerImpl.pushMessage`）。
    #[tokio::test]
    async fn daily_traffic_alert_fires_and_is_pushed_via_configured_channel() {
        let now = 1_700_000_000_000i64;
        let sink = Arc::new(InMemoryMonitorSink::new());
        // 当日已上传 2000 bytes ≥ 阈值 1000：先建行（at_start = 0）再在同小时内更新
        sink.update_traffic_journal("qb", 0, 0, 0, 0, now);
        sink.update_traffic_journal("qb", 0, 2000, 0, 0, now + 60_000);

        let fetcher = RecordingFetcher::new();
        let host = MonitorHost::new(&traffic_profile(1000), sink.clone())
            .with_alert_manager(alert_manager_with(fetcher.clone()));

        host.run_scheduled(&[], now, false).await;

        // 告警落库（alert 表可见）
        let alerts = sink.alerts();
        assert_eq!(alerts.len(), 1);
        assert!(
            alerts[0].identifier.starts_with("dataTrafficCapping-"),
            "identifier = dataTrafficCapping-<当日 0 点 epoch 秒>，实际: {}",
            alerts[0].identifier
        );
        assert_eq!(alerts[0].level, pbh_core::modules::AlertLevel::Warn);

        // 同一告警经推送渠道分发：标题带级别前缀，正文含渲染后的参数
        let requests = fetcher.requests();
        assert_eq!(requests.len(), 1, "恰好推送一次");
        let body = requests[0].body.clone().expect("请求体不应为空");
        assert!(body.contains("[PeerBanHelper/WARN]"), "{body}");
        assert!(body.contains("1000 B"), "正文应渲染阈值参数：{body}");

        // 推送管理器内的去重：同一 identifier 的未读告警不重复发布
        host.run_scheduled(&[], now + 60_000 * 30, false).await;
        assert_eq!(fetcher.requests().len(), 1, "一天只发一次");
    }

    /// `traffic-monitoring.daily: -1`（上游默认）：完全禁用 ⇒ 不告警、不推送、无网络流量。
    #[tokio::test]
    async fn daily_threshold_disabled_publishes_no_alert_and_no_push() {
        let now = 1_700_000_000_000i64;
        let sink = Arc::new(InMemoryMonitorSink::new());
        sink.update_traffic_journal("qb", 0, 0, 0, 0, now);
        sink.update_traffic_journal("qb", 0, i64::from(i32::MAX), 0, 0, now + 60_000);

        let fetcher = RecordingFetcher::new();
        let host = MonitorHost::new(&traffic_profile(-1), sink.clone())
            .with_alert_manager(alert_manager_with(fetcher.clone()));

        host.run_scheduled(&[], now, false).await;
        host.shutdown(&[], now + 120_000).await;

        assert!(sink.alerts().is_empty(), "禁用时不应发布告警");
        assert!(fetcher.requests().is_empty(), "禁用时不应产生推送流量");
    }
}
