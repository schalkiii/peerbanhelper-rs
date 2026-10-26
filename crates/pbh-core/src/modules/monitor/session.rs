// ===========================================================================
// peer-analyse-service.session-analyse
// ===========================================================================

/// `module.peer-analyse-service.session-analyse` 的运行期配置。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionAnalyseSettings {
    /// `cleanup-interval`（清理任务间隔；对齐 `getLong`，缺省 0）
    pub cleanup_interval_ms: i64,
    /// `data-flush-interval`（刷写间隔；对齐 `getLong`，缺省 0）
    pub data_flush_interval_ms: i64,
    /// `data-retention-time`（`reloadConfig` 读的保留期；对齐 `getLong`，缺省 0）
    pub data_retention_time_ms: i64,
}

/// `flushData()` 的结果（供日志/测试检视；上游只统计 deleted 且未使用）。
#[derive(Debug, Clone, Default)]
pub struct SessionFlushSummary {
    pub aggregated_rows_out_of_day: usize,
    pub aggregated_rows_in_day: usize,
    /// 「不在今天」分支写入的聚合行（`saveAggregating(..., true)` 覆盖写）
    pub saved_out_of_day: Vec<ConnectionMetricsRow>,
    /// 「在今天」分支写入的聚合行（`saveAggregating(..., false)` 合并写）
    pub saved_in_day: Vec<ConnectionMetricsRow>,
    /// 两个分支删除的 track 行总数
    pub deleted: usize,
}

/// 对齐上游 `SessionAnalyseServiceModule`（配置键 `peer-analyse-service.session-analyse`）。
///
/// - `onPeersRetrieved` → [`Self::sync_peers`]：把非握手中的 peer 写入 track 缓存
///   （缓存键 `(当日 0 点, downloader, torrent, peer 地址, 端口)`）；
/// - `flushData` → [`Self::flush_data`]：先刷缓存入库，再按「是否今天」分两批聚合到
///   `peer_connection_metrics`（非今天覆盖写、今天合并写），并删除已聚合的 track 行；
/// - `cleanup` → [`Self::cleanup`]：删除 `timeframe_at <= now - data-retention-time` 的聚合数据；
/// - `check` 恒 `pass()`。
///
/// 与上游的差异：上游 track 缓存是 `PBHCache`（容量 1000、3 分钟超时淘汰时后台刷库），
/// 本移植用普通 `HashMap`，仅由定时 `flush_data` 刷库（结果一致，写库时机更集中）。
pub struct SessionAnalyseServiceModule {
    pub settings: SessionAnalyseSettings,
    sink: Arc<dyn MonitorSink>,
    track_cache: Mutex<HashMap<MetricsTrackKey, MetricsTrackRow>>,
}

impl SessionAnalyseServiceModule {
    pub fn new(sink: Arc<dyn MonitorSink>, settings: SessionAnalyseSettings) -> Self {
        Self {
            settings,
            sink,
            track_cache: Mutex::new(HashMap::new()),
        }
    }

    /// 对齐 `PeerConnectionMetricsTrackService.syncPeers`：握手中的 peer 直接跳过。
    pub fn sync_peers(
        &self,
        downloader_id: &str,
        torrent: &TorrentData,
        peers: &[PeerData],
        now_ms: i64,
    ) {
        let torrent_id = self.sink.ensure_torrent(torrent);
        let timeframe_at_ms = start_of_today_ms(now_ms);
        let mut cache = self.track_cache.lock().unwrap_or_else(|e| e.into_inner());
        for peer in peers {
            if peer.is_handshaking() {
                continue;
            }
            let key = MetricsTrackKey {
                timeframe_at_ms,
                downloader: downloader_id.to_string(),
                torrent_id: torrent_id.clone(),
                address: compressed_ip(&peer.ip),
                port: peer.port,
            };
            let mut row = match cache.get(&key) {
                Some(cached) => cached.clone(),
                // 上游 cache-miss 时先按唯一键回查数据库，命中则复用该实体
                None => self
                    .sink
                    .load_metrics_track(&key)
                    .unwrap_or_else(|| MetricsTrackRow::new(key.clone())),
            };
            row.peer_id = peer.peer_id.clone();
            row.client_name = peer.client_name.clone();
            row.last_flags = lt_std_string_opt(peer.flags.as_deref());
            cache.insert(key, row);
        }
    }

    /// 对齐 `connectionMetricsTrackDao.flushAll()`：把缓存里的 track 行整体 upsert 入库
    /// （上游同样**不清空**缓存）。
    pub fn flush_all(&self) {
        let rows: Vec<MetricsTrackRow> = {
            let cache = self.track_cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.values().cloned().collect()
        };
        if !rows.is_empty() {
            self.sink.upsert_metrics_tracks(&rows);
        }
        // 对齐上游 PBHCache：flush 后仅保留当天时段的条目，避免跨天 key 持续累积导致无界增长
        // （留存条目已全量 upsert 入库，淘汰不丢数据）
        let today = start_of_today_ms(now_ms());
        self.track_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|k, _| k.timeframe_at_ms == today);
    }

    /// 对齐 `flushData()`。
    pub fn flush_data(&self, now_ms: i64) -> SessionFlushSummary {
        let mut summary = SessionFlushSummary::default();
        // 上游：`long deleted = 0; connectionMetricsTrackDao.flushAll();`
        self.flush_all();
        let start_of_today = start_of_today_ms(now_ms);

        let not_in_the_day = self.sink.list_metrics_tracks_not_at(start_of_today);
        let aggregated_not_in_the_day = aggregate_connection_metrics(&not_in_the_day);
        summary.aggregated_rows_out_of_day = aggregated_not_in_the_day.len();
        self.sink
            .save_connection_metrics(&aggregated_not_in_the_day, true);
        summary.saved_out_of_day = aggregated_not_in_the_day;
        summary.deleted += self.sink.delete_metrics_tracks(&not_in_the_day);

        let in_the_day = self.sink.list_metrics_tracks_at(start_of_today);
        let aggregated_in_the_day = aggregate_connection_metrics(&in_the_day);
        summary.aggregated_rows_in_day = aggregated_in_the_day.len();
        self.sink
            .save_connection_metrics(&aggregated_in_the_day, false);
        summary.saved_in_day = aggregated_in_the_day;
        summary.deleted += self.sink.delete_metrics_tracks(&in_the_day);

        summary
    }

    /// 对齐 `cleanup()`：`removeOutdatedData(now - data-retention-time)`（**闭区间** `le`）。
    ///
    /// 注意上游此处**没有** `dataRetentionTime <= 0` 保护（与 `PeerRecordingServiceModule` 不同）：
    /// 保留期为 0 时会删除 `timeframe_at <= now` 的全部数据。
    ///
    /// 上游把清理包在 `BackgroundTaskManager.addTaskAsync(...).join()` 里（文案
    /// `MODULE_PEER_ANALYSING_DELETING_EXPIRED_DATA`），语义上等价于同步执行；由调用方调度。
    pub fn cleanup(&self, now_ms: i64) -> usize {
        let before = now_ms - self.settings.data_retention_time_ms;
        self.sink.remove_connection_metrics_before(before)
    }

    /// 缓存条目数（对齐上游 cache 大小，供日志/测试）。
    pub fn cache_len(&self) -> usize {
        self.track_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

impl RuleModule for SessionAnalyseServiceModule {
    fn name(&self) -> &str {
        "Session Analyse Service"
    }

    fn config_name(&self) -> &str {
        "peer-analyse-service.session-analyse"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn check(
        &self,
        _downloader_id: &str,
        _torrent: &TorrentData,
        _peer: &PeerData,
        _ctx: &CheckContext,
    ) -> CheckResult {
        // 上游本模块没有 shouldBanPeer；pass()
        CheckResult::pass(self.config_name())
    }
}

use super::*;
