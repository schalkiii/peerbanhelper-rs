//! 内存封禁表与「全量/增量下发」判定（对齐上游 `BanList` / `DownloaderServerImpl`）。
//!
//! 上游的封禁状态保存在内存 `BanList`（按 IP 索引）中，每个 ban wave 开始时先
//! `removeExpiredBans()` 解封到期条目，再把「新增 + 移除」交给下载器：
//! `removed` 非空时必须走全量 `setPreferences`，否则下载器侧会残留已解封的 IP。

use std::collections::HashMap;

/// 一条封禁记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BannedRecord {
    pub ip: String,
    /// 解封时间（epoch ms）；`0` 表示不自动解封
    pub unban_at_ms: i64,
    /// 命中模块的 configName
    pub module: String,
    /// 是否为「断开连接用」的临时封禁（PCB 快速测试）
    pub ban_for_disconnect: bool,
}

/// 内存封禁表。
#[derive(Debug, Default, Clone)]
pub struct BanList {
    entries: HashMap<String, BannedRecord>,
    /// 出现过重复封禁（上游 `needReApplyBanList`），需要在下一轮强制全量重放
    need_reapply: bool,
}

impl BanList {
    pub fn new() -> Self {
        Self::default()
    }

    /// 载入持久化记录（启动时从数据库恢复）。
    pub fn load<I: IntoIterator<Item = BannedRecord>>(&mut self, items: I) {
        for item in items {
            self.entries.insert(item.ip.clone(), item);
        }
    }

    /// 封禁一个地址；返回 `true` 表示该地址此前已在封禁表中（上游视为重复封禁，
    /// 且会置位 `needReApplyBanList`）。重复封禁以最后一次的时长为准。
    pub fn add(
        &mut self,
        ip: &str,
        unban_at_ms: i64,
        module: &str,
        ban_for_disconnect: bool,
    ) -> bool {
        let duplicate = self.entries.contains_key(ip);
        if duplicate {
            self.need_reapply = true;
        }
        self.entries.insert(
            ip.to_string(),
            BannedRecord {
                ip: ip.to_string(),
                unban_at_ms,
                module: module.to_string(),
                ban_for_disconnect,
            },
        );
        duplicate
    }

    pub fn contains(&self, ip: &str) -> bool {
        self.entries.contains_key(ip)
    }

    /// 遍历全部封禁记录（`auto-range-ban` 需要扫描已封禁地址）。
    pub fn iter(&self) -> impl Iterator<Item = (&String, &BannedRecord)> {
        self.entries.iter()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 移除到期条目，返回被解封的记录。
    ///
    /// 对齐 Java `OffsetDateTime.now().isAfter(unbanAt)`：**严格大于**才算过期。
    pub fn remove_expired(&mut self, now_ms: i64) -> Vec<BannedRecord> {
        let expired: Vec<String> = self
            .entries
            .values()
            .filter(|e| e.unban_at_ms > 0 && now_ms > e.unban_at_ms)
            .map(|e| e.ip.clone())
            .collect();
        expired.iter().filter_map(|ip| self.entries.remove(ip)).collect()
    }

    pub fn remove(&mut self, ip: &str) -> Option<BannedRecord> {
        self.entries.remove(ip)
    }

    /// 全量下发用的地址列表（排序以保证载荷可复现；上游使用 HashSet，顺序不保证）。
    pub fn keys_sorted(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.entries.keys().cloned().collect();
        keys.sort();
        keys
    }

    /// 是否出现过重复封禁（上游 `needReApplyBanList`），需要强制全量重放。
    pub fn need_reapply(&self) -> bool {
        self.need_reapply
    }

    /// 全量重放完成后清除标记（对齐上游 `needReApplyBanList.set(false)`）。
    pub fn clear_need_reapply(&mut self) {
        self.need_reapply = false;
    }

    /// 强制下一轮全量重放（web 手动封禁/解封后调用，
    /// 对齐上游手动操作后 `BanListManager.addBan` + 立即 `banWaveAsync` 的全量下发语义）。
    pub fn mark_reapply(&mut self) {
        self.need_reapply = true;
    }

    /// 清空封禁表（web `DELETE /api/bans` 的 `*` 语义；同时标记下一轮全量重放）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.need_reapply = true;
    }
}

/// 是否需要下发**全量**封禁列表。
///
/// 对齐 Java `AbstractQbittorrent.setBanList`：
/// ```text
/// if (removed != null && removed.isEmpty() && added != null && incrementBan && !applyFullList)
///     setBanListIncrement(added);
/// else
///     setBanListFull(fullList);
/// ```
pub fn needs_full_ban_list(removed_count: usize, increment_ban: bool, apply_full: bool) -> bool {
    apply_full || removed_count > 0 || !increment_ban
}
