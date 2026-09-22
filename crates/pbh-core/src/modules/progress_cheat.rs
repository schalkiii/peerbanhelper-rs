//! 虚假进度检查器（`progress-cheat-blocker`），忠实复刻上游两级（IP + 前缀）状态机。
//!
//! 契约见 SPEC 5.4；算法逐行对齐 Java `ProgressCheatBlocker.shouldBanPeer`。

use crate::defaults::pcb as cfg;
use crate::defaults::PCB_BAN_DURATION_MS;
use crate::i18n::{format_percent, TranslationComponent};
use crate::iputil::prefix_block;
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, PeerAction, RuleModule};
use std::collections::HashMap;
use std::sync::Mutex;

#[derive(Clone, Debug)]
pub struct PcbConfig {
    pub torrent_minimum_size: i64,
    pub block_excessive_clients: bool,
    pub excessive_threshold: f64,
    pub maximum_difference: f64,
    pub rewind_maximum_difference: f64,
    pub ipv4_prefix_length: u8,
    pub ipv6_prefix_length: u8,
    pub ban_duration_ms: i64,
    pub max_wait_duration_ms: i64,
    pub fast_pcb_test_percentage: f64,
    pub fast_pcb_test_block_duration_ms: i64,
    /// 是否把状态落库（上游 `enable-persist`，默认 true）
    pub persist_enabled: bool,
    /// 持久化数据保留时长（上游 `persist-duration`，默认 14 天）
    pub persist_duration_ms: i64,
}

impl Default for PcbConfig {
    fn default() -> Self {
        Self {
            torrent_minimum_size: cfg::MINIMUM_SIZE,
            block_excessive_clients: cfg::BLOCK_EXCESSIVE_CLIENTS,
            excessive_threshold: cfg::EXCESSIVE_THRESHOLD,
            maximum_difference: cfg::MAXIMUM_DIFFERENCE,
            rewind_maximum_difference: cfg::REWIND_MAXIMUM_DIFFERENCE,
            ipv4_prefix_length: cfg::IPV4_PREFIX_LENGTH,
            ipv6_prefix_length: cfg::IPV6_PREFIX_LENGTH,
            ban_duration_ms: PCB_BAN_DURATION_MS,
            max_wait_duration_ms: cfg::MAX_WAIT_DURATION_MS,
            fast_pcb_test_percentage: cfg::FAST_PCB_TEST_PERCENTAGE,
            fast_pcb_test_block_duration_ms: cfg::FAST_PCB_TEST_BLOCK_DURATION_MS,
            persist_enabled: cfg::ENABLE_PERSIST,
            persist_duration_ms: cfg::PERSIST_DURATION_MS,
        }
    }
}

/// 单个 IP 或前缀的跟踪实体（对齐 PCBAddressEntity / PCBRangeEntity）。
#[derive(Clone, Debug, Default)]
pub struct PcbEntity {
    pub last_report_uploaded: i64,
    pub tracking_uploaded_increase_total: i64,
    pub last_report_progress: f64,
    pub last_torrent_completed_size: i64,
    pub progress_difference_counter: i64,
    pub rewind_counter: i64,
    /// 封禁延迟窗口结束时间（epoch ms），0 表示未安排
    pub ban_delay_window_end_ms: i64,
    pub fast_pcb_test_executed: bool,
    pub last_time_seen_ms: i64,
    /// 该实体首次出现时间（epoch ms；对齐上游 `firstTimeSeen`，落库列 `first_time_seen`）
    pub first_time_seen_ms: i64,
    /// 该 IP 首次出现时的端口（仅用于 `pcb_address` 行主键，不参与判定）
    pub port: u16,
    /// 自上次落库后是否有变更（对齐上游 `isDirty`）
    pub dirty: bool,
}

/// 持久化实体的类型（对应 `pcb_addr` / `pcb_range` 两张表）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PcbEntityKind {
    Addr,
    Range,
}

impl PcbEntityKind {
    pub fn is_addr(&self) -> bool {
        matches!(self, PcbEntityKind::Addr)
    }
}

/// 持久化行（对齐上游 `PCBAddressEntity` / `PCBRangeEntity` 的落库字段）。
#[derive(Clone, Debug, PartialEq)]
pub struct PcbPersistRow {
    pub kind: PcbEntityKind,
    pub downloader_id: String,
    pub torrent_id: String,
    /// `Range` 行为前缀字符串；`Addr` 行为 IP
    pub key: String,
    /// `Addr` 行的端口（`Range` 行为 0）
    pub port: u16,
    pub last_report_uploaded: i64,
    pub tracking_uploaded_increase_total: i64,
    pub last_report_progress: f64,
    pub last_torrent_completed_size: i64,
    pub progress_difference_counter: i64,
    pub rewind_counter: i64,
    pub ban_delay_window_end_ms: i64,
    pub fast_pcb_test_executed: bool,
    pub last_time_seen_ms: i64,
    /// `pcb_address` / `pcb_range` 的 `first_time_seen` 列。
    pub first_time_seen_ms: i64,
}

impl PcbPersistRow {
    fn from_entity(
        kind: PcbEntityKind,
        downloader_id: &str,
        torrent_id: &str,
        key: &str,
        e: &PcbEntity,
    ) -> Self {
        Self {
            kind,
            downloader_id: downloader_id.to_string(),
            torrent_id: torrent_id.to_string(),
            key: key.to_string(),
            port: if kind.is_addr() { e.port } else { 0 },
            last_report_uploaded: e.last_report_uploaded,
            tracking_uploaded_increase_total: e.tracking_uploaded_increase_total,
            last_report_progress: e.last_report_progress,
            last_torrent_completed_size: e.last_torrent_completed_size,
            progress_difference_counter: e.progress_difference_counter,
            rewind_counter: e.rewind_counter,
            ban_delay_window_end_ms: e.ban_delay_window_end_ms,
            fast_pcb_test_executed: e.fast_pcb_test_executed,
            last_time_seen_ms: e.last_time_seen_ms,
            // 内存实体未记录首次出现时间时退回 `last_time_seen`（仅用于落库展示列）。
            first_time_seen_ms: if e.first_time_seen_ms > 0 {
                e.first_time_seen_ms
            } else {
                e.last_time_seen_ms
            },
        }
    }

    fn to_entity(&self) -> PcbEntity {
        PcbEntity {
            last_report_uploaded: self.last_report_uploaded,
            tracking_uploaded_increase_total: self.tracking_uploaded_increase_total,
            last_report_progress: self.last_report_progress,
            last_torrent_completed_size: self.last_torrent_completed_size,
            progress_difference_counter: self.progress_difference_counter,
            rewind_counter: self.rewind_counter,
            ban_delay_window_end_ms: self.ban_delay_window_end_ms,
            fast_pcb_test_executed: self.fast_pcb_test_executed,
            last_time_seen_ms: self.last_time_seen_ms,
            first_time_seen_ms: self.first_time_seen_ms,
            port: self.port,
            dirty: false,
        }
    }
}

/// 前缀实体键：`(downloader, torrent, prefix)` —— 对齐 Java `CacheKeyPrefix`。
type RangeKey = (String, String, String);
/// IP 实体键：`(downloader, torrent, ip)` —— 对齐 Java `CacheKeyAddr`。
///
/// 注意：**上游的键不含端口**。若把端口计入键，同一 IP 的不同端口会被当作不同实体，
/// 上传增量被重复累加（进而误判「过量下载」/进度差），与 Java 行为不一致。
/// 端口仅用于数据库行（`pcb_addr` 主键含 port），取该 IP 首次出现时的端口。
type AddrKey = (String, String, String);

#[derive(Default, Debug)]
pub struct PcbStore {
    range: HashMap<RangeKey, PcbEntity>,
    addr: HashMap<AddrKey, PcbEntity>,
}

impl PcbStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 同时取出（前缀实体, IP 实体）的可变引用（对齐 loadFromDatabase 同时加载两者）。
    pub fn pair(
        &mut self,
        range_key: &RangeKey,
        addr_key: &AddrKey,
    ) -> (&mut PcbEntity, &mut PcbEntity) {
        // range 字段先借、addr 字段后借，两个不同字段可同时可变借用。
        let range = self.range.entry(range_key.clone()).or_default();
        let addr = self.addr.entry(addr_key.clone()).or_default();
        (range, addr)
    }

    pub fn entity_count(&self) -> usize {
        self.range.len() + self.addr.len()
    }

    /// 从持久化行恢复状态（对齐 `loadFromDatabase` 命中数据库的分支）。
    pub fn load(&mut self, rows: impl IntoIterator<Item = PcbPersistRow>) {
        for row in rows {
            let entity = row.to_entity();
            match row.kind {
                PcbEntityKind::Addr => {
                    self.addr
                        .insert((row.downloader_id, row.torrent_id, row.key), entity);
                }
                PcbEntityKind::Range => {
                    self.range
                        .insert((row.downloader_id, row.torrent_id, row.key), entity);
                }
            }
        }
    }

    /// 取出并清除所有「自上次落库后变更过」的实体（对齐 `batchFlushBackDatabase*` 的 `isDirty` 过滤）。
    pub fn dirty_rows(&mut self) -> Vec<PcbPersistRow> {
        let mut out = Vec::new();
        for ((dl, torrent, key), entity) in self.range.iter_mut() {
            if entity.dirty {
                out.push(PcbPersistRow::from_entity(
                    PcbEntityKind::Range,
                    dl,
                    torrent,
                    key,
                    entity,
                ));
                entity.dirty = false;
            }
        }
        for ((dl, torrent, key), entity) in self.addr.iter_mut() {
            if entity.dirty {
                out.push(PcbPersistRow::from_entity(
                    PcbEntityKind::Addr,
                    dl,
                    torrent,
                    key,
                    entity,
                ));
                entity.dirty = false;
            }
        }
        out
    }
}

pub struct ProgressCheatBlocker {
    pub config: PcbConfig,
    pub store: Mutex<PcbStore>,
}

impl Default for ProgressCheatBlocker {
    fn default() -> Self {
        Self::new(PcbConfig::default())
    }
}

impl ProgressCheatBlocker {
    pub fn new(config: PcbConfig) -> Self {
        Self {
            config,
            store: Mutex::new(PcbStore::new()),
        }
    }

    /// 从数据库恢复状态（`enable-persist` 关闭时忽略）。
    pub fn load_persisted(&self, rows: impl IntoIterator<Item = PcbPersistRow>) {
        if !self.config.persist_enabled {
            return;
        }
        if let Ok(mut store) = self.store.lock() {
            store.load(rows);
        }
    }

    /// 取出需要落库的实体（每轮 wave 结束后调用）。
    pub fn flush_dirty(&self) -> Vec<PcbPersistRow> {
        if !self.config.persist_enabled {
            return Vec::new();
        }
        self.store
            .lock()
            .map(|mut s| s.dirty_rows())
            .unwrap_or_default()
    }

    /// 清理超过 `persist-duration` 未出现的记录（对齐 `cleanDatabase`）。
    pub fn cleanup_expired(&self, older_than_ms: i64) -> usize {
        if !self.config.persist_enabled {
            return 0;
        }
        let mut store = match self.store.lock() {
            Ok(s) => s,
            Err(_) => return 0,
        };
        let before = store.entity_count();
        store
            .range
            .retain(|_, e| e.last_time_seen_ms >= older_than_ms);
        store
            .addr
            .retain(|_, e| e.last_time_seen_ms >= older_than_ms);
        before - store.entity_count()
    }

    fn file_too_small(&self, torrent_size: i64) -> bool {
        torrent_size < self.config.torrent_minimum_size
    }

    fn window_scheduled(&self, range: &PcbEntity, addr: &PcbEntity) -> bool {
        range.ban_delay_window_end_ms > 0 || addr.ban_delay_window_end_ms > 0
    }
    fn window_expired(&self, range: &PcbEntity, addr: &PcbEntity, now_ms: i64) -> bool {
        (range.ban_delay_window_end_ms > 0 && range.ban_delay_window_end_ms < now_ms)
            || (addr.ban_delay_window_end_ms > 0 && addr.ban_delay_window_end_ms < now_ms)
    }
    fn schedule_window(&self, range: &mut PcbEntity, addr: &mut PcbEntity, now_ms: i64) {
        if range.ban_delay_window_end_ms <= 0 {
            range.ban_delay_window_end_ms = now_ms + self.config.max_wait_duration_ms;
        }
        if addr.ban_delay_window_end_ms <= 0 {
            addr.ban_delay_window_end_ms = now_ms + self.config.max_wait_duration_ms;
        }
    }
    fn reset_window(&self, range: &mut PcbEntity, addr: &mut PcbEntity) {
        range.ban_delay_window_end_ms = 0;
        addr.ban_delay_window_end_ms = 0;
    }
}

impl RuleModule for ProgressCheatBlocker {
    fn name(&self) -> &str {
        "Progress Cheat Blocker"
    }
    fn config_name(&self) -> &str {
        "progress-cheat-blocker"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn check(
        &self,
        downloader_id: &str,
        torrent: &TorrentData,
        peer: &PeerData,
        ctx: &CheckContext,
    ) -> CheckResult {
        let module = self.config_name().to_string();
        if peer.is_handshaking() {
            return CheckResult::handshaking(&module);
        }
        let c = &self.config;
        let now = ctx.now_ms;

        let prefix_string = prefix_block(&peer.ip, c.ipv4_prefix_length, c.ipv6_prefix_length)
            .unwrap_or_else(|| peer.ip.clone());
        let range_key = (
            downloader_id.to_string(),
            torrent.id().to_string(),
            prefix_string,
        );
        let addr_key = (
            downloader_id.to_string(),
            torrent.id().to_string(),
            peer.ip.clone(),
        );

        let mut store = self.store.lock().unwrap();
        let (range, addr) = store.pair(&range_key, &addr_key);
        // 端口只用于 `pcb_addr` 行主键：取该 IP 首次出现时的端口（键本身不含端口）
        if addr.port == 0 {
            addr.port = peer.port;
        }

        // 上传增量（处理回绕）
        let computed_incremental = if peer.uploaded < addr.last_report_uploaded {
            peer.uploaded
        } else {
            peer.uploaded - addr.last_report_uploaded
        };
        addr.tracking_uploaded_increase_total += computed_incremental;
        range.tracking_uploaded_increase_total += computed_incremental;
        let computed_uploaded = peer.uploaded.max(
            addr.tracking_uploaded_increase_total
                .max(range.tracking_uploaded_increase_total),
        );

        let result = self.evaluate(torrent, peer, ctx, range, addr, computed_uploaded, now);

        // finally：无论结果如何都更新上报快照
        {
            addr.last_report_uploaded = peer.uploaded;
            range.last_report_uploaded = peer.uploaded;
            if peer.progress != 0.0 {
                addr.last_report_progress = peer.progress;
                range.last_report_progress = peer.progress;
            }
            let completed = torrent.completed_size();
            addr.last_torrent_completed_size = completed.max(addr.last_torrent_completed_size);
            range.last_torrent_completed_size = completed.max(range.last_torrent_completed_size);
            addr.last_time_seen_ms = now;
            range.last_time_seen_ms = now;
            // 标记为待落库（对齐上游实体的 isDirty）
            addr.dirty = true;
            range.dirty = true;
        }

        result
    }
}

impl ProgressCheatBlocker {
    #[allow(clippy::too_many_arguments)]
    fn evaluate(
        &self,
        torrent: &TorrentData,
        peer: &PeerData,
        ctx: &CheckContext,
        range: &mut PcbEntity,
        addr: &mut PcbEntity,
        computed_uploaded: i64,
        now: i64,
    ) -> CheckResult {
        let module = self.config_name().to_string();
        let c = &self.config;
        let torrent_size = torrent.total_size;
        let completed_size = torrent.completed_size();
        let computed_completed_size = completed_size.max(
            range
                .last_torrent_completed_size
                .max(addr.last_torrent_completed_size),
        );

        if torrent_size <= 0 {
            return CheckResult::pass(&module);
        }
        if !peer.is_uploading_to_peer() {
            return CheckResult::pass(&module);
        }

        // 快速 PCB 测试
        if c.fast_pcb_test_percentage > 0.0
            && !self.file_too_small(torrent_size)
            && ctx.has_feature("UNBAN_IP")
        {
            let never_tested = !addr.fast_pcb_test_executed || !range.fast_pcb_test_executed;
            if never_tested
                && computed_uploaded as f64 >= c.fast_pcb_test_percentage * torrent_size as f64
            {
                addr.fast_pcb_test_executed = true;
                range.fast_pcb_test_executed = true;
                return CheckResult {
                    module: module.clone(),
                    action: PeerAction::BanForDisconnect,
                    ban_duration_ms: c.fast_pcb_test_block_duration_ms,
                    rule: "fastPcbTest".to_string(),
                    reason: "progress cheat testing".to_string(),
                    data: serde_json::json!({ "type": "fastPcbTest" }),
                    rule_key: None,
                    reason_key: None,
                }
                .with_keys(
                    TranslationComponent::new("PCB_RULE_PEER_PROGRESS_CHEAT_TESTING"),
                    TranslationComponent::new("PCB_DESCRIPTION_PEER_PROGRESS_CHEAT_TESTING"),
                );
            }
        }

        let computed_progress = computed_uploaded as f64 / torrent_size as f64;
        let reported_progress = peer.progress;

        // 过量下载检查
        if computed_uploaded != -1 && c.block_excessive_clients {
            if computed_uploaded > torrent_size {
                let threshold = (torrent_size.max(c.torrent_minimum_size) as f64
                    * c.excessive_threshold) as i64;
                if computed_uploaded > threshold {
                    self.reset_window(range, addr);
                    return CheckResult::ban(
                        &module,
                        c.ban_duration_ms,
                        "excessiveMaxDownloadThreshold",
                        "excessive download beyond torrent size",
                        serde_json::json!({ "type": "excessiveMaxDownloadThreshold", "maxAllowedExcessiveThreshold": threshold }),
                    )
                    .with_keys(
                        TranslationComponent::new("PCB_RULE_REACHED_MAX_ALLOWED_EXCESSIVE_THRESHOLD"),
                        TranslationComponent::with_params(
                            "MODULE_PCB_EXCESSIVE_DOWNLOAD",
                            vec![
                                torrent_size.to_string().into(),
                                computed_uploaded.to_string().into(),
                                threshold.to_string().into(),
                            ],
                        ),
                    );
                }
            } else if completed_size > 0 && computed_uploaded > completed_size {
                let threshold = (computed_completed_size.max(c.torrent_minimum_size) as f64
                    * c.excessive_threshold) as i64;
                if computed_uploaded > threshold {
                    self.reset_window(range, addr);
                    return CheckResult::ban(
                        &module,
                        c.ban_duration_ms,
                        "excessiveMaxDownloadThresholdForIncompleteTask",
                        "excessive download beyond completed size",
                        serde_json::json!({ "type": "excessiveMaxDownloadThresholdForIncompleteTask", "maxAllowedExcessiveThreshold": threshold }),
                    )
                    .with_keys(
                        TranslationComponent::new("PCB_RULE_REACHED_MAX_ALLOWED_EXCESSIVE_THRESHOLD"),
                        TranslationComponent::with_params(
                            "MODULE_PCB_EXCESSIVE_DOWNLOAD_INCOMPLETE",
                            vec![
                                torrent_size.to_string().into(),
                                completed_size.to_string().into(),
                                computed_uploaded.to_string().into(),
                                threshold.to_string().into(),
                            ],
                        ),
                    );
                }
            }
        }

        // 客户端自报进度更高则跳过
        if computed_progress <= reported_progress {
            return CheckResult::pass(&module);
        }

        // 差值测试
        let difference = (computed_progress - reported_progress).abs();
        if difference > c.maximum_difference
            && !self.file_too_small(torrent_size)
            && peer.is_uploading_to_peer()
        {
            if !self.window_scheduled(range, addr) {
                self.schedule_window(range, addr, now);
                return CheckResult::pass(&module);
            }
            if self.window_expired(range, addr, now) {
                range.progress_difference_counter += 1;
                addr.progress_difference_counter += 1;
                self.reset_window(range, addr);
                return CheckResult::ban(
                    &module,
                    c.ban_duration_ms,
                    "deSyncDifference",
                    "reported progress differs from computed beyond threshold",
                    serde_json::json!({
                        "type": "deSyncDifference",
                        "difference": difference,
                        "computedProgress": computed_progress,
                        "clientReportedProgress": reported_progress
                    }),
                )
                .with_keys(
                    TranslationComponent::new("PCB_RULE_REACHED_MAX_DIFFERENCE"),
                    TranslationComponent::with_params(
                        "MODULE_PCB_PEER_BAN_INCORRECT_PROGRESS",
                        vec![
                            format_percent(reported_progress).into(),
                            format_percent(computed_progress).into(),
                            format_percent(difference).into(),
                        ],
                    ),
                );
            }
        }

        // 进度倒退测试
        if c.rewind_maximum_difference > 0.0 && !self.file_too_small(torrent_size) {
            let last_report_progress = addr.last_report_progress.max(range.last_report_progress);
            let rewind = last_report_progress - peer.progress;
            if rewind > c.rewind_maximum_difference && peer.is_uploading_to_peer() {
                if peer.progress > 0.0 || self.window_expired(range, addr, now) {
                    addr.rewind_counter += 1;
                    range.rewind_counter += 1;
                    self.reset_window(range, addr);
                    return CheckResult::ban(
                        &module,
                        c.ban_duration_ms,
                        "rewindProgress",
                        "progress rewound beyond allowed threshold",
                        serde_json::json!({
                            "type": "rewindProgress",
                            "rewind": rewind,
                            "lastReportProgress": last_report_progress,
                            "currentProgress": peer.progress
                        }),
                    )
                    .with_keys(
                        TranslationComponent::new("PCB_RULE_PROGRESS_REWIND"),
                        TranslationComponent::with_params(
                            "MODULE_PCB_PEER_BAN_REWIND",
                            vec![
                                format_percent(peer.progress).into(),
                                format_percent(computed_progress).into(),
                                format_percent(last_report_progress).into(),
                                format_percent(rewind).into(),
                                format_percent(c.rewind_maximum_difference).into(),
                            ],
                        ),
                    );
                } else if !self.window_scheduled(range, addr) {
                    self.schedule_window(range, addr, now);
                }
            }
        }

        CheckResult::pass(&module)
    }
}
