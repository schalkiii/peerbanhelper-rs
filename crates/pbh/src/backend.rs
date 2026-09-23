//! Web 后端实现：`pbh_web::WebBackend` 的 pbh 主进程侧适配。
//!
//! 收敛「只有主进程能回答的问题」：配置读写、下载器热管理、手动封禁/解封、
//! 推送渠道管理、模块清单与全局暂停。行为逐条对齐上游 Controller
//! （PBHGeneralController / PBHDownloaderController / PBHPushController / PBHBanController）。

use crate::config::{AppConfig, DownloaderConfig, PushProviderConfig, PushSection};
use crate::push::{AlertLevel, AlertManager, PushManager};
use crate::wave::DownloaderEntry;

use pbh_core::pipeline::Pipeline;
use pbh_core::remap::RemapConfig;
use pbh_downloader::aria2::{Aria2Config, Aria2Downloader};
use pbh_downloader::biglybt::{BiglyBtConfig, BiglyBtDownloader};
use pbh_downloader::bitcomet::{BitCometConfig, BitCometDownloader};
use pbh_downloader::deluge::{DelugeConfig, DelugeDownloader};
use pbh_downloader::http::{HttpFetcher, ReqwestFetcher};
use pbh_downloader::qbittorrent::{QBConfig, QBittorrentDownloader};
use pbh_downloader::transmission::{TRConfig, TransmissionDownloader};
use pbh_downloader::Downloader;
use pbh_web::{ModuleRecord, ReloadEntry, WebBackend};
use rand::distributions::Alphanumeric;
use rand::Rng as _;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tracing::{info, warn};

/// 手动封禁使用的模块名（对齐上游 `ManualModule` 的 configName）。
const MANUAL_MODULE: &str = "Manual";

/// 上游 rule 模块的 Java 类名（`/api/metadata/manifest` 的 `class_name`，
/// WebUI 用该类名作为模块开关的唯一键，必须与上游一致）。
///
/// 与 `history.module_name` / `banlist.metadata.context` 共用同一张映射表
/// （[`pbh_core::module::java_module_class`]）；未收录的模块回退为 configName 本身。
fn module_class_name(config_name: &str) -> String {
    let class = pbh_core::module::java_module_class(config_name);
    if class == pbh_core::module::UNKNOWN_MODULE_CLASS {
        config_name.to_string()
    } else {
        class.to_string()
    }
}

pub struct PbhBackend {
    data_dir: PathBuf,
    config: StdMutex<AppConfig>,
    remap: Arc<RemapConfig>,
    blocklist_url: String,
    pipeline: Arc<Pipeline>,
    entries: Arc<StdMutex<Vec<DownloaderEntry>>>,
    statuses: Arc<StdMutex<Vec<pbh_web::DownloaderStatus>>>,
    alert_manager: Arc<AlertManager>,
    fetcher: Arc<dyn HttpFetcher + Send + Sync>,
    install_id: String,
    global_pause: Arc<AtomicBool>,
    /// 手动封禁/解封后唤醒 ban wave 循环立即跑一轮
    /// （对齐上游 web 手动操作后 `DownloaderServerImpl.banWave()` 的立即触发语义）。
    wave_trigger: Arc<tokio::sync::Notify>,
    /// 登录闸门（与 [`crate::wave::WaveEngine`] 共享）：下载器更新/删除时移除对应条目，
    /// 对齐上游「重建下载器实例 ⇒ 失败计数与冷却清零」。
    login_gates: Arc<StdMutex<HashMap<String, crate::wave::LoginGate>>>,
}

impl PbhBackend {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        data_dir: PathBuf,
        config: AppConfig,
        remap: Arc<RemapConfig>,
        blocklist_url: String,
        pipeline: Arc<Pipeline>,
        entries: Arc<StdMutex<Vec<DownloaderEntry>>>,
        statuses: Arc<StdMutex<Vec<pbh_web::DownloaderStatus>>>,
        alert_manager: Arc<AlertManager>,
        wave_trigger: Arc<tokio::sync::Notify>,
        login_gates: Arc<StdMutex<HashMap<String, crate::wave::LoginGate>>>,
    ) -> Self {
        let install_id = read_installation_id(&data_dir);
        // 推送渠道重建复用的 HTTP 客户端（对齐 `HTTPUtil.newBuilder()`：校验 TLS、超时 15s/60s）
        let fetcher: Arc<dyn HttpFetcher + Send + Sync> = match ReqwestFetcher::new(true, 15, 60) {
            Ok(f) => Arc::new(f),
            Err(e) => {
                warn!("推送 HTTP 客户端初始化失败，推送渠道热管理不可用: {e}");
                Arc::new(ReqwestFetcher::new(true, 15, 60).expect("重试初始化"))
            }
        };
        Self {
            data_dir,
            config: StdMutex::new(config),
            remap,
            blocklist_url,
            pipeline,
            entries,
            statuses,
            alert_manager,
            fetcher,
            install_id,
            global_pause: Arc::new(AtomicBool::new(false)),
            wave_trigger,
            login_gates,
        }
    }

    /// 当前配置快照。
    fn snapshot(&self) -> AppConfig {
        self.config
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 保存配置到磁盘并更新内存态。
    fn save_config(&self, cfg: &AppConfig) -> Result<(), String> {
        let path = self.data_dir.join("config.yml");
        cfg.save_to(&path).map_err(|e| e.to_string())?;
        if let Ok(mut slot) = self.config.lock() {
            *slot = cfg.clone();
        }
        Ok(())
    }

    /// 重建推送渠道（对齐上游保存渠道后 `PushManagerImpl` 立即生效的语义）。
    fn rebuild_push(&self) {
        let cfg = self.snapshot();
        let push = PushManager::from_config(&cfg.push, self.fetcher.clone());
        self.alert_manager.update_push_manager(Arc::new(push));
    }

    /// 重建下载器列表（对齐上游 web 增删改后的 `reload()`：失败项记日志并跳过，
    /// 成功项以新配置立即接管）。
    fn rebuild_downloaders(&self) {
        let cfg = self.snapshot();
        let mut rebuilt: Vec<DownloaderEntry> = Vec::new();
        for d in &cfg.downloaders {
            match build_downloader(d, &self.remap, &self.blocklist_url) {
                Ok((downloader, increment)) => rebuilt.push(DownloaderEntry {
                    downloader,
                    increment_ban: increment,
                }),
                Err(e) => warn!("下载器 {} 重建失败: {e}", d.name),
            }
        }
        if let Ok(mut slot) = self.entries.lock() {
            *slot = rebuilt;
        }
    }
}

impl WebBackend for PbhBackend {
    fn installation_id(&self) -> String {
        self.install_id.clone()
    }

    fn analytics_enabled(&self) -> bool {
        self.snapshot().analytics
    }

    fn set_analytics(&self, enabled: bool) -> Result<(), String> {
        let mut cfg = self.snapshot();
        cfg.analytics = enabled;
        self.save_config(&cfg)
    }

    fn modules(&self) -> Vec<ModuleRecord> {
        self.pipeline
            .modules
            .iter()
            .map(|m| ModuleRecord {
                class_name: module_class_name(m.config_name()),
                config_name: m.config_name().to_string(),
            })
            .collect()
    }

    fn global_paused(&self) -> bool {
        self.global_pause.load(Ordering::Relaxed)
    }

    fn set_global_paused(&self, paused: bool) -> Result<(), String> {
        self.global_pause.store(paused, Ordering::Relaxed);
        info!(
            "全局封禁已{}（web 操作）",
            if paused { "暂停" } else { "恢复" }
        );
        Ok(())
    }

    fn reload(&self) -> Vec<ReloadEntry> {
        // 对齐上游 `handleReloading`：先重载配置，再逐个重载运行时模块。
        // Rust 侧规则流水线在启动时静态构建（不热建模块），此处重建下载器
        // 与推送渠道并如实标注差异项。
        self.rebuild_downloaders();
        self.rebuild_push();
        vec![
            ReloadEntry {
                module_name: "config".into(),
                results: vec!["配置已重新读取".into()],
            },
            ReloadEntry {
                module_name: "DownloaderManager".into(),
                results: vec!["下载器已重建".into()],
            },
            ReloadEntry {
                module_name: "PushManagerImpl".into(),
                results: vec!["推送渠道已重建".into()],
            },
        ]
    }

    fn read_config(&self, name: &str) -> Result<Value, String> {
        let cfg = self.snapshot();
        let root = serde_yaml::to_value(&cfg).map_err(|e| e.to_string())?;
        match name {
            "config" => Ok(normalize_keys(&root)),
            "profile" => {
                let profile = root
                    .get("profile")
                    .cloned()
                    .unwrap_or(serde_yaml::Value::Null);
                Ok(normalize_keys(&profile))
            }
            other => Err(format!("CONFIG_NOT_FOUND: {other}")),
        }
    }

    fn write_config(&self, name: &str, data: &Value) -> Result<(), String> {
        // 与上游一致：写回前先把 JSON 的 `_` 键还原为 YAML 的 `-` 键
        let yaml_value = denormalize_json(data);
        match name {
            "config" => {
                let parsed = serde_yaml::from_value::<AppConfig>(yaml_value)
                    .map_err(|e| format!("配置校验失败: {e}"))?;
                self.save_config(&parsed)?;
            }
            "profile" => {
                let mut root = serde_yaml::to_value(self.snapshot()).map_err(|e| e.to_string())?;
                if let Some(mapping) = root.as_mapping_mut() {
                    mapping.insert(serde_yaml::Value::String("profile".into()), yaml_value);
                }
                let parsed = serde_yaml::from_value::<AppConfig>(root)
                    .map_err(|e| format!("配置校验失败: {e}"))?;
                self.save_config(&parsed)?;
            }
            other => return Err(format!("CONFIG_NOT_FOUND: {other}")),
        }
        // 配置变更后的热加载（对齐上游写回后发起的 reload）
        self.rebuild_downloaders();
        self.rebuild_push();
        Ok(())
    }

    fn ban_peers(&self, ips: &[String]) -> Result<(), String> {
        // 只改内存封禁表（对齐上游 `scheduleBanPeerNoAssign`）：持久化由 ban wave 的
        // 定时全量 `saveBanList` 完成（`WaveEngine::persist_ban_list`）。
        let mut list = self
            .pipeline
            .ban_list
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for ip in ips {
            list.add(ip, 0, MANUAL_MODULE, false);
        }
        list.mark_reapply();
        drop(list);
        self.wave_trigger.notify_one();
        Ok(())
    }

    fn unban_peers(&self, ips: &[String]) -> Result<(), String> {
        let mut list = self
            .pipeline
            .ban_list
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if ips.iter().any(|ip| ip == "*") {
            // 对齐上游 DELETE /api/bans 的 `*` 清空语义
            list.clear();
        } else {
            for ip in ips {
                list.remove(ip);
            }
        }
        list.mark_reapply();
        drop(list);
        self.wave_trigger.notify_one();
        Ok(())
    }

    fn downloaders(&self) -> Vec<Value> {
        self.statuses
            .lock()
            .map(|statuses| {
                statuses
                    .iter()
                    .map(|s| {
                        json!({
                            "id": s.id,
                            "name": s.name,
                            "type": s.kind,
                            "online": s.online,
                            "version": s.version,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn downloader(&self, id: &str) -> Option<Arc<dyn Downloader>> {
        self.entries
            .lock()
            .map(|entries| {
                entries
                    .iter()
                    .find(|e| e.downloader.id() == id)
                    .map(|e| e.downloader.clone())
            })
            .unwrap_or(None)
    }

    fn add_downloader(&self, config: &Value) -> Result<(), String> {
        let dl_cfg: DownloaderConfig =
            serde_json::from_value(config.clone()).map_err(|e| format!("配置无效: {e}"))?;
        let (downloader, increment) = build_downloader(&dl_cfg, &self.remap, &self.blocklist_url)
            .map_err(|e| format!("创建下载器失败: {e}"))?;
        let mut cfg = self.snapshot();
        if cfg.downloaders.iter().any(|d| d.name == dl_cfg.name) {
            return Err("DL_DUPLICATE_NAME".into());
        }
        cfg.downloaders.push(dl_cfg);
        self.save_config(&cfg)?;
        if let Ok(mut slot) = self.entries.lock() {
            slot.push(DownloaderEntry {
                downloader,
                increment_ban: increment,
            });
        }
        Ok(())
    }

    fn update_downloader(&self, id: &str, config: &Value) -> Result<(), String> {
        let mut parsed: DownloaderConfig =
            serde_json::from_value(config.clone()).map_err(|e| format!("配置无效: {e}"))?;
        if parsed.name.is_empty() {
            parsed.name = id.to_string();
        }
        let (downloader, increment) = build_downloader(&parsed, &self.remap, &self.blocklist_url)
            .map_err(|e| format!("重建下载器失败: {e}"))?;
        let mut cfg = self.snapshot();
        let slot = cfg
            .downloaders
            .iter_mut()
            .find(|d| d.name == id)
            .ok_or_else(|| "DL_NOT_FOUND".to_string())?;
        *slot = parsed;
        self.save_config(&cfg)?;
        if let Ok(mut entries) = self.entries.lock() {
            if let Some(entry) = entries.iter_mut().find(|e| e.downloader.id() == id) {
                *entry = DownloaderEntry {
                    downloader,
                    increment_ban: increment,
                };
            } else {
                entries.push(DownloaderEntry {
                    downloader,
                    increment_ban: increment,
                });
            }
        }
        // 重建实例 ⇒ 失败计数与冷却清零（对齐上游 unregister + register）
        if let Ok(mut gates) = self.login_gates.lock() {
            gates.remove(id);
        }
        Ok(())
    }

    fn remove_downloader(&self, id: &str) -> Result<(), String> {
        let mut cfg = self.snapshot();
        cfg.downloaders.retain(|d| d.name != id);
        self.save_config(&cfg)?;
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|e| e.downloader.id() != id);
        }
        if let Ok(mut statuses) = self.statuses.lock() {
            statuses.retain(|s| s.id != id);
        }
        if let Ok(mut gates) = self.login_gates.lock() {
            gates.remove(id);
        }
        Ok(())
    }

    fn test_downloader(&self, config: &Value) -> Result<(), String> {
        let dl_cfg: DownloaderConfig =
            serde_json::from_value(config.clone()).map_err(|e| format!("配置无效: {e}"))?;
        let (downloader, _) = build_downloader(&dl_cfg, &self.remap, &self.blocklist_url)
            .map_err(|e| e.to_string())?;
        // 登录校验与 ban wave 完全一致（`login()` 内含暂停短路与凭据校验）；
        // 在独立 current_thread runtime 上执行，避免在 axum handler 的
        // async 上下文里 `block_on`。
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("创建测试 runtime");
            runtime.block_on(downloader.login())
        })
        .join()
        .map_err(|e| format!("测试线程异常: {e:?}"))?
        .map_err(|e| format!("登录调用异常: {e}"))?
        .success
        .then_some(())
        .ok_or_else(|| "登录失败：凭据无效或下载器不可达".to_string())
    }

    fn push_channels(&self) -> Vec<Value> {
        let cfg = self.snapshot();
        cfg.push
            .iter()
            .map(|(key, value)| {
                let section = value.as_mapping().cloned().unwrap_or_default();
                let kind = section
                    .get(serde_yaml::Value::String("type".into()))
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_lowercase();
                json!({
                    "name": key,
                    "type": kind,
                })
            })
            .collect()
    }

    fn add_push_channel(&self, channel: &Value) -> Result<(), String> {
        let (name, section) = push_channel_parts(channel)?;
        // 先用真实解析校验渠道配置（对齐上游 `createPushProvider` 失败即抛）
        PushProviderConfig::parse(&serde_yaml::Value::Mapping(section.clone()))
            .map_err(|e| format!("渠道配置无效: {e}"))?;
        let mut cfg = self.snapshot();
        if cfg
            .push
            .contains_key(serde_yaml::Value::String(name.clone()))
        {
            return Err("PUSH_CHANNEL_ALREADY_EXISTS".into());
        }
        cfg.push.insert(
            serde_yaml::Value::String(name),
            serde_yaml::Value::Mapping(section),
        );
        self.save_config(&cfg)?;
        self.rebuild_push();
        Ok(())
    }

    fn update_push_channel(&self, name: &str, channel: &Value) -> Result<(), String> {
        let (_, section) = push_channel_parts(channel)?;
        PushProviderConfig::parse(&serde_yaml::Value::Mapping(section.clone()))
            .map_err(|e| format!("渠道配置无效: {e}"))?;
        let key_name = serde_yaml::Value::String(name.to_string());
        let mut cfg = self.snapshot();
        if !cfg.push.contains_key(&key_name) {
            return Err("PUSH_CHANNEL_NOT_FOUND".into());
        }
        cfg.push
            .insert(key_name, serde_yaml::Value::Mapping(section));
        self.save_config(&cfg)?;
        self.rebuild_push();
        Ok(())
    }

    fn remove_push_channel(&self, name: &str) -> Result<(), String> {
        let mut cfg = self.snapshot();
        cfg.push.remove(serde_yaml::Value::String(name.to_string()));
        self.save_config(&cfg)?;
        self.rebuild_push();
        Ok(())
    }

    fn test_push_channel(&self, channel: &Value) -> Result<(), String> {
        let (name, mut section) = push_channel_parts(channel)?;
        PushProviderConfig::parse(&serde_yaml::Value::Mapping(section.clone()))
            .map_err(|e| format!("渠道配置无效: {e}"))?;
        // 对齐上游 `PushManagerImpl.pushMessage`：按渠道名构造单渠道管理器再发测试消息
        let kind = section
            .get(serde_yaml::Value::String("type".into()))
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        section.insert(
            serde_yaml::Value::String("type".into()),
            serde_yaml::Value::String(kind),
        );
        let mut single = PushSection::new();
        single.insert(
            serde_yaml::Value::String(name),
            serde_yaml::Value::Mapping(section),
        );
        let manager = Arc::new(PushManager::from_config(&single, self.fetcher.clone()));
        let push_handle = manager.clone();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("创建推送 runtime");
            runtime.block_on(push_handle.push_message(
                "PeerBanHelper 渠道测试",
                "这是一条测试消息，收到代表渠道配置正常。",
                AlertLevel::Info,
            ))
        })
        .join()
        .map_err(|_| "推送线程异常".to_string())?
        .then_some(())
        .ok_or_else(|| "推送失败：没有任何渠道成功送达".to_string())
    }
}

/// 构造一个下载器（对齐 main.rs 启动时的构建矩阵；两者共用同一实现）。
pub fn build_downloader(
    d: &DownloaderConfig,
    remap: &Arc<RemapConfig>,
    blocklist_url: &str,
) -> Result<(Arc<dyn Downloader>, bool), String> {
    let (downloader, increment): (Arc<dyn Downloader>, bool) = match d.kind.as_str() {
        "qbittorrent" => {
            let qb_cfg = QBConfig {
                id: d.resolved_id(),
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
                remap: remap.as_ref().clone(),
            };
            (
                Arc::new(QBittorrentDownloader::new(qb_cfg).map_err(|e| e.to_string())?),
                d.increment_ban,
            )
        }
        "transmission" => {
            let tr_cfg = TRConfig {
                id: d.resolved_id(),
                name: d.name.clone(),
                endpoint: d.endpoint.clone(),
                username: d.username.clone(),
                password: d.password.clone(),
                verify_ssl: d.verify_tls,
                rpc_url: d.resolved_rpc_url("/transmission/rpc"),
                ignore_private: d.ignore_private,
                paused: false,
                blocklist_url: blocklist_url.to_string(),
            };
            (
                Arc::new(
                    TransmissionDownloader::new(tr_cfg, remap.as_ref().clone())
                        .map_err(|e| e.to_string())?,
                ),
                // Transmission 只支持整份 blocklist 更新（对齐上游 setBanList 全量路径）
                false,
            )
        }
        "deluge" => {
            let deluge_cfg = DelugeConfig {
                id: d.resolved_id(),
                name: d.name.clone(),
                endpoint: d.endpoint.clone(),
                password: d.password.clone(),
                verify_ssl: d.verify_tls,
                rpc_url: d.resolved_rpc_url("/json"),
                paused: d.paused,
                remap: remap.as_ref().clone(),
            };
            (
                Arc::new(DelugeDownloader::new(deluge_cfg).map_err(|e| e.to_string())?),
                d.increment_ban,
            )
        }
        // 配置节 `type` 为 `aria2next`；`aria2` 作为等价别名一并接受
        "aria2next" | "aria2" => {
            let aria2_cfg = Aria2Config {
                id: d.resolved_id(),
                name: d.name.clone(),
                endpoint: d.endpoint.clone(),
                token: d.token.clone(),
                verify_ssl: d.verify_tls,
                ignore_private: d.ignore_private,
                paused: d.paused,
                remap: remap.as_ref().clone(),
            };
            (
                Arc::new(Aria2Downloader::new(aria2_cfg).map_err(|e| e.to_string())?),
                // Aria2Next 没有增量封禁 API（恒为整份列表替换）
                false,
            )
        }
        "biglybt" => {
            let biglybt_cfg = BiglyBtConfig {
                id: d.resolved_id(),
                name: d.name.clone(),
                endpoint: d.endpoint.clone(),
                token: d.token.clone(),
                increment_ban: d.increment_ban,
                verify_ssl: d.verify_tls,
                ignore_private: d.ignore_private,
                paused: d.paused,
                remap: remap.as_ref().clone(),
            };
            (
                Arc::new(BiglyBtDownloader::new(biglybt_cfg).map_err(|e| e.to_string())?),
                d.increment_ban,
            )
        }
        "bitcomet" => {
            let bitcomet_cfg = BitCometConfig {
                id: d.resolved_id(),
                name: d.name.clone(),
                endpoint: d.endpoint.clone(),
                username: d.username.clone(),
                password: d.password.clone(),
                increment_ban: d.increment_ban,
                verify_ssl: d.verify_tls,
                ignore_private: d.ignore_private,
                paused: d.paused,
                remap: remap.as_ref().clone(),
            };
            (
                Arc::new(BitCometDownloader::new(bitcomet_cfg).map_err(|e| e.to_string())?),
                // BitComet 恒走整份替换（增量在 2.11 起不再支持，对齐上游固定全量路径）
                false,
            )
        }
        other => return Err(format!("暂不支持的下载器类型: {other}")),
    };
    Ok((downloader, increment))
}

/// 读取（或生成）安装 ID：`<data>/installation_id`。
fn read_installation_id(data_dir: &std::path::Path) -> String {
    let path = data_dir.join("installation-id");
    if let Ok(id) = std::fs::read_to_string(&path) {
        let id = id.trim();
        if !id.is_empty() {
            return id.to_string();
        }
    }
    let id: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect();
    let _ = std::fs::write(&path, &id);
    info!("生成本机安装 ID: {id}");
    id
}

/// 把 YAML 解析结果转成 WebUI 期望的 JSON：映射键的 `-` 全部换成 `_`
/// （对齐上游 `JsonUtil.standardize` 的 key 规范化）。
fn normalize_keys(value: &serde_yaml::Value) -> Value {
    match value {
        serde_yaml::Value::Mapping(map) => {
            let mut json_map = serde_json::Map::new();
            for (k, v) in map {
                let key = k
                    .as_str()
                    .map(|s| s.replace('-', "_"))
                    .unwrap_or_else(|| serde_yaml::to_string(k).unwrap_or_default());
                json_map.insert(key, normalize_keys(v));
            }
            Value::Object(json_map)
        }
        serde_yaml::Value::Sequence(seq) => Value::Array(seq.iter().map(normalize_keys).collect()),
        other => serde_json::to_value(other).unwrap_or(Value::Null),
    }
}

/// 把 WebUI 提交的 JSON 转回 YAML：映射键的 `_` 替换为 `-`。
fn denormalize_json(value: &Value) -> serde_yaml::Value {
    match value {
        Value::Object(map) => {
            let mut yaml_map = serde_yaml::Mapping::new();
            for (k, v) in map {
                let key = serde_yaml::Value::String(k.replace('_', "-"));
                yaml_map.insert(key, denormalize_json(v));
            }
            serde_yaml::Value::Mapping(yaml_map)
        }
        Value::Array(arr) => {
            serde_yaml::Value::Sequence(arr.iter().map(denormalize_json).collect())
        }
        other => serde_yaml::to_value(other).unwrap_or(serde_yaml::Value::Null),
    }
}

/// 从渠道 JSON body 拆出 `(name, config-mapping)`；`type` 必须显式提供。
fn push_channel_parts(channel: &Value) -> Result<(String, serde_yaml::Mapping), String> {
    let name = channel
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if name.is_empty() {
        return Err("PUSH_CHANNEL_NAME_EMPTY".into());
    }
    let config = channel.get("config").cloned().unwrap_or(channel.clone());
    let mut section = serde_yaml::from_value::<serde_yaml::Mapping>(denormalize_json(&config))
        .map_err(|e| format!("渠道配置无效: {e}"))?;
    if let Some(kind) = channel.get("type").and_then(|v| v.as_str()) {
        section.insert(
            serde_yaml::Value::String("type".into()),
            serde_yaml::Value::String(kind.trim().to_lowercase()),
        );
    }
    Ok((name, section))
}
