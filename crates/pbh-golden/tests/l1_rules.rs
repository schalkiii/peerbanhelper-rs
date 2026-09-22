//! L1：规则匹配器与默认规则集语义（SPEC 第 4 节 [GOLDEN]）。

use pbh_core::defaults::{DEFAULT_BANNED_CLIENT_NAME, DEFAULT_BANNED_PEER_ID};
use pbh_core::rule::{RuleSet, Verdict};
use pbh_core::Matcher;
use serde_json::Value;

fn matcher(json: &str) -> Matcher {
    let v: Value = serde_json::from_str(json).unwrap();
    Matcher::parse(&v).unwrap()
}

fn peer_id_rules() -> RuleSet {
    RuleSet::from_json_text(
        &DEFAULT_BANNED_PEER_ID
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    )
    .unwrap()
}
fn client_rules() -> RuleSet {
    RuleSet::from_json_text(
        &DEFAULT_BANNED_CLIENT_NAME
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    )
    .unwrap()
}

#[test]
fn default_peer_id_rules_catch_known_leechers() {
    let rs = peer_id_rules();
    for id in [
        "-hp001-2.3",
        "-Xm0001-abc",
        "-dt001-",
        "-SD000-",
        "-qD000-",
        "-BN00-",
        "-DL00-",
        "-TS00-",
        "-FG00-",
        "-TT00-",
        "-NX00-",
    ] {
        assert!(rs.r#match(Some(id)).hit, "should ban peer id {id}");
    }
    // CONTAINS 规则
    assert!(rs.r#match(Some("xx-rn0.0.0-yy")).hit);
    assert!(rs.r#match(Some("cacao-client")).hit);
    // EQUALS（大小写不敏感）
    assert!(rs.r#match(Some("Unknown")).hit);
    assert!(rs.r#match(Some("未知")).hit);
    // 正常 qB peer id 不应命中
    assert!(!rs.r#match(Some("-qB5020-abcdefghijkl")).hit);
    assert!(!rs.r#match(Some("-TR3000-xxxxxxxx")).hit);
}

#[test]
fn default_client_name_rules() {
    let rs = client_rules();
    for c in [
        "hp/torrent/1.0",
        "hp 1.0",
        "dt/torrent",
        "xm 2.0",
        "taipei-torrent/3",
        "xfplay/9",
        "Unknown [something]",
        "Gopeed bt-exp",
        "rain 0.0.0 client",
        "gopeed dev build",
        "StellarPlayer/2",
        "SP something",
        "flashget x",
        "tudou dl",
        "TorrentStorm",
        "QQDownload",
        "qbittorrent/3.3.15",
        "github.com/thank423/trafficconsume",
        "\u{07AD}__前缀后缀",
    ] {
        assert!(rs.r#match(Some(c)).hit, "should ban client {c}");
    }
    assert!(!rs.r#match(Some("qBittorrent v5.0.2")).hit);
    assert!(!rs.r#match(Some("Transmission 4.0.4")).hit);
}

#[test]
fn false_short_circuits() {
    // 命中 CONTAINS -hp，但显式 FALSE 规则优先 -> 不封禁
    let rules = vec![
        matcher(r#"{"method":"CONTAINS","content":"-hp"}"#),
        matcher(r#"{"method":"EQUALS","content":"-hp-whitelist","hit":"FALSE","miss":"DEFAULT"}"#),
    ];
    // 普通 -hp 仍封禁（第二条 EQUALS miss=DEFAULT）
    assert!(RuleSet::new(rules).r#match(Some("-hp001")).hit);
    let rules2 = vec![
        matcher(r#"{"method":"CONTAINS","content":"-hp"}"#),
        matcher(r#"{"method":"STARTS_WITH","content":"-hp","hit":"FALSE","miss":"DEFAULT"}"#),
    ];
    // 第二条对所有 -hp 开头判 FALSE -> 短路不封
    assert!(!RuleSet::new(rules2).r#match(Some("-hp001")).hit);
}

#[test]
fn regex_full_match_semantics() {
    // Java matches() 整段匹配
    let m = matcher(r#"{"method":"REGEX","content":"-hp.*"}"#);
    assert_eq!(m.matches(Some("-hp001")), Verdict::True);
    assert_eq!(m.matches(Some("x-hp001")), Verdict::Default);
}

#[test]
fn starts_contains_are_case_insensitive_equals_ignorecase() {
    assert_eq!(
        matcher(r#"{"method":"STARTS_WITH","content":"-HP"}"#).matches(Some("-hp00")),
        Verdict::True
    );
    assert_eq!(
        matcher(r#"{"method":"CONTAINS","content":"CACAO"}"#).matches(Some("xxcacaoyy")),
        Verdict::True
    );
    assert_eq!(
        matcher(r#"{"method":"EQUALS","content":"unknown"}"#).matches(Some("Unknown")),
        Verdict::True
    );
}

#[test]
fn null_and_empty_content() {
    // null 归一为空串，不应命中 CONTAINS
    assert_eq!(
        matcher(r#"{"method":"CONTAINS","content":"x"}"#).matches(None),
        Verdict::Default
    );
}

/// 上游 `profile.yml`（v9.5.1）的默认规则集：Rust 常量必须**逐字、逐序**一致。
/// 顺序会影响 `matchRule` 上报的命中规则；method 差异会直接改变封禁决策。
#[test]
fn default_rule_sets_match_upstream_profile_yml() {
    let expected_peer_id: Vec<&str> = vec![
        r#"{"method":"STARTS_WITH","content":"-hp"}"#,
        r#"{"method":"STARTS_WITH","content":"-xm"}"#,
        r#"{"method":"STARTS_WITH","content":"-dt"}"#,
        r#"{"method":"CONTAINS","content":"-rn0.0.0"}"#,
        r#"{"method":"STARTS_WITH","content":"-sd"}"#,
        r#"{"method":"STARTS_WITH","content":"-xf"}"#,
        r#"{"method":"STARTS_WITH","content":"-qd"}"#,
        r#"{"method":"STARTS_WITH","content":"-bn"}"#,
        r#"{"method":"STARTS_WITH","content":"-dl"}"#,
        r#"{"method":"STARTS_WITH","content":"-ts"}"#,
        r#"{"method":"STARTS_WITH","content":"-fg"}"#,
        r#"{"method":"STARTS_WITH","content":"-tt"}"#,
        r#"{"method":"STARTS_WITH","content":"-nx"}"#,
        r#"{"method":"CONTAINS","content":"cacao"}"#,
        r#"{"method":"EQUALS","content":"Unknown"}"#,
        r#"{"method":"EQUALS","content":"未知"}"#,
    ];
    assert_eq!(
        DEFAULT_BANNED_PEER_ID,
        expected_peer_id.as_slice(),
        "banned-peer-id 必须与上游 profile.yml 逐字逐序一致"
    );

    // 注意：`` 为上游 profile.yml 第 92 行的真实字节 \xde\xad__（U+07AD 后跟两个下划线）
    let expected_client_name: Vec<&str> = vec![
        r#"{"method":"STARTS_WITH","content":"hp/torrent"}"#,
        r#"{"method":"STARTS_WITH","content":"hp "}"#,
        r#"{"method":"STARTS_WITH","content":"dt/torrent"}"#,
        r#"{"method":"STARTS_WITH","content":"dt "}"#,
        r#"{"method":"STARTS_WITH","content":"xm/torrent"}"#,
        r#"{"method":"STARTS_WITH","content":"xm "}"#,
        r#"{"method":"STARTS_WITH","content":"taipei-torrent"}"#,
        r#"{"method":"CONTAINS","content":"rain 0.0.0"}"#,
        r#"{"method":"CONTAINS","content":"gopeed dev"}"#,
        r#"{"method":"STARTS_WITH","content":"xfplay"}"#,
        r#"{"method":"CONTAINS","content":"StellarPlayer"}"#,
        r#"{"method":"CONTAINS","content":"SP "}"#,
        r#"{"method":"CONTAINS","content":"flashget"}"#,
        r#"{"method":"CONTAINS","content":"tudou"}"#,
        r#"{"method":"CONTAINS","content":"torrentstorm"}"#,
        r#"{"method":"CONTAINS","content":"qqdownload"}"#,
        r#"{"method":"STARTS_WITH","content":"qbittorrent/3.3.15"}"#,
        r#"{"method":"STARTS_WITH","content":"github.com/thank423/trafficconsume"}"#,
        "{\"method\":\"STARTS_WITH\",\"content\":\"\u{07AD}__\"}",
        r#"{"method":"STARTS_WITH","content":"Unknown ["}"#,
        r#"{"method":"STARTS_WITH","content":"Gopeed bt-exp"}"#,
    ];
    assert_eq!(
        DEFAULT_BANNED_CLIENT_NAME,
        expected_client_name.as_slice(),
        "banned-client-name 必须与上游 profile.yml 逐字逐序一致"
    );
}

/// client-name 中 `qbittorrent/3.3.15` 与 `github.com/...` 在上游是 **STARTS_WITH**，
/// 早期 Rust 实现误写成 CONTAINS，会把「前缀不是 qbittorrent」的正常客户端也封禁。
#[test]
fn legacy_client_rules_are_prefix_matches() {
    let rs = client_rules();
    assert!(rs.r#match(Some("qbittorrent/3.3.15 libtorrent/1.2")).hit);
    assert!(
        !rs.r#match(Some("Rename qbittorrent/3.3.15")).hit,
        "STARTS_WITH 语义：非前缀匹配不得命中"
    );
    assert!(rs.r#match(Some("\u{07AD}__client")).hit);
    assert!(
        !rs.r#match(Some("x\u{07AD}__client")).hit,
        "0xDEAD 规则是 STARTS_WITH"
    );
}

/// Java `RuleParser.matchRule`：每命中一条 TRUE 都会覆盖记录，因此**最后一条** TRUE 规则胜出。
/// Rust 早期实现只记录首个 TRUE，会导致上报的命中规则与 Java 不一致。
#[test]
fn match_rule_records_the_last_matching_true_rule() {
    let rules = vec![
        matcher(r#"{"method":"CONTAINS","content":"-hp"}"#),
        matcher(r#"{"method":"CONTAINS","content":"-xo"}"#),
    ];
    let r = RuleSet::new(rules).r#match(Some("-hp001-xo"));
    assert!(r.hit);
    assert_eq!(r.index, 1, "Java 记录最后一条命中 TRUE 的规则");
}

/// Java `StringLengthMatcher` 使用 `String.length()`（UTF-16 码元数）。
/// Rust `chars().count()` 统计 Unicode 标量值，遇到增补平面字符（如 emoji）会不一致。
#[test]
fn length_matches_java_utf16_length() {
    let two = matcher(r#"{"method":"LENGTH","min":2,"max":2}"#);
    assert_eq!(
        two.matches(Some("💩")),
        Verdict::True,
        "emoji 在 Java 中长度为 2"
    );
    let one = matcher(r#"{"method":"LENGTH","min":1,"max":1}"#);
    assert_eq!(
        one.matches(Some("💩")),
        Verdict::Default,
        "emoji 不应被算作长度 1"
    );
    // 中文原样按码元数：'蕲' 在 UTF-16 中为 1 个码元
    let one_cjk = matcher(r#"{"method":"LENGTH","min":1,"max":1}"#);
    assert_eq!(one_cjk.matches(Some("蕲")), Verdict::True);
}

/// Java 的 STARTS_WITH/ENDS_WITH/CONTAINS 使用 `toLowerCase(Locale.ROOT)`（Unicode 感知），
/// EQUALS 使用 `String.equalsIgnoreCase`（同样是 Unicode 感知）。
/// Rust 早期实现用 `to_ascii_lowercase` / `eq_ignore_ascii_case`，会漏判非 ASCII 大小写。
#[test]
fn case_folding_is_unicode_aware_like_java() {
    assert_eq!(
        matcher(r#"{"method":"STARTS_WITH","content":"Ä"}"#).matches(Some("äbc")),
        Verdict::True
    );
    assert_eq!(
        matcher(r#"{"method":"ENDS_WITH","content":"Ä"}"#).matches(Some("bcä")),
        Verdict::True
    );
    assert_eq!(
        matcher(r#"{"method":"CONTAINS","content":"Ä"}"#).matches(Some("xxäyy")),
        Verdict::True
    );
    assert_eq!(
        matcher(r#"{"method":"EQUALS","content":"Äbc"}"#).matches(Some("äBC")),
        Verdict::True
    );
}
