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
//! ## 已知缺口（非静默省略）
//!
//! 1. **持久化仍是内存版**：`MonitorSink` 目前注入的是 pbh-core 的
//!    [`InMemoryMonitorSink`]，DB 版 sink（`alerts` / `traffic_journal_v3` /
//!    `peer_connection_metrics(_track)` / `peer_records` / `tracked_swarm` 五张表）
//!    **尚未接线**，因此监控数据重启即丢、WebUI 的监控视图读不到数据。
//!    操作清单（逐条对齐上游实体与 `resources/mapper/sqlite/*.xml`）见
//!    `pbh-core/src/modules/monitor.rs` 头部的 INTEGRATION SNIPPET 【3】。
//! 2. **时长/限速回落缺失**：`Downloader` trait 未暴露 `getSpeedLimiter()` /
//!    `setSpeedLimiter()`，故 `traffic-sliding-capping` 的滑动窗口限速在本移植中
//!    与上游「`getSpeedLimiter() == null` ⇒ `continue`」同义：只计算不落地。
//!    上游默认关闭该功能（`traffic-sliding-capping.enabled: false`），默认配置下无行为差异。

use pbh_core::config::ProfileConfig;
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::module::RuleModule;
use pbh_core::modules::{
    ActiveMonitoringModule, DownloaderTrafficStats, MonitorSink, PeerRecordingServiceModule,
    SessionAnalyseServiceModule, SpeedLimitChange, SwarmTrackingModule,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

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
        };
        if let Some(swarm) = host.swarm_tracking() {
            swarm.on_enable();
        }
        host
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
    pub async fn run_scheduled(
        &self,
        entries: &[DownloaderEntry],
        now_ms: i64,
    ) -> Vec<SpeedLimitChange> {
        let mut changes = Vec::new();

        // 1) `updateTrafficStatus`：先判断到期（持锁），再在锁外做下载器网络 I/O
        let traffic_due = match self.schedule.lock() {
            Ok(mut schedule) => self.active_monitoring().is_some_and(|_| {
                due(&mut schedule.traffic_tick, TRAFFIC_TICK_INTERVAL.as_millis() as i64)
            }),
            Err(_) => false,
        };
        if traffic_due {
            if let Some(active) = self.active_monitoring() {
                let stats = collect_traffic_stats(entries).await;
                changes = active.on_tick(&stats, now_ms);
                for change in &changes {
                    info!(
                        "active-monitoring: 下载器 {} 上传限速调整为 {} bytes/s",
                        change.downloader_name, change.limiter.upload
                    );
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
            if due(&mut schedule.session_flush, session.settings.data_flush_interval_ms) {
                let summary = session.flush_data(now_ms);
                debug!(
                    "session-analyse flushData: track 行 {} / {}，删除 {} 条，缓存 {} 条",
                    summary.aggregated_rows_out_of_day,
                    summary.aggregated_rows_in_day,
                    summary.deleted,
                    session.cache_len()
                );
            }
            if due(&mut schedule.session_cleanup, session.settings.cleanup_interval_ms) {
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
            if due(&mut schedule.swarm_flush, swarm.settings.data_flush_interval_ms) {
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
            active.on_tick(&stats, now_ms);
        }
        if let Some(session) = self.session_analyse() {
            session.flush_data(now_ms);
        }
        if let Some(recording) = self.peer_recording() {
            recording.flush();
        }
    }
}

/// 对齐 `updateTrafficStatus()` 的取值口径：
/// `login()` 失败（抛异常或不 success）→ 跳过；`getStatistics()` 抛异常 → 记日志并跳过。
///
/// `getSpeedLimiter()` 在本移植未暴露（见模块文档的已知缺口 2），固定为 `None`；
/// 特性标志按上游 `DownloaderFeatureFlag.TRAFFIC_STATS` 的名字比较。
pub async fn collect_traffic_stats(entries: &[DownloaderEntry]) -> Vec<DownloaderTrafficStats> {
    let mut stats = Vec::with_capacity(entries.len());
    for entry in entries {
        let downloader = entry.downloader.clone();
        let login = match downloader.login().await {
            Ok(login) => login,
            Err(e) => {
                debug!("active-monitoring: 下载器 {} 登录失败，跳过本轮流量统计: {e}", downloader.id());
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
        stats.push(DownloaderTrafficStats::new(
            downloader.id(),
            downloader.name(),
            true,
            statistics.all_time_download,
            statistics.all_time_upload,
            None,
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
        host.run_scheduled(&[], 0).await;
        assert_eq!(sink.tracked_swarm().len(), 1);
        let restarted = MonitorHost::new(&profile(), sink.clone());
        assert!(restarted.swarm_tracking().is_some());
        assert!(sink.tracked_swarm().is_empty(), "resetTable -> 数据随重启丢弃");
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

        host.run_scheduled(&[], 0).await;
        assert_eq!(host.peer_recording().unwrap().cache_len(), 2, "flush 不清空缓存");
        assert_eq!(sink.peer_records().len(), 2, "peer-recording.flush 写库");
        assert_eq!(sink.tracked_swarm().len(), 2, "swarm-tracking.flushAll 写库");
        assert_eq!(host.swarm_tracking().unwrap().count(), 2, "count = trackedSwarmDao.count()");
        // session-analyse 的 track 行先 upsert、再按「是否今天」聚合进 metrics 后删除
        assert_eq!(sink.connection_metrics().len(), 1, "flushData 聚合落库");
        assert!(sink.metrics_tracks().is_empty(), "聚合后的 track 行被删除");
    }

    /// fixed-delay 语义：首次立即执行，间隔未到时不再执行。
    #[tokio::test]
    async fn scheduled_tasks_run_immediately_then_respect_the_interval() {
        let sink = Arc::new(InMemoryMonitorSink::new());
        let host = MonitorHost::new(&profile(), sink.clone());
        let torrent = torrent();
        host.on_peers_retrieved("qb", &torrent, &[peer("1.2.3.4")], 0);

        host.run_scheduled(&[], 0).await;
        assert_eq!(sink.tracked_swarm().len(), 1);
        // 第二次调用（间隔远未到）不应重复刷写：缓存行数不变且无新增行
        host.run_scheduled(&[], 1_000).await;
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
        assert!(!sink.connection_metrics().is_empty(), "onDisable 的 flushData()");
    }
}
