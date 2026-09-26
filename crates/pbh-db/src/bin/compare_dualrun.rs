//! 长时对跑的离线对账工具（长时并行对跑结束后，对两侧 SQLite 做封禁差异比对）。
//!
//! 用法：`cargo run -p pbh-db --bin compare_dualrun -- <java.db> <rust.db> [--since "2026-09-24"] [--out diff.csv]`
//!
//! 比对策略（见 PLAN「长时对跑基建」）：在线逐条比对不采用（时钟漂移 + 重试时序差异
//! 会在天级产生假阳性），以离线 DB 对账为准——`history` 表按 (IP, 端口, 小时桶) 匹配，
//! 时间列/列名两侧可能不同，先 `PRAGMA table_info` 探测再取候选列。

use std::collections::BTreeMap;
use std::process::ExitCode;

use rusqlite::Connection;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut java_db = String::new();
    let mut rust_db = String::new();
    let mut since = String::new();
    let mut out_csv = String::new();
    // `--ip-only`：键只保留 IP（去掉端口）。长跑对账中同一 IP 两侧常封到不同端口的
    // 连接（判定语义按 IP 封禁），按 `ip:port` 粒度比对会产生大量伪差异。
    let mut ip_only = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--since" => since = iter.next().cloned().unwrap_or_default(),
            "--out" => out_csv = iter.next().cloned().unwrap_or_default(),
            "--ip-only" => ip_only = true,
            other if java_db.is_empty() => java_db = other.to_string(),
            other => rust_db = other.to_string(),
        }
    }
    if java_db.is_empty() || rust_db.is_empty() {
        eprintln!(
            "用法: compare_dualrun <java.db> <rust.db> [--since YYYY-MM-DD] [--out diff.csv] [--ip-only]"
        );
        return ExitCode::from(2);
    }
    let (java, rust) = match (open(&java_db), open(&rust_db)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("打开数据库失败: {e}");
            return ExitCode::from(2);
        }
    };

    println!(
        "== 对账：java={java_db} rust={rust_db} since={since:?} ip_only={ip_only} =="
    );
    shared_row_counts(&java, "Java", &java_db);
    shared_row_counts(&rust, "Rust", &rust_db);

    let report = compare_history(&java, &rust, &since, ip_only);
    println!(
        "history: Java {} 条独有 / Rust {} 条独有 / {} 条同址不同模块",
        report.only_java.len(),
        report.only_rust.len(),
        report.module_mismatch.len()
    );
    if let Some(path) = (!out_csv.is_empty()).then_some(&out_csv) {
        match write_csv(path, &report) {
            Ok(()) => println!("差异已写入 {path}"),
            Err(e) => eprintln!("写 CSV 失败: {e}"),
        }
    } else {
        for (side, rows) in [("Java", &report.only_java), ("Rust", &report.only_rust)] {
            for key in rows.iter().take(20) {
                println!("[{side} 独有] {key}");
            }
        }
        for line in report.module_mismatch.iter().take(20) {
            println!("[模块不一致] {line}");
        }
    }
    // 退出码：0 = 封禁集合与模块判定一致；1 = 存在差异
    if report.is_consistent() {
        println!("结论：history 封禁集合与命中模块一致 ✔");
        ExitCode::SUCCESS
    } else {
        println!("结论：history 存在差异（详见上方/CSV）✘");
        ExitCode::from(1)
    }
}

fn open(path: &str) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
}

fn table_exists(conn: &Connection, table: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// 按候选顺序探测存在的列名。
fn pick_column(conn: &Connection, table: &str, candidates: &[&str]) -> Option<String> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})")).ok()?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .ok()?
        .flatten()
        .collect();
    candidates
        .iter()
        .find(|c| names.iter().any(|n| n.eq_ignore_ascii_case(c)))
        .map(|c| (*c).to_string())
}

/// 时间值归一化为「小时桶」（数值当毫秒、文本取 ISO 前 13 位），吸收两侧秒级时钟差。
fn hour_bucket(raw: rusqlite::types::Value) -> Option<String> {
    use rusqlite::types::Value as V;
    match raw {
        V::Integer(ms) => Some((ms / 3_600_000).to_string()),
        V::Text(s) => s.get(..13).map(|p| p.to_string()).or(Some(s)),
        _ => None,
    }
}

/// IP 文本规范化：Java（`InetAddress.getHostAddress`）与 Rust 的 IPv6 压缩/映射写法
/// 可能不同（例如 `::ffff:198.18.0.11` vs `::ffff:c612:b`），按 RFC 5952 统一后再入键，
/// 避免同址因表示法差异被误判为「独有」。无法解析时原样返回。
fn normalize_ip(raw: &str) -> String {
    use std::net::IpAddr;
    let trimmed = raw.trim().trim_start_matches('[').trim_end_matches(']');
    match trimmed.parse::<IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => trimmed.to_string(),
    }
}

/// 对账报告：两侧各自的「独有键」+「同址同小时但命中模块集合不相交」的键。
struct Report {
    only_java: Vec<String>,
    only_rust: Vec<String>,
    /// `key [java=…, rust=…]` 文本行：两侧都封了该地址，但命中的模块集合无交集
    module_mismatch: Vec<String>,
}

impl Report {
    fn is_consistent(&self) -> bool {
        self.only_java.is_empty() && self.only_rust.is_empty() && self.module_mismatch.is_empty()
    }
}

fn compare_history(java: &Connection, rust: &Connection, since: &str, ip_only: bool) -> Report {
    // 键 = `规范化IP:端口@小时桶`（`--ip-only` 时去掉端口）；值 = 该键上命中的模块集合
    // （两侧同用 Java 类全名）。长跑中同一 IP 两侧常封到不同端口的连接（封禁语义按 IP），
    // `--ip-only` 用于消除该端口粒度伪差异。
    let read_side =
        |conn: &Connection| -> Option<BTreeMap<String, std::collections::BTreeSet<String>>> {
            if !table_exists(conn, "history") {
                eprintln!("警告：该库没有 history 表，跳过 history 比对");
                return None;
            }
            let ip = pick_column(conn, "history", &["ip", "peer_ip", "ip_address", "address"])?;
            let port = pick_column(conn, "history", &["port", "peer_port"]);
            let time = pick_column(conn, "history", &["ban_at", "created_at", "timestamp"])?;
            let module = pick_column(conn, "history", &["module_name", "module"]);
            let port_expr = port.as_deref().unwrap_or("0");
            let module_expr = module.as_deref().unwrap_or("''");
            let mut sql = format!("SELECT {ip}, {port_expr}, {time}, {module_expr} FROM history");
            let mut params: Vec<String> = Vec::new();
            if !since.is_empty() && time.as_str() == "ban_at" {
                // 数值毫秒列才支持数值窗口；文本列全量读出后按前缀过滤
                sql.push_str(" WHERE ban_at >= ?1");
                let since_ms = chrono_like_day_start_ms(since);
                params.push(since_ms.to_string());
            }
            let mut map: BTreeMap<String, std::collections::BTreeSet<String>> = BTreeMap::new();
            let mut stmt = conn.prepare(&sql).ok()?;
            let mut rows = stmt.query(rusqlite::params_from_iter(params.iter())).ok()?;
            while let Some(row) = rows.next().ok()? {
                let ip: String = row.get(0).ok()?;
                let port: i64 = row.get(1).unwrap_or(0);
                let bucket = hour_bucket(row.get(2).ok()?).unwrap_or_default();
                let bucket = if !since.is_empty() && bucket.as_str() < since {
                    continue;
                } else {
                    bucket
                };
                let module: String = row.get(3).unwrap_or_default();
                let ip_norm = normalize_ip(&ip);
                let key = if ip_only {
                    format!("{ip_norm}@{bucket}")
                } else {
                    format!("{ip_norm}:{port}@{bucket}")
                };
                map.entry(key).or_default().insert(module);
            }
            Some(map)
        };
    let java_map = read_side(java).unwrap_or_default();
    let rust_map = read_side(rust).unwrap_or_default();
    let only_java: Vec<String> = java_map
        .keys()
        .filter(|k| !rust_map.contains_key(*k))
        .cloned()
        .collect();
    let only_rust: Vec<String> = rust_map
        .keys()
        .filter(|k| !java_map.contains_key(*k))
        .cloned()
        .collect();
    let module_mismatch: Vec<String> = java_map
        .keys()
        .filter(|k| rust_map.contains_key(*k))
        .filter_map(|k| {
            let j = &java_map[k];
            let r = &rust_map[k];
            if j.is_disjoint(r) {
                Some(format!("{k} [java={} | rust={}]", join_set(j), join_set(r)))
            } else {
                None
            }
        })
        .collect();
    Report {
        only_java,
        only_rust,
        module_mismatch,
    }
}

fn join_set(set: &std::collections::BTreeSet<String>) -> String {
    set.iter()
        .map(|s| {
            // 只保留类名末段，报告紧凑可读
            s.rsplit('.').next().unwrap_or(s).to_string()
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// `--since` 解析（**UTC**）：`YYYY-MM-DD`（当日零点）或 `YYYY-MM-DDTHH:MM[:SS]`（分钟级窗口，
/// 用于长跑对账只比对两侧同时在线的时段）；解析失败返回 0（= 不过滤）。
fn chrono_like_day_start_ms(date: &str) -> i64 {
    use chrono::{NaiveDate, NaiveDateTime, TimeZone};
    for fmt in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(date, fmt) {
            return chrono::Utc.from_utc_datetime(&dt).timestamp_millis();
        }
    }
    match NaiveDate::parse_from_str(date, "%Y-%m-%d").map(|d| d.and_hms_opt(0, 0, 0)) {
        Ok(Some(dt)) => chrono::Utc.from_utc_datetime(&dt).timestamp_millis(),
        _ => 0,
    }
}

fn write_csv(path: &str, report: &Report) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "side,key")?;
    for k in &report.only_java {
        writeln!(f, "java,{k}")?;
    }
    for k in &report.only_rust {
        writeln!(f, "rust,{k}")?;
    }
    for m in &report.module_mismatch {
        writeln!(f, "module-mismatch,{m}")?;
    }
    Ok(())
}

fn shared_row_counts(conn: &Connection, label: &str, path: &str) {
    println!("-- {label} ({path}) 共享表行数 --");
    for table in [
        "history",
        "alert",
        "torrents",
        "traffic_journal_v3",
        "peer_records",
        "tracked_swarm",
        "peer_connection_metrics",
    ] {
        if table_exists(conn, table) {
            let n: i64 = conn
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap_or(-1);
            println!("   {table}: {n}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_history() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE history (id INTEGER PRIMARY KEY, ip TEXT, port INTEGER, ban_at INTEGER, module_name TEXT);
             INSERT INTO history (ip, port, ban_at, module_name) VALUES
               ('1.2.3.4', 51413, 1789000000000, 'com.ghostchu.peerbanhelper.module.impl.rule.PeerIdBlacklist'),
               ('5.6.7.8', 80,    1789000060000, 'com.ghostchu.peerbanhelper.module.impl.rule.MultiDialingBlocker');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn identical_histories_have_no_diff() {
        let a = memory_history();
        let b = memory_history();
        let r = compare_history(&a, &b, "", false);
        assert!(r.is_consistent());
    }

    #[test]
    fn missing_rows_are_reported_per_side() {
        let a = memory_history();
        let b = Connection::open_in_memory().unwrap();
        b.execute_batch(
            "CREATE TABLE history (id INTEGER PRIMARY KEY, ip TEXT, port INTEGER, ban_at INTEGER, module_name TEXT);
             INSERT INTO history (ip, port, ban_at, module_name) VALUES ('1.2.3.4', 51413, 1789000000000, 'com.ghostchu.peerbanhelper.module.impl.rule.PeerIdBlacklist');",
        )
        .unwrap();
        let r = compare_history(&a, &b, "", false);
        assert_eq!(r.only_java.len(), 1, "Java 独有：5.6.7.8");
        assert!(r.only_rust.is_empty());
        assert!(r.module_mismatch.is_empty());
    }

    #[test]
    fn same_address_with_disjoint_modules_is_flagged() {
        let a = memory_history();
        let b = Connection::open_in_memory().unwrap();
        b.execute_batch(
            "CREATE TABLE history (id INTEGER PRIMARY KEY, ip TEXT, port INTEGER, ban_at INTEGER, module_name TEXT);
             INSERT INTO history (ip, port, ban_at, module_name) VALUES
               ('1.2.3.4', 51413, 1789000060000, 'com.ghostchu.peerbanhelper.module.impl.rule.MultiDialingBlocker');",
        )
        .unwrap();
        let r = compare_history(&a, &b, "", false);
        // 1.2.3.4 两侧都封了（同小时桶），但模块无交集 → 记一条模块不一致
        assert_eq!(r.module_mismatch.len(), 1);
        assert!(r.module_mismatch[0].contains("PeerIdBlacklist"));
        assert!(r.module_mismatch[0].contains("MultiDialingBlocker"));
    }

    #[test]
    fn since_supports_day_and_minute_precision() {
        // 2026-09-23T00:00:00Z
        assert_eq!(chrono_like_day_start_ms("2026-09-23"), 1_790_121_600_000);
        // 同日 12:31（分钟级窗口，长跑对账常用）
        assert_eq!(
            chrono_like_day_start_ms("2026-09-23T12:31"),
            1_790_121_600_000 + (12 * 60 + 31) * 60_000
        );
        // 非法输入 → 0 = 不过滤
        assert_eq!(chrono_like_day_start_ms("not-a-date"), 0);
    }

    #[test]
    fn equivalent_ipv6_text_forms_are_normalized() {
        // Java 十六进制映射写法 vs Rust 点分映射写法 → 规范化后同键，不算差异
        let a = Connection::open_in_memory().unwrap();
        let b = Connection::open_in_memory().unwrap();
        for (conn, ip) in [(&a, "::ffff:c612:b"), (&b, "::ffff:198.18.0.11")] {
            conn.execute_batch(&format!(
                "CREATE TABLE history (id INTEGER PRIMARY KEY, ip TEXT, port INTEGER, ban_at INTEGER, module_name TEXT);
                 INSERT INTO history (ip, port, ban_at, module_name) VALUES ('{ip}', 6881, 1789000000000, 'M');"
            ))
            .unwrap();
        }
        let r = compare_history(&a, &b, "", false);
        assert!(r.is_consistent(), "等价 IPv6 写法不应被记为差异");
    }
}
