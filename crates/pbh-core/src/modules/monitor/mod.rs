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

mod active;
mod recording;
mod rows;
mod session;
mod sink;
mod tracking;
mod util;

pub use active::*;
pub use recording::*;
pub use rows::*;
pub use session::*;
pub use sink::*;
pub use tracking::*;
pub use util::*;

#[cfg(test)]
mod tests;
