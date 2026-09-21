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
