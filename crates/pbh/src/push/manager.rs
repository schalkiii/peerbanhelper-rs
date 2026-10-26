//! PushManager 与 AlertManager（渠道配置、告警落库与发送）。

use super::*;
use crate::config::{PushProviderConfig, PushSection};
use pbh_core::i18n::{TranslationComponent, Translator};
use pbh_downloader::http::HttpFetcher;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// PushManager
// ---------------------------------------------------------------------------

/// 对齐上游 `PushManagerImpl`：持有按配置顺序注册的渠道列表。
pub struct PushManager {
    provider_list: Vec<PushProvider>,
}

impl PushManager {
    /// 对齐 `PushManagerImpl(HTTPUtil)` 构造时的 `reloadConfig()`：
    /// 逐条读取 `push:` 段，单个渠道加载失败只记录日志并跳过（其余渠道照常注册）；
    /// 未识别/缺失 `type` 的渠道同样跳过。配置为空 ⇒ 空渠道列表（不产生任何流量）。
    pub fn from_config(section: &PushSection, fetcher: Arc<dyn HttpFetcher>) -> Self {
        let mut provider_list = Vec::new();
        for (key, raw) in section {
            let Some(name) = key.as_str() else {
                error!("Unable to load Push Provider: 渠道名必须是字符串: {key:?}");
                continue;
            };
            // 对齐上游 `name.replace(".", "-")`
            let name = name.replace('.', "-");
            match PushProviderConfig::parse(raw)
                .map(|config| PushProvider::from_config(&name, &config, fetcher.clone()))
            {
                Ok(provider) => provider_list.push(provider),
                Err(e) => error!("Unable to load Push Provider: {name}: {e}"),
            }
        }
        Self { provider_list }
    }

    /// 对齐 `PushManagerImpl.getProviderList()`。
    pub fn provider_list(&self) -> &[PushProvider] {
        &self.provider_list
    }

    /// 对齐 `PushManagerImpl.pushMessage(title, description, level)`：
    /// 按注册顺序逐个推送，单渠道异常只记录日志、其余渠道继续，
    /// 整体成功 = 任一渠道成功（上游 `AtomicBoolean` 的 OR 语义）。
    pub async fn push_message(&self, title: &str, description: &str, level: AlertLevel) -> bool {
        let mut any_success = false;
        for provider in &self.provider_list {
            match provider.push(title, description, level).await {
                Ok(true) => any_success = true,
                Ok(false) => {}
                // 对齐 `log.error(tlUI(Lang.UNABLE_TO_PUSH_ALERT_VIA, provider.getClass().getName()))`
                // （Rust 无类名，用「渠道类型（渠道名）」替代）
                Err(e) => error!(
                    "无法通过 {}（{}）推送警报: {e}",
                    provider.config_type(),
                    provider.name()
                ),
            }
        }
        any_success
    }
}

// ---------------------------------------------------------------------------
// AlertManager
// ---------------------------------------------------------------------------

/// 一条已发布告警的进程内记录（替代上游 `alert` 表）。
struct AlertRecord {
    /// 供 `getHighestUnreadAlertLevel` 使用（对齐上游 `AlertEntity.level`）
    #[allow(dead_code)]
    level: AlertLevel,
    read: bool,
}

/// 对齐上游 `alert/AlertManagerImpl` 的告警发布子集。
///
/// 未移植部分（本阶段无 WebUI、无 `alert` 表，见模块文档第 2、3 条）：
/// - `AlertService` 的落库与 30 天清理（`cleanup`）、`getUnreadAlerts()` 列表；
/// - `Main.getGuiManager().createNotification(...)` ⇒ 以同级别的 tracing 事件替代；
/// - Sentry 上报（上游在 catch 中 `Sentry.captureException`）。
pub struct AlertManager {
    /// 可替换的推送管理器：Web 渠道管理（`/api/push`）会整体重建并热替换，
    /// 告警推送始终使用最新实例（对齐上游 `PushManagerImpl.reloadConfig()` 的即时生效语义）。
    push_manager: Mutex<Arc<PushManager>>,
    translator: Arc<Translator>,
    locale: String,
    /// identifier → 记录；`identifierAlertExists` = 存在且未读，
    /// `identifierAlertExistsIncludeRead` = 存在即真。
    alerts: Mutex<HashMap<String, AlertRecord>>,
}

impl AlertManager {
    pub fn new(
        push_manager: Arc<PushManager>,
        translator: Arc<Translator>,
        locale: impl Into<String>,
    ) -> Self {
        Self {
            push_manager: Mutex::new(push_manager),
            translator,
            locale: locale.into(),
            alerts: Mutex::new(HashMap::new()),
        }
    }

    /// 文案表（供 ban wave 等复用同一 translator/locale 渲染日志）。
    pub fn translator(&self) -> &Arc<Translator> {
        &self.translator
    }

    /// 当前 locale（与上游 `Main.DEF_LOCALE` 同义）。
    pub fn locale(&self) -> &str {
        &self.locale
    }

    /// 对齐 `AlertManager.identifierAlertExists`（存在且**未读**）。
    pub fn identifier_alert_exists(&self, identifier: &str) -> bool {
        self.alerts
            .lock()
            .map(|alerts| alerts.get(identifier).is_some_and(|record| !record.read))
            .unwrap_or(false)
    }

    /// 对齐 `AlertManager.identifierAlertExistsIncludeRead`。
    pub fn identifier_alert_exists_include_read(&self, identifier: &str) -> bool {
        self.alerts
            .lock()
            .map(|alerts| alerts.contains_key(identifier))
            .unwrap_or(false)
    }

    /// 对齐 `AlertManager.markAlertAsRead`（上游写库；此处只改进程内状态）。
    ///
    /// 上游由 WebUI「标记已读」调用；本阶段无 WebUI，可供后续接入。
    #[allow(dead_code)]
    pub fn mark_alert_as_read(&self, identifier: &str) {
        if let Ok(mut alerts) = self.alerts.lock() {
            if let Some(record) = alerts.get_mut(identifier) {
                record.read = true;
            }
        }
    }

    /// 热替换推送管理器（`/api/push` 保存渠道后调用，对齐上游 `PushManagerImpl`
    /// 的 add/remove + `savePushProviders()` 即时生效语义）。
    pub fn update_push_manager(&self, manager: Arc<PushManager>) {
        if let Ok(mut slot) = self.push_manager.lock() {
            *slot = manager;
        }
    }

    /// 对齐 `AlertManager.getHighestUnreadAlertLevel`（未读告警中 `ordinal()` 最大者）。
    #[allow(dead_code)]
    pub fn get_highest_unread_alert_level(&self) -> Option<AlertLevel> {
        self.alerts
            .lock()
            .map(|alerts| {
                alerts
                    .values()
                    .filter(|record| !record.read)
                    .map(|record| record.level)
                    .max()
            })
            .unwrap_or(None)
    }

    /// 对齐 `AlertManagerImpl.publishAlert(push, level, identifier, title, content)`：
    /// 1. 已有相同 identifier 的未读告警 ⇒ 直接返回；
    /// 2. 记录告警（上游为 `alertDao.saveOrUpdate`）；
    /// 3. `push = true` 时调用 [`PushManager::push_message`]，标题为
    ///    `"[PeerBanHelper/<LEVEL>] " + tlUI(title)`、正文为 `tlUI(content)`，
    ///    全部渠道失败时记录 error；
    /// 4. 按级别映射通知级别（ERROR/FATAL → error、WARN → warn、其余 → info，
    ///    对齐 `AlertManagerImpl` 里的 `slf4jLevel` 分支）。
    ///
    /// 与上游的唯一差异：未配置任何渠道时**不**记录 error（上游 `pushMessage` 返回 false
    /// 会打印 `UNABLE_TO_PUSH_ALERT_VIA_PROVIDERS`），改为 debug 记录，
    /// 以便默认（空 `push:` 段）配置静默运行。
    pub async fn publish_alert(
        &self,
        push: bool,
        level: AlertLevel,
        identifier: &str,
        title: &TranslationComponent,
        content: &TranslationComponent,
    ) {
        if self.identifier_alert_exists(identifier) {
            return;
        }
        if let Ok(mut alerts) = self.alerts.lock() {
            alerts.insert(identifier.to_string(), AlertRecord { level, read: false });
        }
        let title_text = self.translator.render(title, &self.locale);
        let content_text = self.translator.render(content, &self.locale);
        if push {
            // 注意：只借用瞬时快照（Arc<PushManager> 是 Send），避免把
            // MutexGuard 带过 await 导致 async block 非 Send。
            let manager = {
                let slot = self.push_manager.lock().unwrap_or_else(|e| e.into_inner());
                (*slot).clone()
            };
            if manager.provider_list().is_empty() {
                debug!("未配置任何推送渠道，跳过推送: {identifier}");
            } else if !manager
                .push_message(
                    &format!("[PeerBanHelper/{}] {title_text}", level.name()),
                    &content_text,
                    level,
                )
                .await
            {
                error!("无法通过任何推送渠道推送警报");
            }
        }
        match level {
            AlertLevel::Error | AlertLevel::Fatal => error!("{title_text}: {content_text}"),
            AlertLevel::Warn => warn!("{title_text}: {content_text}"),
            AlertLevel::Tip | AlertLevel::Info => info!("{title_text}: {content_text}"),
        }
    }
}
