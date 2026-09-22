//! BTN 上报数据源（[`BtnSubmitSource`]）的 SQLite 实现。
//!
//! 对齐上游 `BtnNetwork.getServer()` 暴露的只读数据出口：
//!
//! | 数据源方法 | 上游 | 表 / 分页 |
//! | --- | --- | --- |
//! | `batch_ban_history` | `HistoryService.page(id > cursor)` + `BtnBan.from` | `history` JOIN `torrents`，每页 100 |
//! | `batch_swarm_history` | `TrackedSwarmService.page(lastTimeSeen >= x, id > y)` | `tracked_swarm`，每页 1000 |
//! | `batch_peer_history` | `PeerRecordService.getPendingSubmitPeerRecords(since)` | `peer_records` JOIN `torrents`，每页 5000 |
//!
//! 遗留协议的 `legacy_peer_snapshot` / `legacy_ban_snapshot` 依赖 `DownloaderServer` 的
//! **内存**快照（live peers / `BanMetadata`），SQLite 数据源不提供，保持 trait 的默认空
//! 实现（等价上游「没有待提交数据」）；这两个方法只在 `min_protocol_version < 20`
//! 的旧服务器上使用。

use std::sync::Arc;

use pbh_core::btn_transport::{
    BtnBanHistoryRow, BtnPeerHistoryRow, BtnSubmitSource, BtnSwarmHistoryRow,
};
use pbh_core::i18n::{TranslationComponent, Translator};
use pbh_core::model::TorrentData;
use rusqlite::params;
use tracing::warn;

use crate::Database;

/// `HistoryEntity`（表 `history`）的一行：[`Database::insert_history`] 的输入。
#[derive(Clone, Debug)]
pub struct HistoryRecord {
    /// 封禁时刻（epoch 毫秒）
    pub ban_at_ms: i64,
    /// 解封时刻（epoch 毫秒；上游 `banAt + duration`）
    pub unban_at_ms: i64,
    pub ip: String,
    pub port: u16,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_uploaded: Option<i64>,
    pub peer_downloaded: Option<i64>,
    pub peer_progress: f64,
    /// `metadata.getTorrent().getProgress()`
    pub downloader_progress: f64,
    /// torrent 快照（内部经 [`Database::ensure_torrent_id`] 解析为 `torrent_id`）
    pub torrent: TorrentData,
    /// `metadata.getContext()`（模块 configName）
    pub module_name: String,
    /// `metadata.getRule()`：`TranslationComponent` 的 JSON
    pub rule_name: String,
    /// `metadata.getDescription()`：`TranslationComponent` 的 JSON
    pub description: String,
    pub flags: Option<String>,
    /// `metadata.getDownloader().id()`
    pub downloader: String,
    /// `metadata.getStructuredData()` 的 JSON 文本
    pub structured_data: Option<String>,
    /// IP 库查询结果（`IPGeoData` 的 JSON；缺省 = 未装 IP 库，落 NULL）
    pub peer_geoip: Option<String>,
}

/// `history` JOIN `torrents` 的一行（`batch_ban_history` 的中间结构）。
struct BanHistoryJoinRow {
    id: i64,
    ban_at_ms: i64,
    peer_ip: String,
    peer_port: u16,
    peer_id: Option<String>,
    peer_client_name: Option<String>,
    peer_uploaded: Option<i64>,
    peer_downloaded: Option<i64>,
    peer_progress: f64,
    downloader_progress: f64,
    module_name: String,
    /// `TranslationComponent` 的 JSON，由数据源渲染后上报
    rule_name: String,
    /// `TranslationComponent` 的 JSON，由数据源渲染后上报
    description: String,
    flags: Option<String>,
    structured_data: Option<String>,
    torrent_hash: String,
    torrent_size: i64,
    torrent_is_private: bool,
}

impl Database {
    /// `HistoryService.save`：写入一条封禁历史（对齐 `PersistMetrics.recordPeerBan`）。
    ///
    /// 返回新行主键（`submit_bans` 的游标基于该 id 升序推进）。
    pub fn insert_history(&self, record: &HistoryRecord) -> anyhow::Result<i64> {
        let torrent_id = self.ensure_torrent_id(&record.torrent)?;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO history
             (ban_at, unban_at, ip, port, peer_id, peer_client_name, peer_uploaded,
              peer_downloaded, peer_progress, downloader_progress, torrent_id, module_name,
              rule_name, description, flags, downloader, structured_data, peer_geoip)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
            params![
                record.ban_at_ms,
                record.unban_at_ms,
                record.ip,
                record.port as i64,
                record.peer_id,
                record.peer_client_name,
                record.peer_uploaded,
                record.peer_downloaded,
                record.peer_progress,
                record.downloader_progress,
                torrent_id,
                record.module_name,
                record.rule_name,
                record.description,
                record.flags,
                record.downloader,
                record.structured_data,
                record.peer_geoip,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// `historyDao.page(id > cursor_id)` 的有序分页（`submit_bans` 用）。
    ///
    /// 对齐上游 `BtnBan.from` 的两个前置条件：
    /// - torrent 行缺失（历史库不完整）时该行无法转换，直接跳过；
    /// - `peer_uploaded` / `peer_downloaded` 为 NULL 时上游拆箱 NPE 被 catch 过滤，同样跳过。
    fn page_ban_history_after(
        &self,
        cursor_id: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<BanHistoryJoinRow>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT h.id, h.ban_at, h.ip, h.port, h.peer_id, h.peer_client_name,
                    h.peer_uploaded, h.peer_downloaded, h.peer_progress, h.downloader_progress,
                    h.module_name, h.rule_name, h.description, h.flags, h.structured_data,
                    t.info_hash, t.size, t.private_torrent
             FROM history h
             JOIN torrents t ON t.id = h.torrent_id
             WHERE h.id > ?1
             ORDER BY h.id ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![cursor_id, limit as i64], |row| {
            Ok(BanHistoryJoinRow {
                id: row.get(0)?,
                ban_at_ms: row.get(1)?,
                peer_ip: row.get(2)?,
                peer_port: row.get(3)?,
                peer_id: row.get(4)?,
                peer_client_name: row.get(5)?,
                peer_uploaded: row.get(6)?,
                peer_downloaded: row.get(7)?,
                peer_progress: row.get(8)?,
                downloader_progress: row.get(9)?,
                module_name: row.get(10)?,
                rule_name: row.get(11)?,
                description: row.get(12)?,
                flags: row.get(13)?,
                structured_data: row.get(14)?,
                torrent_hash: row.get(15)?,
                torrent_size: row.get(16)?,
                torrent_is_private: row.get::<_, Option<i64>>(17)?.is_some_and(|v| v != 0),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// `swarmDao.page(lastTimeSeen >= x, id > y, orderByAsc(lastTimeSeen, id))`。
    fn page_swarm_history(
        &self,
        last_time_seen_ms: i64,
        id_after: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<BtnSwarmHistoryRow>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, ip, port, info_hash, torrent_is_private, torrent_size, downloader,
                    downloader_progress, peer_id, client_name, peer_progress, uploaded,
                    uploaded_offset, upload_speed, downloaded, downloaded_offset, download_speed,
                    last_flags, first_time_seen, last_time_seen, download_speed_max,
                    upload_speed_max
             FROM tracked_swarm
             WHERE last_time_seen >= ?1 AND id > ?2
             ORDER BY last_time_seen ASC, id ASC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![last_time_seen_ms, id_after, limit as i64], |row| {
            Ok(BtnSwarmHistoryRow {
                id: row.get(0)?,
                peer_ip: row.get(1)?,
                peer_port: row.get(2)?,
                torrent_hash: row.get(3)?,
                torrent_is_private: row.get::<_, Option<i64>>(4)?.map(|v| v != 0),
                torrent_size: row.get(5)?,
                downloader: row.get(6)?,
                downloader_progress: row.get(7)?,
                peer_id: row.get(8)?,
                peer_client_name: row.get(9)?,
                peer_progress: row.get(10)?,
                // 上游 `BtnSwarm.from`：`toPeerTraffic ← uploaded`、`fromPeerTraffic ← downloaded`
                to_peer_traffic: row.get(11)?,
                to_peer_traffic_offset: row.get(12)?,
                upload_speed: row.get(13)?,
                from_peer_traffic: row.get(14)?,
                from_peer_traffic_offset: row.get(15)?,
                download_speed: row.get(16)?,
                peer_last_flags: row.get(17)?,
                first_time_seen_ms: row.get(18)?,
                last_time_seen_ms: row.get(19)?,
                download_speed_max: row.get(20)?,
                upload_speed_max: row.get(21)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// `getPendingSubmitPeerRecords(since)`：`last_time_seen > since OR IS NULL`，按时间升序。
    fn page_peer_history(
        &self,
        last_time_seen_ms: i64,
        limit: usize,
    ) -> anyhow::Result<Vec<BtnPeerHistoryRow>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT p.address, p.port, p.peer_id, p.client_name, t.info_hash, t.size,
                    t.private_torrent, p.downloaded, p.downloaded_offset, p.uploaded,
                    p.uploaded_offset, p.first_time_seen, p.last_time_seen, p.last_flags
             FROM peer_records p
             JOIN torrents t ON t.id = p.torrent_id
             WHERE p.last_time_seen > ?1 OR p.last_time_seen IS NULL
             ORDER BY p.last_time_seen ASC
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![last_time_seen_ms, limit as i64], |row| {
            Ok(BtnPeerHistoryRow {
                ip_address: row.get(0)?,
                port: row.get(1)?,
                peer_id: row.get(2)?,
                client_name: row.get(3)?,
                torrent_hash: row.get(4)?,
                torrent_size: row.get(5)?,
                // 上游 `LegacyBtnPeerHistory.from` 未赋值该字段（Java 默认 false），
                // 这里按 torrent 实体实际值填充，信息更完整且不改变协议字段类型
                torrent_is_private: row.get::<_, Option<i64>>(6)?.is_some_and(|v| v != 0),
                downloaded: row.get(7)?,
                downloaded_offset: row.get(8)?,
                uploaded: row.get(9)?,
                uploaded_offset: row.get(10)?,
                first_time_seen_ms: row.get(11)?,
                last_time_seen_ms: row.get(12)?,
                peer_flag: row.get(13)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

/// `BtnSubmitSource` 的 SQLite 实现（`BtnNetwork::with_submit_source` 注入）。
///
/// `translator` / `locale` 用于把 `history.rule_name` / `description` 的
/// `TranslationComponent` JSON 渲染成上报文本（对齐上游 `tlUI(component)`）。
pub struct DbBtnSubmitSource {
    db: Arc<Database>,
    translator: Arc<Translator>,
    locale: String,
}

impl std::fmt::Debug for DbBtnSubmitSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DbBtnSubmitSource")
            .field("locale", &self.locale)
            .finish_non_exhaustive()
    }
}

impl DbBtnSubmitSource {
    pub fn new(db: Arc<Database>, translator: Arc<Translator>, locale: impl Into<String>) -> Self {
        Self {
            db,
            translator,
            locale: locale.into(),
        }
    }

    /// 渲染 `TranslationComponent` 的 JSON（对齐上游 `tlUI(component)`）；
    /// JSON 解析失败（脏数据）时回退为原始文本，保证上报内容始终可读。
    fn render(&self, raw: &str) -> String {
        match serde_json::from_str::<TranslationComponent>(raw) {
            Ok(component) => self.translator.render(&component, &self.locale),
            Err(_) => raw.to_string(),
        }
    }
}

impl BtnSubmitSource for DbBtnSubmitSource {
    fn batch_ban_history(&self, cursor_id: i64, limit: usize) -> Vec<BtnBanHistoryRow> {
        let rows = match self.db.page_ban_history_after(cursor_id, limit) {
            Ok(rows) => rows,
            Err(e) => {
                warn!("BTN submit_bans 数据源查询失败: {e}");
                return Vec::new();
            }
        };
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            // 上游 `BtnBan.from`：`Long` 拆箱 NPE 被 `createSubmitRequest` 的 catch 过滤
            let (Some(peer_uploaded), Some(peer_downloaded)) =
                (row.peer_uploaded, row.peer_downloaded)
            else {
                continue;
            };
            result.push(BtnBanHistoryRow {
                id: row.id,
                ban_at_ms: row.ban_at_ms,
                peer_ip: row.peer_ip,
                peer_port: row.peer_port,
                peer_id: row.peer_id,
                peer_client_name: row.peer_client_name,
                peer_progress: row.peer_progress,
                peer_flag: row.flags,
                torrent_hash: row.torrent_hash,
                torrent_is_private: row.torrent_is_private,
                torrent_size: row.torrent_size,
                // 上游映射方向：`fromPeerTraffic ← peerDownloaded`、`toPeerTraffic ← peerUploaded`
                from_peer_traffic: peer_downloaded,
                to_peer_traffic: peer_uploaded,
                downloader_progress: row.downloader_progress,
                module: row.module_name,
                rule: self.render(&row.rule_name),
                description: self.render(&row.description),
                // 上游 `JsonUtil.standard().toJson(...)`：NULL 也输出字符串 "null"
                structured_data: Some(row.structured_data.unwrap_or_else(|| "null".to_string())),
            });
        }
        result
    }

    fn batch_swarm_history(
        &self,
        last_time_seen_ms: i64,
        id_after: i64,
        limit: usize,
    ) -> Vec<BtnSwarmHistoryRow> {
        match self
            .db
            .page_swarm_history(last_time_seen_ms, id_after, limit)
        {
            Ok(rows) => rows,
            Err(e) => {
                warn!("BTN submit_swarm 数据源查询失败: {e}");
                Vec::new()
            }
        }
    }

    fn batch_peer_history(&self, last_time_seen_ms: i64, limit: usize) -> Vec<BtnPeerHistoryRow> {
        match self.db.page_peer_history(last_time_seen_ms, limit) {
            Ok(rows) => rows,
            Err(e) => {
                warn!("BTN submit_histories 数据源查询失败: {e}");
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Arc<Database> {
        Arc::new(Database::open_in_memory().expect("内存库"))
    }

    fn source(db: &Arc<Database>) -> DbBtnSubmitSource {
        DbBtnSubmitSource::new(db.clone(), Arc::new(Translator::embedded()), "zh_cn")
    }

    fn torrent(hash: &str) -> TorrentData {
        TorrentData {
            hash: hash.to_string(),
            name: "示例种子".to_string(),
            progress: 0.75,
            total_size: 2048,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: Some(true),
        }
    }

    fn history(ip: &str, torrent_hash: &str) -> HistoryRecord {
        HistoryRecord {
            ban_at_ms: 1_700_000_000_000,
            unban_at_ms: 1_700_003_600_000,
            ip: ip.to_string(),
            port: 6881,
            peer_id: Some("peer-1".to_string()),
            peer_client_name: Some("qBittorrent".to_string()),
            peer_uploaded: Some(1000),
            peer_downloaded: Some(2000),
            peer_progress: 0.25,
            downloader_progress: 0.75,
            torrent: torrent(torrent_hash),
            module_name: "peer-analyse-service".to_string(),
            rule_name: r#"{"key":"BTN_NETWORK_NOT_ENABLED","params":[]}"#.to_string(),
            description: r#"{"key":"自定义描述","params":[]}"#.to_string(),
            flags: Some("X".to_string()),
            downloader: "qbittorrent".to_string(),
            structured_data: None,
            peer_geoip: None,
        }
    }

    fn count(db: &Database, table: &str) -> i64 {
        let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
    }

    fn insert_swarm(db: &Database, ip: &str, port: u16, last_time_seen: i64) {
        let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO tracked_swarm
             (ip, port, info_hash, torrent_is_private, torrent_size, downloader,
              downloader_progress, peer_id, client_name, peer_progress, uploaded,
              uploaded_offset, upload_speed, downloaded, downloaded_offset, download_speed,
              last_flags, first_time_seen, last_time_seen, download_speed_max, upload_speed_max)
             VALUES (?1, ?2, 'hash-swarm', 1, 4096, 'qb', 0.5, 'peer-1', 'client', 0.1,
                     11, 1, 2, 33, 3, 4, 'U', 100, ?3, 5, 6)",
            params![ip, port as i64, last_time_seen],
        )
        .unwrap();
    }

    fn insert_peer_record(db: &Database, address: &str, torrent_id: i64, last_time_seen: i64) {
        let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO peer_records
             (address, port, torrent_id, downloader, peer_id, client_name, uploaded,
              uploaded_offset, upload_speed, downloaded, downloaded_offset, download_speed,
              last_flags, first_time_seen, last_time_seen, peer_geoip)
             VALUES (?1, 6881, ?2, 'qb', 'peer-1', 'client', 10, 1, 2, 20, 2, 3, 'U', 50, ?3, NULL)",
            params![address, torrent_id, last_time_seen],
        )
        .unwrap();
    }

    /// `history` 落库 + `submit_bans` 分页：torrent 复用、字段映射、游标推进、文案渲染。
    #[test]
    fn insert_history_reuses_torrent_and_batch_ban_history_follows_id_cursor() {
        let db = db();
        let source = source(&db);
        assert_eq!(db.insert_history(&history("1.1.1.1", "hash-a")).unwrap(), 1);
        assert_eq!(db.insert_history(&history("1.1.1.2", "hash-a")).unwrap(), 2);
        assert_eq!(db.insert_history(&history("1.1.1.3", "hash-b")).unwrap(), 3);
        assert_eq!(count(&db, "torrents"), 2, "同 info hash 复用 torrents 行");
        assert_eq!(count(&db, "history"), 3);

        let rows = source.batch_ban_history(0, 100);
        assert_eq!(rows.len(), 3);
        let row = &rows[0];
        assert_eq!(row.id, 1);
        assert_eq!(row.torrent_hash, "hash-a");
        assert!(row.torrent_is_private);
        assert_eq!(row.torrent_size, 2048);
        // 上游映射方向：`fromPeerTraffic ← peerDownloaded`、`toPeerTraffic ← peerUploaded`
        assert_eq!(row.from_peer_traffic, 2000);
        assert_eq!(row.to_peer_traffic, 1000);
        // `TranslationComponent` JSON 按服务端语言渲染；未知 key 原样输出
        assert!(row.rule.contains("未启用"), "rule={}", row.rule);
        assert_eq!(row.description, "自定义描述");
        // 上游 `JsonUtil.standard().toJson(null)` 输出字符串 "null"
        assert_eq!(row.structured_data.as_deref(), Some("null"));

        // 游标：`id > cursor` 升序推进
        let next = source.batch_ban_history(row.id, 100);
        assert_eq!(next.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2, 3]);
        assert!(source.batch_ban_history(3, 100).is_empty());
        // limit 生效
        assert_eq!(source.batch_ban_history(0, 1).len(), 1);
    }

    /// `submit_bans`：`peer_uploaded` / `peer_downloaded` 为 NULL 的行跳过（对齐上游 NPE 过滤）。
    #[test]
    fn batch_ban_history_skips_rows_without_peer_traffic() {
        let db = db();
        let source = source(&db);
        let mut record = history("1.1.1.1", "hash-a");
        record.peer_uploaded = None;
        db.insert_history(&record).unwrap();
        db.insert_history(&history("1.1.1.2", "hash-a")).unwrap();

        let rows = source.batch_ban_history(0, 100);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 2, "缺流量列的旧数据不上报");
    }

    /// `submit_swarm`：二元游标 `(last_time_seen >= x, id > y)` 与字段方向映射。
    #[test]
    fn batch_swarm_history_follows_two_part_cursor() {
        let db = db();
        let source = source(&db);
        insert_swarm(&db, "1.2.3.4", 6881, 200);
        insert_swarm(&db, "1.2.3.5", 6882, 200);
        insert_swarm(&db, "1.2.3.6", 6883, 300);

        let all = source.batch_swarm_history(0, 0, 1000);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].torrent_hash, "hash-swarm");
        // 上游 `BtnSwarm.from`：`toPeerTraffic ← uploaded`、`fromPeerTraffic ← downloaded`
        assert_eq!(all[0].to_peer_traffic, 11);
        assert_eq!(all[0].to_peer_traffic_offset, 1);
        assert_eq!(all[0].from_peer_traffic, 33);
        assert_eq!(all[0].from_peer_traffic_offset, 3);
        assert_eq!(all[0].torrent_is_private, Some(true));

        let page = source.batch_swarm_history(200, 1, 1000);
        assert_eq!(page.iter().map(|r| r.id).collect::<Vec<_>>(), vec![2, 3]);
        assert!(source.batch_swarm_history(300, 3, 1000).is_empty());
        assert_eq!(source.batch_swarm_history(0, 0, 1).len(), 1);
    }

    /// `submit_histories`：时间游标 `last_time_seen > since` 与 torrent JOIN。
    #[test]
    fn batch_peer_history_follows_time_cursor() {
        let db = db();
        let source = source(&db);
        let torrent_id = db.ensure_torrent_id(&torrent("hash-peer")).unwrap();
        insert_peer_record(&db, "9.9.9.1", torrent_id, 100);
        insert_peer_record(&db, "9.9.9.2", torrent_id, 200);
        insert_peer_record(&db, "9.9.9.3", torrent_id, 300);

        let all = source.batch_peer_history(0, 5000);
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].torrent_hash, "hash-peer");
        assert_eq!(all[0].uploaded, 10);
        assert_eq!(all[0].downloaded, 20);
        assert_eq!(all[0].port, 6881);
        assert_eq!(all[0].peer_flag.as_deref(), Some("U"));

        let page = source.batch_peer_history(200, 5000);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].last_time_seen_ms, 300);
        assert!(source.batch_peer_history(300, 5000).is_empty());
        assert_eq!(source.batch_peer_history(0, 1).len(), 1);
    }

    /// 遗留协议快照保持 trait 默认空实现（数据源无法提供内存 live peers / ban list）。
    #[test]
    fn legacy_snapshots_stay_empty() {
        let db = db();
        let source = source(&db);
        assert!(source.legacy_peer_snapshot().is_empty());
        assert!(source.legacy_ban_snapshot().is_empty());
    }
}
