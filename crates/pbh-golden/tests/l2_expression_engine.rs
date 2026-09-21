//! [GOLDEN] `expression-engine` 模块契约（对齐上游 `ExpressionRule` / `ScriptEngineManager.handleResult`）。
//!
//! 关键行为（必须稳定，任意脚本改动不得改变）：
//! 1. 脚本目录为空 / 无有效脚本 → 恒 `pass()`，不改变任何封禁决策；
//! 2. 返回值映射：Boolean(true→BAN/false→pass)、Number(0→pass/1→BAN/2→SKIP)、
//!    String(空白→pass、`@`开头→SKIP 且 reason 为原文、其它→BAN)；
//! 3. 多条脚本聚合：SKIP 优先（短路），其次 BAN；
//! 4. 脚本可读取上游注入的 `peer` / `torrent` 字段。

use pbh_core::module::{CheckContext, PeerAction, RuleModule};
use pbh_core::modules::ExpressionEngine;
use pbh_core::{PeerData, TorrentData};
use std::fs;

fn sample_peer() -> PeerData {
    PeerData {
        client_name: Some("qBittorrent/4.3.9".to_string()),
        peer_id: Some("-qB4390-".to_string()),
        dl_speed: 0,
        downloaded: 0,
        up_speed: 0,
        uploaded: 0,
        progress: 0.5,
        flags: Some(String::new()),
        ip: "1.2.3.4".to_string(),
        port: 12345,
        raw_ip: "1.2.3.4:12345".to_string(),
        connection: None,
    }
}

fn sample_torrent() -> TorrentData {
    TorrentData {
        hash: "abc".to_string(),
        name: "t".to_string(),
        progress: 0.5,
        total_size: 1000,
        piece_size: 0,
        pieces_have: 0,
        completed_override: None,
        dlspeed: 0,
        upspeed: 0,
        is_private: Some(false),
    }
}

fn run_script(script: &str) -> PeerAction {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("pbh_golden_expr_{}_{}", std::process::id(), n));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("s.av"), script).unwrap();
    let m = ExpressionEngine::new(0, Some(dir.to_str().unwrap()));
    m.check("qb", &sample_torrent(), &sample_peer(), &CheckContext::default())
        .action
}

#[test]
fn empty_or_invalid_script_is_noop() {
    let dir = std::env::temp_dir().join(format!("pbh_golden_expr_empty_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let m = ExpressionEngine::new(0, Some(dir.to_str().unwrap()));
    assert_eq!(
        m.check("qb", &sample_torrent(), &sample_peer(), &CheckContext::default())
            .action,
        PeerAction::NoAction
    );

    // 编译失败的脚本被跳过 → 仍无动作
    let dir2 = std::env::temp_dir().join(format!("pbh_golden_expr_bad_{}", std::process::id()));
    fs::create_dir_all(&dir2).unwrap();
    fs::write(dir2.join("x.av"), "let x = ;").unwrap();
    let m2 = ExpressionEngine::new(0, Some(dir2.to_str().unwrap()));
    assert_eq!(
        m2.check("qb", &sample_torrent(), &sample_peer(), &CheckContext::default())
            .action,
        PeerAction::NoAction
    );
}

#[test]
fn boolean_return_semantics() {
    assert_eq!(run_script("true"), PeerAction::Ban);
    assert_eq!(run_script("false"), PeerAction::NoAction);
}

#[test]
fn number_return_semantics() {
    assert_eq!(run_script("0"), PeerAction::NoAction);
    assert_eq!(run_script("1"), PeerAction::Ban);
    assert_eq!(run_script("2"), PeerAction::Skip);
    // 其它数值视为无效 → pass
    assert_eq!(run_script("3"), PeerAction::NoAction);
}

#[test]
fn string_return_semantics() {
    assert_eq!(run_script("\"\""), PeerAction::NoAction);
    assert_eq!(run_script("\"hello\""), PeerAction::Ban);
    let dir = std::env::temp_dir().join(format!("pbh_golden_expr_at_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("s.av"), "\"@my-custom-reason\"").unwrap();
    let m = ExpressionEngine::new(0, Some(dir.to_str().unwrap()));
    let r = m.check("qb", &sample_torrent(), &sample_peer(), &CheckContext::default());
    assert_eq!(r.action, PeerAction::Skip);
    assert_eq!(r.reason_key.as_ref().unwrap().key, "my-custom-reason");
}

#[test]
fn skip_takes_priority_over_ban() {
    let dir = std::env::temp_dir().join(format!("pbh_golden_expr_agg_{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("ban.av"), "1").unwrap();
    fs::write(dir.join("skip.av"), "2").unwrap();
    let m = ExpressionEngine::new(0, Some(dir.to_str().unwrap()));
    assert_eq!(
        m.check("qb", &sample_torrent(), &sample_peer(), &CheckContext::default())
            .action,
        PeerAction::Skip
    );
}

#[test]
fn script_reads_peer_fields() {
    // 验证上游注入的 peer / torrent 字段对脚本可见
    assert_eq!(
        run_script("if peer.ip == \"1.2.3.4\" && torrent.hash == \"abc\" { 1 } else { 0 }"),
        PeerAction::Ban
    );
}
