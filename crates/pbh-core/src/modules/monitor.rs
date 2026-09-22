// === INTEGRATION SNIPPET (applied by main agent) ===
//
// 【1】`pbh/src/default-config.yml`：在 `profile.module:` 段内新增（值逐字对齐上游
//      `profile.yml` 的 `module.active-monitoring`（第 381-405 行）与
//      `module.peer-analyse-service`（第 458-483 行））：
//
// ```yaml
//     active-monitoring:
//       enabled: true
//       # 流量监控：当日上行流量阈值告警
//       traffic-monitoring:
//         daily: -1              # -1 禁用，单位 bytes
//       # 流量滑动窗口上传限速
//       traffic-sliding-capping:
//         enabled: false
//         # 注意：上游 profile.yml 第 399 行写的是 `max-allowed-upload-traffic`，但
//         # ActiveMonitoringModule 读取的是 `daily-max-allowed-upload-traffic`
//         # （由 ProfileUpdateScript v27 写入）。本移植与 Java 一致，只读 Java 的键。
//         daily-max-allowed-upload-traffic: 53687091200
//         max-speed: 10485760
//         min-speed: 0
//     peer-analyse-service:
//       session-analyse:        # 会话统计与分析服务
//         enabled: true
//         data-flush-interval: 3600000
//         cleanup-interval: 3600000
//         data-retention-time: 15552000000
//       swarm-tracking:         # 用户群跟踪（重启后数据自动删除）
//         enabled: true
//         data-flush-interval: 3600000
//       peer-recording:         # 对等体跟踪记录服务
//         enabled: true
//         data-flush-interval: 900000
//         data-retention-time: 5184000000
//         data-cleanup-interval: 604800000
// ```
//
// 【2】`pbh-core/src/config.rs`：在 `impl ProfileConfig` 内新增（`sink` 由应用层注入，见【3】）：
//
// ```rust
// /// 按上游 `PeerBanHelper.registerModules()` 的顺序构造监控模块：
// /// `ActiveMonitoring → SwarmTracking → SessionAnalyse → PeerRecording`。
// ///
// /// 上游 `RunCheckModuleOrgan` 只对 `RuleFeatureModule` 调 `shouldBanPeer`，这四个模块
// /// 都不是 `RuleFeatureModule`，因此**从不参与 peer 判定**；忠实做法是把它们交给应用层
// /// 调度器（不加进 `Pipeline`）。它们的 `check` 恒 `pass()`，加进去也不影响任何裁决。
// pub fn build_monitor_modules(
//     &self,
//     sink: std::sync::Arc<dyn crate::modules::MonitorSink>,
// ) -> Vec<Box<dyn crate::module::RuleModule>> {
//     use crate::modules::{
//         ActiveMonitoringModule, PeerRecordingServiceModule, SessionAnalyseServiceModule,
//         SwarmTrackingModule,
//     };
//     let mut modules: Vec<Box<dyn RuleModule>> = Vec::new();
//     if let Some(cfg) = &self.module.active_monitoring {
//         if enabled_or_disabled(&cfg.enabled) {
//             modules.push(Box::new(ActiveMonitoringModule::new(sink.clone(), cfg.to_settings())));
//         }
//     }
//     if let Some(cfg) = &self.module.peer_analyse_service {
//         if let Some(sub) = &cfg.swarm_tracking {
//             if enabled_or_disabled(&sub.enabled) {
//                 modules.push(Box::new(SwarmTrackingModule::new(sink.clone(), sub.to_settings())));
//             }
//         }
//         if let Some(sub) = &cfg.session_analyse {
//             if enabled_or_disabled(&sub.enabled) {
//                 modules.push(Box::new(SessionAnalyseServiceModule::new(sink.clone(), sub.to_settings())));
//             }
//         }
//         if let Some(sub) = &cfg.peer_recording {
//             if enabled_or_disabled(&sub.enabled) {
//                 modules.push(Box::new(PeerRecordingServiceModule::new(sink.clone(), sub.to_settings())));
//             }
//         }
//     }
//     modules
// }
// ```
//
// 应用层（`pbh/src/main.rs`）按定时任务驱动（对齐上游 `registerScheduledTask`）：
// ```text
// active-monitoring: updateTrafficStatus 每 1 分钟（首次 delay 0）
//     -> ActiveMonitoringModule::on_tick(&stats, now_ms)
// session-analyse:   flushData(每 data-flush-interval, 默认 3600000) -> flush_data(now_ms)
//                    cleanup(每 cleanup-interval, 默认 3600000)      -> cleanup(now_ms)
//                    onPeersRetrieved                                  -> sync_peers(id, torrent, peers, now_ms)
// peer-recording:    flush(每 data-flush-interval, 默认 900000)        -> flush()
//                    cleanup(每 data-cleanup-interval, 默认 604800000) -> cleanup(now_ms)
//                    onPeersRetrieved                                  -> on_peers_retrieved(id, torrent, peers, now_ms)
// swarm-tracking:    启动时 resetTable                               -> on_enable()
//                    flushAll(每 data-flush-interval, 默认 3600000)    -> flush_all()
//                    onPeersRetrieved                                  -> sync_peers(id, torrent, peers, now_ms)
// 关闭时（对齐各模块 onDisable）：active-monitoring 再跑一次 on_tick；session-analyse 跑
// flush_data(now_ms)；peer-recording 跑 flush()；swarm-tracking 无需额外动作（上游只是 closeCache）。
// Web API：`/api/modules/swarm-tracking` -> SwarmTrackingModule::count()；
// `/api/modules/swarm-tracking/details` 的分页查询由 `pbh-web` 直接读 DB 版 sink 的
// tracked_swarm 表实现（对齐上游 TrackedSwarmService.page）。
// ```
//
// 【3】`pbh` crate 的 DB 版 `MonitorSink` 实现（建议放 `pbh-db`）必须提供下列操作，
//      SQL 语义逐条对齐上游实体与 `resources/mapper/sqlite/*.xml`：
// ```text
// ensure_torrent(torrent) -> torrent_id
//   TorrentService.createIfNotExists：torrents 表按 info_hash 唯一定位/插入，返回主键。
// update_traffic_journal(downloader, dl, ul, dl_proto, ul_proto, now_ms)
//   TrafficJournalService.updateData：timestamp = getStartOfHour(now)；表 traffic_journal_v3
//   按 (downloader, timestamp) 唯一。新建：data_*_at_start = 传入值、data_* = 传入值；
//   已存在：仅当 data_overall_downloaded < dl（uploaded / protocol 同理）时更新该列，
//   *_at_start 列永不更新。
// query_traffic_overall(downloader, start, end) -> Vec<TrafficDataComputed>   闭区间
//   None -> selectAllDownloadersOverallData：GROUP BY timestamp，
//           uploaded = SUM(data_overall_uploaded) - SUM(data_overall_uploaded_at_start)（可负）；
//   Some -> selectSpecificDownloaderOverallData：逐行 MAX(0, uploaded - uploaded_at_start)。
// alert_exists_include_read(id) / publish_alert(push, level, id, title, content)
//   AlertManager.identifierAlertExistsIncludeRead / publishAlert：alerts 表按 identifier 查询
//   （无论已读，故本 trait 不含已读状态）；title/content 存 TranslationComponent 的 JSON。
// load_metrics_track(key) / upsert_metrics_tracks(rows) / list_metrics_tracks_at(t) /
// list_metrics_tracks_not_at(t) / delete_metrics_tracks(rows)
//   peer_connection_metrics_track 表：唯一键 (timeframe_at, downloader, torrent_id, address, port)；
//   upsert 只更新 peer_id / client_name / last_flags；list 为 eq / ne(timeframe_at)；
//   delete 用 deleteByIds（逐条删，勿 batchDelete —— 上游 #1518 的 SQLITE_TOOBIG 规避）。
// save_connection_metrics(rows, overwrite) / remove_connection_metrics_before(before)
//   PeerConnectionMetricsService.saveAggregating：peer_connection_metrics 表按
//   (timeframe_at, downloader) 查：存在 && overwrite -> 新值覆盖（保留 id）；
//   存在 && !overwrite -> merge()（上游 merge() **漏掉 local_not_interested**，本移植同样漏掉）；
//   不存在 -> 插入。
//   removeOutdatedData = splitBatchDelete(`timeframe_at <= before`)，**闭区间**。
// upsert_peer_record(row) / remove_peer_records_before(before)
//   peer_records 表按 (address, torrent_id, downloader) 冲突；UPDATE 的 CASE 表达式等价于本文件
//   的 apply_peer_record_upsert（DB 实现可照抄 SQL 或复用该函数）；first_time_seen 与 peer_geoip
//   永不更新；peer_geoip 由 DB 层用 IPDBManager 等价查询填充（对齐上游在 DAO 内查 IP 库）。
//   cleanup = splitBatchDelete(`last_time_seen < before`)，**开区间**。
// upsert_tracked_swarm / load_last_tracked_swarm(ip, port, info_hash, downloader) /
// reset_tracked_swarm / count_tracked_swarm
//   tracked_swarm 临时表：resetTable = `DELETE FROM tracked_swarm`（应用启动时调用 -> 数据随重启丢弃）；
//   upsert 冲突键 (ip, port, info_hash, downloader)，UPDATE 覆盖除 id 外全部列（含 first_time_seen）；
//   load = `WHERE ... ORDER BY id DESC LIMIT 1`；count 供 `/api/modules/swarm-tracking`。
// ```
//
// 【4】上游 `PBHCache` 的容量/超时淘汰刷库未移植（见各模块头部差异说明）：本移植只用
//      HashMap + 定时 flush，写库时机更集中，结果一致。
// === END INTEGRATION SNIPPET ===

//! 监控类模块（`module/impl/monitor`），忠实复刻上游：
//!
//! | 配置键 | 上游类 | 职责 |
//! | --- | --- | --- |
//! | `active-monitoring` | `ActiveMonitoringModule` | 小时级流量日志、每日上行阈值告警、滑动窗口上传限速 |
//! | `peer-analyse-service.session-analyse` | `SessionAnalyseServiceModule` | 按天聚合 peer 连接指标 + 保留期清理 |
//! | `peer-analyse-service.peer-recording` | `PeerRecordingServiceModule` | Peer 状态/会话/传输/偏移量记录 + 清理 |
//! | `peer-analyse-service.swarm-tracking` | `SwarmTrackingModule` | 本次运行会话内的 swarm peer 跟踪（重启即清空） |
//!
//! 四者上游都没有 `shouldBanPeer`（不是 `RuleFeatureModule`，`RunCheckModuleOrgan` 第 51 行会跳过），
//! 因此 **`check` 恒 `pass()`、绝不封禁**；业务逻辑在定时任务/`onPeersRetrieved` 回调里，本移植用
//! 显式方法暴露：[`ActiveMonitoringModule::on_tick`]、[`SessionAnalyseServiceModule::flush_data`]、
//! [`SessionAnalyseServiceModule::sync_peers`]、[`PeerRecordingServiceModule::on_peers_retrieved`]、
//! [`SwarmTrackingModule::sync_peers`] 等。
//!
//! pbh-core 不持有数据库：全部读写经由 [`MonitorSink`]（默认内存实现 [`InMemoryMonitorSink`]），
//! DB 版本由 `pbh` crate 实现（操作清单见文件头 INTEGRATION SNIPPET 【3】）。

use crate::i18n::TranslationComponent;
use crate::model::{PeerData, PeerFlag, TorrentData};
use crate::module::{CheckContext, CheckResult, RuleModule};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

// ===========================================================================
// 时间与格式化工具（对齐 `util/TimeUtil` / `util/MsgUtil`）
// ===========================================================================

/// 对齐上游 `ActiveMonitoringModule` 内硬编码的滑动窗口（`Duration.ofHours(24)`）。
pub const SLIDING_WINDOW_MILLIS: i64 = 24 * 60 * 60 * 1000;

fn local_naive(ms: i64) -> chrono::NaiveDateTime {
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

// ===========================================================================
// 存储层数据类型（字段逐一对齐上游实体/DTO）
// ===========================================================================

/// 对齐上游 `databasent/dto/TrafficDataComputed`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficDataComputed {
    pub timestamp_ms: i64,
    pub data_overall_uploaded: i64,
    pub data_overall_downloaded: i64,
}

/// 对齐上游 `databasent/table/TrafficJournalEntity`（表 `traffic_journal_v3`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficJournalRow {
    pub downloader: String,
    /// `getStartOfHour(now)`（毫秒）
    pub timestamp_ms: i64,
    pub data_overall_downloaded: i64,
    pub data_overall_uploaded: i64,
    pub data_overall_downloaded_at_start: i64,
    pub data_overall_uploaded_at_start: i64,
    pub protocol_overall_downloaded: i64,
    pub protocol_overall_uploaded: i64,
    pub protocol_overall_downloaded_at_start: i64,
    pub protocol_overall_uploaded_at_start: i64,
}

/// 对齐上游 `PeerConnectionMetricsTrackServiceImpl.CacheKey`
/// （唯一键 `(timeframe_at, downloader, torrent_id, address, port)`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetricsTrackKey {
    /// `getStartOfToday(now)`（毫秒）
    pub timeframe_at_ms: i64,
    pub downloader: String,
    pub torrent_id: String,
    /// 上游为 `InetAddress`（缓存键用 `toCompressedString()`）
    pub address: String,
    pub port: u16,
}

/// 对齐上游 `databasent/table/PeerConnectionMetricsTrackEntity`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsTrackRow {
    pub key: MetricsTrackKey,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    pub last_flags: Option<String>,
}

impl MetricsTrackRow {
    pub fn new(key: MetricsTrackKey) -> Self {
        Self {
            key,
            peer_id: None,
            client_name: None,
            last_flags: None,
        }
    }
}

/// 对齐上游 `databasent/table/PeerConnectionMetricsEntity`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionMetricsRow {
    pub timeframe_at_ms: i64,
    pub downloader: String,
    pub total_connections: i64,
    pub incoming_connections: i64,
    pub remote_refuse_transfer_to_client: i64,
    pub remote_accept_transfer_to_client: i64,
    pub local_refuse_transfer_to_peer: i64,
    pub local_accept_transfer_to_peer: i64,
    pub local_not_interested: i64,
    pub question_status: i64,
    pub optimistic_unchoke: i64,
    pub from_dht: i64,
    pub from_pex: i64,
    pub from_lsd: i64,
    pub from_tracker_or_other: i64,
    pub rc4_encrypted: i64,
    pub plain_text_encrypted: i64,
    pub utp_socket: i64,
    pub tcp_socket: i64,
}

impl ConnectionMetricsRow {
    /// 对齐上游 `PeerConnectionMetricsEntity#merge`。
    ///
    /// **上游此处漏掉了 `local_not_interested`**（未累加该列），本移植保持同样的遗漏，
    /// 以便与上游落库的数据完全一致。
    pub fn merge(&mut self, appender: &ConnectionMetricsRow) {
        self.total_connections += appender.total_connections;
        self.incoming_connections += appender.incoming_connections;
        self.remote_refuse_transfer_to_client += appender.remote_refuse_transfer_to_client;
        self.remote_accept_transfer_to_client += appender.remote_accept_transfer_to_client;
        self.local_refuse_transfer_to_peer += appender.local_refuse_transfer_to_peer;
        self.local_accept_transfer_to_peer += appender.local_accept_transfer_to_peer;
        // 上游 merge() 未合并 local_not_interested（有意保留）
        self.question_status += appender.question_status;
        self.optimistic_unchoke += appender.optimistic_unchoke;
        self.from_dht += appender.from_dht;
        self.from_pex += appender.from_pex;
        self.from_lsd += appender.from_lsd;
        self.from_tracker_or_other += appender.from_tracker_or_other;
        self.rc4_encrypted += appender.rc4_encrypted;
        self.plain_text_encrypted += appender.plain_text_encrypted;
        self.utp_socket += appender.utp_socket;
        self.tcp_socket += appender.tcp_socket;
    }
}

/// 对齐上游 `databasent/table/PeerRecordEntity`（表 `peer_records`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRecordRow {
    /// `InetAddress`（本移植用压缩 IP 字符串）
    pub address: String,
    pub port: u16,
    pub torrent_id: String,
    pub downloader: String,
    pub peer_id: String,
    pub client_name: String,
    pub uploaded: i64,
    pub uploaded_offset: i64,
    pub upload_speed: i64,
    pub downloaded: i64,
    pub downloaded_offset: i64,
    pub download_speed: i64,
    pub last_flags: Option<String>,
    pub first_time_seen_ms: i64,
    pub last_time_seen_ms: i64,
    /// 上游由 DAO 内 `IPDBManager.queryIPDB(inet).geoData()` 填充
    pub peer_geoip: Option<String>,
}

/// 对齐上游 `PeerRecordMapper.upsert`（sqlite XML）的 `ON CONFLICT ... DO UPDATE` CASE 表达式。
///
/// 冲突键：`(address, torrent_id, downloader)`。`first_time_seen` 与 `peer_geoip` **永不更新**；
/// 其余列在 `excluded.last_time_seen < peer_records.last_time_seen`（上报时间更旧）时保留旧值，
/// 否则按 SQL 取值（流量列按「本次上报值 - 已存偏移量」累加；差为负说明对方计数被重置，
/// 直接把本次上报值整体累加）。
pub fn apply_peer_record_upsert(
    existing: &PeerRecordRow,
    incoming: &PeerRecordRow,
) -> PeerRecordRow {
    let mut merged = existing.clone();
    if incoming.last_time_seen_ms < existing.last_time_seen_ms {
        return merged;
    }
    merged.uploaded = if incoming.uploaded - existing.uploaded_offset < 0 {
        existing.uploaded + incoming.uploaded
    } else {
        existing.uploaded + incoming.uploaded - existing.uploaded_offset
    };
    merged.downloaded = if incoming.downloaded - existing.downloaded_offset < 0 {
        existing.downloaded + incoming.downloaded
    } else {
        existing.downloaded + incoming.downloaded - existing.downloaded_offset
    };
    merged.uploaded_offset = incoming.uploaded;
    merged.downloaded_offset = incoming.downloaded;
    merged.upload_speed = incoming.upload_speed;
    merged.download_speed = incoming.download_speed;
    merged.port = incoming.port;
    merged.peer_id = incoming.peer_id.clone();
    merged.client_name = incoming.client_name.clone();
    merged.last_flags = incoming.last_flags.clone();
    merged.last_time_seen_ms = incoming.last_time_seen_ms;
    merged
}

/// 对齐上游 `TrackedSwarmServiceImpl.CacheKey`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackedSwarmKey {
    pub ip: String,
    pub port: u16,
    pub info_hash: String,
    pub downloader: String,
}

/// 对齐上游 `databasent/table/tmp/TrackedSwarmEntity`（临时表 `tracked_swarm`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackedSwarmRow {
    pub id: Option<i64>,
    pub ip: String,
    pub port: u16,
    pub info_hash: String,
    pub torrent_is_private: Option<bool>,
    pub torrent_size: i64,
    pub downloader: String,
    pub downloader_progress: f64,
    pub peer_id: String,
    pub client_name: String,
    pub peer_progress: f64,
    pub uploaded: i64,
    pub uploaded_offset: i64,
    pub upload_speed: i64,
    pub downloaded: i64,
    pub downloaded_offset: i64,
    pub download_speed: i64,
    pub last_flags: String,
    pub first_time_seen_ms: i64,
    pub last_time_seen_ms: i64,
    pub download_speed_max: i64,
    pub upload_speed_max: i64,
    /// 对齐 `AbstractCanDirtyEntity.dirty`（供 BTN 提交等上层使用）
    pub dirty: bool,
}

impl TrackedSwarmRow {
    pub fn key(&self) -> TrackedSwarmKey {
        TrackedSwarmKey {
            ip: self.ip.clone(),
            port: self.port,
            info_hash: self.info_hash.clone(),
            downloader: self.downloader.clone(),
        }
    }
}

/// 对齐上游 `alert/AlertLevel`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AlertLevel {
    Tip,
    Info,
    Warn,
    Error,
    Fatal,
}

/// 已发布的告警（对齐 `AlertManager.publishAlert` 的参数）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertRecord {
    pub push: bool,
    pub level: AlertLevel,
    pub identifier: String,
    pub title: TranslationComponent,
    pub description: TranslationComponent,
}

/// 一次 `updateTrafficMonitoringService()` 发布的告警。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficMonitoringAlert {
    pub alert: AlertRecord,
    /// 当日已上传总量（bytes）
    pub total_uploaded: i64,
    /// `traffic-monitoring.daily`
    pub threshold: i64,
}

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

// ===========================================================================
// active-monitoring
// ===========================================================================

/// `module.active-monitoring` 的运行期配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveMonitoringSettings {
    /// `traffic-monitoring.daily`（对齐 `getLong("traffic-monitoring.daily", -1)`）
    pub daily_traffic_capping: i64,
    /// `traffic-sliding-capping.enabled`（对齐 `getBoolean(...)`，缺省 false）
    pub use_traffic_sliding_capping: bool,
    /// `traffic-sliding-capping.daily-max-allowed-upload-traffic`（对齐 `getLong(...)`，缺省 0）
    pub max_traffic_allowed_in_window_period: i64,
    /// `traffic-sliding-capping.max-speed`
    pub traffic_sliding_capping_max_speed: i64,
    /// `traffic-sliding-capping.min-speed`
    pub traffic_sliding_capping_min_speed: i64,
}

impl Default for ActiveMonitoringSettings {
    fn default() -> Self {
        Self {
            daily_traffic_capping: -1,
            use_traffic_sliding_capping: false,
            max_traffic_allowed_in_window_period: 0,
            traffic_sliding_capping_max_speed: 0,
            traffic_sliding_capping_min_speed: 0,
        }
    }
}

/// 对齐 `DownloaderSpeedLimiter`（上传/下载速率上限，bytes/s）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SpeedLimiter {
    pub upload: i64,
    pub download: i64,
}

/// 一个下载器在 `updateTrafficStatus` 时刻的状态快照（对齐上游依次调用的
/// `downloader.login()` / `getStatistics()` / `getSpeedLimiter()` / `getFeatureFlags()`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloaderTrafficStats {
    pub id: String,
    /// 展示名（仅用于日志）
    pub name: String,
    /// `downloader.login().success()`：未成功登录的下载器本轮跳过（上游在 catch 中记日志）
    pub logged_in: bool,
    pub total_downloaded: i64,
    pub total_uploaded: i64,
    /// `None` 对齐 `getSpeedLimiter() == null`（不支持限速）-> 跳过
    pub speed_limiter: Option<SpeedLimiter>,
    /// `featureFlags.contains(TRAFFIC_STATS)`：不上报流量的下载器不参与滑动窗口限速
    pub traffic_stats_feature: bool,
}

impl DownloaderTrafficStats {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        logged_in: bool,
        total_downloaded: i64,
        total_uploaded: i64,
        speed_limiter: Option<SpeedLimiter>,
        traffic_stats_feature: bool,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            logged_in,
            total_downloaded,
            total_uploaded,
            speed_limiter,
            traffic_stats_feature,
        }
    }
}

/// 对齐上游 `TrafficJournalServiceImpl.SlidingWindowDynamicSpeedLimiter`
/// （字段一一对应；Java 的 `Long`/`Double` 装箱类型 -> `Option`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SlidingWindowDynamicSpeedLimiter {
    pub window_size_millis: i64,
    pub window_start_time: i64,
    pub uploaded_in_window: i64,
    pub old_speed_limit: i64,
    pub new_speed_limit: i64,
    pub threshold: i64,
    pub min_speed: i64,
    pub max_speed: i64,
    pub increase_factor: Option<f64>,
    pub decrease_factor: Option<f64>,
    /// 上游只把 `newSpeed` 设为 maxSpeed，**没有**给该字段赋值，因此恒为 `None`
    /// （保留字段以对齐结构）。
    pub reached_maximum_speed: Option<bool>,
    pub reached_minimum_speed: Option<bool>,
}

/// 一次滑动窗口限速调整的落地结果（对齐
/// `new DownloaderSpeedLimiter(calculatedData.getNewSpeedLimit(), speedLimiter.download())`
/// 后的 `downloader.setSpeedLimiter(...)`）。
#[derive(Debug, Clone, PartialEq)]
pub struct SpeedLimitChange {
    pub downloader_id: String,
    pub downloader_name: String,
    /// 需要下发的新的速率限制（上传取新值，下载保持原值）
    pub limiter: SpeedLimiter,
    pub computed: SlidingWindowDynamicSpeedLimiter,
}

/// 对齐 `TrafficJournalService#tweakSpeedLimiterBySlidingWindow` 的**纯计算**部分。
///
/// 参数：窗口内已上传流量、当前上传限速、阈值、最小/最大限速、当前时间。
/// 窗口固定 24 小时（上游 `Duration.ofHours(24)`），窗口起点 = `now_ms - 24h`。
pub fn compute_sliding_window_limiter(
    uploaded_in_window: i64,
    old_speed_limit: i64,
    threshold: i64,
    min_speed: i64,
    max_speed: i64,
    now_ms: i64,
) -> SlidingWindowDynamicSpeedLimiter {
    let window_size_millis = SLIDING_WINDOW_MILLIS;
    let mut result = SlidingWindowDynamicSpeedLimiter {
        window_size_millis,
        window_start_time: now_ms - window_size_millis,
        uploaded_in_window,
        old_speed_limit,
        new_speed_limit: 0,
        threshold,
        min_speed,
        max_speed,
        increase_factor: None,
        decrease_factor: None,
        reached_maximum_speed: None,
        reached_minimum_speed: None,
    };

    let new_speed;
    if uploaded_in_window >= threshold {
        // 应用节流策略 - 当达到或超过阈值时
        if old_speed_limit <= min_speed {
            new_speed = min_speed;
            result.reached_minimum_speed = Some(true);
        } else {
            // 计算减少因子 b = l_current / l
            let b = if threshold > 0 {
                uploaded_in_window as f64 / threshold as f64
            } else {
                1.0
            };
            let reduced = (old_speed_limit as f64 / b).trunc() as i64;
            new_speed = min_speed.max(reduced);
            result.decrease_factor = Some(b);
        }
    } else if old_speed_limit >= max_speed && max_speed > 0 {
        // 应用解除节流策略 - 未达到阈值，且当前限速已达上限
        new_speed = max_speed;
    } else {
        // 计算增加因子 a = (l - l_current) / w，并转换为每秒字节数
        let a = (threshold - uploaded_in_window) as f64 / window_size_millis as f64 * 1000.0;
        // 上游这里是 Math.addExact(...)（溢出抛异常）；本移植饱和处理以避免 panic
        let increased = old_speed_limit.saturating_add(a.trunc() as i64);
        new_speed = max_speed.min(increased);
        result.increase_factor = Some(a);
    }
    result.new_speed_limit = new_speed;
    result
}

/// 对齐上游 `ActiveMonitoringModule`（配置键 `active-monitoring`）。
///
/// - 每小时把各下载器流量快照写入 `traffic_journal_v3`（[`Self::on_tick`]）；
/// - `traffic-monitoring.daily > 0` 时，当日上传总量达到阈值即发一次 WARN 告警
///   （identifier = `dataTrafficCapping-<当日 0 点的 epoch 秒>`，见
///   [`Self::update_traffic_monitoring`]）；
/// - `traffic-sliding-capping.enabled` 时按 24 小时滑动窗口调整下载器上传限速
///   （[`Self::update_traffic_capping_service`]）；
/// - `check` 恒 `pass()`，绝不封禁。
pub struct ActiveMonitoringModule {
    pub settings: ActiveMonitoringSettings,
    sink: Arc<dyn MonitorSink>,
}

impl ActiveMonitoringModule {
    pub fn new(sink: Arc<dyn MonitorSink>, settings: ActiveMonitoringSettings) -> Self {
        Self { settings, sink }
    }

    /// 对齐 `updateTrafficStatus()`：写流量日志 → 跑阈值告警 → 跑滑动窗口限速。
    ///
    /// 上游由 `registerScheduledTask(this::updateTrafficStatus, 0, 1, MINUTES)` 每 1 分钟调用。
    /// 本移植无需 `&mut self`：全部状态（含告警去重）都在 [`MonitorSink`] 内。
    ///
    /// 返回 `(限速变更, 本次新发布的日流量阈值告警)`：告警的落库已在
    /// [`MonitorSink::publish_alert`] 完成；`push = true` 的推送分发由应用层接线
    /// （对齐上游 `AlertManagerImpl.publishAlert(push=true)` 的推送分支，见 pbh 的 `MonitorHost`）。
    pub fn on_tick(
        &self,
        downloaders: &[DownloaderTrafficStats],
        now_ms: i64,
    ) -> (Vec<SpeedLimitChange>, Option<TrafficMonitoringAlert>) {
        for stats in downloaders {
            // 上游：`if (downloader.login().success())`，失败在 catch 中记日志
            if !stats.logged_in {
                continue;
            }
            self.sink.update_traffic_journal(
                &stats.id,
                stats.total_downloaded,
                stats.total_uploaded,
                0,
                0,
                now_ms,
            );
        }
        let alert = self.update_traffic_monitoring(now_ms);
        let changes = self.update_traffic_capping_service(downloaders, now_ms);
        (changes, alert)
    }
    /// 对齐 `updateTrafficMonitoringService()`：当日上传流量超阈值 → 发布一次告警。
    pub fn update_traffic_monitoring(&self, now_ms: i64) -> Option<TrafficMonitoringAlert> {
        if self.settings.daily_traffic_capping <= 0 {
            return None;
        }
        let start_of_today = start_of_today_ms(now_ms);
        let data = self
            .sink
            .query_traffic_overall(None, start_of_today, end_of_today_ms(now_ms));
        let total_bytes: i64 = data.iter().map(|d| d.data_overall_uploaded).sum();
        // 上游 identifier 用的是 `getStartOfToday(now).toEpochSecond()`
        let identifier = format!("dataTrafficCapping-{}", start_of_today / 1000);
        if total_bytes < self.settings.daily_traffic_capping {
            return None;
        }
        // 一天只发一次
        if self.sink.alert_exists_include_read(&identifier) {
            return None;
        }
        let title = TranslationComponent::with_params(
            "MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_TITLE",
            vec![format_date_only(now_ms).into()],
        );
        let description = TranslationComponent::with_params(
            "MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_DESCRIPTION",
            vec![
                format_date_time(now_ms).into(),
                human_readable_byte_count_bin(total_bytes).into(),
                human_readable_byte_count_bin(self.settings.daily_traffic_capping).into(),
            ],
        );
        let alert = AlertRecord {
            push: true,
            level: AlertLevel::Warn,
            identifier,
            title,
            description,
        };
        self.sink.publish_alert(
            alert.push,
            alert.level,
            &alert.identifier,
            &alert.title,
            &alert.description,
        );
        Some(TrafficMonitoringAlert {
            alert,
            total_uploaded: total_bytes,
            threshold: self.settings.daily_traffic_capping,
        })
    }

    /// 对齐 `updateTrafficCappingService()`：逐个下载器计算并下发新的上传限速。
    ///
    /// 跳过条件逐条保留：未登录成功、`getSpeedLimiter() == null`、
    /// 特性标志中没有 `TRAFFIC_STATS`。
    pub fn update_traffic_capping_service(
        &self,
        downloaders: &[DownloaderTrafficStats],
        now_ms: i64,
    ) -> Vec<SpeedLimitChange> {
        if !self.settings.use_traffic_sliding_capping {
            return Vec::new();
        }
        let mut changes = Vec::new();
        for stats in downloaders {
            if !stats.logged_in {
                continue;
            }
            let Some(limiter) = stats.speed_limiter else {
                continue;
            };
            if !stats.traffic_stats_feature {
                continue;
            }
            // 上游这里传的是 `null`（对所有下载器合并统计）
            let computed = self.tweak_speed_limiter_by_sliding_window(None, limiter, now_ms);
            let new_limiter = SpeedLimiter {
                upload: computed.new_speed_limit,
                download: limiter.download,
            };
            tracing::debug!(
                "active-monitoring: 下载器 {} 上传限速 {} -> {}（{}）",
                stats.name,
                human_readable_byte_count_bin(limiter.upload),
                human_readable_byte_count_bin(new_limiter.upload),
                human_readable_byte_count_si(new_limiter.upload),
            );
            changes.push(SpeedLimitChange {
                downloader_id: stats.id.clone(),
                downloader_name: stats.name.clone(),
                limiter: new_limiter,
                computed,
            });
        }
        changes
    }

    /// 对齐 `trafficJournalDao.tweakSpeedLimiterBySlidingWindow(downloader, speedLimiter, ...)`：
    /// 先按窗口查询流量，再做 [`compute_sliding_window_limiter`] 的纯计算。
    pub fn tweak_speed_limiter_by_sliding_window(
        &self,
        downloader: Option<&str>,
        current: SpeedLimiter,
        now_ms: i64,
    ) -> SlidingWindowDynamicSpeedLimiter {
        let start = now_ms - SLIDING_WINDOW_MILLIS;
        let traffic = self.sink.query_traffic_overall(downloader, start, now_ms);
        let uploaded: i64 = traffic.iter().map(|d| d.data_overall_uploaded).sum();
        compute_sliding_window_limiter(
            uploaded,
            current.upload,
            self.settings.max_traffic_allowed_in_window_period,
            self.settings.traffic_sliding_capping_min_speed,
            self.settings.traffic_sliding_capping_max_speed,
            now_ms,
        )
    }
}

impl RuleModule for ActiveMonitoringModule {
    fn name(&self) -> &str {
        "Active Monitoring"
    }

    fn config_name(&self) -> &str {
        "active-monitoring"
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
        // 上游 ActiveMonitoringModule 没有 shouldBanPeer（非 RuleFeatureModule），
        // RunCheckModuleOrgan 第 51 行会跳过它；这里保持 pass()，绝不封禁。
        CheckResult::pass(self.config_name())
    }
}

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

// ===========================================================================
// peer-analyse-service.peer-recording
// ===========================================================================

/// `module.peer-analyse-service.peer-recording` 的运行期配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecordingSettings {
    /// `data-retention-time`（对齐 `getLong("data-retention-time", -1)`）
    pub data_retention_time_ms: i64,
    /// `data-cleanup-interval`（对齐 `getLong("data-cleanup-interval", -1)`）
    pub data_cleanup_interval_ms: i64,
    /// `data-flush-interval`（对齐 `getLong("data-flush-interval", 20000)`）
    pub data_flush_interval_ms: i64,
}

impl Default for PeerRecordingSettings {
    fn default() -> Self {
        Self {
            data_retention_time_ms: -1,
            data_cleanup_interval_ms: -1,
            data_flush_interval_ms: 20_000,
        }
    }
}

/// 对齐上游 `PBHCache` 的两个外部开关默认值
/// （`pbh.module.peerRecordingServiceModule.diskWriteCache.size` / `.timeout`）。
pub const PEER_RECORDING_CACHE_SIZE: usize = 3500;
pub const PEER_RECORDING_CACHE_TIMEOUT_MS: i64 = 180_000;

/// 对齐上游 `CacheKey(downloader, TorrentWrapper, PeerAddress)`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerRecordCacheKey {
    pub downloader: String,
    /// `TorrentWrapper` 的等价键
    pub torrent_hash: String,
    pub peer_ip: String,
    pub peer_port: u16,
}

/// 对齐上游 `PeerRecordServiceImpl.PeerRecordCachingEntire`。
#[derive(Debug, Clone)]
pub struct PeerRecordCachingEntire {
    pub timestamp_ms: i64,
    pub downloader: String,
    pub torrent: TorrentData,
    pub peer: PeerData,
    /// 对齐 `AbstractCanDirtyEntity.dirty`：`flushToDatabase` 后置 false
    pub dirty: bool,
}

impl PeerRecordCachingEntire {
    pub fn new(
        timestamp_ms: i64,
        downloader: &str,
        torrent: &TorrentData,
        peer: &PeerData,
    ) -> Self {
        Self {
            timestamp_ms,
            downloader: downloader.to_string(),
            torrent: torrent.clone(),
            peer: peer.clone(),
            // 上游构造时 dirty = true
            dirty: true,
        }
    }

    /// 对齐 Lombok `@Data` 生成的 `equals`：逐字段比较 `TorrentWrapper` / `PeerWrapper`。
    pub fn same_as(&self, other: &Self) -> bool {
        self.timestamp_ms == other.timestamp_ms
            && self.downloader == other.downloader
            && self.dirty == other.dirty
            && torrent_snapshot_same(&self.torrent, &other.torrent)
            && peer_snapshot_same(&self.peer, &other.peer)
    }
}

fn torrent_snapshot_same(a: &TorrentData, b: &TorrentData) -> bool {
    a.hash == b.hash
        && a.name == b.name
        && a.progress == b.progress
        && a.total_size == b.total_size
        && a.piece_size == b.piece_size
        && a.pieces_have == b.pieces_have
        && a.completed_override == b.completed_override
        && a.dlspeed == b.dlspeed
        && a.upspeed == b.upspeed
        && a.is_private == b.is_private
}

fn peer_snapshot_same(a: &PeerData, b: &PeerData) -> bool {
    a.client_name == b.client_name
        && a.peer_id == b.peer_id
        && a.dl_speed == b.dl_speed
        && a.downloaded == b.downloaded
        && a.up_speed == b.up_speed
        && a.uploaded == b.uploaded
        && a.progress == b.progress
        && a.flags == b.flags
        && a.ip == b.ip
        && a.port == b.port
        && a.raw_ip == b.raw_ip
        && a.connection == b.connection
}

/// 对齐上游 `PeerRecordServiceImpl.flushToDatabase` 的 peer_id 截断：
/// `peer.getId().length() > 8 ? substring(0, 8) : peer.getId()`（本移植按字符截断）。
fn truncate_peer_id(peer_id: Option<&str>) -> String {
    let Some(peer_id) = peer_id else {
        return String::new();
    };
    if peer_id.chars().count() > 8 {
        peer_id.chars().take(8).collect()
    } else {
        peer_id.to_string()
    }
}

/// 对齐上游 `PeerRecordingServiceModule`（配置键 `peer-analyse-service.peer-recording`）。
///
/// - `onPeersRetrieved` → [`Self::on_peers_retrieved`]：只保留「客户端名非空 / peer-id 非空 /
///   非握手中」的 peer，写入磁盘写缓存（键 `(downloader, torrent, peer 地址)`）；
/// - `flush` → [`Self::flush`]：把缓存整体写库（对齐 `flush()` 的批量 upsert，写后 dirty = false）；
/// - `cleanup` → [`Self::cleanup`]：`data-retention-time > 0` 时删除
///   `last_time_seen < now - data-retention-time`（**开区间** `lt`）；
/// - `check` 恒 `pass()`。
///
/// 与上游的差异：上游缓存 `PBHCache`（容量 [`PEER_RECORDING_CACHE_SIZE`]、
/// 超时 [`PEER_RECORDING_CACHE_TIMEOUT_MS`]，淘汰时后台刷库），本移植用普通 `HashMap`，
/// 仅由定时 `flush` 写库。
pub struct PeerRecordingServiceModule {
    pub settings: PeerRecordingSettings,
    sink: Arc<dyn MonitorSink>,
    cache: Mutex<HashMap<PeerRecordCacheKey, PeerRecordCachingEntire>>,
}

impl PeerRecordingServiceModule {
    pub fn new(sink: Arc<dyn MonitorSink>, settings: PeerRecordingSettings) -> Self {
        Self {
            settings,
            sink,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 对齐 `onPeersRetrieved` 的过滤 + 缓存写入。
    pub fn on_peers_retrieved(
        &self,
        downloader_id: &str,
        torrent: &TorrentData,
        peers: &[PeerData],
        now_ms: i64,
    ) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        for peer in peers {
            let client_name = peer.client_name.as_deref().unwrap_or("");
            let peer_id = peer.peer_id.as_deref().unwrap_or("");
            let keep = (!client_name.trim().is_empty())
                || (!peer_id.trim().is_empty())
                || !peer.is_handshaking();
            if !keep {
                continue;
            }
            let cache_key = PeerRecordCacheKey {
                downloader: downloader_id.to_string(),
                torrent_hash: torrent.hash.clone(),
                peer_ip: compressed_ip(&peer.ip),
                peer_port: peer.port,
            };
            let current = PeerRecordCachingEntire::new(now_ms, downloader_id, torrent, peer);
            // 上游：`inCache = cache.get(key, () -> current)`，不相等才 `put`（相等则不做任何事）
            match cache.get(&cache_key) {
                Some(cached) if cached.same_as(&current) => {}
                _ => {
                    cache.insert(cache_key, current);
                }
            }
        }
    }

    /// 对齐 `flush()`：事务内把所有缓存条目 `flushToDatabase`（并置 dirty = false）。
    pub fn flush(&self) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let entries: Vec<PeerRecordCachingEntire> = cache.values().cloned().collect();
        for entry in &entries {
            let row = self.to_row(entry);
            self.sink.upsert_peer_record(&row);
        }
        for entry in cache.values_mut() {
            entry.dirty = false;
        }
    }

    /// 对齐 `cleanup()`：`dataRetentionTime <= 0` 时直接返回（上游 log 后 return）。
    ///
    /// 返回 `None` 表示未执行；`Some(n)` 为删除条数。
    pub fn cleanup(&self, now_ms: i64) -> Option<usize> {
        if self.settings.data_retention_time_ms <= 0 {
            return None;
        }
        let before = now_ms - self.settings.data_retention_time_ms;
        Some(self.sink.remove_peer_records_before(before))
    }

    /// 缓存条目数（供日志/测试）。
    pub fn cache_len(&self) -> usize {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 对齐 `flushToDatabase(PeerRecordCachingEntire)`。
    fn to_row(&self, entry: &PeerRecordCachingEntire) -> PeerRecordRow {
        let torrent_id = self.sink.ensure_torrent(&entry.torrent);
        let peer = &entry.peer;
        PeerRecordRow {
            address: compressed_ip(&peer.ip),
            port: peer.port,
            torrent_id,
            downloader: entry.downloader.clone(),
            peer_id: truncate_peer_id(peer.peer_id.as_deref()),
            client_name: peer.client_name.clone().unwrap_or_default(),
            // uploaded / downloaded 同时写入「上报值」与「偏移量」
            uploaded: peer.uploaded,
            uploaded_offset: peer.uploaded,
            upload_speed: peer.up_speed,
            downloaded: peer.downloaded,
            downloaded_offset: peer.downloaded,
            download_speed: peer.dl_speed,
            last_flags: lt_std_string_opt(peer.flags.as_deref()),
            // 上游 flush 时把 first/last time seen 都写成缓存条目的时间戳
            first_time_seen_ms: entry.timestamp_ms,
            last_time_seen_ms: entry.timestamp_ms,
            // 上游在 DAO 内用 IPDBManager 填充；本移植交由 DB 版 sink 填充
            peer_geoip: None,
        }
    }
}

impl RuleModule for PeerRecordingServiceModule {
    fn name(&self) -> &str {
        "Peer Recording Service"
    }

    fn config_name(&self) -> &str {
        "peer-analyse-service.peer-recording"
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

// ===========================================================================
// peer-analyse-service.swarm-tracking
// ===========================================================================

/// `module.peer-analyse-service.swarm-tracking` 的运行期配置。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SwarmTrackingSettings {
    /// `data-flush-interval`（对齐 `getLong("data-flush-interval")`，缺省 0）
    pub data_flush_interval_ms: i64,
}

/// 对齐上游 `SwarmTrackingModule`（配置键 `peer-analyse-service.swarm-tracking`）。
///
/// 只跟踪**本次运行会话**内的 swarm peer：`onEnable` → [`Self::on_enable`]（`resetTable`）
/// 清空整张临时表，因此重启后数据自动丢弃；`flushAll` → [`Self::flush_all`] 按
/// `data-flush-interval` 把缓存 upsert 入库；`check` 恒 `pass()`。
///
/// 与上游的差异：上游缓存 `PBHCache`（容量 1000、3 分钟超时淘汰时后台刷库），
/// 本移植用普通 `HashMap`，仅由定时 `flush_all` 写库。
pub struct SwarmTrackingModule {
    pub settings: SwarmTrackingSettings,
    sink: Arc<dyn MonitorSink>,
    cache: Mutex<HashMap<TrackedSwarmKey, TrackedSwarmRow>>,
}

impl SwarmTrackingModule {
    pub fn new(sink: Arc<dyn MonitorSink>, settings: SwarmTrackingSettings) -> Self {
        Self {
            settings,
            sink,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 对齐 `onEnable()`：`trackedSwarmDao.resetTable()`（清空临时表 + 本地缓存）。
    pub fn on_enable(&self) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.sink.reset_tracked_swarm();
    }

    /// 对齐 `onPeersRetrieved`：握手中的 peer 跳过（其余 peer 逐个 `syncPeers`）。
    pub fn sync_peers(
        &self,
        downloader_id: &str,
        torrent: &TorrentData,
        peers: &[PeerData],
        now_ms: i64,
    ) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        for peer in peers {
            if peer.is_handshaking() {
                continue;
            }
            let key = TrackedSwarmKey {
                ip: compressed_ip(&peer.ip),
                port: peer.port,
                info_hash: torrent.hash.clone(),
                downloader: downloader_id.to_string(),
            };
            let mut entity = match cache.get(&key) {
                Some(cached) => cached.clone(),
                None => {
                    // 上游 cache-miss：先按唯一键取最后一行；命中则只刷新 last_time_seen
                    match self.sink.load_last_tracked_swarm(
                        &key.ip,
                        key.port,
                        &key.info_hash,
                        &key.downloader,
                    ) {
                        Some(mut last) => {
                            last.last_time_seen_ms = now_ms;
                            last
                        }
                        None => new_tracked_swarm_row(downloader_id, torrent, peer, now_ms),
                    }
                }
            };
            // 上游：peer.getDownloaded() >= offset ? 差值 : peer.getDownloaded()
            // （计数被重置时按整值累加）
            let new_downloaded = if peer.downloaded >= entity.downloaded_offset {
                peer.downloaded - entity.downloaded_offset
            } else {
                peer.downloaded
            };
            let new_uploaded = if peer.uploaded >= entity.uploaded_offset {
                peer.uploaded - entity.uploaded_offset
            } else {
                peer.uploaded
            };
            entity.downloaded += new_downloaded;
            entity.uploaded += new_uploaded;
            entity.downloaded_offset = peer.downloaded;
            entity.uploaded_offset = peer.uploaded;
            entity.client_name = peer.client_name.clone().unwrap_or_default();
            entity.peer_id = peer.peer_id.clone().unwrap_or_default();
            entity.last_flags = peer.flags.as_deref().map(lt_std_string).unwrap_or_default();
            entity.last_time_seen_ms = now_ms;
            entity.download_speed_max = entity.download_speed_max.max(peer.dl_speed);
            entity.upload_speed_max = entity.upload_speed_max.max(peer.up_speed);
            entity.dirty = true;
            cache.insert(key, entity);
        }
    }

    /// 对齐 `flushAll()`：把缓存里的跟踪行整体 upsert 入库（上游同样不清空缓存）。
    pub fn flush_all(&self) {
        let rows: Vec<TrackedSwarmRow> = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.values().cloned().collect()
        };
        for row in rows {
            self.sink.upsert_tracked_swarm(&row);
        }
    }

    /// 对齐 `trackedSwarmDao.count()`（Web API `/api/modules/swarm-tracking`）。
    pub fn count(&self) -> usize {
        self.sink.count_tracked_swarm()
    }

    /// 缓存条目数（供日志/测试）。
    pub fn cache_len(&self) -> usize {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// 对齐 `new TrackedSwarmEntity(...)` 的字段顺序与初值。
fn new_tracked_swarm_row(
    downloader_id: &str,
    torrent: &TorrentData,
    peer: &PeerData,
    now_ms: i64,
) -> TrackedSwarmRow {
    TrackedSwarmRow {
        id: None,
        ip: compressed_ip(&peer.ip),
        port: peer.port,
        info_hash: torrent.hash.clone(),
        torrent_is_private: torrent.is_private,
        torrent_size: torrent.total_size,
        downloader: downloader_id.to_string(),
        downloader_progress: torrent.progress,
        peer_id: peer.peer_id.clone().unwrap_or_default(),
        client_name: peer.client_name.clone().unwrap_or_default(),
        peer_progress: peer.progress,
        uploaded: 0,
        uploaded_offset: 0,
        upload_speed: peer.up_speed,
        downloaded: 0,
        downloaded_offset: 0,
        download_speed: peer.dl_speed,
        last_flags: peer.flags.as_deref().map(lt_std_string).unwrap_or_default(),
        first_time_seen_ms: now_ms,
        last_time_seen_ms: now_ms,
        download_speed_max: peer.dl_speed,
        upload_speed_max: peer.up_speed,
        dirty: true,
    }
}

impl RuleModule for SwarmTrackingModule {
    fn name(&self) -> &str {
        "Swarm Tracking Module"
    }

    fn config_name(&self) -> &str {
        "peer-analyse-service.swarm-tracking"
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::PeerAction;

    fn sink() -> Arc<InMemoryMonitorSink> {
        Arc::new(InMemoryMonitorSink::new())
    }

    fn torrent() -> TorrentData {
        TorrentData {
            hash: "abcdef0123456789".to_string(),
            name: "示例种子".to_string(),
            progress: 0.5,
            total_size: 1_000_000_000,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: Some(false),
        }
    }

    fn peer(ip: &str, port: u16, flags: &str) -> PeerData {
        PeerData {
            client_name: Some("qBittorrent 4.6.0".to_string()),
            peer_id: Some("-qB4600-abcdefghijkl".to_string()),
            dl_speed: 1024,
            downloaded: 4096,
            up_speed: 2048,
            uploaded: 8192,
            progress: 0.25,
            flags: Some(flags.to_string()),
            ip: ip.to_string(),
            port,
            raw_ip: format!("{ip}:{port}"),
            connection: Some("uTP".to_string()),
        }
    }

    fn handshaking_peer(ip: &str, port: u16) -> PeerData {
        let mut p = peer(ip, port, "");
        p.up_speed = 0;
        p.dl_speed = 0;
        p.client_name = None;
        p.peer_id = None;
        p
    }

    fn track_row(
        timeframe_at_ms: i64,
        downloader: &str,
        address: &str,
        port: u16,
        last_flags: Option<&str>,
    ) -> MetricsTrackRow {
        MetricsTrackRow {
            key: MetricsTrackKey {
                timeframe_at_ms,
                downloader: downloader.to_string(),
                torrent_id: "t1".to_string(),
                address: address.to_string(),
                port,
            },
            peer_id: None,
            client_name: None,
            last_flags: last_flags.map(|s| s.to_string()),
        }
    }

    fn metrics_row(timeframe_at_ms: i64, downloader: &str, total: i64) -> ConnectionMetricsRow {
        ConnectionMetricsRow {
            timeframe_at_ms,
            downloader: downloader.to_string(),
            total_connections: total,
            ..Default::default()
        }
    }

    // ---------------- 工具函数 ----------------

    #[test]
    fn human_readable_byte_count_matches_upstream() {
        // 对齐 `MsgUtil.humanReadableByteCountBin`
        assert_eq!(human_readable_byte_count_bin(0), "0 B");
        assert_eq!(human_readable_byte_count_bin(1023), "1023 B");
        assert_eq!(human_readable_byte_count_bin(1024), "1.0 KiB");
        assert_eq!(human_readable_byte_count_bin(1536), "1.5 KiB");
        assert_eq!(human_readable_byte_count_bin(5 * 1024 * 1024), "5.0 MiB");
        assert_eq!(human_readable_byte_count_bin(25_000_000_000), "23.3 GiB");
        assert_eq!(human_readable_byte_count_bin(i64::MAX), "8.0 EiB");
        assert_eq!(human_readable_byte_count_bin(-1024), "-1.0 KiB");
        // 对齐 `MsgUtil.humanReadableByteCountSI`
        assert_eq!(human_readable_byte_count_si(999), "999 B");
        assert_eq!(human_readable_byte_count_si(1000), "1.0 kB");
        assert_eq!(human_readable_byte_count_si(5_000_000), "5.0 MB");
        assert_eq!(human_readable_byte_count_si(-1000), "-1.0 kB");
    }

    #[test]
    fn lt_std_string_rebuilds_canonical_flags() {
        // 对齐 `PeerFlag#toString()` 的顺序与默认值语义
        assert_eq!(lt_std_string("d u"), "d u");
        assert_eq!(lt_std_string("D U O S X L E e P"), "D U O S X L E e P");
        // 'I' 表示非本地连接；空串保持为空串（Java 中 `""` 不是 null）
        assert_eq!(lt_std_string(""), "");
        assert_eq!(lt_std_string("I"), "I");
        // 'K'（远程未 choke 且我们不感兴趣）与 '?'（我们未 choke 且对方不感兴趣）
        assert_eq!(lt_std_string("K ?"), "K ?");
        assert_eq!(lt_std_string_opt(None), None);
        assert_eq!(lt_std_string_opt(Some("")), Some(String::new()));
        assert_eq!(
            compressed_ip("2001:0db8:0000:0000:0000:0000:0000:0001"),
            "2001:db8::1"
        );
        assert_eq!(compressed_ip("::ffff:1.2.3.4"), "1.2.3.4");
        assert_eq!(compressed_ip("not-an-ip"), "not-an-ip");
    }

    #[test]
    fn local_day_and_hour_boundaries_are_local() {
        let now = 1_700_000_000_123i64;
        let start_of_today = start_of_today_ms(now);
        let start_of_hour = start_of_hour_ms(now);
        assert!(start_of_today <= now);
        assert!(now - start_of_today < 90_000_000);
        assert!(start_of_hour <= now);
        assert!(now - start_of_hour < 3_600_000);
        // 起点是固定点（幂等）
        assert_eq!(start_of_today_ms(start_of_today), start_of_today);
        assert_eq!(start_of_hour_ms(start_of_hour), start_of_hour);
        // 本地时间语义：当天 0 点整 / 当前小时的第 0 分 0 秒
        assert_eq!(
            local_naive(start_of_today).format("%H:%M:%S").to_string(),
            "00:00:00"
        );
        assert_eq!(
            local_naive(start_of_hour).format("%M:%S").to_string(),
            "00:00"
        );
        // 当日 23:59:59.999 晚于当前时刻
        assert!(end_of_today_ms(now) > now);
        assert!(end_of_today_ms(now) - start_of_today < 90_000_000);
        // 格式化
        assert_eq!(format_date_only(now).len(), 10);
        assert_eq!(format_date_time(now).len(), 19);
    }

    // ---------------- 模块元信息与 pass-through ----------------

    #[test]
    fn config_names_match_upstream() {
        let s = sink();
        let active = ActiveMonitoringModule::new(s.clone(), ActiveMonitoringSettings::default());
        assert_eq!(active.name(), "Active Monitoring");
        assert_eq!(active.config_name(), "active-monitoring");

        let session =
            SessionAnalyseServiceModule::new(s.clone(), SessionAnalyseSettings::default());
        assert_eq!(session.name(), "Session Analyse Service");
        assert_eq!(
            session.config_name(),
            "peer-analyse-service.session-analyse"
        );

        let recording =
            PeerRecordingServiceModule::new(s.clone(), PeerRecordingSettings::default());
        assert_eq!(recording.name(), "Peer Recording Service");
        assert_eq!(
            recording.config_name(),
            "peer-analyse-service.peer-recording"
        );

        let swarm = SwarmTrackingModule::new(s, SwarmTrackingSettings::default());
        assert_eq!(swarm.name(), "Swarm Tracking Module");
        assert_eq!(swarm.config_name(), "peer-analyse-service.swarm-tracking");
    }

    #[test]
    fn all_four_modules_check_is_passthrough() {
        let s = sink();
        let modules: Vec<Box<dyn RuleModule>> = vec![
            Box::new(ActiveMonitoringModule::new(
                s.clone(),
                ActiveMonitoringSettings::default(),
            )),
            Box::new(SessionAnalyseServiceModule::new(
                s.clone(),
                SessionAnalyseSettings::default(),
            )),
            Box::new(PeerRecordingServiceModule::new(
                s.clone(),
                PeerRecordingSettings::default(),
            )),
            Box::new(SwarmTrackingModule::new(
                s,
                SwarmTrackingSettings::default(),
            )),
        ];
        let ctx = CheckContext {
            now_ms: 1_700_000_000_000,
            features: Vec::new(),
        };
        for module in &modules {
            for candidate in [
                peer("1.2.3.4", 51413, "d u"),
                handshaking_peer("10.0.0.1", 6881),
            ] {
                let result = module.check("qb", &torrent(), &candidate, &ctx);
                assert_eq!(
                    result.action,
                    PeerAction::NoAction,
                    "{}",
                    module.config_name()
                );
                assert_eq!(result.ban_duration_ms, 0);
                assert_eq!(result.module, module.config_name());
                assert_eq!(result.data["status"], "pass");
                assert_eq!(
                    result.reason_key,
                    Some(TranslationComponent::new("Check passed"))
                );
            }
        }
    }

    // ---------------- active-monitoring ----------------

    fn active_module(
        s: Arc<InMemoryMonitorSink>,
        daily: i64,
        sliding: bool,
        threshold: i64,
        min_speed: i64,
        max_speed: i64,
    ) -> ActiveMonitoringModule {
        ActiveMonitoringModule::new(
            s,
            ActiveMonitoringSettings {
                daily_traffic_capping: daily,
                use_traffic_sliding_capping: sliding,
                max_traffic_allowed_in_window_period: threshold,
                traffic_sliding_capping_max_speed: max_speed,
                traffic_sliding_capping_min_speed: min_speed,
            },
        )
    }

    #[test]
    fn tick_writes_hourly_journal_and_skips_logged_out_downloaders() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        let module = active_module(s.clone(), -1, false, 0, 0, 0);
        let stats = vec![
            DownloaderTrafficStats::new("qb", "qBittorrent", true, 111, 222, None, true),
            DownloaderTrafficStats::new("tr", "Transmission", false, 333, 444, None, true),
        ];
        let (changes, alert) = module.on_tick(&stats, now);
        assert!(changes.is_empty());
        assert!(alert.is_none(), "daily = -1 ⇒ 阈值告警禁用");

        let journal = s.traffic_journal();
        assert_eq!(journal.len(), 1, "未登录成功的下载器不写日志");
        assert_eq!(journal[0].downloader, "qb");
        assert_eq!(journal[0].timestamp_ms, start_of_hour_ms(now));
        assert_eq!(journal[0].data_overall_downloaded, 111);
        assert_eq!(journal[0].data_overall_uploaded, 222);
        assert_eq!(journal[0].data_overall_uploaded_at_start, 222);

        // 同一小时内的第二次 tick 只做 max 更新，at_start 保持不变
        let stats = vec![DownloaderTrafficStats::new(
            "qb",
            "qBittorrent",
            true,
            111,
            999,
            None,
            true,
        )];
        module.on_tick(&stats, now + 60_000);
        let journal = s.traffic_journal();
        assert_eq!(journal.len(), 1);
        assert_eq!(journal[0].data_overall_uploaded, 999);
        assert_eq!(journal[0].data_overall_uploaded_at_start, 222);
    }

    #[test]
    fn daily_threshold_alert_fires_once_and_matches_upstream_payload() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        let module = active_module(s.clone(), 1000, false, 0, 0, 0);
        // 当日已上传 2000 bytes：先建行（at_start = 0）再在同小时内更新，使当日增量为 2000
        s.update_traffic_journal("qb", 0, 0, 0, 0, now);
        s.update_traffic_journal("qb", 0, 2000, 0, 0, now + 60_000);

        let alert = module.update_traffic_monitoring(now).expect("应发布告警");
        assert_eq!(alert.alert.level, AlertLevel::Warn);
        assert!(alert.alert.push);
        assert_eq!(
            alert.alert.identifier,
            format!("dataTrafficCapping-{}", start_of_today_ms(now) / 1000)
        );
        assert_eq!(alert.total_uploaded, 2000);
        assert_eq!(alert.threshold, 1000);
        assert_eq!(
            alert.alert.title.key,
            "MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_TITLE"
        );
        assert_eq!(alert.alert.title.params, vec![format_date_only(now).into()]);
        assert_eq!(
            alert.alert.description.key,
            "MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_DESCRIPTION"
        );
        assert_eq!(
            alert.alert.description.params,
            vec![
                format_date_time(now).into(),
                "2.0 KiB".to_string().into(),
                "1000 B".to_string().into(),
            ]
        );
        assert_eq!(s.alerts().len(), 1);

        // 一天只发一次：同一 identifier 已存在（含已读）时不再发布
        assert!(module.update_traffic_monitoring(now + 1000).is_none());
        assert_eq!(s.alerts().len(), 1);
    }

    #[test]
    fn daily_threshold_boundary_and_disabled_state() {
        let now = 1_700_000_000_000i64;

        // daily <= 0（-1 或 0）：完全禁用，即使流量巨大也不告警
        for disabled in [-1i64, 0] {
            let s = sink();
            let module = active_module(s.clone(), disabled, false, 0, 0, 0);
            s.update_traffic_journal("qb", 0, 1000, 0, 0, now);
            s.update_traffic_journal("qb", 0, i64::from(i32::MAX), 0, 0, now);
            assert!(module.update_traffic_monitoring(now).is_none());
            assert!(s.alerts().is_empty());
        }

        // 低于阈值（1999 < 2000）：不告警
        let below = sink();
        let module = active_module(below.clone(), 2000, false, 0, 0, 0);
        below.update_traffic_journal("qb", 0, 0, 0, 0, now);
        below.update_traffic_journal("qb", 0, 1999, 0, 0, now);
        assert!(module.update_traffic_monitoring(now).is_none());
        assert!(below.alerts().is_empty());

        // 恰好等于阈值：上游只判断 `totalBytes < dailyTrafficCapping` 才返回 -> 会告警
        let equal = sink();
        let module = active_module(equal.clone(), 2000, false, 0, 0, 0);
        equal.update_traffic_journal("qb", 0, 0, 0, 0, now);
        equal.update_traffic_journal("qb", 0, 2000, 0, 0, now);
        assert!(module.update_traffic_monitoring(now).is_some());
        assert_eq!(equal.alerts().len(), 1);
    }

    #[test]
    fn sliding_window_limiter_math_vectors() {
        let now = 1_700_000_000_000i64;

        // 向量 1：超阈值且当前限速高于 min -> b = 已上传/阈值，新限速 = max(min, 当前/b)
        let got = compute_sliding_window_limiter(2500, 5000, 1000, 0, 10_000, now);
        assert_eq!(got.uploaded_in_window, 2500);
        assert_eq!(got.old_speed_limit, 5000);
        assert_eq!(got.new_speed_limit, 2000);
        assert_eq!(got.decrease_factor, Some(2.5));
        assert_eq!(got.increase_factor, None);
        assert_eq!(got.reached_minimum_speed, None);
        assert_eq!(got.reached_maximum_speed, None);
        assert_eq!(got.window_size_millis, SLIDING_WINDOW_MILLIS);
        assert_eq!(got.window_start_time, now - SLIDING_WINDOW_MILLIS);

        // 向量 2：超阈值且当前限速 <= min -> 取 min 并标记 reachedMinimumSpeed
        let got = compute_sliding_window_limiter(1000, 500, 1000, 600, 10_000, now);
        assert_eq!(got.new_speed_limit, 600);
        assert_eq!(got.reached_minimum_speed, Some(true));
        assert_eq!(got.decrease_factor, None);

        // 向量 3：未超阈值且当前限速已达 max（max > 0）-> 取 max，且上游不设置 increaseFactor
        let got = compute_sliding_window_limiter(250_000, 10_000, 1_000_000, 0, 10_000, now);
        assert_eq!(got.new_speed_limit, 10_000);
        assert_eq!(got.increase_factor, None);
        assert_eq!(got.decrease_factor, None);
        assert_eq!(got.reached_maximum_speed, None);

        // 向量 4：未超阈值且低于 max -> a = (阈值 - 已上传)/窗口毫秒 * 1000，新限速 = 当前 + (long)a
        let got = compute_sliding_window_limiter(250_000, 0, 1_000_000, 0, 10_000, now);
        let expected_a = 750_000f64 / SLIDING_WINDOW_MILLIS as f64 * 1000.0;
        assert_eq!(got.new_speed_limit, expected_a.trunc() as i64);
        assert_eq!(got.new_speed_limit, 8);
        assert!((got.increase_factor.unwrap() - expected_a).abs() < 1e-9);

        // 向量 5：阈值为 0 时 b 退化为 1.0（上游 `thresholdBytes > 0 ? ... : 1.0`）
        let got = compute_sliding_window_limiter(0, 5000, 0, 0, 10_000, now);
        assert_eq!(got.new_speed_limit, 5000);
        assert_eq!(got.decrease_factor, Some(1.0));

        // 向量 6：max = 0（不限速）时不进入 max 分支，取 min(0, 当前 + a)
        let got = compute_sliding_window_limiter(999_999, 100, 1_000_000, 0, 0, now);
        assert_eq!(got.new_speed_limit, 0);
        assert!(got.increase_factor.unwrap() > 0.0);
    }

    #[test]
    fn sliding_window_capping_uses_window_traffic_and_keeps_download_limit() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        // 阈值 1000 bytes / 24h，min 0、max 10000 bytes/s
        let module = active_module(s.clone(), -1, true, 1000, 0, 10_000);
        // 窗口内已上传 2500 bytes
        s.update_traffic_journal("qb", 0, 0, 0, 0, now);
        s.update_traffic_journal("qb", 0, 2500, 0, 0, now);
        // 窗口外的历史数据（25 小时前）不参与计算
        let old = now - SLIDING_WINDOW_MILLIS - 3_600_000;
        s.update_traffic_journal("qb", 0, 0, 0, 0, old);
        s.update_traffic_journal("qb", 0, 999_999, 0, 0, old);

        let stats = vec![
            DownloaderTrafficStats::new(
                "qb",
                "qBittorrent",
                true,
                0,
                0,
                Some(SpeedLimiter {
                    upload: 5000,
                    download: 777,
                }),
                true,
            ),
            // 不支持限速（getSpeedLimiter() == null）-> 跳过
            DownloaderTrafficStats::new("no-limiter", "NoLimiter", true, 0, 0, None, true),
            // 无 TRAFFIC_STATS 特性标志 -> 跳过
            DownloaderTrafficStats::new(
                "no-stats",
                "NoStats",
                true,
                0,
                0,
                Some(SpeedLimiter {
                    upload: 5000,
                    download: 777,
                }),
                false,
            ),
            // 未登录成功 -> 跳过
            DownloaderTrafficStats::new(
                "logged-out",
                "LoggedOut",
                false,
                0,
                0,
                Some(SpeedLimiter {
                    upload: 5000,
                    download: 777,
                }),
                true,
            ),
        ];
        let changes = module.update_traffic_capping_service(&stats, now);
        assert_eq!(changes.len(), 1);
        let change = &changes[0];
        assert_eq!(change.downloader_id, "qb");
        assert_eq!(change.downloader_name, "qBittorrent");
        assert_eq!(change.computed.uploaded_in_window, 2500);
        assert_eq!(change.computed.decrease_factor, Some(2.5));
        assert_eq!(change.limiter.upload, 2000);
        assert_eq!(change.limiter.download, 777, "下载限速保持原值");
        assert_eq!(change.limiter.upload, change.computed.new_speed_limit);
    }

    #[test]
    fn sliding_window_capping_respects_enable_flag() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        let disabled = active_module(s.clone(), -1, false, 1000, 0, 10_000);
        s.update_traffic_journal("qb", 0, 0, 0, 0, now);
        s.update_traffic_journal("qb", 0, 2500, 0, 0, now);
        let stats = vec![DownloaderTrafficStats::new(
            "qb",
            "qBittorrent",
            true,
            0,
            0,
            Some(SpeedLimiter {
                upload: 5000,
                download: 777,
            }),
            true,
        )];
        let (changes, alert) = disabled.on_tick(&stats, now);
        assert!(changes.is_empty());
        assert!(alert.is_none());
        assert!(disabled
            .update_traffic_capping_service(&stats, now)
            .is_empty());
    }

    // ---------------- session-analyse ----------------

    #[test]
    fn aggregation_counts_every_flag_branch() {
        let today = 1_700_000_000_000i64;
        let rows = vec![
            // I d u O H E P：非本地连接 + 我们感兴趣但被 choke + 对方感兴趣且被我们 choke
            // + optimistic + DHT + rc4 + uTP
            track_row(today, "qb", "1.2.3.4", 51413, Some("I d u O H E P")),
            // D X：我们感兴趣且未被 choke（remoteAccept）+ PEX
            track_row(today, "qb", "1.2.3.5", 51413, Some("D X")),
            // 空串（非 null）：所有位为默认值 -> fromTrackerOrOther + tcpSocket
            track_row(today, "qb", "1.2.3.6", 51413, Some("")),
            // 无 flags：只累加 totalConnections
            track_row(today, "qb", "1.2.3.7", 51413, None),
        ];
        let agg = aggregate_connection_metrics(&rows);
        assert_eq!(agg.len(), 1);
        let m = &agg[0];
        assert_eq!(m.timeframe_at_ms, today);
        assert_eq!(m.downloader, "qb");
        assert_eq!(m.total_connections, 4);
        assert_eq!(m.incoming_connections, 1); // I
        assert_eq!(m.remote_refuse_transfer_to_client, 1); // d
        assert_eq!(m.remote_accept_transfer_to_client, 1); // D
        assert_eq!(m.local_refuse_transfer_to_peer, 1); // u
        assert_eq!(m.local_accept_transfer_to_peer, 0);
        assert_eq!(m.local_not_interested, 0);
        assert_eq!(m.question_status, 0);
        assert_eq!(m.optimistic_unchoke, 1);
        assert_eq!(m.from_dht, 1);
        assert_eq!(m.from_pex, 1);
        assert_eq!(m.from_lsd, 0);
        assert_eq!(m.from_tracker_or_other, 1); // 空串所在分支
        assert_eq!(m.rc4_encrypted, 1);
        assert_eq!(m.plain_text_encrypted, 0);
        assert_eq!(m.utp_socket, 1);
        assert_eq!(m.tcp_socket, 2); // "D X" 与空串
    }

    #[test]
    fn aggregation_groups_by_timeframe_and_downloader() {
        let day1 = 1_700_000_000_000i64;
        let day2 = day1 + 86_400_000;
        let rows = vec![
            track_row(day1, "qb", "1.2.3.4", 1, Some("D")),
            track_row(day1, "tr", "1.2.3.5", 2, Some("D")),
            track_row(day2, "qb", "1.2.3.6", 3, Some("D")),
            track_row(day1, "qb", "1.2.3.7", 4, Some("D")),
        ];
        let agg = aggregate_connection_metrics(&rows);
        // 分组缓冲顺序 = 输入顺序（与上游 findOrCreateBuffer 一致）
        assert_eq!(agg.len(), 3);
        assert_eq!(
            (
                agg[0].timeframe_at_ms,
                agg[0].downloader.as_str(),
                agg[0].total_connections
            ),
            (day1, "qb", 2)
        );
        assert_eq!(
            (
                agg[1].timeframe_at_ms,
                agg[1].downloader.as_str(),
                agg[1].total_connections
            ),
            (day1, "tr", 1)
        );
        assert_eq!(
            (
                agg[2].timeframe_at_ms,
                agg[2].downloader.as_str(),
                agg[2].total_connections
            ),
            (day2, "qb", 1)
        );
        assert!(aggregate_connection_metrics(&[]).is_empty());
    }

    #[test]
    fn flush_data_aggregates_both_branches_and_deletes_tracks() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        let today = start_of_today_ms(now);
        let yesterday = today - 86_400_000;
        let module = SessionAnalyseServiceModule::new(
            s.clone(),
            SessionAnalyseSettings {
                cleanup_interval_ms: 3_600_000,
                data_flush_interval_ms: 3_600_000,
                data_retention_time_ms: 15_552_000_000,
            },
        );

        // 「不在今天」的旧 track 行 + 一条已存在的聚合行（overwrite=true 应被覆盖）
        s.upsert_metrics_tracks(&[track_row(yesterday, "qb", "1.2.3.4", 51413, Some("D"))]);
        s.save_connection_metrics(&[metrics_row(yesterday, "qb", 5)], true);
        // 「在今天」的 track 行 + 一条已存在的聚合行（overwrite=false 应合并）
        s.upsert_metrics_tracks(&[track_row(today, "qb", "1.2.3.5", 51413, Some("D"))]);
        s.save_connection_metrics(&[metrics_row(today, "qb", 5)], true);

        // 走 onPeersRetrieved 路径再塞一条今天的 track（握手中的 peer 被跳过）
        module.sync_peers(
            "qb",
            &torrent(),
            &[
                peer("1.2.3.6", 51413, "d"),
                handshaking_peer("1.2.3.7", 51413),
            ],
            now,
        );
        assert_eq!(module.cache_len(), 1);

        let summary = module.flush_data(now);
        assert_eq!(summary.aggregated_rows_out_of_day, 1);
        assert_eq!(summary.aggregated_rows_in_day, 1);
        // 1 条昨天的 track 行 + 2 条今天的 track 行（预置 1 条 + onPeersRetrieved 1 条）
        assert_eq!(summary.deleted, 3);
        assert_eq!(summary.saved_out_of_day[0].total_connections, 1);
        assert_eq!(summary.saved_in_day[0].total_connections, 2);
        assert!(s.metrics_tracks().is_empty(), "聚合后的 track 行全部删除");

        let metrics = s.connection_metrics();
        assert_eq!(metrics.len(), 2);
        let out_of_day = metrics
            .iter()
            .find(|m| m.timeframe_at_ms == yesterday)
            .unwrap();
        assert_eq!(out_of_day.total_connections, 1, "overwrite=true 覆盖旧值 5");
        let in_day = metrics.iter().find(|m| m.timeframe_at_ms == today).unwrap();
        assert_eq!(
            in_day.total_connections, 7,
            "overwrite=false 在旧值 5 上合并 2"
        );
    }

    #[test]
    fn sync_peers_reuses_existing_track_row() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        let today = start_of_today_ms(now);
        let module = SessionAnalyseServiceModule::new(s.clone(), SessionAnalyseSettings::default());
        // 先有一条同键的 track 行（模拟上一次运行写入的数据）
        let key = MetricsTrackKey {
            timeframe_at_ms: today,
            downloader: "qb".to_string(),
            torrent_id: torrent().hash,
            address: "1.2.3.4".to_string(),
            port: 51413,
        };
        let mut existing = MetricsTrackRow::new(key.clone());
        existing.peer_id = Some("old".to_string());
        existing.last_flags = Some("K".to_string());
        s.upsert_metrics_tracks(&[existing]);

        module.sync_peers("qb", &torrent(), &[peer("1.2.3.4", 51413, "d u")], now);
        module.flush_all();
        let rows = s.metrics_tracks();
        assert_eq!(rows.len(), 1, "同键不新增行");
        assert_eq!(rows[0].peer_id, Some("-qB4600-abcdefghijkl".to_string()));
        assert_eq!(rows[0].client_name, Some("qBittorrent 4.6.0".to_string()));
        assert_eq!(rows[0].last_flags, Some("d u".to_string()));
        assert_eq!(rows[0].key, key);
    }

    #[test]
    fn connection_metrics_merge_skips_local_not_interested_like_upstream() {
        let mut base = metrics_row(1_700_000_000_000, "qb", 1);
        base.local_not_interested = 4;
        let mut appender = metrics_row(1_700_000_000_000, "qb", 2);
        appender.local_not_interested = 9;
        appender.from_pex = 3;
        base.merge(&appender);
        assert_eq!(base.total_connections, 3);
        assert_eq!(base.from_pex, 3);
        // 上游 merge() 漏掉 local_not_interested
        assert_eq!(base.local_not_interested, 4);
    }

    #[test]
    fn session_analyse_retention_boundary_is_inclusive() {
        let s = sink();
        let now = 10_000i64;
        let before = now - 1_000;
        let module = SessionAnalyseServiceModule::new(
            s.clone(),
            SessionAnalyseSettings {
                data_retention_time_ms: 1_000,
                ..Default::default()
            },
        );
        s.save_connection_metrics(
            &[
                metrics_row(before, "qb", 7),
                metrics_row(before + 1, "qb", 8),
            ],
            true,
        );
        // 对齐 `le(timeframe_at, before)`：恰好等于边界的数据也会被删除
        assert_eq!(module.cleanup(now), 1);
        let left = s.connection_metrics();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].timeframe_at_ms, before + 1);

        // 保留期为 0 时上游没有保护：before = now，删除 `timeframe_at <= now`
        let s2 = sink();
        let module =
            SessionAnalyseServiceModule::new(s2.clone(), SessionAnalyseSettings::default());
        s2.save_connection_metrics(
            &[metrics_row(now, "qb", 1), metrics_row(now + 1, "qb", 2)],
            true,
        );
        assert_eq!(module.cleanup(now), 1);
        assert_eq!(s2.connection_metrics().len(), 1);
    }

    // ---------------- peer-recording ----------------

    fn peer_record_row(last_time_seen_ms: i64, address: &str, uploaded: i64) -> PeerRecordRow {
        PeerRecordRow {
            address: address.to_string(),
            port: 51413,
            torrent_id: "t1".to_string(),
            downloader: "qb".to_string(),
            peer_id: "-qB4600-".to_string(),
            client_name: "qBittorrent".to_string(),
            uploaded,
            uploaded_offset: uploaded,
            upload_speed: 10,
            downloaded: uploaded,
            downloaded_offset: uploaded,
            download_speed: 20,
            last_flags: Some("d".to_string()),
            first_time_seen_ms: 1_000,
            last_time_seen_ms,
            peer_geoip: None,
        }
    }

    #[test]
    fn peer_recording_filter_matches_upstream() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        let module = PeerRecordingServiceModule::new(s.clone(), PeerRecordingSettings::default());

        // 客户端名与 peer-id 都是空白但非握手中 -> 保留
        let mut blank = peer("1.2.3.4", 51413, "");
        blank.client_name = Some("   ".to_string());
        blank.peer_id = Some(String::new());
        // 客户端名与 peer-id 为空、非握手中 -> 保留
        let mut no_client = peer("1.2.3.5", 51413, "d");
        no_client.client_name = None;
        no_client.peer_id = None;
        // 客户端名与 peer-id 为空 + 握手中 -> 丢弃
        let mut dropped = handshaking_peer("1.2.3.6", 51413);
        dropped.client_name = Some(String::new());
        dropped.peer_id = None;
        // 客户端名与 peer-id 为空但握手中为假（有速率）-> 保留
        let mut kept_by_traffic = handshaking_peer("1.2.3.7", 51413);
        kept_by_traffic.up_speed = 10;

        module.on_peers_retrieved(
            "qb",
            &torrent(),
            &[blank, no_client, dropped, kept_by_traffic],
            now,
        );
        assert_eq!(module.cache_len(), 3);
    }

    #[test]
    fn peer_recording_flush_does_not_double_count() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        let module = PeerRecordingServiceModule::new(s.clone(), PeerRecordingSettings::default());
        module.on_peers_retrieved("qb", &torrent(), &[peer("1.2.3.4", 51413, "d u")], now);
        module.flush();
        let rows = s.peer_records();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.address, "1.2.3.4");
        assert_eq!(row.port, 51413);
        assert_eq!(row.torrent_id, torrent().hash);
        assert_eq!(row.peer_id, "-qB4600-", "peer-id 截断到 8 字符");
        assert_eq!(row.client_name, "qBittorrent 4.6.0");
        assert_eq!(row.uploaded, 8192);
        assert_eq!(row.uploaded_offset, 8192);
        assert_eq!(row.upload_speed, 2048);
        assert_eq!(row.downloaded, 4096);
        assert_eq!(row.downloaded_offset, 4096);
        assert_eq!(row.download_speed, 1024);
        assert_eq!(row.last_flags, Some("d u".to_string()));
        assert_eq!(row.first_time_seen_ms, now);
        assert_eq!(row.last_time_seen_ms, now);

        // 同一时间戳再次 flush：last_time_seen 不早于旧值 -> 按 delta 累加，结果不变
        module.on_peers_retrieved("qb", &torrent(), &[peer("1.2.3.4", 51413, "d u")], now);
        module.flush();
        let rows = s.peer_records();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].uploaded, 8192, "重复 flush 不会重复计数");
        assert_eq!(rows[0].downloaded, 4096);
        assert_eq!(rows[0].first_time_seen_ms, now, "first_time_seen 永不更新");

        // 上报值增长时按增量累加
        let mut later = peer("1.2.3.4", 51413, "d u");
        later.uploaded = 10_000;
        later.downloaded = 5_000;
        module.on_peers_retrieved("qb", &torrent(), &[later], now + 60_000);
        module.flush();
        let rows = s.peer_records();
        assert_eq!(rows[0].uploaded, 8192 + (10_000 - 8192));
        assert_eq!(rows[0].downloaded, 4096 + (5_000 - 4096));
        assert_eq!(rows[0].uploaded_offset, 10_000);
        assert_eq!(rows[0].last_time_seen_ms, now + 60_000);
    }

    #[test]
    fn peer_recording_snapshot_equality_matches_lombok_equals() {
        let snapshot =
            PeerRecordCachingEntire::new(1_000, "qb", &torrent(), &peer("1.2.3.4", 51413, "d"));
        assert!(snapshot.same_as(&snapshot.clone()));

        let mut dirty_changed = snapshot.clone();
        dirty_changed.dirty = false;
        assert!(!snapshot.same_as(&dirty_changed));

        let mut time_changed = snapshot.clone();
        time_changed.timestamp_ms = 2_000;
        assert!(!snapshot.same_as(&time_changed));

        let mut peer_changed = snapshot.clone();
        peer_changed.peer.uploaded += 1;
        assert!(!snapshot.same_as(&peer_changed));

        let mut torrent_changed = snapshot.clone();
        torrent_changed.torrent.progress = 0.9;
        assert!(!snapshot.same_as(&torrent_changed));
    }

    #[test]
    fn peer_record_upsert_case_expression_matches_sql() {
        let existing = peer_record_row(2_000, "1.2.3.4", 1_000);

        // 上报时间更旧：整行保持旧值
        let mut stale = existing.clone();
        stale.last_time_seen_ms = 1_000;
        stale.uploaded = 5_000;
        stale.peer_id = "new".to_string();
        let merged = apply_peer_record_upsert(&existing, &stale);
        assert_eq!(merged.uploaded, 1_000);
        assert_eq!(merged.uploaded_offset, 1_000);
        assert_eq!(merged.peer_id, "-qB4600-");
        assert_eq!(merged.last_time_seen_ms, 2_000);

        // 增量 >= 0：uploaded = 旧值 + (本次上报 - 旧偏移量)
        let mut advanced = existing.clone();
        advanced.last_time_seen_ms = 3_000;
        advanced.uploaded = 1_500;
        let merged = apply_peer_record_upsert(&existing, &advanced);
        assert_eq!(merged.uploaded, 1_500);
        assert_eq!(merged.uploaded_offset, 1_500);
        assert_eq!(merged.last_time_seen_ms, 3_000);

        // 增量 < 0（对方计数被重置）：uploaded = 旧值 + 本次上报值
        let mut reset = existing.clone();
        reset.last_time_seen_ms = 3_000;
        reset.uploaded = 200;
        let merged = apply_peer_record_upsert(&existing, &reset);
        assert_eq!(merged.uploaded, 1_200);
        assert_eq!(merged.uploaded_offset, 200);

        // first_time_seen / peer_geoip 永不更新
        let mut incoming = existing.clone();
        incoming.last_time_seen_ms = 3_000;
        incoming.first_time_seen_ms = 9_999;
        incoming.peer_geoip = Some("{\"country\":\"CN\"}".to_string());
        let merged = apply_peer_record_upsert(&existing, &incoming);
        assert_eq!(merged.first_time_seen_ms, 1_000);
        assert_eq!(merged.peer_geoip, None);
    }

    #[test]
    fn peer_recording_retention_boundary_is_strict() {
        let s = sink();
        let now = 10_000i64;
        let module = PeerRecordingServiceModule::new(
            s.clone(),
            PeerRecordingSettings {
                data_retention_time_ms: 1_000,
                data_cleanup_interval_ms: 604_800_000,
                data_flush_interval_ms: 900_000,
            },
        );
        // 对齐 `lt(last_time_seen, before)`：恰好等于边界保留，早 1ms 删除
        s.upsert_peer_record(&peer_record_row(9_000, "1.2.3.4", 100));
        s.upsert_peer_record(&peer_record_row(8_999, "1.2.3.5", 100));
        assert_eq!(module.cleanup(now), Some(1));
        let left = s.peer_records();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].address, "1.2.3.4");

        // `data-retention-time <= 0` 时上游直接 return（不删任何数据）
        let s2 = sink();
        s2.upsert_peer_record(&peer_record_row(1, "1.2.3.4", 100));
        let disabled =
            PeerRecordingServiceModule::new(s2.clone(), PeerRecordingSettings::default());
        assert_eq!(disabled.cleanup(now), None);
        assert_eq!(s2.peer_records().len(), 1);
    }

    // ---------------- swarm-tracking ----------------

    fn swarm_module(s: Arc<InMemoryMonitorSink>) -> SwarmTrackingModule {
        SwarmTrackingModule::new(
            s,
            SwarmTrackingSettings {
                data_flush_interval_ms: 3_600_000,
            },
        )
    }

    #[test]
    fn swarm_tracking_records_deltas_and_speed_maxes() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        let module = swarm_module(s.clone());
        let mut first = peer("1.2.3.4", 51413, "d u");
        first.downloaded = 1_000;
        first.uploaded = 2_000;
        first.dl_speed = 100;
        first.up_speed = 50;
        module.sync_peers("qb", &torrent(), &[first.clone()], now);
        module.flush_all();

        let rows = s.tracked_swarm();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(module.count(), 1);
        assert_eq!(row.ip, "1.2.3.4");
        assert_eq!(row.info_hash, torrent().hash);
        assert_eq!(row.downloader, "qb");
        assert_eq!(row.downloaded, 1_000, "首次 offset 为 0 -> 差值 = 上报值");
        assert_eq!(row.uploaded, 2_000);
        assert_eq!(row.downloaded_offset, 1_000);
        assert_eq!(row.uploaded_offset, 2_000);
        assert_eq!(row.download_speed, 100);
        assert_eq!(row.upload_speed, 50);
        assert_eq!(row.download_speed_max, 100);
        assert_eq!(row.upload_speed_max, 50);
        assert_eq!(row.last_flags, "d u");
        assert_eq!(row.peer_id, "-qB4600-abcdefghijkl");
        assert_eq!(row.client_name, "qBittorrent 4.6.0");
        assert_eq!(row.torrent_size, torrent().total_size);
        assert_eq!(row.torrent_is_private, Some(false));
        assert_eq!(row.first_time_seen_ms, now);
        assert_eq!(row.last_time_seen_ms, now);
        assert!(row.dirty);

        // 第二次采样：计数增长按差值累加；速度只更新历史最大值（download_speed 不更新）
        let mut second = first.clone();
        second.downloaded = 1_500;
        second.uploaded = 2_600;
        second.dl_speed = 10;
        second.up_speed = 500;
        module.sync_peers("qb", &torrent(), &[second.clone()], now + 60_000);
        module.flush_all();
        let rows = s.tracked_swarm();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.downloaded, 1_500);
        assert_eq!(row.uploaded, 2_600);
        assert_eq!(row.downloaded_offset, 1_500);
        assert_eq!(row.uploaded_offset, 2_600);
        assert_eq!(row.download_speed_max, 100);
        assert_eq!(row.upload_speed_max, 500);
        assert_eq!(row.download_speed, 100, "上游只更新 max，不更新当前速度");
        assert_eq!(row.last_time_seen_ms, now + 60_000);
        assert_eq!(row.first_time_seen_ms, now);

        // 计数被重置（新值 < offset）：按整值累加
        let mut reset = second.clone();
        reset.downloaded = 50;
        reset.uploaded = 10;
        module.sync_peers("qb", &torrent(), &[reset], now + 120_000);
        module.flush_all();
        let row = &s.tracked_swarm()[0];
        assert_eq!(row.downloaded, 1_550);
        assert_eq!(row.uploaded, 2_610);
    }

    #[test]
    fn swarm_tracking_skips_handshaking_peers() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        let module = swarm_module(s.clone());
        module.sync_peers(
            "qb",
            &torrent(),
            &[
                handshaking_peer("1.2.3.9", 51413),
                peer("1.2.3.8", 51413, "d"),
            ],
            now,
        );
        assert_eq!(module.cache_len(), 1);
        module.flush_all();
        let rows = s.tracked_swarm();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ip, "1.2.3.8");
    }

    #[test]
    fn swarm_tracking_reuses_existing_row_from_sink() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        let module = swarm_module(s.clone());
        let mut base = peer("1.2.3.4", 51413, "d u");
        base.downloaded = 800;
        base.uploaded = 1_000;
        // 模拟上一次运行留下的行：已累计 700/900，偏移量为当时的上报值 800/1000
        let mut existing = new_tracked_swarm_row("qb", &torrent(), &base, now - 500_000);
        existing.downloaded = 700;
        existing.uploaded = 900;
        existing.downloaded_offset = 800;
        existing.uploaded_offset = 1_000;
        existing.first_time_seen_ms = now - 500_000;
        existing.last_time_seen_ms = now - 500_000;
        s.upsert_tracked_swarm(&existing);

        // cache-miss：从库里取回旧行（offset 来自上一次运行），继续累加
        let mut current = peer("1.2.3.4", 51413, "d u");
        current.downloaded = 1_500;
        current.uploaded = 2_600;
        module.sync_peers("qb", &torrent(), &[current], now);
        module.flush_all();
        let rows = s.tracked_swarm();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.downloaded, 700 + (1_500 - 800));
        assert_eq!(row.uploaded, 900 + (2_600 - 1_000));
        assert_eq!(
            row.first_time_seen_ms,
            now - 500_000,
            "复用旧行的首次出现时间"
        );
        assert_eq!(row.last_time_seen_ms, now);
    }

    #[test]
    fn swarm_tracking_restart_clears_data() {
        let s = sink();
        let now = 1_700_000_000_000i64;
        let module = swarm_module(s.clone());
        module.sync_peers("qb", &torrent(), &[peer("1.2.3.4", 51413, "d")], now);
        module.flush_all();
        assert_eq!(module.count(), 1);

        // 模拟重启：onEnable -> resetTable（临时表清空、缓存清空）
        module.on_enable();
        assert_eq!(module.count(), 0);
        assert!(s.tracked_swarm().is_empty());
        assert_eq!(module.cache_len(), 0);
        // 重启后 flush 不会把旧数据写回
        module.flush_all();
        assert!(s.tracked_swarm().is_empty());

        // 新会话重新开始累积
        module.sync_peers("qb", &torrent(), &[peer("1.2.3.4", 51413, "d")], now + 1);
        module.flush_all();
        assert_eq!(module.count(), 1);
        assert_eq!(s.tracked_swarm()[0].downloaded, 4096);
        assert_eq!(s.tracked_swarm()[0].first_time_seen_ms, now + 1);
    }
}
