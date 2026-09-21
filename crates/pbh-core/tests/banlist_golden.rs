//! 内存封禁表与「全量/增量下发」判定的黄金测试。
//!
//! 对齐上游：
//! - `DownloaderServerImpl.removeExpiredBans()`：`now.isAfter(unbanAt)` 才解封（严格大于）。
//! - `DownloaderServerImpl.banPeer()`：重复封禁同一地址时标记需要全量重放。
//! - `AbstractQbittorrent.setBanList()`：
//!   `removed 为空 && increment-ban && !applyFullList` 才走增量，否则全量。

use pbh_core::banlist::{needs_full_ban_list, BanList, BannedRecord};

fn record(ip: &str, unban_at_ms: i64) -> BannedRecord {
    BannedRecord {
        ip: ip.to_string(),
        unban_at_ms,
        module: "peer-id-blacklist".to_string(),
        ban_for_disconnect: false,
    }
}

#[test]
fn expired_entries_are_unbanned_only_strictly_after_unban_at() {
    let mut list = BanList::new();
    assert!(!list.add("1.1.1.1", 1_000, "peer-id-blacklist", false), "首次封禁不是重复");
    assert!(!list.add("2.2.2.2", 5_000, "peer-id-blacklist", false));

    // now == unbanAt：Java `isAfter` 为 false -> 仍处于封禁中
    assert!(list.remove_expired(1_000).is_empty(), "等于到期时间不算过期");
    assert_eq!(list.len(), 2);

    let expired = list.remove_expired(1_001);
    assert_eq!(expired.len(), 1);
    assert_eq!(expired[0].ip, "1.1.1.1");
    assert_eq!(list.len(), 1);
    assert!(list.contains("2.2.2.2"));
    assert!(!list.contains("1.1.1.1"));
}

#[test]
fn duplicate_ban_is_reported_so_the_wave_can_replay_the_full_list() {
    let mut list = BanList::new();
    assert!(!list.add("1.1.1.1", 1_000, "peer-id-blacklist", false));
    assert!(list.add("1.1.1.1", 2_000, "peer-id-blacklist", false), "重复封禁需被识别");
    // 以最后一次封禁时长为准
    let expired = list.remove_expired(1_500);
    assert!(expired.is_empty(), "重复封禁应刷新解封时间");
    assert_eq!(list.len(), 1);
}

#[test]
fn full_list_is_applied_whenever_anything_was_removed() {
    // 有解封项 -> 必须全量（否则下载器侧仍留着已解封的 IP）
    assert!(needs_full_ban_list(1, true, false));
    // 关闭增量封禁 -> 全量
    assert!(needs_full_ban_list(0, false, false));
    // 显式要求全量重放 -> 全量
    assert!(needs_full_ban_list(0, true, true));
    // 仅新增且启用增量 -> 增量
    assert!(!needs_full_ban_list(0, true, false));
}

#[test]
fn keys_are_deterministic_for_full_banlist_payloads() {
    let mut list = BanList::new();
    list.load(vec![record("9.9.9.9", 100), record("1.1.1.1", 100), record("5.5.5.5", 100)]);
    assert_eq!(list.keys_sorted(), vec!["1.1.1.1", "5.5.5.5", "9.9.9.9"]);
}
