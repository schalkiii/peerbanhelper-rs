//! 内存封禁表与「全量/增量下发」判定（对齐上游 `BanList` / `DownloaderServerImpl`）。
//!
//! 上游的封禁状态保存在内存 `BanList`（按 IP 索引）中，每个 ban wave 开始时先
//! `removeExpiredBans()` 解封到期条目，再把「新增 + 移除」交给下载器：
//! `removed` 非空时必须走全量 `setPreferences`，否则下载器侧会残留已解封的 IP。
//!
//! 每条记录携带完整的 [`BanMetadata`]（上游 `BanList = Map<IPAddress, BanMetadata>`）：
//! 它既落 `banlist` 表（`BanListService.saveBanList` 的 `metadata` 列），
//! 也供 Web 展示（`/api/bans` 的 `BanDTO`）与遗留协议上报（`legacy_ban_snapshot`）。

use crate::i18n::TranslationComponent;
use crate::model::{PeerData, TorrentData};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// 上游 `DownloaderBasicInfo`（封禁快照里的下载器信息）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloaderBasicInfo {
    pub id: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
}

/// 上游 `PeerAddress`（封禁快照里的地址）。
///
/// `teredo*` / `nat*` 字段属于地址翻译链路；本移植的翻译在 `remap` 层完成，
/// 这里保留字段并填「未翻译」形态（与上游未命中翻译时一致）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BannedPeerAddress {
    pub downloader_raw_ip: String,
    pub downloader_raw_port: u16,
    pub teredo_client_udp_port: u16,
    pub natted_client_port: u16,
    pub ip: String,
    pub port: u16,
    pub nat_translated: bool,
    pub teredo_translated: bool,
}

/// 上游 `PeerWrapper`（封禁快照里的 peer）。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BannedPeer {
    pub address: BannedPeerAddress,
    pub raw_ip: String,
    pub id: String,
    pub client_name: String,
    pub downloaded: i64,
    pub download_speed: i64,
    pub uploaded: i64,
    pub upload_speed: i64,
    pub progress: f64,
    pub flags: Option<String>,
}

impl BannedPeer {
    /// 由观测到的 [`PeerData`] 组装（对齐 `new PeerWrapper(peer)`）。
    pub fn from_peer(peer: &PeerData) -> Self {
        Self {
            address: BannedPeerAddress {
                downloader_raw_ip: peer.raw_ip.clone(),
                downloader_raw_port: peer.port,
                teredo_client_udp_port: 0,
                natted_client_port: 0,
                ip: peer.ip.clone(),
                port: peer.port,
                nat_translated: false,
                teredo_translated: false,
            },
            raw_ip: peer.raw_ip.clone(),
            id: peer.peer_id.clone().unwrap_or_default(),
            client_name: peer.client_name.clone().unwrap_or_default(),
            downloaded: peer.downloaded,
            download_speed: peer.dl_speed,
            uploaded: peer.uploaded,
            upload_speed: peer.up_speed,
            progress: peer.progress,
            flags: peer.flags.clone(),
        }
    }
}

/// 上游 `TorrentWrapper`（封禁快照里的种子）。
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BannedTorrent {
    /// 下载器内部 ID；本移植的 `TorrentData` 没有独立 ID，取 info hash（对齐上游多数适配器的做法）。
    pub id: String,
    pub size: i64,
    pub completed_size: i64,
    pub name: String,
    pub hash: String,
    pub private_torrent: bool,
    pub progress: f64,
    pub rt_upload_speed: i64,
    pub rt_download_speed: i64,
}

impl BannedTorrent {
    /// 由观测到的 [`TorrentData`] 组装（对齐 `new TorrentWrapper(torrent)`）。
    pub fn from_torrent(torrent: &TorrentData) -> Self {
        let completed_size = match torrent.completed_override {
            Some(value) if value > 0 => value,
            _ => ((torrent.total_size as f64) * torrent.progress.clamp(0.0, 1.0)) as i64,
        };
        Self {
            id: torrent.hash.clone(),
            size: torrent.total_size,
            completed_size,
            name: torrent.name.clone(),
            hash: torrent.hash.clone(),
            private_torrent: torrent.is_private.unwrap_or(false),
            progress: torrent.progress,
            rt_upload_speed: torrent.upspeed,
            rt_download_speed: torrent.dlspeed,
        }
    }
}

fn default_reverse_lookup() -> String {
    "N/A".to_string()
}

/// 上游 `BanMetadata`（`banlist.metadata` 列的 JSON 载体，`JsonUtil.tiny()` 语义）。
///
/// 字段名逐字对齐上游序列化结果（camelCase、时间为 epoch 毫秒数字、忽略 null 字段）；
/// 反序列化对缺字段宽容（`#[serde(default)]`），以兼容上游历史数据与本移植的精简数据。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BanMetadata {
    /// 上游 `CheckResult.moduleContext` 的 Java 类全名（见 [`crate::module::java_module_class`]）。
    pub context: String,
    /// `UUID.randomUUID().toString().replace("-", "")`。
    pub random_id: String,
    #[serde(rename = "banAt")]
    pub ban_at_ms: i64,
    #[serde(rename = "unbanAt")]
    pub unban_at_ms: i64,
    pub ban_for_disconnect: bool,
    pub exclude_from_report: bool,
    pub exclude_from_display: bool,
    pub rule: TranslationComponent,
    pub description: TranslationComponent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_data: Option<serde_json::Value>,
    pub downloader: DownloaderBasicInfo,
    pub torrent: BannedTorrent,
    pub peer: BannedPeer,
    #[serde(default = "default_reverse_lookup")]
    pub reverse_lookup: String,
}

impl Default for BanMetadata {
    fn default() -> Self {
        Self {
            context: String::new(),
            random_id: String::new(),
            ban_at_ms: 0,
            unban_at_ms: 0,
            ban_for_disconnect: false,
            exclude_from_report: false,
            exclude_from_display: false,
            rule: TranslationComponent::new(""),
            description: TranslationComponent::new(""),
            structured_data: None,
            downloader: DownloaderBasicInfo::default(),
            torrent: BannedTorrent::default(),
            peer: BannedPeer::default(),
            reverse_lookup: default_reverse_lookup(),
        }
    }
}

impl BanMetadata {
    /// 精简构造（手动封禁 / 老库迁移 / 缺字段数据兜底）。
    pub fn minimal(ip: &str, context: &str, ban_at_ms: i64, unban_at_ms: i64) -> Self {
        let mut peer = BannedPeer::default();
        peer.address.ip = ip.to_string();
        peer.address.port = 0;
        peer.raw_ip = ip.to_string();
        Self {
            context: context.to_string(),
            random_id: random_id(),
            ban_at_ms,
            unban_at_ms,
            rule: TranslationComponent::new(context),
            description: TranslationComponent::new(context),
            peer,
            ..Default::default()
        }
    }
}

/// 32 位十六进制随机 ID（对齐 `UUID.randomUUID().toString().replace("-", "")` 的形状）。
///
/// 用 `RandomState`（SipHash 随机密钥，每个实例由线程本地 RNG 播种）拼出 32 位 hex，
/// 避免为一次随机 ID 引入额外依赖。
pub fn random_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(32);
    for salt in 0..2u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(salt);
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

/// 一条封禁记录（内存封禁表的值）。
#[derive(Clone, Debug, PartialEq)]
pub struct BannedRecord {
    pub ip: String,
    /// 解封时间（epoch ms）；`0` 表示不自动解封
    pub unban_at_ms: i64,
    /// 命中模块的 configName（内部语义；展示/落库用 [`BannedRecord::metadata`] 的 `context`）
    pub module: String,
    /// 是否为「断开连接用」的临时封禁（PCB 快速测试）
    pub ban_for_disconnect: bool,
    /// 完整封禁元数据（上游 `BanMetadata`）。
    pub metadata: BanMetadata,
}

impl BannedRecord {
    /// 精简构造（无观测数据时：手动封禁、旧数据恢复）。
    pub fn minimal(ip: &str, unban_at_ms: i64, module: &str, ban_for_disconnect: bool) -> Self {
        Self {
            ip: ip.to_string(),
            unban_at_ms,
            module: module.to_string(),
            ban_for_disconnect,
            metadata: BanMetadata::minimal(ip, module, 0, unban_at_ms),
        }
    }
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

    /// 封禁一个地址（精简元数据）；返回 `true` 表示该地址此前已在封禁表中
    /// （上游视为重复封禁，且会置位 `needReApplyBanList`）。重复封禁以最后一次的时长为准。
    pub fn add(
        &mut self,
        ip: &str,
        unban_at_ms: i64,
        module: &str,
        ban_for_disconnect: bool,
    ) -> bool {
        self.add_record(BannedRecord::minimal(ip, unban_at_ms, module, ban_for_disconnect))
    }

    /// 写入一条完整记录（ban wave 落库路径：携带 peer/torrent/规则等完整 `BanMetadata`）。
    pub fn add_record(&mut self, record: BannedRecord) -> bool {
        let duplicate = self.entries.contains_key(&record.ip);
        if duplicate {
            self.need_reapply = true;
        }
        self.entries.insert(record.ip.clone(), record);
        duplicate
    }

    /// 全量快照（按 IP 排序，供落库/展示/上报）。
    pub fn records_sorted(&self) -> Vec<BannedRecord> {
        let mut records: Vec<BannedRecord> = self.entries.values().cloned().collect();
        records.sort_by(|a, b| a.ip.cmp(&b.ip));
        records
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 上游 `banlist.metadata` 的真实 JSON（字段取自 Java 版数据库）必须能解析，
    /// 且写回时保持上游形状（`banAt` 为 epoch 毫秒数字、camelCase 字段名）。
    #[test]
    fn upstream_ban_metadata_json_roundtrips() {
        let json = r#"{"context":"com.ghostchu.peerbanhelper.module.impl.rule.ProgressCheatBlocker",
"randomId":"4e79337903c24551ae0052cebd11c536","banAt":1789958922872,"unbanAt":1792550922872,
"banForDisconnect":false,"excludeFromReport":false,"excludeFromDisplay":false,
"rule":{"key":"PCB_RULE_PROGRESS_REWIND","params":[]},
"description":{"key":"MODULE_PCB_PEER_BAN_REWIND","params":["3.47%","6.31%"]},
"structuredData":{"type":"rewindProgress"},
"downloader":{"id":"bbe015c9-31bc-423b-b6b2-2d17c37a5113","name":"qBittorrent_a","type":"qBittorrent"},
"torrent":{"id":"84546ebb","size":11846597498,"completedSize":11848908800,"name":"示例种子",
"hash":"84546ebb","privateTorrent":false,"progress":1.0,"rtUploadSpeed":12732,"rtDownloadSpeed":0},
"peer":{"address":{"downloaderRawIp":"36.27.66.61:3097","downloaderRawPort":3097,
"teredoClientUdpPort":0,"nattedClientPort":0,"ip":"36.27.66.61","port":3097,
"natTranslated":false,"teredoTranslated":false},"rawIp":"36.27.66.61:3097","id":"-qB5220-",
"clientName":"qBittorrent/5.2.2","downloaded":0,"downloadSpeed":0,"uploaded":742413312,
"uploadSpeed":17911,"progress":0.034690264612436295,"flags":"U E"},"reverseLookup":"N/A"}"#;
        let metadata: BanMetadata = serde_json::from_str(json).expect("上游 JSON 可解析");
        assert_eq!(
            metadata.context,
            "com.ghostchu.peerbanhelper.module.impl.rule.ProgressCheatBlocker"
        );
        assert_eq!(metadata.ban_at_ms, 1_789_958_922_872);
        assert_eq!(metadata.rule.key, "PCB_RULE_PROGRESS_REWIND");
        assert_eq!(metadata.description.params.len(), 2);
        assert_eq!(metadata.peer.uploaded, 742_413_312);
        assert_eq!(metadata.peer.address.port, 3097);
        assert_eq!(metadata.torrent.size, 11_846_597_498);
        assert_eq!(metadata.downloader.kind, "qBittorrent");

        let text = serde_json::to_string(&metadata).unwrap();
        assert!(text.contains(r#""banAt":1789958922872"#), "{text}");
        assert!(text.contains(r#""privateTorrent":false"#), "{text}");
        assert!(text.contains(r#""reverseLookup":"N/A""#), "{text}");
    }

    /// 精简/迁移数据（缺 peer/torrent 明细、缺 params）也能解析，字段回落到默认值。
    #[test]
    fn minimal_ban_metadata_json_is_accepted() {
        let metadata: BanMetadata =
            serde_json::from_str(r#"{"context":"ip-address-blocker","banAt":1,"unbanAt":2}"#)
                .unwrap();
        assert_eq!(metadata.context, "ip-address-blocker");
        assert_eq!(metadata.unban_at_ms, 2);
        assert!(metadata.peer.id.is_empty());
        assert_eq!(metadata.reverse_lookup, "N/A");
        // `banForDisconnect` 缺省为 false（不是「未读」，因此显式断言默认值）
        assert!(!metadata.ban_for_disconnect);
    }
}
