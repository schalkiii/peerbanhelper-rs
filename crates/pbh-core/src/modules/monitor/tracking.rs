// ===========================================================================
// peer-analyse-service.swarm-tracking
// ===========================================================================

/// `module.peer-analyse-service.swarm-tracking` 的运行期配置。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SwarmTrackingSettings {
    /// `data-flush-interval`（对齐 `getLong("data-flush-interval")`，缺省 0）
    pub data_flush_interval_ms: i64,
}

/// 对齐上游 `SwarmTrackingModule`（配置键 `peer-analyse-service.swarm-tracking`）。
///
/// 只跟踪**本次运行会话**内的 swarm peer：`onEnable` → [`Self::on_enable`]（`resetTable`）
/// 清空整张临时表，因此重启后数据自动丢弃；`flushAll` → [`Self::flush_all`] 按
/// `data-flush-interval` 把缓存 upsert 入库；`check` 恒 `pass()`。
///
/// 与上游的差异：上游缓存 `PBHCache`（容量 1000、3 分钟超时淘汰时后台刷库），
/// 本移植用普通 `HashMap`，仅由定时 `flush_all` 写库。
pub struct SwarmTrackingModule {
    pub settings: SwarmTrackingSettings,
    sink: Arc<dyn MonitorSink>,
    cache: Mutex<HashMap<TrackedSwarmKey, TrackedSwarmRow>>,
}

impl SwarmTrackingModule {
    pub fn new(sink: Arc<dyn MonitorSink>, settings: SwarmTrackingSettings) -> Self {
        Self {
            settings,
            sink,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 对齐 `onEnable()`：`trackedSwarmDao.resetTable()`（清空临时表 + 本地缓存）。
    pub fn on_enable(&self) {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.sink.reset_tracked_swarm();
    }

    /// 对齐 `onPeersRetrieved`：握手中的 peer 跳过（其余 peer 逐个 `syncPeers`）。
    pub fn sync_peers(
        &self,
        downloader_id: &str,
        torrent: &TorrentData,
        peers: &[PeerData],
        now_ms: i64,
    ) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        for peer in peers {
            if peer.is_handshaking() {
                continue;
            }
            let key = TrackedSwarmKey {
                ip: compressed_ip(&peer.ip),
                port: peer.port,
                info_hash: torrent.hash.clone(),
                downloader: downloader_id.to_string(),
            };
            let mut entity = match cache.get(&key) {
                Some(cached) => cached.clone(),
                None => {
                    // 上游 cache-miss：先按唯一键取最后一行；命中则只刷新 last_time_seen
                    match self.sink.load_last_tracked_swarm(
                        &key.ip,
                        key.port,
                        &key.info_hash,
                        &key.downloader,
                    ) {
                        Some(mut last) => {
                            last.last_time_seen_ms = now_ms;
                            last
                        }
                        None => new_tracked_swarm_row(downloader_id, torrent, peer, now_ms),
                    }
                }
            };
            // 上游：peer.getDownloaded() >= offset ? 差值 : peer.getDownloaded()
            // （计数被重置时按整值累加）
            let new_downloaded = if peer.downloaded >= entity.downloaded_offset {
                peer.downloaded - entity.downloaded_offset
            } else {
                peer.downloaded
            };
            let new_uploaded = if peer.uploaded >= entity.uploaded_offset {
                peer.uploaded - entity.uploaded_offset
            } else {
                peer.uploaded
            };
            entity.downloaded += new_downloaded;
            entity.uploaded += new_uploaded;
            entity.downloaded_offset = peer.downloaded;
            entity.uploaded_offset = peer.uploaded;
            entity.client_name = peer.client_name.clone().unwrap_or_default();
            entity.peer_id = peer.peer_id.clone().unwrap_or_default();
            entity.last_flags = peer.flags.as_deref().map(lt_std_string).unwrap_or_default();
            entity.last_time_seen_ms = now_ms;
            entity.download_speed_max = entity.download_speed_max.max(peer.dl_speed);
            entity.upload_speed_max = entity.upload_speed_max.max(peer.up_speed);
            entity.dirty = true;
            cache.insert(key, entity);
        }
    }

    /// 对齐 `flushAll()`：把缓存里的跟踪行整体 upsert 入库（上游同样不清空缓存）。
    pub fn flush_all(&self) {
        let rows: Vec<TrackedSwarmRow> = {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.values().cloned().collect()
        };
        for row in rows {
            self.sink.upsert_tracked_swarm(&row);
        }
    }

    /// 对齐 `trackedSwarmDao.count()`（Web API `/api/modules/swarm-tracking`）。
    pub fn count(&self) -> usize {
        self.sink.count_tracked_swarm()
    }

    /// 缓存条目数（供日志/测试）。
    pub fn cache_len(&self) -> usize {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// 对齐 `new TrackedSwarmEntity(...)` 的字段顺序与初值。
pub(crate) fn new_tracked_swarm_row(
    downloader_id: &str,
    torrent: &TorrentData,
    peer: &PeerData,
    now_ms: i64,
) -> TrackedSwarmRow {
    TrackedSwarmRow {
        id: None,
        ip: compressed_ip(&peer.ip),
        port: peer.port,
        info_hash: torrent.hash.clone(),
        torrent_is_private: torrent.is_private,
        torrent_size: torrent.total_size,
        downloader: downloader_id.to_string(),
        downloader_progress: torrent.progress,
        peer_id: peer.peer_id.clone().unwrap_or_default(),
        client_name: peer.client_name.clone().unwrap_or_default(),
        peer_progress: peer.progress,
        uploaded: 0,
        uploaded_offset: 0,
        upload_speed: peer.up_speed,
        downloaded: 0,
        downloaded_offset: 0,
        download_speed: peer.dl_speed,
        last_flags: peer.flags.as_deref().map(lt_std_string).unwrap_or_default(),
        first_time_seen_ms: now_ms,
        last_time_seen_ms: now_ms,
        download_speed_max: peer.dl_speed,
        upload_speed_max: peer.up_speed,
        dirty: true,
    }
}

impl RuleModule for SwarmTrackingModule {
    fn name(&self) -> &str {
        "Swarm Tracking Module"
    }

    fn config_name(&self) -> &str {
        "peer-analyse-service.swarm-tracking"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn check(
        &self,
        _downloader_id: &str,
        _torrent: &TorrentData,
        _peer: &PeerData,
        _ctx: &CheckContext,
    ) -> CheckResult {
        // 上游本模块没有 shouldBanPeer；pass()
        CheckResult::pass(self.config_name())
    }
}

use super::*;
