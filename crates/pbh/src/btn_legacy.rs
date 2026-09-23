//! BTN 遗留协议（`min < 20`）上报快照的数据源桥接。
//!
//! 上游 `LegacyBtnAbilitySubmitPeers` / `LegacyBtnAbilitySubmitBans` 依赖
//! `DownloaderServer` 的内存数据（live peers / ban list）。本移植由 wave 在每轮
//! 拉取 peers 时写入 [`LivePeerMap`]，封禁快照直接读内存封禁表 [`BanList`]；
//! DB 批量数据（history / swarm / peer_records）仍委托 [`DbBtnSubmitSource`](pbh_db) 内层实现。

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex as StdMutex};

use pbh_core::banlist::BanList;
use pbh_core::btn_transport::{
    BtnBanHistoryRow, BtnLegacyBanRow, BtnLegacyPeerRow, BtnPeerHistoryRow, BtnSubmitSource,
    BtnSwarmHistoryRow,
};
use pbh_core::i18n::Translator;
use pbh_core::model::{PeerData, TorrentData};

/// 下载器 ID → 最近一轮观测到的 live peers。
///
/// wave 在每下载器轮次开始时清空该下载器的旧快照、每个 torrent 拉取成功后追加
/// （对齐上游 `DownloaderServer` 每轮覆盖 livePeers 的语义）。
pub type LivePeerMap = Arc<StdMutex<HashMap<String, Vec<BtnLegacyPeerRow>>>>;

pub fn new_live_peer_map() -> LivePeerMap {
    Arc::new(StdMutex::new(HashMap::new()))
}

fn non_empty(s: String) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// 由一次 `fetch_peers` 的结果构造遗留协议 peer 行
/// （对齐 `LegacyBtnPeer.from(TorrentWrapper, PeerWrapper)`： downloaded / uploaded /
/// progress 取自 peer，rt 速度 / size / 私有标记 / 下载器进度取自 torrent）。
pub fn peer_rows_from(torrent: &TorrentData, peers: &[PeerData]) -> Vec<BtnLegacyPeerRow> {
    peers
        .iter()
        .map(|peer| BtnLegacyPeerRow {
            ip_address: peer.ip.clone(),
            peer_port: peer.port,
            peer_id: non_empty(peer.peer_id.clone().unwrap_or_default()),
            client_name: non_empty(peer.client_name.clone().unwrap_or_default()),
            torrent_hash: torrent.hash.clone(),
            torrent_is_private: torrent.is_private.unwrap_or(false),
            torrent_size: torrent.total_size,
            downloaded: peer.downloaded,
            rt_download_speed: peer.dl_speed,
            uploaded: peer.uploaded,
            rt_upload_speed: peer.up_speed,
            peer_progress: peer.progress,
            downloader_progress: torrent.progress,
            peer_flag: peer.flags.clone().filter(|f| !f.is_empty()),
        })
        .collect()
}

/// 由内存封禁表构造遗留协议封禁行（对齐 `LegacyBtnAbilitySubmitBans.generateBans`）：
/// 跳过 `ban_for_disconnect` / `exclude_from_report`；按 peer 去重；
/// `btn_ban` = context 是否为 BTN 模块类名；`rule` = description 的本地化渲染
/// （对齐上游 `tl(DEF_LOCALE, meta.getDescription())`）。
///
/// `ban_unique_id` 用封禁元数据的 `random_id`：上游是 `sha256(metadata.toString())`，
/// Java `toString()` 无法逐字节复现，取同样唯一的随机 ID（语义一致：去重键）。
pub fn ban_rows_from_ban_list(
    ban_list: &BanList,
    translator: &Translator,
    locale: &str,
) -> Vec<BtnLegacyBanRow> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (_ip, record) in ban_list.iter() {
        let meta = &record.metadata;
        if meta.ban_for_disconnect || meta.exclude_from_report {
            continue;
        }
        let key = (
            meta.peer.address.ip.clone(),
            meta.peer.address.port,
            meta.torrent.hash.clone(),
        );
        if !seen.insert(key) {
            continue;
        }
        out.push(BtnLegacyBanRow {
            ban_at_ms: meta.ban_at_ms,
            ban_unique_id: meta.random_id.clone(),
            module: meta.context.clone(),
            rule: translator.render(&meta.description, locale),
            btn_ban: meta.context.ends_with(".BtnNetworkOnline"),
            peer: BtnLegacyPeerRow {
                ip_address: meta.peer.address.ip.clone(),
                peer_port: meta.peer.address.port,
                peer_id: non_empty(meta.peer.id.clone()),
                client_name: non_empty(meta.peer.client_name.clone()),
                torrent_hash: meta.torrent.hash.clone(),
                torrent_is_private: meta.torrent.private_torrent,
                torrent_size: meta.torrent.size,
                downloaded: meta.peer.downloaded,
                rt_download_speed: meta.torrent.rt_download_speed,
                uploaded: meta.peer.uploaded,
                rt_upload_speed: meta.torrent.rt_upload_speed,
                peer_progress: meta.peer.progress,
                downloader_progress: meta.torrent.progress,
                peer_flag: meta.peer.flags.clone(),
            },
            structured_data: meta.structured_data.clone(),
        });
    }
    out
}

/// 组合数据源：DB 批量数据委托内层（`DbBtnSubmitSource`），
/// 遗留协议快照来自内存（live peers 注册表 + 内存封禁表）。
pub struct LegacyAwareSubmitSource {
    inner: Arc<dyn BtnSubmitSource>,
    live_peers: LivePeerMap,
    ban_list: Arc<StdMutex<BanList>>,
    translator: Arc<Translator>,
    locale: String,
}

impl std::fmt::Debug for LegacyAwareSubmitSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LegacyAwareSubmitSource")
            .field("locale", &self.locale)
            .finish_non_exhaustive()
    }
}

impl LegacyAwareSubmitSource {
    pub fn new(
        inner: Arc<dyn BtnSubmitSource>,
        live_peers: LivePeerMap,
        ban_list: Arc<StdMutex<BanList>>,
        translator: Arc<Translator>,
        locale: impl Into<String>,
    ) -> Self {
        Self {
            inner,
            live_peers,
            ban_list,
            translator,
            locale: locale.into(),
        }
    }
}

impl BtnSubmitSource for LegacyAwareSubmitSource {
    fn batch_ban_history(&self, cursor_id: i64, limit: usize) -> Vec<BtnBanHistoryRow> {
        self.inner.batch_ban_history(cursor_id, limit)
    }

    fn batch_swarm_history(
        &self,
        last_time_seen_ms: i64,
        id_after: i64,
        limit: usize,
    ) -> Vec<BtnSwarmHistoryRow> {
        self.inner
            .batch_swarm_history(last_time_seen_ms, id_after, limit)
    }

    fn batch_peer_history(&self, last_time_seen_ms: i64, limit: usize) -> Vec<BtnPeerHistoryRow> {
        self.inner.batch_peer_history(last_time_seen_ms, limit)
    }

    fn legacy_peer_snapshot(&self) -> Vec<BtnLegacyPeerRow> {
        let map = self.live_peers.lock().unwrap_or_else(|e| e.into_inner());
        map.values().flatten().cloned().collect()
    }

    fn legacy_ban_snapshot(&self) -> Vec<BtnLegacyBanRow> {
        let list = self.ban_list.lock().unwrap_or_else(|e| e.into_inner());
        ban_rows_from_ban_list(&list, &self.translator, &self.locale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbh_core::banlist::{BanMetadata, BannedRecord};

    fn torrent() -> TorrentData {
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
            is_private: Some(true),
        }
    }

    fn peer(ip: &str, port: u16) -> PeerData {
        PeerData {
            client_name: Some("qB/4.3".to_string()),
            peer_id: Some("-qB4390-".to_string()),
            dl_speed: 10,
            downloaded: 100,
            up_speed: 20,
            uploaded: 200,
            progress: 0.4,
            flags: Some("uT".to_string()),
            ip: ip.to_string(),
            port,
            raw_ip: format!("{ip}:{port}"),
            connection: None,
        }
    }

    #[test]
    fn peer_rows_map_peer_and_torrent_fields() {
        let rows = peer_rows_from(&torrent(), &[peer("1.2.3.4", 51413)]);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.ip_address, "1.2.3.4");
        assert_eq!(row.peer_port, 51413);
        assert_eq!(row.torrent_hash, "abc");
        assert!(row.torrent_is_private);
        assert_eq!(row.torrent_size, 1000);
        assert_eq!(row.downloaded, 100);
        assert_eq!(row.rt_download_speed, 10);
        assert_eq!(row.uploaded, 200);
        assert_eq!(row.rt_upload_speed, 20);
        assert_eq!(row.peer_progress, 0.4);
        assert_eq!(row.downloader_progress, 0.5);
        assert_eq!(row.peer_flag.as_deref(), Some("uT"));
    }

    #[test]
    fn ban_rows_skip_disconnect_and_dedupe() {
        let mut meta = BanMetadata::minimal("1.2.3.4", "ctx", 100, 200);
        meta.torrent.hash = "abc".to_string();
        let mut meta2 = BanMetadata::minimal("5.6.7.8", "ctx", 300, 400);
        meta2.ban_for_disconnect = true;
        let record = |ip: &str, unban: i64, meta: BanMetadata, disconnect: bool| BannedRecord {
            ip: ip.to_string(),
            unban_at_ms: unban,
            module: "ctx".to_string(),
            ban_for_disconnect: disconnect,
            metadata: meta,
        };
        let mut list = BanList::new();
        list.load([
            record("1.2.3.4", 200, meta.clone(), false),
            // 同 peer 的重复封禁（上游按 LegacyBtnPeer 去重）
            record("dup", 200, meta.clone(), false),
            // ban_for_disconnect 跳过
            record("5.6.7.8", 400, meta2, true),
        ]);
        let translator = Translator::embedded();
        let rows = ban_rows_from_ban_list(&list, &translator, "zh_cn");
        assert_eq!(rows.len(), 1, "同 peer 去重 + 跳过 ban_for_disconnect");
        let row = &rows[0];
        assert_eq!(row.ban_at_ms, 100);
        assert_eq!(row.module, "ctx");
        assert!(!row.btn_ban);
        assert_eq!(row.peer.ip_address, "1.2.3.4");
    }

    #[test]
    fn wrapper_delegates_batches_and_serves_legacy_snapshots() {
        #[derive(Debug)]
        struct Empty;
        impl BtnSubmitSource for Empty {}
        let registry = new_live_peer_map();
        registry
            .lock()
            .unwrap()
            .entry("dl".to_string())
            .or_default()
            .extend(peer_rows_from(&torrent(), &[peer("1.2.3.4", 51413)]));
        let mut list = BanList::new();
        list.load([BannedRecord::minimal("9.9.9.9", 200, "ctx", false)]);
        let wrapper = LegacyAwareSubmitSource::new(
            Arc::new(Empty),
            registry,
            Arc::new(StdMutex::new(list)),
            Arc::new(Translator::embedded()),
            "zh_cn",
        );
        assert_eq!(wrapper.batch_ban_history(0, 10).len(), 0, "委托内层");
        assert_eq!(wrapper.legacy_peer_snapshot().len(), 1);
        assert_eq!(wrapper.legacy_ban_snapshot().len(), 1);
    }
}
