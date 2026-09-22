//! ban wave 执行引擎：登录 → 拉 torrent → 信号量并发拉 peers → 规则判定 → 封禁/落库。
//!
//! 阶段顺序对齐上游 `DownloaderServerImpl.banWave()`：
//! 1. 解封到期条目（`removeExpiredBans`）
//! 2. 跑判定流水线并逐个 `banPeer`（写入内存封禁表 + 落库）
//! 3. 下发封禁列表：`removed` 非空 / 关闭增量 / 需要全量重放时走全量 `setPreferences`，
//!    否则走增量 `/transfer/banPeers`；两者都无变化时不触碰下载器。

use crate::push::{AlertLevel, AlertManager};
use pbh_core::banlist::{needs_full_ban_list, BanList, BannedRecord};
use pbh_core::i18n::{Param, TranslationComponent, Translator};
use pbh_core::model::PeerData;
use pbh_core::module::{CheckContext, CheckResult};
use pbh_core::modules::ProgressCheatBlocker;
use pbh_core::pipeline::Decision;
use pbh_core::Pipeline;
use pbh_db::{BanLog, Database};
use pbh_downloader::{BanEntry, Downloader};
use pbh_web::{DownloaderStatus, Metrics};
use std::net::IpAddr;
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

#[derive(Default, Debug, Clone)]
pub struct WaveReport {
    pub online_downloaders: usize,
    pub torrents: usize,
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
}

pub struct WaveEngine {
    /// 下载器列表由 Web 后端热管理（`/api/downloaders` 增删改），与引擎共享同一份实例。
    pub entries: Arc<StdMutex<Vec<DownloaderEntry>>>,
    pub pipeline: Arc<Pipeline>,
    pub db: Arc<Database>,
    pub metrics: Arc<StdMutex<Metrics>>,
    pub statuses: Arc<StdMutex<Vec<DownloaderStatus>>>,
    /// 文案表（内嵌上游 lang + 可选 data/lang 覆盖）
    pub translator: Arc<Translator>,
    /// 落库/日志使用的文案语言
    pub locale: String,
    pub persist_banlist: bool,
    pub max_concurrent: usize,
    /// 告警/推送管理器（对齐上游注入到 `DigestionSession` → `RunCheckModuleOrgan` 的
    /// `AlertManager`）
    pub alert_manager: Arc<AlertManager>,
    /// 监控模块宿主（`active-monitoring` / `peer-analyse-service.*`）。
    ///
    /// 这些模块不参与 peer 判定，只消费「拉取到的 peers」与定时任务，因此挂在 wave 上：
    /// 每拉到一份 peers 就派发一次 `onPeersRetrieved`（对齐上游回调时机）。
    pub monitor: Arc<crate::monitor::MonitorHost>,
}

impl WaveEngine {
    /// 内存封禁表（由 `Pipeline` 持有，供 `auto-range-ban` 读取）。
    fn ban_list(&self) -> &Arc<StdMutex<BanList>> {
        &self.pipeline.ban_list
    }

    /// 执行一轮 ban wave。
    pub async fn run_once(&self, now_ms: i64) -> WaveReport {
        let mut report = WaveReport::default();
        let mut statuses = Vec::new();

        // 1) 解封到期条目
        let removed = self.remove_expired_bans(now_ms);
        report.unbanned = removed.len();

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
                    report.torrents += out.torrents;
                    report.peers += out.peers;
                    report.skipped += out.skipped;
                    report.banned += out.bans.len();
                    statuses.push(out.status);
                    pending.push((idx, out.bans));
                }
                Err(e) => report.errors.push(e),
            }
        }

        // 3) 写入内存封禁表 + 落库（对齐上游：先跑完整个 digestion，再逐个 `banPeer`，
        //    因此本 wave 内的判定互相看不到本轮新封禁，跨下载器也不会互相影响）
        for (idx, bans) in &pending {
            self.record_bans(&entries[*idx], bans, now_ms);
        }

        // 3) 下发：重复封禁（上游 needReApplyBanList）时全部下载器走全量
        //
        // 注意：上游的 `bannedPeers` 是**跨下载器的全局集合**，每个下载器都收到同一份新增列表
        // （`updateDownloader(downloader, !bannedPeers.isEmpty() || !unbannedPeers.isEmpty(), bannedPeers, unbannedPeers, false)`），
        // 因此本 wave 内任一下载器新增的封禁项会下发给**所有**下载器。
        let global_added: Vec<BanEntry> = pending
            .iter()
            .flat_map(|(_, bans)| bans.iter().map(|b| b.entry.clone()))
            .collect();
        let force_full = self.ban_list().lock().map(|b| b.need_reapply()).unwrap_or(false);
        for (idx, _) in &pending {
            self.apply_bans(&entries[*idx], &global_added, removed.len(), force_full)
                .await;
        }
        if force_full {
            if let Ok(mut b) = self.ban_list().lock() {
                b.clear_need_reapply();
            }
        }

        // 4) PCB 状态落库（对齐上游 `batchFlushBackDatabase*`，只写 dirty 实体）
        self.persist_pcb_state();

        *self.statuses.lock().unwrap() = statuses;
        {
            let mut m = self.metrics.lock().unwrap();
            m.downloader_count = report.online_downloaders;
            m.torrent_count = report.torrents;
            m.peer_count = report.peers;
            m.banned_total = self.ban_list().lock().map(|b| b.len()).unwrap_or(report.banned);
        }
        report
    }

    /// 把本轮变更过的 PCB 实体写回数据库。
    fn persist_pcb_state(&self) {
        let Some(pcb) = self.pipeline.module_as::<ProgressCheatBlocker>("progress-cheat-blocker")
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

    /// 取回 PCB 模块（供启动时恢复历史与定期清理）。
    pub fn pcb_module(&self) -> Option<&ProgressCheatBlocker> {
        self.pipeline.module_as::<ProgressCheatBlocker>("progress-cheat-blocker")
    }

    /// 解封到期条目并同步清理数据库。
    fn remove_expired_bans(&self, now_ms: i64) -> Vec<BannedRecord> {
        let expired = match self.ban_list().lock() {
            Ok(mut list) => list.remove_expired(now_ms),
            Err(_) => Vec::new(),
        };
        if expired.is_empty() {
            return expired;
        }
        let ips: Vec<String> = expired.iter().map(|e| e.ip.clone()).collect();
        if self.persist_banlist {
            if let Err(e) = self.db.remove_banned_ips(&ips) {
                warn!("清理到期封禁失败: {e}");
            }
        }
        let normal = expired.iter().filter(|e| !e.ban_for_disconnect).count();
        if normal > 0 {
            info!("解封到期对等体 {normal} 个");
        }
        expired
    }

    /// 渲染可翻译文案；缺失 key 时回退到内部标识。
    fn render_text(
        &self,
        key: &Option<TranslationComponent>,
        fallback: &str,
    ) -> String {
        match key {
            Some(component) => self.translator.render(component, &self.locale),
            None => fallback.to_string(),
        }
    }

    /// 记录新封禁：落库 + 写入内存封禁表。
    fn record_bans(&self, entry: &DownloaderEntry, bans: &[BanRecord], now_ms: i64) {
        for b in bans {
            let unban_at_ms = if b.duration > 0 { now_ms + b.duration } else { 0 };
            let log = BanLog {
                id: None,
                downloader_id: entry.downloader.id().to_string(),
                torrent_hash: b.torrent_hash.clone(),
                torrent_name: b.torrent_name.clone(),
                ip: b.entry.ip.clone(),
                port: b.entry.port as i64,
                peer_id: b.peer_id.clone(),
                client_name: b.client_name.clone(),
                module: b.module.clone(),
                // 与上游一致：落库的是按服务端语言渲染后的文案（作为无 key 时的兜底）
                rule: self.render_text(&b.rule_key, &b.rule),
                reason: self.render_text(&b.reason_key, &b.reason),
                // 同时落库结构化 `TranslationComponent`（JSON），供 API 按请求 locale 重新本地化
                rule_key: b.rule_key.as_ref().map(|c| serde_json::to_string(c).unwrap_or_default()),
                reason_key: b.reason_key.as_ref().map(|c| serde_json::to_string(c).unwrap_or_default()),
                ban_duration: b.duration,
                created_at: now_ms,
            };
            if let Err(e) = self.db.insert_ban_log(&log) {
                warn!("insert ban log failed: {e}");
            }
            if self.persist_banlist {
                if let Err(e) = self.db.upsert_banned_ip(&b.entry.ip, &b.module, b.duration) {
                    warn!("upsert banned ip failed: {e}");
                }
            }
            let duplicate = match self.ban_list().lock() {
                Ok(mut list) => list.add(&b.entry.ip, unban_at_ms, &b.module, b.ban_for_disconnect),
                Err(_) => false,
            };
            if duplicate {
                warn!("对等体 {} 已在封禁表中，下一轮将全量重放封禁列表", b.entry.ip);
            }
        }
    }

    /// 下发封禁列表（增量或全量）。本轮既无新增也无解封时不请求下载器。
    ///
    /// `added` 为**本轮全局新增**（跨下载器），对齐上游传给每个下载器的 `bannedPeers`。
    async fn apply_bans(
        &self,
        entry: &DownloaderEntry,
        added: &[BanEntry],
        removed_count: usize,
        force_full: bool,
    ) {
        if added.is_empty() && removed_count == 0 {
            return;
        }
        let dl = entry.downloader.clone();
        let full = needs_full_ban_list(removed_count, entry.increment_ban, force_full);
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
        let login = dl.login().await.map_err(|e| format!("{} login error: {e}", dl.id()))?;
        let mut status = DownloaderStatus {
            id: dl.id().to_string(),
            name: dl.name().to_string(),
            kind: dl.downloader_type().to_string(),
            online: login.success,
            version: login.version.clone(),
        };
        if !login.success {
            status.online = false;
            return Ok(DownloaderOutput {
                status,
                torrents: 0,
                peers: 0,
                skipped: 0,
                bans: Vec::new(),
            });
        }

        let torrents = dl.fetch_torrents().await.map_err(|e| format!("{} torrents: {e}", dl.id()))?;
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
                let ctx = CheckContext { now_ms, features };
                let mut out = TorrentOutput { peer_count: peers.len(), ..Default::default() };
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
                agg.torrents += 1;
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
        vec![
            Param::Text(downloader_id.to_string()),
            Param::Text(address),
        ],
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
            flags.is_from_incoming() || !flags.outgoing_connection() || flags.from_dht || flags.from_pex
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
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
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
