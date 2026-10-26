// ===========================================================================
// 存储层数据类型（字段逐一对齐上游实体/DTO）
// ===========================================================================

/// 对齐上游 `databasent/dto/TrafficDataComputed`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficDataComputed {
    pub timestamp_ms: i64,
    pub data_overall_uploaded: i64,
    pub data_overall_downloaded: i64,
}

/// 对齐上游 `databasent/table/TrafficJournalEntity`（表 `traffic_journal_v3`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrafficJournalRow {
    pub downloader: String,
    /// `getStartOfHour(now)`（毫秒）
    pub timestamp_ms: i64,
    pub data_overall_downloaded: i64,
    pub data_overall_uploaded: i64,
    pub data_overall_downloaded_at_start: i64,
    pub data_overall_uploaded_at_start: i64,
    pub protocol_overall_downloaded: i64,
    pub protocol_overall_uploaded: i64,
    pub protocol_overall_downloaded_at_start: i64,
    pub protocol_overall_uploaded_at_start: i64,
}

/// 对齐上游 `PeerConnectionMetricsTrackServiceImpl.CacheKey`
/// （唯一键 `(timeframe_at, downloader, torrent_id, address, port)`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetricsTrackKey {
    /// `getStartOfToday(now)`（毫秒）
    pub timeframe_at_ms: i64,
    pub downloader: String,
    pub torrent_id: String,
    /// 上游为 `InetAddress`（缓存键用 `toCompressedString()`）
    pub address: String,
    pub port: u16,
}

/// 对齐上游 `databasent/table/PeerConnectionMetricsTrackEntity`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricsTrackRow {
    pub key: MetricsTrackKey,
    pub peer_id: Option<String>,
    pub client_name: Option<String>,
    pub last_flags: Option<String>,
}

impl MetricsTrackRow {
    pub fn new(key: MetricsTrackKey) -> Self {
        Self {
            key,
            peer_id: None,
            client_name: None,
            last_flags: None,
        }
    }
}

/// 对齐上游 `databasent/table/PeerConnectionMetricsEntity`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionMetricsRow {
    pub timeframe_at_ms: i64,
    pub downloader: String,
    pub total_connections: i64,
    pub incoming_connections: i64,
    pub remote_refuse_transfer_to_client: i64,
    pub remote_accept_transfer_to_client: i64,
    pub local_refuse_transfer_to_peer: i64,
    pub local_accept_transfer_to_peer: i64,
    pub local_not_interested: i64,
    pub question_status: i64,
    pub optimistic_unchoke: i64,
    pub from_dht: i64,
    pub from_pex: i64,
    pub from_lsd: i64,
    pub from_tracker_or_other: i64,
    pub rc4_encrypted: i64,
    pub plain_text_encrypted: i64,
    pub utp_socket: i64,
    pub tcp_socket: i64,
}

impl ConnectionMetricsRow {
    /// 对齐上游 `PeerConnectionMetricsEntity#merge`。
    ///
    /// **上游此处漏掉了 `local_not_interested`**（未累加该列），本移植保持同样的遗漏，
    /// 以便与上游落库的数据完全一致。
    pub fn merge(&mut self, appender: &ConnectionMetricsRow) {
        self.total_connections += appender.total_connections;
        self.incoming_connections += appender.incoming_connections;
        self.remote_refuse_transfer_to_client += appender.remote_refuse_transfer_to_client;
        self.remote_accept_transfer_to_client += appender.remote_accept_transfer_to_client;
        self.local_refuse_transfer_to_peer += appender.local_refuse_transfer_to_peer;
        self.local_accept_transfer_to_peer += appender.local_accept_transfer_to_peer;
        // 上游 merge() 未合并 local_not_interested（有意保留）
        self.question_status += appender.question_status;
        self.optimistic_unchoke += appender.optimistic_unchoke;
        self.from_dht += appender.from_dht;
        self.from_pex += appender.from_pex;
        self.from_lsd += appender.from_lsd;
        self.from_tracker_or_other += appender.from_tracker_or_other;
        self.rc4_encrypted += appender.rc4_encrypted;
        self.plain_text_encrypted += appender.plain_text_encrypted;
        self.utp_socket += appender.utp_socket;
        self.tcp_socket += appender.tcp_socket;
    }
}

/// 对齐上游 `databasent/table/PeerRecordEntity`（表 `peer_records`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerRecordRow {
    /// `InetAddress`（本移植用压缩 IP 字符串）
    pub address: String,
    pub port: u16,
    pub torrent_id: String,
    pub downloader: String,
    pub peer_id: String,
    pub client_name: String,
    pub uploaded: i64,
    pub uploaded_offset: i64,
    pub upload_speed: i64,
    pub downloaded: i64,
    pub downloaded_offset: i64,
    pub download_speed: i64,
    pub last_flags: Option<String>,
    pub first_time_seen_ms: i64,
    pub last_time_seen_ms: i64,
    /// 上游由 DAO 内 `IPDBManager.queryIPDB(inet).geoData()` 填充
    pub peer_geoip: Option<String>,
}

/// 对齐上游 `PeerRecordMapper.upsert`（sqlite XML）的 `ON CONFLICT ... DO UPDATE` CASE 表达式。
///
/// 冲突键：`(address, torrent_id, downloader)`。`first_time_seen` 与 `peer_geoip` **永不更新**；
/// 其余列在 `excluded.last_time_seen < peer_records.last_time_seen`（上报时间更旧）时保留旧值，
/// 否则按 SQL 取值（流量列按「本次上报值 - 已存偏移量」累加；差为负说明对方计数被重置，
/// 直接把本次上报值整体累加）。
pub fn apply_peer_record_upsert(
    existing: &PeerRecordRow,
    incoming: &PeerRecordRow,
) -> PeerRecordRow {
    let mut merged = existing.clone();
    if incoming.last_time_seen_ms < existing.last_time_seen_ms {
        return merged;
    }
    merged.uploaded = if incoming.uploaded - existing.uploaded_offset < 0 {
        existing.uploaded + incoming.uploaded
    } else {
        existing.uploaded + incoming.uploaded - existing.uploaded_offset
    };
    merged.downloaded = if incoming.downloaded - existing.downloaded_offset < 0 {
        existing.downloaded + incoming.downloaded
    } else {
        existing.downloaded + incoming.downloaded - existing.downloaded_offset
    };
    merged.uploaded_offset = incoming.uploaded;
    merged.downloaded_offset = incoming.downloaded;
    merged.upload_speed = incoming.upload_speed;
    merged.download_speed = incoming.download_speed;
    merged.port = incoming.port;
    merged.peer_id = incoming.peer_id.clone();
    merged.client_name = incoming.client_name.clone();
    merged.last_flags = incoming.last_flags.clone();
    merged.last_time_seen_ms = incoming.last_time_seen_ms;
    merged
}

/// 对齐上游 `TrackedSwarmServiceImpl.CacheKey`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrackedSwarmKey {
    pub ip: String,
    pub port: u16,
    pub info_hash: String,
    pub downloader: String,
}

/// 对齐上游 `databasent/table/tmp/TrackedSwarmEntity`（临时表 `tracked_swarm`）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrackedSwarmRow {
    pub id: Option<i64>,
    pub ip: String,
    pub port: u16,
    pub info_hash: String,
    pub torrent_is_private: Option<bool>,
    pub torrent_size: i64,
    pub downloader: String,
    pub downloader_progress: f64,
    pub peer_id: String,
    pub client_name: String,
    pub peer_progress: f64,
    pub uploaded: i64,
    pub uploaded_offset: i64,
    pub upload_speed: i64,
    pub downloaded: i64,
    pub downloaded_offset: i64,
    pub download_speed: i64,
    pub last_flags: String,
    pub first_time_seen_ms: i64,
    pub last_time_seen_ms: i64,
    pub download_speed_max: i64,
    pub upload_speed_max: i64,
    /// 对齐 `AbstractCanDirtyEntity.dirty`（供 BTN 提交等上层使用）
    pub dirty: bool,
}

impl TrackedSwarmRow {
    pub fn key(&self) -> TrackedSwarmKey {
        TrackedSwarmKey {
            ip: self.ip.clone(),
            port: self.port,
            info_hash: self.info_hash.clone(),
            downloader: self.downloader.clone(),
        }
    }
}

/// 对齐上游 `alert/AlertLevel`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum AlertLevel {
    Tip,
    Info,
    Warn,
    Error,
    Fatal,
}

/// 已发布的告警（对齐 `AlertManager.publishAlert` 的参数）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlertRecord {
    pub push: bool,
    pub level: AlertLevel,
    pub identifier: String,
    pub title: TranslationComponent,
    pub description: TranslationComponent,
}

/// 一次 `updateTrafficMonitoringService()` 发布的告警。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrafficMonitoringAlert {
    pub alert: AlertRecord,
    /// 当日已上传总量（bytes）
    pub total_uploaded: i64,
    /// `traffic-monitoring.daily`
    pub threshold: i64,
}

use super::*;
