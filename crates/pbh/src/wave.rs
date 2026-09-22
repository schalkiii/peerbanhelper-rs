//! ban wave 执行引擎：登录 → 拉 torrent → 信号量并发拉 peers → 规则判定 → 封禁/落库。
//!
//! 阶段顺序对齐上游 `DownloaderServerImpl.banWave()`：
//! 1. 解封到期条目（`removeExpiredBans`）
//! 2. 跑判定流水线并逐个 `banPeer`（写入内存封禁表 + 落库）
//! 3. 下发封禁列表：`removed` 非空 / 关闭增量 / 需要全量重放时走全量 `setPreferences`，
//!    否则走增量 `/transfer/banPeers`；两者都无变化时不触碰下载器。

use crate::push::{AlertLevel, AlertManager};
use pbh_core::banlist::{needs_full_ban_list, BanList, BannedRecord};
use pbh_core::geoip::GeoIpProvider;
use pbh_core::i18n::{Param, TranslationComponent, Translator};
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::module::{CheckContext, CheckResult};
use pbh_core::modules::ProgressCheatBlocker;
use pbh_core::pipeline::Decision;
use pbh_core::Pipeline;
use pbh_db::{peer_geoip_json, BanLog, Database, HistoryRecord};
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
    // `history` 表落库所需的 peer / torrent 观测（对齐 `PersistMetrics.recordPeerBan`）
    peer_uploaded: i64,
    peer_downloaded: i64,
    peer_progress: f64,
    /// 对方 torrent 进度（`downloader_progress` 列）
    torrent_progress: f64,
    torrent_size: i64,
    torrent_is_private: Option<bool>,
    flags: Option<String>,
    structured_data: serde_json::Value,
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
    /// IP 库（`history.peer_geoip` 落库用；缺省 = 未装 IP 库，落 NULL）。
    ///
    /// 与 [`crate::monitor::MonitorHost`] 的落库 sink 共用同一份 provider（对齐上游
    /// `PersistMetrics` 注入的 `IPDBManager`）。
    pub geo: Option<Arc<dyn GeoIpProvider>>,
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

    /// 查询 IP 库并转成 `peer_geoip` 的 JSON（缺省 = 未装 IP 库，落 NULL）。
    fn query_peer_geoip(&self, ip: &str) -> Option<String> {
        let provider = self.geo.as_ref()?;
        let address = pbh_core::iputil::parse_addr(ip)?;
        provider.query(address).as_ref().map(peer_geoip_json)
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
                        dlspeed: 0,
                        upspeed: 0,
                        is_private: b.torrent_is_private,
                    },
                    module_name: b.module.clone(),
                    // `rule_name` / `description` 落 `TranslationComponent` 的 JSON（对齐上游
                    // `TranslationComponentTypeHandler`）；缺 key 时把渲染文本包成组件，
                    // `Translator` 查不到模板会原样返回，与直接落文本等价
                    rule_name: rule_component_json(&b.rule_key, &b.rule),
                    description: rule_component_json(&b.reason_key, &b.reason),
                    flags: b.flags.clone(),
                    downloader: entry.downloader.id().to_string(),
                    structured_data: Some(b.structured_data.to_string()),
                    peer_geoip: self.query_peer_geoip(&b.entry.ip),
                };
                if let Err(e) = self.db.insert_history(&history) {
                    warn!("insert history failed: {e}");
                }
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
                            peer_uploaded: peer.uploaded,
                            peer_downloaded: peer.downloaded,
                            peer_progress: peer.progress,
                            torrent_progress: torrent.progress,
                            torrent_size: torrent.total_size,
                            torrent_is_private: torrent.is_private,
                            flags: peer.flags.clone(),
                            structured_data: result.data.clone(),
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

/// `history.rule_name` / `description` 的落库值：`TranslationComponent` 的 JSON
/// （对齐上游 `TranslationComponentTypeHandler`）。
///
/// 缺 key 时把渲染文本包成组件：`Translator` 查不到模板会原样返回 key，
/// 与直接落文本等价，且保证列内容始终是合法 JSON。
fn rule_component_json(key: &Option<TranslationComponent>, fallback: &str) -> String {
    let component = match key {
        Some(component) => component.clone(),
        None => TranslationComponent::new(fallback),
    };
    serde_json::to_string(&component).unwrap_or_else(|_| fallback.to_string())
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
            Box::pin(async {
                Ok(LoginResult {
                    success: true,
                    message: String::new(),
                    version: "5.0.0".to_string(),
                })
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

    fn engine(db: Arc<Database>) -> WaveEngine {
        let translator = Arc::new(Translator::embedded());
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
            translator,
            locale: "zh_cn".to_string(),
            persist_banlist: false,
            max_concurrent: 1,
            alert_manager,
            monitor,
            geo: None,
        }
    }

    fn ban(ip: &str, ban_for_disconnect: bool) -> BanRecord {
        BanRecord {
            entry: BanEntry { ip: ip.to_string(), port: 6881, raw_ip: format!("{ip}:6881") },
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
            torrent_is_private: Some(true),
            flags: Some("U".to_string()),
            structured_data: serde_json::json!({ "type": "test" }),
        }
    }

    /// `record_bans` 写 `history`（对齐 `PersistMetrics.recordPeerBan`），
    /// `ban-for-disconnect` 不落 history。
    #[test]
    fn record_bans_writes_history_and_skips_disconnect() {
        let db = Arc::new(Database::open_in_memory().expect("内存库"));
        let engine = engine(db.clone());
        let entry = DownloaderEntry { downloader: Arc::new(StubDownloader), increment_ban: true };

        engine.record_bans(&entry, &[ban("1.1.1.1", false), ban("1.1.1.2", true)], 1_700_000_000_000);

        // ban_logs 记录全部封禁；history 只记录非 disconnect
        let source =
            DbBtnSubmitSource::new(db.clone(), Arc::new(Translator::embedded()), "zh_cn");
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
