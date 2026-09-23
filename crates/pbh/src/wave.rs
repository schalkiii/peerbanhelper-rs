//! ban wave 执行引擎：登录 → 拉 torrent → 信号量并发拉 peers → 规则判定 → 封禁/落库。
//!
//! 阶段顺序对齐上游 `DownloaderServerImpl.banWave()`：
//! 1. 解封到期条目（`removeExpiredBans`）
//! 2. 跑判定流水线并逐个 `banPeer`（写入内存封禁表 + 落库）
//! 3. 下发封禁列表：`removed` 非空 / 关闭增量 / 需要全量重放时走全量 `setPreferences`，
//!    否则走增量 `/transfer/banPeers`；两者都无变化时不触碰下载器。

use crate::push::{AlertLevel, AlertManager};
use pbh_core::banlist::{
    needs_full_ban_list, random_id, BanList, BanMetadata, BannedPeer, BannedPeerAddress,
    BannedRecord, BannedTorrent, DownloaderBasicInfo,
};
use pbh_core::geoip::GeoIpProvider;
use pbh_core::i18n::{Param, TranslationComponent};
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::module::{CheckContext, CheckResult};
use pbh_core::modules::{IdleConnectionDosProtection, ProgressCheatBlocker};
use pbh_core::pipeline::Decision;
use pbh_core::Pipeline;
use pbh_db::{peer_geoip_json, Database, HistoryRecord};
use pbh_downloader::{BanEntry, Downloader, LoginResult, LoginStatus};
use pbh_web::{DownloaderStatus, Metrics};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicI64, AtomicU32, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::Semaphore;
use tracing::{info, warn};

/// 单个下载器的运行配置
#[derive(Clone)]
pub struct DownloaderEntry {
    pub downloader: Arc<dyn Downloader>,
    pub increment_ban: bool,
}

/// 上游 `Lang.BAN_PEER` 首参：`PeerAddress` 的 Lombok `@Data` `toString()` 全字段 dump。
///
/// 字段顺序与上游声明一致；NAT/Teredo 翻译字段在本移植默认（`auto-stun.enabled=false`，
/// 翻译恒直通）时恒为 `null` / `0` / `false`——与上游同一场景逐字一致。
fn upstream_peer_address_dump(entry: &BanEntry) -> String {
    format!(
        "PeerAddress(downloaderRawIp={}, downloaderRawPort={}, teredoClientIp=null, \
         teredoClientUdpPort=0, nattedClientIp=null, nattedClientPort=0, ip={}, address={}, \
         port={}, natTranslated=false, teredoTranslated=false)",
        entry.raw_ip, entry.port, entry.ip, entry.ip, entry.port
    )
}

/// Java `Double.toString` 兼容格式（日志逐字对齐用）：
/// 整数值补 `.0`（`0.0` 而非 `0`）；`< 1e-3` 或 `>= 1e7` 走 `1.0E-4` 风格科学计数法。
fn java_double_to_string(v: f64) -> String {
    if !v.is_finite() {
        return v.to_string();
    }
    let abs = v.abs();
    if abs != 0.0 && (abs < 1e-3 || abs >= 1e7) {
        let s = format!("{v:e}");
        if let Some((mant, exp)) = s.split_once('e') {
            let mant = if mant.contains('.') {
                mant.to_string()
            } else {
                format!("{mant}.0")
            };
            return format!("{mant}E{exp}");
        }
        return s;
    }
    if v.fract() == 0.0 {
        format!("{v:.1}")
    } else {
        format!("{v}")
    }
}

/// 登录闸门，对齐上游 `AbstractDownloader.login()` 的失败退避。
///
/// 上游在连续 `failedLoginAttempts >= 15` 次登录后把 `nextLoginTry` 推到
/// `now + 30min`，此后 `login()` **立即返回、完全不发网络请求**。缺了这层之后，
/// 每个不可达的下载器都会让每一轮 ban wave 白付一次连接超时代价
/// （实机实测：只挂一个连不上的下载器时 2003ms/轮，而 Java 侧同一场景仅 87ms/轮）。
#[derive(Clone, Debug, Default)]
pub struct LoginGate {
    failed_attempts: Arc<AtomicU32>,
    next_login_try_ms: Arc<AtomicI64>,
}

impl LoginGate {
    /// 上游 `failedLoginAttempts >= 15`
    pub const MAX_ATTEMPTS: u32 = 15;
    /// 上游 `1000 * 60 * 30`
    pub const COOLDOWN_MS: i64 = 30 * 60 * 1000;

    /// 冷却期内为 true，此时不应发起任何登录请求。
    pub fn is_cooling(&self, now_ms: i64) -> bool {
        now_ms < self.next_login_try_ms.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub fn attempts(&self) -> u32 {
        self.failed_attempts.load(Ordering::Relaxed)
    }

    fn record_success(&self) {
        self.failed_attempts.store(0, Ordering::Relaxed);
    }

    /// 记录一次失败；达到上限时进入冷却并返回 true。
    fn record_failure(&self, now_ms: i64) -> bool {
        let attempts = self.failed_attempts.fetch_add(1, Ordering::Relaxed) + 1;
        if attempts < Self::MAX_ATTEMPTS {
            return false;
        }
        self.failed_attempts.store(0, Ordering::Relaxed);
        self.next_login_try_ms
            .store(now_ms + Self::COOLDOWN_MS, Ordering::Relaxed);
        true
    }

    pub async fn login(
        &self,
        dl: &Arc<dyn Downloader>,
        now_ms: i64,
    ) -> anyhow::Result<LoginResult> {
        if self.is_cooling(now_ms) {
            let cooling_until = self.next_login_try_ms.load(Ordering::Relaxed);
            // 对齐上游冷却分支：返回 REQUIRE_TAKE_ACTIONS（外层不计数）
            return Ok(LoginResult::require_take_actions(
                format!("too many failed login attempts, retry after {cooling_until}"),
                String::new(),
            ));
        }
        match dl.login().await {
            Ok(result) => {
                if result.success {
                    self.record_success();
                } else if result.status == LoginStatus::IncorrectCredential {
                    // 对齐上游：只有 INCORRECT_CREDENTIAL 递增计数；EXCEPTION /
                    // REQUIRE_TAKE_ACTIONS / NETWORK_ERROR / PAUSED 等均不计数
                    //（上游 qB/BitComet/Deluge/BiglyBT/Aria2 的 login0 内部 catch，
                    //  网络故障恢复即恢复，不产生冷却盲区）
                    self.record_failure(now_ms);
                }
                Ok(result)
            }
            Err(e) => {
                // 对应上游 `login0` 把异常抛到外层（如 Transmission 无 try/catch）→ 计数
                self.record_failure(now_ms);
                Err(e)
            }
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct WaveReport {
    /// 登录成功的下载器数（供状态/指标展示；**不是**上游日志里的 `downloaders` 口径）
    pub online_downloaders: usize,
    /// 至少有一个 peer 进入检查的下载器数（对齐上游 `ProcessingStatistics.downloaders`）
    pub checked_downloaders: usize,
    /// 至少有一个 peer 进入检查的种子数（对齐上游 `ProcessingStatistics.torrents`）
    pub torrents: usize,
    /// 进入检查的 peer 数（对齐上游 `ProcessingStatistics.peers`）
    pub peers: usize,
    pub banned: usize,
    pub unbanned: usize,
    pub skipped: usize,
    pub errors: Vec<String>,
}

struct BanRecord {
    entry: BanEntry,
    module: String,
    rule: String,
    reason: String,
    /// 上游对应的可翻译规则名/原因（用于落库与展示）
    rule_key: Option<TranslationComponent>,
    reason_key: Option<TranslationComponent>,
    duration: i64,
    ban_for_disconnect: bool,
    peer_id: String,
    client_name: String,
    torrent_hash: String,
    torrent_name: String,
    // `history` 表落库所需的 peer / torrent 观测（对齐 `PersistMetrics.recordPeerBan`）
    peer_uploaded: i64,
    peer_downloaded: i64,
    peer_progress: f64,
    /// 对方 torrent 进度（`downloader_progress` 列）
    torrent_progress: f64,
    torrent_size: i64,
    /// 下载器口径的已完成字节数（`TorrentWrapper.completedSize`）。
    ///
    /// 上游取 `torrent.getCompletedSize()`（qB = `piece_size*pieces_have`，否则 -1；
    /// Transmission = `sizeWhenDone*percentDone`），**不是** `size × progress`；
    /// 多数字适配器设置了 `completed_override`，直接用进度乘总量会算出不同的值。
    torrent_completed_size: i64,
    torrent_is_private: Option<bool>,
    flags: Option<String>,
    structured_data: serde_json::Value,
    /// 上游 `CheckResult.moduleContext` 的 Java 类全名（落 `history.module_name` 与 BTN 上报）
    module_class: String,
    /// peer 瞬时速率（`BanMetadata.peer.downloadSpeed` / `uploadSpeed`）
    peer_dl_speed: i64,
    peer_up_speed: i64,
    /// torrent 瞬时速率（`BanMetadata.torrent.rtDownloadSpeed` / `rtUploadSpeed`）
    torrent_dl_speed: i64,
    torrent_up_speed: i64,
}

pub struct WaveEngine {
    /// 下载器列表由 Web 后端热管理（`/api/downloaders` 增删改），与引擎共享同一份实例。
    pub entries: Arc<StdMutex<Vec<DownloaderEntry>>>,
    pub pipeline: Arc<Pipeline>,
    pub db: Arc<Database>,
    pub metrics: Arc<StdMutex<Metrics>>,
    pub statuses: Arc<StdMutex<Vec<DownloaderStatus>>>,
    pub persist_banlist: bool,
    /// 演练模式（`--dry-run`）：照常判定与落库，但不向下载器下发封禁/解封/限速。
    pub dry_run: bool,
    pub max_concurrent: usize,
    /// 告警/推送管理器（对齐上游注入到 `DigestionSession` → `RunCheckModuleOrgan` 的
    /// `AlertManager`）
    pub alert_manager: Arc<AlertManager>,
    /// 监控模块宿主（`active-monitoring` / `peer-analyse-service.*`）。
    ///
    /// 这些模块不参与 peer 判定，只消费「拉取到的 peers」与定时任务，因此挂在 wave 上：
    /// 每拉到一份 peers 就派发一次 `onPeersRetrieved`（对齐上游回调时机）。
    pub monitor: Arc<crate::monitor::MonitorHost>,
    /// IP 库（`history.peer_geoip` 落库用；缺省 = 未装 IP 库，落 NULL）。
    ///
    /// 与 [`crate::monitor::MonitorHost`] 的落库 sink 共用同一份 provider（对齐上游
    /// `PersistMetrics` 注入的 `IPDBManager`）。
    pub geo: Option<Arc<dyn GeoIpProvider>>,
    /// 下载器 ID → 登录闸门（对齐上游挂在 `AbstractDownloader` 上的失败计数与冷却）。
    ///
    /// 与 Web 后端共享：下载器更新/删除时由后端移除对应条目（对齐上游
    /// `unregisterDownloader` 重建实例使计数清零的语义）。
    pub login_gates: Arc<StdMutex<HashMap<String, LoginGate>>>,
    /// BTN 遗留协议 live peers 快照（下载器 ID → 本轮观测到的 peers）；
    /// 由 wave 每轮写入、`btn_legacy::LegacyAwareSubmitSource` 消费
    /// （对齐上游 `DownloaderServer` 的 livePeers 数据源）。
    pub live_peers: crate::btn_legacy::LivePeerMap,
}

impl WaveEngine {
    /// 内存封禁表（由 `Pipeline` 持有，供 `auto-range-ban` 读取）。
    fn ban_list(&self) -> &Arc<StdMutex<BanList>> {
        &self.pipeline.ban_list
    }

    /// 执行一轮 ban wave。
    pub async fn run_once(&self, now_ms: i64) -> WaveReport {
        // BTN 遗留协议 live peers：对齐上游 `endSession` 的整表替换——每轮开始清空全表，
        // 由各下载器任务重新写入；登录失败/已删除的下载器本轮不产生任何行（旧数据随之消失）。
        {
            let mut map = self.live_peers.lock().unwrap_or_else(|e| e.into_inner());
            map.clear();
        }
        let mut report = WaveReport::default();
        let mut statuses = Vec::new();

        // 1) 解封到期条目
        let removed = self.remove_expired_bans(now_ms);
        report.unbanned = removed.len();
        // 解封后同步清理 PCB 历史（对齐上游 `@Subscribe onPeerUnBan` →
        // `pcbAddressDao.deleteEntry`）：否则解封后 peer 重连会沿用旧的累计上传量，
        // 下一次判定不再获得宽限窗口。
        self.on_peers_unbanned(&removed);

        // 判重基线：对齐上游在写入任何封禁**之前**取的 `banList.copyKeySet()` 快照，
        // 使「同一 wave 内同一 IP 被多个 torrent / 多个下载器命中」不算重复封禁。
        let ban_baseline = self
            .ban_list()
            .lock()
            .map(|list| list.key_snapshot())
            .unwrap_or_default();

        // 2) 全部下载器判定（此时不触碰下载器，也不写封禁表）
        // 先取快照并释放锁：判定/下发均为异步 I/O，持锁跨 await 会阻塞下载器热管理
        let mut pending: Vec<(usize, Vec<BanRecord>)> = Vec::new();
        let entries: Vec<DownloaderEntry> = match self.entries.lock() {
            Ok(entries) => entries.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        for (idx, entry) in entries.iter().enumerate() {
            match self.run_downloader(entry, now_ms).await {
                Ok(out) => {
                    if out.status.online {
                        report.online_downloaders += 1;
                    }
                    // 上游 `ProcessingStatistics` 只统计「产生了 CheckResult」的条目：
                    // 下载器进入计数的前提是至少有一个 peer 通过了判定（登录成功但 0 peer
                    // 的下载器不计入，日志与上游逐字可比）
                    if out.peers > 0 {
                        report.checked_downloaders += 1;
                    }
                    report.torrents += out.torrents;
                    report.peers += out.peers;
                    report.skipped += out.skipped;
                    report.banned += out.bans.len();
                    statuses.push(out.status);
                    pending.push((idx, out.bans));
                }
                Err(e) => {
                    // 上游 `TorrentsFetchOrgan` 抛异常时该下载器的 lastStatus 仍可被 Web 读到，
                    // 这里补一条离线状态，避免下载器从 `/api/downloaders` 列表里消失
                    statuses.push(DownloaderStatus {
                        id: entry.downloader.id().to_string(),
                        name: entry.downloader.name().to_string(),
                        kind: entry.downloader.downloader_type().to_string(),
                        online: false,
                        version: String::new(),
                    });
                    report.errors.push(e);
                }
            }
        }

        // 3) 写入内存封禁表 + 落库（对齐上游：先跑完整个 digestion，再逐个 `banPeer`，
        //    因此本 wave 内的判定互相看不到本轮新封禁，跨下载器也不会互相影响）
        for (idx, bans) in &pending {
            self.record_bans(&entries[*idx], bans, &ban_baseline, now_ms);
        }

        // 3) 下发：本轮有新增封禁或解封时，对**全部**下载器下发（对齐上游
        //    `updateDownloader(downloader, !bannedPeers.isEmpty() || !unbannedPeers.isEmpty(), …)`
        //    的 `downloaderManager.stream()` 全量遍历——判定阶段登录失败的下载器会在
        //    下发阶段重新登录，有机会补上封禁列表）；重复封禁（needReApplyBanList）走全量。
        //
        // 注意：上游的 `bannedPeers` 是**跨下载器的全局集合**，每个下载器都收到同一份新增列表，
        // 因此本 wave 内任一下载器新增的封禁项会下发给**所有**下载器。
        let global_added: Vec<BanEntry> = pending
            .iter()
            .flat_map(|(_, bans)| bans.iter().map(|b| b.entry.clone()))
            .collect();
        let force_full = self
            .ban_list()
            .lock()
            .map(|b| b.need_reapply())
            .unwrap_or(false);
        for entry in &entries {
            self.apply_bans(entry, &global_added, removed.len(), force_full, now_ms)
                .await;
        }
        if force_full {
            if let Ok(mut b) = self.ban_list().lock() {
                b.clear_need_reapply();
            }
        }

        // 4) PCB 状态落库（对齐上游 `batchFlushBackDatabase*`，只写 dirty 实体）
        self.persist_pcb_state();

        *self.statuses.lock().unwrap_or_else(|e| e.into_inner()) = statuses;
        {
            let mut m = self.metrics.lock().unwrap_or_else(|e| e.into_inner());
            m.downloader_count = report.online_downloaders;
            m.torrent_count = report.torrents;
            m.peer_count = report.peers;
            m.banned_total = self
                .ban_list()
                .lock()
                .map(|b| b.len())
                .unwrap_or(report.banned);
        }
        report
    }

    /// 把本轮变更过的 PCB 实体写回数据库。
    fn persist_pcb_state(&self) {
        let Some(pcb) = self
            .pipeline
            .module_as::<ProgressCheatBlocker>("progress-cheat-blocker")
        else {
            return;
        };
        let rows = pcb.flush_dirty();
        if rows.is_empty() {
            return;
        }
        if let Err(e) = self.db.upsert_pcb_rows(&rows) {
            warn!("PCB 状态落库失败: {e}");
        }
    }

    /// 仅在「需要全量重放」时执行一次下发（暂停/手动触发路径用）。
    ///
    /// 返回是否真的下发过。对齐上游 `banWave` 暂停分支里的
    /// `if (needReApplyBanList.get()) reApplyBanListForDownloaders();`。
    pub async fn consume_pending_replay(
        &self,
        entries: &[DownloaderEntry],
        now_ms: i64,
    ) -> bool {
        let force_full = self
            .ban_list()
            .lock()
            .map(|b| b.need_reapply())
            .unwrap_or(false);
        if !force_full {
            return false;
        }
        for entry in entries {
            self.apply_bans(entry, &[], 0, true, now_ms).await;
        }
        if let Ok(mut b) = self.ban_list().lock() {
            b.clear_need_reapply();
        }
        true
    }

    /// 解封后的收尾：清掉 PCB 的逐 IP 历史（内存实体 + `pcb_address` 行）。
    ///
    /// 上游由 `PeerUnbanEvent` 触发 `pcbAddressDao.deleteEntry`，因此解封后 peer 重连会
    /// 重新累计上传增量并重新获得宽限窗口；缺失这一步会让「封禁 → 解封 → 重连」路径
    /// 带着旧状态直接进入判定，下一轮立刻再封。
    fn on_peers_unbanned(&self, removed: &[BannedRecord]) {
        if removed.is_empty() {
            return;
        }
        let Some(pcb) = self.pcb_module() else {
            return;
        };
        for record in removed {
            let torrent_id = record.metadata.torrent.hash.clone();
            if torrent_id.is_empty() {
                continue;
            }
            pcb.on_unban(&record.metadata.downloader.id, &torrent_id, &record.ip);
            if let Err(e) = self
                .db
                .delete_pcb_addr(&record.metadata.downloader.id, &torrent_id, &record.ip)
            {
                warn!("删除 PCB 历史失败 ({}): {e}", record.ip);
            }
        }
    }

    /// 取回 PCB 模块（供启动时恢复历史与定期清理）。
    pub fn pcb_module(&self) -> Option<&ProgressCheatBlocker> {
        self.pipeline
            .module_as::<ProgressCheatBlocker>("progress-cheat-blocker")
    }

    /// 解封到期条目（内存封禁表为准；持久化由 [`WaveEngine::persist_ban_list`] 定时全量保存）。
    fn remove_expired_bans(&self, now_ms: i64) -> Vec<BannedRecord> {
        let expired = match self.ban_list().lock() {
            Ok(mut list) => list.remove_expired(now_ms),
            Err(_) => Vec::new(),
        };
        if expired.is_empty() {
            return expired;
        }
        let normal = expired.iter().filter(|e| !e.ban_for_disconnect).count();
        if normal > 0 {
            info!("解封到期对等体 {normal} 个");
        }
        expired
    }

    /// 查询 IP 库并转成 `peer_geoip` 的 JSON（缺省 = 未装 IP 库，落 NULL）。
    fn query_peer_geoip(&self, ip: &str) -> Option<String> {
        let provider = self.geo.as_ref()?;
        let address = pbh_core::iputil::parse_addr(ip)?;
        provider.query(address).as_ref().map(peer_geoip_json)
    }

    /// 记录新封禁：落库 + 写入内存封禁表。
    ///
    /// `baseline` 为本 wave 开始前的封禁地址快照（对齐上游 `banList.copyKeySet()`）。
    fn record_bans(
        &self,
        entry: &DownloaderEntry,
        bans: &[BanRecord],
        baseline: &std::collections::HashSet<String>,
        now_ms: i64,
    ) {
        for b in bans {
            let unban_at_ms = if b.duration > 0 {
                now_ms + b.duration
            } else {
                0
            };
            // 上游 `BanMetadata`（`banlist.metadata` 列的载体，同时供 Web 展示与遗留上报）
            let rule_component = b
                .rule_key
                .clone()
                .unwrap_or_else(|| TranslationComponent::new(&b.rule));
            let description_component = b
                .reason_key
                .clone()
                .unwrap_or_else(|| TranslationComponent::new(&b.reason));
            let metadata = BanMetadata {
                context: b.module_class.clone(),
                random_id: random_id(),
                ban_at_ms: now_ms,
                unban_at_ms,
                ban_for_disconnect: b.ban_for_disconnect,
                exclude_from_report: b.ban_for_disconnect,
                exclude_from_display: b.ban_for_disconnect,
                rule: rule_component.clone(),
                description: description_component.clone(),
                structured_data: if b.structured_data.is_null() {
                    None
                } else {
                    Some(b.structured_data.clone())
                },
                downloader: DownloaderBasicInfo {
                    id: entry.downloader.id().to_string(),
                    name: entry.downloader.name().to_string(),
                    kind: entry.downloader.downloader_type().to_string(),
                },
                torrent: BannedTorrent {
                    id: b.torrent_hash.clone(),
                    size: b.torrent_size,
                    completed_size: b.torrent_completed_size,
                    name: b.torrent_name.clone(),
                    hash: b.torrent_hash.clone(),
                    private_torrent: b.torrent_is_private.unwrap_or(false),
                    progress: b.torrent_progress,
                    rt_upload_speed: b.torrent_up_speed,
                    rt_download_speed: b.torrent_dl_speed,
                },
                peer: BannedPeer {
                    address: BannedPeerAddress {
                        downloader_raw_ip: b.entry.raw_ip.clone(),
                        downloader_raw_port: b.entry.port,
                        teredo_client_udp_port: 0,
                        natted_client_port: 0,
                        ip: b.entry.ip.clone(),
                        port: b.entry.port,
                        nat_translated: false,
                        teredo_translated: false,
                    },
                    raw_ip: b.entry.raw_ip.clone(),
                    id: b.peer_id.clone(),
                    client_name: b.client_name.clone(),
                    downloaded: b.peer_downloaded,
                    download_speed: b.peer_dl_speed,
                    uploaded: b.peer_uploaded,
                    upload_speed: b.peer_up_speed,
                    progress: b.peer_progress,
                    flags: b.flags.clone(),
                },
                reverse_lookup: "N/A".to_string(),
            };
            // 封禁历史（`PersistMetrics.recordPeerBan`）：`ban-for-disconnect` 不落 history
            if !b.ban_for_disconnect {
                let history = HistoryRecord {
                    ban_at_ms: now_ms,
                    unban_at_ms,
                    ip: b.entry.ip.clone(),
                    port: b.entry.port,
                    peer_id: Some(b.peer_id.clone()),
                    peer_client_name: Some(b.client_name.clone()),
                    peer_uploaded: Some(b.peer_uploaded),
                    peer_downloaded: Some(b.peer_downloaded),
                    peer_progress: b.peer_progress,
                    downloader_progress: b.torrent_progress,
                    torrent: TorrentData {
                        hash: b.torrent_hash.clone(),
                        name: b.torrent_name.clone(),
                        progress: b.torrent_progress,
                        total_size: b.torrent_size,
                        piece_size: 0,
                        pieces_have: 0,
                        completed_override: None,
                        dlspeed: b.torrent_dl_speed,
                        upspeed: b.torrent_up_speed,
                        is_private: b.torrent_is_private,
                    },
                    // `module_name` 落上游 `CheckResult.moduleContext` 的 Java 类全名
                    module_name: b.module_class.clone(),
                    // `rule_name` / `description` 落 `TranslationComponent` 的 JSON（对齐
                    // `TranslationComponentTypeHandler`）；缺 key 时把渲染文本包成组件，
                    // `Translator` 查不到模板会原样返回，与直接落文本等价
                    rule_name: serde_json::to_string(&rule_component).unwrap_or_default(),
                    description: serde_json::to_string(&description_component).unwrap_or_default(),
                    flags: b.flags.clone(),
                    downloader: entry.downloader.id().to_string(),
                    // 模块未设置结构化数据时上游落 NULL（`PersistMetrics`），不是字符串 "null"
                    structured_data: if b.structured_data.is_null() {
                        None
                    } else {
                        Some(b.structured_data.to_string())
                    },
                    peer_geoip: self.query_peer_geoip(&b.entry.ip),
                };
                if let Err(e) = self.db.insert_history(&history) {
                    warn!("insert history failed: {e}");
                }
            }
            let duplicate = match self.ban_list().lock() {
                Ok(mut list) => list.add_record_in_wave(
                    BannedRecord {
                        ip: b.entry.ip.clone(),
                        unban_at_ms,
                        module: b.module.clone(),
                        ban_for_disconnect: b.ban_for_disconnect,
                        metadata,
                    },
                    baseline,
                ),
                Err(poisoned) => {
                    poisoned.into_inner().add_record_in_wave(
                        BannedRecord {
                            ip: b.entry.ip.clone(),
                            unban_at_ms,
                            module: b.module.clone(),
                            ban_for_disconnect: b.ban_for_disconnect,
                            metadata,
                        },
                        baseline,
                    )
                }
            };
            if duplicate {
                // 上游为 `log.error(Lang.DUPLICATE_BAN, ...)`
                warn!("对等体 {} 重复封禁，将全量重放封禁列表", b.entry.ip);
            }
            // 单条封禁日志（上游 `Lang.BAN_PEER`，仅 `action != BAN_FOR_DISCONNECT` 时打印；
            // ban-for-disconnect 静默，对齐 `DownloaderServerImpl` 第 252 行）
            if !b.ban_for_disconnect {
                let line = self.alert_manager.translator().render(
                    &TranslationComponent::with_params(
                        "BAN_PEER",
                        vec![
                            upstream_peer_address_dump(&b.entry).into(),
                            b.peer_id.clone().into(),
                            b.client_name.clone().into(),
                            java_double_to_string(b.peer_progress).into(),
                            b.peer_uploaded.to_string().into(),
                            b.peer_downloaded.to_string().into(),
                            b.torrent_name.clone().into(),
                            description_component.clone().into(),
                        ],
                    ),
                    self.alert_manager.locale(),
                );
                info!("{line}");
            }
        }
    }

    /// 把内存封禁表整表写回 `banlist`（对齐 `DownloaderServerImpl.saveBanList`：
    /// `delete all` + 逐条 `insert` 的 JSON 元数据）。由调度循环按上游间隔调用。
    pub fn persist_ban_list(&self) -> usize {
        if !self.persist_banlist {
            return 0;
        }
        let records = self
            .ban_list()
            .lock()
            .map(|list| list.records_sorted())
            .unwrap_or_default();
        let entries: Vec<(String, String)> = records
            .iter()
            .map(|record| {
                let metadata =
                    serde_json::to_string(&record.metadata).unwrap_or_else(|_| "{}".to_string());
                (record.ip.clone(), metadata)
            })
            .collect();
        match self.db.save_ban_list(&entries) {
            Ok(written) => written,
            Err(e) => {
                warn!("封禁列表落库失败: {e}");
                0
            }
        }
    }

    /// 下发封禁列表（增量或全量），对齐上游 `updateDownloader`：
    ///
    /// 1. 本轮既无新增也无解封**且无需全量重放**时直接返回（`if (!updateBanList) return;`）。
    ///    注意 `force_full` 必须计入该门控：手动封禁/解封（`mark_reapply`）只置位
    ///    `needReApplyBanList`，本轮可能既无新增也无解封，漏掉这一项会让变更永远不下发。
    /// 2. 下发前**重新登录**（每轮第二次 `login()`，经同一 LoginGate 计数）——
    ///    判定阶段登录失败的下载器在此有恢复机会；失败仅记日志并跳过（PAUSED 静默）；
    /// 3. 成功后按需增量/全量下发。dry-run 在登录前短路（不下发任何请求）。
    async fn apply_bans(
        &self,
        entry: &DownloaderEntry,
        added: &[BanEntry],
        removed_count: usize,
        force_full: bool,
        now_ms: i64,
    ) {
        if added.is_empty() && removed_count == 0 && !force_full {
            return;
        }
        let dl = entry.downloader.clone();
        let full = needs_full_ban_list(removed_count, entry.increment_ban, force_full);
        if self.dry_run {
            info!(
                "[dry-run] 跳过向下载器 {} 下发封禁列表（full={full}，新增 {}，解封 {removed_count}）",
                dl.id(),
                added.len()
            );
            return;
        }
        let gate = self
            .login_gates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(dl.id().to_string())
            .or_default()
            .clone();
        let login = match gate.login(&dl, now_ms).await {
            Ok(r) => r,
            Err(e) => {
                warn!("{} 下发阶段登录失败，跳过封禁列表更新: {e}", dl.id());
                return;
            }
        };
        if !login.success {
            if login.status != LoginStatus::Paused {
                warn!(
                    "{} 下发阶段登录失败，跳过封禁列表更新: {}",
                    dl.id(),
                    login.message
                );
            }
            return;
        }
        if full {
            let ips = self
                .ban_list()
                .lock()
                .map(|b| b.keys_sorted())
                .unwrap_or_default();
            if let Err(e) = dl.replace_banned_ips(&ips).await {
                warn!("replace_banned_ips for {} failed: {e}", dl.id());
            }
        } else if let Err(e) = dl.ban_peers(added).await {
            warn!("ban_peers for {} failed: {e}", dl.id());
        }
    }

    async fn run_downloader(
        &self,
        entry: &DownloaderEntry,
        now_ms: i64,
    ) -> Result<DownloaderOutput, String> {
        let dl = entry.downloader.clone();
        let gate = self
            .login_gates
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(dl.id().to_string())
            .or_default()
            .clone();
        if gate.is_cooling(now_ms) {
            // 对齐上游冷却分支：每次 login() 都发布 WARN 告警（push=true，
            // AlertManager 按 identifier 去重；冷却期内持续可见）
            self.alert_manager
                .publish_alert(
                    true,
                    AlertLevel::Warn,
                    &format!("downloader-too-many-failed-attempt-{}", dl.id()),
                    &TranslationComponent::with_params(
                        "DOWNLOADER_ALERT_TOO_MANY_FAILED_ATTEMPT_TITLE",
                        vec![Param::Text(dl.name().to_string())],
                    ),
                    &TranslationComponent::with_params(
                        "DOWNLOADER_ALERT_TOO_MANY_FAILED_ATTEMPT_DESCRIPTION",
                        vec![
                            Param::Text(dl.name().to_string()),
                            Param::Text("ERROR".to_string()),
                            Param::Text(String::new()),
                        ],
                    ),
                )
                .await;
        }
        let login = gate
            .login(&dl, now_ms)
            .await
            .map_err(|e| format!("{} login error: {e}", dl.id()))?;
        let mut status = DownloaderStatus {
            id: dl.id().to_string(),
            name: dl.name().to_string(),
            kind: dl.downloader_type().to_string(),
            online: login.success,
            version: login.version.clone(),
        };
        if !login.success {
            status.online = false;
            // 对齐上游 `DownloaderLoginOrgan`：登录失败的下载器要留下可读状态与一条错误日志
            //（`ERR_CLIENT_LOGIN_FAILURE_SKIP`）；PAUSED 属主动暂停，上游静默。
            if login.status != LoginStatus::Paused {
                warn!(
                    "下载器 {} 登录失败，跳过本轮检查: {}",
                    dl.id(),
                    login.message
                );
            }
            return Ok(DownloaderOutput {
                status,
                torrents: 0,
                peers: 0,
                skipped: 0,
                bans: Vec::new(),
            });
        }

        let torrents = dl
            .fetch_torrents()
            .await
            .map_err(|e| format!("{} torrents: {e}", dl.id()))?;
        let sem = Arc::new(Semaphore::new(self.max_concurrent.max(1)));
        let features = dl.feature_flags();
        let mut joins = Vec::new();

        for torrent in torrents {
            let dl = dl.clone();
            let permit_sem = sem.clone();
            let pipeline = self.pipeline.clone();
            let features = features.clone();
            let alert_manager = self.alert_manager.clone();
            let monitor = self.monitor.clone();
            let live_peers = self.live_peers.clone();
            joins.push(tokio::spawn(async move {
                let _permit = permit_sem.acquire().await.ok();
                let peers = match dl.fetch_peers(&torrent).await {
                    Ok(p) => p,
                    Err(e) => {
                        warn!("fetch peers for {} failed: {e}", torrent.hash);
                        return TorrentOutput::default();
                    }
                };
                // 监控模块的 `onPeersRetrieved`（与判定无关，只做统计/记录）
                monitor.on_peers_retrieved(dl.id(), &torrent, &peers, now_ms);
                // `idle-connection-dos-protection` 的 `onPeersRetrieved`：上游由
                // `DownloaderServerImpl` 逐模块派发，用于剔除已消失的空闲连接
                //（默认关闭，启用后缺失该调用会让跟踪表只增不减）。
                if let Some(idle) = pipeline.module_as::<IdleConnectionDosProtection>(
                    "idle-connection-dos-protection",
                ) {
                    let keys: Vec<(String, u16)> = peers
                        .iter()
                        .map(|p| (p.ip.clone(), p.port))
                        .collect();
                    idle.on_peers_retrieved(&keys);
                }
                // BTN 遗留协议 live peers 快照（对齐上游 `DownloaderServer` 的 livePeers）；
                // 作用域内完成写入，避免 MutexGuard 跨 await（非 Send）
                {
                    let mut map = live_peers.lock().unwrap_or_else(|e| e.into_inner());
                    map.entry(dl.id().to_string())
                        .or_default()
                        .extend(crate::btn_legacy::peer_rows_from(&torrent, &peers));
                }
                let ctx = CheckContext { now_ms, features };
                let mut out = TorrentOutput {
                    peer_count: peers.len(),
                    ..Default::default()
                };
                for peer in peers {
                    match pipeline.evaluate(dl.id(), &torrent, &peer, &ctx) {
                        Decision::None => {}
                        Decision::Skip(result) => {
                            out.skipped += 1;
                            // 对齐 `RunCheckModuleOrgan.checkIfPossibleBadConfig`：命中了
                            // `ignore-peers-from-addresses`（bypass）的 peer 再判定一次
                            //「疑似 NAT 误配」，命中则发布 ERROR 级告警并推送。
                            // 说明：上游整个封禁链路里**只有这一处**告警/推送（封禁本身不推送），
                            // 且 `downloader-nat-setup-error@<id>` 只要曾发布过就不再重复
                            //（`identifierAlertExistsIncludeRead` 去重，等价「按下载器聚合」）。
                            if is_bypass_skip(&result) {
                                publish_bad_nat_setup_alert(&alert_manager, dl.id(), &peer).await;
                            }
                        }
                        Decision::Ban(result) => out.bans.push(BanRecord {
                            entry: BanEntry {
                                ip: peer.ip.clone(),
                                port: peer.port,
                                raw_ip: peer.raw_ip.clone(),
                            },
                            module: result.module.clone(),
                            rule: result.rule.clone(),
                            reason: result.reason.clone(),
                            rule_key: result.rule_key.clone(),
                            reason_key: result.reason_key.clone(),
                            duration: result.ban_duration_ms,
                            ban_for_disconnect: result.action
                                == pbh_core::module::PeerAction::BanForDisconnect,
                            peer_id: peer.peer_id.clone().unwrap_or_default(),
                            client_name: peer.client_name.clone().unwrap_or_default(),
                            torrent_hash: torrent.hash.clone(),
                            torrent_name: torrent.name.clone(),
                            peer_uploaded: peer.uploaded,
                            peer_downloaded: peer.downloaded,
                            peer_progress: peer.progress,
                            torrent_progress: torrent.progress,
                            torrent_size: torrent.total_size,
                            torrent_completed_size: torrent.completed_size(),
                            torrent_is_private: torrent.is_private,
                            flags: peer.flags.clone(),
                            structured_data: result.data.clone(),
                            module_class: pbh_core::module::java_module_class(&result.module)
                                .to_string(),
                            peer_dl_speed: peer.dl_speed,
                            peer_up_speed: peer.up_speed,
                            torrent_dl_speed: torrent.dlspeed,
                            torrent_up_speed: torrent.upspeed,
                        }),
                    }
                }
                out
            }));
        }

        let mut agg = DownloaderOutput {
            status,
            torrents: 0,
            peers: 0,
            skipped: 0,
            bans: Vec::new(),
        };
        for j in joins {
            if let Ok(o) = j.await {
                // 上游 `ProcessingStatistics.torrents` 只统计「有 peer 通过判定」的种子
                if o.peer_count > 0 {
                    agg.torrents += 1;
                }
                agg.peers += o.peer_count;
                agg.skipped += o.skipped;
                agg.bans.extend(o.bans);
            }
        }
        Ok(agg)
    }
}

struct DownloaderOutput {
    status: DownloaderStatus,
    torrents: usize,
    peers: usize,
    skipped: usize,
    bans: Vec<BanRecord>,
}

/// 该 SKIP 是否来自 bypass 分支（`ignore-peers-from-addresses`）。
///
/// 对齐上游 `RunCheckModuleOrgan.checkIfPossibleBadConfig` 返回的 `CheckResult`：
/// 它的 `StructuredData` 带 `type = "ignoredAddresses"`，即本移植 `Pipeline::evaluate`
/// 在 ignore 命中时写下的同一个标记。
fn is_bypass_skip(result: &CheckResult) -> bool {
    result.data.get("type").and_then(|v| v.as_str()) == Some("ignoredAddresses")
}

/// 对齐 `RunCheckModuleOrgan.checkIfPossibleBadConfig` 里的告警分支：
/// peer 落在忽略地址内 + 疑似 NAT 误配 ⇒ 发布 `AlertLevel.ERROR` 告警（带推送）。
///
/// 上游从「每个规则模块 × 每个 peer」反复调用该检查，靠 identifier
/// `downloader-nat-setup-error@<downloaderId>` 的去重把推送**聚合为每下载器一次**
/// （`identifierAlertExistsIncludeRead` 一旦为真即不再发布，`markAlertAsRead` 也不会解除）。
async fn publish_bad_nat_setup_alert(
    alert_manager: &AlertManager,
    downloader_id: &str,
    peer: &PeerData,
) {
    if !peer_has_possible_bad_nat_config(peer) {
        return;
    }
    let identifier = format!("downloader-nat-setup-error@{downloader_id}");
    if alert_manager.identifier_alert_exists_include_read(&identifier) {
        return;
    }
    // 描述里的地址是**未做 IPv4 归一化**的压缩写法
    // （对齐 `peer.getPeerAddress().getAddress().toCompressedString()`）
    let address = pbh_core::iputil::parse_addr(&peer.ip)
        .map(|addr| addr.to_string())
        .unwrap_or_else(|| peer.ip.clone());
    let title = TranslationComponent::new("DOWNLOADER_DOCKER_INCORRECT_NETWORK_DETECTED_TITLE");
    let content = TranslationComponent::with_params(
        "DOWNLOADER_DOCKER_INCORRECT_NETWORK_DETECTED_DESCRIPTION",
        vec![Param::Text(downloader_id.to_string()), Param::Text(address)],
    );
    alert_manager
        .publish_alert(true, AlertLevel::Error, &identifier, &title, &content)
        .await;
}

/// 对齐 `RunCheckModuleOrgan.isPeerHavePossibleBadNatConfig`：
/// 「来自 DHT/PEX/Tracker 或入站连接」+「非握手中」+「网关式地址（`.1`/`.0`）」+
/// 「链路本地或未指定地址」⇒ 认为用户搞砸了 NAT 设置（下载器把网关当成 peer 报了回来）。
///
/// 与上游的差异：上游的 `isFromTracker()` 在 libtorrent flags 解析（`PeerFlag.parseLibTorrent`）
/// 中恒为 `false`（该来源无对应字符），本移植的 `PeerFlag` 也没有该字段，故同样不参与判定。
fn peer_has_possible_bad_nat_config(peer: &PeerData) -> bool {
    let from_bad_source = match peer.peer_flag() {
        // 上游 `peer.getFlags() == null` ⇒ 整个条件为真
        None => true,
        Some(flags) => {
            flags.is_from_incoming()
                || !flags.outgoing_connection()
                || flags.from_dht
                || flags.from_pex
        }
    };
    if !from_bad_source || peer.is_handshaking() {
        return false;
    }
    let Some(addr) = pbh_core::iputil::parse_addr(&peer.ip) else {
        return false;
    };
    // 对齐 `if (addr.isIPv4Convertible()) addr = addr.toIPv4();`
    let addr = match addr {
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    };
    let addr_str = addr.to_string();
    (addr_str.ends_with(".1") || addr_str.ends_with(".0"))
        && (is_link_local(addr) || is_unspecified_address(addr))
}

/// 对齐 `inet.ipaddr` 的 `IPAddress.isLocal()`（链路本地：`169.254.0.0/16` / `fe80::/10`）。
fn is_link_local(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// 对齐 `inet.ipaddr` 的 `IPAddress.isAnyLocal()`（未指定地址：`0.0.0.0` / `::`）。
fn is_unspecified_address(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_unspecified(),
        IpAddr::V6(v6) => v6.is_unspecified(),
    }
}

#[derive(Default)]
struct TorrentOutput {
    peer_count: usize,
    skipped: usize,
    bans: Vec<BanRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbh_core::btn_transport::BtnSubmitSource;
    use pbh_core::modules::{InMemoryMonitorSink, MonitorSink};
    use pbh_db::DbBtnSubmitSource;
    use pbh_downloader::http::{BoxFuture, HttpFetcher, HttpRequest, HttpResponse};
    use pbh_downloader::{DownloaderStatistics, LoginResult};

    /// 测试用空 HTTP 客户端（测试不配置推送渠道，实际不会被调用）。
    struct NoopFetcher;

    impl HttpFetcher for NoopFetcher {
        fn execute<'a>(
            &'a self,
            _request: HttpRequest,
        ) -> BoxFuture<'a, anyhow::Result<HttpResponse>> {
            Box::pin(async { Ok(HttpResponse::new(200, "{}".to_string())) })
        }
    }

    /// 测试用下载器：`record_bans` 只用到 `id()`，其余能力不会被触发。
    struct StubDownloader;

    impl Downloader for StubDownloader {
        fn id(&self) -> &str {
            "qb"
        }
        fn name(&self) -> &str {
            "qBittorrent"
        }
        fn downloader_type(&self) -> &'static str {
            "qbittorrent"
        }
        fn feature_flags(&self) -> Vec<String> {
            Vec::new()
        }
        fn login<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<LoginResult>> {
            Box::pin(async { Ok(LoginResult::success(String::new(), "5.0.0")) })
        }
        fn fetch_torrents<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Vec<TorrentData>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn fetch_peers<'a>(
            &'a self,
            _torrent: &'a TorrentData,
        ) -> BoxFuture<'a, anyhow::Result<Vec<PeerData>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn ban_peers<'a>(&'a self, _peers: &'a [BanEntry]) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn replace_banned_ips<'a>(
            &'a self,
            _ips: &'a [String],
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn statistics<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<DownloaderStatistics>> {
            Box::pin(async { Ok(DownloaderStatistics::default()) })
        }
        fn get_speed_limiter<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<(i64, i64)>> {
            Box::pin(async { Ok((0, 0)) })
        }
        fn set_speed_limiter<'a>(
            &'a self,
            _upload: i64,
            _download: i64,
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// 对齐上游 `AbstractDownloader.login()`：连续失败达上限后进入冷却，
    /// 冷却期内不再发起登录请求（否则每个不可达下载器每轮都要白付一次连接超时）。
    #[test]
    fn login_gate_cools_down_after_max_attempts() {
        let gate = LoginGate::default();
        let now = 1_700_000_000_000;
        for _ in 0..(LoginGate::MAX_ATTEMPTS - 1) {
            assert!(!gate.record_failure(now));
        }
        assert_eq!(gate.attempts(), LoginGate::MAX_ATTEMPTS - 1);
        assert!(!gate.is_cooling(now), "未达上限时不应冷却");

        assert!(gate.record_failure(now), "达到上限应进入冷却");
        assert!(gate.is_cooling(now));
        assert!(gate.is_cooling(now + LoginGate::COOLDOWN_MS - 1));
        assert!(
            !gate.is_cooling(now + LoginGate::COOLDOWN_MS),
            "冷却到期后应恢复"
        );
    }

    /// 计数口径测试用下载器：可配置种子清单与各种子下的 peer。
    struct CounterMock {
        id: String,
        torrents: Vec<pbh_core::model::TorrentData>,
        peers: HashMap<String, Vec<pbh_core::model::PeerData>>,
    }

    impl Downloader for CounterMock {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            "CounterMock"
        }
        fn downloader_type(&self) -> &'static str {
            "qbittorrent"
        }
        fn feature_flags(&self) -> Vec<String> {
            Vec::new()
        }
        fn login<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<LoginResult>> {
            Box::pin(async { Ok(LoginResult::success(String::new(), "5.0.0")) })
        }
        fn fetch_torrents<'a>(
            &'a self,
        ) -> BoxFuture<'a, anyhow::Result<Vec<pbh_core::model::TorrentData>>> {
            let torrents = self.torrents.clone();
            Box::pin(async move { Ok(torrents) })
        }
        fn fetch_peers<'a>(
            &'a self,
            torrent: &'a pbh_core::model::TorrentData,
        ) -> BoxFuture<'a, anyhow::Result<Vec<pbh_core::model::PeerData>>> {
            let peers = self.peers.get(&torrent.hash).cloned().unwrap_or_default();
            Box::pin(async move { Ok(peers) })
        }
        fn ban_peers<'a>(&'a self, _peers: &'a [BanEntry]) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn replace_banned_ips<'a>(
            &'a self,
            _ips: &'a [String],
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn statistics<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<DownloaderStatistics>> {
            Box::pin(async { Ok(DownloaderStatistics::default()) })
        }
        fn get_speed_limiter<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<(i64, i64)>> {
            Box::pin(async { Ok((0, 0)) })
        }
        fn set_speed_limiter<'a>(
            &'a self,
            _upload: i64,
            _download: i64,
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    #[test]
    fn ban_peer_log_helpers_match_upstream_formatting() {
        // Java Double.toString：整数值补 .0、常规值取最短往返表示
        assert_eq!(java_double_to_string(0.0), "0.0");
        assert_eq!(java_double_to_string(1.0), "1.0");
        assert_eq!(
            java_double_to_string(0.6225200891494751),
            "0.6225200891494751"
        );
        // 科学计数法阈值对齐 Java（< 1e-3 或 >= 1e7）
        assert_eq!(java_double_to_string(1e-4), "1.0E-4");
        assert_eq!(java_double_to_string(12_345_678.0), "1.2345678E7");

        // PeerAddress dump：字段顺序与上游 Lombok @Data toString 一致
        let entry = BanEntry {
            ip: "2001:da8:d800:338:1152:d353:fb4e:6f18".to_string(),
            port: 14331,
            raw_ip: "[2001:da8:d800:338:1152:d353:fb4e:6f18]:14331".to_string(),
        };
        assert_eq!(
            upstream_peer_address_dump(&entry),
            "PeerAddress(downloaderRawIp=[2001:da8:d800:338:1152:d353:fb4e:6f18]:14331, \
             downloaderRawPort=14331, teredoClientIp=null, teredoClientUdpPort=0, \
             nattedClientIp=null, nattedClientPort=0, \
             ip=2001:da8:d800:338:1152:d353:fb4e:6f18, \
             address=2001:da8:d800:338:1152:d353:fb4e:6f18, port=14331, natTranslated=false, \
             teredoTranslated=false)"
        );
    }

    /// 计数对齐上游 `ProcessingStatistics`：只统计「有 peer 通过判定」的下载器/种子。
    #[tokio::test]
    async fn wave_report_counts_follow_upstream_processing_statistics() {
        let db = Arc::new(Database::open_in_memory().unwrap());
        let engine = engine(db);
        let torrent = |hash: &str| pbh_core::model::TorrentData {
            hash: hash.to_string(),
            name: hash.to_string(),
            progress: 1.0,
            total_size: 1024,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: Some(false),
        };
        let peer = pbh_core::model::PeerData {
            client_name: Some("qBittorrent".to_string()),
            peer_id: Some("-qB5230-abcdefghijkl".to_string()),
            dl_speed: 0,
            downloaded: 0,
            up_speed: 0,
            uploaded: 0,
            progress: 0.1,
            flags: Some("U".to_string()),
            ip: "198.51.100.7".to_string(),
            port: 6881,
            raw_ip: "198.51.100.7:6881".to_string(),
            connection: None,
        };
        let mut peers = HashMap::new();
        peers.insert("hash-a".to_string(), vec![peer]);
        // 下载器 1：两个种子，只有一个有 peer
        engine.entries.lock().unwrap().push(DownloaderEntry {
            downloader: Arc::new(CounterMock {
                id: "counter-1".to_string(),
                torrents: vec![torrent("hash-a"), torrent("hash-b")],
                peers,
            }),
            increment_ban: true,
        });
        // 下载器 2：登录成功但没有任何种子/peer（上游不计入 downloaders）
        engine.entries.lock().unwrap().push(DownloaderEntry {
            downloader: Arc::new(CounterMock {
                id: "counter-2".to_string(),
                torrents: Vec::new(),
                peers: HashMap::new(),
            }),
            increment_ban: true,
        });

        let report = engine.run_once(1_700_000_000_000).await;
        assert_eq!(report.online_downloaders, 2, "两个下载器均登录成功");
        assert_eq!(
            report.checked_downloaders, 1,
            "上游口径：0 peer 的下载器不计入 downloaders"
        );
        assert_eq!(report.torrents, 1, "上游口径：0 peer 的种子不计入 torrents");
        assert_eq!(report.peers, 1);
    }

    /// 可配置登录结果的测试下载器（验证 LoginGate 的计数口径）。
    struct GateMock {
        result: StdMutex<LoginResult>,
        fail_with_err: std::sync::atomic::AtomicBool,
    }

    impl Downloader for GateMock {
        fn id(&self) -> &str {
            "gate-mock"
        }
        fn name(&self) -> &str {
            "GateMock"
        }
        fn downloader_type(&self) -> &'static str {
            "qbittorrent"
        }
        fn feature_flags(&self) -> Vec<String> {
            Vec::new()
        }
        fn login<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<LoginResult>> {
            Box::pin(async {
                if self.fail_with_err.load(Ordering::Relaxed) {
                    Err(anyhow::anyhow!("connect timeout"))
                } else {
                    Ok(self
                        .result
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone())
                }
            })
        }
        fn fetch_torrents<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Vec<TorrentData>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn fetch_peers<'a>(
            &'a self,
            _torrent: &'a TorrentData,
        ) -> BoxFuture<'a, anyhow::Result<Vec<PeerData>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn ban_peers<'a>(&'a self, _peers: &'a [BanEntry]) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn replace_banned_ips<'a>(
            &'a self,
            _ips: &'a [String],
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
        fn statistics<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<DownloaderStatistics>> {
            Box::pin(async { Ok(DownloaderStatistics::default()) })
        }
        fn get_speed_limiter<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<(i64, i64)>> {
            Box::pin(async { Ok((0, 0)) })
        }
        fn set_speed_limiter<'a>(
            &'a self,
            _upload: i64,
            _download: i64,
        ) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// 对齐上游计数口径：`Ok(INCORRECT_CREDENTIAL)` 与 `Err`（login0 外抛异常，如
    /// Transmission）递增计数；`Ok(EXCEPTION)`（qB/BitComet/Deluge/BiglyBT/Aria2 的
    /// login0 内部 catch）不计数——网络故障恢复即恢复，不产生冷却盲区；成功清零。
    #[tokio::test]
    async fn login_gate_counts_per_upstream_status_semantics() {
        let now = 1_700_000_000_000i64;
        let mock = Arc::new(GateMock {
            result: StdMutex::new(LoginResult::exception("io error")),
            fail_with_err: std::sync::atomic::AtomicBool::new(false),
        });
        let dl: Arc<dyn Downloader> = mock.clone();
        let gate = LoginGate::default();

        // EXCEPTION 状态：重复任意次都不计数、不冷却
        for _ in 0..(LoginGate::MAX_ATTEMPTS * 2) {
            let r = gate.login(&dl, now).await.unwrap();
            assert_eq!(r.status, LoginStatus::Exception);
            assert!(!gate.is_cooling(now));
        }
        assert_eq!(gate.attempts(), 0);

        // INCORRECT_CREDENTIAL：达到上限进入冷却，冷却期内不再发起登录请求
        *mock.result.lock().unwrap() = LoginResult::incorrect_credential("bad credential");
        for _ in 0..(LoginGate::MAX_ATTEMPTS - 1) {
            gate.login(&dl, now).await.unwrap();
        }
        assert_eq!(gate.attempts(), LoginGate::MAX_ATTEMPTS - 1);
        let last = gate.login(&dl, now).await.unwrap();
        assert_eq!(last.status, LoginStatus::IncorrectCredential);
        assert!(gate.is_cooling(now), "第 15 次凭据失败应进入冷却");
        // 冷却期内的登录尝试直接返回 REQUIRE_TAKE_ACTIONS（完全不发网络请求）
        let cooled = gate.login(&dl, now).await.unwrap();
        assert_eq!(cooled.status, LoginStatus::RequireTakeActions);

        // Err（login0 外抛异常）：同样计数
        let gate = LoginGate::default();
        mock.fail_with_err.store(true, Ordering::Relaxed);
        for _ in 0..LoginGate::MAX_ATTEMPTS {
            assert!(gate.login(&dl, now).await.is_err());
        }
        assert!(gate.is_cooling(now));

        // 登录成功清零计数
        let gate = LoginGate::default();
        mock.fail_with_err.store(false, Ordering::Relaxed);
        *mock.result.lock().unwrap() = LoginResult::incorrect_credential("bad credential");
        for _ in 0..5 {
            gate.login(&dl, now).await.unwrap();
        }
        *mock.result.lock().unwrap() = LoginResult::success("OK", "5.0.0");
        gate.login(&dl, now).await.unwrap();
        assert_eq!(gate.attempts(), 0, "登录成功应清零失败计数");
    }

    fn engine(db: Arc<Database>) -> WaveEngine {
        let translator = Arc::new(pbh_core::i18n::Translator::embedded());
        let push_manager =
            crate::push::PushManager::from_config(&Default::default(), Arc::new(NoopFetcher));
        let alert_manager = Arc::new(AlertManager::new(
            Arc::new(push_manager),
            translator.clone(),
            "zh_cn",
        ));
        let sink: Arc<dyn MonitorSink> = Arc::new(InMemoryMonitorSink::new());
        let monitor = Arc::new(crate::monitor::MonitorHost::new(
            &pbh_core::config::ProfileConfig::default(),
            sink,
        ));
        WaveEngine {
            entries: Arc::new(StdMutex::new(Vec::new())),
            pipeline: Arc::new(Pipeline::default()),
            db,
            metrics: Arc::new(StdMutex::new(Metrics::default())),
            statuses: Arc::new(StdMutex::new(Vec::new())),
            persist_banlist: false,
            dry_run: false,
            max_concurrent: 1,
            login_gates: Default::default(),
            live_peers: crate::btn_legacy::new_live_peer_map(),
            alert_manager,
            monitor,
            geo: None,
        }
    }

    fn ban(ip: &str, ban_for_disconnect: bool) -> BanRecord {
        BanRecord {
            entry: BanEntry {
                ip: ip.to_string(),
                port: 6881,
                raw_ip: format!("{ip}:6881"),
            },
            module: "peer-analyse-service".to_string(),
            rule: "测试规则".to_string(),
            reason: "测试原因".to_string(),
            rule_key: Some(TranslationComponent::new("BTN_NETWORK_NOT_ENABLED")),
            reason_key: None,
            duration: 3_600_000,
            ban_for_disconnect,
            peer_id: "peer-1".to_string(),
            client_name: "qBittorrent".to_string(),
            torrent_hash: "hash-a".to_string(),
            torrent_name: "示例种子".to_string(),
            peer_uploaded: 1000,
            peer_downloaded: 2000,
            peer_progress: 0.25,
            torrent_progress: 0.75,
            torrent_size: 2048,
            torrent_completed_size: 1536,
            torrent_is_private: Some(true),
            flags: Some("U".to_string()),
            structured_data: serde_json::json!({ "type": "test" }),
            module_class: pbh_core::module::java_module_class("peer-analyse-service").to_string(),
            peer_dl_speed: 128,
            peer_up_speed: 256,
            torrent_dl_speed: 512,
            torrent_up_speed: 1024,
        }
    }

    /// `record_bans` 写 `history`（对齐 `PersistMetrics.recordPeerBan`），
    /// `ban-for-disconnect` 不落 history。
    #[test]
    fn record_bans_writes_history_and_skips_disconnect() {
        let db = Arc::new(Database::open_in_memory().expect("内存库"));
        let engine = engine(db.clone());
        let entry = DownloaderEntry {
            downloader: Arc::new(StubDownloader),
            increment_ban: true,
        };

        let baseline = std::collections::HashSet::new();
        engine.record_bans(
            &entry,
            &[ban("1.1.1.1", false), ban("1.1.1.2", true)],
            &baseline,
            1_700_000_000_000,
        );

        // ban_logs 记录全部封禁；history 只记录非 disconnect
        let source = DbBtnSubmitSource::new(
            db.clone(),
            Arc::new(pbh_core::i18n::Translator::embedded()),
            "zh_cn",
        );
        let rows = source.batch_ban_history(0, 100);
        assert_eq!(rows.len(), 1, "ban-for-disconnect 不落 history");
        let row = &rows[0];
        assert_eq!(row.peer_ip, "1.1.1.1");
        assert_eq!(row.ban_at_ms, 1_700_000_000_000);
        assert_eq!(row.torrent_hash, "hash-a");
        assert!(row.torrent_is_private);
        assert_eq!(row.torrent_size, 2048);
        // 上游映射方向：`fromPeerTraffic ← peerDownloaded`、`toPeerTraffic ← peerUploaded`
        assert_eq!(row.from_peer_traffic, 2000);
        assert_eq!(row.to_peer_traffic, 1000);
        assert_eq!(row.downloader_progress, 0.75);
        // `rule_key` 落 `TranslationComponent` JSON 后按服务端语言渲染；
        // 缺失 key 的 reason 退化为把文本包成组件，渲染结果即原文
        assert!(row.rule.contains("未启用"), "rule={}", row.rule);
        assert_eq!(row.description, "测试原因");
        assert_eq!(row.structured_data.as_deref(), Some(r#"{"type":"test"}"#));
    }
}
