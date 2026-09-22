//! SQLite 持久化：封禁日志、封禁列表、PCB 历史（SPEC 第 8 节）与监控数据
//! （[`monitor`] 子模块，SPEC 第 9 节，对齐上游 `databasent` 的 `alert` /
//! `traffic_journal_v3` / `peer_connection_metrics(_track)` / `peer_records` / `tracked_swarm`）。

pub mod monitor;

pub use monitor::{AlertRow, DbMonitorSink};

use chrono::{DateTime, Utc};
use pbh_core::btn_transport::BtnMetadataStore;
use pbh_core::modules::progress_cheat::{PcbEntityKind, PcbPersistRow};
use rusqlite::{Connection, OptionalExtension};
use std::sync::{Arc, Mutex};
use tracing::warn;

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

    /// BannedIp 排行查询（`/api/bans/ranks`）。
    ///
    /// `filter` 为 `(ip 模糊匹配, 前置 LIMIT)`，对齐上游 `Paginable#query` + `ORDER BY hit_count DESC`。
    /// `limit` 为 `-1` 时表示不过滤行数（全量）。
    /// BannedIp 排行查询（`/api/bans/ranks`）。
    ///
    /// `filter` 为 IP 模糊匹配（对齐上游 `Paginable#query` 的 `ip ? LIKE %filter%`），
    /// 排序固定 `hit_count DESC`；每条附带该 IP 在 `ban_logs` 中的累计封禁次数。
    pub fn page_banned_rank(
        &self,
        filter: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<(BannedIp, usize)>> {
        let conn = self.conn.lock().unwrap();
        let sql = if filter.is_empty() {
            "SELECT ip, first_banned_at, last_banned_at, module, hit_count, ban_until,
                    (SELECT COUNT(*) FROM ban_logs WHERE ban_logs.ip=banned_ips.ip) AS total
             FROM banned_ips ORDER BY hit_count DESC LIMIT ?1 OFFSET ?2"
        } else {
            "SELECT ip, first_banned_at, last_banned_at, module, hit_count, ban_until,
                    (SELECT COUNT(*) FROM ban_logs WHERE ban_logs.ip=banned_ips.ip) AS total
             FROM banned_ips WHERE ip LIKE ?1 ESCAPE '\\' ORDER BY hit_count DESC LIMIT ?2 OFFSET ?3"
        };
        let mut stmt = conn.prepare(sql)?;
        let cols = |row: &rusqlite::Row<'_>| {
            Ok((
                BannedIp {
                    ip: row.get(0)?,
                    first_banned_at: row.get(1)?,
                    last_banned_at: row.get(2)?,
                    module: row.get(3)?,
                    hit_count: row.get(4)?,
                    ban_until: row.get(5)?,
                },
                row.get::<_, i64>(6)? as usize,
            ))
        };
        let rows = if filter.is_empty() {
            stmt.query_map(rusqlite::params![limit, offset], cols)?
        } else {
            stmt.query_map(
                rusqlite::params![format!("%{filter}%"), limit, offset],
                cols,
            )?
        };
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
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

    // ---------- 键值元数据（meta） ----------

    /// 读回一个键（不存在 ⇒ `None`）。
    pub fn get_meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT value FROM meta WHERE key=?1",
                rusqlite::params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// 写入/覆盖一个键（upsert）。
    pub fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
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

    // ---------- Web 历史 / 排行 / 统计查询（对齐上游 `HistoryMapper` 与 `PBH*Controller`）----------

    /// WebUI 通用排序白名单（`orderBy` 字段 → 数据库列 + 所属表）。
    ///
    /// 与上游 `Orderable.addRemapping` 的语义一致：DTO 字段名映射到 `history` /
    /// `peer_records` 表的实际列；未知字段直接拒绝（返回 `None`），避免 SQL 注入。
    pub fn history_order_column(field: &str) -> Option<(String, String)> {
        let (table, column) = match field {
            "banAt" => ("h", "created_at"),
            "unbanAt" => ("h", "created_at"),
            "peerIp" | "ip" => ("h", "ip"),
            "peerPort" | "port" => ("h", "port"),
            "peerId" => ("h", "peer_id"),
            "peerClientName" => ("h", "client_name"),
            "peerUploaded" => ("h", "created_at"), // 无对应列：退回禁止
            "peerDownloaded" => ("h", "created_at"),
            "peerProgress" => ("h", "created_at"),
            "torrentInfoHash" => ("t", "info_hash"),
            "torrentName" => ("t", "name"),
            "module" => ("h", "module"),
            "rule" => ("h", "rule"),
            "description" => ("h", "reason"),
            "id" => ("h", "id"),
            _ => return None,
        };
        Some((table.to_string(), column.to_string()))
    }

    /// `PeerRecord` 排序白名单（`accessHistory` 端点）。
    pub fn peer_record_order(field: &str) -> Option<String> {
        match field {
            "peerId" | "peer_id" => Some("peer_id".into()),
            "clientName" | "client_name" => Some("client_name".into()),
            "firstTimeSeen" | "first_time_seen" => Some("first_time_seen".into()),
            "lastTimeSeen" | "last_time_seen" => Some("last_time_seen".into()),
            "uploaded" => Some("uploaded".into()),
            "downloaded" => Some("downloaded".into()),
            "uploadSpeed" | "upload_speed" => Some("upload_speed".into()),
            "downloadSpeed" | "download_speed" => Some("download_speed".into()),
            "id" => Some("id".into()),
            _ => None,
        }
    }

    /// 分页查询封禁历史（对齐上游 `HistoryService.getBanLogs` / `queryBanHistoryByIp` /
    /// `queryBanHistoryByTorrentId`）。`ip` / `torrent_hash` 至少给一个；都为空时查全表。
    pub fn page_ban_history(
        &self,
        ip: Option<&str>,
        torrent_hash: Option<&str>,
        order: &[(String, bool)],
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<(Vec<BanHistoryRow>, i64)> {
        let mut conditions: Vec<String> = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(ip) = ip {
            conditions.push("h.ip = ?".to_string());
            params.push(Box::new(ip.to_string()));
        }
        if let Some(hash) = torrent_hash {
            conditions.push("h.torrent_hash = ?".to_string());
            params.push(Box::new(hash.to_string()));
        }
        let where_sql = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        // 排序：白名单字段 → `表.列`；非法字段直接报错（对齐上游 `checkSafeFieldName`）
        let mut order_clauses: Vec<String> = Vec::new();
        for (field, asc) in order {
            let Some((table, column)) = Self::history_order_column(field) else {
                anyhow::bail!("非法排序列: {field}");
            };
            order_clauses.push(format!(
                "{table}.{column} {}",
                if *asc { "ASC" } else { "DESC" }
            ));
        }
        if order_clauses.is_empty() {
            order_clauses.push("h.created_at DESC".to_string());
        }
        order_clauses.push("h.id DESC".to_string());
        let order_sql = order_clauses.join(", ");

        let conn = self.conn.lock().unwrap();
        let count_sql = format!("SELECT COUNT(*) FROM ban_logs h {where_sql}");
        let total: i64 = conn
            .query_row(
                &count_sql,
                rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
                |r| r.get(0),
            )
            .map_err(|e| anyhow::anyhow!("ban history count: {e}"))?;

        let sql = format!(
            "SELECT h.id, h.created_at AS h_at, 0 AS unban_at, h.ip, h.port, h.peer_id,
                    h.client_name, COALESCE(t.id, 0), COALESCE(t.info_hash, ''), h.torrent_name,
                    COALESCE(t.size, 0), h.module, h.rule, h.reason, h.downloader_id,
                    0 AS peer_uploaded, 0 AS peer_downloaded, 0.0 AS peer_progress
             FROM ban_logs h LEFT JOIN torrents t ON t.info_hash = h.torrent_hash
             {where_sql} ORDER BY {order_sql} LIMIT ? OFFSET ?"
        );
        let mut rows: Vec<BanHistoryRow> = Vec::new();
        {
            let mut stmt = conn.prepare(&sql)?;
            let mut query_params: Vec<&dyn rusqlite::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            query_params.push(&limit);
            query_params.push(&offset);
            let iter = stmt.query_map(rusqlite::params_from_iter(query_params), |row| {
                Ok(BanHistoryRow {
                    id: row.get(0)?,
                    ban_at: row.get(1)?,
                    unban_at: row.get(2)?,
                    ip: row.get(3)?,
                    port: row.get(4)?,
                    peer_id: row.get(5)?,
                    peer_client_name: row.get(6)?,
                    torrent_id: row.get(7)?,
                    torrent_info_hash: row.get(8)?,
                    torrent_name: row.get(9)?,
                    torrent_size: row.get(10)?,
                    module: row.get(11)?,
                    rule: row.get(12)?,
                    description: row.get(13)?,
                    downloader: row.get(14)?,
                })
            })?;
            for r in iter {
                rows.push(r?);
            }
        }
        Ok((rows, total))
    }

    /// 封禁排行：`SELECT ip, COUNT(*) FROM ban_logs GROUP BY ip ORDER BY count DESC`
    /// 对齐上游 `HistoryMapper.getBannedIps`（`filter` 为 IP 前缀，空串/`None` 表示不限）。
    pub fn page_ban_rank(
        &self,
        ip_prefix: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<(Vec<(String, i64)>, i64)> {
        let conn = self.conn.lock().unwrap();
        let (filter_sql, count_sql) = match ip_prefix {
            Some(prefix) if !prefix.is_empty() => (
                Some("WHERE ip LIKE ?1 || '%'"),
                Some("WHERE ip LIKE ?1 || '%'"),
            ),
            _ => (None, None),
        };
        let total: i64 = if let Some(where_sql) = count_sql {
            if let Some(prefix) = ip_prefix {
                conn.query_row(
                    &format!("SELECT COUNT(DISTINCT ip) FROM ban_logs {where_sql}"),
                    rusqlite::params![prefix],
                    |r| r.get(0),
                )
                .unwrap_or(0)
            } else {
                0
            }
        } else {
            conn.query_row("SELECT COUNT(DISTINCT ip) FROM ban_logs", [], |r| r.get(0))
                .unwrap_or(0)
        };
        let sql = if let Some(where_sql) = filter_sql {
            format!(
                "SELECT ip, COUNT(*) AS count FROM ban_logs {where_sql} GROUP BY ip ORDER BY count DESC LIMIT ? OFFSET ?"
            )
        } else {
            "SELECT ip, COUNT(*) AS count FROM ban_logs GROUP BY ip ORDER BY count DESC LIMIT ? OFFSET ?".to_string()
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = if let Some(prefix) = ip_prefix.filter(|p| !p.is_empty()) {
            stmt.query_map(rusqlite::params![prefix, limit, offset], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(rusqlite::params![limit, offset], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        Ok((rows, total))
    }

    /// 删除全部（/清）banned_ips；返回删除数量。
    pub fn clear_banned_ips(&self) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM banned_ips", [])?)
    }

    /// 在 `since_ms` 之后（含）的会话总连接数（`weeklySessions` 计数等）。
    pub fn peer_session_count_since(&self, since_ms: i64) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT COALESCE(SUM(total_connections), 0) FROM peer_connection_metrics WHERE timeframe_at >= ?1",
                rusqlite::params![since_ms],
                |r| r.get(0),
            )
            .unwrap_or(0))
    }

    /// 当前 swarm 中唯一 IP 数量（`trackedSwarmCount` 计数）。
    pub fn tracked_swarm_size(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row("SELECT COUNT(DISTINCT ip) FROM tracked_swarm", [], |r| r.get(0))
            .unwrap_or(0))
    }

    /// 取指定 IP 最近一条封禁日志（`/api/bans` 列表的 context 用）。
    pub fn last_ban_log_by_ip(&self, ip: &str) -> anyhow::Result<Option<BanLog>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, downloader_id, torrent_hash, torrent_name, ip, port, peer_id, client_name,
                    module, rule, reason, rule_key, reason_key, ban_duration, created_at
             FROM ban_logs WHERE ip = ?1 ORDER BY created_at DESC, id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(rusqlite::params![ip], |row| {
            Ok(BanLog {
                id: row.get(0)?,
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
        })?;
        rows.next().transpose().map_err(Into::into)
    }

    /// `module + rule` 分组统计（`/api/statistic/rules`）。
    pub fn rule_stats(&self) -> anyhow::Result<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT module, rule, COUNT(*) AS c FROM ban_logs GROUP BY module, rule ORDER BY c DESC",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `peer_records.first_time_seen` 按天归组计数（连接趋势近似，`timeframe_at` 对齐图表时间轴）。
    pub fn peer_first_seen_trend(
        &self,
        start_ms: i64,
        end_ms: i64,
        downloader: Option<&str>,
    ) -> anyhow::Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock().unwrap();
        let (cond, has_downloader) = match downloader {
            Some(_) => (" AND downloader = ?3", true),
            None => ("", false),
        };
        let sql = format!(
            "SELECT (first_time_seen / 86400000) * 86400000 AS day, COUNT(*) FROM peer_records
             WHERE first_time_seen >= ?1 AND first_time_seen < ?2{cond}
             GROUP BY day ORDER BY day"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = if has_downloader {
            let d = downloader.unwrap_or_default();
            stmt.query_map(rusqlite::params![start_ms, end_ms, d], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(rusqlite::params![start_ms, end_ms], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    /// `traffic_journal_v3` 按天汇总（上传 / 下载各为 `(timestamp, (downloaded, uploaded))`）。
    pub fn traffic_trend(
        &self,
        start_ms: i64,
        end_ms: i64,
        downloader: Option<&str>,
    ) -> anyhow::Result<Vec<(i64, (i64, i64))>> {
        let conn = self.conn.lock().unwrap();
        let (cond, has_downloader) = match downloader {
            Some(_) => (" AND downloader = ?3", true),
            None => ("", false),
        };
        let sql = format!(
            "SELECT (timestamp / 86400000) * 86400000 AS day,
                    COALESCE(SUM(data_overall_downloaded), 0),
                    COALESCE(SUM(data_overall_uploaded), 0)
             FROM traffic_journal_v3 WHERE timestamp >= ?1 AND timestamp < ?2{cond}
             GROUP BY day ORDER BY day"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = if has_downloader {
            let d = downloader.unwrap_or_default();
            stmt.query_map(rusqlite::params![start_ms, end_ms, d], |r| {
                Ok((r.get::<_, i64>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(rusqlite::params![start_ms, end_ms], |r| {
                Ok((r.get::<_, i64>(0)?, (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?)))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    /// `peer_connection_metrics` 按天汇总（`(day, [total, incoming])`）。
    pub fn connection_metrics_trend(
        &self,
        start_ms: i64,
        end_ms: i64,
        downloader: Option<&str>,
    ) -> anyhow::Result<Vec<(i64, [i64; 2])>> {
        let conn = self.conn.lock().unwrap();
        let (cond, has_downloader) = match downloader {
            Some(_) => (" AND downloader = ?3", true),
            None => ("", false),
        };
        let sql = format!(
            "SELECT (timeframe_at / 86400000) * 86400000 AS day,
                    COALESCE(SUM(total_connections), 0),
                    COALESCE(SUM(incoming_connections), 0)
             FROM peer_connection_metrics WHERE timeframe_at >= ?1 AND timeframe_at < ?2{cond}
             GROUP BY day ORDER BY day"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = if has_downloader {
            let d = downloader.unwrap_or_default();
            stmt.query_map(rusqlite::params![start_ms, end_ms, d], |r| {
                Ok((r.get::<_, i64>(0)?, [r.get::<_, i64>(1)?, r.get::<_, i64>(2)?]))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(rusqlite::params![start_ms, end_ms], |r| {
                Ok((r.get::<_, i64>(0)?, [r.get::<_, i64>(1)?, r.get::<_, i64>(2)?]))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    /// 按 IP 前缀或种子关键字查询访问历史（对齐 `PeerRecordService.query` +
    /// `TorrentService.get` 的组合）。返回 (行, 总数)；`order` 元素为 (字段名, 升序?)，
    /// 字段名走 `peer_record_order` 白名单。
    #[allow(clippy::too_many_arguments)]
    pub fn query_access_history(
        &self,
        ip_prefix: Option<&str>,
        keyword: Option<&str>,
        order: &[(String, bool)],
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<(Vec<AccessHistoryRow>, i64)> {
        let conn = self.conn.lock().unwrap();
        let (cond, params): (String, Vec<Box<dyn rusqlite::ToSql>>) = if let Some(ip) = ip_prefix {
            ("WHERE p.address LIKE ?1 || '%'".to_string(), vec![Box::new(ip.to_string())])
        } else if let Some(kw) = keyword {
            (
                "WHERE t.info_hash = ?1 OR t.name LIKE ?1 || '%'".to_string(),
                vec![Box::new(kw.to_string())],
            )
        } else {
            (String::new(), Vec::new())
        };

        let mut order_sql = String::new();
        for (field, asc) in order {
            if let Some(col) = Self::peer_record_order(field) {
                order_sql.push_str(&format!("p.{col} {}, ", if *asc { "ASC" } else { "DESC" }));
            }
        }
        order_sql.push_str("p.last_time_seen DESC");

        let count_sql =
            format!("SELECT COUNT(*) FROM peer_records p LEFT JOIN torrents t ON t.id = p.torrent_id{cond}");
        let total: i64 = conn
            .query_row(
                &count_sql,
                rusqlite::params_from_iter(params.iter().map(|b| b.as_ref())),
                |r| r.get(0),
            )
            .unwrap_or(0);
        let sql = format!(
            "SELECT p.id, p.address, p.port, p.torrent_id, COALESCE(t.info_hash, ''), COALESCE(t.name, ''),
                    COALESCE(t.size, 0), p.downloader, COALESCE(p.peer_id, ''), COALESCE(p.client_name, ''),
                    p.uploaded, p.downloaded, p.upload_speed, p.download_speed,
                    COALESCE(p.last_flags, ''), p.first_time_seen, p.last_time_seen
             FROM peer_records p LEFT JOIN torrents t ON t.id = p.torrent_id{cond}
             ORDER BY {order_sql} LIMIT ? OFFSET ?"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut query_params: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
        query_params.push(&limit);
        query_params.push(&offset);
        let rows = stmt
            .query_map(rusqlite::params_from_iter(query_params), |row| {
                Ok(AccessHistoryRow {
                    id: row.get(0)?,
                    peer_ip: row.get(1)?,
                    port: row.get(2)?,
                    torrent_id: row.get(3)?,
                    torrent_info_hash: row.get(4)?,
                    torrent_name: row.get(5)?,
                    torrent_size: row.get(6)?,
                    downloader: row.get(7)?,
                    peer_id: row.get(8)?,
                    client_name: row.get(9)?,
                    uploaded: row.get(10)?,
                    downloaded: row.get(11)?,
                    upload_speed: row.get(12)?,
                    download_speed: row.get(13)?,
                    last_flags: row.get(14)?,
                    first_time_seen: row.get(15)?,
                    last_time_seen: row.get(16)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((rows, total))
    }

    /// 某 IP 的访问统计（对齐 `PeerRecordService.getPeerInfo` 的计数部分）。
    ///
    /// 返回 (总访问次数, 涉及的不同种子数, 最早/最晚时间, 累计上传/下载)。
    pub fn peer_access_summary(&self, ip: &str) -> anyhow::Result<(i64, i64, i64, i64, i64, i64)> {
        let conn = self.conn.lock().unwrap();
        let row = conn.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT torrent_id), COALESCE(MIN(first_time_seen), 0),
                    COALESCE(MAX(last_time_seen), 0), COALESCE(SUM(uploaded), 0), COALESCE(SUM(downloaded), 0)
             FROM peer_records WHERE address = ?",
            rusqlite::params![ip],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            },
        )?;
        Ok(row)
    }

    /// 查询种子列表（含封禁/访问统计，用于 `/api/torrent/query`）。
    pub fn torrent_list(
        &self,
        keyword: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<(Vec<TorrentRow>, i64)> {
        let conn = self.conn.lock().unwrap();
        let cond = match keyword {
            Some(kw) if !kw.is_empty() => "WHERE t.name LIKE ?1 || '%' OR t.info_hash = ?1",
            _ => "",
        };
        let kw = keyword.unwrap_or("").to_string();
        let total: i64 = {
            let params: [&dyn rusqlite::ToSql; 2] = [&kw, &kw];
            conn.query_row(
                &format!("SELECT COUNT(*) FROM torrents t {cond} "),
                rusqlite::params_from_iter(params.iter()),
                |r| r.get(0),
            )
            .unwrap_or(0)
        };
        let sql = format!(
            "SELECT t.id, t.info_hash, t.name, t.size,
                    COALESCE((SELECT COUNT(*) FROM ban_logs b WHERE b.torrent_hash = t.info_hash), 0),
                    COALESCE((SELECT COUNT(*) FROM peer_records p WHERE p.torrent_id = t.id), 0)
             FROM torrents t {cond}
             ORDER BY t.id DESC LIMIT ? OFFSET ?"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut query_params: Vec<&dyn rusqlite::ToSql> = Vec::new();
        if !cond.is_empty() {
            query_params.push(&kw);
            query_params.push(&kw);
        }
        query_params.push(&limit);
        query_params.push(&offset);
        let rows = stmt
            .query_map(rusqlite::params_from_iter(query_params), |row| {
                Ok(TorrentRow {
                    id: row.get(0)?,
                    info_hash: row.get(1)?,
                    name: row.get(2)?,
                    size: row.get(3)?,
                    peer_ban_count: row.get(4)?,
                    peer_access_count: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((rows, total))
    }

    /// 按 hash 查询单个种子统计。
    pub fn torrent_by_hash(&self, hash: &str) -> anyhow::Result<Option<TorrentRow>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT t.id, t.info_hash, t.name, t.size,
                        COALESCE((SELECT COUNT(*) FROM ban_logs b WHERE b.torrent_hash = t.info_hash), 0),
                        COALESCE((SELECT COUNT(*) FROM peer_records p WHERE p.torrent_id = t.id), 0)
                 FROM torrents t WHERE t.info_hash = ?1 LIMIT 1",
                rusqlite::params![hash],
                |row| {
                    Ok(TorrentRow {
                        id: row.get(0)?,
                        info_hash: row.get(1)?,
                        name: row.get(2)?,
                        size: row.get(3)?,
                        peer_ban_count: row.get(4)?,
                        peer_access_count: row.get(5)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// 在时间窗口内按天汇总封禁数（对齐上游 `banTrends`，时间戳按天向下取整）。
    pub fn ban_trends(
        &self,
        start_ms: i64,
        end_ms: i64,
        downloader: Option<&str>,
    ) -> anyhow::Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock().unwrap();
        let sql = if downloader.is_some() {
            "SELECT (created_at / 86400000) * 86400000 AS day, COUNT(*)
             FROM ban_logs WHERE created_at >= ?1 AND created_at < ?2 AND downloader_id = ?3
             GROUP BY day ORDER BY day"
        } else {
            "SELECT (created_at / 86400000) * 86400000 AS day, COUNT(*)
             FROM ban_logs WHERE created_at >= ?1 AND created_at < ?2
             GROUP BY day ORDER BY day"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = if let Some(d) = downloader {
            stmt.query_map(rusqlite::params![start_ms, end_ms, d], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(rusqlite::params![start_ms, end_ms], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    /// 字段统计（对齐上游 `/api/statistic/analysis/field` 的 `countOrSum` 两种模式）。
    ///
    /// `mode = "count"` 计数，`mode = "sum"` 求和。`field` 必须是 `ban_logs` 的白名单列，
    /// 非法列返回空列表（上游也会因 SQL 校验失败而返回空）。
    pub fn field_stats(
        &self,
        field: &str,
        mode: &str,
        percent_filter: f64,
        downloader: Option<&str>,
    ) -> anyhow::Result<Vec<(String, i64, f64)>> {
        const WHITELIST: &[&str] = &[
            "downloader_id", "torrent_name", "torrent_hash", "ip", "port", "peer_id",
            "client_name", "module", "rule", "reason",
        ];
        if !WHITELIST.contains(&field) {
            return Ok(Vec::new());
        }
        let (cond, d_param): (String, bool) = match downloader {
            Some(_) => (" AND downloader_id = ?3".to_string(), true),
            None => (String::new(), false),
        };
        let select = if mode.eq_ignore_ascii_case("sum") {
            format!("SUM({field})")
        } else {
            format!("COUNT({field})")
        };
        let sql = format!(
            "SELECT COALESCE(CAST({field} AS TEXT), 'unknown') AS k, {select} AS v
             FROM ban_logs WHERE 1=1{cond} GROUP BY k ORDER BY v DESC"
        );
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        let list: Vec<(String, i64, f64)> = if d_param {
            let d = downloader.unwrap_or_default();
            stmt.query_map(rusqlite::params![d], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(|(k, v)| (k, v, 0.0))
            .collect()
        } else {
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(|(k, v)| (k, v, 0.0))
                .collect()
        };
        let total: i64 = list.iter().map(|(_, v, _)| v).sum();
        let filtered: Vec<(String, i64, f64)> = list
            .into_iter()
            .filter(|(_, v, _)| {
                let pct = if total > 0 { *v as f64 / total as f64 } else { 0.0 };
                pct >= percent_filter
            })
            .map(|(k, v, _)| {
                let pct = if total > 0 { v as f64 / total as f64 } else { 0.0 };
                (k, v, pct)
            })
            .collect();
        Ok(filtered)
    }
}

// ---------- Web 历史 / 排行相关行类型（顶层，供 API 层引用）----------

/// 一条封禁历史（对齐上游 `HistoryEntity` 的 Web 视图；`unban_at` 为 0 表示未解封）。
#[derive(Debug, Clone)]
pub struct BanHistoryRow {
    pub id: i64,
    pub ban_at: i64,
    pub unban_at: i64,
    pub ip: String,
    pub port: i64,
    pub peer_id: String,
    pub peer_client_name: String,
    pub torrent_id: i64,
    pub torrent_info_hash: String,
    pub torrent_name: String,
    pub torrent_size: i64,
    pub module: String,
    pub rule: String,
    pub description: String,
    pub downloader: String,
}

/// 种子统计行（供 `/api/torrent/query` 列表与 `/api/torrent/{hash}`）。
#[derive(Debug, Clone)]
pub struct TorrentRow {
    pub id: i64,
    pub info_hash: String,
    pub name: String,
    pub size: i64,
    pub peer_ban_count: i64,
    pub peer_access_count: i64,
}

/// 一条 Peer 访问历史（`PeerRecord` 与 `Torrent` 的 JOIN 视图，对齐 `AccessHistoryDTO`）。
#[derive(Debug, Clone)]
pub struct AccessHistoryRow {
    pub id: i64,
    pub peer_ip: String,
    pub port: i64,
    pub torrent_id: i64,
    pub torrent_info_hash: String,
    pub torrent_name: String,
    pub torrent_size: i64,
    pub downloader: String,
    pub peer_id: String,
    pub client_name: String,
    pub uploaded: i64,
    pub downloaded: i64,
    pub upload_speed: i64,
    pub download_speed: i64,
    pub last_flags: String,
    pub first_time_seen: i64,
    pub last_time_seen: i64,
}

/// BTN 规则缓存的持久化位置（≈ 上游 `MetadataService` / `metadataDao`）。
///
/// 上游把规则集与 IP 列表缓存写进 metadata 表，重启后各 ability 的 `load()` 直接回灌、
/// 不重新拉取；这里复用已有的 `meta(key, value)` 键值表（与 `schema_version` 共存），
/// 键名逐字一致（`btn.ability.rules.cache` / `btn.ability.ip_*`）。
///
/// 所有 DB 失败一律 log-and-continue（对齐上游 DAO 外层 `catch (Throwable) + log`）：
/// 读失败按「无缓存」处理，写失败只丢缓存、不影响本轮已注入的规则。
pub struct DbMetadataStore {
    db: Arc<Database>,
}

impl std::fmt::Debug for DbMetadataStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("DbMetadataStore").finish_non_exhaustive()
    }
}

impl DbMetadataStore {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }
}

impl BtnMetadataStore for DbMetadataStore {
    fn get(&self, key: &str) -> Option<String> {
        match self.db.get_meta(key) {
            Ok(value) => value,
            Err(e) => {
                warn!("BTN 缓存读取失败（按无缓存处理）: {key} - {e}");
                None
            }
        }
    }

    fn set(&self, key: &str, value: &str) {
        if let Err(e) = self.db.set_meta(key, value) {
            warn!("BTN 缓存写入失败（本轮规则仍生效）: {key} - {e}");
        }
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

    /// BTN 规则缓存复用 `meta` 键值表（`BtnMetadataStore` 契约：get / set 覆盖写）
    #[test]
    fn metadata_store_persists_btn_cache() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let store: Arc<dyn BtnMetadataStore> = Arc::new(DbMetadataStore::new(db.clone()));
        assert_eq!(store.get("btn.ability.rules.cache"), None);
        store.set("btn.ability.rules.cache", r#"{"version":"v1"}"#);
        assert_eq!(store.get("btn.ability.rules.cache").as_deref(), Some(r#"{"version":"v1"}"#));
        // 覆盖写（上游 metadataDao 的 upsert）
        store.set("btn.ability.rules.cache", r#"{"version":"v2"}"#);
        assert_eq!(store.get("btn.ability.rules.cache").as_deref(), Some(r#"{"version":"v2"}"#));
        // 与 `schema_version` 共用同一张表，互不干扰
        assert_eq!(db.get_meta("schema_version").unwrap().as_deref(), Some("1"));
        assert_eq!(db.get_meta("btn.ability.rules.cache").unwrap().as_deref(), Some(r#"{"version":"v2"}"#));
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
