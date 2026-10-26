//! 推送级别与渠道枚举（字段/语义对齐上游）。

use super::*;
use crate::config::PushProviderConfig;
use pbh_downloader::http::HttpFetcher;
use std::sync::Arc;

/// 对齐上游 `alert/AlertLevel.java`：枚举顺序即 `ordinal()`，
/// `getHighestUnreadAlertLevel` 依赖该顺序。
///
/// 本阶段的封禁链路只产生 `ERROR`（`downloader-nat-setup-error`），其余级别为对齐上游
/// 告警 API 保留（`publishAlert` 可接受任意级别，`NtfyPushProvider` 也只按级别做死代码映射）。
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertLevel {
    Tip,
    Info,
    Warn,
    Error,
    Fatal,
}

/// `pbh_core::modules::AlertLevel` → 推送层的 [`AlertLevel`]（枚举成员一一对应），
/// 供监控模块的告警推送分支复用同一渲染/分发路径。
impl From<pbh_core::modules::AlertLevel> for AlertLevel {
    fn from(level: pbh_core::modules::AlertLevel) -> Self {
        match level {
            pbh_core::modules::AlertLevel::Tip => AlertLevel::Tip,
            pbh_core::modules::AlertLevel::Info => AlertLevel::Info,
            pbh_core::modules::AlertLevel::Warn => AlertLevel::Warn,
            pbh_core::modules::AlertLevel::Error => AlertLevel::Error,
            pbh_core::modules::AlertLevel::Fatal => AlertLevel::Fatal,
        }
    }
}

impl AlertLevel {
    /// 对齐 `AlertLevel.name()`（大写枚举名，用于标题前缀）。
    pub fn name(self) -> &'static str {
        match self {
            AlertLevel::Tip => "TIP",
            AlertLevel::Info => "INFO",
            AlertLevel::Warn => "WARN",
            AlertLevel::Error => "ERROR",
            AlertLevel::Fatal => "FATAL",
        }
    }

    /// 对齐 `NtfyPushProvider.mapLevelToPriority`。
    ///
    /// 注意：上游该方法是**私有且从未被调用**的死代码（`push` 用的是配置里的 `priority`），
    /// 此处保留映射仅为逐条对齐，实际发送时不使用。
    #[allow(dead_code)]
    pub fn ntfy_priority(self) -> i64 {
        match self {
            AlertLevel::Tip => 1,
            AlertLevel::Info => 2,
            AlertLevel::Warn => 3,
            AlertLevel::Error => 4,
            AlertLevel::Fatal => 5,
        }
    }
}

/// 一个运行时的推送渠道（对齐上游 `util/push/impl/*` 的九个 `AbstractPushProvider` 实现）。
///
/// 字段即对应 `loadFromYaml` 归一化后的 `Config`（例如 pushplus/serverchan 的空白可选字段
/// 在这里被置为 `None`，与上游 `isBlank() → null` 一致）；`fetcher` 即上游注入的 `HTTPUtil`。
pub enum PushProvider {
    PushPlus {
        name: String,
        fetcher: Arc<dyn HttpFetcher>,
        token: String,
        topic: Option<String>,
        channel: Option<String>,
    },
    ServerChan {
        name: String,
        fetcher: Arc<dyn HttpFetcher>,
        send_key: String,
        channel: Option<String>,
        open_id: Option<String>,
    },
    Telegram {
        name: String,
        fetcher: Arc<dyn HttpFetcher>,
        token: String,
        chat_id: String,
    },
    Bark {
        name: String,
        fetcher: Arc<dyn HttpFetcher>,
        backend_url: String,
        device_key: String,
        message_group: String,
    },
    PushDeer {
        name: String,
        fetcher: Arc<dyn HttpFetcher>,
        endpoint: String,
        push_key: String,
    },
    Gotify {
        name: String,
        fetcher: Arc<dyn HttpFetcher>,
        endpoint: String,
        priority: i64,
    },
    Ntfy {
        name: String,
        fetcher: Arc<dyn HttpFetcher>,
        server_url: String,
        topic: String,
        token: String,
        priority: i64,
        tags: String,
    },
    Webhook {
        name: String,
        fetcher: Arc<dyn HttpFetcher>,
        url: String,
        method: String,
        content_type: String,
        body_template: String,
        /// 保持配置顺序的自定义请求头
        headers: Vec<(String, String)>,
    },
    Smtp {
        name: String,
        settings: SmtpSettings,
    },
}

impl PushProvider {
    /// 对齐 `PushProvider.getName()`。
    pub fn name(&self) -> &str {
        match self {
            PushProvider::PushPlus { name, .. }
            | PushProvider::ServerChan { name, .. }
            | PushProvider::Telegram { name, .. }
            | PushProvider::Bark { name, .. }
            | PushProvider::PushDeer { name, .. }
            | PushProvider::Gotify { name, .. }
            | PushProvider::Ntfy { name, .. }
            | PushProvider::Webhook { name, .. }
            | PushProvider::Smtp { name, .. } => name,
        }
    }

    /// 对齐 `PushProvider.getConfigType()`。
    pub fn config_type(&self) -> &'static str {
        match self {
            PushProvider::PushPlus { .. } => "pushplus",
            PushProvider::ServerChan { .. } => "serverchan",
            PushProvider::Telegram { .. } => "telegram",
            PushProvider::Bark { .. } => "bark",
            PushProvider::PushDeer { .. } => "pushdeer",
            PushProvider::Gotify { .. } => "gotify",
            PushProvider::Ntfy { .. } => "ntfy",
            PushProvider::Webhook { .. } => "webhook",
            PushProvider::Smtp { .. } => "smtp",
        }
    }

    /// 对齐各 provider 的 `loadFromYaml`：把配置段归一化成运行时渠道实例。
    pub fn from_config(
        name: &str,
        config: &PushProviderConfig,
        fetcher: Arc<dyn HttpFetcher>,
    ) -> PushProvider {
        match config {
            // 上游 `loadFromYaml`：token/topic/channel 取默认空串，空白 ⇒ null
            PushProviderConfig::PushPlus {
                token,
                topic,
                channel,
            } => PushProvider::PushPlus {
                name: name.to_string(),
                fetcher,
                token: token.clone(),
                topic: blank_to_none(topic),
                channel: blank_to_none(channel),
            },
            PushProviderConfig::ServerChan {
                sendkey,
                channel,
                openid,
            } => PushProvider::ServerChan {
                name: name.to_string(),
                fetcher,
                send_key: sendkey.clone(),
                channel: blank_to_none(channel),
                open_id: blank_to_none(openid),
            },
            PushProviderConfig::Telegram { token, chatid } => PushProvider::Telegram {
                name: name.to_string(),
                fetcher,
                token: token.clone(),
                chat_id: chatid.clone(),
            },
            // 上游 `loadFromYaml` 不对 message_group 做空值归一化（空串照发）
            PushProviderConfig::Bark {
                backend_url,
                device_key,
                message_group,
            } => PushProvider::Bark {
                name: name.to_string(),
                fetcher,
                backend_url: backend_url.clone(),
                device_key: device_key.clone(),
                message_group: message_group.clone(),
            },
            PushProviderConfig::PushDeer { endpoint, pushkey } => PushProvider::PushDeer {
                name: name.to_string(),
                fetcher,
                endpoint: endpoint.clone(),
                push_key: pushkey.clone(),
            },
            PushProviderConfig::Gotify { endpoint, priority } => PushProvider::Gotify {
                name: name.to_string(),
                fetcher,
                endpoint: endpoint.clone(),
                priority: *priority,
            },
            PushProviderConfig::Ntfy {
                server_url,
                topic,
                token,
                priority,
                tags,
            } => PushProvider::Ntfy {
                name: name.to_string(),
                fetcher,
                server_url: server_url.clone(),
                topic: topic.clone(),
                token: token.clone(),
                priority: *priority,
                tags: tags.clone(),
            },
            PushProviderConfig::Webhook {
                url,
                method,
                content_type,
                body_template,
                headers,
            } => {
                PushProvider::Webhook {
                    name: name.to_string(),
                    fetcher,
                    url: url.clone(),
                    method: method.clone(),
                    content_type: content_type.clone(),
                    body_template: body_template.clone(),
                    // 对齐 `applyCustomHeaders`：值为 null 的表项被忽略
                    headers: headers
                        .iter()
                        .filter_map(|(k, v)| v.as_ref().map(|v| (k.clone(), v.clone())))
                        .collect(),
                }
            }
            PushProviderConfig::Smtp {
                host,
                port,
                auth,
                username,
                password,
                sender,
                name: sender_name,
                receiver,
                encryption,
                send_partial,
            } => PushProvider::Smtp {
                name: name.to_string(),
                settings: SmtpSettings {
                    host: host.clone(),
                    port: *port,
                    auth: *auth,
                    username: username.clone(),
                    password: password.clone(),
                    sender: sender.clone(),
                    sender_name: sender_name.clone(),
                    receivers: receiver.clone(),
                    encryption: encryption.clone(),
                    send_partial: *send_partial,
                },
            },
        }
    }

    /// 对齐 `PushProvider.push(title, content, level)`。
    ///
    /// 返回值语义与上游一致：
    /// - `Ok(true)`：上游 `return true`（HTTP 渠道成功；SMTP 发送成功）；
    /// - `Ok(false)`：上游显式 `return false`（仅 `SmtpPushProvider.push` 捕获发送异常的分支）；
    /// - `Err`：上游抛出的 `IllegalStateException` / `IllegalArgumentException`
    ///   （由 `PushManagerImpl.pushMessage` 捕获并记录日志）。
    pub async fn push(
        &self,
        title: &str,
        content: &str,
        level: AlertLevel,
    ) -> anyhow::Result<bool> {
        self.push_at(title, content, level, now_millis()).await
    }

    /// [`PushProvider::push`] 的显式时间版本：`now_ms` 仅 webhook 模板的
    /// `{date}`/`{time}`/`{datetime}` 使用（上游 `System.currentTimeMillis()`），
    /// 独立参数便于测试注入固定时间。
    pub async fn push_at(
        &self,
        title: &str,
        content: &str,
        level: AlertLevel,
        now_ms: i64,
    ) -> anyhow::Result<bool> {
        match self {
            PushProvider::PushPlus {
                fetcher,
                token,
                topic,
                channel,
                ..
            } => {
                pushplus_push(
                    fetcher,
                    token,
                    topic.as_deref(),
                    channel.as_deref(),
                    title,
                    content,
                )
                .await
            }
            PushProvider::ServerChan {
                fetcher,
                send_key,
                channel,
                open_id,
                ..
            } => {
                serverchan_push(
                    fetcher,
                    send_key,
                    channel.as_deref(),
                    open_id.as_deref(),
                    title,
                    content,
                )
                .await
            }
            PushProvider::Telegram {
                fetcher,
                token,
                chat_id,
                ..
            } => telegram_push(fetcher, token, chat_id, title, content).await,
            PushProvider::Bark {
                fetcher,
                backend_url,
                device_key,
                message_group,
                ..
            } => {
                bark_push(
                    fetcher,
                    backend_url,
                    device_key,
                    message_group,
                    title,
                    content,
                )
                .await
            }
            PushProvider::PushDeer {
                fetcher,
                endpoint,
                push_key,
                ..
            } => pushdeer_push(fetcher, endpoint, push_key, title, content).await,
            PushProvider::Gotify {
                fetcher,
                endpoint,
                priority,
                ..
            } => gotify_push(fetcher, endpoint, *priority, title, content).await,
            PushProvider::Ntfy {
                fetcher,
                server_url,
                topic,
                token,
                priority,
                tags,
                ..
            } => {
                ntfy_push(
                    fetcher, server_url, topic, token, *priority, tags, title, content,
                )
                .await
            }
            PushProvider::Webhook {
                name,
                fetcher,
                url,
                method,
                content_type,
                body_template,
                headers,
            } => {
                webhook_push(WebhookTask {
                    name,
                    fetcher,
                    url,
                    method,
                    content_type,
                    body_template,
                    headers,
                    title,
                    content,
                    level,
                    now_ms,
                })
                .await
            }
            PushProvider::Smtp { settings, .. } => smtp_push(settings, title, content).await,
        }
    }
}
