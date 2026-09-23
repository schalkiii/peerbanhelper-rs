mod backend;
mod btn_legacy;
mod config;
mod monitor;
mod push;
mod ring;
mod rulesub;
mod submodule;
mod wave;

use clap::Parser;
use pbh_core::auto_stun::{AutoStunRefresher, DEFAULT_REFRESH_INTERVAL, DEFAULT_STUN_TIMEOUT};
use pbh_core::banlist::BannedRecord;
use pbh_core::btn_transport::{
    now_millis, BtnHttpClient, BtnMetadataStore, BtnNetwork, BtnNetworkConfig, BtnSubmitSource,
    ReqwestBlockingHttpClient, SharedBtnNetwork,
};
use pbh_core::config::IpRuleListConfig;
use pbh_core::geoip::{geoip_force_disabled, GeoIpDb, GeoIpProvider};
use pbh_core::modules::progress_cheat::PcbEntityKind;
use pbh_core::modules::{BtnNetworkOnline, MonitorSink, ProgressCheatBlocker};
use pbh_core::Pipeline;
use pbh_db::{Database, DbBtnSubmitSource, DbMetadataStore, DbMonitorSink};
use pbh_downloader::http::ReqwestFetcher;
use pbh_web::{build_router, AppState, DownloaderStatus, Metrics, RingLog};
use ring::RingLayer;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tracing::{debug, info, warn};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
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
    /// 演练模式：照常登录/拉取/判定/落库，但不向下载器下发封禁、解封与限速
    /// （用于与 Java 版并行观察，避免改动下载器状态）
    #[arg(long)]
    dry_run: bool,
    /// 长时对跑身份标记：写入 `metadata` 表（`dualrun_tag`）并随启动日志输出，
    /// 便于事后区分/归集两侧记录（对齐 PLAN「长时对跑基建 — --dualrun-tag 身份标记」）
    #[arg(long)]
    tag: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ring_log = Arc::new(RingLog::new(4096));
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,pbh=debug,reqwest=warn".into());
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_thread_names(true))
        .with(RingLayer::new(ring_log.clone()))
        .init();

    let args = Args::parse();
    let data_dir = args.data.clone();
    std::fs::create_dir_all(&data_dir)?;
    // 暴露数据目录给库内按 `<data>/...` 解析的路径（expression-engine 脚本目录等，
    // 对齐上游 Main.getDataDirectory() 的语义）
    std::env::set_var("PBH_DATA_DIR", &data_dir);

    let (mut cfg, cfg_path) = config::AppConfig::load_or_create(&data_dir)?;
    if let Some(port) = args.port {
        cfg.server.http = port;
    }
    if let Some(addr) = args.address {
        cfg.server.address = addr;
    }

    // 数据库：对齐上游部署布局 `<data>/persist/peerbanhelper-nt.db`；
    // 无 `persist/` 目录时沿用本移植的单文件布局 `<data>/peerbanhelper.db`。
    let db_path = {
        let upstream = data_dir.join("persist").join("peerbanhelper-nt.db");
        if upstream.exists() || data_dir.join("persist").is_dir() {
            upstream
        } else {
            data_dir.join("peerbanhelper.db")
        }
    };
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db = Arc::new(Database::open(
        db_path.to_str().unwrap_or("peerbanhelper.db"),
    )?);
    info!("SQLite 已打开: {}", db_path.display());
    if let Some(tag) = &args.tag {
        // 身份标记 + 启动时间：长时对跑的离线对账按 tag 归集两侧记录
        let _ = db.set_meta("dualrun_tag", tag);
        let _ = db.set_meta(
            "dualrun_started_at",
            &chrono::Utc::now().timestamp_millis().to_string(),
        );
        info!("长时对跑标记: {tag}");
    }
    if args.dry_run {
        info!("已启用演练模式（--dry-run）：不会向下载器下发封禁/解封/限速");
    }

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
    let remap = Arc::new(cfg.remap_config());
    let blocklist_url = format!(
        "http://{}:{}/blocklist/p2p-plain-format",
        cfg.server.public_host(),
        cfg.server.http
    );
    let mut built_entries: Vec<DownloaderEntry> = Vec::new();
    for d in &cfg.downloaders {
        match backend::build_downloader(d, &remap, &blocklist_url) {
            Ok((downloader, increment)) => built_entries.push(DownloaderEntry {
                downloader,
                increment_ban: increment,
            }),
            Err(e) => warn!("下载器 {} 初始化失败: {e}", d.name),
        }
    }
    info!("已加载 {} 个下载器", built_entries.len());
    let entries: Arc<Mutex<Vec<DownloaderEntry>>> = Arc::new(Mutex::new(built_entries));

    // GeoIP 数据源（对齐上游 `@Autowired IPDBManager`）：
    // `pbh.forceDisableIPDB` 打开、或 `<data>/ipdb/geoip/*.mmdb` 缺失/损坏时不注入 provider，
    // 此时 `ip-address-blocker` 的 asn / region / city / net-type 四个维度全部不命中
    //（与上游 IPDB 初始化失败的降级行为一致）。
    // 后台任务注册表（对齐上游 `BackgroundTaskManager`）：GeoIP 更新进度经
    // `GeoIpTaskAdapter` 映射为 `Lang.IPDB_DOWNLOAD_MMDB` 任务，WebUI 经 SSE
    // `GET /api/tasks/live` 读取（对齐上游 `PBHBackgroundTaskController`）。
    let background_tasks = Arc::new(pbh_web::BackgroundTaskRegistry::new());
    let geo: Option<Arc<dyn GeoIpProvider>> = if geoip_force_disabled() {
        info!("pbh.forceDisableIPDB 已打开，跳过 GeoIP 数据库加载");
        None
    } else {
        let ipdb_dir = data_dir.join("ipdb");
        let geoip_task_adapter = Arc::new(pbh_web::GeoIpTaskAdapter::new(background_tasks.clone()));
        let geoip_progress_sink = geoip_task_adapter.sink();
        // GeoIP 数据库自动更新（对齐上游 `IPDBManager#setupIPDB` → `IPDB` 构造函数的
        // 「先 updateMMDB 再 loadMMDB」：更新完成后再加载，本次启动即可用上新库）。
        // 镜像顺序、`.mmdb.xz` + XZ 解压、45 天 mtime 间隔、原子替换均见 `geoip_update`。
        // 对齐上游 `IPDB` 构造函数：无论 `auto-update` 开关，启动都先跑一次更新检查——
        // `auto-update: true` 按 45 天 mtime 周期刷新；`auto-update: false` 时上游对
        // 「本地缺失」的库仍会下载一次（`needUpdateMMDB` 对缺失恒 true），已存在的库不覆盖；
        // 库齐全且未过期时零网络请求。更新跑在阻塞线程上，`reqwest::blocking` 不会踩 tokio runtime。
        {
            let updater_dir = ipdb_dir.clone();
            let updater_config = cfg.ip_database.clone();
            match tokio::task::spawn_blocking(
                move || -> anyhow::Result<pbh_core::geoip_update::UpdateReport> {
                    let http = pbh_core::geoip_update::ReqwestBlockingHttpClient::new()?;
                    let updater = pbh_core::geoip_update::GeoIpUpdater::new(
                        updater_dir,
                        updater_config.clone(),
                        &http,
                    )
                    .with_progress_sink(geoip_progress_sink);
                    let updater = if updater_config.auto_update {
                        updater
                    } else {
                        // 上游语义：`auto-update: false` 仍补齐缺失的库（已存在的库不动）
                        updater.with_download_missing(true)
                    };
                    Ok(updater.update_if_needed())
                },
            )
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

    // 文案表：内嵌上游 lang 资源，允许 data/lang 覆盖（与 wave / BTN 上报共用，统一 locale 渲染）
    let translator = Arc::new(pbh_core::i18n::Translator::with_overrides(Some(
        &data_dir.join("lang"),
    )));

    // BTN 传输层（对齐上游 `btn/BtnNetwork`）：规则集 / IP 白黑名单全部由 BTN 服务器下发。
    // `btn.enabled: true` 且 `config-url` 非空（[`BtnNetworkConfig::is_active`]）时才构造客户端
    // 并起后台线程：握手 → abilities 到期调度（含 PoW captcha）→ 注入 `btn` 模块。
    // 否则（出厂默认）**不构造客户端、不发请求、不起线程**，模块保持
    // 「未注入规则 ⇒ 恒 `pass()`、绝不封禁」。
    //
    // 上报类能力（submit_bans / submit_swarm / submit_histories）的数据源是 SQLite
    // （[`DbBtnSubmitSource`]）：`history` / `tracked_swarm` / `peer_records` 表。
    // `rule_name` / `description` 按服务端语言渲染（对齐上游 `tlUI(component)`）。
    let btn_network_slot: pbh_core::btn_transport::SharedBtnNetwork = Default::default();
    // BTN 遗留协议 live peers 快照（wave 每轮写入；见 btn_legacy 模块文档）
    let live_peer_map = btn_legacy::new_live_peer_map();
    let _btn_transport: Option<std::thread::JoinHandle<()>> = if cfg.btn.is_active() {
        // 阻塞式 HTTP 客户端自带 tokio 运行时，**必须**在工作线程内部构造
        // （在 async 上下文里构造/析构会 panic：`Cannot drop a runtime in a context
        // where blocking is not allowed`），因此这里只传入构造函数。
        // 规则缓存落 SQLite 的 `meta` 表（对齐上游 `metadataDao`；重启后回灌、不重拉）
        let metadata = Arc::new(DbMetadataStore::new(db.clone()));
        let submit_source: Arc<dyn BtnSubmitSource> =
            Arc::new(btn_legacy::LegacyAwareSubmitSource::new(
                Arc::new(DbBtnSubmitSource::new(
                    db.clone(),
                    translator.clone(),
                    cfg.language.locale.clone(),
                )),
                live_peer_map.clone(),
                pipeline.ban_list.clone(),
                translator.clone(),
                cfg.language.locale.clone(),
            ));
        let handle = spawn_btn_transport(
            &cfg.btn,
            &pipeline,
            || {
                ReqwestBlockingHttpClient::new()
                    .map(|client| Arc::new(client) as Arc<dyn BtnHttpClient>)
            },
            metadata,
            submit_source,
            btn_network_slot.clone(),
        );
        if handle.is_none() {
            warn!(
                "btn.enabled=true 但流水线里没有启用 btn 模块\
                 （profile.module.btn）：跳过 BTN 接线"
            );
        }
        handle
    } else {
        info!(
            "BTN 传输层未启用（config.yml 的 btn.enabled=false 或 config-url 为空）：\
             不构造客户端、不发起请求、不创建线程；btn 模块恒 pass、绝不封禁"
        );
        None
    };

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
    let monitor_sink: Arc<dyn MonitorSink> =
        Arc::new(DbMonitorSink::with_geo(db.clone(), geo.clone()));
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

    // 内存封禁表：从数据库恢复未到期条目（对齐上游 loadBanListToMemory）。
    // 封禁表由 pipeline 持有，auto-range-ban 模块会读取它。
    // `banlist.metadata` 是 `BanMetadata` 的 JSON；解析失败的行跳过（对齐上游
    // `readBanList` 的 `catch` 语义：个别坏数据不阻塞启动）。
    if cfg.persist.banlist {
        let now = chrono::Utc::now().timestamp_millis();
        let mut records: Vec<BannedRecord> = Vec::new();
        for (address, metadata) in db.read_ban_list()? {
            let parsed: pbh_core::banlist::BanMetadata = match serde_json::from_str(&metadata) {
                Ok(parsed) => parsed,
                Err(e) => {
                    warn!("封禁记录 {address} 的 metadata 解析失败，已跳过: {e}");
                    continue;
                }
            };
            if parsed.unban_at_ms > 0 && parsed.unban_at_ms <= now {
                continue; // 已过期
            }
            records.push(BannedRecord {
                ip: address,
                unban_at_ms: parsed.unban_at_ms,
                module: parsed.context.clone(),
                ban_for_disconnect: parsed.ban_for_disconnect,
                metadata: parsed,
            });
        }
        info!("已从数据库恢复 {} 条封禁记录", records.len());
        if let Ok(mut list) = pipeline.ban_list.lock() {
            list.load(records);
        }
    }

    // Web 状态
    let metrics = Arc::new(Mutex::new(Metrics::default()));
    let statuses = Arc::new(Mutex::new(Vec::<DownloaderStatus>::new()));
    // Web 后端：配置读写 / 下载器热管理 / 手动封禁 / 推送渠道（对齐上游各 Controller）
    let wave_trigger = Arc::new(tokio::sync::Notify::new());
    let global_pause = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // 登录闸门：wave 与 Web 后端共享；下载器更新/删除时重置（对齐上游
    // `unregisterDownloader + registerDownloader` 使失败计数随实例重建而清零）
    let login_gates: Arc<Mutex<HashMap<String, wave::LoginGate>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let backend = Arc::new(backend::PbhBackend::new(
        data_dir.clone(),
        cfg.clone(),
        remap.clone(),
        blocklist_url.clone(),
        pipeline.clone(),
        entries.clone(),
        statuses.clone(),
        alert_manager.clone(),
        wave_trigger.clone(),
        login_gates.clone(),
    ));
    // 规则订阅：与 Web 后端（`/api/sub/*`）共享同一份运行时配置
    let rulesub_shared: Arc<RwLock<IpRuleListConfig>> = Arc::new(RwLock::new(
        cfg.profile.module.ip_rule_list.clone().unwrap_or_default(),
    ));
    let state = AppState {
        db: db.clone(),
        token: Arc::new(Mutex::new(cfg.server.token.clone())),
        metrics: metrics.clone(),
        started: Arc::new(std::time::Instant::now()),
        downloaders: statuses.clone(),
        static_dir: Arc::new(Mutex::new(Some(data_dir.join("static")))),
        ban_list: pipeline.ban_list.clone(),
        remap: remap.clone(),
        translator: translator.clone(),
        locale: cfg.language.locale.clone(),
        backend: backend.clone(),
        log_ring: ring_log.clone(),
        global_pause: global_pause.clone(),
        sub_module: {
            // 规则订阅 Web 后端：共享运行时规则配置，增删改即时回写 config 文件
            let http = reqwest::Client::builder()
                .timeout(Duration::from_secs(120))
                .user_agent("PeerBanHelper-RS")
                .build()
                .ok();
            http.map(|http| {
                let backend: Arc<dyn pbh_web::SubModule> = submodule::RuleSubBackend::new(
                    pipeline.clone(),
                    db.clone(),
                    data_dir.clone(),
                    Arc::new(http),
                    cfg_path.clone(),
                    rulesub_shared.read().unwrap().clone(),
                );
                backend
            })
        },
        btn_network: btn_network_slot.clone(),
        tasks: background_tasks.clone(),
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
    // `rulesub_shared` 与 Web 后端（`/api/sub/*`）共享同一份配置：Web 增删改会即时反映到刷新。
    if rulesub_shared.read().unwrap().enabled() {
        let pipeline = pipeline.clone();
        let data_dir = data_dir.clone();
        let db = db.clone();
        let rulesub_shared = rulesub_shared.clone();
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
            loop {
                let cfg = rulesub_shared.read().unwrap().clone();
                let interval = Duration::from_millis(cfg.check_interval_ms.max(60_000) as u64);
                // 启动立即拉取一次（首次不等待 interval，对齐上游 initialDelay=0）
                for line in rulesub::refresh_all(
                    &pipeline,
                    &cfg,
                    &data_dir,
                    &http,
                    Some(&db),
                    rulesub::UPDATE_TYPE_AUTO,
                )
                .await
                {
                    info!("{line}");
                }
                rulesub::log_summary(&pipeline);
                tokio::time::sleep(interval).await;
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
        persist_banlist: cfg.persist.banlist,
        dry_run: args.dry_run,
        max_concurrent: 128,
        alert_manager,
        monitor,
        geo,
        login_gates,
        live_peers: live_peer_map,
    };

    let period = Duration::from_millis(cfg.profile.check_interval.max(500));
    let mut ticker = tokio::time::interval(period);
    // 对齐上游 `scheduleWithFixedDelay`：上一轮结束后再等一个间隔，
    // 避免某轮耗时超过间隔时立刻补跑（突发）导致下载器被连续冲击。
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let cleanup_days = cfg.persist.ban_logs_keep_days;
    // 封禁列表落库节奏（对齐上游 `saveBanList` 的 10s 首延迟 + 1h 固定间隔）
    let banlist_save_period = Duration::from_secs(3600);
    let mut next_banlist_save = std::time::Instant::now() + Duration::from_secs(10);
    let mut wave_count: u64 = 0;
    // PCB 过期清理：对齐上游 `scheduleWithFixedDelay(this::cleanDatabase, 0, 8, HOURS)`
    let pcb_cleanup_period = Duration::from_secs(8 * 3600);
    let mut next_pcb_cleanup = std::time::Instant::now();

    info!("ban wave 间隔: {:?}", period);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                // 全局暂停（web `/api/general/global` 切换）：暂停期跳过 ban wave，
                // 但监控模块定时任务照常（对齐上游 pause 下任务仍执行的语义）
                if global_pause.load(std::sync::atomic::Ordering::Relaxed) {
                    continue;
                }
                let now = chrono::Utc::now().timestamp_millis();
                let wave_started = std::time::Instant::now();
                let report = engine.run_once(now).await;
                wave_count += 1;
                // 完成日志与上游 `Lang.BAN_WAVE_CHECK_COMPLETED` 逐字一致（含参数顺序），
                // 计数为上游 `ProcessingStatistics` 口径（只统计有检查结果的下载器/种子/peer），
                // 便于长时对跑直接把两侧日志逐行 diff。附加诊断降级到 DEBUG。
                let finish_line = translator.render(
                    &pbh_core::i18n::TranslationComponent::with_params(
                        "BAN_WAVE_CHECK_COMPLETED",
                        vec![
                            report.checked_downloaders.to_string().into(),
                            report.torrents.to_string().into(),
                            report.peers.to_string().into(),
                            report.banned.to_string().into(),
                            report.unbanned.to_string().into(),
                            (wave_started.elapsed().as_millis() as i64)
                                .to_string()
                                .into(),
                        ],
                    ),
                    &cfg.language.locale,
                );
                // 上游：`if (!hideFinishLogs && !downloaderManager.isEmpty())`
                let has_downloaders = !engine
                    .entries
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_empty();
                if !cfg.logger.hide_finish_log && has_downloaders {
                    info!("{finish_line}");
                }
                debug!(
                    "wave#{wave_count}: 在线下载器={} 跳过={} 错误={}",
                    report.online_downloaders,
                    report.skipped,
                    report.errors.len()
                );
                for e in &report.errors {
                    warn!("wave error: {e}");
                }
                // 监控模块的定时任务（内部按各模块的上游间隔判断到期；首次 delay 0 立即执行）：
                // active-monitoring 每 1 分钟 updateTrafficStatus、session-analyse /
                // peer-recording / swarm-tracking 按各自的 flush / cleanup 间隔。
                // 快照后释放锁再 await：定时任务内有网络 I/O，持锁跨 await 会阻塞下载器热管理
                let entries_snapshot: Vec<_> =
                    engine.entries.lock().unwrap_or_else(|e| e.into_inner()).clone();
                engine.monitor.run_scheduled(&entries_snapshot, now, args.dry_run).await;
                // 封禁列表落库（对齐上游 `scheduleWithFixedDelay(saveBanList, 10s, 1h)`：
                // 首轮后 10 秒内首次保存，之后每小时一次）
                if cfg.persist.banlist && std::time::Instant::now() >= next_banlist_save {
                    let written = engine.persist_ban_list();
                    debug!("封禁列表已落库 {written} 条");
                    next_banlist_save = std::time::Instant::now() + banlist_save_period;
                }
                // 每 100 轮清理一次过期封禁历史（`persist.ban-logs-keep-days`）
                if wave_count.is_multiple_of(100) && cleanup_days > 0 {
                    if let Ok(n) = db.cleanup_history(cleanup_days) {
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
            _ = wave_trigger.notified() => {
                // 手动封禁/解封后的立即 ban wave（对齐上游 web 手动操作即触发）
                let now = chrono::Utc::now().timestamp_millis();
                let report = engine.run_once(now).await;
                if !report.errors.is_empty() {
                    for e in &report.errors {
                        warn!("wave error: {e}");
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("收到退出信号，停止…");
                // 对齐各监控模块的 onDisable：active-monitoring 再跑一次 on_tick、
                // session-analyse 跑 flush_data、peer-recording 跑 flush。
                let entries_snapshot: Vec<_> =
                    engine.entries.lock().unwrap_or_else(|e| e.into_inner()).clone();
                engine
                    .monitor
                    .shutdown(&entries_snapshot, chrono::Utc::now().timestamp_millis())
                    .await;
                break;
            }
        }
    }
    server.abort();
    Ok(())
}

/// BTN 后台同步线程的 tick 间隔。
///
/// 上游每个 ability 由 `ScheduledExecutorService` 按自己的 `interval`（典型值 24h）+
/// `random_initial_delay` 调度；本移植由本线程统一轮询 [`BtnNetwork::sync_due`]，
/// tick 只决定「多快注意到某个 ability 到期」。取 5 秒：远小于服务端可能的分钟级
/// `interval`（保证到期即跑、误差 ≤ 5s），又没有可观测开销；配置握手失败的重试由
/// [`BtnNetwork`] 自己按上游 `RETRY_PERIOD_SECONDS`（600s）节流，与 tick 无关。
const BTN_SYNC_TICK: Duration = Duration::from_secs(5);

/// 接线 BTN 传输层：仅当 `btn.enabled && config-url` 非空（[`BtnNetworkConfig::is_active`]）、
/// 且流水线里启用了 `btn` 模块时才构造 [`BtnNetwork`] 并起后台线程；
/// 其余情况返回 `None` —— **不构造客户端、不发请求、不起线程**。
///
/// 用 `std::thread` + 阻塞式 HTTP 客户端（而非 tokio task）：`reqwest::blocking` 自带运行时、
/// 不能在 async 上下文构造/析构，故 `http_factory` 也只在线程内被调用一次。
/// 独立线程同样不会拖慢/阻塞 ban wave。
///
/// 构造完成的 [`BtnNetwork`] 会写入 `network_slot`，供 Web 层的 `btnQuery` 端点复用
/// （对齐上游注入 `PBHPeerController` 的 `BtnNetwork` 单例）。
fn spawn_btn_transport<F>(
    config: &BtnNetworkConfig,
    pipeline: &Arc<Pipeline>,
    http_factory: F,
    metadata: Arc<dyn BtnMetadataStore>,
    submit_source: Arc<dyn BtnSubmitSource>,
    network_slot: SharedBtnNetwork,
) -> Option<std::thread::JoinHandle<()>>
where
    F: Fn() -> anyhow::Result<Arc<dyn BtnHttpClient>> + Send + 'static,
{
    if !config.is_active() {
        return None;
    }
    // 上游 `BtnNetwork` 与判定模块是两个对象，但本移植的规则注入入口全部在模块上：
    // 流水线里没有 `btn` 模块就没有任何可落地的数据 ⇒ 视为未启用（直接返回 `None`）。
    pipeline.module_as::<BtnNetworkOnline>("btn")?;
    let pipeline = pipeline.clone();
    let config_url = config.config_url.clone();
    let config = config.clone();
    let thread = std::thread::Builder::new()
        .name("btn-transport".to_string())
        .spawn(move || {
            let http = match http_factory() {
                Ok(http) => http,
                Err(e) => {
                    warn!("BTN HTTP 客户端初始化失败，BTN 同步线程退出: {e}");
                    return;
                }
            };
            // 允许列表更新后需要解封（上游 `DownloaderServer.getBanList()`）
            let network = Arc::new(
                BtnNetwork::new(config, http, metadata)
                    .with_ban_list(pipeline.ban_list.clone())
                    .with_submit_source(submit_source),
            );
            network_slot.set(network.clone());
            info!("BTN 传输层已启动：config-url={config_url}, tick={BTN_SYNC_TICK:?}");
            loop {
                // 对齐上游 `scheduleWithFixedDelay(this::updateRule, 0, interval, MS)`：先跑再等
                let Some(module) = pipeline.module_as::<BtnNetworkOnline>("btn") else {
                    warn!("btn 模块已不在流水线中，BTN 同步线程退出");
                    return;
                };
                // 上游各 ability 的 `try/catch`：任何失败都记日志并继续下一轮
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    network.check_if_need_retry_config(module);
                    network.sync_due(module, now_millis())
                })) {
                    Ok(report) => {
                        for sync in report {
                            debug!("BTN ability 已同步: {} updated={}", sync.key, sync.updated);
                        }
                    }
                    Err(_) => warn!("BTN 同步出现异常，已忽略并继续下一轮"),
                }
                std::thread::sleep(BTN_SYNC_TICK);
            }
        });
    match thread {
        Ok(handle) => Some(handle),
        Err(e) => {
            warn!("BTN 同步线程创建失败: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbh_core::btn_transport::{
        BtnHttpRequest, BtnHttpResponse, ClosureHttpClient, InMemoryMetadataStore,
    };
    use pbh_core::model::{PeerData, TorrentData};
    use pbh_core::module::{CheckContext, PeerAction};
    use pbh_core::RuleModule;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn counting_http(calls: Arc<AtomicUsize>) -> Arc<dyn BtnHttpClient> {
        Arc::new(ClosureHttpClient::new(move |_: &BtnHttpRequest| {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok(BtnHttpResponse::new(200, "{}"))
        }))
    }

    fn btn_pipeline() -> Arc<Pipeline> {
        let mut pipeline = Pipeline::default();
        pipeline.add_module(Box::new(BtnNetworkOnline::new(
            pbh_core::modules::btn::BTN_BAN_DURATION_MS,
        )));
        Arc::new(pipeline)
    }

    /// 测试用空数据源：trait 默认实现即「没有任何待提交数据」。
    #[derive(Debug)]
    struct EmptySubmitSource;
    impl BtnSubmitSource for EmptySubmitSource {}

    fn peer() -> PeerData {
        PeerData {
            client_name: Some("Xunlei".to_string()),
            peer_id: Some("-hp001-abcdefghijkl".to_string()),
            dl_speed: 1000,
            downloaded: 1000,
            up_speed: 1000,
            uploaded: 1000,
            progress: 0.5,
            flags: Some("d u".to_string()),
            ip: "1.2.3.4".to_string(),
            port: 51413,
            raw_ip: "1.2.3.4:51413".to_string(),
            connection: Some("uTP".to_string()),
        }
    }

    fn torrent() -> TorrentData {
        TorrentData {
            hash: "h".to_string(),
            name: "t".to_string(),
            progress: 1.0,
            total_size: 1_000_000_000,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: Some(false),
        }
    }

    /// 出厂默认（`btn` 未启用/未配置）⇒ 不构造 `BtnNetwork`、不起线程、零 HTTP 请求，
    /// 模块保持「未注入规则 ⇒ 恒 pass、绝不封禁」
    #[test]
    fn disabled_btn_spawns_no_transport_and_module_passes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let http = counting_http(calls.clone());
        let pipeline = btn_pipeline();
        let metadata = Arc::new(InMemoryMetadataStore::new());

        for config in [
            BtnNetworkConfig::default(),
            BtnNetworkConfig {
                enabled: true,
                ..Default::default()
            },
            BtnNetworkConfig {
                config_url: "https://btn.test/config".to_string(),
                ..Default::default()
            },
            BtnNetworkConfig {
                enabled: true,
                config_url: "   ".to_string(),
                ..Default::default()
            },
        ] {
            let http = http.clone();
            assert!(
                spawn_btn_transport(
                    &config,
                    &pipeline,
                    move || Ok(http.clone()),
                    metadata.clone(),
                    Arc::new(EmptySubmitSource),
                    SharedBtnNetwork::default(),
                )
                .is_none(),
                "未启用时不得构造客户端/起线程"
            );
        }
        assert_eq!(calls.load(Ordering::Relaxed), 0, "未启用时一个请求都不能发");

        let btn = pipeline
            .module_as::<BtnNetworkOnline>("btn")
            .expect("btn 模块");
        assert!(!btn.is_manager_initialized());
        let result = btn.check("qbittorrent", &torrent(), &peer(), &CheckContext::default());
        assert_eq!(result.action, PeerAction::NoAction);
        assert_eq!(result.data["status"], "pass");
    }

    /// `btn` 段启用但流水线里没有 `btn` 模块 ⇒ 同样不起线程、不发请求
    #[test]
    fn active_btn_without_module_spawns_nothing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let http = counting_http(calls.clone());
        let pipeline = Arc::new(Pipeline::default());
        let config = BtnNetworkConfig {
            enabled: true,
            config_url: "https://btn.test/config".to_string(),
            ..Default::default()
        };
        assert!(spawn_btn_transport(
            &config,
            &pipeline,
            move || Ok(http.clone()),
            Arc::new(InMemoryMetadataStore::new()),
            Arc::new(EmptySubmitSource),
            SharedBtnNetwork::default(),
        )
        .is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
}
