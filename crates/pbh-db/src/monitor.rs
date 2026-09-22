//! 监控模块的 SQLite 落库层（[`MonitorSink`] 的数据库实现）。
//!
//! 逐方法对齐上游 `databasent/service/impl/common/*ServiceImpl` 与
//! `resources/mapper/sqlite/*.xml`，表结构见 `schema.sql`：
//!
//! | `MonitorSink` 方法 | 上游 | 表 / 冲突键 |
//! | --- | --- | --- |
//! | [`DbMonitorSink::ensure_torrent`] | `TorrentServiceImpl.createIfNotExists` + `TorrentMapper.upsert` | `torrents` / `info_hash` |
//! | [`DbMonitorSink::update_traffic_journal`] | `TrafficJournalServiceImpl.updateData` | `traffic_journal_v3` / `(timestamp, downloader)` |
//! | [`DbMonitorSink::query_traffic_overall`] | `selectAllDownloadersOverallData` / `selectSpecificDownloaderOverallData` | 闭区间；聚合 `SUM` 允许负值，单下载器 `MAX(0, …)` |
//! | [`DbMonitorSink::alert_exists_include_read`] / [`DbMonitorSink::publish_alert`] | `AlertServiceImpl` / `AlertManagerImpl.publishAlert` | `alert` |
//! | [`DbMonitorSink::upsert_metrics_tracks`] 等 | `PeerConnectionMetricsTrackServiceImpl` | `peer_connection_metrics_track` / `(timeframe_at, downloader, torrent_id, address, port)` |
//! | [`DbMonitorSink::save_connection_metrics`] | `PeerConnectionMetricsServiceImpl.saveAggregating` | `peer_connection_metrics` / `(timeframe_at, downloader)` |
//! | [`DbMonitorSink::upsert_peer_record`] | `PeerRecordServiceImpl.flushToDatabase` + `PeerRecordMapper.upsert` | `peer_records` / `(address, torrent_id, downloader)` |
//! | [`DbMonitorSink::upsert_tracked_swarm`] 等 | `TrackedSwarmServiceImpl` | `tracked_swarm` / `(ip, port, info_hash, downloader)` |
//!
//! 约定（等价于上游 `catch (Throwable) + log`）：[`MonitorSink`] 的方法不返回错误，
//! 任何 SQL 失败都只记日志并继续，绝不 panic、绝不打断 ban wave。
//!
//! 与上游的差异（都不改变任何判定结果）：
//! - `delete_metrics_tracks`：上游 `deleteEntries` -> `deleteByIds`（按主键）逐条删，
//!   本移植的 [`MetricsTrackRow`] 不带主键，改为按唯一键逐条删（同样规避 #1518 的
//!   `SQLITE_TOOBIG`；唯一键与主键在该表上一一对应，语义等价）。
//! - `ensure_torrent` 失败时回退返回 info hash（与内存实现的键语义一致），上游此时抛异常。

use crate::Database;
use pbh_core::geoip::{GeoIpProvider, IpGeoData};
use pbh_core::i18n::TranslationComponent;
use pbh_core::model::TorrentData;
use pbh_core::modules::monitor::start_of_hour_ms;
use pbh_core::modules::{
    AlertLevel, ConnectionMetricsRow, MetricsTrackKey, MetricsTrackRow, MonitorSink, PeerRecordRow,
    TrackedSwarmRow, TrafficDataComputed,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;
use std::sync::{Arc, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::error;

/// 当前 Unix 毫秒时间戳（告警读写与 Web 端点共用）。
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `peer_records` 的列（读取顺序与 [`map_peer_record`] 一致）。
pub const PEER_RECORD_COLUMNS: &str = "address, port, torrent_id, downloader, peer_id, client_name, \
     uploaded, uploaded_offset, upload_speed, downloaded, downloaded_offset, download_speed, \
     last_flags, first_time_seen, last_time_seen, peer_geoip";

/// `peer_records` 的 upsert（逐字对齐 `resources/mapper/sqlite/PeerRecordMapper.xml`）：
/// 冲突键 `(address, torrent_id, downloader)`；`first_time_seen` 与 `peer_geoip` **永不更新**；
/// 其余列在 `excluded.last_time_seen < peer_records.last_time_seen` 时保留旧值。
const PEER_RECORD_UPSERT_SQL: &str = "INSERT INTO peer_records (
        address, port, torrent_id, downloader,
        peer_id, client_name,
        uploaded, uploaded_offset, upload_speed,
        downloaded, downloaded_offset, download_speed,
        last_flags, first_time_seen, last_time_seen,
        peer_geoip
    )
    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
    ON CONFLICT(address, torrent_id, downloader) DO UPDATE SET
    uploaded = CASE
        WHEN excluded.last_time_seen < peer_records.last_time_seen THEN peer_records.uploaded
        WHEN (excluded.uploaded - peer_records.uploaded_offset) < 0
        THEN peer_records.uploaded + excluded.uploaded
        ELSE peer_records.uploaded + excluded.uploaded - peer_records.uploaded_offset
    END,
    downloaded = CASE
        WHEN excluded.last_time_seen < peer_records.last_time_seen THEN peer_records.downloaded
        WHEN (excluded.downloaded - peer_records.downloaded_offset) < 0
        THEN peer_records.downloaded + excluded.downloaded
        ELSE peer_records.downloaded + excluded.downloaded - peer_records.downloaded_offset
    END,
    uploaded_offset = CASE
        WHEN excluded.last_time_seen < peer_records.last_time_seen
        THEN peer_records.uploaded_offset
        ELSE excluded.uploaded
    END,
    downloaded_offset = CASE
        WHEN excluded.last_time_seen < peer_records.last_time_seen
        THEN peer_records.downloaded_offset
        ELSE excluded.downloaded
    END,
    upload_speed = CASE
        WHEN excluded.last_time_seen < peer_records.last_time_seen
        THEN peer_records.upload_speed
        ELSE excluded.upload_speed
    END,
    download_speed = CASE
        WHEN excluded.last_time_seen < peer_records.last_time_seen
        THEN peer_records.download_speed
        ELSE excluded.download_speed
    END,
    port = CASE
        WHEN excluded.last_time_seen < peer_records.last_time_seen
        THEN peer_records.port
        ELSE excluded.port
    END,
    peer_id = CASE
        WHEN excluded.last_time_seen < peer_records.last_time_seen
        THEN peer_records.peer_id
        ELSE excluded.peer_id
    END,
    client_name = CASE
        WHEN excluded.last_time_seen < peer_records.last_time_seen
        THEN peer_records.client_name
        ELSE excluded.client_name
    END,
    last_flags = CASE
        WHEN excluded.last_time_seen < peer_records.last_time_seen
        THEN peer_records.last_flags
        ELSE excluded.last_flags
    END,
    last_time_seen = CASE
        WHEN excluded.last_time_seen < peer_records.last_time_seen
        THEN peer_records.last_time_seen
        ELSE excluded.last_time_seen
    END";

/// `tracked_swarm` 的 upsert（逐字对齐 `resources/mapper/sqlite/TrackedSwarmMapper.xml`）：
/// 冲突键 `(ip, port, info_hash, downloader)`，UPDATE 覆盖除 `id` 与冲突键外的**全部**列
/// （含 `first_time_seen`）。
const TRACKED_SWARM_UPSERT_SQL: &str = "INSERT INTO tracked_swarm (
        ip, port, info_hash, torrent_is_private, torrent_size,
        downloader, downloader_progress, peer_id, client_name,
        peer_progress, uploaded, uploaded_offset, upload_speed,
        downloaded, downloaded_offset, download_speed, last_flags,
        first_time_seen, last_time_seen, download_speed_max, upload_speed_max)
    VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)
    ON CONFLICT(ip, port, info_hash, downloader) DO UPDATE SET
        torrent_is_private = excluded.torrent_is_private,
        torrent_size = excluded.torrent_size,
        downloader_progress = excluded.downloader_progress,
        peer_id = excluded.peer_id,
        client_name = excluded.client_name,
        peer_progress = excluded.peer_progress,
        uploaded = excluded.uploaded,
        uploaded_offset = excluded.uploaded_offset,
        upload_speed = excluded.upload_speed,
        downloaded = excluded.downloaded,
        downloaded_offset = excluded.downloaded_offset,
        download_speed = excluded.download_speed,
        last_flags = excluded.last_flags,
        first_time_seen = excluded.first_time_seen,
        last_time_seen = excluded.last_time_seen,
        download_speed_max = excluded.download_speed_max,
        upload_speed_max = excluded.upload_speed_max";

/// `tracked_swarm` 的读取列（顺序与 [`map_tracked_swarm`] 一致）。
const TRACKED_SWARM_COLUMNS: &str = "id, ip, port, info_hash, torrent_is_private, torrent_size, \
     downloader, downloader_progress, peer_id, client_name, peer_progress, uploaded, \
     uploaded_offset, upload_speed, downloaded, downloaded_offset, download_speed, last_flags, \
     first_time_seen, last_time_seen, download_speed_max, upload_speed_max";

/// `tracked_swarm` 允许排序的列（对齐 `Orderable` 只用表自身的列名）。
pub const TRACKED_SWARM_ORDER_COLUMNS: &[&str] = &[
    "id",
    "ip",
    "port",
    "info_hash",
    "torrent_is_private",
    "torrent_size",
    "downloader",
    "downloader_progress",
    "peer_id",
    "client_name",
    "peer_progress",
    "uploaded",
    "uploaded_offset",
    "upload_speed",
    "downloaded",
    "downloaded_offset",
    "download_speed",
    "last_flags",
    "first_time_seen",
    "last_time_seen",
    "download_speed_max",
    "upload_speed_max",
];

/// `peer_connection_metrics` 的计数列（对齐 `PeerConnectionMetricsEntity` 的字段顺序）。
const CONNECTION_METRICS_COLUMNS: &[&str] = &[
    "total_connections",
    "incoming_connections",
    "remote_refuse_transfer_to_client",
    "remote_accept_transfer_to_client",
    "local_refuse_transfer_to_peer",
    "local_accept_transfer_to_peer",
    "local_not_interested",
    "question_status",
    "optimistic_unchoke",
    "from_dht",
    "from_pex",
    "from_lsd",
    "from_tracker_or_other",
    "rc4_encrypted",
    "plain_text_encrypted",
    "utp_socket",
    "tcp_socket",
];

/// `alert` 表的一行（供 Web API `/api/alerts` 读取）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlertRow {
    pub id: i64,
    pub create_at_ms: i64,
    /// 未读为 `None`（上游 `AlertEntity.readAt`）
    pub read_at_ms: Option<i64>,
    /// 上游 `AlertLevel` 的枚举名（`TIP` / `INFO` / `WARN` / `ERROR` / `FATAL`）
    pub level: String,
    pub identifier: String,
    /// `TranslationComponent` 的 JSON（对齐 `TranslationComponentTypeHandler`）
    pub title: String,
    pub content: String,
}

/// `MonitorSink` 的 SQLite 实现：与 `Database` 共享同一份连接。
///
/// `geo` 为可选的 IP 库查询器，用于填充 `peer_records.peer_geoip`
/// （对齐上游在 `PeerRecordServiceImpl` 内调 `IPDBManager.queryIPDB`）。
pub struct DbMonitorSink {
    db: Arc<Database>,
    geo: Option<Arc<dyn GeoIpProvider>>,
}

impl DbMonitorSink {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db, geo: None }
    }

    /// 注入 IP 库（缺省 = 上游 `IPDBManager.ipdb == null`：`peer_geoip` 落 NULL）。
    pub fn with_geo(db: Arc<Database>, geo: Option<Arc<dyn GeoIpProvider>>) -> Self {
        Self { db, geo }
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.db.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `TorrentService.createIfNotExists`：返回 `torrents` 表主键（十进制字符串）。
    fn ensure_torrent_id(&self, torrent: &TorrentData) -> anyhow::Result<String> {
        let conn = self.conn();
        if let Some(existing) = select_torrent(&conn, &torrent.hash)? {
            // 已存在且（记录完整 或 传入数据更差）-> 直接复用，不回写
            let existing_is_complete = existing.size > 0 && existing.private_torrent.is_some();
            let incoming_is_poor = torrent.total_size <= 0 && torrent.is_private.is_none();
            if existing_is_complete || incoming_is_poor {
                return Ok(existing.id.to_string());
            }
        }
        // 对齐 `TorrentMapper.upsert`：只有 size / private_torrent 会被条件式回填，name 不回写
        conn.execute(
            "INSERT INTO torrents (info_hash, name, size, private_torrent)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(info_hash) DO UPDATE SET
               size = CASE WHEN torrents.size <= 0 THEN excluded.size ELSE torrents.size END,
               private_torrent = CASE
                 WHEN torrents.private_torrent IS NULL THEN excluded.private_torrent
                 ELSE torrents.private_torrent END",
            params![
                torrent.hash,
                torrent.name,
                torrent.total_size,
                torrent.is_private.map(i64::from),
            ],
        )?;
        select_torrent(&conn, &torrent.hash)?
            .map(|row| row.id.to_string())
            .ok_or_else(|| anyhow::anyhow!("torrents upsert 后取不到行: {}", torrent.hash))
    }

    /// `TrafficJournalService.updateData`。
    ///
    /// 上游是「先 select 再比较」：新建行 `*_at_start` = 传入值、`data_*` = `max(0, 传入值)`；
    /// 已存在行 `data_*` 只在传入值更大时抬高、`*_at_start` 永不更新。
    /// 等价 SQL：`data_* = MAX(现有值, MAX(0, 传入值))`（新行的现有值为 0）。
    fn update_traffic_journal_inner(
        &self,
        downloader: &str,
        overall_downloaded: i64,
        overall_uploaded: i64,
        overall_downloaded_protocol: i64,
        overall_uploaded_protocol: i64,
        now_ms: i64,
    ) -> anyhow::Result<()> {
        let timestamp = start_of_hour_ms(now_ms);
        let conn = self.conn();
        conn.execute(
            "INSERT INTO traffic_journal_v3 (
                timestamp, downloader,
                data_overall_downloaded_at_start, data_overall_downloaded,
                data_overall_uploaded_at_start, data_overall_uploaded,
                protocol_overall_downloaded_at_start, protocol_overall_downloaded,
                protocol_overall_uploaded_at_start, protocol_overall_uploaded)
             VALUES (?1, ?2, ?3, MAX(0, ?3), ?4, MAX(0, ?4), ?5, MAX(0, ?5), ?6, MAX(0, ?6))
             ON CONFLICT(timestamp, downloader) DO UPDATE SET
               data_overall_downloaded = MAX(
                 traffic_journal_v3.data_overall_downloaded, excluded.data_overall_downloaded),
               data_overall_uploaded = MAX(
                 traffic_journal_v3.data_overall_uploaded, excluded.data_overall_uploaded),
               protocol_overall_downloaded = MAX(
                 traffic_journal_v3.protocol_overall_downloaded, excluded.protocol_overall_downloaded),
               protocol_overall_uploaded = MAX(
                 traffic_journal_v3.protocol_overall_uploaded, excluded.protocol_overall_uploaded)",
            params![
                timestamp,
                downloader,
                overall_downloaded,
                overall_uploaded,
                overall_downloaded_protocol,
                overall_uploaded_protocol,
            ],
        )?;
        Ok(())
    }

    /// 对齐 `selectAllDownloadersOverallData`（`GROUP BY timestamp`，差值可为负）。
    fn query_all_downloaders_overall(
        &self,
        start_ms: i64,
        end_ms: i64,
    ) -> anyhow::Result<Vec<TrafficDataComputed>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT timestamp,
                    SUM(data_overall_uploaded) - SUM(data_overall_uploaded_at_start),
                    SUM(data_overall_downloaded) - SUM(data_overall_downloaded_at_start)
             FROM traffic_journal_v3
             WHERE timestamp >= ?1 AND timestamp <= ?2
             GROUP BY timestamp",
        )?;
        let rows = stmt.query_map(params![start_ms, end_ms], map_traffic_data)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 对齐 `selectSpecificDownloaderOverallData`（逐行 `MAX(0, 差值)`）。
    fn query_specific_downloader_overall(
        &self,
        downloader: &str,
        start_ms: i64,
        end_ms: i64,
    ) -> anyhow::Result<Vec<TrafficDataComputed>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT timestamp,
                    MAX(0, data_overall_uploaded - data_overall_uploaded_at_start),
                    MAX(0, data_overall_downloaded - data_overall_downloaded_at_start)
             FROM traffic_journal_v3
             WHERE downloader = ?1 AND timestamp >= ?2 AND timestamp <= ?3",
        )?;
        let rows = stmt.query_map(params![downloader, start_ms, end_ms], map_traffic_data)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 对齐 `AlertManagerImpl.publishAlert` 的落库部分。
    ///
    /// 上游 `alert` 表只有 `level / identifier / title / content / create_at / read_at`，
    /// **没有** `push` 列：`push` 只决定是否走推送渠道（应用层职责），不落库。
    fn publish_alert_inner(
        &self,
        level: AlertLevel,
        identifier: &str,
        title: &TranslationComponent,
        description: &TranslationComponent,
    ) -> anyhow::Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO alert (create_at, read_at, level, identifier, title, content)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5)",
            params![
                crate::now_ms(),
                alert_level_name(level),
                identifier,
                serde_json::to_string(title)?,
                serde_json::to_string(description)?,
            ],
        )?;
        Ok(())
    }

    fn upsert_metrics_tracks_inner(&self, rows: &[MetricsTrackRow]) -> anyhow::Result<usize> {
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO peer_connection_metrics_track (
                    timeframe_at, downloader, torrent_id, address, port, peer_id, client_name, last_flags)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT (timeframe_at, downloader, torrent_id, address, port) DO UPDATE SET
                   peer_id = EXCLUDED.peer_id,
                   client_name = EXCLUDED.client_name,
                   last_flags = EXCLUDED.last_flags",
            )?;
            for row in rows {
                written += stmt.execute(params![
                    row.key.timeframe_at_ms,
                    row.key.downloader,
                    row.key.torrent_id,
                    row.key.address,
                    row.key.port as i64,
                    row.peer_id,
                    row.client_name,
                    row.last_flags,
                ])?;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// `list(eq(timeframe_at, t))` / `list(ne(timeframe_at, t))`。
    fn list_metrics_tracks(
        &self,
        timeframe_at_ms: i64,
        eq: bool,
    ) -> anyhow::Result<Vec<MetricsTrackRow>> {
        let conn = self.conn();
        let sql = format!(
            "SELECT timeframe_at, downloader, torrent_id, address, port, peer_id, client_name, last_flags
             FROM peer_connection_metrics_track WHERE timeframe_at {} ?1",
            if eq { "=" } else { "!=" }
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![timeframe_at_ms], map_metrics_track)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn delete_metrics_tracks_inner(&self, rows: &[MetricsTrackRow]) -> anyhow::Result<usize> {
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let mut deleted = 0usize;
        {
            // 逐条删除（对应上游 `deleteByIds`，不用 `IN (...)` 批量删除以规避 #1518 的 SQLITE_TOOBIG）
            let mut stmt = tx.prepare(
                "DELETE FROM peer_connection_metrics_track
                 WHERE timeframe_at = ?1 AND downloader = ?2 AND torrent_id = ?3
                   AND address = ?4 AND port = ?5",
            )?;
            for row in rows {
                deleted += stmt.execute(params![
                    row.key.timeframe_at_ms,
                    row.key.downloader,
                    row.key.torrent_id,
                    row.key.address,
                    row.key.port as i64,
                ])?;
            }
        }
        tx.commit()?;
        Ok(deleted)
    }

    fn save_connection_metrics_inner(
        &self,
        rows: &[ConnectionMetricsRow],
        overwrite: bool,
    ) -> anyhow::Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let conn = self.conn();
        let tx = conn.unchecked_transaction()?;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare(&connection_metrics_upsert_sql(overwrite))?;
            for row in rows {
                written += stmt.execute(params![
                    row.timeframe_at_ms,
                    row.downloader,
                    row.total_connections,
                    row.incoming_connections,
                    row.remote_refuse_transfer_to_client,
                    row.remote_accept_transfer_to_client,
                    row.local_refuse_transfer_to_peer,
                    row.local_accept_transfer_to_peer,
                    row.local_not_interested,
                    row.question_status,
                    row.optimistic_unchoke,
                    row.from_dht,
                    row.from_pex,
                    row.from_lsd,
                    row.from_tracker_or_other,
                    row.rc4_encrypted,
                    row.plain_text_encrypted,
                    row.utp_socket,
                    row.tcp_socket,
                ])?;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// `peer_geoip` 的取值：行内已有值优先，其次按 IP 查 IP 库（对齐上游在 DAO 内查询），
    /// 都取不到时落 NULL。
    fn resolve_peer_geoip(&self, row: &PeerRecordRow) -> Option<String> {
        if row.peer_geoip.is_some() {
            return row.peer_geoip.clone();
        }
        let provider = self.geo.as_ref()?;
        let address = pbh_core::iputil::parse_addr(&row.address)?;
        provider.query(address).as_ref().map(peer_geoip_json)
    }

    fn upsert_peer_record_inner(&self, row: &PeerRecordRow) -> anyhow::Result<usize> {
        let peer_geoip = self.resolve_peer_geoip(row);
        let conn = self.conn();
        Ok(conn.execute(
            PEER_RECORD_UPSERT_SQL,
            params![
                row.address,
                row.port as i64,
                row.torrent_id,
                row.downloader,
                row.peer_id,
                row.client_name,
                row.uploaded,
                row.uploaded_offset,
                row.upload_speed,
                row.downloaded,
                row.downloaded_offset,
                row.download_speed,
                row.last_flags,
                row.first_time_seen_ms,
                row.last_time_seen_ms,
                peer_geoip,
            ],
        )?)
    }

    fn upsert_tracked_swarm_inner(&self, row: &TrackedSwarmRow) -> anyhow::Result<usize> {
        let conn = self.conn();
        Ok(conn.execute(
            TRACKED_SWARM_UPSERT_SQL,
            params![
                row.ip,
                row.port as i64,
                row.info_hash,
                row.torrent_is_private.map(i64::from),
                row.torrent_size,
                row.downloader,
                row.downloader_progress,
                row.peer_id,
                row.client_name,
                row.peer_progress,
                row.uploaded,
                row.uploaded_offset,
                row.upload_speed,
                row.downloaded,
                row.downloaded_offset,
                row.download_speed,
                row.last_flags,
                row.first_time_seen_ms,
                row.last_time_seen_ms,
                row.download_speed_max,
                row.upload_speed_max,
            ],
        )?)
    }

    fn load_last_tracked_swarm_inner(
        &self,
        ip: &str,
        port: u16,
        info_hash: &str,
        downloader: &str,
    ) -> anyhow::Result<Option<TrackedSwarmRow>> {
        let conn = self.conn();
        let sql = format!(
            "SELECT {TRACKED_SWARM_COLUMNS} FROM tracked_swarm
             WHERE ip = ?1 AND port = ?2 AND info_hash = ?3 AND downloader = ?4
             ORDER BY id DESC LIMIT 1"
        );
        let mut stmt = conn.prepare(&sql)?;
        Ok(stmt
            .query_row(
                params![ip, port as i64, info_hash, downloader],
                map_tracked_swarm,
            )
            .optional()?)
    }
}

impl MonitorSink for DbMonitorSink {
    fn ensure_torrent(&self, torrent: &TorrentData) -> String {
        match self.ensure_torrent_id(torrent) {
            Ok(id) => id,
            Err(e) => {
                error!("监控数据落库失败（torrents upsert, {}）: {e}", torrent.hash);
                // 上游此处抛异常；本移植回退为 info hash，保持分组键可用（与内存实现一致）
                torrent.hash.clone()
            }
        }
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
        if let Err(e) = self.update_traffic_journal_inner(
            downloader,
            overall_downloaded,
            overall_uploaded,
            overall_downloaded_protocol,
            overall_uploaded_protocol,
            now_ms,
        ) {
            error!("监控数据落库失败（traffic_journal_v3, 下载器 {downloader}）: {e}");
        }
    }

    fn query_traffic_overall(
        &self,
        downloader: Option<&str>,
        start_ms: i64,
        end_ms: i64,
    ) -> Vec<TrafficDataComputed> {
        let result = match downloader {
            None => self.query_all_downloaders_overall(start_ms, end_ms),
            Some(id) => self.query_specific_downloader_overall(id, start_ms, end_ms),
        };
        match result {
            Ok(rows) => rows,
            Err(e) => {
                error!("监控数据查询失败（traffic_journal_v3 流量汇总）: {e}");
                Vec::new()
            }
        }
    }

    fn alert_exists_include_read(&self, identifier: &str) -> bool {
        // 无论是否已读（`AlertServiceImpl.identifierAlertExistsIncludeRead`）
        let conn = self.conn();
        let exists: rusqlite::Result<i64> = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM alert WHERE identifier = ?1)",
            params![identifier],
            |row| row.get(0),
        );
        match exists {
            Ok(found) => found != 0,
            Err(e) => {
                error!("监控数据查询失败（alert 按 identifier 判存）: {e}");
                false
            }
        }
    }

    fn publish_alert(
        &self,
        _push: bool,
        level: AlertLevel,
        identifier: &str,
        title: &TranslationComponent,
        description: &TranslationComponent,
    ) {
        if let Err(e) = self.publish_alert_inner(level, identifier, title, description) {
            error!("监控告警落库失败（alert, identifier={identifier}）: {e}");
        }
    }

    fn load_metrics_track(&self, key: &MetricsTrackKey) -> Option<MetricsTrackRow> {
        let conn = self.conn();
        let result: rusqlite::Result<Option<MetricsTrackRow>> = (|| {
            let mut stmt = conn.prepare(
                "SELECT timeframe_at, downloader, torrent_id, address, port, peer_id, client_name, last_flags
                 FROM peer_connection_metrics_track
                 WHERE timeframe_at = ?1 AND downloader = ?2 AND torrent_id = ?3
                   AND address = ?4 AND port = ?5
                 LIMIT 1",
            )?;
            stmt.query_row(
                params![
                    key.timeframe_at_ms,
                    key.downloader,
                    key.torrent_id,
                    key.address,
                    key.port as i64
                ],
                map_metrics_track,
            )
            .optional()
        })();
        match result {
            Ok(row) => row,
            Err(e) => {
                error!("监控数据查询失败（peer_connection_metrics_track 唯一键回查）: {e}");
                None
            }
        }
    }

    fn upsert_metrics_tracks(&self, rows: &[MetricsTrackRow]) {
        if rows.is_empty() {
            return;
        }
        if let Err(e) = self.upsert_metrics_tracks_inner(rows) {
            error!(
                "监控数据落库失败（peer_connection_metrics_track upsert {} 行）: {e}",
                rows.len()
            );
        }
    }

    fn list_metrics_tracks_at(&self, timeframe_at_ms: i64) -> Vec<MetricsTrackRow> {
        match self.list_metrics_tracks(timeframe_at_ms, true) {
            Ok(rows) => rows,
            Err(e) => {
                error!("监控数据查询失败（peer_connection_metrics_track eq 时间窗）: {e}");
                Vec::new()
            }
        }
    }

    fn list_metrics_tracks_not_at(&self, timeframe_at_ms: i64) -> Vec<MetricsTrackRow> {
        match self.list_metrics_tracks(timeframe_at_ms, false) {
            Ok(rows) => rows,
            Err(e) => {
                error!("监控数据查询失败（peer_connection_metrics_track ne 时间窗）: {e}");
                Vec::new()
            }
        }
    }

    fn delete_metrics_tracks(&self, rows: &[MetricsTrackRow]) -> usize {
        if rows.is_empty() {
            return 0;
        }
        match self.delete_metrics_tracks_inner(rows) {
            Ok(deleted) => deleted,
            Err(e) => {
                error!(
                    "监控数据清理失败（peer_connection_metrics_track 删除 {} 行）: {e}",
                    rows.len()
                );
                0
            }
        }
    }

    fn save_connection_metrics(&self, rows: &[ConnectionMetricsRow], overwrite: bool) {
        if let Err(e) = self.save_connection_metrics_inner(rows, overwrite) {
            error!(
                "监控数据落库失败（peer_connection_metrics 聚合 {} 行）: {e}",
                rows.len()
            );
        }
    }

    fn remove_connection_metrics_before(&self, before_ms: i64) -> usize {
        // 上游 `removeOutdatedData` 用 `le`：**闭区间**
        let conn = self.conn();
        match conn.execute(
            "DELETE FROM peer_connection_metrics WHERE timeframe_at <= ?1",
            params![before_ms],
        ) {
            Ok(deleted) => deleted,
            Err(e) => {
                error!("监控数据清理失败（peer_connection_metrics 过期删除）: {e}");
                0
            }
        }
    }

    fn upsert_peer_record(&self, row: &PeerRecordRow) {
        if let Err(e) = self.upsert_peer_record_inner(row) {
            error!(
                "监控数据落库失败（peer_records upsert, {}@{}）: {e}",
                row.downloader, row.address
            );
        }
    }

    fn remove_peer_records_before(&self, before_ms: i64) -> usize {
        // 上游 `PeerRecordService.cleanup` 用 `lt`：**开区间**
        let conn = self.conn();
        match conn.execute(
            "DELETE FROM peer_records WHERE last_time_seen < ?1",
            params![before_ms],
        ) {
            Ok(deleted) => deleted,
            Err(e) => {
                error!("监控数据清理失败（peer_records 过期删除）: {e}");
                0
            }
        }
    }

    fn upsert_tracked_swarm(&self, row: &TrackedSwarmRow) {
        if let Err(e) = self.upsert_tracked_swarm_inner(row) {
            error!(
                "监控数据落库失败（tracked_swarm upsert, {}:{}）: {e}",
                row.ip, row.port
            );
        }
    }

    fn load_last_tracked_swarm(
        &self,
        ip: &str,
        port: u16,
        info_hash: &str,
        downloader: &str,
    ) -> Option<TrackedSwarmRow> {
        match self.load_last_tracked_swarm_inner(ip, port, info_hash, downloader) {
            Ok(row) => row,
            Err(e) => {
                error!("监控数据查询失败（tracked_swarm 取最近一行）: {e}");
                None
            }
        }
    }

    fn reset_tracked_swarm(&self) {
        let conn = self.conn();
        if let Err(e) = conn.execute("DELETE FROM tracked_swarm", []) {
            error!("监控数据清理失败（tracked_swarm 整表清空）: {e}");
        }
    }

    fn count_tracked_swarm(&self) -> usize {
        match self.db.tracked_swarm_count() {
            Ok(count) => count.max(0) as usize,
            Err(e) => {
                error!("监控数据查询失败（tracked_swarm 计数）: {e}");
                0
            }
        }
    }
}

/// Web API 读取侧（`pbh-web` 直接读 `tracked_swarm` 与 `alert`）。
impl Database {
    /// `/api/modules/swarm-tracking` 的 `trackedSwarmDao.count()`。
    pub fn tracked_swarm_count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn.query_row("SELECT COUNT(*) FROM tracked_swarm", [], |row| row.get(0))?)
    }

    /// `/api/modules/swarm-tracking/details` 的 `TrackedSwarmService.page`。
    ///
    /// `order_by` 为空时与上游一致（不带 `ORDER BY`）；列名按 [`TRACKED_SWARM_ORDER_COLUMNS`]
    /// 白名单校验（对齐 `Orderable#apply` + `SQLHelper.checkSafeFieldName`，非法字段 -> 报错/500）。
    pub fn page_tracked_swarm(
        &self,
        order_by: &[(String, bool)],
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<(Vec<TrackedSwarmRow>, i64)> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let order_clause = build_order_by(order_by)?;
        let total: i64 =
            conn.query_row("SELECT COUNT(*) FROM tracked_swarm", [], |row| row.get(0))?;
        let sql = format!(
            "SELECT {TRACKED_SWARM_COLUMNS} FROM tracked_swarm{order_clause} LIMIT ?1 OFFSET ?2"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![limit, offset], map_tracked_swarm)?;
        Ok((rows.collect::<Result<Vec<_>, _>>()?, total))
    }

    /// `/api/alerts` 的 `AlertServiceImpl.getUnreadAlerts`（`read_at IS NULL`，创建时间倒序）。
    pub fn list_unread_alerts(&self) -> anyhow::Result<Vec<AlertRow>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, create_at, read_at, level, identifier, title, content
             FROM alert WHERE read_at IS NULL ORDER BY create_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(AlertRow {
                id: row.get(0)?,
                create_at_ms: row.get(1)?,
                read_at_ms: row.get(2)?,
                level: row.get(3)?,
                identifier: row.get(4)?,
                title: row.get(5)?,
                content: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// `/api/alert/{id}/dismiss` 的 `AlertService.getById`；不存在返回 `None`。
    pub fn get_alert_by_id(&self, id: i64) -> anyhow::Result<Option<AlertRow>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, create_at, read_at, level, identifier, title, content
             FROM alert WHERE id=?1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![id], |row| {
            Ok(AlertRow {
                id: row.get(0)?,
                create_at_ms: row.get(1)?,
                read_at_ms: row.get(2)?,
                level: row.get(3)?,
                identifier: row.get(4)?,
                title: row.get(5)?,
                content: row.get(6)?,
            })
        })?;
        rows.next().transpose().map_err(Into::into)
    }

    /// `/api/alert/{id}/dismiss`：标记单条已读（`setReadAt` + `saveOrUpdate`）。
    pub fn mark_alert_read(&self, id: i64) -> anyhow::Result<()> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE alert SET read_at=?1 WHERE id=?2 AND read_at IS NULL",
            rusqlite::params![now, id],
        )?;
        Ok(())
    }

    /// `/api/alert/dismissAll` 的 `AlertService.markAllAsRead`。
    pub fn mark_all_alerts_read(&self) -> anyhow::Result<usize> {
        let now = now_ms();
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn.execute(
            "UPDATE alert SET read_at=?1 WHERE read_at IS NULL",
            rusqlite::params![now],
        )?)
    }

    /// `/api/alert/{id}` 的 `AlertService.removeById`。
    pub fn delete_alert_by_id(&self, id: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute("DELETE FROM alert WHERE id=?1", rusqlite::params![id])?;
        Ok(())
    }
}

/// 对齐 `Orderable#generateOrderBy`：`field|asc` / `field|desc`（缺省 asc）；空列表不带排序。
fn build_order_by(order_by: &[(String, bool)]) -> anyhow::Result<String> {
    if order_by.is_empty() {
        return Ok(String::new());
    }
    let mut parts = Vec::with_capacity(order_by.len());
    for (field, asc) in order_by {
        let column = safe_order_column(field)?;
        parts.push(format!("{column} {}", if *asc { "ASC" } else { "DESC" }));
    }
    Ok(format!(" ORDER BY {}", parts.join(", ")))
}

/// 校验排序列：对齐 `SQLHelper.checkSafeFieldName` 的字段名模式
/// `^[a-zA-Z0-9_]+(\.[a-zA-Z0-9_]+)*$`，并限定为 `tracked_swarm` 自身的列。
fn safe_order_column(field: &str) -> anyhow::Result<&'static str> {
    let bare = field.strip_prefix("tracked_swarm.").unwrap_or(field);
    if bare.is_empty() || !bare.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        anyhow::bail!("非法的排序列: {field}");
    }
    TRACKED_SWARM_ORDER_COLUMNS
        .iter()
        .find(|column| **column == bare)
        .copied()
        .ok_or_else(|| anyhow::anyhow!("tracked_swarm 不存在排序列: {field}"))
}

/// 对齐 `PeerConnectionMetricsServiceImpl.saveAggregating` 的 upsert SQL：
/// `overwrite` = 整行覆盖（保留主键，含 `local_not_interested`）；否则累加合并，
/// 且**与上游 `merge()` 一致地漏掉 `local_not_interested`**。
fn connection_metrics_upsert_sql(overwrite: bool) -> String {
    let columns = CONNECTION_METRICS_COLUMNS.join(", ");
    let placeholders = (3..CONNECTION_METRICS_COLUMNS.len() + 3)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let assignments = CONNECTION_METRICS_COLUMNS
        .iter()
        .filter(|column| overwrite || **column != "local_not_interested")
        .map(|column| {
            if overwrite {
                format!("{column} = excluded.{column}")
            } else {
                format!("{column} = peer_connection_metrics.{column} + excluded.{column}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "INSERT INTO peer_connection_metrics (timeframe_at, downloader, {columns})
         VALUES (?1, ?2, {placeholders})
         ON CONFLICT(timeframe_at, downloader) DO UPDATE SET {assignments}"
    )
}

/// 对齐 Gson `AlertLevel` 的枚举名（`TIP` / `INFO` / `WARN` / `ERROR` / `FATAL`）。
fn alert_level_name(level: AlertLevel) -> &'static str {
    match level {
        AlertLevel::Tip => "TIP",
        AlertLevel::Info => "INFO",
        AlertLevel::Warn => "WARN",
        AlertLevel::Error => "ERROR",
        AlertLevel::Fatal => "FATAL",
    }
}

/// `peer_geoip` 的 JSON：对齐 `BasicJsonTypeHandler`（`JsonUtil.standard()` = Gson +
/// `@JsonUtil.Hidden` 字段排除 + `serializeNulls`），只保留未标注 `@JsonUtil.Hidden` 的字段：
/// `city.name` / `country.iso` / `as.number` / `network.isp` / `network.netType`，null 也输出。
fn peer_geoip_json(geo: &IpGeoData) -> String {
    json!({
        "city": geo.city.as_ref().map(|city| json!({ "name": city.name })),
        "country": geo.country.as_ref().map(|country| json!({ "iso": country.iso })),
        "as": geo.as_data.as_ref().map(|as_data| json!({ "number": as_data.number })),
        "network": geo
            .network
            .as_ref()
            .map(|network| json!({ "isp": network.isp, "netType": network.net_type })),
    })
    .to_string()
}

/// `torrents` 表按 info_hash 取一行（`TorrentServiceImpl.queryByInfoHash`）。
struct TorrentLookup {
    id: i64,
    size: i64,
    private_torrent: Option<bool>,
}

fn select_torrent(conn: &Connection, info_hash: &str) -> anyhow::Result<Option<TorrentLookup>> {
    let mut stmt = conn.prepare(
        "SELECT id, size, private_torrent FROM torrents WHERE info_hash = ?1 ORDER BY id LIMIT 1",
    )?;
    Ok(stmt
        .query_row(params![info_hash], |row| {
            Ok(TorrentLookup {
                id: row.get(0)?,
                size: row.get(1)?,
                private_torrent: row.get::<_, Option<i64>>(2)?.map(|v| v != 0),
            })
        })
        .optional()?)
}

fn map_traffic_data(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrafficDataComputed> {
    Ok(TrafficDataComputed {
        timestamp_ms: row.get(0)?,
        data_overall_uploaded: row.get(1)?,
        data_overall_downloaded: row.get(2)?,
    })
}

fn map_metrics_track(row: &rusqlite::Row<'_>) -> rusqlite::Result<MetricsTrackRow> {
    Ok(MetricsTrackRow {
        key: MetricsTrackKey {
            timeframe_at_ms: row.get(0)?,
            downloader: row.get(1)?,
            torrent_id: text_like(row, 2)?,
            address: row.get(3)?,
            port: row.get::<_, i64>(4)?.clamp(0, u16::MAX as i64) as u16,
        },
        peer_id: row.get(5)?,
        client_name: row.get(6)?,
        last_flags: row.get(7)?,
    })
}

fn map_tracked_swarm(row: &rusqlite::Row<'_>) -> rusqlite::Result<TrackedSwarmRow> {
    Ok(TrackedSwarmRow {
        id: Some(row.get(0)?),
        ip: row.get(1)?,
        port: row.get::<_, i64>(2)?.clamp(0, u16::MAX as i64) as u16,
        info_hash: row.get(3)?,
        torrent_is_private: row.get::<_, Option<i64>>(4)?.map(|v| v != 0),
        torrent_size: row.get(5)?,
        downloader: row.get(6)?,
        downloader_progress: row.get(7)?,
        peer_id: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
        client_name: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
        peer_progress: row.get(10)?,
        uploaded: row.get(11)?,
        uploaded_offset: row.get(12)?,
        upload_speed: row.get(13)?,
        downloaded: row.get(14)?,
        downloaded_offset: row.get(15)?,
        download_speed: row.get(16)?,
        last_flags: row.get::<_, Option<String>>(17)?.unwrap_or_default(),
        first_time_seen_ms: row.get(18)?,
        last_time_seen_ms: row.get(19)?,
        download_speed_max: row.get(20)?,
        upload_speed_max: row.get(21)?,
        // 读出的实体不是脏的（对齐 `AbstractCanDirtyEntity.dirty = false`）
        dirty: false,
    })
}

/// `torrent_id` 在库里是 INTEGER，而本移植的行结构用字符串（内存实现用 info hash），
/// 故读取时兼容两种存储类。
fn text_like(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<String> {
    Ok(match row.get_ref(index)? {
        rusqlite::types::ValueRef::Null => String::new(),
        rusqlite::types::ValueRef::Integer(value) => value.to_string(),
        rusqlite::types::ValueRef::Real(value) => value.to_string(),
        rusqlite::types::ValueRef::Text(value) => String::from_utf8_lossy(value).into_owned(),
        rusqlite::types::ValueRef::Blob(value) => String::from_utf8_lossy(value).into_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbh_core::geoip::{CityData, NetworkData, StaticGeoIp};
    use pbh_core::modules::monitor::start_of_hour_ms;

    const HOUR_MS: i64 = 3_600_000;

    fn db() -> Arc<Database> {
        Arc::new(Database::open_in_memory().expect("内存库"))
    }

    fn torrent(hash: &str) -> TorrentData {
        TorrentData {
            hash: hash.to_string(),
            name: "示例种子".to_string(),
            progress: 0.5,
            total_size: 1024,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: Some(false),
        }
    }

    fn count_of(db: &Database, table: &str) -> i64 {
        let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
            .unwrap()
    }

    /// `traffic_journal_v3` 的原始列（用于判断 `*_at_start` 是否被改动）。
    fn raw_traffic_row(db: &Database, downloader: &str, timestamp: i64) -> (i64, i64, i64, i64) {
        let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            "SELECT data_overall_downloaded, data_overall_downloaded_at_start,
                    data_overall_uploaded, data_overall_uploaded_at_start
             FROM traffic_journal_v3 WHERE downloader = ?1 AND timestamp = ?2",
            params![downloader, timestamp],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("traffic_journal_v3 行")
    }

    /// `peer_connection_metrics` 的 (id, total, question, local_not_interested, incoming)，
    /// 按 id 排序（用于验证覆盖写保留主键）。
    fn raw_connection_metrics(db: &Database) -> Vec<(i64, i64, i64, i64, i64)> {
        let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn
            .prepare(
                "SELECT id, total_connections, question_status, local_not_interested,
                        incoming_connections
                 FROM peer_connection_metrics ORDER BY id",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?))
            })
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    /// `peer_records` 按 address 取一行（生产侧暂无读取接口，测试直接查库）。
    fn raw_peer_record(db: &Database, address: &str) -> PeerRecordRow {
        let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(
            &format!("SELECT {PEER_RECORD_COLUMNS} FROM peer_records WHERE address = ?1"),
            params![address],
            |row| {
                Ok(PeerRecordRow {
                    address: row.get(0)?,
                    port: row.get::<_, i64>(1)?.clamp(0, u16::MAX as i64) as u16,
                    torrent_id: text_like(row, 2)?,
                    downloader: row.get(3)?,
                    peer_id: row.get(4)?,
                    client_name: row.get(5)?,
                    uploaded: row.get(6)?,
                    uploaded_offset: row.get(7)?,
                    upload_speed: row.get(8)?,
                    downloaded: row.get(9)?,
                    downloaded_offset: row.get(10)?,
                    download_speed: row.get(11)?,
                    last_flags: row.get(12)?,
                    first_time_seen_ms: row.get(13)?,
                    last_time_seen_ms: row.get(14)?,
                    peer_geoip: row.get(15)?,
                })
            },
        )
        .expect("peer_records 行")
    }

    // ---------------- ensure_torrent（TorrentService.createIfNotExists） ----------------

    #[test]
    fn ensure_torrent_reuses_primary_key_and_backfills_only_missing_columns() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        // 首次写入：size / private 都是「空值」形态
        let poor = TorrentData {
            total_size: 0,
            is_private: None,
            ..torrent("hash-1")
        };
        assert_eq!(sink.ensure_torrent(&poor), "1", "主键从 1 开始");
        assert_eq!(sink.ensure_torrent(&poor), "1", "同 info hash 复用主键");

        // 传入数据更完整 -> 走 upsert 的条件回填（size / private_torrent 为空才填）
        let rich = TorrentData {
            name: "换了个名字".to_string(),
            total_size: 2048,
            is_private: Some(true),
            ..torrent("hash-1")
        };
        assert_eq!(sink.ensure_torrent(&rich), "1");
        {
            let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
            let (name, size, private): (String, i64, Option<i64>) = conn
                .query_row(
                    "SELECT name, size, private_torrent FROM torrents WHERE info_hash = 'hash-1'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(size, 2048, "size <= 0 时才回填");
            assert_eq!(private, Some(1), "private_torrent IS NULL 时才回填");
            assert_eq!(name, poor.name, "上游 upsert 不回写 name");
        }

        // 记录已完整 -> 直接复用，即使传入数据不同也不回写
        let other = TorrentData {
            name: "又不一致".to_string(),
            total_size: 4096,
            is_private: Some(false),
            ..torrent("hash-1")
        };
        assert_eq!(sink.ensure_torrent(&other), "1");
        {
            let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
            let size: i64 = conn
                .query_row("SELECT size FROM torrents WHERE info_hash = 'hash-1'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(size, 2048);
        }
        assert_eq!(sink.ensure_torrent(&torrent("hash-2")), "3", "不同 info hash -> 新主键");
        // 注：中间那次 upsert 走的是 ON CONFLICT DO UPDATE，SQLite 的 AUTOINCREMENT 仍会
        // 消耗一个 id（对齐上游同一 SQL 的行为），所以下一个新 hash 拿到 3 而不是 2。
    }

    // ---------------- update_traffic_journal（TrafficJournalService.updateData） ----------------

    #[test]
    fn traffic_journal_only_raises_columns_and_never_touches_at_start() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        let now = 1_700_000_000_000;
        let hour = start_of_hour_ms(now);

        sink.update_traffic_journal("qb", 100, 200, 7, 8, now);
        assert_eq!(
            raw_traffic_row(&db, "qb", hour),
            (100, 100, 200, 200),
            "新建行：data_* 与 *_at_start 都取传入值"
        );

        // 更小 / 相等的值只能被忽略
        sink.update_traffic_journal("qb", 50, 200, 0, 0, now + 60_000);
        assert_eq!(raw_traffic_row(&db, "qb", hour), (100, 100, 200, 200));
        assert_eq!(count_of(&db, "traffic_journal_v3"), 1, "同一小时只有一行");

        // 更大的值只抬高 data_*，*_at_start 保持不变
        sink.update_traffic_journal("qb", 300, 400, 70, 80, now + 120_000);
        assert_eq!(raw_traffic_row(&db, "qb", hour), (300, 100, 400, 200));

        // 下一个小时 -> 新行，*_at_start 取当时的传入值
        sink.update_traffic_journal("qb", 500, 600, 1, 2, now + HOUR_MS);
        assert_eq!(count_of(&db, "traffic_journal_v3"), 2);
        assert_eq!(raw_traffic_row(&db, "qb", hour + HOUR_MS), (500, 500, 600, 600));
        assert_eq!(
            raw_traffic_row(&db, "qb", hour),
            (300, 100, 400, 200),
            "上一小时的行不受影响"
        );
    }

    #[test]
    fn traffic_journal_new_row_clamps_data_but_keeps_at_start_input() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        sink.update_traffic_journal("qb", -5, -6, 0, 0, 0);
        // 上游新建后立刻比较 `0 < 传入值`：负值不抬高 data_*，但 *_at_start 原样落库
        assert_eq!(raw_traffic_row(&db, "qb", start_of_hour_ms(0)), (0, -5, 0, -6));
    }

    // ---------------- query_traffic_overall（闭区间 + 聚合 / MAX(0, …)） ----------------

    #[test]
    fn traffic_overall_closed_range_and_aggregation_semantics() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        let base = start_of_hour_ms(1_700_000_000_000);
        // 参数顺序：downloaded, uploaded
        sink.update_traffic_journal("A", 100, 100, 0, 0, base);
        sink.update_traffic_journal("B", 150, 200, 0, 0, base);
        // A 的下载被抬高到 110（上传 50 不抬高，保持 100）
        sink.update_traffic_journal("A", 110, 50, 0, 0, base);

        // 聚合分支：`SUM(data) - SUM(at_start)` —— A 上传 100-100=0、下载 110-100=+10；
        // B 两列都恰好 0 -> 合计 downloaded = +10
        assert_eq!(
            sink.query_traffic_overall(None, base, base),
            vec![TrafficDataComputed {
                timestamp_ms: base,
                data_overall_uploaded: 0,
                data_overall_downloaded: 10,
            }]
        );
        // 单下载器分支：逐行 `MAX(0, 差值)`
        assert_eq!(
            sink.query_traffic_overall(Some("A"), base, base),
            vec![TrafficDataComputed {
                timestamp_ms: base,
                data_overall_uploaded: 0,
                data_overall_downloaded: 10,
            }]
        );
        assert_eq!(
            sink.query_traffic_overall(Some("B"), base, base),
            vec![TrafficDataComputed {
                timestamp_ms: base,
                data_overall_uploaded: 0,
                data_overall_downloaded: 0,
            }]
        );

        // `data < at_start` 的行（老库/手工写入才可能出现）：
        // 聚合分支允许负值，单下载器分支被 `MAX(0, …)` 夹到 0
        {
            let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute(
                "INSERT INTO traffic_journal_v3 (timestamp, downloader,
                     data_overall_uploaded_at_start, data_overall_uploaded,
                     data_overall_downloaded_at_start, data_overall_downloaded,
                     protocol_overall_uploaded_at_start, protocol_overall_uploaded,
                     protocol_overall_downloaded_at_start, protocol_overall_downloaded)
                 VALUES (?1, 'C', 0, 0, 100, 0, 0, 0, 0, 0)",
                params![base],
            )
            .unwrap();
        }
        let aggregated = sink.query_traffic_overall(None, base, base);
        assert_eq!(aggregated[0].data_overall_downloaded, 10 - 100, "聚合允许负差值");
        assert_eq!(
            sink.query_traffic_overall(Some("C"), base, base)[0].data_overall_downloaded,
            0,
            "单下载器逐行夹到 0"
        );

        // 闭区间：两端都命中，越界不命中
        let last_hour = base + HOUR_MS;
        sink.update_traffic_journal("A", 900, 900, 0, 0, last_hour);
        assert_eq!(sink.query_traffic_overall(None, base, last_hour).len(), 2);
        assert_eq!(sink.query_traffic_overall(None, base, base).len(), 1);
        assert_eq!(sink.query_traffic_overall(None, base + 1, last_hour).len(), 1);
        assert_eq!(sink.query_traffic_overall(None, base - HOUR_MS, base - 1).len(), 0);
        assert_eq!(sink.query_traffic_overall(None, last_hour + 1, last_hour + 2).len(), 0);
    }

    // ---------------- alert（AlertManager.publishAlert） ----------------

    #[test]
    fn alert_publish_and_exists_include_read() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        assert!(!sink.alert_exists_include_read("dataTrafficCapping-1"));

        let title = TranslationComponent::with_params(
            "MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_TITLE",
            vec!["2026-09-20".into()],
        );
        let content =
            TranslationComponent::new("MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_DESCRIPTION");
        sink.publish_alert(true, AlertLevel::Warn, "dataTrafficCapping-1", &title, &content);
        assert!(sink.alert_exists_include_read("dataTrafficCapping-1"));
        assert!(!sink.alert_exists_include_read("dataTrafficCapping-2"));

        let alerts = db.list_unread_alerts().unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].level, "WARN");
        assert_eq!(alerts[0].identifier, "dataTrafficCapping-1");
        assert!(alerts[0].create_at_ms > 0, "create_at = OffsetDateTime.now()");
        assert_eq!(alerts[0].read_at_ms, None, "新告警未读");
        assert_eq!(
            serde_json::from_str::<TranslationComponent>(&alerts[0].title).unwrap(),
            title
        );
        assert_eq!(
            serde_json::from_str::<TranslationComponent>(&alerts[0].content).unwrap(),
            content
        );
    }

    // ---------------- metrics track（唯一键 upsert / eq / ne / 逐条删除） ----------------

    fn track_key(timeframe_at_ms: i64, address: &str) -> MetricsTrackKey {
        MetricsTrackKey {
            timeframe_at_ms,
            downloader: "qb".to_string(),
            torrent_id: "1".to_string(),
            address: address.to_string(),
            port: 6881,
        }
    }

    #[test]
    fn metrics_track_upsert_only_updates_three_columns_and_lists_by_timeframe() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        let today = 1_700_000_000_000;
        let yesterday = today - 86_400_000;

        let mut row = MetricsTrackRow::new(track_key(today, "1.2.3.4"));
        row.peer_id = Some("-qB4500-first".into());
        row.client_name = Some("qBittorrent/4.5.0".into());
        row.last_flags = Some("d u".into());
        sink.upsert_metrics_tracks(std::slice::from_ref(&row));

        // cache-miss 回查（对齐 `syncPeers` 的 selectOne）
        assert_eq!(sink.load_metrics_track(&track_key(today, "1.2.3.4")), Some(row.clone()));
        assert_eq!(sink.load_metrics_track(&track_key(today, "5.6.7.8")), None);

        // 冲突键命中：只覆盖 peer_id / client_name / last_flags（可置空）
        let mut updated = MetricsTrackRow::new(track_key(today, "1.2.3.4"));
        updated.peer_id = Some("-qB4500-second".into());
        updated.client_name = Some("qBittorrent/4.6.0".into());
        updated.last_flags = None;
        sink.upsert_metrics_tracks(std::slice::from_ref(&updated));
        let loaded = sink.load_metrics_track(&track_key(today, "1.2.3.4")).unwrap();
        assert_eq!(loaded.peer_id.as_deref(), Some("-qB4500-second"));
        assert_eq!(loaded.client_name.as_deref(), Some("qBittorrent/4.6.0"));
        assert_eq!(loaded.last_flags, None);
        assert_eq!(count_of(&db, "peer_connection_metrics_track"), 1, "同一唯一键只有一行");

        // 另一天 / 另一地址 -> 各自新行
        let mut other_day = MetricsTrackRow::new(track_key(yesterday, "1.2.3.4"));
        other_day.last_flags = Some("U".into());
        let mut other_addr = MetricsTrackRow::new(track_key(today, "5.6.7.8"));
        other_addr.last_flags = Some("e".into());
        sink.upsert_metrics_tracks(&[other_day.clone(), other_addr.clone()]);
        assert_eq!(count_of(&db, "peer_connection_metrics_track"), 3);

        // list(eq(timeframe_at, today)) / list(ne(timeframe_at, today))
        let mut in_day = sink.list_metrics_tracks_at(today);
        in_day.sort_by(|a, b| a.key.address.cmp(&b.key.address));
        assert_eq!(in_day.len(), 2);
        assert_eq!(in_day[0].key.address, "1.2.3.4");
        assert_eq!(in_day[1].key.address, "5.6.7.8");
        assert_eq!(sink.list_metrics_tracks_not_at(today), vec![other_day]);
        assert_eq!(sink.list_metrics_tracks_not_at(yesterday).len(), 2);

        // deleteEntries -> deleteByIds：逐条删（只删传入的行）
        assert_eq!(sink.delete_metrics_tracks(&in_day), 2);
        assert_eq!(count_of(&db, "peer_connection_metrics_track"), 1);
        assert!(sink.list_metrics_tracks_at(today).is_empty());
        assert_eq!(sink.delete_metrics_tracks(&[]), 0);
        assert_eq!(
            sink.delete_metrics_tracks(&[updated]),
            0,
            "已删除的行再次删除返回 0"
        );
    }

    // ---------------- connection metrics（saveAggregating / merge 漏列 / 闭区间清理） ----------------

    fn metrics_row(timeframe_at_ms: i64) -> ConnectionMetricsRow {
        ConnectionMetricsRow {
            timeframe_at_ms,
            downloader: "qb".to_string(),
            total_connections: 3,
            incoming_connections: 4,
            remote_refuse_transfer_to_client: 5,
            remote_accept_transfer_to_client: 6,
            local_refuse_transfer_to_peer: 7,
            local_accept_transfer_to_peer: 8,
            local_not_interested: 9,
            question_status: 10,
            optimistic_unchoke: 11,
            from_dht: 12,
            from_pex: 13,
            from_lsd: 14,
            from_tracker_or_other: 15,
            rc4_encrypted: 16,
            plain_text_encrypted: 17,
            utp_socket: 18,
            tcp_socket: 19,
        }
    }

    #[test]
    fn connection_metrics_overwrite_replaces_row_and_merge_omits_local_not_interested() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        let today = 1_700_000_000_000;
        let row = metrics_row(today);

        sink.save_connection_metrics(std::slice::from_ref(&row), false);
        assert_eq!(raw_connection_metrics(&db), vec![(1, 3, 10, 9, 4)]);

        // overwrite=true：整行覆盖（保留主键），local_not_interested 也被覆盖
        let mut replacement = metrics_row(today);
        replacement.total_connections = 30;
        replacement.local_not_interested = 99;
        replacement.question_status = 0;
        sink.save_connection_metrics(std::slice::from_ref(&replacement), true);
        assert_eq!(
            raw_connection_metrics(&db),
            vec![(1, 30, 0, 99, 4)],
            "覆盖写保留 id、整行替换"
        );

        // overwrite=false：累加合并，且与上游 merge() 一致地**不累加** local_not_interested
        sink.save_connection_metrics(std::slice::from_ref(&row), false);
        assert_eq!(
            raw_connection_metrics(&db),
            vec![(1, 33, 10, 99, 8)],
            "合并写：local_not_interested 保持 99"
        );

        // 冲突键是 (timeframe_at, downloader)：另一下载器 / 另一时间窗都是新行
        let mut other_downloader = metrics_row(today);
        other_downloader.downloader = "tr".to_string();
        let mut other_timeframe = metrics_row(today + 86_400_000);
        other_timeframe.downloader = "tr".to_string();
        sink.save_connection_metrics(&[other_downloader, other_timeframe], false);
        assert_eq!(raw_connection_metrics(&db).len(), 3);
    }

    #[test]
    fn remove_connection_metrics_before_is_closed_interval() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        let base = 1_700_000_000_000;
        let mut newer = metrics_row(base + 1);
        newer.downloader = "tr".to_string();
        sink.save_connection_metrics(&[metrics_row(base), newer], false);
        assert_eq!(raw_connection_metrics(&db).len(), 2);

        // `le`：边界上的行也会被删除
        assert_eq!(sink.remove_connection_metrics_before(base), 1);
        let stored = raw_connection_metrics(&db);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].0, 2, "留下的是更新的一行");
        assert_eq!(sink.remove_connection_metrics_before(base), 0);
        assert_eq!(sink.remove_connection_metrics_before(base + 1), 1);
        assert!(raw_connection_metrics(&db).is_empty());
    }

    // ---------------- peer records（CASE upsert / 永不更新的列 / 开区间清理） ----------------

    fn peer_record(address: &str, last_time_seen_ms: i64) -> PeerRecordRow {
        PeerRecordRow {
            address: address.to_string(),
            port: 6881,
            torrent_id: "1".to_string(),
            downloader: "qb".to_string(),
            peer_id: "-qB4500-".to_string(),
            client_name: "qBittorrent/4.5.0".to_string(),
            uploaded: 1000,
            uploaded_offset: 1000,
            upload_speed: 100,
            downloaded: 2000,
            downloaded_offset: 2000,
            download_speed: 200,
            last_flags: Some("d u".to_string()),
            first_time_seen_ms: 10,
            last_time_seen_ms,
            peer_geoip: Some("{\"city\":null}".to_string()),
        }
    }

    #[test]
    fn peer_record_upsert_keeps_first_time_seen_and_geoip_and_applies_delta_case() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        sink.upsert_peer_record(&peer_record("1.2.3.4", 1000));
        let first = raw_peer_record(&db, "1.2.3.4");
        assert_eq!(first.first_time_seen_ms, 10);
        assert_eq!(first.peer_geoip.as_deref(), Some("{\"city\":null}"));

        // 更旧的上报：所有 CASE 都保留旧值（last_time_seen 也不回退）
        let mut older = peer_record("1.2.3.4", 500);
        older.uploaded = 7;
        older.downloaded = 8;
        older.uploaded_offset = 7;
        older.downloaded_offset = 8;
        older.port = 1234;
        older.peer_id = "changed".to_string();
        older.client_name = "changed".to_string();
        older.last_flags = Some("changed".to_string());
        older.upload_speed = 1;
        older.download_speed = 2;
        older.peer_geoip = Some("{\"city\":{\"name\":\"new\"}}".to_string());
        sink.upsert_peer_record(&older);
        assert_eq!(raw_peer_record(&db, "1.2.3.4"), first, "更旧的上报被整行忽略");

        // 较新的上报：uploaded + 本次值 - 已存偏移量
        let mut newer = peer_record("1.2.3.4", 2000);
        newer.uploaded = 3000;
        newer.downloaded = 5000;
        newer.uploaded_offset = 3000;
        newer.downloaded_offset = 5000;
        newer.peer_id = "-qB4600-".to_string();
        newer.peer_geoip = Some("{\"city\":{\"name\":\"new\"}}".to_string());
        sink.upsert_peer_record(&newer);
        let merged = raw_peer_record(&db, "1.2.3.4");
        assert_eq!(merged.uploaded, 1000 + 3000 - 1000);
        assert_eq!(merged.downloaded, 2000 + 5000 - 2000);
        assert_eq!(merged.uploaded_offset, 3000);
        assert_eq!(merged.downloaded_offset, 5000);
        assert_eq!(merged.peer_id, "-qB4600-");
        assert_eq!(merged.last_time_seen_ms, 2000);
        assert_eq!(merged.first_time_seen_ms, 10, "first_time_seen 永不更新");
        assert_eq!(
            merged.peer_geoip.as_deref(),
            Some("{\"city\":null}"),
            "peer_geoip 永不更新"
        );

        // 计数被重置（本次值 < 已存偏移量）：整个本次值累加
        let mut reset = peer_record("1.2.3.4", 3000);
        reset.uploaded = 10;
        reset.downloaded = 20;
        reset.uploaded_offset = 10;
        reset.downloaded_offset = 20;
        sink.upsert_peer_record(&reset);
        let after_reset = raw_peer_record(&db, "1.2.3.4");
        assert_eq!(after_reset.uploaded, 3000 + 10, "计数被重置 -> 整个本次值累加");
        assert_eq!(after_reset.downloaded, 5000 + 20, "计数被重置 -> 整个本次值累加");
        assert_eq!(after_reset.uploaded_offset, 10);
        assert_eq!(after_reset.downloaded_offset, 20);
        assert_eq!(after_reset.last_time_seen_ms, 3000);
    }

    #[test]
    fn peer_records_conflict_key_excludes_port_and_cleanup_is_open_interval() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        sink.upsert_peer_record(&peer_record("1.2.3.4", 100));
        // 同地址、不同 torrent / 不同下载器 -> 新行
        let mut other_torrent = peer_record("1.2.3.4", 100);
        other_torrent.torrent_id = "2".to_string();
        other_torrent.port = 9999;
        let mut other_downloader = peer_record("1.2.3.4", 100);
        other_downloader.downloader = "tr".to_string();
        sink.upsert_peer_record(&other_torrent);
        sink.upsert_peer_record(&other_downloader);
        assert_eq!(count_of(&db, "peer_records"), 3);
        // 同地址 + 同 torrent + 同下载器、仅端口不同 -> 仍是一行（V1_3 去掉了 port）
        let mut same_key = peer_record("1.2.3.4", 200);
        same_key.port = 6882;
        sink.upsert_peer_record(&same_key);
        assert_eq!(count_of(&db, "peer_records"), 3, "冲突键不含 port");
        assert_eq!(raw_peer_record(&db, "1.2.3.4").port, 6882, "port 仍会被更新");

        // `lt`：开区间，边界上的行保留
        assert_eq!(sink.remove_peer_records_before(200), 2, "只删 last_time_seen < 200 的行");
        assert_eq!(count_of(&db, "peer_records"), 1);
        assert_eq!(raw_peer_record(&db, "1.2.3.4").last_time_seen_ms, 200);
        assert_eq!(sink.remove_peer_records_before(200), 0);
        assert_eq!(sink.remove_peer_records_before(201), 1);
        assert_eq!(count_of(&db, "peer_records"), 0);
    }

    #[test]
    fn peer_record_geoip_is_filled_by_the_ip_database_on_insert() {
        let db = db();
        let mut geo = StaticGeoIp::new();
        geo.insert(
            "1.2.3.4".parse().unwrap(),
            IpGeoData {
                city: Some(CityData {
                    name: Some("示例城市".into()),
                    ..CityData::default()
                }),
                network: Some(NetworkData {
                    isp: Some("示例运营商".into()),
                    net_type: Some("wideband".into()),
                }),
                ..IpGeoData::default()
            },
        );
        let sink = DbMonitorSink::with_geo(db.clone(), Some(Arc::new(geo)));
        let mut row = peer_record("1.2.3.4", 1000);
        row.peer_geoip = None;
        sink.upsert_peer_record(&row);
        let stored = raw_peer_record(&db, "1.2.3.4");
        let geo_json: serde_json::Value =
            serde_json::from_str(stored.peer_geoip.as_deref().unwrap()).unwrap();
        assert_eq!(geo_json["city"]["name"], "示例城市");
        assert_eq!(geo_json["network"]["netType"], "wideband");
        assert_eq!(geo_json["network"]["isp"], "示例运营商");
        assert!(geo_json["country"].is_null(), "无数据时输出 null");
        assert!(geo_json["as"].is_null());

        // 行内自带 peer_geoip 时优先（不查 IP 库）；新键（另一 torrent_id）落到另一行
        let mut with_geo = peer_record("1.2.3.4", 1000);
        with_geo.torrent_id = "2".to_string();
        with_geo.peer_geoip = Some("{\"city\":null}".to_string());
        sink.upsert_peer_record(&with_geo);
        {
            let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
            let stored: Option<String> = conn
                .query_row(
                    "SELECT peer_geoip FROM peer_records WHERE address = '1.2.3.4' AND torrent_id = 2",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(stored.as_deref(), Some("{\"city\":null}"), "行内值优先于 IP 库查询");
        }
        // 同一行再次上报其它 geo：peer_geoip 永不更新（DB 里仍是首插的值）
        let mut again = peer_record("1.2.3.4", 1500);
        again.peer_geoip = Some("{\"city\":{\"name\":\"changed\"}}".to_string());
        sink.upsert_peer_record(&again);
        let stored = raw_peer_record(&db, "1.2.3.4");
        assert_eq!(
            stored.peer_geoip.as_deref(),
            Some(
                "{\"as\":null,\"city\":{\"name\":\"示例城市\"},\"country\":null,\
                 \"network\":{\"isp\":\"示例运营商\",\"netType\":\"wideband\"}}"
            ),
            "peer_geoip 永不更新"
        );

        // 未注入 IP 库（对齐 `IPDBManager.ipdb == null`）-> NULL
        let sink_without_geo = DbMonitorSink::new(db.clone());
        let mut unknown = peer_record("5.6.7.8", 1000);
        unknown.peer_geoip = None;
        sink_without_geo.upsert_peer_record(&unknown);
        assert_eq!(raw_peer_record(&db, "5.6.7.8").peer_geoip, None);
    }

    // ---------------- tracked swarm（upsert / 取最近一行 / 清空 / 计数 / 分页） ----------------

    fn swarm_row(address: &str, first_time_seen_ms: i64, last_time_seen_ms: i64) -> TrackedSwarmRow {
        TrackedSwarmRow {
            id: None,
            ip: address.to_string(),
            port: 6881,
            info_hash: "abcdef0123456789".to_string(),
            torrent_is_private: Some(false),
            torrent_size: 1_000_000_000,
            downloader: "qb".to_string(),
            downloader_progress: 0.25,
            peer_id: "-qB4500-aaaaaaaa".to_string(),
            client_name: "qBittorrent/4.5.0".to_string(),
            peer_progress: 0.5,
            uploaded: 100,
            uploaded_offset: 100,
            upload_speed: 10,
            downloaded: 200,
            downloaded_offset: 200,
            download_speed: 20,
            last_flags: "d u".to_string(),
            first_time_seen_ms,
            last_time_seen_ms,
            download_speed_max: 30,
            upload_speed_max: 40,
            dirty: true,
        }
    }

    #[test]
    fn tracked_swarm_upsert_overwrites_every_column_but_id_and_counts() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        sink.upsert_tracked_swarm(&swarm_row("1.2.3.4", 1000, 1000));
        assert_eq!(sink.count_tracked_swarm(), 1);
        let loaded = sink
            .load_last_tracked_swarm("1.2.3.4", 6881, "abcdef0123456789", "qb")
            .expect("已写入的行");
        assert_eq!(loaded.id, Some(1));
        assert!(!loaded.dirty, "读出的实体不脏");
        assert_eq!(loaded.peer_progress, 0.5, "REAL 列往返");
        assert_eq!(loaded.torrent_is_private, Some(false));

        // 冲突键命中：除 id 外的全部列都被覆盖（含 first_time_seen）
        let mut updated = swarm_row("1.2.3.4", 5000, 9000);
        updated.peer_id = "-qB4600-bbbbbbbb".to_string();
        updated.last_flags = "U".to_string();
        updated.uploaded = 700;
        updated.torrent_is_private = None;
        sink.upsert_tracked_swarm(&updated);
        assert_eq!(sink.count_tracked_swarm(), 1, "唯一键命中不新增行");
        let loaded = sink
            .load_last_tracked_swarm("1.2.3.4", 6881, "abcdef0123456789", "qb")
            .unwrap();
        assert_eq!(loaded.id, Some(1), "主键保留");
        assert_eq!(loaded.first_time_seen_ms, 5000, "first_time_seen 也覆盖");
        assert_eq!(loaded.last_time_seen_ms, 9000);
        assert_eq!(loaded.peer_id, "-qB4600-bbbbbbbb");
        assert_eq!(loaded.last_flags, "U");
        assert_eq!(loaded.uploaded, 700);
        assert_eq!(loaded.torrent_is_private, None, "可空列往返");

        // 另一 peer / 另一 info hash -> 新行
        sink.upsert_tracked_swarm(&swarm_row("5.6.7.8", 1000, 1000));
        let mut other_hash = swarm_row("1.2.3.4", 1000, 1000);
        other_hash.info_hash = "ffffffffffffffff".to_string();
        sink.upsert_tracked_swarm(&other_hash);
        assert_eq!(sink.count_tracked_swarm(), 3);
        assert_eq!(db.tracked_swarm_count().unwrap(), 3);
        assert!(sink
            .load_last_tracked_swarm("9.9.9.9", 6881, "abcdef0123456789", "qb")
            .is_none());
    }

    #[test]
    fn tracked_swarm_load_last_returns_the_highest_id_and_reset_clears_the_table() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        // 唯一索引保证同一键只有一行；上游的 `ORDER BY id DESC LIMIT 1` 是防御性写法，
        // 这里去掉索引构造「同键多行」以锁定该语义。
        {
            let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute("DROP INDEX idx_tracked_swarm_unique", []).unwrap();
            conn.execute(
                "INSERT INTO tracked_swarm (ip, port, info_hash, torrent_is_private, torrent_size,
                     downloader, downloader_progress, peer_id, client_name, peer_progress, uploaded,
                     uploaded_offset, upload_speed, downloaded, downloaded_offset, download_speed,
                     last_flags, first_time_seen, last_time_seen, download_speed_max, upload_speed_max)
                 VALUES ('1.2.3.4', 6881, 'abcdef0123456789', 0, 1, 'qb', 0, '', '', 0, 0, 0, 0, 0, 0, 0,
                     '', 111, 111, 0, 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tracked_swarm (ip, port, info_hash, torrent_is_private, torrent_size,
                     downloader, downloader_progress, peer_id, client_name, peer_progress, uploaded,
                     uploaded_offset, upload_speed, downloaded, downloaded_offset, download_speed,
                     last_flags, first_time_seen, last_time_seen, download_speed_max, upload_speed_max)
                 VALUES ('1.2.3.4', 6881, 'abcdef0123456789', 0, 1, 'qb', 0, '', '', 0, 0, 0, 0, 0, 0, 0,
                     '', 222, 222, 0, 0)",
                [],
            )
            .unwrap();
        }
        let loaded = sink
            .load_last_tracked_swarm("1.2.3.4", 6881, "abcdef0123456789", "qb")
            .unwrap();
        assert_eq!(loaded.last_time_seen_ms, 222, "ORDER BY id DESC LIMIT 1");
        assert_eq!(loaded.id, Some(2));

        // resetTable -> DELETE FROM tracked_swarm
        assert_eq!(sink.count_tracked_swarm(), 2);
        sink.reset_tracked_swarm();
        assert_eq!(sink.count_tracked_swarm(), 0);
        assert_eq!(db.tracked_swarm_count().unwrap(), 0);
    }

    #[test]
    fn tracked_swarm_paging_and_order_by_whitelist() {
        let db = db();
        let sink = DbMonitorSink::new(db.clone());
        for index in 0..5 {
            let mut row = swarm_row(&format!("10.0.0.{index}"), 1000 + index, 1000 + index);
            row.port = 6881 + index as u16;
            sink.upsert_tracked_swarm(&row);
        }
        // 无 orderBy：与上游一致不带 ORDER BY，只分页
        let (rows, total) = db.page_tracked_swarm(&[], 2, 0).unwrap();
        assert_eq!(total, 5);
        assert_eq!(rows.len(), 2);
        let (rows, total) = db.page_tracked_swarm(&[], 2, 4).unwrap();
        assert_eq!(total, 5);
        assert_eq!(rows.len(), 1);

        // orderBy=last_time_seen|desc
        let (rows, _) = db
            .page_tracked_swarm(&[("last_time_seen".to_string(), false)], 10, 0)
            .unwrap();
        let seen: Vec<i64> = rows.iter().map(|row| row.last_time_seen_ms).collect();
        assert_eq!(seen, vec![1004, 1003, 1002, 1001, 1000]);
        // 带表名前缀同样允许（对齐 `Orderable` 的 `field` 语义）
        let (rows, _) = db
            .page_tracked_swarm(&[("tracked_swarm.id".to_string(), false)], 1, 0)
            .unwrap();
        assert_eq!(rows[0].id, Some(5));

        // 非法列名 -> 报错（对齐 `SQLHelper.checkSafeFieldName`）
        assert!(db
            .page_tracked_swarm(&[("id; DROP TABLE tracked_swarm".to_string(), true)], 1, 0)
            .is_err());
        assert!(db.page_tracked_swarm(&[("no_such_column".to_string(), true)], 1, 0).is_err());
        assert!(db.page_tracked_swarm(&[("".to_string(), true)], 1, 0).is_err());
    }
}
