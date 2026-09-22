//! 内置规则模块（SPEC 第 5 节）。

pub mod anti_vampire;
pub mod auto_range_ban;
pub mod btn;
pub mod client_name;
pub mod expression_engine;
pub mod idle_protection;
pub mod ip_blacklist;
pub mod ip_rule_list;
pub mod monitor;
pub mod multi_dialing;
pub mod peer_id;
pub mod progress_cheat;
pub mod ptr_blacklist;
pub mod string_blacklist;

pub use anti_vampire::{AntiVampire, AntiVampireSettings};
pub use auto_range_ban::AutoRangeBan;
pub use btn::BtnNetworkOnline;
pub use client_name::ClientNameBlacklist;
pub use expression_engine::ExpressionEngine;
pub use idle_protection::{IdleConnectionDosProtection, IdleProtectionSettings, ProtectionMode};
pub use ip_blacklist::IpBlacklist;
pub use ip_rule_list::{IpRuleListModule, RuleListEntry, RuleSubscription};
pub use monitor::{
    ActiveMonitoringModule, ActiveMonitoringSettings, AlertLevel, AlertRecord,
    ConnectionMetricsRow, DownloaderTrafficStats, InMemoryMonitorSink, MetricsTrackKey,
    MetricsTrackRow, MonitorSink, PeerRecordCacheKey, PeerRecordCachingEntire, PeerRecordRow,
    PeerRecordingServiceModule, PeerRecordingSettings, SessionAnalyseServiceModule,
    SessionAnalyseSettings, SessionFlushSummary, SlidingWindowDynamicSpeedLimiter,
    SpeedLimitChange, SpeedLimiter, SwarmTrackingModule, SwarmTrackingSettings, TrackedSwarmKey,
    TrackedSwarmRow, TrafficDataComputed, TrafficJournalRow, TrafficMonitoringAlert,
};
pub use multi_dialing::{MultiDialingBlocker, MultiDialingSettings};
pub use peer_id::PeerIdBlacklist;
pub use progress_cheat::{PcbConfig, ProgressCheatBlocker};
pub use ptr_blacklist::{PtrBlacklist, PtrCache};
pub use string_blacklist::StringBlacklist;
