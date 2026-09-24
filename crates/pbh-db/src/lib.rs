//! SQLite 持久化：封禁日志、封禁列表、PCB 历史（SPEC 第 8 节）与监控数据
//! （[`monitor`] 子模块，SPEC 第 9 节，对齐上游 `databasent` 的 `alert` /
//! `traffic_journal_v3` / `peer_connection_metrics(_track)` / `peer_records` / `tracked_swarm`）。

pub mod btn_source;
pub mod monitor;

pub use btn_source::{DbBtnSubmitSource, HistoryRecord};
pub use monitor::{peer_geoip_json, AlertRow, DbMonitorSink};

use chrono::{DateTime, Utc};
use pbh_core::btn_transport::BtnMetadataStore;
use pbh_core::model::TorrentData;
use pbh_core::modules::progress_cheat::{PcbEntityKind, PcbPersistRow};
use rusqlite::{Connection, OptionalExtension};
use std::sync::{Arc, Mutex};
use tracing::warn;

pub const SCHEMA_VERSION: i64 = 1;

/// `history` 表的一行 + `torrents` 表 JOIN 结果。
///
/// 字段即上游 `BanLogDTO` / `BtnBan` 的取值来源；`rule` / `description` 存
/// `TranslationComponent` 的 JSON，读取方按需渲染（对齐 `TranslationComponentTypeHandler`）。
#[derive(Clone, Debug)]
pub struct HistoryRow {
    pub id: i64,
    pub ban_at: i64,
    pub unban_at: i64,
    pub ip: String,
    pub port: i64,
    pub peer_id: Option<String>,
    pub peer_client_name: Option<String>,
    pub peer_uploaded: Option<i64>,
    pub peer_downloaded: Option<i64>,
    pub peer_progress: f64,
    pub downloader_progress: f64,
    pub module: String,
    pub rule: String,
    pub description: String,
    pub flags: Option<String>,
    pub downloader: String,
    pub structured_data: Option<String>,
    pub peer_geoip: Option<String>,
    /// `torrents.info_hash`（JOIN 不到时为 `None`，对齐上游 `TorrentEntityDTO.from(null)` 的容忍）
    pub torrent_info_hash: Option<String>,
    pub torrent_name: Option<String>,
    pub torrent_size: i64,
}

/// `rule_sub_log` 的一行（规则订阅更新日志）。
#[derive(Clone, Debug)]
pub struct RuleSubLogRow {
    pub id: i64,
    pub rule_id: String,
    pub update_time: i64,
    pub count: i64,
    /// `AUTO` / `MANUAL`（对齐 `IPBanRuleUpdateType`）
    pub update_type: String,
}

/// `rule_sub_info` 的一行（规则订阅当前状态）。
#[derive(Clone, Debug)]
pub struct RuleSubInfoRow {
    pub rule_id: String,
    pub enabled: bool,
    pub rule_name: String,
    pub sub_url: String,
    pub last_update: Option<i64>,
    pub ent_count: Option<i64>,
}

/// `history` 的读取列（含 `torrents` LEFT JOIN；顺序与 [`map_history_row`] 一致）。
const HISTORY_SELECT_SQL: &str = "SELECT h.id, h.ban_at, h.unban_at, h.ip, h.port, h.peer_id, \
     h.peer_client_name, h.peer_uploaded, h.peer_downloaded, h.peer_progress, \
     h.downloader_progress, h.module_name, h.rule_name, h.description, h.flags, h.downloader, \
     h.structured_data, h.peer_geoip, t.info_hash, t.name, t.size \
     FROM history h LEFT JOIN torrents t ON t.id = h.torrent_id";

/// `history` 允许排序的列（对齐上游 `Orderable` 只用表自身的列名，防注入）。
const HISTORY_ORDER_COLUMNS: &[&str] = &[
    "id",
    "ban_at",
    "unban_at",
    "ip",
    "port",
    "peer_uploaded",
    "peer_downloaded",
    "peer_progress",
    "downloader_progress",
    "module_name",
    "downloader",
];

fn map_history_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryRow> {
    Ok(HistoryRow {
        id: row.get(0)?,
        ban_at: row.get(1)?,
        unban_at: row.get(2)?,
        ip: row.get(3)?,
        port: row.get(4)?,
        peer_id: row.get(5)?,
        peer_client_name: row.get(6)?,
        peer_uploaded: row.get(7)?,
        peer_downloaded: row.get(8)?,
        peer_progress: row.get(9)?,
        downloader_progress: row.get(10)?,
        module: row.get(11)?,
        rule: row.get(12)?,
        description: row.get(13)?,
        flags: row.get(14)?,
        downloader: row.get(15)?,
        structured_data: row.get(16)?,
        peer_geoip: row.get(17)?,
        torrent_info_hash: row.get(18)?,
        torrent_name: row.get(19)?,
        torrent_size: row.get::<_, Option<i64>>(20)?.unwrap_or(0),
    })
}

pub struct Database {
    conn: Mutex<Connection>,
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// 本地时区当日 0 点的 epoch 毫秒（对齐上游 `TimeUtil.getStartOfToday`）。
///
/// 上游按 JVM 默认时区截断；世界协调时截断会让「按天分桶」与上游错开一个时区偏移。
pub(crate) fn start_of_today_ms(ts_ms: i64) -> i64 {
    use chrono::{Local, TimeZone};
    let Some(local) = Local.timestamp_millis_opt(ts_ms).single() else {
        return ts_ms;
    };
    local
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .and_then(|naive| naive.and_local_timezone(Local).single())
        .map(|midnight| midnight.timestamp_millis())
        .unwrap_or(ts_ms)
}

/// 上游 `HistoryServiceImpl.mapField`：驼峰字段 → `history`（JOIN `torrents`）的列名。
///
/// `peerId` 按上游传入的 `substringLength = 8` 截断；未知字段返回 `None`
/// （上游会因字段名校验失败而返回空结果）。
fn history_field_column(field: &str) -> Option<String> {
    let mapped = match field {
        "torrentName" | "torrent_name" => "t.name",
        "module" | "moduleName" | "module_name" => "h.module_name",
        "clientName" | "peerClientName" | "client_name" | "peer_client_name" => {
            "h.peer_client_name"
        }
        "peerIp" | "ip" => "h.ip",
        "port" => "h.port",
        "peerId" | "peer_id" => "substr(h.peer_id, 1, 8)",
        "peerUploaded" | "peer_uploaded" => "h.peer_uploaded",
        "peerDownloaded" | "peer_downloaded" => "h.peer_downloaded",
        "peerProgress" | "peer_progress" => "h.peer_progress",
        "time" => "h.ban_at",
        "downloader" | "downloader_id" => "h.downloader",
        "rule" => "h.rule_name",
        "description" => "h.description",
        "torrentHash" | "torrent_hash" => "h.torrent_id",
        _ => return None,
    };
    Some(mapped.to_string())
}

/// `torrents` 表按 info_hash 取一行（`TorrentServiceImpl.queryByInfoHash`）。
pub(crate) struct TorrentLookup {
    pub(crate) id: i64,
    pub(crate) size: i64,
    pub(crate) private_torrent: Option<bool>,
}

pub(crate) fn select_torrent(
    conn: &Connection,
    info_hash: &str,
) -> anyhow::Result<Option<TorrentLookup>> {
    let mut stmt = conn.prepare(
        "SELECT id, size, private_torrent FROM torrents WHERE info_hash = ?1 ORDER BY id LIMIT 1",
    )?;
    Ok(stmt
        .query_row(rusqlite::params![info_hash], |row| {
            Ok(TorrentLookup {
                id: row.get(0)?,
                size: row.get(1)?,
                private_torrent: row.get::<_, Option<i64>>(2)?.map(|v| v != 0),
            })
        })
        .optional()?)
}

/// 老库（本移植早期版本的自建表）向「对齐上游」的表结构做一次性搬运。
///
/// 仅当旧表存在且新表为空时执行；旧表保留不删（便于人工回滚/排查）。
/// 封禁列表的完整 `BanMetadata` 已不可恢复，用最小结构重建（保留地址/时间/模块）。
fn migrate_legacy_tables(conn: &Connection) -> anyhow::Result<()> {
    if table_exists(conn, "meta")? {
        let _ = conn.execute(
            "INSERT OR IGNORE INTO metadata(k, v) SELECT key, value FROM meta",
            [],
        );
    }
    if table_exists(conn, "pcb_addr")? && table_is_empty(conn, "pcb_address")? {
        let _ = conn.execute(
            "INSERT INTO pcb_address
               (ip, port, torrent_id, last_report_progress, last_report_uploaded,
                tracking_uploaded_increase_total, rewind_counter, progress_difference_counter,
                first_time_seen, last_time_seen, downloader, ban_delay_window_end_at,
                fast_pcb_test_execute_at, last_torrent_completed_size)
             SELECT key, port, torrent_id, last_report_progress, last_report_uploaded,
                    tracking_uploaded_increase_total, rewind_counter, progress_difference_counter,
                    last_time_seen_ms, last_time_seen_ms, downloader_id, ban_delay_window_end_ms,
                    CASE WHEN fast_pcb_test_executed != 0 THEN last_time_seen_ms ELSE 0 END,
                    last_torrent_completed_size
             FROM pcb_addr",
            [],
        );
    }
    if table_exists(conn, "pcb_range")? && table_is_empty(conn, "pcb_range")? {
        let _ = conn.execute(
            "INSERT INTO pcb_range
               (ip_range, torrent_id, last_report_progress, last_report_uploaded,
                tracking_uploaded_increase_total, rewind_counter, progress_difference_counter,
                first_time_seen, last_time_seen, downloader, ban_delay_window_end_at,
                fast_pcb_test_execute_at, last_torrent_completed_size)
             SELECT key, torrent_id, last_report_progress, last_report_uploaded,
                    tracking_uploaded_increase_total, rewind_counter, progress_difference_counter,
                    last_time_seen_ms, last_time_seen_ms, downloader_id, ban_delay_window_end_ms,
                    CASE WHEN fast_pcb_test_executed != 0 THEN last_time_seen_ms ELSE 0 END,
                    last_torrent_completed_size
             FROM pcb_range",
            [],
        );
    }
    if table_exists(conn, "banned_ips")? && table_is_empty(conn, "banlist")? {
        let mut stmt = conn.prepare(
            "SELECT ip, first_banned_at, last_banned_at, module, hit_count, ban_until
             FROM banned_ips",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, i64>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (ip, first_banned_at, last_banned_at, module, hit_count, ban_until) in rows {
            let metadata = serde_json::json!({
                "context": module,
                "banAt": first_banned_at,
                "unbanAt": ban_until,
                "banForDisconnect": false,
                "excludeFromReport": false,
                "excludeFromDisplay": false,
                "rule": { "key": module, "params": [] },
                "description": { "key": module, "params": [] },
                "hitCount": hit_count,
                "lastBanTime": last_banned_at,
            });
            let _ = conn.execute(
                "INSERT OR IGNORE INTO banlist (address, metadata) VALUES (?1, ?2)",
                rusqlite::params![ip, metadata.to_string()],
            );
        }
    }
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            rusqlite::params![name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn table_is_empty(conn: &Connection, name: &str) -> anyhow::Result<bool> {
    let count: i64 = conn.query_row(&format!("SELECT COUNT(*) FROM {name}"), [], |r| r.get(0))?;
    Ok(count == 0)
}

impl Database {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(include_str!("schema.sql"))?;
        migrate_legacy_tables(&conn)?;
        // 增量迁移（老库缺列时补齐；新库已含列，忽略重复列错误）
        for sql in [
            "ALTER TABLE ban_logs ADD COLUMN rule_key TEXT",
            "ALTER TABLE ban_logs ADD COLUMN reason_key TEXT",
        ] {
            let _ = conn.execute(sql, []);
        }
        conn.execute(
            "INSERT OR IGNORE INTO metadata(k, v) VALUES ('schema_version', ?1)",
            rusqlite::params![SCHEMA_VERSION],
        )?;
        Ok(())
    }

    // ---------- torrents（TorrentService.createIfNotExists） ----------

    /// `TorrentService.createIfNotExists`：返回 `torrents` 表主键。
    ///
    /// 与 [`monitor::DbMonitorSink`] 的 `ensure_torrent` 同一语义（复用同一段 SQL）；
    /// 上游由 `PersistMetrics.recordPeerBan` 与监控模块共用，此处一并提升到 `Database`。
    pub fn ensure_torrent_id(&self, torrent: &TorrentData) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = select_torrent(&conn, &torrent.hash)? {
            // 已存在且（记录完整 或 传入数据更差）-> 直接复用，不回写
            let existing_is_complete = existing.size > 0 && existing.private_torrent.is_some();
            let incoming_is_poor = torrent.total_size <= 0 && torrent.is_private.is_none();
            if existing_is_complete || incoming_is_poor {
                return Ok(existing.id);
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
            rusqlite::params![
                torrent.hash,
                torrent.name,
                torrent.total_size,
                torrent.is_private.map(i64::from),
            ],
        )?;
        select_torrent(&conn, &torrent.hash)?
            .map(|row| row.id)
            .ok_or_else(|| anyhow::anyhow!("torrents upsert 后取不到行: {}", torrent.hash))
    }

    // ---------- 封禁历史（`history`，对齐上游 `HistoryService`） ----------

    /// WebUI 封禁日志分页（`/api/bans/logs`）。
    ///
    /// `order` 为 `(history 列名, 是否升序)` 列表（调用方先用
    /// [`Database::history_order_column`] 把 DTO 字段名映射为列名）；空列表按
    /// 上游默认 `ban_at DESC`。
    pub fn page_history(
        &self,
        order: &[(String, bool)],
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<(Vec<HistoryRow>, i64)> {
        let mut clauses: Vec<String> = Vec::new();
        for (column, asc) in order {
            if !HISTORY_ORDER_COLUMNS.contains(&column.as_str()) {
                continue;
            }
            clauses.push(format!("h.{column} {}", if *asc { "ASC" } else { "DESC" }));
        }
        if clauses.is_empty() {
            clauses.push("h.ban_at DESC".to_string());
        }
        let sql = format!(
            "{HISTORY_SELECT_SQL} ORDER BY {} LIMIT ?1 OFFSET ?2",
            clauses.join(", ")
        );
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn.query_row("SELECT COUNT(*) FROM history", [], |r| r.get(0))?;
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![limit, offset], map_history_row)?;
        Ok((rows.collect::<Result<Vec<_>, _>>()?, total))
    }

    /// 指定 torrent（info_hash）的封禁历史（`GET /api/torrent/{info_hash}/banHistory`）。
    ///
    /// `history` 通过 `torrent_id` 关联 `torrents`，按 info_hash 过滤需要 JOIN。
    pub fn history_by_torrent(
        &self,
        info_hash: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<(Vec<HistoryRow>, i64)> {
        let sql = format!(
            "{HISTORY_SELECT_SQL} WHERE t.info_hash = ?1 ORDER BY h.ban_at DESC LIMIT ?2 OFFSET ?3"
        );
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM history h JOIN torrents t ON t.id = h.torrent_id
             WHERE t.info_hash = ?1",
            rusqlite::params![info_hash],
            |r| r.get(0),
        )?;
        let mut rows: Vec<HistoryRow> = Vec::new();
        {
            let mut stmt = conn.prepare(&sql)?;
            let iter =
                stmt.query_map(rusqlite::params![info_hash, limit, offset], map_history_row)?;
            for row in iter {
                rows.push(row?);
            }
        }
        Ok((rows, total))
    }

    /// 指定 IP 的封禁历史（`GET /api/peer/{ip}/banHistory`），按 `ban_at` 倒序分页。
    pub fn history_by_ip(
        &self,
        ip: &str,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<(Vec<HistoryRow>, i64)> {
        let sql = format!(
            "{HISTORY_SELECT_SQL} WHERE h.ip = ?1 ORDER BY h.ban_at DESC LIMIT ?2 OFFSET ?3"
        );
        let conn = self.conn.lock().unwrap();
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM history WHERE ip = ?1",
            rusqlite::params![ip],
            |r| r.get(0),
        )?;
        let mut rows: Vec<HistoryRow> = Vec::new();
        {
            let mut stmt = conn.prepare(&sql)?;
            let iter = stmt.query_map(rusqlite::params![ip, limit, offset], map_history_row)?;
            for row in iter {
                rows.push(row?);
            }
        }
        Ok((rows, total))
    }

    /// 指定 IP 的封禁次数（`/api/peer/{ip}` 的 `banCount`）。
    pub fn history_count_by_ip(&self, ip: &str) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM history WHERE ip = ?1",
            rusqlite::params![ip],
            |r| r.get(0),
        )?)
    }

    /// 最近一条历史（`/api/bans` 列表的上下文兜底；对齐上游从 `history` 取最后一条的展示语义）。
    pub fn last_history_by_ip(&self, ip: &str) -> anyhow::Result<Option<HistoryRow>> {
        let sql = format!("{HISTORY_SELECT_SQL} WHERE h.ip = ?1 ORDER BY h.ban_at DESC LIMIT 1");
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&sql)?;
        Ok(stmt
            .query_row(rusqlite::params![ip], map_history_row)
            .optional()?)
    }

    /// `HistoryService.getBannedIps`：按 IP 聚合的封禁次数排行（`/api/bans/ranks`）。
    ///
    /// `filter` 为 IP 模糊匹配（`LIKE %filter%`）；排序固定 `COUNT(*) DESC`。
    pub fn page_history_rank(
        &self,
        filter: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<(Vec<(String, i64)>, i64)> {
        let conn = self.conn.lock().unwrap();
        let (total, sql): (i64, String) = match filter {
            Some(_) => (
                conn.query_row(
                    "SELECT COUNT(DISTINCT ip) FROM history WHERE ip LIKE ?1 ESCAPE '\\'",
                    rusqlite::params![format!("%{}%", filter.unwrap_or_default())],
                    |r| r.get(0),
                )?,
                "SELECT ip, COUNT(*) AS c FROM history WHERE ip LIKE ?1 ESCAPE '\\' \
                 GROUP BY ip ORDER BY c DESC LIMIT ?2 OFFSET ?3"
                    .to_string(),
            ),
            None => (
                conn.query_row("SELECT COUNT(DISTINCT ip) FROM history", [], |r| r.get(0))?,
                "SELECT ip, COUNT(*) AS c FROM history GROUP BY ip ORDER BY c DESC LIMIT ?1 OFFSET ?2"
                    .to_string(),
            ),
        };
        let mut stmt = conn.prepare(&sql)?;
        let rows = match filter {
            Some(keyword) => stmt
                .query_map(
                    rusqlite::params![format!("%{keyword}%"), limit, offset],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                )?
                .collect::<Result<Vec<_>, _>>()?,
            None => stmt
                .query_map(rusqlite::params![limit, offset], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?,
        };
        Ok((rows, total))
    }

    /// 删除 `ban_at` 早于 `keep_days` 天的历史（`persist.ban-logs-keep-days`），返回删除条数。
    ///
    /// 边界对齐上游 `HistoryServiceImpl` 的 `le(ban_at, threshold)`（**闭区间**）。
    pub fn cleanup_history(&self, keep_days: i64) -> anyhow::Result<usize> {
        let cutoff = now_ms() - keep_days * 86_400_000;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn.execute(
            "DELETE FROM history WHERE ban_at <= ?1",
            rusqlite::params![cutoff],
        )?)
    }

    /// 删除 `keep_days` 天前的告警（上游 `AlertServiceImpl.deleteOldAlerts`，闭区间）。
    ///
    /// 上游由 `AlertManagerImpl` 每天调度一次；缺失这一步会让 `alert` 表与
    /// `/api/alerts` 的未读列表无限增长。
    pub fn cleanup_alerts(&self, keep_days: i64) -> anyhow::Result<usize> {
        let cutoff = now_ms() - keep_days * 86_400_000;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn.execute(
            "DELETE FROM alert WHERE create_at <= ?1",
            rusqlite::params![cutoff],
        )?)
    }

    // ---------- 持久化封禁列表（`banlist`，对齐上游 `BanListService`） ----------

    /// `BanListService.readBanList`：读回 `(address, BanMetadata JSON)` 列表。
    pub fn read_ban_list(&self) -> anyhow::Result<Vec<(String, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT address, metadata FROM banlist")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// `BanListService.saveBanList`：整表替换（事务内先清空再写入，返回写入条数）。
    pub fn save_ban_list(&self, entries: &[(String, String)]) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM banlist", [])?;
        let mut written = 0usize;
        {
            let mut stmt = tx.prepare("INSERT INTO banlist (address, metadata) VALUES (?1, ?2)")?;
            for (address, metadata) in entries {
                written += stmt.execute(rusqlite::params![address, metadata])?;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// 清空持久化封禁列表（`DELETE /api/bans` 的 `*` 落库部分）。
    pub fn clear_ban_list(&self) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute("DELETE FROM banlist", [])?)
    }

    // ---------- 键值元数据（`metadata`，对齐上游 `MetadataService`） ----------

    /// 读回一个键（不存在 ⇒ `None`）。
    pub fn get_meta(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                "SELECT v FROM metadata WHERE k=?1",
                rusqlite::params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// 写入/覆盖一个键（upsert）。
    pub fn set_meta(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO metadata(k, v) VALUES (?1, ?2)
             ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    // ---------- 规则订阅（`rule_sub_info` / `rule_sub_log`，对齐上游 `RuleSub*Service`） ----------

    /// `RuleSubLogService.save`：追加一条规则更新日志。
    pub fn insert_rule_sub_log(
        &self,
        rule_id: &str,
        count: usize,
        update_type: &str,
        now_ms: i64,
    ) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO rule_sub_log (rule_id, update_time, count, update_type)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![rule_id, now_ms, count as i64, update_type],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 规则更新日志（按 `update_time` 倒序，WebUI 展示历史）。
    pub fn list_rule_sub_log(
        &self,
        rule_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<RuleSubLogRow>> {
        let conn = self.conn.lock().unwrap();
        let mut rows: Vec<RuleSubLogRow> = Vec::new();
        let offset = offset.max(0);
        if let Some(id) = rule_id {
            let mut stmt = conn.prepare(
                "SELECT id, rule_id, update_time, count, update_type FROM rule_sub_log
                 WHERE rule_id = ?1 ORDER BY update_time DESC LIMIT ?2 OFFSET ?3",
            )?;
            let iter = stmt.query_map(rusqlite::params![id, limit, offset], map_rule_sub_log)?;
            for row in iter {
                rows.push(row?);
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, rule_id, update_time, count, update_type FROM rule_sub_log
                 ORDER BY update_time DESC LIMIT ?1 OFFSET ?2",
            )?;
            let iter = stmt.query_map(rusqlite::params![limit, offset], map_rule_sub_log)?;
            for row in iter {
                rows.push(row?);
            }
        }
        Ok(rows)
    }

    /// `RuleSubInfoService.save`：写入/覆盖规则订阅状态。
    pub fn upsert_rule_sub_info(
        &self,
        rule_id: &str,
        enabled: bool,
        rule_name: &str,
        sub_url: &str,
        last_update: Option<i64>,
        ent_count: Option<i64>,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO rule_sub_info (rule_id, enabled, rule_name, sub_url, last_update, ent_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(rule_id) DO UPDATE SET
               enabled=excluded.enabled,
               rule_name=excluded.rule_name,
               sub_url=excluded.sub_url,
               last_update=excluded.last_update,
               ent_count=excluded.ent_count",
            rusqlite::params![rule_id, enabled as i64, rule_name, sub_url, last_update, ent_count],
        )?;
        Ok(())
    }

    /// 规则订阅日志总数（供 WebUI 分页）。
    pub fn count_rule_sub_log(&self, rule_id: Option<&str>) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = match rule_id {
            Some(id) => conn.query_row(
                "SELECT COUNT(*) FROM rule_sub_log WHERE rule_id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )?,
            None => conn.query_row("SELECT COUNT(*) FROM rule_sub_log", [], |r| r.get(0))?,
        };
        Ok(count)
    }

    /// 读取单条规则订阅的当前状态（对齐上游 `RuleSubInfoService.get`）。
    pub fn get_rule_sub_info(&self, rule_id: &str) -> anyhow::Result<Option<RuleSubInfoRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT rule_id, enabled, rule_name, sub_url, last_update, ent_count \
             FROM rule_sub_info WHERE rule_id = ?1",
        )?;
        let mut iter = stmt.query_map(rusqlite::params![rule_id], |r| {
            Ok(RuleSubInfoRow {
                rule_id: r.get(0)?,
                enabled: r.get::<_, i64>(1)? != 0,
                rule_name: r.get(2)?,
                sub_url: r.get(3)?,
                last_update: r.get(4)?,
                ent_count: r.get(5)?,
            })
        })?;
        Ok(iter.next().transpose()?)
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
            // `pcb_address` 按 (ip, port, torrent_id, downloader) 唯一；`pcb_range` 无 port 列，
            // 唯一键为 (ip_range, torrent_id, downloader)（均对齐上游实体/索引）。
            if row.kind.is_addr() {
                written += tx.execute(
                    "INSERT INTO pcb_address
                       (ip, port, torrent_id, last_report_progress, last_report_uploaded,
                        tracking_uploaded_increase_total, rewind_counter, progress_difference_counter,
                        first_time_seen, last_time_seen, downloader, ban_delay_window_end_at,
                        fast_pcb_test_execute_at, last_torrent_completed_size)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)
                     ON CONFLICT(ip, port, torrent_id, downloader) DO UPDATE SET
                       last_report_progress=excluded.last_report_progress,
                       last_report_uploaded=excluded.last_report_uploaded,
                       tracking_uploaded_increase_total=excluded.tracking_uploaded_increase_total,
                       rewind_counter=excluded.rewind_counter,
                       progress_difference_counter=excluded.progress_difference_counter,
                       last_time_seen=excluded.last_time_seen,
                       ban_delay_window_end_at=excluded.ban_delay_window_end_at,
                       fast_pcb_test_execute_at=excluded.fast_pcb_test_execute_at,
                       last_torrent_completed_size=excluded.last_torrent_completed_size",
                    rusqlite::params![
                        row.key,
                        row.port as i64,
                        row.torrent_id,
                        row.last_report_progress,
                        row.last_report_uploaded,
                        row.tracking_uploaded_increase_total,
                        row.rewind_counter,
                        row.progress_difference_counter,
                        row.first_time_seen_ms,
                        row.last_time_seen_ms,
                        row.downloader_id,
                        row.ban_delay_window_end_ms,
                        // 上游 `fast_pcb_test_execute_at` 是时间戳列；本移植内部用布尔，
                        // 落库时换算为「执行时刻 = last_time_seen」或 0（未执行）。
                        if row.fast_pcb_test_executed { row.last_time_seen_ms } else { 0 },
                        row.last_torrent_completed_size,
                    ],
                )?;
            } else {
                written += tx.execute(
                    "INSERT INTO pcb_range
                       (ip_range, torrent_id, last_report_progress, last_report_uploaded,
                        tracking_uploaded_increase_total, rewind_counter, progress_difference_counter,
                        first_time_seen, last_time_seen, downloader, ban_delay_window_end_at,
                        fast_pcb_test_execute_at, last_torrent_completed_size)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)
                     ON CONFLICT(ip_range, torrent_id, downloader) DO UPDATE SET
                       last_report_progress=excluded.last_report_progress,
                       last_report_uploaded=excluded.last_report_uploaded,
                       tracking_uploaded_increase_total=excluded.tracking_uploaded_increase_total,
                       rewind_counter=excluded.rewind_counter,
                       progress_difference_counter=excluded.progress_difference_counter,
                       last_time_seen=excluded.last_time_seen,
                       ban_delay_window_end_at=excluded.ban_delay_window_end_at,
                       fast_pcb_test_execute_at=excluded.fast_pcb_test_execute_at,
                       last_torrent_completed_size=excluded.last_torrent_completed_size",
                    rusqlite::params![
                        row.key,
                        row.torrent_id,
                        row.last_report_progress,
                        row.last_report_uploaded,
                        row.tracking_uploaded_increase_total,
                        row.rewind_counter,
                        row.progress_difference_counter,
                        row.first_time_seen_ms,
                        row.last_time_seen_ms,
                        row.downloader_id,
                        row.ban_delay_window_end_ms,
                        if row.fast_pcb_test_executed { row.last_time_seen_ms } else { 0 },
                        row.last_torrent_completed_size,
                    ],
                )?;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// 读取某个类型（`pcb_address` / `pcb_range`）的全部持久化实体。
    pub fn load_pcb_rows(&self, kind: PcbEntityKind) -> anyhow::Result<Vec<PcbPersistRow>> {
        let conn = self.conn.lock().unwrap();
        let is_addr = kind.is_addr();
        let sql = if is_addr {
            "SELECT ip, port, torrent_id, last_report_progress, last_report_uploaded,
                    tracking_uploaded_increase_total, rewind_counter, progress_difference_counter,
                    first_time_seen, last_time_seen, downloader, ban_delay_window_end_at,
                    fast_pcb_test_execute_at, last_torrent_completed_size FROM pcb_address"
        } else {
            "SELECT ip_range, torrent_id, last_report_progress, last_report_uploaded,
                    tracking_uploaded_increase_total, rewind_counter, progress_difference_counter,
                    first_time_seen, last_time_seen, downloader, ban_delay_window_end_at,
                    fast_pcb_test_execute_at, last_torrent_completed_size FROM pcb_range"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| {
            // 两个分支的列序不同（`pcb_range` 没有 `port` 列）：
            // `base` 为 `torrent_id` 所在的下标，其余业务列紧随其后。
            let (key, port, base) = if is_addr {
                (r.get::<_, String>(0)?, r.get::<_, i64>(1)?, 2usize)
            } else {
                (r.get::<_, String>(0)?, 0i64, 1usize)
            };
            Ok(PcbPersistRow {
                kind,
                key,
                port: port.clamp(0, u16::MAX as i64) as u16,
                torrent_id: r.get(base)?,
                last_report_progress: r.get(base + 1)?,
                last_report_uploaded: r.get(base + 2)?,
                tracking_uploaded_increase_total: r.get(base + 3)?,
                rewind_counter: r.get(base + 4)?,
                progress_difference_counter: r.get(base + 5)?,
                first_time_seen_ms: r.get(base + 6)?,
                last_time_seen_ms: r.get(base + 7)?,
                downloader_id: r.get(base + 8)?,
                ban_delay_window_end_ms: r.get(base + 9)?,
                fast_pcb_test_executed: r.get::<_, i64>(base + 10)? != 0,
                last_torrent_completed_size: r.get(base + 11)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// 删除某个 `(downloader, torrent, ip)` 的 PCB 逐 IP 历史。
    ///
    /// 对齐上游 `@Subscribe onPeerUnBan` → `pcbAddressDao.deleteEntry(torrentId, addr)`：
    /// peer 解封后立即清掉它的进度历史（前缀聚合行按上游保持不动）。
    /// `pcb_address` 的主键含 `port`，这里按 IP 删除该下载器+种子下的全部端口。
    pub fn delete_pcb_addr(
        &self,
        downloader_id: &str,
        torrent_id: &str,
        ip: &str,
    ) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn.execute(
            "DELETE FROM pcb_address WHERE downloader = ?1 AND torrent_id = ?2 AND ip = ?3",
            rusqlite::params![downloader_id, torrent_id, ip],
        )?)
    }

    pub fn cleanup_pcb(&self, older_than_ms: i64) -> anyhow::Result<usize> {
        let conn = self.conn.lock().unwrap();
        let mut n = 0;
        for t in ["pcb_address", "pcb_range"] {
            n += conn.execute(
                &format!("DELETE FROM {t} WHERE last_time_seen < ?1"),
                rusqlite::params![older_than_ms],
            )?;
        }
        Ok(n)
    }

    // ---------- Web 历史 / 排行 / 统计查询（对齐上游 `HistoryMapper` 与 `PBH*Controller`）----------

    /// WebUI 通用排序白名单（`orderBy` 的 DTO 字段名 → `history` 列名）。
    ///
    /// 与上游 `PBHBanController.handleLogs` 的 `Orderable.addRemapping` 一致；
    /// 未知字段直接拒绝（返回 `None`），避免 SQL 注入。
    pub fn history_order_column(field: &str) -> Option<String> {
        let column = match field {
            "banAt" => "ban_at",
            "unbanAt" => "unban_at",
            "peerIp" | "ip" => "ip",
            "peerPort" | "port" => "port",
            "peerId" => "peer_id",
            "peerClientName" => "peer_client_name",
            "peerUploaded" => "peer_uploaded",
            "peerDownloaded" => "peer_downloaded",
            "peerProgress" => "peer_progress",
            "module" => "module_name",
            "rule" => "rule_name",
            "description" => "description",
            "downloader" => "downloader",
            "id" => "id",
            _ => return None,
        };
        Some(column.to_string())
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

    /// 最近 7 天的会话总连接数（对齐上游 `handleCounter` 的 `weeklySessions`）。
    ///
    /// 上游窗口是 `[getStartOfToday(now-7d), getStartOfToday(now)]`——两端都按**本地**
    /// 当日 0 点截断且为**闭区间**；只给 `timeframe_at >= now-7d` 会多算今天与 7 天前
    /// 的非整天数据。
    pub fn peer_session_count_week(&self, now_ms: i64) -> anyhow::Result<i64> {
        let start = start_of_today_ms(now_ms - 7 * 86_400_000);
        let end = start_of_today_ms(now_ms);
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn
            .query_row(
                "SELECT COALESCE(SUM(total_connections), 0) FROM peer_connection_metrics
                 WHERE timeframe_at >= ?1 AND timeframe_at <= ?2",
                rusqlite::params![start, end],
                |r| r.get(0),
            )
            .unwrap_or(0))
    }

    /// 当前 swarm 中唯一 IP 数量（`trackedSwarmCount` 计数）。
    ///
    /// 对齐上游 `trackedSwarmDao.count()` = `COUNT(*)`：与
    /// [`Database::tracked_swarm_count`]（`/api/modules/swarm-tracking`）口径一致。
    pub fn tracked_swarm_size(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn
            .query_row("SELECT COUNT(*) FROM tracked_swarm", [], |r| r.get(0))
            .unwrap_or(0))
    }

    /// `module + rule` 分组统计（`/api/statistic/rules`，数据源 `history`）。
    pub fn rule_stats(&self) -> anyhow::Result<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT module_name, rule_name, COUNT(*) AS c FROM history
             GROUP BY module_name, rule_name ORDER BY c DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
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
                Ok((
                    r.get::<_, i64>(0)?,
                    (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(rusqlite::params![start_ms, end_ms], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    (r.get::<_, i64>(1)?, r.get::<_, i64>(2)?),
                ))
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
                Ok((
                    r.get::<_, i64>(0)?,
                    [r.get::<_, i64>(1)?, r.get::<_, i64>(2)?],
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(rusqlite::params![start_ms, end_ms], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    [r.get::<_, i64>(1)?, r.get::<_, i64>(2)?],
                ))
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
            (
                "WHERE p.address LIKE ?1 || '%'".to_string(),
                vec![Box::new(ip.to_string())],
            )
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

        let count_sql = format!(
            "SELECT COUNT(*) FROM peer_records p LEFT JOIN torrents t ON t.id = p.torrent_id{cond}"
        );
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
        let mut query_params: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(|b| b.as_ref()).collect();
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
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        // 关键词命中「名称前缀」或「完整 info_hash」；两个条件各占一个占位符
        //（重复写 `?1` 会让占位符个数与实际绑定个数不符）。
        let has_kw = keyword.map(|kw| !kw.is_empty()).unwrap_or(false);
        let kw = keyword.unwrap_or("").to_string();
        let cond = if has_kw {
            "WHERE t.name LIKE ?1 || '%' OR t.info_hash = ?2"
        } else {
            ""
        };
        // 无关键词时 limit/offset 顺延为 ?1/?2
        let (p_limit, p_offset) = if has_kw { ("?3", "?4") } else { ("?1", "?2") };
        let total: i64 = {
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::new();
            if has_kw {
                params.push(&kw);
                params.push(&kw);
            }
            conn.query_row(
                &format!("SELECT COUNT(*) FROM torrents t {cond}"),
                rusqlite::params_from_iter(params),
                |r| r.get(0),
            )
            .unwrap_or(0)
        };
        // 封禁数取 `history.torrent_id`（上游 `HistoryService`），`ban_logs` 是废弃表
        let sql = format!(
            "SELECT t.id, t.info_hash, t.name, t.size,
                    COALESCE((SELECT COUNT(*) FROM history b WHERE b.torrent_id = t.id), 0),
                    COALESCE((SELECT COUNT(*) FROM peer_records p WHERE p.torrent_id = t.id), 0)
             FROM torrents t {cond}
             ORDER BY t.id DESC LIMIT {p_limit} OFFSET {p_offset}"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut query_params: Vec<&dyn rusqlite::ToSql> = Vec::new();
        if has_kw {
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

    /// 在时间窗口内按天汇总封禁数（对齐上游 `handleBanTrends`）。
    ///
    /// 上游数据源是 `HistoryEntity`（`history.ban_at` / `downloader`），分桶用
    /// `TimeUtil.getStartOfToday`（**本地时区**当日 0 点），窗口两端为闭区间。
    pub fn ban_trends(
        &self,
        start_ms: i64,
        end_ms: i64,
        downloader: Option<&str>,
    ) -> anyhow::Result<Vec<(i64, i64)>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let sql = if downloader.is_some() {
            "SELECT ban_at FROM history WHERE ban_at >= ?1 AND ban_at <= ?2 AND downloader = ?3"
        } else {
            "SELECT ban_at FROM history WHERE ban_at >= ?1 AND ban_at <= ?2"
        };
        let mut stmt = conn.prepare(sql)?;
        let map_fn = |r: &rusqlite::Row| r.get::<_, i64>(0);
        let stamps: Vec<i64> = if let Some(d) = downloader {
            stmt.query_map(rusqlite::params![start_ms, end_ms, d], map_fn)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(rusqlite::params![start_ms, end_ms], map_fn)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut buckets: std::collections::BTreeMap<i64, i64> = std::collections::BTreeMap::new();
        for ts in stamps {
            *buckets.entry(start_of_today_ms(ts)).or_insert(0) += 1;
        }
        Ok(buckets.into_iter().collect())
    }

    /// 字段统计（对齐上游 `/api/statistic/analysis/field` 的 `countOrSum` 两种模式）。
    ///
    /// `mode = "count"` 计数，`mode = "sum"` 求和。
    ///
    /// 数据源为 `history h LEFT JOIN torrents t`（`torrentName` 取 `t.name`，`peerId`
    /// 按上游 `substringLength=8` 截断）；字段名走上游 `mapField` 的驼峰→下划线映射
    /// 与白名单，非法列返回空列表（上游 SQL 校验失败同样是空）。
    pub fn field_stats(
        &self,
        field: &str,
        mode: &str,
        percent_filter: f64,
        downloader: Option<&str>,
    ) -> anyhow::Result<Vec<(String, i64, f64)>> {
        let Some(column) = history_field_column(field) else {
            return Ok(Vec::new());
        };
        let (cond, d_param): (String, bool) = match downloader {
            Some(_) => (" AND h.downloader = ?1".to_string(), true),
            None => (String::new(), false),
        };
        let select = if mode.eq_ignore_ascii_case("sum") {
            format!("SUM({column})")
        } else {
            format!("COUNT({column})")
        };
        let sql = format!(
            "SELECT COALESCE(CAST({column} AS TEXT), 'unknown') AS k, {select} AS v
             FROM history h LEFT JOIN torrents t ON h.torrent_id = t.id
             WHERE 1=1{cond} GROUP BY k ORDER BY v DESC"
        );
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
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
                let pct = if total > 0 {
                    *v as f64 / total as f64
                } else {
                    0.0
                };
                pct >= percent_filter
            })
            .map(|(k, v, _)| {
                let pct = if total > 0 {
                    v as f64 / total as f64
                } else {
                    0.0
                };
                (k, v, pct)
            })
            .collect();
        Ok(filtered)
    }
}

// ---------- Web 历史 / 排行相关行类型（顶层，供 API 层引用）----------

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
        formatter
            .debug_struct("DbMetadataStore")
            .finish_non_exhaustive()
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

fn map_rule_sub_log(row: &rusqlite::Row<'_>) -> rusqlite::Result<RuleSubLogRow> {
    Ok(RuleSubLogRow {
        id: row.get(0)?,
        rule_id: row.get(1)?,
        update_time: row.get(2)?,
        count: row.get(3)?,
        update_type: row.get(4)?,
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

    fn history_record(ip: &str) -> HistoryRecord {
        HistoryRecord {
            ban_at_ms: 1_700_000_000_000,
            unban_at_ms: 0,
            ip: ip.to_string(),
            port: 6881,
            peer_id: Some("peer".into()),
            peer_client_name: Some("client".into()),
            peer_uploaded: Some(1),
            peer_downloaded: Some(2),
            peer_progress: 0.1,
            downloader_progress: 0.2,
            torrent: torrent("hash-1"),
            module_name: "com.ghostchu.peerbanhelper.module.impl.rule.PeerIdBlacklist".to_string(),
            rule_name: r#"{"key":"MODULE_IPB_RULE","params":[]}"#.to_string(),
            description: r#"{"key":"MODULE_IPB_RULE_DESCRIPTION","params":[]}"#.to_string(),
            flags: Some("U".into()),
            downloader: "qb".into(),
            structured_data: None,
            peer_geoip: None,
        }
    }

    /// `history` 落库 + Web 查询（分页 / 按 IP / 排行 / 清理）。
    #[test]
    fn history_roundtrip_and_queries() {
        let db = Database::open_in_memory().unwrap();
        for i in 0..3 {
            db.insert_history(&history_record(&format!("1.2.3.{i}")))
                .unwrap();
        }
        db.insert_history(&history_record("1.2.3.1")).unwrap();

        let (rows, total) = db.page_history(&[], 10, 0).unwrap();
        assert_eq!(total, 4);
        assert_eq!(rows.len(), 4);
        // JOIN `torrents` 的字段可用于 `BanLogDTO`
        assert_eq!(rows[0].torrent_info_hash.as_deref(), Some("hash-1"));
        assert_eq!(rows[0].torrent_name.as_deref(), Some("示例种子"));
        assert_eq!(rows[0].torrent_size, 1024);

        let (by_ip, by_ip_total) = db.history_by_ip("1.2.3.1", 10, 0).unwrap();
        assert_eq!(by_ip.len(), 2);
        assert_eq!(by_ip_total, 2);
        assert_eq!(db.history_count_by_ip("1.2.3.1").unwrap(), 2);
        let (by_torrent, torrent_total) = db.history_by_torrent("hash-1", 10, 0).unwrap();
        assert_eq!(torrent_total, 4);
        assert_eq!(by_torrent.len(), 4);
        assert!(db.last_history_by_ip("1.2.3.1").unwrap().is_some());
        assert!(db.last_history_by_ip("1.2.3.9").unwrap().is_none());

        let (rank, rank_total) = db.page_history_rank(None, 10, 0).unwrap();
        assert_eq!(rank_total, 3);
        assert_eq!(rank[0], ("1.2.3.1".to_string(), 2));
        let (filtered, filtered_total) = db.page_history_rank(Some("1.2.3.2"), 10, 0).unwrap();
        assert_eq!(filtered_total, 1);
        assert_eq!(filtered.len(), 1);

        // 排序：白名单列名（调用方用 `history_order_column` 把 DTO 字段名映射为列名）
        assert_eq!(
            Database::history_order_column("peerIp").as_deref(),
            Some("ip")
        );
        let order = vec![("ip".to_string(), false)];
        let (rows, _) = db.page_history(&order, 10, 0).unwrap();
        assert_eq!(rows[0].ip, "1.2.3.2");
        // 非法字段被忽略，退回默认 `ban_at DESC`
        let (rows, _) = db
            .page_history(&[("nope".to_string(), true)], 10, 0)
            .unwrap();
        assert_eq!(rows.len(), 4);

        // 清理：`ban_at` 早于 cutoff 的行被删除
        assert_eq!(db.cleanup_history(3650).unwrap(), 0);
        assert_eq!(db.cleanup_history(0).unwrap(), 4);
        assert_eq!(db.page_history(&[], 10, 0).unwrap().1, 0);
    }

    /// `banlist` 整表替换（对齐 `BanListServiceImpl.saveBanList`）。
    #[test]
    fn ban_list_save_replaces_all_rows() {
        let db = Database::open_in_memory().unwrap();
        db.save_ban_list(&[("1.2.3.4".into(), r#"{"banAt":1}"#.into())])
            .unwrap();
        let rows = db.read_ban_list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "1.2.3.4");
        assert_eq!(rows[0].1, r#"{"banAt":1}"#);

        let written = db
            .save_ban_list(&[
                ("5.6.7.8".into(), r#"{"banAt":2}"#.into()),
                ("5.6.7.9".into(), r#"{"banAt":3}"#.into()),
            ])
            .unwrap();
        assert_eq!(written, 2);
        let rows = db.read_ban_list().unwrap();
        assert_eq!(rows.len(), 2, "整表替换：旧行不再存在");
        assert!(rows.iter().all(|(addr, _)| addr.starts_with("5.6.7")));

        assert_eq!(db.clear_ban_list().unwrap(), 2);
        assert!(db.read_ban_list().unwrap().is_empty());
    }

    /// `rule_sub_info` / `rule_sub_log` 往返（规则订阅状态与更新日志）。
    #[test]
    fn rule_sub_tables_roundtrip() {
        let db = Database::open_in_memory().unwrap();
        db.upsert_rule_sub_info(
            "all-in-one",
            true,
            "all-in-one",
            "https://bcr.pbh-btn.com/combine/all.txt",
            Some(1000),
            Some(42),
        )
        .unwrap();
        db.upsert_rule_sub_info(
            "all-in-one",
            true,
            "all-in-one",
            "https://bcr.pbh-btn.com/combine/all.txt",
            Some(2000),
            Some(43),
        )
        .unwrap();
        {
            let conn = db.conn.lock().unwrap();
            let (enabled, last, count): (i64, Option<i64>, Option<i64>) = conn
                .query_row(
                    "SELECT enabled, last_update, ent_count FROM rule_sub_info WHERE rule_id='all-in-one'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(enabled, 1);
            assert_eq!(last, Some(2000), "覆盖写");
            assert_eq!(count, Some(43));
        }

        db.insert_rule_sub_log("all-in-one", 42, "AUTO", 1000)
            .unwrap();
        db.insert_rule_sub_log("all-in-one", 43, "MANUAL", 2000)
            .unwrap();
        let logs = db.list_rule_sub_log(Some("all-in-one"), 10, 0).unwrap();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].count, 43, "按 update_time 倒序");
        assert_eq!(logs[0].update_type, "MANUAL");
        assert_eq!(db.list_rule_sub_log(None, 10, 0).unwrap().len(), 2);
        assert!(db
            .list_rule_sub_log(Some("other"), 10, 0)
            .unwrap()
            .is_empty());
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
            first_time_seen_ms: 111,
            last_time_seen_ms: 222,
            ..default_pcb_row()
        };
        assert_eq!(db.upsert_pcb_rows(std::slice::from_ref(&row)).unwrap(), 1);
        let loaded = db.load_pcb_rows(PcbEntityKind::Addr).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].tracking_uploaded_increase_total, 99);
        assert_eq!(loaded[0].port, 51413);
        assert_eq!(loaded[0].ban_delay_window_end_ms, 12_345);
        assert_eq!(loaded[0].first_time_seen_ms, 111);
        assert_eq!(loaded[0].last_time_seen_ms, 222);
        // 同一主键再次写入为更新而非新增
        assert_eq!(db.upsert_pcb_rows(std::slice::from_ref(&row)).unwrap(), 1);
        assert_eq!(db.load_pcb_rows(PcbEntityKind::Addr).unwrap().len(), 1);
        // range 表相互独立（列结构不同：无 port）
        assert!(db.load_pcb_rows(PcbEntityKind::Range).unwrap().is_empty());
        let range_row = PcbPersistRow {
            kind: PcbEntityKind::Range,
            key: "1.2.3.0/24".into(),
            port: 0,
            ..default_pcb_row()
        };
        assert_eq!(
            db.upsert_pcb_rows(std::slice::from_ref(&range_row))
                .unwrap(),
            1
        );
        let loaded = db.load_pcb_rows(PcbEntityKind::Range).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].key, "1.2.3.0/24");
    }

    /// BTN 规则缓存复用 `metadata` 键值表（`BtnMetadataStore` 契约：get / set 覆盖写）。
    #[test]
    fn metadata_store_persists_btn_cache() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let store: Arc<dyn BtnMetadataStore> = Arc::new(DbMetadataStore::new(db.clone()));
        assert_eq!(store.get("btn.ability.rules.cache"), None);
        store.set("btn.ability.rules.cache", r#"{"version":"v1"}"#);
        assert_eq!(
            store.get("btn.ability.rules.cache").as_deref(),
            Some(r#"{"version":"v1"}"#)
        );
        // 覆盖写（上游 metadataDao 的 upsert）
        store.set("btn.ability.rules.cache", r#"{"version":"v2"}"#);
        assert_eq!(
            store.get("btn.ability.rules.cache").as_deref(),
            Some(r#"{"version":"v2"}"#)
        );
        // 与 `schema_version` 共用同一张表，互不干扰
        assert_eq!(db.get_meta("schema_version").unwrap().as_deref(), Some("1"));
        assert_eq!(
            db.get_meta("btn.ability.rules.cache").unwrap().as_deref(),
            Some(r#"{"version":"v2"}"#)
        );
    }

    /// 老库（本移植早期自建表）向对齐上游的表结构做一次性搬运。
    #[test]
    fn legacy_tables_migrate_into_upstream_layout() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE banned_ips (
                 ip TEXT PRIMARY KEY, first_banned_at INTEGER NOT NULL, last_banned_at INTEGER NOT NULL,
                 module TEXT NOT NULL, hit_count INTEGER NOT NULL DEFAULT 1, ban_until INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE pcb_addr (
                 downloader_id TEXT NOT NULL, torrent_id TEXT NOT NULL, key TEXT NOT NULL,
                 port INTEGER NOT NULL, last_report_uploaded INTEGER NOT NULL DEFAULT 0,
                 tracking_uploaded_increase_total INTEGER NOT NULL DEFAULT 0,
                 last_report_progress REAL NOT NULL DEFAULT 0,
                 last_torrent_completed_size INTEGER NOT NULL DEFAULT 0,
                 progress_difference_counter INTEGER NOT NULL DEFAULT 0, rewind_counter INTEGER NOT NULL DEFAULT 0,
                 ban_delay_window_end_ms INTEGER NOT NULL DEFAULT 0,
                 fast_pcb_test_executed INTEGER NOT NULL DEFAULT 0,
                 last_time_seen_ms INTEGER NOT NULL DEFAULT 0,
                 PRIMARY KEY (downloader_id, torrent_id, key, port));
             INSERT INTO meta VALUES ('btn.ability.rules.cache', '{\"v\":1}');
             INSERT INTO banned_ips VALUES ('1.2.3.4', 100, 200, 'ip-address-blocker', 3, 300);
             INSERT INTO pcb_addr VALUES ('qb', 'h', '1.2.3.4', 6881, 1, 2, 0.5, 3, 4, 5, 6, 1, 700);",
        )
        .unwrap();
        conn.execute_batch(include_str!("schema.sql")).unwrap();
        migrate_legacy_tables(&conn).unwrap();

        let meta: String = conn
            .query_row(
                "SELECT v FROM metadata WHERE k='btn.ability.rules.cache'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(meta, r#"{"v":1}"#);

        let (address, metadata): (String, String) = conn
            .query_row("SELECT address, metadata FROM banlist", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(address, "1.2.3.4");
        assert!(metadata.contains("\"unbanAt\":300"), "{metadata}");
        assert!(
            metadata.contains("\"context\":\"ip-address-blocker\""),
            "{metadata}"
        );

        let (ip, port, downloader, first, last, delay): (String, i64, String, i64, i64, i64) = conn
            .query_row(
                "SELECT ip, port, downloader, first_time_seen, last_time_seen, ban_delay_window_end_at
                 FROM pcb_address",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .unwrap();
        assert_eq!(
            (ip.as_str(), port, downloader.as_str()),
            ("1.2.3.4", 6881, "qb")
        );
        assert_eq!(
            (first, last),
            (700, 700),
            "旧表无 first_time_seen，用 last_time_seen 兜底"
        );
        assert_eq!(delay, 6);

        // 再次迁移不产生重复行（新表非空即跳过）
        migrate_legacy_tables(&conn).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM banlist", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    /// 统计接口对齐上游 `HistoryServiceImpl` / `PBHMetricsController`：
    /// `ban_trends` 按本地当日 0 点分桶、`field_stats` 数据源为 `history`（JOIN `torrents`）、
    /// `peer_session_count_week` 窗口为「本地当日 0 点」的闭区间。
    #[test]
    fn statistics_queries_align_with_upstream_history_source() {
        let db = Database::open_in_memory().unwrap();
        let day = start_of_today_ms(1_700_000_000_000);
        let mut a = history_record("1.2.3.1");
        a.ban_at_ms = day + 1;
        let mut b = history_record("1.2.3.2");
        b.ban_at_ms = day + 2;
        let mut c = history_record("1.2.3.3");
        c.ban_at_ms = day + 86_400_001; // 次日
        db.insert_history(&a).unwrap();
        db.insert_history(&b).unwrap();
        db.insert_history(&c).unwrap();

        // 按本地当日 0 点分桶：同一天 2 条、次日 1 条（升序）
        let trends = db.ban_trends(0, 2_000_000_000_000, None).unwrap();
        assert_eq!(trends.len(), 2, "应按天分成 2 个桶");
        assert_eq!(trends[0].1, 2, "首日 2 条");
        assert_eq!(trends[1].1, 1, "次日 1 条");
        assert_eq!(trends[1].0 - trends[0].0, 86_400_000, "相邻桶间隔一天");

        // 按下载器过滤
        let scoped = db.ban_trends(0, 2_000_000_000_000, Some("qb")).unwrap();
        assert_eq!(scoped.len(), 2);
        let total: i64 = scoped.iter().map(|(_, n)| *n).sum();
        assert_eq!(total, 3);

        // 字段统计（rule_name）数据源为 history
        let stats = db.field_stats("rule", "count", 0.0, None).unwrap();
        assert!(!stats.is_empty(), "非法字段才返回空，rule 合法");
        let total: i64 = stats.iter().map(|(_, n, _)| *n).sum();
        assert_eq!(total, 3, "统计应覆盖 history 全部 3 行");

        // weeklySessions：窗口按本地当日 0 点闭区间；直接插入 metrics 行验证
        let now = day + 10_000; // 当天
        let start = start_of_today_ms(now - 7 * 86_400_000);
        let end = start_of_today_ms(now);
        {
            let conn = db.conn.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute(
                "INSERT INTO peer_connection_metrics (timeframe_at, downloader, total_connections, incoming_connections, remote_refuse_transfer_to_client, remote_accept_transfer_to_client, local_refuse_transfer_to_peer, local_accept_transfer_to_peer, local_not_interested, question_status, optimistic_unchoke, from_dht, from_pex, from_lsd, from_tracker_or_other, rc4_encrypted, plain_text_encrypted, utp_socket, tcp_socket)
                 VALUES (?1, 'qb', 5, 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0)",
                rusqlite::params![start + 1000],
            ).unwrap();
            conn.execute(
                "INSERT INTO peer_connection_metrics (timeframe_at, downloader, total_connections, incoming_connections, remote_refuse_transfer_to_client, remote_accept_transfer_to_client, local_refuse_transfer_to_peer, local_accept_transfer_to_peer, local_not_interested, question_status, optimistic_unchoke, from_dht, from_pex, from_lsd, from_tracker_or_other, rc4_encrypted, plain_text_encrypted, utp_socket, tcp_socket)
                 VALUES (?1, 'qb', 7, 0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0)",
                rusqlite::params![end],
            ).unwrap();
        }
        let weekly = db.peer_session_count_week(now).unwrap();
        assert_eq!(weekly, 12, "本周会话数应为两行之和（5 + 7）");
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
            first_time_seen_ms: 0,
        }
    }
}
