// ===========================================================================
// peer-analyse-service.peer-recording
// ===========================================================================

/// `module.peer-analyse-service.peer-recording` 的运行期配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerRecordingSettings {
    /// `data-retention-time`（对齐 `getLong("data-retention-time", -1)`）
    pub data_retention_time_ms: i64,
    /// `data-cleanup-interval`（对齐 `getLong("data-cleanup-interval", -1)`）
    pub data_cleanup_interval_ms: i64,
    /// `data-flush-interval`（对齐 `getLong("data-flush-interval", 20000)`）
    pub data_flush_interval_ms: i64,
}

impl Default for PeerRecordingSettings {
    fn default() -> Self {
        Self {
            data_retention_time_ms: -1,
            data_cleanup_interval_ms: -1,
            data_flush_interval_ms: 20_000,
        }
    }
}

/// 对齐上游 `PBHCache` 的两个外部开关默认值
/// （`pbh.module.peerRecordingServiceModule.diskWriteCache.size` / `.timeout`）。
pub const PEER_RECORDING_CACHE_SIZE: usize = 3500;
pub const PEER_RECORDING_CACHE_TIMEOUT_MS: i64 = 180_000;

/// 对齐上游 `CacheKey(downloader, TorrentWrapper, PeerAddress)`。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PeerRecordCacheKey {
    pub downloader: String,
    /// `TorrentWrapper` 的等价键
    pub torrent_hash: String,
    pub peer_ip: String,
    pub peer_port: u16,
}

/// 对齐上游 `PeerRecordServiceImpl.PeerRecordCachingEntire`。
#[derive(Debug, Clone)]
pub struct PeerRecordCachingEntire {
    pub timestamp_ms: i64,
    pub downloader: String,
    pub torrent: TorrentData,
    pub peer: PeerData,
    /// 对齐 `AbstractCanDirtyEntity.dirty`：`flushToDatabase` 后置 false
    pub dirty: bool,
}

impl PeerRecordCachingEntire {
    pub fn new(
        timestamp_ms: i64,
        downloader: &str,
        torrent: &TorrentData,
        peer: &PeerData,
    ) -> Self {
        Self {
            timestamp_ms,
            downloader: downloader.to_string(),
            torrent: torrent.clone(),
            peer: peer.clone(),
            // 上游构造时 dirty = true
            dirty: true,
        }
    }

    /// 对齐 Lombok `@Data` 生成的 `equals`：逐字段比较 `TorrentWrapper` / `PeerWrapper`。
    pub fn same_as(&self, other: &Self) -> bool {
        self.timestamp_ms == other.timestamp_ms
            && self.downloader == other.downloader
            && self.dirty == other.dirty
            && torrent_snapshot_same(&self.torrent, &other.torrent)
            && peer_snapshot_same(&self.peer, &other.peer)
    }
}

fn torrent_snapshot_same(a: &TorrentData, b: &TorrentData) -> bool {
    a.hash == b.hash
        && a.name == b.name
        && a.progress == b.progress
        && a.total_size == b.total_size
        && a.piece_size == b.piece_size
        && a.pieces_have == b.pieces_have
        && a.completed_override == b.completed_override
        && a.dlspeed == b.dlspeed
        && a.upspeed == b.upspeed
        && a.is_private == b.is_private
}

fn peer_snapshot_same(a: &PeerData, b: &PeerData) -> bool {
    a.client_name == b.client_name
        && a.peer_id == b.peer_id
        && a.dl_speed == b.dl_speed
        && a.downloaded == b.downloaded
        && a.up_speed == b.up_speed
        && a.uploaded == b.uploaded
        && a.progress == b.progress
        && a.flags == b.flags
        && a.ip == b.ip
        && a.port == b.port
        && a.raw_ip == b.raw_ip
        && a.connection == b.connection
}

/// 对齐上游 `PeerRecordServiceImpl.flushToDatabase` 的 peer_id 截断：
/// `peer.getId().length() > 8 ? substring(0, 8) : peer.getId()`。
///
/// 上游的 `length()` / `substring` 都是 **UTF-16 码元**口径，因此含 emoji 等增补平面
/// 字符的 peer-id 必须按码元截断，按 Unicode 标量截断会多保留字符。
pub(crate) fn truncate_peer_id(peer_id: Option<&str>) -> String {
    let Some(peer_id) = peer_id else {
        return String::new();
    };
    let units: Vec<u16> = peer_id.encode_utf16().collect();
    if units.len() > 8 {
        String::from_utf16_lossy(&units[..8])
    } else {
        peer_id.to_string()
    }
}

/// 对齐上游 `PeerRecordingServiceModule`（配置键 `peer-analyse-service.peer-recording`）。
///
/// - `onPeersRetrieved` → [`Self::on_peers_retrieved`]：只保留「客户端名非空 / peer-id 非空 /
///   非握手中」的 peer，写入磁盘写缓存（键 `(downloader, torrent, peer 地址)`）；
/// - `flush` → [`Self::flush`]：把缓存整体写库（对齐 `flush()` 的批量 upsert，写后 dirty = false）；
/// - `cleanup` → [`Self::cleanup`]：`data-retention-time > 0` 时删除
///   `last_time_seen < now - data-retention-time`（**开区间** `lt`）；
/// - `check` 恒 `pass()`。
///
/// 与上游的差异：上游缓存 `PBHCache`（容量 [`PEER_RECORDING_CACHE_SIZE`]、
/// 超时 [`PEER_RECORDING_CACHE_TIMEOUT_MS`]，淘汰时后台刷库），本移植用普通 `HashMap`，
/// 仅由定时 `flush` 写库。
pub struct PeerRecordingServiceModule {
    pub settings: PeerRecordingSettings,
    sink: Arc<dyn MonitorSink>,
    cache: Mutex<HashMap<PeerRecordCacheKey, PeerRecordCachingEntire>>,
}

impl PeerRecordingServiceModule {
    pub fn new(sink: Arc<dyn MonitorSink>, settings: PeerRecordingSettings) -> Self {
        Self {
            settings,
            sink,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 对齐 `onPeersRetrieved` 的过滤 + 缓存写入。
    pub fn on_peers_retrieved(
        &self,
        downloader_id: &str,
        torrent: &TorrentData,
        peers: &[PeerData],
        now_ms: i64,
    ) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        for peer in peers {
            let client_name = peer.client_name.as_deref().unwrap_or("");
            let peer_id = peer.peer_id.as_deref().unwrap_or("");
            let keep = (!client_name.trim().is_empty())
                || (!peer_id.trim().is_empty())
                || !peer.is_handshaking();
            if !keep {
                continue;
            }
            let cache_key = PeerRecordCacheKey {
                downloader: downloader_id.to_string(),
                torrent_hash: torrent.hash.clone(),
                peer_ip: compressed_ip(&peer.ip),
                peer_port: peer.port,
            };
            let current = PeerRecordCachingEntire::new(now_ms, downloader_id, torrent, peer);
            // 上游：`inCache = cache.get(key, () -> current)`，不相等才 `put`（相等则不做任何事）
            match cache.get(&cache_key) {
                Some(cached) if cached.same_as(&current) => {}
                _ => {
                    cache.insert(cache_key, current);
                }
            }
        }
    }

    /// 对齐 `flush()`：事务内把所有缓存条目 `flushToDatabase`（并置 dirty = false）。
    pub fn flush(&self) {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let entries: Vec<PeerRecordCachingEntire> = cache.values().cloned().collect();
        for entry in &entries {
            let row = self.to_row(entry);
            self.sink.upsert_peer_record(&row);
        }
        for entry in cache.values_mut() {
            entry.dirty = false;
        }
        // 对齐上游 PBHCache：flush 后按超时（PEER_RECORDING_CACHE_TIMEOUT_MS）淘汰，
        // 仍超限则按时间戳淘汰最旧条目（均为已 flush 的 dirty=false 条目，安全），避免无界增长
        let now = now_ms();
        cache.retain(|_, entry| now - entry.timestamp_ms <= PEER_RECORDING_CACHE_TIMEOUT_MS);
        if cache.len() > PEER_RECORDING_CACHE_SIZE {
            let mut keys: Vec<(PeerRecordCacheKey, i64)> =
                cache.iter().map(|(k, e)| (k.clone(), e.timestamp_ms)).collect();
            keys.sort_by_key(|(_, t)| *t);
            for (k, _) in keys.into_iter().take(cache.len() - PEER_RECORDING_CACHE_SIZE) {
                cache.remove(&k);
            }
        }
    }

    /// 对齐 `cleanup()`：`dataRetentionTime <= 0` 时直接返回（上游 log 后 return）。
    ///
    /// 返回 `None` 表示未执行；`Some(n)` 为删除条数。
    pub fn cleanup(&self, now_ms: i64) -> Option<usize> {
        if self.settings.data_retention_time_ms <= 0 {
            return None;
        }
        let before = now_ms - self.settings.data_retention_time_ms;
        Some(self.sink.remove_peer_records_before(before))
    }

    /// 缓存条目数（供日志/测试）。
    pub fn cache_len(&self) -> usize {
        self.cache.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// 对齐 `flushToDatabase(PeerRecordCachingEntire)`。
    fn to_row(&self, entry: &PeerRecordCachingEntire) -> PeerRecordRow {
        let torrent_id = self.sink.ensure_torrent(&entry.torrent);
        let peer = &entry.peer;
        PeerRecordRow {
            address: compressed_ip(&peer.ip),
            port: peer.port,
            torrent_id,
            downloader: entry.downloader.clone(),
            peer_id: truncate_peer_id(peer.peer_id.as_deref()),
            client_name: peer.client_name.clone().unwrap_or_default(),
            // uploaded / downloaded 同时写入「上报值」与「偏移量」
            uploaded: peer.uploaded,
            uploaded_offset: peer.uploaded,
            upload_speed: peer.up_speed,
            downloaded: peer.downloaded,
            downloaded_offset: peer.downloaded,
            download_speed: peer.dl_speed,
            last_flags: lt_std_string_opt(peer.flags.as_deref()),
            // 上游 flush 时把 first/last time seen 都写成缓存条目的时间戳
            first_time_seen_ms: entry.timestamp_ms,
            last_time_seen_ms: entry.timestamp_ms,
            // 上游在 DAO 内用 IPDBManager 填充；本移植交由 DB 版 sink 填充
            peer_geoip: None,
        }
    }
}

impl RuleModule for PeerRecordingServiceModule {
    fn name(&self) -> &str {
        "Peer Recording Service"
    }

    fn config_name(&self) -> &str {
        "peer-analyse-service.peer-recording"
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
