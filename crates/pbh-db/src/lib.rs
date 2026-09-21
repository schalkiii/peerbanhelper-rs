//! SQLite 持久化：封禁日志、封禁列表、PCB 历史（SPEC 第 8 节）与监控数据
//! （[`monitor`] 子模块，SPEC 第 9 节，对齐上游 `databasent` 的 `alert` /
//! `traffic_journal_v3` / `peer_connection_metrics(_track)` / `peer_records` / `tracked_swarm`）。

pub mod monitor;

pub use monitor::{AlertRow, DbMonitorSink};

use chrono::{DateTime, Utc};
use pbh_core::modules::progress_cheat::{PcbEntityKind, PcbPersistRow};
use rusqlite::Connection;
use std::sync::Mutex;

pub const SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Debug)]
pub struct BanLog {
    pub id: Option<i64>,
    pub downloader_id: String,
    pub torrent_hash: String,
    pub torrent_name: String,
    pub ip: String,
    pub port: i64,
    pub peer_id: String,
    pub client_name: String,
    pub module: String,
    pub rule: String,
    pub reason: String,
    /// 上游 `CheckResult.rule` 的可翻译形式（`TranslationComponent` 的 JSON），落库后可由 API 按请求 locale 重新本地化。
    pub rule_key: Option<String>,
    /// 上游 `CheckResult.reason` 的可翻译形式（`TranslationComponent` 的 JSON）。
    pub reason_key: Option<String>,
    pub ban_duration: i64,
    pub created_at: i64,
}

#[derive(Clone, Debug)]
pub struct BannedIp {
    pub ip: String,
    pub first_banned_at: i64,
    pub last_banned_at: i64,
    pub module: String,
    pub hit_count: i64,
    pub ban_until: i64,
}

/// `pcb_addr` / `pcb_range` 对应的表名（对齐上游实体表）。
fn pcb_table(kind: PcbEntityKind) -> &'static str {
    match kind {
        PcbEntityKind::Addr => "pcb_addr",
        PcbEntityKind::Range => "pcb_range",
    }
}

pub struct Database {
    conn: Mutex<Connection>,
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

impl Database {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self { conn: Mutex::new(conn) };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self { conn: Mutex::new(conn) };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(include_str!("schema.sql"))?;
        // 增量迁移（老库缺列时补齐；新库已含列，忽略重复列错误）
        for sql in [
            "ALTER TABLE ban_logs ADD COLUMN rule_key TEXT",
            "ALTER TABLE ban_logs ADD COLUMN reason_key TEXT",
        ] {
            let _ = conn.execute(sql, []);
        }
        conn.execute(
            "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
            rusqlite::params![SCHEMA_VERSION],
        )?;
        Ok(())
    }

    // ---------- 封禁日志 ----------

    pub fn insert_ban_log(&self, log: &BanLog) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO ban_logs
             (downloader_id, torrent_hash, torrent_name, ip, port, peer_id, client_name,
              module, rule, reason, rule_key, reason_key, ban_duration, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            rusqlite::params![
                log.downloader_id,
                log.torrent_hash,
                log.torrent_name,
                log.ip,
                log.port,
                log.peer_id,
                log.client_name,
                log.module,
                log.rule,
                log.reason,
                log.rule_key,
                log.reason_key,
                log.ban_duration,
                log.created_at,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn list_ban_logs(&self, limit: i64, offset: i64) -> anyhow::Result<(Vec<BanLog>, i64)> {
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM ban_logs", [], |r| r.get(0))?;
        let mut stmt = conn.prepare(
            "SELECT id, downloader_id, torrent_hash, torrent_name, ip, port, peer_id, client_name,
                    module, rule, reason, rule_key, reason_key, ban_duration, created_at
             FROM ban_logs ORDER BY id DESC LIMIT ?1 OFFSET ?2",
        )?;
        let rows = stmt.query_map(rusqlite::params![limit, offset], map_ban_log)?;
        Ok((rows.collect::<Result<Vec<_>, _>>()?, total))
    }

    /// 删除超过 keep_days 天的日志，返回删除条数。
    pub fn cleanup_ban_logs(&self, keep_days: i64) -> anyhow::Result<usize> {
        let cutoff = now_ms() - keep_days * 86_400_000;
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM ban_logs WHERE created_at < ?1", rusqlite::params![cutoff])?)
    }

    // ---------- 封禁列表 ----------

    pub fn upsert_banned_ip(&self, ip: &str, module: &str, ban_duration_ms: i64) -> anyhow::Result<()> {
        let now = now_ms();
        let until = if ban_duration_ms > 0 { now + ban_duration_ms } else { 0 };
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO banned_ips (ip, first_banned_at, last_banned_at, module, hit_count, ban_until)
             VALUES (?1, ?2, ?2, ?3, 1, ?4)
             ON CONFLICT(ip) DO UPDATE SET
               last_banned_at=excluded.last_banned_at,
               module=excluded.module,
               hit_count=hit_count+1,
               ban_until=excluded.ban_until",
            rusqlite::params![ip, now, module, until],
        )?;
        Ok(())
    }

    pub fn list_banned_ips(&self) -> anyhow::Result<Vec<BannedIp>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT ip, first_banned_at, last_banned_at, module, hit_count, ban_until
             FROM banned_ips ORDER BY last_banned_at DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(BannedIp {
                ip: row.get(0)?,
                first_banned_at: row.get(1)?,
                last_banned_at: row.get(2)?,
                module: row.get(3)?,
                hit_count: row.get(4)?,
                ban_until: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn remove_banned_ip(&self, ip: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM banned_ips WHERE ip=?1", rusqlite::params![ip])?;
        Ok(())
    }

    /// 批量移除封禁项（用于到期解封；返回删除条数）。
    pub fn remove_banned_ips(&self, ips: &[String]) -> anyhow::Result<usize> {
        if ips.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        let mut removed = 0usize;
        {
            let mut stmt = tx.prepare("DELETE FROM banned_ips WHERE ip=?1")?;
            for ip in ips {
                removed += stmt.execute(rusqlite::params![ip])?;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    // ---------- PCB 历史 ----------

    /// 批量写入需要落库的 PCB 实体（对齐上游 `batchFlushBackDatabase*`）。
    pub fn upsert_pcb_rows(&self, rows: &[PcbPersistRow]) -> anyhow::Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        let mut written = 0usize;
        for row in rows {
            let table = pcb_table(row.kind);
            let sql = format!(
                "INSERT INTO {table}
                 (downloader_id, torrent_id, key, port, last_report_uploaded,
                  tracking_uploaded_increase_total, last_report_progress, last_torrent_completed_size,
                  progress_difference_counter, rewind_counter, ban_delay_window_end_ms,
                  fast_pcb_test_executed, last_time_seen_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
                 ON CONFLICT(downloader_id, torrent_id, key, port) DO UPDATE SET
                   last_report_uploaded=excluded.last_report_uploaded,
                   tracking_uploaded_increase_total=excluded.tracking_uploaded_increase_total,
                   last_report_progress=excluded.last_report_progress,
                   last_torrent_completed_size=excluded.last_torrent_completed_size,
                   progress_difference_counter=excluded.progress_difference_counter,
                   rewind_counter=excluded.rewind_counter,
                   ban_delay_window_end_ms=excluded.ban_delay_window_end_ms,
                   fast_pcb_test_executed=excluded.fast_pcb_test_executed,
                   last_time_seen_ms=excluded.last_time_seen_ms"
            );
            written += tx.execute(
                &sql,
                rusqlite::params![
                    row.downloader_id,
                    row.torrent_id,
                    row.key,
                    row.port as i64,
                    row.last_report_uploaded,
                    row.tracking_uploaded_increase_total,
                    row.last_report_progress,
                    row.last_torrent_completed_size,
                    row.progress_difference_counter,
                    row.rewind_counter,
                    row.ban_delay_window_end_ms,
                    row.fast_pcb_test_executed as i64,
                    row.last_time_seen_ms,
                ],
            )?;
        }
        tx.commit()?;
        Ok(written)
    }

    /// 读取某个类型（`pcb_addr` / `pcb_range`）的全部持久化实体。
    pub fn load_pcb_rows(&self, kind: PcbEntityKind) -> anyhow::Result<Vec<PcbPersistRow>> {
        let table = pcb_table(kind);
        let sql = format!(
            "SELECT downloader_id, torrent_id, key, port, last_report_uploaded,
                    tracking_uploaded_increase_total, last_report_progress, last_torrent_completed_size,
                    progress_difference_counter, rewind_counter, ban_delay_window_end_ms,
                    fast_pcb_test_executed, last_time_seen_ms FROM {table}"
        );
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], |r| {
            Ok(PcbPersistRow {
                kind,
                downloader_id: r.get(0)?,
                torrent_id: r.get(1)?,
                key: r.get(2)?,
                port: r.get::<_, i64>(3)?.clamp(0, u16::MAX as i64) as u16,
                last_report_uploaded: r.get(4)?,
                tracking_uploaded_increase_total: r.get(5)?,
                last_report_progress: r.get(6)?,
                last_torrent_completed_size: r.get(7)?,
                progress_difference_counter: r.get(8)?,
                rewind_counter: r.get(9)?,
                ban_delay_window_end_ms: r.get(10)?,
                fast_pcb_test_executed: r.get::<_, i64>(11)? != 0,
                last_time_seen_ms: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub fn cleanup_pcb(&self, older_than_ms: i64) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut n = 0;
        for t in ["pcb_addr", "pcb_range"] {
            n += conn.execute(
                &format!("DELETE FROM {t} WHERE last_time_seen_ms < ?1"),
                rusqlite::params![older_than_ms],
            )?;
        }
        Ok(n)
    }
}

fn map_ban_log(row: &rusqlite::Row<'_>) -> rusqlite::Result<BanLog> {
    Ok(BanLog {
        id: Some(row.get(0)?),
        downloader_id: row.get(1)?,
        torrent_hash: row.get(2)?,
        torrent_name: row.get(3)?,
        ip: row.get(4)?,
        port: row.get(5)?,
        peer_id: row.get(6)?,
        client_name: row.get(7)?,
        module: row.get(8)?,
        rule: row.get(9)?,
        reason: row.get(10)?,
        rule_key: row.get(11)?,
        reason_key: row.get(12)?,
        ban_duration: row.get(13)?,
        created_at: row.get(14)?,
    })
}

/// 将 epoch 毫秒转为 RFC3339（供 Web 展示）。
pub fn ms_to_rfc3339(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ban_log_crud_and_pagination() {
        let db = Database::open_in_memory().unwrap();
        for i in 0..5 {
            db.insert_ban_log(&BanLog {
                id: None,
                downloader_id: "qb".into(),
                torrent_hash: format!("h{i}"),
                torrent_name: "t".into(),
                ip: format!("1.2.3.{i}"),
                port: 1000 + i,
                peer_id: "-hp".into(),
                client_name: "hp".into(),
                module: "peer-id-blacklist".into(),
                rule: "rule".into(),
                reason: "r".into(),
                rule_key: None,
                reason_key: None,
                ban_duration: 1000,
                created_at: now_ms(),
            })
            .unwrap();
        }
        let (page, total) = db.list_ban_logs(2, 0).unwrap();
        assert_eq!(total, 5);
        assert_eq!(page.len(), 2);
    }

    #[test]
    fn banned_ip_upsert_counts() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_banned_ip("1.2.3.4", "peer-id-blacklist", 1000).unwrap();
        db.upsert_banned_ip("1.2.3.4", "peer-id-blacklist", 1000).unwrap();
        let list = db.list_banned_ips().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].hit_count, 2);
    }

    #[test]
    fn ban_log_roundtrips_translation_keys() {
        let db = Database::open_in_memory().unwrap();
        let rule_key = serde_json::to_string(
            &pbh_core::i18n::TranslationComponent::with_params(
                "MODULE_IBL_MATCH_IP",
                vec!["1.2.3.0/24".into()],
            ),
        )
        .unwrap();
        db.insert_ban_log(&BanLog {
            id: None,
            downloader_id: "qb".into(),
            torrent_hash: "h".into(),
            torrent_name: "t".into(),
            ip: "1.2.3.4".into(),
            port: 1,
            peer_id: "".into(),
            client_name: "".into(),
            module: "ip-address-blocker".into(),
            rule: "匹配 IP 规则: 1.2.3.0/24".into(),
            reason: "r".into(),
            rule_key: Some(rule_key.clone()),
            reason_key: None,
            ban_duration: 0,
            created_at: 0,
        })
        .unwrap();
        let (logs, total) = db.list_ban_logs(10, 0).unwrap();
        assert_eq!(total, 1);
        assert_eq!(logs[0].rule_key.as_deref(), Some(rule_key.as_str()));
        assert!(logs[0].reason_key.is_none());
    }

    #[test]
    fn pcb_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        let row = PcbPersistRow {
            kind: PcbEntityKind::Addr,
            downloader_id: "qb".into(),
            torrent_id: "hash".into(),
            key: "1.2.3.4".into(),
            port: 51413,
            tracking_uploaded_increase_total: 99,
            ban_delay_window_end_ms: 12_345,
            ..default_pcb_row()
        };
        assert_eq!(db.upsert_pcb_rows(std::slice::from_ref(&row)).unwrap(), 1);
        let loaded = db.load_pcb_rows(PcbEntityKind::Addr).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].tracking_uploaded_increase_total, 99);
        assert_eq!(loaded[0].port, 51413);
        assert_eq!(loaded[0].ban_delay_window_end_ms, 12_345);
        // 同一主键再次写入为更新而非新增
        assert_eq!(db.upsert_pcb_rows(std::slice::from_ref(&row)).unwrap(), 1);
        assert_eq!(db.load_pcb_rows(PcbEntityKind::Addr).unwrap().len(), 1);
        // range 表相互独立
        assert!(db.load_pcb_rows(PcbEntityKind::Range).unwrap().is_empty());
    }

    fn default_pcb_row() -> PcbPersistRow {
        PcbPersistRow {
            kind: PcbEntityKind::Addr,
            downloader_id: String::new(),
            torrent_id: String::new(),
            key: String::new(),
            port: 0,
            last_report_uploaded: 0,
            tracking_uploaded_increase_total: 0,
            last_report_progress: 0.0,
            last_torrent_completed_size: 0,
            progress_difference_counter: 0,
            rewind_counter: 0,
            ban_delay_window_end_ms: 0,
            fast_pcb_test_executed: false,
            last_time_seen_ms: 0,
        }
    }
}
