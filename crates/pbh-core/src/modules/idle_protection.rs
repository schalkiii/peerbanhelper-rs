//! 空闲连接 DoS 防护（`idle-connection-dos-protection`），忠实复刻上游 `IdleConnectionDosProtection`。
//!
//! 判定：当 peer 的实时速度与「观察窗口内的平均速度」都低于 `idle-speed-threshold`，
//! 且进度没有变化（`reset-on-status-change`）时，累计空闲时间超过 `max-allowed-idle-time` 即封禁。
//!
//! 说明：
//! - 观察窗口从「首次被记录」开始，直到被重置（速度达标 / 进度变化 / 平均速度达标）；
//! - 上游用 `System.currentTimeMillis()`，本实现取 `CheckContext.now_ms` 以便确定性测试；
//! - 上游 `percentageChange = |progress * 100 - lastProgress|` 的量纲不一致（前者 ×100），
//!   因此只要 peer 汇报的进度非 0，`reset-on-status-change` 会在每次检查时重置计时器——
//!   这一行为被保留（见 SPEC §5.9 与对应黄金测试）。

use crate::i18n::TranslationComponent;
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, RuleModule};
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

/// 保护模式（对齐上游 `ProtectionMode`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtectionMode {
    /// 0：在支持 Peer Flags 的下载器上保护下载与做种任务
    DeterminedByPeerFlags,
    /// 1：仅保护做种任务
    AlwaysSeeding,
    /// 2：无论是否支持 Peer Flags，都保护下载与做种任务
    AlwaysSeedingAndDownloading,
}

impl ProtectionMode {
    pub fn from_code(code: i64) -> Self {
        match code {
            1 => ProtectionMode::AlwaysSeeding,
            2 => ProtectionMode::AlwaysSeedingAndDownloading,
            _ => ProtectionMode::DeterminedByPeerFlags,
        }
    }

    pub fn code(&self) -> i64 {
        match self {
            ProtectionMode::DeterminedByPeerFlags => 0,
            ProtectionMode::AlwaysSeeding => 1,
            ProtectionMode::AlwaysSeedingAndDownloading => 2,
        }
    }
}

/// 模块参数（默认值对齐上游 `profile.yml`）。
#[derive(Clone, Debug)]
pub struct IdleProtectionSettings {
    pub ban_duration_ms: i64,
    pub max_allowed_idle_time_ms: i64,
    pub idle_speed_threshold: i64,
    pub min_status_change_percentage: f64,
    pub reset_on_status_change: bool,
    pub protect_mode: ProtectionMode,
}

impl Default for IdleProtectionSettings {
    fn default() -> Self {
        Self {
            ban_duration_ms: 900_000,
            max_allowed_idle_time_ms: 300_000,
            idle_speed_threshold: 64,
            min_status_change_percentage: 0.001,
            reset_on_status_change: true,
            protect_mode: ProtectionMode::DeterminedByPeerFlags,
        }
    }
}

#[derive(Clone, Debug)]
struct ConnectionInfo {
    idle_start_time: i64,
    percentage: f64,
    uploaded: i64,
    downloaded: i64,
    not_hit_counter: i32,
}

type PeerKey = (String, u16);

pub struct IdleConnectionDosProtection {
    pub settings: IdleProtectionSettings,
    idle_connections: StdMutex<HashMap<PeerKey, ConnectionInfo>>,
}

impl IdleConnectionDosProtection {
    pub fn new(settings: IdleProtectionSettings) -> Self {
        Self {
            settings,
            idle_connections: StdMutex::new(HashMap::new()),
        }
    }

    /// 当前跟踪的空闲连接数（供测试与状态展示）。
    pub fn tracked_peers(&self) -> usize {
        self.idle_connections.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// 每个 torrent 拉取完 peers 后调用：连续 5 次未出现的连接会被清理
    /// （对齐上游 `onPeersRetrieved`）。
    pub fn on_peers_retrieved(&self, peers: &[PeerKey]) {
        if let Ok(mut map) = self.idle_connections.lock() {
            map.retain(|key, info| {
                if !peers.contains(key) {
                    info.not_hit_counter += 1;
                }
                info.not_hit_counter <= 5
            });
        }
    }

    fn invalidate(&self, key: &PeerKey) {
        if let Ok(mut map) = self.idle_connections.lock() {
            map.remove(key);
        }
    }
}

impl RuleModule for IdleConnectionDosProtection {
    fn name(&self) -> &str {
        "Idle Connection DoS Protection"
    }
    fn config_name(&self) -> &str {
        "idle-connection-dos-protection"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn check(
        &self,
        _downloader_id: &str,
        torrent: &TorrentData,
        peer: &PeerData,
        ctx: &CheckContext,
    ) -> CheckResult {
        let module = self.config_name().to_string();
        let s = &self.settings;

        // 模式 1：仅保护做种任务
        if s.protect_mode == ProtectionMode::AlwaysSeeding && !torrent.is_seeding() {
            return CheckResult::pass(&module);
        }
        // 模式 0：下载任务需要 Peer Flags 支持，且「兴趣系统在工作」时忽略
        if s.protect_mode == ProtectionMode::DeterminedByPeerFlags && !torrent.is_seeding() {
            match peer.peer_flag() {
                None => return CheckResult::pass(&module),
                Some(flags) => {
                    if flags.interesting || flags.remote_interested {
                        return CheckResult::pass(&module);
                    }
                }
            }
        }

        let key: PeerKey = (peer.ip.clone(), peer.port);
        // 实时速度达标 -> 视为活跃连接
        if peer.up_speed > s.idle_speed_threshold || peer.dl_speed > s.idle_speed_threshold {
            self.invalidate(&key);
            return CheckResult::pass(&module);
        }

        let info = {
            let Ok(mut map) = self.idle_connections.lock() else {
                return CheckResult::pass(&module);
            };
            map.entry(key.clone())
                .or_insert_with(|| ConnectionInfo {
                    idle_start_time: ctx.now_ms,
                    percentage: peer.progress,
                    uploaded: peer.uploaded,
                    downloaded: peer.downloaded,
                    not_hit_counter: 0,
                })
                .clone()
        };

        let elapsed = ctx.now_ms - info.idle_start_time + 1;
        let avg_upload = (peer.uploaded - info.uploaded) / elapsed;
        let avg_download = (peer.downloaded - info.downloaded) / elapsed;
        let percentage_change = (peer.progress * 100.0 - info.percentage).abs();

        if avg_upload > s.idle_speed_threshold || avg_download > s.idle_speed_threshold {
            self.invalidate(&key);
            return CheckResult::pass(&module);
        }
        if s.reset_on_status_change && percentage_change >= s.min_status_change_percentage {
            self.invalidate(&key);
            return CheckResult::pass(&module);
        }

        let already_idled = ctx.now_ms - info.idle_start_time;
        if already_idled > s.max_allowed_idle_time_ms {
            self.invalidate(&key);
            let host_and_port = format!("{}:{}", peer.ip, peer.port);
            return CheckResult::ban(
                &module,
                s.ban_duration_ms,
                "idleTimeout",
                &format!("peer {host_and_port} idle for {already_idled} ms"),
                serde_json::json!({
                    "ip": host_and_port,
                    "idle_type": "timeout",
                    "idle_start": info.idle_start_time,
                    "last_percentage": info.percentage,
                    "last_uploaded": info.uploaded,
                    "last_downloaded": info.downloaded,
                    "percentage": peer.progress,
                    "uploaded": peer.uploaded,
                    "downloaded": peer.downloaded,
                    "upload_speed": peer.up_speed,
                    "download_speed": peer.dl_speed,
                }),
            )
            .with_keys(
                TranslationComponent::new("MODULE_ICDP_RULE_TITLE"),
                TranslationComponent::with_params(
                    "MODULE_ICDP_RULE_DESCRIPTION",
                    vec![host_and_port.into()],
                ),
            );
        }

        CheckResult::pass(&module)
    }
}
