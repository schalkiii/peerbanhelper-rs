// ===========================================================================
// MonitorSink：pbh-core 看不到数据库，所有持久化操作都走这个注入点
// ===========================================================================

/// 监控模块需要的全部存储/告警操作。
///
/// 每个方法对齐上游一个 DAO 调用（SQL 语义与表结构见文件头 INTEGRATION SNIPPET 【3】）。
/// 方法**不返回错误**：上游在 DAO 调用外层 `catch (Throwable)` 后只记日志，
/// DB 实现内部记录失败即可，模块逻辑与上游一样继续执行。
pub trait MonitorSink: Send + Sync {
    /// 对齐 `TorrentService.createIfNotExists`：返回 torrent 主键。
    ///
    /// 内存实现直接返回 info hash；DB 实现返回 `torrents` 表自增主键
    /// （仅作分组键，不参与判定）。
    fn ensure_torrent(&self, torrent: &TorrentData) -> String;

    /// 对齐 `TrafficJournalService.updateData`（表 `traffic_journal_v3`，键 `(downloader, timestamp)`）。
    fn update_traffic_journal(
        &self,
        downloader: &str,
        overall_downloaded: i64,
        overall_uploaded: i64,
        overall_downloaded_protocol: i64,
        overall_uploaded_protocol: i64,
        now_ms: i64,
    );

    /// 对齐 `getAllDownloadersOverallData`（`None`）/ `getSpecificDownloaderOverallData`（`Some`），闭区间。
    fn query_traffic_overall(
        &self,
        downloader: Option<&str>,
        start_ms: i64,
        end_ms: i64,
    ) -> Vec<TrafficDataComputed>;

    /// 对齐 `AlertManager.identifierAlertExistsIncludeRead`（**包含已读**）。
    fn alert_exists_include_read(&self, identifier: &str) -> bool;

    /// 对齐 `AlertManager.publishAlert`。
    fn publish_alert(
        &self,
        push: bool,
        level: AlertLevel,
        identifier: &str,
        title: &TranslationComponent,
        description: &TranslationComponent,
    );

    /// 对齐 `PeerConnectionMetricsTrackService.syncPeers` 的 cache-miss 查询。
    fn load_metrics_track(&self, key: &MetricsTrackKey) -> Option<MetricsTrackRow>;

    /// 对齐 `PeerConnectionMetricsTrackMapper.upsert`（冲突键为 [`MetricsTrackKey`] 全部字段，
    /// UPDATE 只覆盖 `peer_id` / `client_name` / `last_flags`）。
    fn upsert_metrics_tracks(&self, rows: &[MetricsTrackRow]);

    /// 对齐 `list(eq(timeframe_at, t))`。
    fn list_metrics_tracks_at(&self, timeframe_at_ms: i64) -> Vec<MetricsTrackRow>;

    /// 对齐 `list(ne(timeframe_at, t))`。
    fn list_metrics_tracks_not_at(&self, timeframe_at_ms: i64) -> Vec<MetricsTrackRow>;

    /// 对齐 `deleteEntries`（按行删除，返回删除条数）。
    fn delete_metrics_tracks(&self, rows: &[MetricsTrackRow]) -> usize;

    /// 对齐 `PeerConnectionMetricsService.saveAggregating`。
    fn save_connection_metrics(&self, rows: &[ConnectionMetricsRow], overwrite: bool);

    /// 对齐 `removeOutdatedData`：删除 `timeframe_at <= before_ms`（**闭区间**），返回删除条数。
    fn remove_connection_metrics_before(&self, before_ms: i64) -> usize;

    /// 对齐 `PeerRecordService.flushToDatabase` → `PeerRecordMapper.upsert`。
    fn upsert_peer_record(&self, row: &PeerRecordRow);

    /// 对齐 `PeerRecordService.cleanup`：删除 `last_time_seen < before_ms`（**开区间**），返回删除条数。
    fn remove_peer_records_before(&self, before_ms: i64) -> usize;

    /// 对齐 `TrackedSwarmMapper.upsert`。
    fn upsert_tracked_swarm(&self, row: &TrackedSwarmRow);

    /// 对齐 `WHERE ... ORDER BY id DESC LIMIT 1` 的那条 selectOne。
    fn load_last_tracked_swarm(
        &self,
        ip: &str,
        port: u16,
        info_hash: &str,
        downloader: &str,
    ) -> Option<TrackedSwarmRow>;

    /// 对齐 `TrackedSwarmService.resetTable`（`DELETE FROM tracked_swarm`）。
    fn reset_tracked_swarm(&self);

    /// 对齐 `/api/modules/swarm-tracking` 的 `trackedSwarmDao.count()`。
    fn count_tracked_swarm(&self) -> usize;
}

/// 默认内存实现：语义与 DB 实现一致（冲突键、`<=`/`<` 边界、`MAX(0, ...)` 等），
/// 供测试与无数据库场景使用。
#[derive(Debug, Default)]
pub struct InMemoryMonitorSink {
    torrents: Mutex<HashMap<String, String>>,
    traffic: Mutex<Vec<TrafficJournalRow>>,
    alerts: Mutex<Vec<AlertRecord>>,
    tracks: Mutex<Vec<MetricsTrackRow>>,
    metrics: Mutex<Vec<ConnectionMetricsRow>>,
    peer_records: Mutex<Vec<PeerRecordRow>>,
    tracked_swarm: Mutex<Vec<TrackedSwarmRow>>,
    next_tracked_swarm_id: Mutex<i64>,
}

impl InMemoryMonitorSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前告警列表（按发布顺序，对齐 alerts 表内容）。
    pub fn alerts(&self) -> Vec<AlertRecord> {
        self.alerts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn traffic_journal(&self) -> Vec<TrafficJournalRow> {
        self.traffic
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn metrics_tracks(&self) -> Vec<MetricsTrackRow> {
        self.tracks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn connection_metrics(&self) -> Vec<ConnectionMetricsRow> {
        self.metrics
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn peer_records(&self) -> Vec<PeerRecordRow> {
        self.peer_records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn tracked_swarm(&self) -> Vec<TrackedSwarmRow> {
        self.tracked_swarm
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl MonitorSink for InMemoryMonitorSink {
    fn ensure_torrent(&self, torrent: &TorrentData) -> String {
        let mut torrents = self.torrents.lock().unwrap_or_else(|e| e.into_inner());
        torrents
            .entry(torrent.hash.clone())
            // 上游 createIfNotExists 对已存在 torrent 复用主键；内存实现以 info hash 当主键
            .or_insert_with(|| torrent.hash.clone())
            .clone()
    }

    fn update_traffic_journal(
        &self,
        downloader: &str,
        overall_downloaded: i64,
        overall_uploaded: i64,
        overall_downloaded_protocol: i64,
        overall_uploaded_protocol: i64,
        now_ms: i64,
    ) {
        let timestamp = start_of_hour_ms(now_ms);
        let mut rows = self.traffic.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(row) = rows
            .iter_mut()
            .find(|r| r.downloader == downloader && r.timestamp_ms == timestamp)
        {
            if row.data_overall_downloaded < overall_downloaded {
                row.data_overall_downloaded = overall_downloaded;
            }
            if row.data_overall_uploaded < overall_uploaded {
                row.data_overall_uploaded = overall_uploaded;
            }
            if row.protocol_overall_downloaded < overall_downloaded_protocol {
                row.protocol_overall_downloaded = overall_downloaded_protocol;
            }
            if row.protocol_overall_uploaded < overall_uploaded_protocol {
                row.protocol_overall_uploaded = overall_uploaded_protocol;
            }
            return;
        }
        // 新行沿用上游「先建行、再比较」的顺序：data_* 取 max(0, 传入值)，at_start 取传入值
        let mut row = TrafficJournalRow {
            downloader: downloader.to_string(),
            timestamp_ms: timestamp,
            data_overall_downloaded: 0,
            data_overall_uploaded: 0,
            data_overall_downloaded_at_start: overall_downloaded,
            data_overall_uploaded_at_start: overall_uploaded,
            protocol_overall_downloaded: 0,
            protocol_overall_uploaded: 0,
            protocol_overall_downloaded_at_start: overall_downloaded_protocol,
            protocol_overall_uploaded_at_start: overall_uploaded_protocol,
        };
        if row.data_overall_downloaded < overall_downloaded {
            row.data_overall_downloaded = overall_downloaded;
        }
        if row.data_overall_uploaded < overall_uploaded {
            row.data_overall_uploaded = overall_uploaded;
        }
        if row.protocol_overall_downloaded < overall_downloaded_protocol {
            row.protocol_overall_downloaded = overall_downloaded_protocol;
        }
        if row.protocol_overall_uploaded < overall_uploaded_protocol {
            row.protocol_overall_uploaded = overall_uploaded_protocol;
        }
        rows.push(row);
    }

    fn query_traffic_overall(
        &self,
        downloader: Option<&str>,
        start_ms: i64,
        end_ms: i64,
    ) -> Vec<TrafficDataComputed> {
        let rows = self.traffic.lock().unwrap_or_else(|e| e.into_inner());
        let in_range =
            |r: &&TrafficJournalRow| r.timestamp_ms >= start_ms && r.timestamp_ms <= end_ms;
        match downloader {
            None => {
                // selectAllDownloadersOverallData：GROUP BY timestamp，差值允许为负
                let mut grouped: BTreeMap<i64, (i64, i64, i64, i64)> = BTreeMap::new();
                for row in rows.iter().filter(in_range) {
                    let entry = grouped.entry(row.timestamp_ms).or_insert((0, 0, 0, 0));
                    entry.0 += row.data_overall_uploaded;
                    entry.1 += row.data_overall_uploaded_at_start;
                    entry.2 += row.data_overall_downloaded;
                    entry.3 += row.data_overall_downloaded_at_start;
                }
                grouped
                    .into_iter()
                    .map(
                        |(timestamp_ms, (ul, ul_start, dl, dl_start))| TrafficDataComputed {
                            timestamp_ms,
                            data_overall_uploaded: ul - ul_start,
                            data_overall_downloaded: dl - dl_start,
                        },
                    )
                    .collect()
            }
            Some(id) => rows
                .iter()
                .filter(in_range)
                .filter(|r| r.downloader == id)
                .map(|row| TrafficDataComputed {
                    timestamp_ms: row.timestamp_ms,
                    // selectSpecificDownloaderOverallData：逐行 MAX(0, 差值)
                    data_overall_uploaded: (row.data_overall_uploaded
                        - row.data_overall_uploaded_at_start)
                        .max(0),
                    data_overall_downloaded: (row.data_overall_downloaded
                        - row.data_overall_downloaded_at_start)
                        .max(0),
                })
                .collect(),
        }
    }

    fn alert_exists_include_read(&self, identifier: &str) -> bool {
        self.alerts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|a| a.identifier == identifier)
    }

    fn publish_alert(
        &self,
        push: bool,
        level: AlertLevel,
        identifier: &str,
        title: &TranslationComponent,
        description: &TranslationComponent,
    ) {
        self.alerts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(AlertRecord {
                push,
                level,
                identifier: identifier.to_string(),
                title: title.clone(),
                description: description.clone(),
            });
    }

    fn load_metrics_track(&self, key: &MetricsTrackKey) -> Option<MetricsTrackRow> {
        self.tracks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|r| &r.key == key)
            .cloned()
    }

    fn upsert_metrics_tracks(&self, rows: &[MetricsTrackRow]) {
        let mut tracks = self.tracks.lock().unwrap_or_else(|e| e.into_inner());
        for row in rows {
            match tracks.iter_mut().find(|r| r.key == row.key) {
                Some(existing) => {
                    // 冲突键命中时只更新这三列（对齐 upsert SQL）
                    existing.peer_id = row.peer_id.clone();
                    existing.client_name = row.client_name.clone();
                    existing.last_flags = row.last_flags.clone();
                }
                None => tracks.push(row.clone()),
            }
        }
    }

    fn list_metrics_tracks_at(&self, timeframe_at_ms: i64) -> Vec<MetricsTrackRow> {
        self.tracks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|r| r.key.timeframe_at_ms == timeframe_at_ms)
            .cloned()
            .collect()
    }

    fn list_metrics_tracks_not_at(&self, timeframe_at_ms: i64) -> Vec<MetricsTrackRow> {
        self.tracks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|r| r.key.timeframe_at_ms != timeframe_at_ms)
            .cloned()
            .collect()
    }

    fn delete_metrics_tracks(&self, rows: &[MetricsTrackRow]) -> usize {
        let keys: Vec<&MetricsTrackKey> = rows.iter().map(|r| &r.key).collect();
        let mut tracks = self.tracks.lock().unwrap_or_else(|e| e.into_inner());
        let before = tracks.len();
        tracks.retain(|r| !keys.contains(&&r.key));
        before - tracks.len()
    }

    fn save_connection_metrics(&self, rows: &[ConnectionMetricsRow], overwrite: bool) {
        let mut metrics = self.metrics.lock().unwrap_or_else(|e| e.into_inner());
        for row in rows {
            match metrics.iter_mut().find(|m| {
                m.timeframe_at_ms == row.timeframe_at_ms && m.downloader == row.downloader
            }) {
                Some(existing) => {
                    if overwrite {
                        *existing = row.clone();
                    } else {
                        existing.merge(row);
                    }
                }
                None => metrics.push(row.clone()),
            }
        }
    }

    fn remove_connection_metrics_before(&self, before_ms: i64) -> usize {
        let mut metrics = self.metrics.lock().unwrap_or_else(|e| e.into_inner());
        let before = metrics.len();
        // 上游 removeOutdatedData 用 `le`：闭区间
        metrics.retain(|m| m.timeframe_at_ms > before_ms);
        before - metrics.len()
    }

    fn upsert_peer_record(&self, row: &PeerRecordRow) {
        let mut records = self.peer_records.lock().unwrap_or_else(|e| e.into_inner());
        match records.iter_mut().find(|r| {
            r.address == row.address
                && r.torrent_id == row.torrent_id
                && r.downloader == row.downloader
        }) {
            Some(existing) => *existing = apply_peer_record_upsert(existing, row),
            None => records.push(row.clone()),
        }
    }

    fn remove_peer_records_before(&self, before_ms: i64) -> usize {
        let mut records = self.peer_records.lock().unwrap_or_else(|e| e.into_inner());
        let before = records.len();
        // 上游 cleanup 用 `lt`：开区间
        records.retain(|r| r.last_time_seen_ms >= before_ms);
        before - records.len()
    }

    fn upsert_tracked_swarm(&self, row: &TrackedSwarmRow) {
        let key = row.key();
        let mut rows = self.tracked_swarm.lock().unwrap_or_else(|e| e.into_inner());
        match rows.iter_mut().find(|r| r.key() == key) {
            Some(existing) => {
                // upsert SQL 覆盖除 id 外的全部列（含 first_time_seen）
                let id = existing.id;
                let mut stored = row.clone();
                stored.id = id;
                *existing = stored;
            }
            None => {
                let mut stored = row.clone();
                let mut next = self
                    .next_tracked_swarm_id
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                *next += 1;
                stored.id = Some(*next);
                rows.push(stored);
            }
        }
    }

    fn load_last_tracked_swarm(
        &self,
        ip: &str,
        port: u16,
        info_hash: &str,
        downloader: &str,
    ) -> Option<TrackedSwarmRow> {
        self.tracked_swarm
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|r| {
                r.ip == ip
                    && r.port == port
                    && r.info_hash == info_hash
                    && r.downloader == downloader
            })
            .max_by_key(|r| r.id.unwrap_or(0))
            .cloned()
    }

    fn reset_tracked_swarm(&self) {
        self.tracked_swarm
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn count_tracked_swarm(&self) -> usize {
        self.tracked_swarm
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }
}

/// 对齐上游 `PeerConnectionMetricsService.aggregating`：把 track 行按
/// `(timeframe_at, downloader)` 聚合为 metrics 行。
///
/// 分组缓冲按**输入顺序**线性查找（与上游 `findOrCreateBuffer` 一致）；
/// `last_flags == None` 时只累加 `total_connections`（上游 `flags == null` 分支），
/// `Some("")` 仍会进入 flags 分支（Java 中空串非 null）。
pub fn aggregate_connection_metrics(rows: &[MetricsTrackRow]) -> Vec<ConnectionMetricsRow> {
    let mut buffer: Vec<ConnectionMetricsRow> = Vec::new();
    for row in rows {
        let entity = match buffer.iter_mut().position(|m| {
            m.timeframe_at_ms == row.key.timeframe_at_ms && m.downloader == row.key.downloader
        }) {
            Some(index) => &mut buffer[index],
            None => {
                buffer.push(ConnectionMetricsRow {
                    timeframe_at_ms: row.key.timeframe_at_ms,
                    downloader: row.key.downloader.clone(),
                    ..Default::default()
                });
                buffer.last_mut().expect("刚插入的缓冲行")
            }
        };
        entity.total_connections += 1;
        let Some(flags) = &row.last_flags else {
            continue;
        };
        let f = PeerFlag::parse(flags);
        if !f.local_connection {
            entity.incoming_connections += 1;
        }
        if f.interesting && f.remote_choked {
            entity.remote_refuse_transfer_to_client += 1;
        }
        if f.interesting && !f.remote_choked {
            entity.remote_accept_transfer_to_client += 1;
        }
        if f.remote_interested && f.choked {
            entity.local_refuse_transfer_to_peer += 1;
        }
        if f.remote_interested && !f.choked {
            entity.local_accept_transfer_to_peer += 1;
        }
        if !f.remote_choked && !f.interesting {
            entity.local_not_interested += 1;
        }
        if !f.choked && !f.remote_interested {
            entity.question_status += 1;
        }
        if f.optimistic_unchoke {
            entity.optimistic_unchoke += 1;
        }
        if f.from_dht {
            entity.from_dht += 1;
        } else if f.from_pex {
            entity.from_pex += 1;
        } else if f.from_lsd {
            entity.from_lsd += 1;
        } else {
            entity.from_tracker_or_other += 1;
        }
        if f.rc4_encrypted {
            entity.rc4_encrypted += 1;
        }
        if f.plaintext_encrypted {
            entity.plain_text_encrypted += 1;
        }
        if f.utp_socket {
            entity.utp_socket += 1;
        } else {
            entity.tcp_socket += 1;
        }
    }
    buffer
}

use super::*;
