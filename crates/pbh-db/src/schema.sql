CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ban_logs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    downloader_id TEXT    NOT NULL,
    torrent_hash  TEXT    NOT NULL,
    torrent_name  TEXT    NOT NULL,
    ip            TEXT    NOT NULL,
    port          INTEGER NOT NULL,
    peer_id       TEXT    NOT NULL DEFAULT '',
    client_name   TEXT    NOT NULL DEFAULT '',
    module        TEXT    NOT NULL,
    rule          TEXT    NOT NULL DEFAULT '',
    reason        TEXT    NOT NULL DEFAULT '',
    rule_key      TEXT,
    reason_key    TEXT,
    ban_duration  INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_ban_logs_created ON ban_logs(created_at);
CREATE INDEX IF NOT EXISTS idx_ban_logs_ip ON ban_logs(ip);

CREATE TABLE IF NOT EXISTS banned_ips (
    ip              TEXT PRIMARY KEY,
    first_banned_at INTEGER NOT NULL,
    last_banned_at  INTEGER NOT NULL,
    module          TEXT    NOT NULL,
    hit_count       INTEGER NOT NULL DEFAULT 1,
    ban_until       INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_banned_ips_until ON banned_ips(ban_until);

CREATE TABLE IF NOT EXISTS pcb_addr (
    downloader_id                       TEXT    NOT NULL,
    torrent_id                          TEXT    NOT NULL,
    key                                 TEXT    NOT NULL,
    port                                INTEGER NOT NULL,
    last_report_uploaded                INTEGER NOT NULL DEFAULT 0,
    tracking_uploaded_increase_total    INTEGER NOT NULL DEFAULT 0,
    last_report_progress                REAL    NOT NULL DEFAULT 0,
    last_torrent_completed_size         INTEGER NOT NULL DEFAULT 0,
    progress_difference_counter         INTEGER NOT NULL DEFAULT 0,
    rewind_counter                      INTEGER NOT NULL DEFAULT 0,
    ban_delay_window_end_ms             INTEGER NOT NULL DEFAULT 0,
    fast_pcb_test_executed              INTEGER NOT NULL DEFAULT 0,
    last_time_seen_ms                   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (downloader_id, torrent_id, key, port)
);

CREATE TABLE IF NOT EXISTS pcb_range (
    downloader_id                       TEXT    NOT NULL,
    torrent_id                          TEXT    NOT NULL,
    key                                 TEXT    NOT NULL,
    port                                INTEGER NOT NULL DEFAULT 0,
    last_report_uploaded                INTEGER NOT NULL DEFAULT 0,
    tracking_uploaded_increase_total    INTEGER NOT NULL DEFAULT 0,
    last_report_progress                REAL    NOT NULL DEFAULT 0,
    last_torrent_completed_size         INTEGER NOT NULL DEFAULT 0,
    progress_difference_counter         INTEGER NOT NULL DEFAULT 0,
    rewind_counter                      INTEGER NOT NULL DEFAULT 0,
    ban_delay_window_end_ms             INTEGER NOT NULL DEFAULT 0,
    fast_pcb_test_executed              INTEGER NOT NULL DEFAULT 0,
    last_time_seen_ms                   INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (downloader_id, torrent_id, key, port)
);

-- ===========================================================================
-- 监控模块（`active-monitoring` / `peer-analyse-service.*`）的落库表。
--
-- 逐表对齐上游 SQLite 建表脚本 `resources/db/migration/sqlite/V1_1__initial_sqlite.sql`
-- （表名、列名、唯一索引即 ON CONFLICT 目标），以及后续增量迁移：
--   V1_3：`peer_records` 唯一键去掉 port  -> (address, torrent_id, downloader)
--   V1_4：`peer_connection_metrics_track.peer_id` 允许 NULL
-- 所有时间戳列均为 epoch 毫秒（对齐 `OffsetDateTimeTypeHandlerForSQLite`）。
-- ===========================================================================

-- 对齐 `AlertEntity`（表 `alert`）：`create_at` / `read_at` 可空（未读 = NULL），
-- `title` / `content` 存 `TranslationComponent` 的 JSON（对齐 `TranslationComponentTypeHandler`）。
CREATE TABLE IF NOT EXISTS alert (
    id         INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    create_at  INTEGER NOT NULL,
    read_at    INTEGER NULL,
    level      TEXT    NOT NULL,
    identifier TEXT    NOT NULL,
    title      TEXT    NOT NULL,
    content    TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_alert_alertExists ON alert (read_at, identifier);
CREATE INDEX IF NOT EXISTS idx_alert_readAt ON alert (read_at);
CREATE INDEX IF NOT EXISTS idx_alert_unreadAlerts ON alert (create_at, read_at);

-- 对齐 `TorrentEntity`（表 `torrents`）：`TorrentService.createIfNotExists` 的落点，
-- 主键即 `MonitorSink::ensure_torrent` 返回的分组键。
CREATE TABLE IF NOT EXISTS torrents (
    id              INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    info_hash       TEXT    NOT NULL,
    name            TEXT    NOT NULL,
    size            INTEGER NOT NULL,
    private_torrent INTEGER NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_torrents_info_hash ON torrents (info_hash);
CREATE INDEX IF NOT EXISTS idx_torrents_name ON torrents (name);
CREATE INDEX IF NOT EXISTS idx_torrents_private_torrent ON torrents (private_torrent);

-- 对齐 `TrafficJournalEntity`（表 `traffic_journal_v3`）：唯一键 (timestamp, downloader)。
CREATE TABLE IF NOT EXISTS traffic_journal_v3 (
    id                                   INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    timestamp                            INTEGER NOT NULL,
    downloader                           TEXT    NOT NULL,
    data_overall_uploaded_at_start       INTEGER NOT NULL,
    data_overall_uploaded                INTEGER NOT NULL,
    data_overall_downloaded_at_start     INTEGER NOT NULL,
    data_overall_downloaded              INTEGER NOT NULL,
    protocol_overall_uploaded_at_start   INTEGER NOT NULL,
    protocol_overall_uploaded            INTEGER NOT NULL,
    protocol_overall_downloaded_at_start INTEGER NOT NULL,
    protocol_overall_downloaded          INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_traffic_journal_v3_unique
    ON traffic_journal_v3 (timestamp, downloader);

-- 对齐 `PeerConnectionMetricsTrackEntity`（表 `peer_connection_metrics_track`）：
-- 唯一键 (timeframe_at, downloader, torrent_id, address, port)（V1_4 后 peer_id 可空）。
CREATE TABLE IF NOT EXISTS peer_connection_metrics_track (
    id           INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    timeframe_at INTEGER NOT NULL,
    downloader   TEXT    NOT NULL,
    torrent_id   INTEGER NOT NULL,
    address      TEXT    NOT NULL,
    port         INTEGER NOT NULL,
    peer_id      TEXT    NULL,
    client_name  TEXT    NULL,
    last_flags   TEXT    NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_peer_connection_metrics_track
    ON peer_connection_metrics_track (timeframe_at, downloader, torrent_id, address, port);

-- 对齐 `PeerConnectionMetricsEntity`（表 `peer_connection_metrics`）：
-- 唯一键 (timeframe_at, downloader)；`local_not_interested` 在 `merge()` 中被上游遗漏（见 monitor.rs）。
CREATE TABLE IF NOT EXISTS peer_connection_metrics (
    id                               INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    timeframe_at                     INTEGER NOT NULL,
    downloader                       TEXT    NOT NULL,
    total_connections                INTEGER NOT NULL,
    incoming_connections             INTEGER NOT NULL,
    remote_refuse_transfer_to_client INTEGER NOT NULL,
    remote_accept_transfer_to_client INTEGER NOT NULL,
    local_refuse_transfer_to_peer    INTEGER NOT NULL,
    local_accept_transfer_to_peer    INTEGER NOT NULL,
    local_not_interested             INTEGER NOT NULL,
    question_status                  INTEGER NOT NULL,
    optimistic_unchoke               INTEGER NOT NULL,
    from_dht                         INTEGER NOT NULL,
    from_pex                         INTEGER NOT NULL,
    from_lsd                         INTEGER NOT NULL,
    from_tracker_or_other            INTEGER NOT NULL,
    rc4_encrypted                    INTEGER NOT NULL,
    plain_text_encrypted             INTEGER NOT NULL,
    utp_socket                       INTEGER NOT NULL,
    tcp_socket                       INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_peer_connection_metrics_unique
    ON peer_connection_metrics (timeframe_at, downloader);

-- 对齐 `PeerRecordEntity`（表 `peer_records`）：唯一键 (address, torrent_id, downloader)，
-- `peer_geoip` 存 IP 库查询结果的 JSON（对齐 `BasicJsonTypeHandler` 用的 Gson 写法）。
CREATE TABLE IF NOT EXISTS peer_records (
    id                INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    address           TEXT    NOT NULL,
    port              INTEGER NOT NULL,
    torrent_id        INTEGER NOT NULL,
    downloader        TEXT    NOT NULL,
    peer_id           TEXT    NULL,
    client_name       TEXT    NULL,
    uploaded          INTEGER NOT NULL,
    uploaded_offset   INTEGER NOT NULL,
    upload_speed      INTEGER NOT NULL,
    downloaded        INTEGER NOT NULL,
    downloaded_offset INTEGER NOT NULL,
    download_speed    INTEGER NOT NULL,
    last_flags        TEXT    NULL,
    first_time_seen   INTEGER NOT NULL,
    last_time_seen    INTEGER NOT NULL,
    peer_geoip        TEXT    NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_peer_records_unique
    ON peer_records (address, torrent_id, downloader);
CREATE INDEX IF NOT EXISTS idx_peer_records_address ON peer_records (address);
CREATE INDEX IF NOT EXISTS idx_peer_records_client_analyse
    ON peer_records (downloader, uploaded, downloaded, first_time_seen, last_time_seen);
CREATE INDEX IF NOT EXISTS idx_peer_records_last_time_seen ON peer_records (last_time_seen);
CREATE INDEX IF NOT EXISTS idx_peer_records_session_between
    ON peer_records (downloader, first_time_seen, last_time_seen);

-- 对齐 `TrackedSwarmEntity`（上游建为临时表 `tracked_swarm`）：唯一键 (ip, port, info_hash, downloader)，
-- 应用启动时 `resetTable` 整表清空（数据随本次运行会话存在）。
-- 与上游 V1_1 的两处存储类差异（不改变任何读写语义）：
--   * `peer_progress`：上游写成 `TEXT NOT NULL`（手误，Java 实体是 `double`），此处按实体声明 REAL；
--   * `torrent_is_private`：上游写成 `NOT NULL`（Java `Boolean` 可空，postgres 的 V1_7 迁移已改为可空）。
CREATE TABLE IF NOT EXISTS tracked_swarm (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
    ip                  TEXT    NOT NULL,
    port                INTEGER NOT NULL,
    info_hash           TEXT    NOT NULL,
    torrent_is_private  INTEGER NULL,
    torrent_size        INTEGER NOT NULL,
    downloader          TEXT    NOT NULL,
    downloader_progress REAL    NOT NULL,
    peer_id             TEXT    NULL,
    client_name         TEXT    NULL,
    peer_progress       REAL    NOT NULL,
    uploaded            INTEGER NOT NULL,
    uploaded_offset     INTEGER NOT NULL,
    upload_speed        INTEGER NOT NULL,
    downloaded          INTEGER NOT NULL,
    downloaded_offset   INTEGER NOT NULL,
    download_speed      INTEGER NOT NULL,
    last_flags          TEXT    NULL,
    first_time_seen     INTEGER NOT NULL,
    last_time_seen      INTEGER NOT NULL,
    download_speed_max  INTEGER NOT NULL,
    upload_speed_max    INTEGER NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_tracked_swarm_unique
    ON tracked_swarm (ip, port, info_hash, downloader);
CREATE INDEX IF NOT EXISTS idx_tracked_swarm_last_seen_time
    ON tracked_swarm (last_time_seen DESC);
