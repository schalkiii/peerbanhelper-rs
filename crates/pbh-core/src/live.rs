//! 本轮 wave 的实时快照（对齐上游 `DownloaderServer#getLivePeersSnapshot()`）。
//!
//! 保存「下载器 → torrent → 本轮拉取到的 peers」，供
//! `/api/downloaders/{id}/torrents` 与 `.../peers` 直接读取，避免每次 API 请求
//! 都额外访问下载器；每轮 wave 开始时清空并由各下载器任务重新写入。

use crate::model::{PeerData, TorrentData};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct LiveTorrent {
    pub torrent: TorrentData,
    pub peers: Vec<PeerData>,
}

/// 线程安全的实时快照句柄。
#[derive(Clone, Debug, Default)]
pub struct LiveSnapshot(Arc<Mutex<HashMap<String, Vec<LiveTorrent>>>>);

pub fn new_live_snapshot() -> LiveSnapshot {
    LiveSnapshot::default()
}

impl LiveSnapshot {
    /// 新一轮 wave 开始：清空所有下载器的旧快照。
    pub fn clear(&self) {
        if let Ok(mut map) = self.0.lock() {
            map.clear();
        }
    }

    /// 记录一个 torrent 及其本轮 peers（由各下载器的并发任务调用）。
    pub fn insert(&self, downloader_id: &str, torrent: TorrentData, peers: Vec<PeerData>) {
        if let Ok(mut map) = self.0.lock() {
            map.entry(downloader_id.to_string())
                .or_default()
                .push(LiveTorrent { torrent, peers });
        }
    }

    /// 某下载器本轮的 torrent 列表（按 hash 去重）。
    pub fn torrents(&self, downloader_id: &str) -> Vec<TorrentData> {
        let Ok(map) = self.0.lock() else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        map.get(downloader_id)
            .map(|items| {
                items
                    .iter()
                    .filter(|item| seen.insert(item.torrent.hash.clone()))
                    .map(|item| item.torrent.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 某下载器 + torrent 的本轮 peers。
    pub fn peers(&self, downloader_id: &str, torrent_hash: &str) -> Vec<PeerData> {
        let Ok(map) = self.0.lock() else {
            return Vec::new();
        };
        map.get(downloader_id)
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| item.torrent.hash == torrent_hash)
            })
            .map(|item| item.peers.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn torrent(hash: &str) -> TorrentData {
        TorrentData {
            hash: hash.into(),
            name: "t".into(),
            progress: 1.0,
            total_size: 1,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: Some(false),
        }
    }

    fn peer(ip: &str) -> PeerData {
        PeerData {
            client_name: None,
            peer_id: None,
            dl_speed: 0,
            downloaded: 0,
            up_speed: 0,
            uploaded: 0,
            progress: 0.0,
            flags: None,
            ip: ip.into(),
            port: 1,
            raw_ip: format!("{ip}:1"),
            connection: None,
        }
    }

    #[test]
    fn snapshot_roundtrip_and_clear() {
        let snapshot = new_live_snapshot();
        snapshot.insert("dl", torrent("h1"), vec![peer("1.2.3.4")]);
        snapshot.insert("dl", torrent("h2"), vec![]);
        assert_eq!(snapshot.torrents("dl").len(), 2);
        assert_eq!(snapshot.peers("dl", "h1").len(), 1);
        assert!(snapshot.peers("dl", "missing").is_empty());
        snapshot.clear();
        assert!(snapshot.torrents("dl").is_empty());
    }
}
