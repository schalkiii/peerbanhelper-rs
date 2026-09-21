//! # pbh-core
//!
//! PeerBanHelper Rust 重写的核心：领域模型、规则引擎、规则模块与判定流水线。
//! 行为契约见根目录 SPEC.md，所有 `[GOLDEN]` 点由 pbh-golden 黄金测试锁定。

pub mod auto_stun;
pub mod auto_stun_forwarder;
pub mod auto_stun_probe;
pub mod banlist;
pub mod config;
pub mod defaults;
pub mod geoip;
pub mod geoip_update;
pub mod i18n;
pub mod iputil;
pub mod model;
pub mod module;
pub mod modules;
pub mod pipeline;
pub mod remap;
pub mod rule;

pub use banlist::{needs_full_ban_list, BanList, BannedRecord};
pub use model::{PeerAddress, PeerData, PeerFlag, TorrentData};
pub use module::{CheckContext, CheckResult, PeerAction, RuleModule};
pub use pipeline::{Decision, Pipeline};
pub use rule::{Matcher, RuleSet, Verdict};

/// 构建与上游默认行为一致的判定流水线（默认模块顺序 + bypass 地址）。
///
/// 模块顺序对齐上游 `PeerBanHelper.registerModules()`：
/// `IPBlackList → PeerIdBlacklist → ClientNameBlacklist → (ExpressionRule) →
/// ProgressCheatBlocker → MultiDialingBlocker → AutoRangeBan → (BtnNetworkOnline) →
/// IPBlackRuleList → … → AntiVampire`。
/// 该顺序是「PeerAction 等级与 ban 时长完全并列」时的最终裁决依据。
pub fn default_pipeline() -> Pipeline {
    use modules::{
        AntiVampire, AutoRangeBan, ExpressionEngine, IpBlacklist, IpRuleListModule,
        MultiDialingBlocker, ProgressCheatBlocker, StringBlacklist,
    };
    let mut p = Pipeline::default();
    let ban_list = p.ban_list.clone();
    p.add_module(Box::new(IpBlacklist::default()));
    p.add_module(Box::new(StringBlacklist::peer_id()));
    p.add_module(Box::new(StringBlacklist::client_name()));
    // 对齐上游 registerModules：expression-engine 在 client-name 之后、progress-cheat 之前
    p.add_module(Box::new(ExpressionEngine::new(0, None)));
    p.add_module(Box::new(ProgressCheatBlocker::default()));
    p.add_module(Box::new(MultiDialingBlocker::new(Default::default())));
    // 上游 profile.yml 默认：ipv4: 30、ipv6: 48、ban-duration: 604800000
    p.add_module(Box::new(AutoRangeBan::new(30, 48, 604_800_000, ban_list)));
    // 规则订阅模块本身不持有数据，订阅内容由应用层拉取后注入
    p.add_module(Box::new(IpRuleListModule::new(259_200_000)));
    p.add_module(Box::new(AntiVampire::new(Default::default())));
    p
}
