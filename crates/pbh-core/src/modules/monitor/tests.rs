use super::*;
use crate::module::PeerAction;

fn sink() -> Arc<InMemoryMonitorSink> {
    Arc::new(InMemoryMonitorSink::new())
}

fn torrent() -> TorrentData {
    TorrentData {
        hash: "abcdef0123456789".to_string(),
        name: "示例种子".to_string(),
        progress: 0.5,
        total_size: 1_000_000_000,
        piece_size: 0,
        pieces_have: 0,
        completed_override: None,
        dlspeed: 0,
        upspeed: 0,
        is_private: Some(false),
    }
}

fn peer(ip: &str, port: u16, flags: &str) -> PeerData {
    PeerData {
        client_name: Some("qBittorrent 4.6.0".to_string()),
        peer_id: Some("-qB4600-abcdefghijkl".to_string()),
        dl_speed: 1024,
        downloaded: 4096,
        up_speed: 2048,
        uploaded: 8192,
        progress: 0.25,
        flags: Some(flags.to_string()),
        ip: ip.to_string(),
        port,
        raw_ip: format!("{ip}:{port}"),
        connection: Some("uTP".to_string()),
    }
}

fn handshaking_peer(ip: &str, port: u16) -> PeerData {
    let mut p = peer(ip, port, "");
    p.up_speed = 0;
    p.dl_speed = 0;
    p.client_name = None;
    p.peer_id = None;
    p
}

fn track_row(
    timeframe_at_ms: i64,
    downloader: &str,
    address: &str,
    port: u16,
    last_flags: Option<&str>,
) -> MetricsTrackRow {
    MetricsTrackRow {
        key: MetricsTrackKey {
            timeframe_at_ms,
            downloader: downloader.to_string(),
            torrent_id: "t1".to_string(),
            address: address.to_string(),
            port,
        },
        peer_id: None,
        client_name: None,
        last_flags: last_flags.map(|s| s.to_string()),
    }
}

fn metrics_row(timeframe_at_ms: i64, downloader: &str, total: i64) -> ConnectionMetricsRow {
    ConnectionMetricsRow {
        timeframe_at_ms,
        downloader: downloader.to_string(),
        total_connections: total,
        ..Default::default()
    }
}

// ---------------- 工具函数 ----------------

#[test]
fn human_readable_byte_count_matches_upstream() {
    // 对齐 `MsgUtil.humanReadableByteCountBin`
    assert_eq!(human_readable_byte_count_bin(0), "0 B");
    assert_eq!(human_readable_byte_count_bin(1023), "1023 B");
    assert_eq!(human_readable_byte_count_bin(1024), "1.0 KiB");
    assert_eq!(human_readable_byte_count_bin(1536), "1.5 KiB");
    assert_eq!(human_readable_byte_count_bin(5 * 1024 * 1024), "5.0 MiB");
    assert_eq!(human_readable_byte_count_bin(25_000_000_000), "23.3 GiB");
    assert_eq!(human_readable_byte_count_bin(i64::MAX), "8.0 EiB");
    assert_eq!(human_readable_byte_count_bin(-1024), "-1.0 KiB");
    // 对齐 `MsgUtil.humanReadableByteCountSI`
    assert_eq!(human_readable_byte_count_si(999), "999 B");
    assert_eq!(human_readable_byte_count_si(1000), "1.0 kB");
    assert_eq!(human_readable_byte_count_si(5_000_000), "5.0 MB");
    assert_eq!(human_readable_byte_count_si(-1000), "-1.0 kB");
}

#[test]
fn lt_std_string_rebuilds_canonical_flags() {
    // 对齐 `PeerFlag#toString()` 的顺序与默认值语义
    assert_eq!(lt_std_string("d u"), "d u");
    assert_eq!(lt_std_string("D U O S X L E e P"), "D U O S X L E e P");
    // 'I' 表示非本地连接；空串保持为空串（Java 中 `""` 不是 null）
    assert_eq!(lt_std_string(""), "");
    assert_eq!(lt_std_string("I"), "I");
    // 'K'（远程未 choke 且我们不感兴趣）与 '?'（我们未 choke 且对方不感兴趣）
    assert_eq!(lt_std_string("K ?"), "K ?");
    assert_eq!(lt_std_string_opt(None), None);
    assert_eq!(lt_std_string_opt(Some("")), Some(String::new()));
    assert_eq!(
        compressed_ip("2001:0db8:0000:0000:0000:0000:0000:0001"),
        "2001:db8::1"
    );
    assert_eq!(compressed_ip("::ffff:1.2.3.4"), "1.2.3.4");
    assert_eq!(compressed_ip("not-an-ip"), "not-an-ip");
}

#[test]
fn local_day_and_hour_boundaries_are_local() {
    let now = 1_700_000_000_123i64;
    let start_of_today = start_of_today_ms(now);
    let start_of_hour = start_of_hour_ms(now);
    assert!(start_of_today <= now);
    assert!(now - start_of_today < 90_000_000);
    assert!(start_of_hour <= now);
    assert!(now - start_of_hour < 3_600_000);
    // 起点是固定点（幂等）
    assert_eq!(start_of_today_ms(start_of_today), start_of_today);
    assert_eq!(start_of_hour_ms(start_of_hour), start_of_hour);
    // 本地时间语义：当天 0 点整 / 当前小时的第 0 分 0 秒
    assert_eq!(
        local_naive(start_of_today).format("%H:%M:%S").to_string(),
        "00:00:00"
    );
    assert_eq!(
        local_naive(start_of_hour).format("%M:%S").to_string(),
        "00:00"
    );
    // 当日 23:59:59.999 晚于当前时刻
    assert!(end_of_today_ms(now) > now);
    assert!(end_of_today_ms(now) - start_of_today < 90_000_000);
    // 格式化
    assert_eq!(format_date_only(now).len(), 10);
    assert_eq!(format_date_time(now).len(), 19);
}

// ---------------- 模块元信息与 pass-through ----------------

#[test]
fn config_names_match_upstream() {
    let s = sink();
    let active = ActiveMonitoringModule::new(s.clone(), ActiveMonitoringSettings::default());
    assert_eq!(active.name(), "Active Monitoring");
    assert_eq!(active.config_name(), "active-monitoring");

    let session =
        SessionAnalyseServiceModule::new(s.clone(), SessionAnalyseSettings::default());
    assert_eq!(session.name(), "Session Analyse Service");
    assert_eq!(
        session.config_name(),
        "peer-analyse-service.session-analyse"
    );

    let recording =
        PeerRecordingServiceModule::new(s.clone(), PeerRecordingSettings::default());
    assert_eq!(recording.name(), "Peer Recording Service");
    assert_eq!(
        recording.config_name(),
        "peer-analyse-service.peer-recording"
    );

    let swarm = SwarmTrackingModule::new(s, SwarmTrackingSettings::default());
    assert_eq!(swarm.name(), "Swarm Tracking Module");
    assert_eq!(swarm.config_name(), "peer-analyse-service.swarm-tracking");
}

#[test]
fn all_four_modules_check_is_passthrough() {
    let s = sink();
    let modules: Vec<Box<dyn RuleModule>> = vec![
        Box::new(ActiveMonitoringModule::new(
            s.clone(),
            ActiveMonitoringSettings::default(),
        )),
        Box::new(SessionAnalyseServiceModule::new(
            s.clone(),
            SessionAnalyseSettings::default(),
        )),
        Box::new(PeerRecordingServiceModule::new(
            s.clone(),
            PeerRecordingSettings::default(),
        )),
        Box::new(SwarmTrackingModule::new(
            s,
            SwarmTrackingSettings::default(),
        )),
    ];
    let ctx = CheckContext {
        now_ms: 1_700_000_000_000,
        features: Vec::new(),
    };
    for module in &modules {
        for candidate in [
            peer("1.2.3.4", 51413, "d u"),
            handshaking_peer("10.0.0.1", 6881),
        ] {
            let result = module.check("qb", &torrent(), &candidate, &ctx);
            assert_eq!(
                result.action,
                PeerAction::NoAction,
                "{}",
                module.config_name()
            );
            assert_eq!(result.ban_duration_ms, 0);
            assert_eq!(result.module, module.config_name());
            assert_eq!(result.data["status"], "pass");
            assert_eq!(
                result.reason_key,
                Some(TranslationComponent::new("Check passed"))
            );
        }
    }
}

// ---------------- active-monitoring ----------------

fn active_module(
    s: Arc<InMemoryMonitorSink>,
    daily: i64,
    sliding: bool,
    threshold: i64,
    min_speed: i64,
    max_speed: i64,
) -> ActiveMonitoringModule {
    ActiveMonitoringModule::new(
        s,
        ActiveMonitoringSettings {
            daily_traffic_capping: daily,
            use_traffic_sliding_capping: sliding,
            max_traffic_allowed_in_window_period: threshold,
            traffic_sliding_capping_max_speed: max_speed,
            traffic_sliding_capping_min_speed: min_speed,
        },
    )
}

#[test]
fn tick_writes_hourly_journal_and_skips_logged_out_downloaders() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    let module = active_module(s.clone(), -1, false, 0, 0, 0);
    let stats = vec![
        DownloaderTrafficStats::new("qb", "qBittorrent", true, 111, 222, None, true),
        DownloaderTrafficStats::new("tr", "Transmission", false, 333, 444, None, true),
    ];
    let (changes, alert) = module.on_tick(&stats, now);
    assert!(changes.is_empty());
    assert!(alert.is_none(), "daily = -1 ⇒ 阈值告警禁用");

    let journal = s.traffic_journal();
    assert_eq!(journal.len(), 1, "未登录成功的下载器不写日志");
    assert_eq!(journal[0].downloader, "qb");
    assert_eq!(journal[0].timestamp_ms, start_of_hour_ms(now));
    assert_eq!(journal[0].data_overall_downloaded, 111);
    assert_eq!(journal[0].data_overall_uploaded, 222);
    assert_eq!(journal[0].data_overall_uploaded_at_start, 222);

    // 同一小时内的第二次 tick 只做 max 更新，at_start 保持不变
    let stats = vec![DownloaderTrafficStats::new(
        "qb",
        "qBittorrent",
        true,
        111,
        999,
        None,
        true,
    )];
    module.on_tick(&stats, now + 60_000);
    let journal = s.traffic_journal();
    assert_eq!(journal.len(), 1);
    assert_eq!(journal[0].data_overall_uploaded, 999);
    assert_eq!(journal[0].data_overall_uploaded_at_start, 222);
}

#[test]
fn daily_threshold_alert_fires_once_and_matches_upstream_payload() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    let module = active_module(s.clone(), 1000, false, 0, 0, 0);
    // 当日已上传 2000 bytes：先建行（at_start = 0）再在同小时内更新，使当日增量为 2000
    s.update_traffic_journal("qb", 0, 0, 0, 0, now);
    s.update_traffic_journal("qb", 0, 2000, 0, 0, now + 60_000);

    let alert = module.update_traffic_monitoring(now).expect("应发布告警");
    assert_eq!(alert.alert.level, AlertLevel::Warn);
    assert!(alert.alert.push);
    assert_eq!(
        alert.alert.identifier,
        format!("dataTrafficCapping-{}", start_of_today_ms(now) / 1000)
    );
    assert_eq!(alert.total_uploaded, 2000);
    assert_eq!(alert.threshold, 1000);
    assert_eq!(
        alert.alert.title.key,
        "MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_TITLE"
    );
    assert_eq!(alert.alert.title.params, vec![format_date_only(now).into()]);
    assert_eq!(
        alert.alert.description.key,
        "MODULE_AMM_TRAFFIC_MONITORING_TRAFFIC_ALERT_DESCRIPTION"
    );
    assert_eq!(
        alert.alert.description.params,
        vec![
            format_date_time(now).into(),
            "2.0 KiB".to_string().into(),
            "1000 B".to_string().into(),
        ]
    );
    assert_eq!(s.alerts().len(), 1);

    // 一天只发一次：同一 identifier 已存在（含已读）时不再发布
    assert!(module.update_traffic_monitoring(now + 1000).is_none());
    assert_eq!(s.alerts().len(), 1);
}

#[test]
fn daily_threshold_boundary_and_disabled_state() {
    let now = 1_700_000_000_000i64;

    // daily <= 0（-1 或 0）：完全禁用，即使流量巨大也不告警
    for disabled in [-1i64, 0] {
        let s = sink();
        let module = active_module(s.clone(), disabled, false, 0, 0, 0);
        s.update_traffic_journal("qb", 0, 1000, 0, 0, now);
        s.update_traffic_journal("qb", 0, i64::from(i32::MAX), 0, 0, now);
        assert!(module.update_traffic_monitoring(now).is_none());
        assert!(s.alerts().is_empty());
    }

    // 低于阈值（1999 < 2000）：不告警
    let below = sink();
    let module = active_module(below.clone(), 2000, false, 0, 0, 0);
    below.update_traffic_journal("qb", 0, 0, 0, 0, now);
    below.update_traffic_journal("qb", 0, 1999, 0, 0, now);
    assert!(module.update_traffic_monitoring(now).is_none());
    assert!(below.alerts().is_empty());

    // 恰好等于阈值：上游只判断 `totalBytes < dailyTrafficCapping` 才返回 -> 会告警
    let equal = sink();
    let module = active_module(equal.clone(), 2000, false, 0, 0, 0);
    equal.update_traffic_journal("qb", 0, 0, 0, 0, now);
    equal.update_traffic_journal("qb", 0, 2000, 0, 0, now);
    assert!(module.update_traffic_monitoring(now).is_some());
    assert_eq!(equal.alerts().len(), 1);
}

#[test]
fn sliding_window_limiter_math_vectors() {
    let now = 1_700_000_000_000i64;

    // 向量 1：超阈值且当前限速高于 min -> b = 已上传/阈值，新限速 = max(min, 当前/b)
    let got = compute_sliding_window_limiter(2500, 5000, 1000, 0, 10_000, now);
    assert_eq!(got.uploaded_in_window, 2500);
    assert_eq!(got.old_speed_limit, 5000);
    assert_eq!(got.new_speed_limit, 2000);
    assert_eq!(got.decrease_factor, Some(2.5));
    assert_eq!(got.increase_factor, None);
    assert_eq!(got.reached_minimum_speed, None);
    assert_eq!(got.reached_maximum_speed, None);
    assert_eq!(got.window_size_millis, SLIDING_WINDOW_MILLIS);
    assert_eq!(got.window_start_time, now - SLIDING_WINDOW_MILLIS);

    // 向量 2：超阈值且当前限速 <= min -> 取 min 并标记 reachedMinimumSpeed
    let got = compute_sliding_window_limiter(1000, 500, 1000, 600, 10_000, now);
    assert_eq!(got.new_speed_limit, 600);
    assert_eq!(got.reached_minimum_speed, Some(true));
    assert_eq!(got.decrease_factor, None);

    // 向量 3：未超阈值且当前限速已达 max（max > 0）-> 取 max，且上游不设置 increaseFactor
    let got = compute_sliding_window_limiter(250_000, 10_000, 1_000_000, 0, 10_000, now);
    assert_eq!(got.new_speed_limit, 10_000);
    assert_eq!(got.increase_factor, None);
    assert_eq!(got.decrease_factor, None);
    assert_eq!(got.reached_maximum_speed, None);

    // 向量 4：未超阈值且低于 max -> a = (阈值 - 已上传)/窗口毫秒 * 1000，新限速 = 当前 + (long)a
    let got = compute_sliding_window_limiter(250_000, 0, 1_000_000, 0, 10_000, now);
    let expected_a = 750_000f64 / SLIDING_WINDOW_MILLIS as f64 * 1000.0;
    assert_eq!(got.new_speed_limit, expected_a.trunc() as i64);
    assert_eq!(got.new_speed_limit, 8);
    assert!((got.increase_factor.unwrap() - expected_a).abs() < 1e-9);

    // 向量 5：阈值为 0 时 b 退化为 1.0（上游 `thresholdBytes > 0 ? ... : 1.0`）
    let got = compute_sliding_window_limiter(0, 5000, 0, 0, 10_000, now);
    assert_eq!(got.new_speed_limit, 5000);
    assert_eq!(got.decrease_factor, Some(1.0));

    // 向量 6：max = 0（不限速）时不进入 max 分支，取 min(0, 当前 + a)
    let got = compute_sliding_window_limiter(999_999, 100, 1_000_000, 0, 0, now);
    assert_eq!(got.new_speed_limit, 0);
    assert!(got.increase_factor.unwrap() > 0.0);
}

#[test]
fn sliding_window_capping_uses_window_traffic_and_keeps_download_limit() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    // 阈值 1000 bytes / 24h，min 0、max 10000 bytes/s
    let module = active_module(s.clone(), -1, true, 1000, 0, 10_000);
    // 窗口内已上传 2500 bytes
    s.update_traffic_journal("qb", 0, 0, 0, 0, now);
    s.update_traffic_journal("qb", 0, 2500, 0, 0, now);
    // 窗口外的历史数据（25 小时前）不参与计算
    let old = now - SLIDING_WINDOW_MILLIS - 3_600_000;
    s.update_traffic_journal("qb", 0, 0, 0, 0, old);
    s.update_traffic_journal("qb", 0, 999_999, 0, 0, old);

    let stats = vec![
        DownloaderTrafficStats::new(
            "qb",
            "qBittorrent",
            true,
            0,
            0,
            Some(SpeedLimiter {
                upload: 5000,
                download: 777,
            }),
            true,
        ),
        // 不支持限速（getSpeedLimiter() == null）-> 跳过
        DownloaderTrafficStats::new("no-limiter", "NoLimiter", true, 0, 0, None, true),
        // 无 TRAFFIC_STATS 特性标志 -> 跳过
        DownloaderTrafficStats::new(
            "no-stats",
            "NoStats",
            true,
            0,
            0,
            Some(SpeedLimiter {
                upload: 5000,
                download: 777,
            }),
            false,
        ),
        // 未登录成功 -> 跳过
        DownloaderTrafficStats::new(
            "logged-out",
            "LoggedOut",
            false,
            0,
            0,
            Some(SpeedLimiter {
                upload: 5000,
                download: 777,
            }),
            true,
        ),
    ];
    let changes = module.update_traffic_capping_service(&stats, now);
    assert_eq!(changes.len(), 1);
    let change = &changes[0];
    assert_eq!(change.downloader_id, "qb");
    assert_eq!(change.downloader_name, "qBittorrent");
    assert_eq!(change.computed.uploaded_in_window, 2500);
    assert_eq!(change.computed.decrease_factor, Some(2.5));
    assert_eq!(change.limiter.upload, 2000);
    assert_eq!(change.limiter.download, 777, "下载限速保持原值");
    assert_eq!(change.limiter.upload, change.computed.new_speed_limit);
}

#[test]
fn sliding_window_capping_respects_enable_flag() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    let disabled = active_module(s.clone(), -1, false, 1000, 0, 10_000);
    s.update_traffic_journal("qb", 0, 0, 0, 0, now);
    s.update_traffic_journal("qb", 0, 2500, 0, 0, now);
    let stats = vec![DownloaderTrafficStats::new(
        "qb",
        "qBittorrent",
        true,
        0,
        0,
        Some(SpeedLimiter {
            upload: 5000,
            download: 777,
        }),
        true,
    )];
    let (changes, alert) = disabled.on_tick(&stats, now);
    assert!(changes.is_empty());
    assert!(alert.is_none());
    assert!(disabled
        .update_traffic_capping_service(&stats, now)
        .is_empty());
}

// ---------------- session-analyse ----------------

#[test]
fn aggregation_counts_every_flag_branch() {
    let today = 1_700_000_000_000i64;
    let rows = vec![
        // I d u O H E P：非本地连接 + 我们感兴趣但被 choke + 对方感兴趣且被我们 choke
        // + optimistic + DHT + rc4 + uTP
        track_row(today, "qb", "1.2.3.4", 51413, Some("I d u O H E P")),
        // D X：我们感兴趣且未被 choke（remoteAccept）+ PEX
        track_row(today, "qb", "1.2.3.5", 51413, Some("D X")),
        // 空串（非 null）：所有位为默认值 -> fromTrackerOrOther + tcpSocket
        track_row(today, "qb", "1.2.3.6", 51413, Some("")),
        // 无 flags：只累加 totalConnections
        track_row(today, "qb", "1.2.3.7", 51413, None),
    ];
    let agg = aggregate_connection_metrics(&rows);
    assert_eq!(agg.len(), 1);
    let m = &agg[0];
    assert_eq!(m.timeframe_at_ms, today);
    assert_eq!(m.downloader, "qb");
    assert_eq!(m.total_connections, 4);
    assert_eq!(m.incoming_connections, 1); // I
    assert_eq!(m.remote_refuse_transfer_to_client, 1); // d
    assert_eq!(m.remote_accept_transfer_to_client, 1); // D
    assert_eq!(m.local_refuse_transfer_to_peer, 1); // u
    assert_eq!(m.local_accept_transfer_to_peer, 0);
    assert_eq!(m.local_not_interested, 0);
    assert_eq!(m.question_status, 0);
    assert_eq!(m.optimistic_unchoke, 1);
    assert_eq!(m.from_dht, 1);
    assert_eq!(m.from_pex, 1);
    assert_eq!(m.from_lsd, 0);
    assert_eq!(m.from_tracker_or_other, 1); // 空串所在分支
    assert_eq!(m.rc4_encrypted, 1);
    assert_eq!(m.plain_text_encrypted, 0);
    assert_eq!(m.utp_socket, 1);
    assert_eq!(m.tcp_socket, 2); // "D X" 与空串
}

#[test]
fn aggregation_groups_by_timeframe_and_downloader() {
    let day1 = 1_700_000_000_000i64;
    let day2 = day1 + 86_400_000;
    let rows = vec![
        track_row(day1, "qb", "1.2.3.4", 1, Some("D")),
        track_row(day1, "tr", "1.2.3.5", 2, Some("D")),
        track_row(day2, "qb", "1.2.3.6", 3, Some("D")),
        track_row(day1, "qb", "1.2.3.7", 4, Some("D")),
    ];
    let agg = aggregate_connection_metrics(&rows);
    // 分组缓冲顺序 = 输入顺序（与上游 findOrCreateBuffer 一致）
    assert_eq!(agg.len(), 3);
    assert_eq!(
        (
            agg[0].timeframe_at_ms,
            agg[0].downloader.as_str(),
            agg[0].total_connections
        ),
        (day1, "qb", 2)
    );
    assert_eq!(
        (
            agg[1].timeframe_at_ms,
            agg[1].downloader.as_str(),
            agg[1].total_connections
        ),
        (day1, "tr", 1)
    );
    assert_eq!(
        (
            agg[2].timeframe_at_ms,
            agg[2].downloader.as_str(),
            agg[2].total_connections
        ),
        (day2, "qb", 1)
    );
    assert!(aggregate_connection_metrics(&[]).is_empty());
}

#[test]
fn flush_data_aggregates_both_branches_and_deletes_tracks() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    let today = start_of_today_ms(now);
    let yesterday = today - 86_400_000;
    let module = SessionAnalyseServiceModule::new(
        s.clone(),
        SessionAnalyseSettings {
            cleanup_interval_ms: 3_600_000,
            data_flush_interval_ms: 3_600_000,
            data_retention_time_ms: 15_552_000_000,
        },
    );

    // 「不在今天」的旧 track 行 + 一条已存在的聚合行（overwrite=true 应被覆盖）
    s.upsert_metrics_tracks(&[track_row(yesterday, "qb", "1.2.3.4", 51413, Some("D"))]);
    s.save_connection_metrics(&[metrics_row(yesterday, "qb", 5)], true);
    // 「在今天」的 track 行 + 一条已存在的聚合行（overwrite=false 应合并）
    s.upsert_metrics_tracks(&[track_row(today, "qb", "1.2.3.5", 51413, Some("D"))]);
    s.save_connection_metrics(&[metrics_row(today, "qb", 5)], true);

    // 走 onPeersRetrieved 路径再塞一条今天的 track（握手中的 peer 被跳过）
    module.sync_peers(
        "qb",
        &torrent(),
        &[
            peer("1.2.3.6", 51413, "d"),
            handshaking_peer("1.2.3.7", 51413),
        ],
        now,
    );
    assert_eq!(module.cache_len(), 1);

    let summary = module.flush_data(now);
    assert_eq!(summary.aggregated_rows_out_of_day, 1);
    assert_eq!(summary.aggregated_rows_in_day, 1);
    // 1 条昨天的 track 行 + 2 条今天的 track 行（预置 1 条 + onPeersRetrieved 1 条）
    assert_eq!(summary.deleted, 3);
    assert_eq!(summary.saved_out_of_day[0].total_connections, 1);
    assert_eq!(summary.saved_in_day[0].total_connections, 2);
    assert!(s.metrics_tracks().is_empty(), "聚合后的 track 行全部删除");

    let metrics = s.connection_metrics();
    assert_eq!(metrics.len(), 2);
    let out_of_day = metrics
        .iter()
        .find(|m| m.timeframe_at_ms == yesterday)
        .unwrap();
    assert_eq!(out_of_day.total_connections, 1, "overwrite=true 覆盖旧值 5");
    let in_day = metrics.iter().find(|m| m.timeframe_at_ms == today).unwrap();
    assert_eq!(
        in_day.total_connections, 7,
        "overwrite=false 在旧值 5 上合并 2"
    );
}

#[test]
fn sync_peers_reuses_existing_track_row() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    let today = start_of_today_ms(now);
    let module = SessionAnalyseServiceModule::new(s.clone(), SessionAnalyseSettings::default());
    // 先有一条同键的 track 行（模拟上一次运行写入的数据）
    let key = MetricsTrackKey {
        timeframe_at_ms: today,
        downloader: "qb".to_string(),
        torrent_id: torrent().hash,
        address: "1.2.3.4".to_string(),
        port: 51413,
    };
    let mut existing = MetricsTrackRow::new(key.clone());
    existing.peer_id = Some("old".to_string());
    existing.last_flags = Some("K".to_string());
    s.upsert_metrics_tracks(&[existing]);

    module.sync_peers("qb", &torrent(), &[peer("1.2.3.4", 51413, "d u")], now);
    module.flush_all();
    let rows = s.metrics_tracks();
    assert_eq!(rows.len(), 1, "同键不新增行");
    assert_eq!(rows[0].peer_id, Some("-qB4600-abcdefghijkl".to_string()));
    assert_eq!(rows[0].client_name, Some("qBittorrent 4.6.0".to_string()));
    assert_eq!(rows[0].last_flags, Some("d u".to_string()));
    assert_eq!(rows[0].key, key);
}

#[test]
fn connection_metrics_merge_skips_local_not_interested_like_upstream() {
    let mut base = metrics_row(1_700_000_000_000, "qb", 1);
    base.local_not_interested = 4;
    let mut appender = metrics_row(1_700_000_000_000, "qb", 2);
    appender.local_not_interested = 9;
    appender.from_pex = 3;
    base.merge(&appender);
    assert_eq!(base.total_connections, 3);
    assert_eq!(base.from_pex, 3);
    // 上游 merge() 漏掉 local_not_interested
    assert_eq!(base.local_not_interested, 4);
}

#[test]
fn session_analyse_retention_boundary_is_inclusive() {
    let s = sink();
    let now = 10_000i64;
    let before = now - 1_000;
    let module = SessionAnalyseServiceModule::new(
        s.clone(),
        SessionAnalyseSettings {
            data_retention_time_ms: 1_000,
            ..Default::default()
        },
    );
    s.save_connection_metrics(
        &[
            metrics_row(before, "qb", 7),
            metrics_row(before + 1, "qb", 8),
        ],
        true,
    );
    // 对齐 `le(timeframe_at, before)`：恰好等于边界的数据也会被删除
    assert_eq!(module.cleanup(now), 1);
    let left = s.connection_metrics();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].timeframe_at_ms, before + 1);

    // 保留期为 0 时上游没有保护：before = now，删除 `timeframe_at <= now`
    let s2 = sink();
    let module =
        SessionAnalyseServiceModule::new(s2.clone(), SessionAnalyseSettings::default());
    s2.save_connection_metrics(
        &[metrics_row(now, "qb", 1), metrics_row(now + 1, "qb", 2)],
        true,
    );
    assert_eq!(module.cleanup(now), 1);
    assert_eq!(s2.connection_metrics().len(), 1);
}

// ---------------- peer-recording ----------------

fn peer_record_row(last_time_seen_ms: i64, address: &str, uploaded: i64) -> PeerRecordRow {
    PeerRecordRow {
        address: address.to_string(),
        port: 51413,
        torrent_id: "t1".to_string(),
        downloader: "qb".to_string(),
        peer_id: "-qB4600-".to_string(),
        client_name: "qBittorrent".to_string(),
        uploaded,
        uploaded_offset: uploaded,
        upload_speed: 10,
        downloaded: uploaded,
        downloaded_offset: uploaded,
        download_speed: 20,
        last_flags: Some("d".to_string()),
        first_time_seen_ms: 1_000,
        last_time_seen_ms,
        peer_geoip: None,
    }
}

#[test]
fn peer_recording_filter_matches_upstream() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    let module = PeerRecordingServiceModule::new(s.clone(), PeerRecordingSettings::default());

    // 客户端名与 peer-id 都是空白但非握手中 -> 保留
    let mut blank = peer("1.2.3.4", 51413, "");
    blank.client_name = Some("   ".to_string());
    blank.peer_id = Some(String::new());
    // 客户端名与 peer-id 为空、非握手中 -> 保留
    let mut no_client = peer("1.2.3.5", 51413, "d");
    no_client.client_name = None;
    no_client.peer_id = None;
    // 客户端名与 peer-id 为空 + 握手中 -> 丢弃
    let mut dropped = handshaking_peer("1.2.3.6", 51413);
    dropped.client_name = Some(String::new());
    dropped.peer_id = None;
    // 客户端名与 peer-id 为空但握手中为假（有速率）-> 保留
    let mut kept_by_traffic = handshaking_peer("1.2.3.7", 51413);
    kept_by_traffic.up_speed = 10;

    module.on_peers_retrieved(
        "qb",
        &torrent(),
        &[blank, no_client, dropped, kept_by_traffic],
        now,
    );
    assert_eq!(module.cache_len(), 3);
}

#[test]
fn peer_recording_flush_does_not_double_count() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    let module = PeerRecordingServiceModule::new(s.clone(), PeerRecordingSettings::default());
    module.on_peers_retrieved("qb", &torrent(), &[peer("1.2.3.4", 51413, "d u")], now);
    module.flush();
    let rows = s.peer_records();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.address, "1.2.3.4");
    assert_eq!(row.port, 51413);
    assert_eq!(row.torrent_id, torrent().hash);
    assert_eq!(row.peer_id, "-qB4600-", "peer-id 截断到 8 字符");
    assert_eq!(row.client_name, "qBittorrent 4.6.0");
    assert_eq!(row.uploaded, 8192);
    assert_eq!(row.uploaded_offset, 8192);
    assert_eq!(row.upload_speed, 2048);
    assert_eq!(row.downloaded, 4096);
    assert_eq!(row.downloaded_offset, 4096);
    assert_eq!(row.download_speed, 1024);
    assert_eq!(row.last_flags, Some("d u".to_string()));
    assert_eq!(row.first_time_seen_ms, now);
    assert_eq!(row.last_time_seen_ms, now);

    // 同一时间戳再次 flush：last_time_seen 不早于旧值 -> 按 delta 累加，结果不变
    module.on_peers_retrieved("qb", &torrent(), &[peer("1.2.3.4", 51413, "d u")], now);
    module.flush();
    let rows = s.peer_records();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].uploaded, 8192, "重复 flush 不会重复计数");
    assert_eq!(rows[0].downloaded, 4096);
    assert_eq!(rows[0].first_time_seen_ms, now, "first_time_seen 永不更新");

    // 上报值增长时按增量累加
    let mut later = peer("1.2.3.4", 51413, "d u");
    later.uploaded = 10_000;
    later.downloaded = 5_000;
    module.on_peers_retrieved("qb", &torrent(), &[later], now + 60_000);
    module.flush();
    let rows = s.peer_records();
    assert_eq!(rows[0].uploaded, 8192 + (10_000 - 8192));
    assert_eq!(rows[0].downloaded, 4096 + (5_000 - 4096));
    assert_eq!(rows[0].uploaded_offset, 10_000);
    assert_eq!(rows[0].last_time_seen_ms, now + 60_000);
}

#[test]
fn peer_recording_snapshot_equality_matches_lombok_equals() {
    let snapshot =
        PeerRecordCachingEntire::new(1_000, "qb", &torrent(), &peer("1.2.3.4", 51413, "d"));
    assert!(snapshot.same_as(&snapshot.clone()));

    let mut dirty_changed = snapshot.clone();
    dirty_changed.dirty = false;
    assert!(!snapshot.same_as(&dirty_changed));

    let mut time_changed = snapshot.clone();
    time_changed.timestamp_ms = 2_000;
    assert!(!snapshot.same_as(&time_changed));

    let mut peer_changed = snapshot.clone();
    peer_changed.peer.uploaded += 1;
    assert!(!snapshot.same_as(&peer_changed));

    let mut torrent_changed = snapshot.clone();
    torrent_changed.torrent.progress = 0.9;
    assert!(!snapshot.same_as(&torrent_changed));
}

#[test]
fn peer_record_upsert_case_expression_matches_sql() {
    let existing = peer_record_row(2_000, "1.2.3.4", 1_000);

    // 上报时间更旧：整行保持旧值
    let mut stale = existing.clone();
    stale.last_time_seen_ms = 1_000;
    stale.uploaded = 5_000;
    stale.peer_id = "new".to_string();
    let merged = apply_peer_record_upsert(&existing, &stale);
    assert_eq!(merged.uploaded, 1_000);
    assert_eq!(merged.uploaded_offset, 1_000);
    assert_eq!(merged.peer_id, "-qB4600-");
    assert_eq!(merged.last_time_seen_ms, 2_000);

    // 增量 >= 0：uploaded = 旧值 + (本次上报 - 旧偏移量)
    let mut advanced = existing.clone();
    advanced.last_time_seen_ms = 3_000;
    advanced.uploaded = 1_500;
    let merged = apply_peer_record_upsert(&existing, &advanced);
    assert_eq!(merged.uploaded, 1_500);
    assert_eq!(merged.uploaded_offset, 1_500);
    assert_eq!(merged.last_time_seen_ms, 3_000);

    // 增量 < 0（对方计数被重置）：uploaded = 旧值 + 本次上报值
    let mut reset = existing.clone();
    reset.last_time_seen_ms = 3_000;
    reset.uploaded = 200;
    let merged = apply_peer_record_upsert(&existing, &reset);
    assert_eq!(merged.uploaded, 1_200);
    assert_eq!(merged.uploaded_offset, 200);

    // first_time_seen / peer_geoip 永不更新
    let mut incoming = existing.clone();
    incoming.last_time_seen_ms = 3_000;
    incoming.first_time_seen_ms = 9_999;
    incoming.peer_geoip = Some("{\"country\":\"CN\"}".to_string());
    let merged = apply_peer_record_upsert(&existing, &incoming);
    assert_eq!(merged.first_time_seen_ms, 1_000);
    assert_eq!(merged.peer_geoip, None);
}

#[test]
fn peer_recording_retention_boundary_is_strict() {
    let s = sink();
    let now = 10_000i64;
    let module = PeerRecordingServiceModule::new(
        s.clone(),
        PeerRecordingSettings {
            data_retention_time_ms: 1_000,
            data_cleanup_interval_ms: 604_800_000,
            data_flush_interval_ms: 900_000,
        },
    );
    // 对齐 `lt(last_time_seen, before)`：恰好等于边界保留，早 1ms 删除
    s.upsert_peer_record(&peer_record_row(9_000, "1.2.3.4", 100));
    s.upsert_peer_record(&peer_record_row(8_999, "1.2.3.5", 100));
    assert_eq!(module.cleanup(now), Some(1));
    let left = s.peer_records();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].address, "1.2.3.4");

    // `data-retention-time <= 0` 时上游直接 return（不删任何数据）
    let s2 = sink();
    s2.upsert_peer_record(&peer_record_row(1, "1.2.3.4", 100));
    let disabled =
        PeerRecordingServiceModule::new(s2.clone(), PeerRecordingSettings::default());
    assert_eq!(disabled.cleanup(now), None);
    assert_eq!(s2.peer_records().len(), 1);
}

// ---------------- swarm-tracking ----------------

fn swarm_module(s: Arc<InMemoryMonitorSink>) -> SwarmTrackingModule {
    SwarmTrackingModule::new(
        s,
        SwarmTrackingSettings {
            data_flush_interval_ms: 3_600_000,
        },
    )
}

#[test]
fn swarm_tracking_records_deltas_and_speed_maxes() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    let module = swarm_module(s.clone());
    let mut first = peer("1.2.3.4", 51413, "d u");
    first.downloaded = 1_000;
    first.uploaded = 2_000;
    first.dl_speed = 100;
    first.up_speed = 50;
    module.sync_peers("qb", &torrent(), &[first.clone()], now);
    module.flush_all();

    let rows = s.tracked_swarm();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(module.count(), 1);
    assert_eq!(row.ip, "1.2.3.4");
    assert_eq!(row.info_hash, torrent().hash);
    assert_eq!(row.downloader, "qb");
    assert_eq!(row.downloaded, 1_000, "首次 offset 为 0 -> 差值 = 上报值");
    assert_eq!(row.uploaded, 2_000);
    assert_eq!(row.downloaded_offset, 1_000);
    assert_eq!(row.uploaded_offset, 2_000);
    assert_eq!(row.download_speed, 100);
    assert_eq!(row.upload_speed, 50);
    assert_eq!(row.download_speed_max, 100);
    assert_eq!(row.upload_speed_max, 50);
    assert_eq!(row.last_flags, "d u");
    assert_eq!(row.peer_id, "-qB4600-abcdefghijkl");
    assert_eq!(row.client_name, "qBittorrent 4.6.0");
    assert_eq!(row.torrent_size, torrent().total_size);
    assert_eq!(row.torrent_is_private, Some(false));
    assert_eq!(row.first_time_seen_ms, now);
    assert_eq!(row.last_time_seen_ms, now);
    assert!(row.dirty);

    // 第二次采样：计数增长按差值累加；速度只更新历史最大值（download_speed 不更新）
    let mut second = first.clone();
    second.downloaded = 1_500;
    second.uploaded = 2_600;
    second.dl_speed = 10;
    second.up_speed = 500;
    module.sync_peers("qb", &torrent(), &[second.clone()], now + 60_000);
    module.flush_all();
    let rows = s.tracked_swarm();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.downloaded, 1_500);
    assert_eq!(row.uploaded, 2_600);
    assert_eq!(row.downloaded_offset, 1_500);
    assert_eq!(row.uploaded_offset, 2_600);
    assert_eq!(row.download_speed_max, 100);
    assert_eq!(row.upload_speed_max, 500);
    assert_eq!(row.download_speed, 100, "上游只更新 max，不更新当前速度");
    assert_eq!(row.last_time_seen_ms, now + 60_000);
    assert_eq!(row.first_time_seen_ms, now);

    // 计数被重置（新值 < offset）：按整值累加
    let mut reset = second.clone();
    reset.downloaded = 50;
    reset.uploaded = 10;
    module.sync_peers("qb", &torrent(), &[reset], now + 120_000);
    module.flush_all();
    let row = &s.tracked_swarm()[0];
    assert_eq!(row.downloaded, 1_550);
    assert_eq!(row.uploaded, 2_610);
}

#[test]
fn swarm_tracking_skips_handshaking_peers() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    let module = swarm_module(s.clone());
    module.sync_peers(
        "qb",
        &torrent(),
        &[
            handshaking_peer("1.2.3.9", 51413),
            peer("1.2.3.8", 51413, "d"),
        ],
        now,
    );
    assert_eq!(module.cache_len(), 1);
    module.flush_all();
    let rows = s.tracked_swarm();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ip, "1.2.3.8");
}

#[test]
fn swarm_tracking_reuses_existing_row_from_sink() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    let module = swarm_module(s.clone());
    let mut base = peer("1.2.3.4", 51413, "d u");
    base.downloaded = 800;
    base.uploaded = 1_000;
    // 模拟上一次运行留下的行：已累计 700/900，偏移量为当时的上报值 800/1000
    let mut existing = new_tracked_swarm_row("qb", &torrent(), &base, now - 500_000);
    existing.downloaded = 700;
    existing.uploaded = 900;
    existing.downloaded_offset = 800;
    existing.uploaded_offset = 1_000;
    existing.first_time_seen_ms = now - 500_000;
    existing.last_time_seen_ms = now - 500_000;
    s.upsert_tracked_swarm(&existing);

    // cache-miss：从库里取回旧行（offset 来自上一次运行），继续累加
    let mut current = peer("1.2.3.4", 51413, "d u");
    current.downloaded = 1_500;
    current.uploaded = 2_600;
    module.sync_peers("qb", &torrent(), &[current], now);
    module.flush_all();
    let rows = s.tracked_swarm();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.downloaded, 700 + (1_500 - 800));
    assert_eq!(row.uploaded, 900 + (2_600 - 1_000));
    assert_eq!(
        row.first_time_seen_ms,
        now - 500_000,
        "复用旧行的首次出现时间"
    );
    assert_eq!(row.last_time_seen_ms, now);
}

#[test]
fn swarm_tracking_restart_clears_data() {
    let s = sink();
    let now = 1_700_000_000_000i64;
    let module = swarm_module(s.clone());
    module.sync_peers("qb", &torrent(), &[peer("1.2.3.4", 51413, "d")], now);
    module.flush_all();
    assert_eq!(module.count(), 1);

    // 模拟重启：onEnable -> resetTable（临时表清空、缓存清空）
    module.on_enable();
    assert_eq!(module.count(), 0);
    assert!(s.tracked_swarm().is_empty());
    assert_eq!(module.cache_len(), 0);
    // 重启后 flush 不会把旧数据写回
    module.flush_all();
    assert!(s.tracked_swarm().is_empty());

    // 新会话重新开始累积
    module.sync_peers("qb", &torrent(), &[peer("1.2.3.4", 51413, "d")], now + 1);
    module.flush_all();
    assert_eq!(module.count(), 1);
    assert_eq!(s.tracked_swarm()[0].downloaded, 4096);
    assert_eq!(s.tracked_swarm()[0].first_time_seen_ms, now + 1);
}

/// 上游 `peer.getId().length() > 8` 是 UTF-16 码元数（Java `String.length()`）。
#[test]
fn truncate_peer_id_uses_utf16_code_units() {
    // 直接以 UTF-16 码元序列构造输入，规避源码字面量的编码歧义：
    // a b c [D83D DE00](😀) e f g h i = 10 个码元
    let units: Vec<u16> = vec![
        b'a' as u16,
        b'b' as u16,
        b'c' as u16,
        0xD83D,
        0xDE00,
        b'e' as u16,
        b'f' as u16,
        b'g' as u16,
        b'h' as u16,
        b'i' as u16,
    ];
    let input = String::from_utf16_lossy(&units);
    // 保留前 8 个码元：索引 0..7 = a b c [D83D DE00] e f g = "abc😀efg"
    let got = truncate_peer_id(Some(&input));
    assert_eq!(
        got.encode_utf16().collect::<Vec<u16>>(),
        vec![
            b'a' as u16,
            b'b' as u16,
            b'c' as u16,
            0xD83D,
            0xDE00,
            b'e' as u16,
            b'f' as u16,
            b'g' as u16,
        ],
        "应按 UTF-16 码元（而非 Unicode 标量）截断到 8"
    );

    // 纯 ASCII：超过 8 字符截断（保留前 8 个码元 = "-qB5000-"）
    assert_eq!(truncate_peer_id(Some("-qB5000-000000000000")), "-qB5000-");
    // 不超过 8 字符原样返回
    assert_eq!(truncate_peer_id(Some("-qB5000")), "-qB5000");
    // None 返回空串
    assert_eq!(truncate_peer_id(None), "");
}
