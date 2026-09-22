//! IP 规则订阅（`ip-address-blocker-rules`）的黄金测试。
//!
//! 对齐上游 `IPBlackRuleList.stringToIPList` / `parseRuleLine` / `IPMatcher.match0`：
//! - 空行跳过；`#` 或 `//` 开头的整行是「注释行」，累积到下一条 IP 规则上（多行用 `\n` 连接）
//! - 行内 `#` 或 `//` 之后的文本是该行的尾注释，优先于累积注释
//! - 含 `,` 的行按 DAT/eMule 格式解析：`start , end , level [, comment]`，`level >= 128` 丢弃，
//!   地址区间取「能覆盖整个区间的最小前缀块」
//! - 重复的地址块合并注释（用 `\n` 连接）

use pbh_core::i18n::Translator;
use pbh_core::model::{PeerData, TorrentData};
use pbh_core::module::{CheckContext, PeerAction, RuleModule};
use pbh_core::modules::ip_rule_list::{IpRuleListModule, RuleListEntry, RuleSubscription};

fn peer(ip: &str) -> PeerData {
    PeerData {
        client_name: Some("qBittorrent/4.5.0".into()),
        peer_id: Some("-qB4700-xxxxxxxxxxxx".into()),
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

fn ctx() -> CheckContext {
    CheckContext {
        now_ms: 0,
        features: vec!["UNBAN_IP".into()],
    }
}

fn module(entries: Vec<RuleListEntry>) -> IpRuleListModule {
    let m = IpRuleListModule::new(259_200_000);
    m.set_subscription(RuleSubscription {
        rule_id: "all-in-one".into(),
        name: "all-in-one".into(),
        entries,
    });
    m
}

// ---------- 解析 ----------

#[test]
fn parses_plain_cidr_with_accumulated_and_inline_comments() {
    let data = "\
# 这是头部注释
// 双斜线注释
1.2.3.0/24
4.4.4.4 # 行内注释
8.8.8.0/24 , 
";
    let (entries, count) = pbh_core::modules::ip_rule_list::parse_rule_list(data);
    // 行内的 "8.8.8.0/24 , " 含逗号但字段不足 3 个 -> 丢弃
    assert_eq!(count, 2);
    assert_eq!(entries.len(), 2);

    let first = &entries[0];
    assert_eq!(first.net.to_string(), "1.2.3.0/24");
    // 上游对注释行做 `substring(1)` / `substring(2)`，只去掉标记本身，保留原文（含前导空格）
    assert_eq!(
        first.comment, " 这是头部注释\n 双斜线注释",
        "多行注释用换行连接"
    );

    let second = &entries[1];
    assert_eq!(second.net.to_string(), "4.4.4.4/32");
    // 上游取 `#` 之后的原文（含前导空格）
    assert_eq!(second.comment, " 行内注释", "尾注释优先于累积注释");
}

#[test]
fn parses_dat_emule_format_and_skips_high_levels() {
    // 字段之间不加空格（否则 level 字段会被上游 `Integer.parseInt` 拒绝而丢行）
    // 注意：`level >= 128` 一律丢弃 —— 上游注释里的示例 `..., 200 , ...` 同样会被丢弃
    let data = "\
016.000.000.000,016.255.255.255,127,Yet another organization
032.000.000.000,032.255.255.255,0,And another
064.0.0.0,064.0.0.255,128,too high level
016.0.0.0,016.15.255.255,200,upstream example level also >= 128 so dropped
";
    let (entries, count) = pbh_core::modules::ip_rule_list::parse_rule_list(data);
    assert_eq!(count, 2, "level >= 128 的行不计入");
    assert_eq!(
        entries[0].net.to_string(),
        "16.0.0.0/8",
        "区间取最小覆盖前缀块"
    );
    assert_eq!(entries[0].comment, "Yet another organization");
    assert_eq!(entries[1].net.to_string(), "32.0.0.0/8");
}

/// 上游 `parseRuleLine` 对 DAT 行使用 `getIPAddress` / `Integer.parseInt`（均不 trim），
/// 因此「字段带空格」的 DAT 行会被解析异常吞掉而丢弃。这里用 `level < 128` 的行，
/// 确保丢弃是**因为空格**而非因为 level 阈值——否则会被静默接纳，造成应封禁的网段漏封。
#[test]
fn spaced_dat_lines_are_dropped_like_upstream_integer_parse() {
    // level < 128 但带空格 -> 上游整行丢弃，本实现同样丢弃
    let data = "016.000.000.000 , 016.255.255.255 , 100 , Yet another organization\n";
    let (entries, count) = pbh_core::modules::ip_rule_list::parse_rule_list(data);
    assert!(entries.is_empty());
    assert_eq!(count, 0);

    // 对照：同样的行去掉空格（level 仍为 100）-> 正常解析
    let data2 = "016.000.000.000,016.255.255.255,100,Yet another organization\n";
    let (entries2, count2) = pbh_core::modules::ip_rule_list::parse_rule_list(data2);
    assert_eq!(count2, 1);
    assert_eq!(entries2[0].net.to_string(), "16.0.0.0/8");
}

/// 非对齐 DAT 区间必须向下对齐到前缀块（对齐上游 `coverWithPrefixBlock`）。
/// 若不对其，Rust 的 `IpNet::new` 会因主机位非零返回 None 而整行丢弃——漏封。
#[test]
fn dat_range_is_prefixed_down_to_network_boundary() {
    // [1.2.3.5, 1.2.3.6] -> 上游生成 1.2.3.4/30
    let data = "1.2.3.5,1.2.3.6,100,non-aligned range\n";
    let (entries, count) = pbh_core::modules::ip_rule_list::parse_rule_list(data);
    assert_eq!(count, 1);
    assert_eq!(entries[0].net.to_string(), "1.2.3.4/30");

    // [1.2.3.5, 1.2.3.8] -> 1.2.3.0/28
    let data2 = "1.2.3.5,1.2.3.8,100,non-aligned wider\n";
    let (entries2, count2) = pbh_core::modules::ip_rule_list::parse_rule_list(data2);
    assert_eq!(count2, 1);
    assert_eq!(entries2[0].net.to_string(), "1.2.3.0/28");

    // 命中校验：边界内的地址应被封禁
    let m = module(entries2);
    assert_eq!(
        m.check("d", &torrent(), &peer("1.2.3.7"), &ctx()).action,
        PeerAction::Ban
    );
    assert_eq!(
        m.check("d", &torrent(), &peer("1.2.3.20"), &ctx()).action,
        PeerAction::NoAction
    );
}

#[test]
fn merges_duplicate_networks_into_one_comment() {
    let data = "\
1.2.3.0/24 # first
1.2.3.0/24 # second
";
    let (entries, count) = pbh_core::modules::ip_rule_list::parse_rule_list(data);
    assert_eq!(count, 2);
    assert_eq!(entries.len(), 1, "同一网段只保留一条记录");
    // 尾注释同样保留 `#` 后的原文（含前导空格）
    assert_eq!(entries[0].comment, " first\n second");
}

#[test]
fn skips_invalid_lines_without_creating_placeholder_entries() {
    // 上游 IPAddressUtil 对非法输入返回占位地址 127.123.123.123；
    // 本实现直接跳过（占位地址恒被 bypass，行为等价且不会污染规则集）
    let data = "not-an-ip\n1.2.3.4\n";
    let (entries, count) = pbh_core::modules::ip_rule_list::parse_rule_list(data);
    assert_eq!(entries.len(), 1);
    assert_eq!(count, 1);
    assert_eq!(entries[0].net.to_string(), "1.2.3.4/32");
}

#[test]
fn comment_containing_double_slash_after_hash_is_kept() {
    let data = "1.2.3.4 # see http://example.com/x\n";
    let (entries, _) = pbh_core::modules::ip_rule_list::parse_rule_list(data);
    assert_eq!(entries[0].comment, " see http://example.com/x");
}

// ---------- 模块判定 ----------

#[test]
fn module_bans_ips_present_in_the_subscription() {
    let (entries, _) =
        pbh_core::modules::ip_rule_list::parse_rule_list("# 恶意网段\n10.10.0.0/16 # 数据中心\n");
    let m = module(entries);
    let t = torrent();
    let tr = Translator::embedded();

    let r = m.check("d", &t, &peer("10.10.5.5"), &ctx());
    assert_eq!(r.action, PeerAction::Ban);
    assert_eq!(r.ban_duration_ms, 259_200_000);
    assert_eq!(
        tr.render(r.rule_key.as_ref().unwrap(), "zh_cn"),
        "all-in-one"
    );
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "匹配 IP黑名单订阅 规则: all-in-one, IP 地址: 10.10.5.5, 备注:  数据中心"
    );
    assert_eq!(r.data["ruleName"], "all-in-one");

    // 未命中 -> pass
    assert_eq!(
        m.check("d", &t, &peer("11.11.11.11"), &ctx()).action,
        PeerAction::NoAction
    );
}

/// 无备注的规则：上游注释成分是**空字符串**（非 null），因此渲染为空，
/// `MODULE_IBL_COMMENT_UNKNOWN`（"未提供"）在实际路径中不可达。
#[test]
fn module_renders_empty_comment_when_rule_has_no_comment() {
    let (entries, _) = pbh_core::modules::ip_rule_list::parse_rule_list("10.10.0.0/16\n");
    let m = module(entries);
    let r = m.check("d", &torrent(), &peer("10.10.1.1"), &ctx());
    let tr = Translator::embedded();
    assert_eq!(
        tr.render(r.reason_key.as_ref().unwrap(), "zh_cn"),
        "匹配 IP黑名单订阅 规则: all-in-one, IP 地址: 10.10.1.1, 备注: "
    );
    // 上游文案键本身仍可用
    assert_eq!(
        tr.render(
            &pbh_core::i18n::TranslationComponent::new("MODULE_IBL_COMMENT_UNKNOWN"),
            "zh_cn"
        ),
        "未提供"
    );
}

#[test]
fn module_skips_handshaking_peers_and_prefers_longest_prefix_comment() {
    let (entries, _) = pbh_core::modules::ip_rule_list::parse_rule_list(
        "10.0.0.0/8 # 大网段\n10.10.0.0/16 # 精确网段\n",
    );
    let m = module(entries);

    let mut hs = peer("10.10.1.1");
    hs.up_speed = 0;
    hs.dl_speed = 0;
    let r = m.check("d", &torrent(), &hs, &ctx());
    assert_eq!(r.reason_key.as_ref().unwrap().key, "Peer handshaking");

    // 重叠网段取最长前缀（最精确）的注释
    let r = m.check("d", &torrent(), &peer("10.10.1.1"), &ctx());
    let tr = Translator::embedded();
    assert!(tr
        .render(r.reason_key.as_ref().unwrap(), "zh_cn")
        .contains("精确网段"));
}

#[test]
fn module_updates_and_removes_subscriptions() {
    let m = IpRuleListModule::new(1000);
    let t = torrent();
    assert_eq!(
        m.check("d", &t, &peer("1.2.3.4"), &ctx()).action,
        PeerAction::NoAction
    );

    let (entries, _) = pbh_core::modules::ip_rule_list::parse_rule_list("1.2.3.0/24\n");
    assert!(m.set_subscription(RuleSubscription {
        rule_id: "sub-a".into(),
        name: "sub-a".into(),
        entries,
    }));
    assert_eq!(
        m.check("d", &t, &peer("1.2.3.4"), &ctx()).action,
        PeerAction::Ban
    );
    assert_eq!(m.subscription_count(), 1);

    // 同名订阅更新（不是新增）
    let (entries2, _) = pbh_core::modules::ip_rule_list::parse_rule_list("5.6.7.0/24\n");
    assert!(!m.set_subscription(RuleSubscription {
        rule_id: "sub-a".into(),
        name: "sub-a".into(),
        entries: entries2,
    }));
    assert_eq!(m.subscription_count(), 1);
    assert_eq!(
        m.check("d", &t, &peer("1.2.3.4"), &ctx()).action,
        PeerAction::NoAction
    );
    assert_eq!(
        m.check("d", &t, &peer("5.6.7.8"), &ctx()).action,
        PeerAction::Ban
    );

    m.remove_subscription("sub-a");
    assert_eq!(m.subscription_count(), 0);
    assert_eq!(
        m.check("d", &t, &peer("5.6.7.8"), &ctx()).action,
        PeerAction::NoAction
    );
}

/// 订阅更新决策（sha256 比对 + 缓存回退）。
#[test]
fn update_plan_follows_upstream_hash_and_cache_rules() {
    use pbh_core::modules::ip_rule_list::{plan_rule_update, RuleUpdatePlan};

    // 远程内容与缓存不同 -> 解析并写回缓存
    assert_eq!(
        plan_rule_update(true, true, true, true),
        RuleUpdatePlan::ParseRemote { write_cache: true }
    );
    // 远程与缓存一致但内存未加载 -> 解析远程（不写盘）
    assert_eq!(
        plan_rule_update(true, false, true, false),
        RuleUpdatePlan::ParseRemote { write_cache: false }
    );
    // 远程与缓存一致且已加载 -> 无动作
    assert_eq!(
        plan_rule_update(true, false, true, true),
        RuleUpdatePlan::NoAction
    );
    // 远程失败但存在缓存且未加载 -> 用缓存
    assert_eq!(
        plan_rule_update(false, false, true, false),
        RuleUpdatePlan::ParseCache
    );
    // 远程失败且无缓存 / 已加载 -> 无动作
    assert_eq!(
        plan_rule_update(false, false, false, false),
        RuleUpdatePlan::NoAction
    );
    assert_eq!(
        plan_rule_update(false, false, true, true),
        RuleUpdatePlan::NoAction
    );
}

#[test]
fn ipv6_entries_are_supported() {
    let (entries, _) =
        pbh_core::modules::ip_rule_list::parse_rule_list("2001:db8::/32 # v6 网段\n");
    let m = module(entries);
    let t = torrent();
    assert_eq!(
        m.check("d", &t, &peer("2001:db8:1::1"), &ctx()).action,
        PeerAction::Ban
    );
    assert_eq!(
        m.check("d", &t, &peer("2001:db9::1"), &ctx()).action,
        PeerAction::NoAction
    );
}

// ---------- RuleIndex 前缀长度边界回归 ----------
//
// 回归背景：`RuleIndex::build` 曾用 `!0u32 >> prefix` / `!0u128 >> prefix` 计算网段的结束地址，
// 而 Rust 的移位在「移位量 == 位宽」时 panic（debug 下 `attempt to shift right with overflow`，
// release 下为未定义但通常按位宽取模）——即 `/32` IPv4 与 `/128` IPv6 规则会让整条订阅加载崩溃。
// 现改为 `checked_shr(...).unwrap_or(0)`（`/32`、`/128` 无主机位 ⇒ 掩码为 0）。
// 下列用例锁定：两种极值前缀都能构建并精确命中，且不 panic。

fn comment_of(m: &IpRuleListModule, ip: &str) -> String {
    let r = m.check("d", &torrent(), &peer(ip), &ctx());
    assert_eq!(r.action, PeerAction::Ban, "{ip} 应当命中并封禁");
    Translator::embedded().render(r.reason_key.as_ref().unwrap(), "zh_cn")
}

/// `/32`：无主机位（旧实现此处 panic），必须只匹配该主机地址本身。
#[test]
fn host_prefix_32_builds_and_matches_without_shift_overflow() {
    let (entries, _) = pbh_core::modules::ip_rule_list::parse_rule_list("1.2.3.4/32 # 单机\n");
    assert_eq!(entries[0].net.to_string(), "1.2.3.4/32");
    let m = module(entries);
    let t = torrent();

    assert_eq!(
        m.check("d", &t, &peer("1.2.3.4"), &ctx()).action,
        PeerAction::Ban
    );
    assert!(comment_of(&m, "1.2.3.4").contains("单机"));
    // 相邻地址不属于 /32 网段
    assert_eq!(
        m.check("d", &t, &peer("1.2.3.5"), &ctx()).action,
        PeerAction::NoAction
    );
    assert_eq!(
        m.check("d", &t, &peer("1.2.3.3"), &ctx()).action,
        PeerAction::NoAction
    );

    // DAT 单地址区间同样退化为 `/32`
    let (single, _) = pbh_core::modules::ip_rule_list::parse_rule_list("1.2.3.4,1.2.3.4,100,x\n");
    assert_eq!(single[0].net.to_string(), "1.2.3.4/32");
    let m2 = module(single);
    assert_eq!(
        m2.check("d", &t, &peer("1.2.3.4"), &ctx()).action,
        PeerAction::Ban
    );
}

/// `/128`：同为「移位量 == 位宽」的极值（旧实现 panic）。
#[test]
fn host_prefix_128_builds_and_matches_without_shift_overflow() {
    let (entries, _) =
        pbh_core::modules::ip_rule_list::parse_rule_list("2001:db8::dead:beef # 单机v6\n");
    assert_eq!(entries[0].net.to_string(), "2001:db8::dead:beef/128");
    let m = module(entries);
    let t = torrent();

    assert_eq!(
        m.check("d", &t, &peer("2001:db8::dead:beef"), &ctx())
            .action,
        PeerAction::Ban
    );
    assert!(comment_of(&m, "2001:db8::dead:beef").contains("单机v6"));
    // 同一 /64 内的相邻地址不属于 /128 网段
    assert_eq!(
        m.check("d", &t, &peer("2001:db8::dead:bee0"), &ctx())
            .action,
        PeerAction::NoAction
    );
}

/// `/0`：掩码为主机位全 1（`start | !0`），覆盖该地址族的全部地址。
#[test]
fn zero_prefix_covers_the_whole_address_family() {
    let (entries, _) =
        pbh_core::modules::ip_rule_list::parse_rule_list("0.0.0.0/0 # 全 v4\n::/0 # 全 v6\n");
    assert_eq!(entries[0].net.to_string(), "0.0.0.0/0");
    assert_eq!(entries[1].net.to_string(), "::/0");
    let m = module(entries);
    let t = torrent();

    // 两个端点都必须命中（`end` 计算错误时首/末地址最容易漏）
    for ip in ["0.0.0.0", "1.2.3.4", "255.255.255.255"] {
        assert_eq!(
            m.check("d", &t, &peer(ip), &ctx()).action,
            PeerAction::Ban,
            "{ip}"
        );
    }
    for ip in [
        "::",
        "2001:db8::1",
        "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
    ] {
        assert_eq!(
            m.check("d", &t, &peer(ip), &ctx()).action,
            PeerAction::Ban,
            "{ip}"
        );
    }
    // `/0` 的备注仍可取回
    assert!(comment_of(&m, "8.8.8.8").contains("全 v4"));
    assert!(comment_of(&m, "2001:db8::1").contains("全 v6"));

    // IPv4 `/0` 不影响 IPv6（各自独立分桶）
    let (v4_only, _) = pbh_core::modules::ip_rule_list::parse_rule_list("0.0.0.0/0 # 仅 v4\n");
    let m4 = module(v4_only);
    assert_eq!(
        m4.check("d", &t, &peer("1.2.3.4"), &ctx()).action,
        PeerAction::Ban
    );
    assert_eq!(
        m4.check("d", &t, &peer("2001:db8::1"), &ctx()).action,
        PeerAction::NoAction
    );
}

/// 极值前缀与普通网段共存时，仍按**最长前缀**取备注（`/32` 胜过 `/24` 胜过 `/8`）。
#[test]
fn longest_prefix_wins_with_extreme_prefix_lengths() {
    let (entries, _) = pbh_core::modules::ip_rule_list::parse_rule_list(
        "10.0.0.0/8 # 大网段\n10.1.0.0/16 # 中网段\n10.1.2.3/32 # 精确主机\n2001:db8::/32 # v6 网段\n2001:db8::1/128 # v6 精确主机\n",
    );
    let m = module(entries);

    assert!(comment_of(&m, "10.1.2.3").contains("精确主机"));
    assert!(comment_of(&m, "10.1.2.4").contains("中网段"));
    assert!(comment_of(&m, "10.2.3.4").contains("大网段"));
    assert!(comment_of(&m, "2001:db8::1").contains("v6 精确主机"));
    assert!(comment_of(&m, "2001:db8::2").contains("v6 网段"));

    // 网段末地址不得越界命中（`/8` 的最后一位属于该网段，下一段不属于）
    let t = torrent();
    assert_eq!(
        m.check("d", &t, &peer("10.255.255.255"), &ctx()).action,
        PeerAction::Ban
    );
    assert_eq!(
        m.check("d", &t, &peer("11.0.0.0"), &ctx()).action,
        PeerAction::NoAction
    );
}
