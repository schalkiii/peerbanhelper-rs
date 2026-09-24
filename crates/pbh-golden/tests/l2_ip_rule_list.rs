//! [GOLDEN] `ip-address-blocker-rules`（IP 黑名单远程订阅）模块契约，
//! 对齐上游 `IPBlackRuleList` + `IPMatcher` + `parseRuleLine`。
//!
//! 关键行为（必须稳定，任意解析/索引改动不得改变）：
//! 1. 解析：空行跳过；`#`/`//` 整行注释会**累积**给下一条规则；尾注释（`ip # ...`）优先；
//!    DAT 格式 `start,end,level[,comment]`，`level >= 128` 丢弃；非对齐区间向下对齐到前缀块；
//! 2. 判定：`rule_key` 取订阅名（上游把规则名当字面量 key），`reason` 走
//!    `MODULE_IBL_MATCH_IP_RULE(订阅名, IP, 备注)`，命中网段的备注经最长前缀匹配返回；
//! 3. 更新决策：`plan_rule_update` 对齐上游 sha256 比对流程。

use pbh_core::i18n::Translator;
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::module::{CheckContext, PeerAction, RuleModule};
use pbh_core::modules::ip_rule_list::{
    parse_rule_list, plan_rule_update, IpRuleListModule, RuleSubscription, RuleUpdatePlan,
};

fn peer(ip: &str) -> PeerData {
    PeerData {
        client_name: Some("qBittorrent/4.5.0".into()),
        peer_id: Some("-qB5000-000000000000".into()),
        dl_speed: 1000,
        downloaded: 1000,
        up_speed: 1000,
        uploaded: 1000,
        progress: 0.5,
        flags: Some("d u".into()),
        ip: ip.into(),
        port: 6881,
        raw_ip: format!("{ip}:6881"),
        connection: Some("uTP".into()),
    }
}

fn torrent() -> TorrentData {
    TorrentData {
        hash: "h".into(),
        name: "t".into(),
        progress: 1.0,
        total_size: 1_000_000_000,
        piece_size: 0,
        pieces_have: 0,
        completed_override: None,
        dlspeed: 0,
        upspeed: 0,
        is_private: Some(false),
    }
}

fn ctx() -> CheckContext {
    CheckContext {
        now_ms: 0,
        features: vec!["BAN_IP".into()],
    }
}

/// 解析多格式规则文本并订阅，命中 peer 的 `rule_key` 必须是订阅名、
/// `reason` 必须按 `MODULE_IBL_MATCH_IP_RULE(订阅名, IP, 备注)` 渲染。
#[test]
fn subscription_ban_uses_subscription_name_as_rule_key() {
    let list = "\
# 这是一段累积注释
1.2.3.0/24
192.168.1.1 # 尾注释
1.2.4.0,1.2.4.255,100,range-comment
128.0.0.0,128.0.0.0,200,bad-level
";
    let (entries, count) = parse_rule_list(list);
    // level>=128 的行被丢弃，注释行不计入；有效条目 3 条
    assert_eq!(count, 3, "level>=128 的行不应计入加载行数");
    assert_eq!(entries.len(), 3);

    let module = IpRuleListModule::new(604_800_000);
    assert!(module.set_subscription(RuleSubscription {
        rule_id: "sub-1".into(),
        name: "测试订阅".into(),
        entries,
    }));

    let tr = Translator::embedded();

    // 命中 1.2.3.0/24（累积注释）
    let r = module.check("d", &torrent(), &peer("1.2.3.55"), &ctx());
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(r.ban_duration_ms, 604_800_000);
    assert_eq!(r.rule_key.as_ref().unwrap().key, "测试订阅");
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "匹配 IP黑名单订阅 规则: 测试订阅, IP 地址: 1.2.3.55, 备注:  这是一段累积注释"
    );
    assert_eq!(r.data["ruleName"], "测试订阅");

    // 命中 192.168.1.1/32（尾注释优先于累积注释）
    let r = module.check("d", &torrent(), &peer("192.168.1.1"), &ctx());
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "匹配 IP黑名单订阅 规则: 测试订阅, IP 地址: 192.168.1.1, 备注:  尾注释"
    );

    // 命中 DAT 区间（1.2.4.0/24，向下对齐）
    let r = module.check("d", &torrent(), &peer("1.2.4.100"), &ctx());
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "匹配 IP黑名单订阅 规则: 测试订阅, IP 地址: 1.2.4.100, 备注: range-comment"
    );

    // level>=128 的规则被丢弃 -> 不命中
    assert_eq!(
        module.check("d", &torrent(), &peer("128.0.0.0"), &ctx()).action,
        PeerAction::NoAction
    );
}

/// 最长前缀匹配：重叠网段必须命中「最精确」的那条，并返回其备注。
#[test]
fn longest_prefix_match_wins() {
    let list = "\
10.0.0.0/8 # 大网段
10.1.0.0/16 # 中网段
10.1.2.0/24 # 小网段
";
    let (entries, _) = parse_rule_list(list);
    let module = IpRuleListModule::new(0);
    module.set_subscription(RuleSubscription {
        rule_id: "sub-2".into(),
        name: "分层订阅".into(),
        entries,
    });
    let tr = Translator::embedded();
    let r = module.check("d", &torrent(), &peer("10.1.2.5"), &ctx());
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "匹配 IP黑名单订阅 规则: 分层订阅, IP 地址: 10.1.2.5, 备注:  小网段",
        "必须命中最具体的 /24 而非 /8"
    );
}

/// 非对齐起点区间向下对齐到前缀边界（上游 `coverWithPrefixBlock`）。
#[test]
fn non_aligned_range_is_aligned_to_prefix_block() {
    let list = "1.2.3.5,1.2.3.8,64,loose-range";
    let (entries, count) = parse_rule_list(list);
    assert_eq!(count, 1);
    // [1.2.3.5, 1.2.3.8] -> 1.2.3.0/28（覆盖该区间的最小前缀块，向下对齐）
    assert_eq!(entries[0].net.to_string(), "1.2.3.0/28");
    let module = IpRuleListModule::new(0);
    module.set_subscription(RuleSubscription {
        rule_id: "sub-3".into(),
        name: "对齐订阅".into(),
        entries,
    });
    // 起点 1.2.3.4 在 1.2.3.4/29 内
    assert_eq!(
        module.check("d", &torrent(), &peer("1.2.3.4"), &ctx()).action,
        PeerAction::Ban
    );
    // 1.2.3.16 不在 1.2.3.4/29 内
    assert_eq!(
        module.check("d", &torrent(), &peer("1.2.3.16"), &ctx()).action,
        PeerAction::NoAction
    );
}

/// 更新决策对齐上游 sha256 比对流程。
#[test]
fn update_plan_mirrors_upstream_sha256_flow() {
    // 远程可达且与缓存不同 -> 解析远程并写回缓存
    assert_eq!(
        plan_rule_update(true, true, false, false),
        RuleUpdatePlan::ParseRemote { write_cache: true }
    );
    // 远程可达但内容相同、内存中尚未加载 -> 解析远程（不重复写盘）
    assert_eq!(
        plan_rule_update(true, false, false, false),
        RuleUpdatePlan::ParseRemote { write_cache: false }
    );
    // 远程可达、内容相同且已加载 -> 不做任何动作
    assert_eq!(
        plan_rule_update(true, false, false, true),
        RuleUpdatePlan::NoAction
    );
    // 远程失败、存在缓存且未加载 -> 回退解析缓存
    assert_eq!(
        plan_rule_update(false, false, true, false),
        RuleUpdatePlan::ParseCache
    );
    // 远程失败且无缓存（或未加载）-> 不做任何动作
    assert_eq!(
        plan_rule_update(false, false, false, false),
        RuleUpdatePlan::NoAction
    );
}

/// 握手中（速度皆为 0）的 peer 由本模块直接判 `handshaking`，不进入规则匹配。
#[test]
fn handshaking_peer_is_skipped_before_lookup() {
    let (entries, _) = parse_rule_list("9.9.9.9");
    let module = IpRuleListModule::new(0);
    module.set_subscription(RuleSubscription {
        rule_id: "sub-4".into(),
        name: "hs".into(),
        entries,
    });
    let mut hs = peer("9.9.9.9");
    hs.up_speed = 0;
    hs.dl_speed = 0;
    let r = module.check("d", &torrent(), &hs, &ctx());
    assert_eq!(r.action, PeerAction::NoAction);
    assert_eq!(r.reason_key.as_ref().unwrap().key, "Peer handshaking");
}
