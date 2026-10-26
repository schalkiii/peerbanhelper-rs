// ===========================================================================
// 时间与格式化工具（对齐 `util/TimeUtil` / `util/MsgUtil`）
// ===========================================================================

/// 对齐上游 `ActiveMonitoringModule` 内硬编码的滑动窗口（`Duration.ofHours(24)`）。
pub const SLIDING_WINDOW_MILLIS: i64 = 24 * 60 * 60 * 1000;

pub(crate) fn local_naive(ms: i64) -> chrono::NaiveDateTime {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.naive_local())
        .unwrap_or_else(|| {
            chrono::DateTime::from_timestamp_millis(0)
                .unwrap()
                .naive_utc()
        })
}

/// 对齐 `ZoneId.systemDefault().getRules().getOffset(Instant.ofEpochMilli(ms))`。
fn local_offset_secs_at(ms: i64) -> i32 {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|dt| dt.offset().local_minus_utc())
        .unwrap_or(0)
}

/// 对齐 `LocalDateTime.atOffset(offset)`：把本地时间按给定偏移直接换算为时间戳
/// （不做夏令时校验，与上游一致）。
fn attach_local_offset(naive: chrono::NaiveDateTime, offset_secs: i32) -> i64 {
    naive.and_utc().timestamp_millis() - i64::from(offset_secs) * 1000
}

/// 对齐 `TimeUtil.getStartOfHour(time)`：系统时区下本小时第 0 分 0 秒。
///
/// 与上游的差异：上游取 `Instant.now()` 的时区偏移，本移植取 `time` 的偏移
/// （只在 DST 切换窗口内不同，且本移植的取值可确定性测试）。
pub fn start_of_hour_ms(time_ms: i64) -> i64 {
    use chrono::Timelike;
    let naive = local_naive(time_ms);
    let hour_start = naive
        .date()
        .and_hms_opt(naive.time().hour(), 0, 0)
        .unwrap_or_else(|| naive.date().and_hms_opt(0, 0, 0).unwrap());
    attach_local_offset(hour_start, local_offset_secs_at(time_ms))
}

/// 对齐 `TimeUtil.getStartOfToday(time)`：系统时区下当天 0 点。
pub fn start_of_today_ms(time_ms: i64) -> i64 {
    let naive = local_naive(time_ms);
    let midnight = naive.date().and_hms_opt(0, 0, 0).unwrap();
    attach_local_offset(midnight, local_offset_secs_at(time_ms))
}

/// 当前 Unix 时间戳（毫秒），供缓存淘汰等内部逻辑使用。
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 对齐 `TimeUtil.getEndOfToday(time)`：系统时区下当天 23:59:59.999（差异同 [`start_of_hour_ms`]）。
pub fn end_of_today_ms(time_ms: i64) -> i64 {
    let naive = local_naive(time_ms);
    let day_end = naive.date().and_hms_milli_opt(23, 59, 59, 999).unwrap();
    attach_local_offset(day_end, local_offset_secs_at(time_ms))
}

/// 对齐 `TimeUtil.formatDateTime`（`SimpleDateFormat("yyyy-MM-dd HH:mm:ss")`，系统时区）。
pub fn format_date_time(time_ms: i64) -> String {
    local_naive(time_ms).format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 对齐 `TimeUtil.formatDateOnly`（`SimpleDateFormat("yyyy-MM-dd")`，系统时区）。
pub fn format_date_only(time_ms: i64) -> String {
    local_naive(time_ms).format("%Y-%m-%d").to_string()
}

/// 对齐 `MsgUtil.humanReadableByteCountBin`（1024 进制、`%.1f %ciB`）。
pub fn human_readable_byte_count_bin(bytes: i64) -> String {
    let abs_b = if bytes == i64::MIN {
        i64::MAX
    } else {
        bytes.abs()
    };
    if abs_b < 1024 {
        return format!("{bytes} B");
    }
    let mut value = abs_b;
    // 上游用 `StringCharacterIterator("KMGTPE")`：每轮 `value >>= 10` 后 `next()`，
    // 故单元下标 = 循环次数（0 -> K, 1 -> M, ...）
    let units = ['K', 'M', 'G', 'T', 'P', 'E'];
    let mut unit_index = 0usize;
    let mut i: u32 = 40;
    loop {
        if abs_b <= (0x0fff_cccc_cccc_cccc_i64 >> i) {
            break;
        }
        value >>= 10;
        unit_index = (unit_index + 1).min(units.len() - 1);
        if i < 10 {
            break;
        }
        i -= 10;
    }
    value *= bytes.signum();
    format!("{:.1} {}iB", value as f64 / 1024.0, units[unit_index])
}

/// 对齐 `MsgUtil.humanReadableByteCountSI`（1000 进制、`%.1f %cB`）。
pub fn human_readable_byte_count_si(bytes: i64) -> String {
    if (-1000 < bytes) && (bytes < 1000) {
        return format!("{bytes} B");
    }
    let mut value = bytes;
    let units = ['k', 'M', 'G', 'T', 'P', 'E'];
    let mut unit_index = 0usize;
    while value <= -999_950 || value >= 999_950 {
        value /= 1000;
        unit_index = (unit_index + 1).min(units.len() - 1);
    }
    format!("{:.1} {}B", value as f64 / 1000.0, units[unit_index])
}

/// 对齐缓存键里的 `peer.getPeerAddress().getAddress().toCompressedString()`：
/// IPv6 归一化为压缩形式（`::ffff:1.2.3.4` 经 [`crate::iputil::parse_addr`] 视为 IPv4）。
pub fn compressed_ip(ip: &str) -> String {
    match crate::iputil::parse_addr(ip) {
        Some(addr) => addr.to_string(),
        None => ip.to_string(),
    }
}

/// 对齐 `PeerFlag#getLtStdString()`（即 `PeerFlag#toString()`）：按
/// `d/D u/U K ? O S I H X L E e P` 的顺序以空格重建 libtorrent 标准 flags 串。
pub fn lt_std_string(flags: &str) -> String {
    let f = PeerFlag::parse(flags);
    let mut parts: Vec<&str> = Vec::with_capacity(8);
    if f.interesting {
        parts.push(if f.remote_choked { "d" } else { "D" });
    }
    if f.remote_interested {
        parts.push(if f.choked { "u" } else { "U" });
    }
    if !f.remote_choked && !f.interesting {
        parts.push("K");
    }
    if !f.choked && !f.remote_interested {
        parts.push("?");
    }
    if f.optimistic_unchoke {
        parts.push("O");
    }
    if f.snubbed {
        parts.push("S");
    }
    if !f.local_connection {
        parts.push("I");
    }
    if f.from_dht {
        parts.push("H");
    }
    if f.from_pex {
        parts.push("X");
    }
    if f.from_lsd {
        parts.push("L");
    }
    if f.rc4_encrypted {
        parts.push("E");
    }
    if f.plaintext_encrypted {
        parts.push("e");
    }
    if f.utp_socket {
        parts.push("P");
    }
    parts.join(" ")
}

/// `peer.getFlags() == null ? null : getLtStdString()`。
///
/// 注意：Java 中**空串不是 null**，因此 `Some("")` 会原样保留为空串
/// （下游 `aggregating` 的 `flags != null` 分支会因此命中）。
pub fn lt_std_string_opt(flags: Option<&str>) -> Option<String> {
    flags.map(lt_std_string)
}

use super::*;
