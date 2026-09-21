mod config;
mod monitor;
mod push;
mod rulesub;
mod wave;

use clap::Parser;
use pbh_core::auto_stun::{AutoStunRefresher, DEFAULT_REFRESH_INTERVAL, DEFAULT_STUN_TIMEOUT};
use pbh_core::banlist::BannedRecord;
use pbh_core::geoip::{geoip_force_disabled, GeoIpDb, GeoIpProvider};
use pbh_core::modules::progress_cheat::PcbEntityKind;
use pbh_core::modules::{BtnNetworkOnline, MonitorSink, ProgressCheatBlocker};
use pbh_db::{Database, DbMonitorSink};
use pbh_downloader::aria2::{Aria2Config, Aria2Downloader};
use pbh_downloader::biglybt::{BiglyBtConfig, BiglyBtDownloader};
use pbh_downloader::bitcomet::{BitCometConfig, BitCometDownloader};
use pbh_downloader::deluge::{DelugeConfig, DelugeDownloader};
use pbh_downloader::http::ReqwestFetcher;
use pbh_downloader::qbittorrent::{QBConfig, QBittorrentDownloader};
use pbh_downloader::transmission::{TRConfig, TransmissionDownloader};
use pbh_web::{build_router, AppState, DownloaderStatus, Metrics};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{info, warn};
use wave::{DownloaderEntry, WaveEngine};

#[derive(Parser, Debug)]
#[command(name = "pbh", version, about = "PeerBanHelper Rust faithful rewrite")]
struct Args {
    /// 数据目录（配置、数据库、静态资源）
    #[arg(long, default_value = "./data")]
    data: PathBuf,
    /// 覆盖监听端口
    #[arg(long)]
    port: Option<u16>,
    /// 覆盖监听地址
    #[arg(long)]
    address: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,pbh=debug,reqwest=warn".into()),
        )
        .init();

    let args = Args::parse();
    let data_dir = args.data.clone();
    std::fs::create_dir_all(&data_dir)?;

    let (mut cfg, _cfg_path) = config::AppConfig::load_or_create(&data_dir)?;
    if let Some(port) = args.port {
        cfg.server.http = port;
    }
    if let Some(addr) = args.address {
        cfg.server.address = addr;
    }

    // 数据库
    let db_path = data_dir.join("peerbanhelper.db");
    let db = Arc::new(Database::open(
        db_path.to_str().unwrap_or("peerbanhelper.db"),
    )?);
    info!("SQLite 已打开: {}", db_path.display());

    // 内置 NAT（AutoSTUN）：`ip-remapping.auto-stun.enabled=false` 时**严格 no-op**
    //（对齐上游 `BTStunManager.load()`：不注册任何 provider，翻译恒直通）。
    // 启用时构造映射表挂到 remap 配置上，并起后台线程每 5 秒刷新一次隧道映射
    //（对齐 `BTStunInstance` 的 `scheduleWithFixedDelay(this::restart, 0, 5, SECONDS)`）。
    let _auto_stun_refresher: Option<AutoStunRefresher> = if cfg.ip_remapping.auto_stun.enabled {
        let registry = cfg.ip_remapping.auto_stun.build();
        let servers = cfg.ip_remapping.auto_stun.tcp_servers.clone();
        if servers.is_empty() {
            warn!("auto-stun.enabled=true 但 stun.tcp-servers 为空，翻译将保持直通");
        }
        let refresher = registry.clone().spawn_refresher(
            servers,
            // 上游 `StunTcpTunnelImpl` 固定以 "0.0.0.0" 为源地址；隧道端口交给系统分配
            "0.0.0.0".to_string(),
            0,
            DEFAULT_REFRESH_INTERVAL,
            DEFAULT_STUN_TIMEOUT,
        );
        cfg.ip_remapping = cfg.ip_remapping.clone().with_auto_stun(registry);
        info!(
            "AutoSTUN 已启用（下载器: {:?}，友好回环映射: {}）",
            cfg.ip_remapping.auto_stun.downloaders,
            cfg.ip_remapping.auto_stun.use_friendly_loopback_mapping
        );
        Some(refresher)
    } else {
        None
    };

    // 下载器
    let remap = cfg.remap_config();
    let blocklist_url = format!(
        "http://{}:{}/blocklist/p2p-plain-format",
        cfg.server.public_host(),
        cfg.server.http
    );
    let mut entries: Vec<DownloaderEntry> = Vec::new();
    for d in &cfg.downloaders {
        match d.kind.as_str() {
            "qbittorrent" => {
                let qb_cfg = QBConfig {
                    id: d.name.clone(),
                    name: d.name.clone(),
                    endpoint: d.endpoint.clone(),
                    username: d.username.clone(),
                    password: d.password.clone(),
                    api_key: d.api_key.clone(),
                    basic_auth: None,
                    verify_tls: d.verify_tls,
                    ignore_private: d.ignore_private,
                    increment_ban: d.increment_ban,
                    disable_same_ip_multi_connection: true,
                    max_concurrent_slots: 128,
                    remap: remap.clone(),
                };
                match QBittorrentDownloader::new(qb_cfg) {
                    Ok(dl) => entries.push(DownloaderEntry {
                        downloader: Arc::new(dl),
                        increment_ban: d.increment_ban,
                    }),
                    Err(e) => warn!("下载器 {} 初始化失败: {e}", d.name),
                }
            }
            "transmission" => {
                let tr_cfg = TRConfig {
                    id: d.name.clone(),
                    name: d.name.clone(),
                    endpoint: d.endpoint.clone(),
                    username: d.username.clone(),
                    password: d.password.clone(),
                    verify_ssl: d.verify_tls,
                    rpc_url: d.resolved_rpc_url("/transmission/rpc"),
                    ignore_private: d.ignore_private,
                    paused: false,
                    blocklist_url: blocklist_url.clone(),
                };
                match TransmissionDownloader::new(tr_cfg, remap.clone()) {
                    Ok(dl) => entries.push(DownloaderEntry {
                        downloader: Arc::new(dl),
                        // Transmission 只支持整份 blocklist 更新，必须走全量路径（对齐上游 setBanList）
                        increment_ban: false,
                    }),
                    Err(e) => warn!("下载器 {} 初始化失败: {e}", d.name),
                }
            }
            "deluge" => {
                let deluge_cfg = DelugeConfig {
                    id: d.name.clone(),
                    name: d.name.clone(),
                    endpoint: d.endpoint.clone(),
                    password: d.password.clone(),
                    verify_ssl: d.verify_tls,
                    // 上游 Deluge 的 rpc-url 默认 /json
                    rpc_url: d.resolved_rpc_url("/json"),
                    paused: d.paused,
                    remap: remap.clone(),
                };
                match DelugeDownloader::new(deluge_cfg) {
                    Ok(dl) => entries.push(DownloaderEntry {
                        downloader: Arc::new(dl),
                        // 上游 Deluge 支持增量封禁（peerbanhelperadapter.ban_ips）
                        increment_ban: d.increment_ban,
                    }),
                    Err(e) => warn!("下载器 {} 初始化失败: {e}", d.name),
                }
            }
            // 上游配置节的 `type` 为 `aria2next`；`aria2` 作为等价别名一并接受
            "aria2next" | "aria2" => {
                let aria2_cfg = Aria2Config {
                    id: d.name.clone(),
                    name: d.name.clone(),
                    endpoint: d.endpoint.clone(),
                    // RPC 密钥：既进 `Authorization: token:<secret>` 头，也进 params 首元素
                    token: d.token.clone(),
                    verify_ssl: d.verify_tls,
                    // 上游 Aria2Next 的默认值是「不忽略私有种子」（且该字段实际未被使用）
                    ignore_private: d.ignore_private,
                    paused: d.paused,
                    remap: remap.clone(),
                };
                match Aria2Downloader::new(aria2_cfg) {
                    Ok(dl) => entries.push(DownloaderEntry {
                        downloader: Arc::new(dl),
                        // Aria2Next 没有增量封禁 API：上游 `setBanList` 忽略 added/removed，
                        // 恒为整份列表替换（`aria2.setBtPeerBlocklist`），必须固定全量路径
                        increment_ban: false,
                    }),
                    Err(e) => warn!("下载器 {} 初始化失败: {e}", d.name),
                }
            }
            "biglybt" => {
                let biglybt_cfg = BiglyBtConfig {
                    id: d.name.clone(),
                    name: d.name.clone(),
                    endpoint: d.endpoint.clone(),
                    token: d.token.clone(),
                    increment_ban: d.increment_ban,
                    verify_ssl: d.verify_tls,
                    // 上游 BiglyBT 的默认值是「不忽略私有种子」
                    ignore_private: d.ignore_private,
                    paused: d.paused,
                    remap: remap.clone(),
                };
                match BiglyBtDownloader::new(biglybt_cfg) {
                    Ok(dl) => entries.push(DownloaderEntry {
                        downloader: Arc::new(dl),
                        // 上游 BiglyBT 支持增量封禁（POST /bans）
                        increment_ban: d.increment_ban,
                    }),
                    Err(e) => warn!("下载器 {} 初始化失败: {e}", d.name),
                }
            }
            "bitcomet" => {
                let bitcomet_cfg = BitCometConfig {
                    id: d.name.clone(),
                    name: d.name.clone(),
                    endpoint: d.endpoint.clone(),
                    username: d.username.clone(),
                    password: d.password.clone(),
                    increment_ban: d.increment_ban,
                    verify_ssl: d.verify_tls,
                    // 上游 BitComet 的默认值是「不忽略私有种子」
                    ignore_private: d.ignore_private,
                    paused: d.paused,
                    remap: remap.clone(),
                };
                match BitCometDownloader::new(bitcomet_cfg) {
                    Ok(dl) => entries.push(DownloaderEntry {
                        downloader: Arc::new(dl),
                        // 上游 v9.5.1 的 BitComet 恒走整份替换（setBanListFull）：
                        // 增量导入（import_type=merge）自 BitComet 2.11 起不再支持，
                        // 上游的增量分支因此被删除（登录门槛已是 2.18），必须固定全量路径
                        increment_ban: false,
                    }),
                    Err(e) => warn!("下载器 {} 初始化失败: {e}", d.name),
                }
            }
            other => warn!("暂不支持的下载器类型: {other}（跳过）"),
        }
    }
    info!("已加载 {} 个下载器", entries.len());

    // GeoIP 数据源（对齐上游 `@Autowired IPDBManager`）：
    // `pbh.forceDisableIPDB` 打开、或 `<data>/ipdb/geoip/*.mmdb` 缺失/损坏时不注入 provider，
    // 此时 `ip-address-blocker` 的 asn / region / city / net-type 四个维度全部不命中
    //（与上游 IPDB 初始化失败的降级行为一致）。
    let geo: Option<Arc<dyn GeoIpProvider>> = if geoip_force_disabled() {
        info!("pbh.forceDisableIPDB 已打开，跳过 GeoIP 数据库加载");
        None
    } else {
        let ipdb_dir = data_dir.join("ipdb");
        // GeoIP 数据库自动更新（对齐上游 `IPDBManager#setupIPDB` → `IPDB` 构造函数的
        // 「先 updateMMDB 再 loadMMDB」：更新完成后再加载，本次启动即可用上新库）。
        // 镜像顺序、`.mmdb.xz` + XZ 解压、45 天 mtime 间隔、原子替换均见 `geoip_update`。
        // `ip-database.auto-update` 为 false 时严格 no-op（连本地缺失也不下载）；
        // 更新跑在阻塞线程上，因此 `reqwest::blocking` 不会踩到 tokio runtime。
        if cfg.ip_database.auto_update {
            let updater_dir = ipdb_dir.clone();
            let updater_config = cfg.ip_database.clone();
            match tokio::task::spawn_blocking(move || -> anyhow::Result<
                pbh_core::geoip_update::UpdateReport,
            > {
                let http = pbh_core::geoip_update::ReqwestBlockingHttpClient::new()?;
                Ok(
                    pbh_core::geoip_update::GeoIpUpdater::new(updater_dir, updater_config, &http)
                        .update_if_needed(),
                )
            })
            .await
            {
                Ok(Ok(report)) => {
                    let (updated, failed) = (report.updated().len(), report.failures().len());
                    if updated > 0 || failed > 0 {
                        info!("GeoIP 数据库更新检查完成: 更新 {updated} 个 / 失败 {failed} 个");
                    }
                }
                Ok(Err(e)) => warn!("GeoIP 下载客户端初始化失败，跳过自动更新: {e}"),
                Err(e) => warn!("GeoIP 更新任务异常退出: {e}"),
            }
        }
        match GeoIpDb::load(&ipdb_dir) {
            Ok(db) => {
                info!("GeoIP 数据库已加载: {}", ipdb_dir.display());
                Some(Arc::new(db))
            }
            Err(e) => {
                info!("GeoIP 数据库不可用，ASN/地区/城市/网络类型维度全部不命中: {e}");
                None
            }
        }
    };

    // 判定流水线：由 profile 段驱动（模块开关/ban-duration/规则集/bypass 地址/GeoIP 注入）
    let pipeline = Arc::new(cfg.profile.build_pipeline_with_geo(geo.clone()));
    {
        let names: Vec<&str> = pipeline.modules.iter().map(|m| m.config_name()).collect();
        if names.is_empty() {
            warn!(
                "没有任何规则模块被启用：请检查 config.yml 的 profile.module 段（与上游一致，\
                 配置节缺失或缺少 enabled: true 的模块会被禁用）"
            );
        } else {
            info!("已启用规则模块: {}", names.join(", "));
        }
    }

    // BTN：规则集/IP 白黑名单全部由 BTN 服务器下发，本实现尚未提供传输层
    //（上游 `BtnNetwork` 的握手、abilities 调度、PoW captcha 均未移植，见 PLAN.md 已知缺口）。
    // 未注入任何规则时模块恒 `pass()` —— 启用它不会造成任何封禁。
    if let Some(btn) = pipeline.module_as::<BtnNetworkOnline>("btn") {
        if !btn.is_manager_initialized() {
            info!(
                "BTN 模块已启用（ban-duration={}ms）但客户端未初始化：尚无 BTN 传输层，\
                 规则注入前该模块恒 pass、绝不封禁",
                btn.ban_duration_ms
            );
        }
    }

    // 文案表：内嵌上游 lang 资源，允许 data/lang 覆盖（与 wave 共用，统一 locale 渲染）
    let translator = Arc::new(pbh_core::i18n::Translator::with_overrides(Some(
        &data_dir.join("lang"),
    )));

    // 告警推送：渠道全部来自 `push:` 段（键 = 渠道名，段内 `type` 决定类型）；
    // 段为空 ⇒ 空渠道列表（不产生任何网络流量、不报错）。
    // HTTP 客户端对齐上游 `HTTPUtil.newBuilder()`：校验 TLS（上游默认不跳过校验）、
    // 连接超时 15s；上游未设读超时，这里为不阻塞 ban wave 额外设置了 60s 总超时。
    let push_fetcher = Arc::new(ReqwestFetcher::new(true, 15, 60)?);
    let push_manager = Arc::new(push::PushManager::from_config(&cfg.push, push_fetcher));
    match push_manager.provider_list() {
        [] => info!("未配置推送渠道（config.yml 的 push: 段为空）"),
        providers => {
            let names: Vec<String> = providers
                .iter()
                .map(|p| format!("{}（{}）", p.name(), p.config_type()))
                .collect();
            info!("已加载 {} 个推送渠道: {}", names.len(), names.join(", "));
        }
    }
    let alert_manager = Arc::new(push::AlertManager::new(
        push_manager,
        translator.clone(),
        cfg.language.locale.clone(),
    ));

    // 监控模块（`active-monitoring` / `peer-analyse-service.*`，上游非 RuleFeatureModule，
    // 不参与 peer 判定）：由 ban wave 循环按上游定时任务间隔驱动。
    //
    // 落点为 SQLite（与其余持久化共用同一个 `Database`）：表结构逐条对齐上游
    // `alert` / `traffic_journal_v3` / `peer_connection_metrics(_track)` / `peer_records` /
    // `tracked_swarm`；`peer_records.peer_geoip` 由 sink 内的 IP 库查询填充（对齐上游在 DAO 内查询）。
    // 单元测试仍可用 pbh-core 的内存实现 `InMemoryMonitorSink`。
    let monitor_sink: Arc<dyn MonitorSink> = Arc::new(DbMonitorSink::with_geo(db.clone(), geo));
    // `tracked_swarm` 是「本次运行会话」的临时表：启动时清空一次
    //（对齐上游 `SwarmTrackingModule.onEnable` 的 `TrackedSwarmService.resetTable()`）。
    monitor_sink.reset_tracked_swarm();
    // active-monitoring 的日流量阈值告警除落库外还经推送渠道分发
    //（对齐上游 `AlertManagerImpl.publishAlert(push=true)` 的推送分支）。
    let monitor = Arc::new(
        monitor::MonitorHost::new(&cfg.profile, monitor_sink)
            .with_alert_manager(alert_manager.clone()),
    );
    let monitor_names = monitor.config_names();
    match monitor_names.as_slice() {
        [] => info!(
            "未启用任何监控模块（对应 config.yml 的 profile.module.active-monitoring / \
             profile.module.peer-analyse-service 段）"
        ),
        names => info!(
            "已启用监控模块: {}（落点：SQLite 监控表，Web API: /api/modules/swarm-tracking、/api/alerts）",
            names.join(", ")
        ),
    }

    // PCB 历史恢复（对齐上游 `enable-persist` + `loadFromDatabase` 的落库分支）
    if let Some(pcb) = pipeline.module_as::<ProgressCheatBlocker>("progress-cheat-blocker") {
        let mut rows = db.load_pcb_rows(PcbEntityKind::Addr).unwrap_or_default();
        rows.extend(db.load_pcb_rows(PcbEntityKind::Range).unwrap_or_default());
        info!("已恢复 {} 条 PCB 记录", rows.len());
        pcb.load_persisted(rows);
    }

    // 内存封禁表：从数据库恢复未到期条目（对齐上游 loadBanListToMemory）
    // 封禁表由 pipeline 持有，auto-range-ban 模块会读取它
    if cfg.persist.banlist {
        let now = chrono::Utc::now().timestamp_millis();
        let records: Vec<BannedRecord> = db
            .list_banned_ips()?
            .into_iter()
            .filter(|b| b.ban_until <= 0 || b.ban_until > now)
            .map(|b| BannedRecord {
                ip: b.ip,
                unban_at_ms: b.ban_until,
                module: b.module,
                ban_for_disconnect: false,
            })
            .collect();
        info!("已从数据库恢复 {} 条封禁记录", records.len());
        if let Ok(mut list) = pipeline.ban_list.lock() {
            list.load(records);
        }
    }

    // Web 状态
    let metrics = Arc::new(Mutex::new(Metrics::default()));
    let statuses = Arc::new(Mutex::new(Vec::<DownloaderStatus>::new()));
    let state = AppState {
        db: db.clone(),
        token: Arc::new(Mutex::new(cfg.server.token.clone())),
        metrics: metrics.clone(),
        started: Arc::new(std::time::Instant::now()),
        downloaders: statuses.clone(),
        static_dir: Arc::new(Mutex::new(Some(data_dir.join("static")))),
        ban_list: pipeline.ban_list.clone(),
        remap: Arc::new(remap.clone()),
        translator: translator.clone(),
        locale: cfg.language.locale.clone(),
    };
    let app = build_router(state);
    let bind: SocketAddr = format!("{}:{}", cfg.server.address, cfg.server.http).parse()?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    info!("Web 服务监听: http://{bind}");
    let server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            warn!("web server error: {e}");
        }
    });

    info!("文案语言: {}", cfg.language.locale);

    // IP 黑名单规则订阅：启动立即拉取一次，之后按 check-interval 刷新
    // （对齐上游 `registerScheduledTask(this::reloadConfig, 0, checkInterval)`）
    if let Some(rulesub_cfg) = cfg.profile.module.ip_rule_list.clone() {
        let pipeline = pipeline.clone();
        let data_dir = data_dir.clone();
        tokio::spawn(async move {
            let http = match reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .user_agent("PeerBanHelper-RS")
                .build()
            {
                Ok(client) => client,
                Err(e) => {
                    warn!("规则订阅 HTTP 客户端创建失败: {e}");
                    return;
                }
            };
            let interval = Duration::from_millis(rulesub_cfg.check_interval_ms.max(60_000) as u64);
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now(), interval);
            loop {
                ticker.tick().await;
                for line in rulesub::refresh_all(&pipeline, &rulesub_cfg, &data_dir, &http).await {
                    info!("{line}");
                }
                rulesub::log_summary(&pipeline);
            }
        });
    }

    // ban wave 引擎
    let engine = WaveEngine {
        entries,
        pipeline,
        db: db.clone(),
        metrics,
        statuses,
        translator,
        locale: cfg.language.locale.clone(),
        persist_banlist: cfg.persist.banlist,
        max_concurrent: 128,
        alert_manager,
        monitor,
    };

    let period = Duration::from_millis(cfg.profile.check_interval.max(500));
    let mut ticker = tokio::time::interval(period);
    // 对齐上游 `scheduleWithFixedDelay`：上一轮结束后再等一个间隔，
    // 避免某轮耗时超过间隔时立刻补跑（突发）导致下载器被连续冲击。
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let cleanup_days = cfg.persist.ban_logs_keep_days;
    let mut wave_count: u64 = 0;
    // PCB 过期清理：对齐上游 `scheduleWithFixedDelay(this::cleanDatabase, 0, 8, HOURS)`
    let pcb_cleanup_period = Duration::from_secs(8 * 3600);
    let mut next_pcb_cleanup = std::time::Instant::now();

    info!("ban wave 间隔: {:?}", period);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = chrono::Utc::now().timestamp_millis();
                let report = engine.run_once(now).await;
                wave_count += 1;
                info!(
                    "wave#{}: 在线下载器={} torrents={} peers={} 封禁={} 解封={} 跳过={} 错误={}",
                    wave_count, report.online_downloaders, report.torrents, report.peers,
                    report.banned, report.unbanned, report.skipped, report.errors.len()
                );
                for e in &report.errors {
                    warn!("wave error: {e}");
                }
                // 监控模块的定时任务（内部按各模块的上游间隔判断到期；首次 delay 0 立即执行）：
                // active-monitoring 每 1 分钟 updateTrafficStatus、session-analyse /
                // peer-recording / swarm-tracking 按各自的 flush / cleanup 间隔。
                engine.monitor.run_scheduled(&engine.entries, now).await;
                // 每 100 轮清理一次过期日志
                if wave_count.is_multiple_of(100) && cleanup_days > 0 {
                    if let Ok(n) = db.cleanup_ban_logs(cleanup_days) {
                        if n > 0 { info!("清理过期封禁日志 {n} 条"); }
                    }
                }
                // PCB 过期清理（默认每 8 小时）
                if std::time::Instant::now() >= next_pcb_cleanup {
                    if let Some(pcb) = engine.pcb_module() {
                        let cutoff = now - pcb.config.persist_duration_ms;
                        let evicted = pcb.cleanup_expired(cutoff);
                        match db.cleanup_pcb(cutoff) {
                            Ok(n) if n > 0 || evicted > 0 => {
                                info!("清理过期 PCB 记录：数据库 {n} 条 / 内存 {evicted} 条");
                            }
                            Ok(_) => {}
                            Err(e) => warn!("清理过期 PCB 记录失败: {e}"),
                        }
                    }
                    next_pcb_cleanup = std::time::Instant::now() + pcb_cleanup_period;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("收到退出信号，停止…");
                // 对齐各监控模块的 onDisable：active-monitoring 再跑一次 on_tick、
                // session-analyse 跑 flush_data、peer-recording 跑 flush。
                engine
                    .monitor
                    .shutdown(&engine.entries, chrono::Utc::now().timestamp_millis())
                    .await;
                break;
            }
        }
    }
    server.abort();
    Ok(())
}
