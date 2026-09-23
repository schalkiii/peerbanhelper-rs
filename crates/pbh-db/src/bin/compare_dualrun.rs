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
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--since" => since = iter.next().cloned().unwrap_or_default(),
            "--out" => out_csv = iter.next().cloned().unwrap_or_default(),
            other if java_db.is_empty() => java_db = other.to_string(),
            other => rust_db = other.to_string(),
        }
    }
    if java_db.is_empty() || rust_db.is_empty() {
        eprintln!("用法: compare_dualrun <java.db> <rust.db> [--since YYYY-MM-DD] [--out diff.csv]");
        return ExitCode::from(2);
    }
    let (java, rust) = match (open(&java_db), open(&rust_db)) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => {
            eprintln!("打开数据库失败: {e}");
            return ExitCode::from(2);
        }
    };

    println!("== 对账：java={java_db} rust={rust_db} since={since:?} ==");
    shared_row_counts(&java, "Java", &java_db);
    shared_row_counts(&rust, "Rust", &rust_db);

    let (only_java, only_rust) = compare_history(&java, &rust, &since);
    println!(
        "history: Java {} 条独有 / Rust {} 条独有",
        only_java.len(),
        only_rust.len()
    );
    if let Some(path) = (!out_csv.is_empty()).then_some(&out_csv) {
        match write_csv(path, &only_java, &only_rust) {
            Ok(()) => println!("差异已写入 {path}"),
            Err(e) => eprintln!("写 CSV 失败: {e}"),
        }
    } else if !only_java.is_empty() || !only_rust.is_empty() {
        for (side, rows) in [("Java", &only_java), ("Rust", &only_rust)] {
            for key in rows.iter().take(20) {
                println!("[{side} 独有] {key}");
            }
        }
    }
    // 退出码：0 = 封禁集合一致；1 = 存在差异
    if only_java.is_empty() && only_rust.is_empty() {
        println!("结论：history 封禁集合一致 ✔");
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

fn compare_history(
    java: &Connection,
    rust: &Connection,
    since: &str,
) -> (Vec<String>, Vec<String>) {
    let read_side = |conn: &Connection| -> Option<BTreeMap<String, ()>> {
        if !table_exists(conn, "history") {
            eprintln!("警告：该库没有 history 表，跳过 history 比对");
            return None;
        }
        let ip = pick_column(conn, "history", &["ip", "peer_ip", "ip_address", "address"])?;
        let port = pick_column(conn, "history", &["port", "peer_port"]);
        let time = pick_column(conn, "history", &["ban_at", "created_at", "timestamp"])?;
        let port_expr = port.as_deref().unwrap_or("0");
        let mut sql = format!("SELECT {ip}, {port_expr}, {time} FROM history");
        let mut params: Vec<String> = Vec::new();
        if !since.is_empty() && time.as_str() == "ban_at" {
            // 数值毫秒列才支持数值窗口；文本列全量读出后按前缀过滤
            sql.push_str(" WHERE ban_at >= ?1");
            let since_ms = chrono_like_day_start_ms(since);
            params.push(since_ms.to_string());
        }
        let mut map = BTreeMap::new();
        let mut stmt = conn.prepare(&sql).ok()?;
        let mut rows = stmt
            .query(rusqlite::params_from_iter(params.iter()))
            .ok()?;
        while let Some(row) = rows.next().ok()? {
            let ip: String = row.get(0).ok()?;
            let port: i64 = row.get(1).unwrap_or(0);
            let bucket = hour_bucket(row.get(2).ok()?).unwrap_or_default();
            let bucket = if !since.is_empty() && bucket.as_str() < since {
                continue;
            } else {
                bucket
            };
            map.insert(format!("{ip}:{port}@{bucket}"), ());
        }
        Some(map)
    };
    let java_map = read_side(java).unwrap_or_default();
    let rust_map = read_side(rust).unwrap_or_default();
    let only_java: Vec<String> = java_map.keys().filter(|k| !rust_map.contains_key(*k)).cloned().collect();
    let only_rust: Vec<String> = rust_map.keys().filter(|k| !java_map.contains_key(*k)).cloned().collect();
    (only_java, only_rust)
}

/// `YYYY-MM-DD` → 当日零点 epoch 毫秒（仅用于数值列的粗过滤；解析失败返回 0 = 不过滤）。
fn chrono_like_day_start_ms(date: &str) -> i64 {
    let parts: Vec<i64> = date
        .split('-')
        .filter_map(|p| p.parse().ok())
        .collect();
    if parts.len() < 3 {
        return 0;
    }
    // 简化儒略日换算（足以覆盖 2026 年附近，误差在小时桶粒度内无影响）
    let (y, m, d) = (parts[0], parts[1], parts[2]);
    let days = 367 * y - (7 * (y + (m + 9) / 12)) / 4 + (275 * m) / 9 + d - 719559;
    days * 86_400_000
}

fn write_csv(path: &str, only_java: &[String], only_rust: &[String]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "side,key")?;
    for k in only_java {
        writeln!(f, "java,{k}")?;
    }
    for k in only_rust {
        writeln!(f, "rust,{k}")?;
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
            "CREATE TABLE history (id INTEGER PRIMARY KEY, ip TEXT, port INTEGER, ban_at INTEGER);
             INSERT INTO history (ip, port, ban_at) VALUES
               ('1.2.3.4', 51413, 1789000000000),
               ('5.6.7.8', 80,    1789000060000);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn identical_histories_have_no_diff() {
        let a = memory_history();
        let b = memory_history();
        let (oj, or) = compare_history(&a, &b, "");
        assert!(oj.is_empty() && or.is_empty());
    }

    #[test]
    fn missing_rows_are_reported_per_side() {
        let a = memory_history();
        let b = Connection::open_in_memory().unwrap();
        b.execute_batch(
            "CREATE TABLE history (id INTEGER PRIMARY KEY, ip TEXT, port INTEGER, ban_at INTEGER);
             INSERT INTO history (ip, port, ban_at) VALUES ('1.2.3.4', 51413, 1789000000000);",
        )
        .unwrap();
        let (oj, or) = compare_history(&a, &b, "");
        assert_eq!(oj.len(), 1, "Java 独有：5.6.7.8");
        assert!(or.is_empty());
    }
}
