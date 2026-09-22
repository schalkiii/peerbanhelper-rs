//! 推送（Push）与告警（Alert）子系统。
//!
//! 对齐上游（v9.5.1）：
//! - `util/push/PushManager` + `PushManagerImpl`：按配置注册渠道、逐个推送、单渠道失败隔离、
//!   整体成功 = 任一渠道成功（OR）；
//! - `util/push/PushProvider` + `AbstractPushProvider`：渠道契约与 markdown → HTML 渲染；
//! - `util/push/impl/*`：九个渠道（pushplus / serverchan / smtp / telegram / bark / pushdeer /
//!   gotify / ntfy / webhook）的请求构造与成功判据；
//! - `alert/AlertManager` + `AlertManagerImpl` + `AlertLevel`：告警发布、identifier 去重、
//!   标题前缀、日志级别映射。
//!
//! 与上游的差异（逐条在下文注明）：
//! 1. 配置节名为 `push:`（上游为 `push-notification:`），键 = 渠道名、段内 `type` 决定类型；
//! 2. 告警落库（`AlertService`/`alert` 表）未移植 ⇒ `identifierAlertExists*` 的去重状态改为
//!    进程内集合（重启即丢失，WebUI 也读不到历史告警）；`markAlertAsRead` 只影响该集合；
//! 3. `GuiManager.createNotification`（JavaFX 通知）以相同级别的 tracing 事件替代；
//! 4. 出站 HTTP 走 `pbh_downloader::http::HttpFetcher`（生产环境为 `ReqwestFetcher`，
//!    测试注入 mock）；
//! 5. 上游没有「按告警级别路由渠道」的逻辑：`AlertLevel` 只进入标题前缀
//!    `[PeerBanHelper/<LEVEL>] ` 并透传给渠道（`NtfyPushProvider.mapLevelToPriority` 是
//!    未被调用的死代码，见 [`AlertLevel::ntfy_priority`]）。

use crate::config::{PushProviderConfig, PushSection};
use base64::Engine as _;
use lettre::message::header::ContentType;
use lettre::message::{Mailbox, Message, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::{Tls, TlsParameters};
use lettre::transport::smtp::SmtpTransport;
use lettre::Transport as _;
use pbh_core::i18n::{TranslationComponent, Translator};
use pbh_downloader::http::{HttpFetcher, HttpRequest, HttpResponse};
use serde_json::{Map as JsonMap, Value as JsonValue};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tracing::{debug, error, info, warn};

/// 上游 Telegram / Bark / Ntfy 共用的图标地址
/// （`TelegramPushProvider` / `BarkPushProvider` 内联字面量、`NtfyPushProvider.ICON_URL`）。
const ICON_URL: &str = "https://raw.githubusercontent.com/PBH-BTN/PeerBanHelper/refs/heads/master/src/main/resources/assets/icon.png";

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
            PushProviderConfig::PushPlus { token, topic, channel } => PushProvider::PushPlus {
                name: name.to_string(),
                fetcher,
                token: token.clone(),
                topic: blank_to_none(topic),
                channel: blank_to_none(channel),
            },
            PushProviderConfig::ServerChan { sendkey, channel, openid } => PushProvider::ServerChan {
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
            PushProviderConfig::Bark { backend_url, device_key, message_group } => PushProvider::Bark {
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
            PushProviderConfig::Ntfy { server_url, topic, token, priority, tags } => PushProvider::Ntfy {
                name: name.to_string(),
                fetcher,
                server_url: server_url.clone(),
                topic: topic.clone(),
                token: token.clone(),
                priority: *priority,
                tags: tags.clone(),
            },
            PushProviderConfig::Webhook { url, method, content_type, body_template, headers } => {
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
            PushProvider::PushPlus { fetcher, token, topic, channel, .. } => {
                pushplus_push(fetcher, token, topic.as_deref(), channel.as_deref(), title, content)
                    .await
            }
            PushProvider::ServerChan { fetcher, send_key, channel, open_id, .. } => {
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
            PushProvider::Telegram { fetcher, token, chat_id, .. } => {
                telegram_push(fetcher, token, chat_id, title, content).await
            }
            PushProvider::Bark { fetcher, backend_url, device_key, message_group, .. } => {
                bark_push(fetcher, backend_url, device_key, message_group, title, content).await
            }
            PushProvider::PushDeer { fetcher, endpoint, push_key, .. } => {
                pushdeer_push(fetcher, endpoint, push_key, title, content).await
            }
            PushProvider::Gotify { fetcher, endpoint, priority, .. } => {
                gotify_push(fetcher, endpoint, *priority, title, content).await
            }
            PushProvider::Ntfy { fetcher, server_url, topic, token, priority, tags, .. } => {
                ntfy_push(fetcher, server_url, topic, token, *priority, tags, title, content).await
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
                webhook_push(
                    WebhookTask {
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
                    },
                )
                .await
            }
            PushProvider::Smtp { settings, .. } => smtp_push(settings, title, content).await,
        }
    }
}

/// 对齐各 `loadFromYaml` 内的 `isBlank() → null` 归一化（pushplus 的 topic/channel、
/// serverchan 的 channel/openid）。
fn blank_to_none(value: &str) -> Option<String> {
    if value.trim().is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// okhttp `Response.isSuccessful()`：状态码 ∈ [200, 300)。
fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// 当前时间戳（毫秒），对齐 `System.currentTimeMillis()`。
fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// 执行 HTTP 请求，网络异常按上游 `IOException` 包装为渠道级错误
/// （各 provider 的 `catch (Exception e) → IllegalStateException("Failed to ...")`）。
async fn execute(
    fetcher: &Arc<dyn HttpFetcher>,
    request: HttpRequest,
    provider: &str,
) -> anyhow::Result<HttpResponse> {
    fetcher
        .execute(request)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send push message to {provider}: {e}"))
}

// ---------------------------------------------------------------------------
// pushplus
// ---------------------------------------------------------------------------

/// 对齐 `PushPlusPushProvider.push`：
/// `POST https://www.pushplus.plus/send`，`Content-Type: application/json`，
/// JSON 体 `{token, [topic], [channel], title, content, template:"markdown"}`（HashMap ⇒ Gson，
/// 空白的 topic/channel 在加载期已置 null，因此**整键不出现**），
/// 成功判据：HTTP 2xx **且** 响应体 `code == 200`。
async fn pushplus_push(
    fetcher: &Arc<dyn HttpFetcher>,
    token: &str,
    topic: Option<&str>,
    channel: Option<&str>,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let mut args = JsonMap::new();
    args.insert("token".into(), JsonValue::from(token));
    if let Some(topic) = topic {
        args.insert("topic".into(), JsonValue::from(topic));
    }
    if let Some(channel) = channel {
        args.insert("channel".into(), JsonValue::from(channel));
    }
    args.insert("title".into(), JsonValue::from(title));
    args.insert("content".into(), JsonValue::from(content));
    args.insert("template".into(), JsonValue::from("markdown"));

    let request = HttpRequest::post_json(
        "https://www.pushplus.plus/send",
        serde_json::to_string(&JsonValue::Object(args))?,
    );
    let response = execute(fetcher, request, "PushPlus").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to PushPlus: {}",
            response.body
        ));
    }
    let parsed: JsonValue = serde_json::from_str(&response.body).unwrap_or(JsonValue::Null);
    match parsed.get("code").and_then(JsonValue::as_i64) {
        Some(200) => Ok(true),
        Some(_) => Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to PushPlus: {}",
            parsed
                .get("msg")
                .and_then(JsonValue::as_str)
                .unwrap_or_default()
        )),
        // 上游此处 `ppr.getCode()` 为 null 会 NPE，并被外层 catch 记为推送失败
        None => Err(anyhow::anyhow!(
            "Failed to send push message to PushPlus: {}",
            response.body
        )),
    }
}

// ---------------------------------------------------------------------------
// serverchan
// ---------------------------------------------------------------------------

/// 对齐 `ServerChanPushProvider.push`：
/// `POST https://sctapi.ftqq.com/<sendkey>.send`，`Content-Type: application/json`，
/// JSON 体 `{title, desp, text, [channel], [openid]}`（channel/openid 为 null 时整键不出现），
/// 成功判据：HTTP 2xx（非 2xx 时按 `ServerChanResponse.message` 报错）。
async fn serverchan_push(
    fetcher: &Arc<dyn HttpFetcher>,
    send_key: &str,
    channel: Option<&str>,
    open_id: Option<&str>,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let mut map = JsonMap::new();
    map.insert("title".into(), JsonValue::from(title));
    map.insert("desp".into(), JsonValue::from(content));
    map.insert("text".into(), JsonValue::from(title));
    if let Some(channel) = channel {
        map.insert("channel".into(), JsonValue::from(channel));
    }
    if let Some(open_id) = open_id {
        map.insert("openid".into(), JsonValue::from(open_id));
    }

    let request = HttpRequest::post_json(
        format!("https://sctapi.ftqq.com/{send_key}.send"),
        serde_json::to_string(&JsonValue::Object(map))?,
    );
    let response = execute(fetcher, request, "ServerChan").await?;
    if !is_success(response.status) {
        let message = serde_json::from_str::<JsonValue>(&response.body)
            .ok()
            .and_then(|v| v.get("message").and_then(JsonValue::as_str).map(str::to_string))
            .unwrap_or(response.body);
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to ServerChan: {message}"
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// telegram
// ---------------------------------------------------------------------------

/// 对齐 `TelegramPushProvider.push`：
/// `POST https://api.telegram.org/bot<token>/sendPhoto`，`Content-Type: application/json`，
/// JSON 体 `{chat_id, caption, text, photo, parse_mode}`，其中正文为 `*<title>*\n<content>`、
/// `photo` 为内置图标地址、`parse_mode` = `Markdown`；
/// 成功判据：HTTP 2xx（非 2xx 时按 `TelegramErrResponse.description` 报错）。
async fn telegram_push(
    fetcher: &Arc<dyn HttpFetcher>,
    token: &str,
    chat_id: &str,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let markdown = format!("*{title}*\n{content}");
    let mut map = JsonMap::new();
    map.insert("chat_id".into(), JsonValue::from(chat_id));
    map.insert("caption".into(), JsonValue::from(markdown.as_str()));
    map.insert("text".into(), JsonValue::from(markdown.as_str()));
    map.insert("photo".into(), JsonValue::from(ICON_URL));
    map.insert("parse_mode".into(), JsonValue::from("Markdown"));

    let request = HttpRequest::post_json(
        format!("https://api.telegram.org/bot{token}/sendPhoto"),
        serde_json::to_string(&JsonValue::Object(map))?,
    );
    let response = execute(fetcher, request, "Telegram").await?;
    if !is_success(response.status) {
        let description = serde_json::from_str::<JsonValue>(&response.body)
            .ok()
            .and_then(|v| {
                v.get("description")
                    .and_then(JsonValue::as_str)
                    .map(str::to_string)
            })
            .unwrap_or(response.body);
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to Telegram: {description}"
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// bark
// ---------------------------------------------------------------------------

/// 对齐 `BarkPushProvider.push`：
/// `POST <backend_url>`（默认 `https://api.day.app/push`），`Content-Type: application/json`，
/// JSON 体 `{title, body, device_key, group, icon}`（`group` 为配置值，空串照发）；
/// 成功判据：HTTP 2xx。
async fn bark_push(
    fetcher: &Arc<dyn HttpFetcher>,
    backend_url: &str,
    device_key: &str,
    message_group: &str,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let mut map = JsonMap::new();
    map.insert("title".into(), JsonValue::from(title));
    map.insert("body".into(), JsonValue::from(content));
    map.insert("device_key".into(), JsonValue::from(device_key));
    map.insert("group".into(), JsonValue::from(message_group));
    map.insert("icon".into(), JsonValue::from(ICON_URL));

    let request =
        HttpRequest::post_json(backend_url, serde_json::to_string(&JsonValue::Object(map))?);
    let response = execute(fetcher, request, "Bark").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to Bark: {}",
            response.body
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// pushdeer
// ---------------------------------------------------------------------------

/// 对齐 `PushDeerPushProvider.push`：
/// `POST <endpoint>`（默认 `https://api2.pushdeer.com/message/push`），
/// `Content-Type: application/json`，
/// JSON 体 `{pushkey, text, desp, type:"markdown"}`；成功判据：HTTP 2xx。
async fn pushdeer_push(
    fetcher: &Arc<dyn HttpFetcher>,
    endpoint: &str,
    push_key: &str,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let mut map = JsonMap::new();
    map.insert("pushkey".into(), JsonValue::from(push_key));
    map.insert("text".into(), JsonValue::from(title));
    map.insert("desp".into(), JsonValue::from(content));
    map.insert("type".into(), JsonValue::from("markdown"));

    let request = HttpRequest::post_json(endpoint, serde_json::to_string(&JsonValue::Object(map))?);
    let response = execute(fetcher, request, "PushDeer").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to PushDeer: {}",
            response.body
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// gotify
// ---------------------------------------------------------------------------

/// 对齐 `GotifyPushProvider.push`：
/// `POST <endpoint>`（默认 `https://push.example.de/message?token=<apptoken>`），
/// `Content-Type: application/x-www-form-urlencoded`，
/// 表单 `title=<title>&message=<content>&priority=<priority>`；成功判据：HTTP 2xx。
///
/// 注：上游还构造了一个未被使用的 `HashMap`（死代码），此处不移植。
/// Content-Type 由 `HttpRequest` 的表单字段在传输层产生（reqwest 写入
/// `application/x-www-form-urlencoded`），故不再显式追加同名请求头，避免重复头。
async fn gotify_push(
    fetcher: &Arc<dyn HttpFetcher>,
    endpoint: &str,
    priority: i64,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let request = HttpRequest::post_form(
        endpoint,
        vec![
            ("title".to_string(), title.to_string()),
            ("message".to_string(), content.to_string()),
            ("priority".to_string(), priority.to_string()),
        ],
    );
    let response = execute(fetcher, request, "Gotify").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to Gotify: {}",
            response.body
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// ntfy
// ---------------------------------------------------------------------------

/// 对齐 `NtfyPushProvider.push`：
/// `POST <server_url>/<topic>`（默认 `https://ntfy.sh`），
/// 体为原始正文（`Content-Type: text/plain; charset=utf-8`，内容即 `content`），
/// 请求头 `Title: =?UTF-8?B?<base64(title)>?=`、`Priority: <配置值>`、`Icon: <图标地址>`，
/// 以及 token 非空白时的 `Authorization: Bearer <token>`、tags 非空白时的 `Tags: <tags>`；
/// 成功判据：HTTP 2xx。
#[allow(clippy::too_many_arguments)] // 逐参对齐上游 NtfyPushProvider 的配置字段
async fn ntfy_push(
    fetcher: &Arc<dyn HttpFetcher>,
    server_url: &str,
    topic: &str,
    token: &str,
    priority: i64,
    tags: &str,
    title: &str,
    content: &str,
) -> anyhow::Result<bool> {
    let encoded_title = format!(
        "=?UTF-8?B?{}?=",
        base64::engine::general_purpose::STANDARD.encode(title.as_bytes())
    );
    let mut request = HttpRequest {
        method: "POST".into(),
        url: format!("{server_url}/{topic}"),
        body: Some(content.to_string()),
        headers: vec![("Content-Type".into(), "text/plain; charset=utf-8".into())],
        ..Default::default()
    };
    request = request
        .with_header("Title", &encoded_title)
        .with_header("Priority", &priority.to_string())
        .with_header("Icon", ICON_URL);
    if !token.trim().is_empty() {
        request = request.with_header("Authorization", &format!("Bearer {token}"));
    }
    if !tags.trim().is_empty() {
        request = request.with_header("Tags", tags);
    }

    let response = execute(fetcher, request, "Ntfy").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP Failed while sending push messages to Ntfy: {}",
            response.body
        ));
    }
    Ok(true)
}

// ---------------------------------------------------------------------------
// webhook
// ---------------------------------------------------------------------------

/// webhook 一次推送所需的全部输入（字段较多，用结构体传参）。
struct WebhookTask<'a> {
    name: &'a str,
    fetcher: &'a Arc<dyn HttpFetcher>,
    url: &'a str,
    method: &'a str,
    content_type: &'a str,
    body_template: &'a str,
    headers: &'a [(String, String)],
    title: &'a str,
    content: &'a str,
    level: AlertLevel,
    now_ms: i64,
}

/// 对齐 `WebhookPushProvider.push`：URL 与请求体都做模板替换，
/// 成功判据：HTTP 2xx。
async fn webhook_push(task: WebhookTask<'_>) -> anyhow::Result<bool> {
    if task.url.trim().is_empty() {
        return Err(anyhow::anyhow!("Webhook URL cannot be empty"));
    }

    let method = webhook_normalize_method(task.method)?;
    let content_type = webhook_normalize_content_type(task.content_type);
    let rendered_body = render_template(
        task.body_template,
        task.name,
        task.title,
        task.content,
        task.level,
        &content_type,
        false,
        task.now_ms,
    );
    let rendered_url = render_template(
        task.url,
        task.name,
        task.title,
        task.content,
        task.level,
        &content_type,
        true,
        task.now_ms,
    );

    // `createRequestBody`：GET 无请求体；POST 恒有请求体（内容可为空串）
    let body = if method == "GET" { None } else { Some(rendered_body) };
    let mut request = HttpRequest {
        method: method.clone(),
        url: rendered_url,
        body,
        ..Default::default()
    };
    // `applyContentType`：非 GET 且有请求体时写入 Content-Type
    if method != "GET" && request.body.is_some() {
        request = request.with_header("Content-Type", &content_type);
    }
    // `applyCustomHeaders`：键非空白、值非 null；值按 RFC 8187 编码非 ASCII
    for (key, value) in task.headers {
        if !key.trim().is_empty() {
            request = request.with_header(key, &encode_header_value(value));
        }
    }

    let response = execute(task.fetcher, request, "Webhook").await?;
    if !is_success(response.status) {
        return Err(anyhow::anyhow!(
            "HTTP failed while sending push messages to Webhook: {}",
            response.body
        ));
    }
    Ok(true)
}

/// 对齐 `WebhookPushProvider.normalizeMethod`：空 ⇒ POST；仅接受 GET/POST（大写归一）。
fn webhook_normalize_method(method: &str) -> anyhow::Result<String> {
    if method.trim().is_empty() {
        return Ok("POST".to_string());
    }
    let normalized = method.trim().to_uppercase();
    match normalized.as_str() {
        "GET" | "POST" => Ok(normalized),
        _ => Err(anyhow::anyhow!("Unsupported webhook method: {method}")),
    }
}

/// 对齐 `WebhookPushProvider.normalizeContentType`：空/非法 ⇒ `application/json`。
///
/// 上游用 okhttp `MediaType.parse(...).toString()` 做规范化；本移植只做 trim 与
/// `type/subtype` 形状校验（参数原样保留），无法解析时回退默认值。
fn webhook_normalize_content_type(content_type: &str) -> String {
    let trimmed = content_type.trim();
    if trimmed.is_empty() {
        return "application/json".to_string();
    }
    let mut parts = trimmed.splitn(2, '/');
    let (Some(main), Some(sub)) = (parts.next(), parts.next()) else {
        return "application/json".to_string();
    };
    if main.trim().is_empty() || sub.trim().is_empty() {
        return "application/json".to_string();
    }
    trimmed.to_string()
}

/// 对齐 `WebhookPushProvider.renderTemplate`：替换 `{title}`/`{content}`/`{level}`/
/// `{date}`/`{time}`/`{datetime}`/`{channelName}`。
///
/// `url_encode` 为 true（URL 场景）时按 `URLEncoder.encode(..., UTF_8).replace("+", "%20")` 编码；
/// 否则当 content-type 含 `json`（大小写不敏感）时按 JSON 字符串转义后去掉两侧引号。
#[allow(clippy::too_many_arguments)]
fn render_template(
    template: &str,
    channel_name: &str,
    title: &str,
    content: &str,
    level: AlertLevel,
    content_type: &str,
    url_encode: bool,
    now_ms: i64,
) -> String {
    let json = content_type.to_lowercase().contains("json");
    template
        .replace("{title}", &transform_value(title, url_encode, json))
        .replace("{content}", &transform_value(content, url_encode, json))
        .replace("{level}", &transform_value(level.name(), url_encode, json))
        .replace("{date}", &transform_value(&format_date_only(now_ms), url_encode, json))
        .replace("{time}", &transform_value(&format_time_only(now_ms), url_encode, json))
        .replace("{datetime}", &transform_value(&format_date_time(now_ms), url_encode, json))
        .replace("{channelName}", &transform_value(channel_name, url_encode, json))
}

/// 对齐 `WebhookPushProvider.transformValue`（`null` 值在 Rust 侧不存在，故不做空判断）。
fn transform_value(value: &str, url_encode: bool, json: bool) -> String {
    if url_encode {
        return url_encode_component(value);
    }
    if !json {
        return value.to_string();
    }
    // 对齐 `JsonUtil.standard().toJson(value)` 后去掉首尾引号（Gson 已 disableHtmlEscaping）
    let quoted = serde_json::to_string(value).unwrap_or_else(|_| String::from("\"\""));
    quoted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or_default()
        .to_string()
}

/// 对齐 `AbstractPushProvider.markdown2Html`（上游用 commonmark-java 的 `HtmlRenderer`；
/// 本移植用 pulldown-cmark 默认选项，同为 CommonMark 规范实现）。
fn markdown_to_html(markdown: &str) -> String {
    let parser = pulldown_cmark::Parser::new(markdown);
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, parser);
    out
}

/// 对齐 `TimeUtil.formatDateOnly`：`SimpleDateFormat("yyyy-MM-dd")`（系统时区）。
fn format_date_only(now_ms: i64) -> String {
    format_local(now_ms, "%Y-%m-%d")
}

/// 对齐 `TimeUtil.formatTimeOnly`：`SimpleDateFormat("HH:mm:ss")`（系统时区）。
fn format_time_only(now_ms: i64) -> String {
    format_local(now_ms, "%H:%M:%S")
}

/// 对齐 `TimeUtil.formatDateTime`：`SimpleDateFormat("yyyy-MM-dd HH:mm:ss")`（系统时区）。
fn format_date_time(now_ms: i64) -> String {
    format_local(now_ms, "%Y-%m-%d %H:%M:%S")
}

fn format_local(now_ms: i64, pattern: &str) -> String {
    chrono::DateTime::from_timestamp_millis(now_ms)
        .map(|utc| utc.with_timezone(&chrono::Local).format(pattern).to_string())
        .unwrap_or_default()
}

/// 对齐 `WebhookPushProvider.encodeHeaderValue`（RFC 8187）：
/// 值全为 ASCII（≤ 0x7E）时原样返回；否则前缀 `UTF-8''`，把 > 0x7E 的字节写成大写 `%XX`。
fn encode_header_value(value: &str) -> String {
    if value.trim().is_empty() {
        return value.to_string();
    }
    if !value.chars().any(|c| c as u32 > 0x7E) {
        return value.to_string();
    }
    let mut out = String::from("UTF-8''");
    for byte in value.as_bytes() {
        if *byte > 0x7E {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        } else {
            out.push(*byte as char);
        }
    }
    out
}

/// 对齐 `URLEncoder.encode(value, UTF_8).replace("+", "%20")`：
/// 仅 `A-Za-z0-9.-*_` 原样输出，空格写成 `%20`，其余字节写成大写 `%XX`。
fn url_encode_component(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'*' | b'_' => {
                out.push(*byte as char)
            }
            b' ' => out.push_str("%20"),
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// smtp
// ---------------------------------------------------------------------------

/// 对齐 `SmtpPushProvider.Config`。
#[derive(Clone, Debug)]
pub struct SmtpSettings {
    pub host: String,
    pub port: u16,
    pub auth: bool,
    pub username: String,
    pub password: String,
    pub sender: String,
    pub sender_name: String,
    pub receivers: Vec<String>,
    pub encryption: String,
    /// 对齐 `mail.smtp.sendpartial`：仅持久化、不参与发送（lettre 无等价开关，
    /// JavaMail 也只在该属性下处理「多收件人部分失败」，见 [`smtp_send_mail`] 注释）
    #[allow(dead_code)]
    pub send_partial: bool,
}

/// 对齐 `SmtpPushProvider.Encryption`（`Encryption.valueOf` 大小写敏感）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmtpEncryption {
    None,
    StartTls,
    EnforceStartTls,
    SslTls,
}

/// 对齐 `Encryption.valueOf(config.getEncryption())`：仅精确匹配枚举名，其余返回 `None`
/// （上游此时 `log.error("Unable to load mail encryption type: ...")` 并**不启用任何 TLS**）。
fn parse_smtp_encryption(value: &str) -> Option<SmtpEncryption> {
    match value {
        "NONE" => Some(SmtpEncryption::None),
        "STARTTLS" => Some(SmtpEncryption::StartTls),
        "ENFORCE_STARTTLS" => Some(SmtpEncryption::EnforceStartTls),
        "SSLTLS" => Some(SmtpEncryption::SslTls),
        _ => None,
    }
}

/// 对齐 `SmtpPushProvider.push`：发送失败记录 warn 并返回 `false`（不向管理器抛出）。
///
/// 与上游的两点差异（不影响 `PushManager` 的 OR 结果）：
/// 1. 上游只捕获 `MessagingException`，其它异常会冒泡给 `PushManagerImpl`；
///    lettre 的错误类型无法区分，本移植把所有发送失败都规约为 `Ok(false)` + warn；
/// 2. 上游在告警线程内同步阻塞发送；本移植用 `spawn_blocking` 承载阻塞式 SMTP，
///    对调用方仍表现为「await 完成一次发送」。
async fn smtp_push(settings: &SmtpSettings, title: &str, content: &str) -> anyhow::Result<bool> {
    let settings = settings.clone();
    let title = title.to_string();
    let content = content.to_string();
    let result =
        tokio::task::spawn_blocking(move || smtp_send_mail(&settings, &title, &content)).await;
    match result {
        Ok(Ok(())) => Ok(true),
        Ok(Err(e)) => {
            warn!("Unable to push message via SMTP: {e}");
            Ok(false)
        }
        Err(e) => {
            warn!("Unable to push message via SMTP: {e}");
            Ok(false)
        }
    }
}

/// 对齐 `SmtpPushProvider.sendMail`：每次发送都新建 Session/Transport，设置发件人
/// （显示名取配置的 `name`）、收件人（非法地址 warn 后跳过）、主题（UTF-8）与
/// `text/html; charset=UTF-8` 正文（正文是 markdown 渲染后的 HTML）。
///
/// 与上游的差异：
/// - 上游返回 JavaMail 的 `MimeMessage.getMessageID()`；lettre 的 `Response` 不含 Message-ID，
///   本移植返回 `()`（调用方本就不使用该值）；
/// - `mail.smtp.sendpartial` 无 lettre 等价开关（JavaMail 仅在多收件人部分失败时生效）。
fn smtp_send_mail(settings: &SmtpSettings, subject: &str, text: &str) -> anyhow::Result<()> {
    // 对齐上游 `mail.smtp.*` 属性：host/port 恒取配置值（`getInt("port")` 缺省为 0）
    let mut builder = SmtpTransport::builder_dangerous(settings.host.as_str()).port(settings.port);
    match parse_smtp_encryption(&settings.encryption) {
        Some(SmtpEncryption::None) => {}
        Some(SmtpEncryption::StartTls) => {
            builder = builder.tls(Tls::Opportunistic(smtp_tls_parameters(&settings.host)?));
        }
        Some(SmtpEncryption::EnforceStartTls) => {
            builder = builder.tls(Tls::Required(smtp_tls_parameters(&settings.host)?));
        }
        Some(SmtpEncryption::SslTls) => {
            builder = builder.tls(Tls::Wrapper(smtp_tls_parameters(&settings.host)?));
        }
        None => {
            // 对齐上游 `log.error("Unable to load mail encryption type: {}, it's valid?")`
            // 之后继续发送（不启用任何 TLS）
            error!(
                "Unable to load mail encryption type: {}, it's valid?",
                settings.encryption
            );
        }
    }
    if settings.auth {
        builder = builder.credentials(Credentials::new(
            settings.username.clone(),
            settings.password.clone(),
        ));
    }
    let transport = builder.build();

    let message = smtp_build_message(settings, subject, text)?;
    transport
        .send(&message)
        .map_err(|e| anyhow::anyhow!("unable to send mail: {e}"))?;
    Ok(())
}

/// 组装邮件（对齐 `SmtpPushProvider.sendMail` 中 MimeMessage 部分的四步：
/// 发件人、收件人、主题、HTML 正文）。
fn smtp_build_message(
    settings: &SmtpSettings,
    subject: &str,
    text: &str,
) -> anyhow::Result<Message> {
    let mut from = Mailbox::from_str(&settings.sender)
        .map_err(|e| anyhow::anyhow!("invalid sender address [{}]: {e}", settings.sender))?;
    // 对齐 `new InternetAddress(sender, senderName, "UTF-8")`：显示名取配置的 `name`
    from.name = Some(settings.sender_name.clone());

    let mut recipients = Vec::new();
    for raw in &settings.receivers {
        // 对齐上游逐个解析、非法地址 warn 后跳过
        match Mailbox::from_str(raw) {
            Ok(mailbox) => recipients.push(mailbox),
            Err(e) => warn!("The email address [{raw}] is invalid: {e}"),
        }
    }
    if recipients.is_empty() {
        // 对齐 JavaMail 在无有效收件人时抛出的 SendFailedException("No recipient addresses")
        anyhow::bail!("No recipient addresses");
    }

    let mut message_builder = Message::builder().from(from);
    for recipient in recipients {
        message_builder = message_builder.to(recipient);
    }
    message_builder
        .subject(subject)
        .singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .body(markdown_to_html(text)),
        )
        .map_err(|e| anyhow::anyhow!("unable to build mail message: {e}"))
}

fn smtp_tls_parameters(host: &str) -> anyhow::Result<TlsParameters> {
    TlsParameters::new(host.to_string())
        .map_err(|e| anyhow::anyhow!("invalid TLS parameters: {e}"))
}

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

    /// 对齐 `AlertManager.identifierAlertExists`（存在且**未读**）。
    pub fn identifier_alert_exists(&self, identifier: &str) -> bool {
        self.alerts
            .lock()
            .map(|alerts| {
                alerts
                    .get(identifier)
                    .is_some_and(|record| !record.read)
            })
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

#[cfg(test)]
mod tests {
    use super::*;
    use pbh_core::i18n::Param;
    use pbh_downloader::http::BoxFuture;

    /// 记录请求、返回固定响应的 mock fetcher。
    struct MockFetcher {
        status: u16,
        body: String,
        requests: Mutex<Vec<HttpRequest>>,
    }

    impl MockFetcher {
        fn new(status: u16, body: &str) -> Arc<Self> {
            Arc::new(Self {
                status,
                body: body.to_string(),
                requests: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<HttpRequest> {
            self.requests.lock().unwrap().clone()
        }

        fn only_request(&self) -> HttpRequest {
            let requests = self.requests();
            assert_eq!(requests.len(), 1, "应当恰好发出一个请求");
            requests[0].clone()
        }
    }

    impl HttpFetcher for MockFetcher {
        fn execute<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, anyhow::Result<HttpResponse>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(req);
                Ok(HttpResponse::new(self.status, self.body.clone()))
            })
        }
    }

    /// 由 YAML 片段构造管理器（键 = 渠道名）。
    fn manager(yaml: &str, fetcher: Arc<MockFetcher>) -> PushManager {
        let section: PushSection = serde_yaml::from_str(yaml).expect("测试用 push 配置应可解析");
        PushManager::from_config(&section, fetcher)
    }

    fn only_provider(manager: &PushManager) -> &PushProvider {
        assert_eq!(manager.provider_list().len(), 1, "应当恰好注册一个渠道");
        &manager.provider_list()[0]
    }

    /// 断言 JSON 请求体：比较解析后的 `Value`（JSON 对象键顺序无语义；
    /// 上游用 HashMap + Gson，键顺序本就不确定）。
    fn assert_json_body(req: &HttpRequest, expected: &str) {
        let actual: JsonValue = serde_json::from_str(req.body.as_deref().expect("请求体不应为空"))
            .expect("请求体必须是合法 JSON");
        let expected: JsonValue = serde_json::from_str(expected).expect("期望值必须是合法 JSON");
        assert_eq!(actual, expected);
    }

    // ---------------- 空配置 ----------------

    #[tokio::test]
    async fn empty_provider_list_is_noop() {
        let fetcher = MockFetcher::new(200, "{}");
        let section = PushSection::default();
        let manager = PushManager::from_config(&section, fetcher.clone());
        assert!(manager.provider_list().is_empty());
        assert!(!manager.push_message("t", "c", AlertLevel::Warn).await);
        assert!(fetcher.requests().is_empty(), "空配置不应产生任何请求");
    }

    // ---------------- pushplus ----------------

    #[tokio::test]
    async fn pushplus_builds_exact_request_and_checks_code() {
        let fetcher = MockFetcher::new(200, r#"{"code":200,"msg":"请求成功"}"#);
        let m = manager(
            "my-plus:\n  type: pushplus\n  token: \"tk\"\n  topic: \"tp\"\n  channel: \"wechat\"\n",
            fetcher.clone(),
        );
        let provider = only_provider(&m);
        assert_eq!(provider.name(), "my-plus");
        assert_eq!(provider.config_type(), "pushplus");
        assert!(provider.push("标题", "正文", AlertLevel::Warn).await.unwrap());

        let req = fetcher.only_request();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://www.pushplus.plus/send");
        assert_eq!(req.header("Content-Type"), Some("application/json"));
        assert_json_body(
            &req,
            r#"{"token":"tk","topic":"tp","channel":"wechat","title":"标题","content":"正文","template":"markdown"}"#,
        );
    }

    #[tokio::test]
    async fn pushplus_omits_blank_optional_fields() {
        let fetcher = MockFetcher::new(200, r#"{"code":200}"#);
        let m = manager("p:\n  type: pushplus\n  token: \"tk\"\n", fetcher.clone());
        assert!(only_provider(&m)
            .push("t", "c", AlertLevel::Info)
            .await
            .unwrap());
        // 空白的 topic/channel 整键不出现（对齐上游 `isBlank() → null` + 条件 put）
        assert_json_body(
            &fetcher.only_request(),
            r#"{"token":"tk","title":"t","content":"c","template":"markdown"}"#,
        );
    }

    #[tokio::test]
    async fn pushplus_body_code_is_success_criterion() {
        // HTTP 200 但业务 code != 200 ⇒ 失败（对齐上游 IllegalStateException）
        let fetcher = MockFetcher::new(200, r#"{"code":500,"msg":"token 错误"}"#);
        let m = manager("p:\n  type: pushplus\n  token: \"tk\"\n", fetcher);
        let err = only_provider(&m)
            .push("t", "c", AlertLevel::Info)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("token 错误"), "{err}");

        // HTTP 非 2xx ⇒ 失败
        let fetcher = MockFetcher::new(500, "boom");
        let m = manager("p:\n  type: pushplus\n  token: \"tk\"\n", fetcher);
        assert!(only_provider(&m)
            .push("t", "c", AlertLevel::Info)
            .await
            .is_err());
    }

    // ---------------- serverchan ----------------

    #[tokio::test]
    async fn serverchan_builds_exact_request() {
        let fetcher = MockFetcher::new(200, r#"{"code":0}"#);
        let m = manager(
            "sc:\n  type: serverchan\n  sendkey: \"SCT123\"\n  channel: \"9\"\n  openid: \"oid\"\n",
            fetcher.clone(),
        );
        assert!(only_provider(&m)
            .push("标题", "正文", AlertLevel::Warn)
            .await
            .unwrap());
        let req = fetcher.only_request();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://sctapi.ftqq.com/SCT123.send");
        assert_eq!(req.header("Content-Type"), Some("application/json"));
        assert_json_body(
            &req,
            r#"{"title":"标题","desp":"正文","text":"标题","channel":"9","openid":"oid"}"#,
        );
    }

    #[tokio::test]
    async fn serverchan_omits_blank_channel_and_openid_and_reports_message() {
        let fetcher = MockFetcher::new(200, r#"{"code":0}"#);
        let m = manager("sc:\n  type: serverchan\n  sendkey: \"SCT\"\n", fetcher.clone());
        assert!(only_provider(&m)
            .push("t", "c", AlertLevel::Info)
            .await
            .unwrap());
        // 空白的 channel/openid 整键不出现
        assert_json_body(&fetcher.only_request(), r#"{"title":"t","desp":"c","text":"t"}"#);

        // 非 2xx：错误信息取响应体 message
        let fetcher = MockFetcher::new(400, r#"{"message":"bad sendkey"}"#);
        let m = manager("sc:\n  type: serverchan\n  sendkey: \"SCT\"\n", fetcher);
        let err = only_provider(&m)
            .push("t", "c", AlertLevel::Info)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("bad sendkey"), "{err}");
    }

    // ---------------- telegram ----------------

    #[tokio::test]
    async fn telegram_builds_exact_request() {
        let fetcher = MockFetcher::new(200, r#"{"ok":true}"#);
        let m = manager(
            "tg:\n  type: telegram\n  token: \"123:abc\"\n  chatid: \"-100\"\n",
            fetcher.clone(),
        );
        assert!(only_provider(&m)
            .push("标题", "正文", AlertLevel::Error)
            .await
            .unwrap());
        let req = fetcher.only_request();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://api.telegram.org/bot123:abc/sendPhoto");
        assert_eq!(req.header("Content-Type"), Some("application/json"));
        let expected = format!(
            "{{\"chat_id\":\"-100\",\"caption\":\"*标题*\\n正文\",\"text\":\"*标题*\\n正文\",\"photo\":\"{ICON_URL}\",\"parse_mode\":\"Markdown\"}}"
        );
        assert_json_body(&req, &expected);
    }

    #[tokio::test]
    async fn telegram_reports_description_on_failure() {
        let fetcher =
            MockFetcher::new(400, r#"{"ok":false,"error_code":400,"description":"chat not found"}"#);
        let m = manager("tg:\n  type: telegram\n  token: \"t\"\n  chatid: \"1\"\n", fetcher);
        let err = only_provider(&m)
            .push("t", "c", AlertLevel::Info)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("chat not found"), "{err}");
    }

    // ---------------- bark ----------------

    #[tokio::test]
    async fn bark_builds_exact_request() {
        let fetcher = MockFetcher::new(200, r#"{"code":200}"#);
        let m = manager(
            "b:\n  type: bark\n  backend_url: \"https://bark.example/push\"\n  device_key: \"dk\"\n  message_group: \"PBH\"\n",
            fetcher.clone(),
        );
        assert!(only_provider(&m)
            .push("标题", "正文", AlertLevel::Warn)
            .await
            .unwrap());
        let req = fetcher.only_request();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://bark.example/push");
        assert_eq!(req.header("Content-Type"), Some("application/json"));
        let expected = format!(
            "{{\"title\":\"标题\",\"body\":\"正文\",\"device_key\":\"dk\",\"group\":\"PBH\",\"icon\":\"{ICON_URL}\"}}"
        );
        assert_json_body(&req, &expected);
    }

    #[tokio::test]
    async fn bark_defaults_backend_url_to_official_api() {
        let fetcher = MockFetcher::new(200, "{}");
        let m = manager("b:\n  type: bark\n  device_key: \"dk\"\n", fetcher.clone());
        assert!(only_provider(&m)
            .push("t", "c", AlertLevel::Info)
            .await
            .unwrap());
        let req = fetcher.only_request();
        assert_eq!(req.url, "https://api.day.app/push");
        // 空 group 照发（上游不对 message_group 做空值归一化）
        assert_json_body(
            &req,
            &format!(
                "{{\"title\":\"t\",\"body\":\"c\",\"device_key\":\"dk\",\"group\":\"\",\"icon\":\"{ICON_URL}\"}}"
            ),
        );
    }

    // ---------------- pushdeer ----------------

    #[tokio::test]
    async fn pushdeer_builds_exact_request() {
        let fetcher = MockFetcher::new(200, r#"{"code":0}"#);
        let m = manager(
            "pd:\n  type: pushdeer\n  endpoint: \"https://api2.pushdeer.com/message/push\"\n  pushkey: \"pk\"\n",
            fetcher.clone(),
        );
        assert!(only_provider(&m)
            .push("标题", "正文", AlertLevel::Info)
            .await
            .unwrap());
        let req = fetcher.only_request();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://api2.pushdeer.com/message/push");
        assert_eq!(req.header("Content-Type"), Some("application/json"));
        assert_json_body(
            &req,
            r#"{"pushkey":"pk","text":"标题","desp":"正文","type":"markdown"}"#,
        );
    }

    // ---------------- gotify ----------------

    #[tokio::test]
    async fn gotify_posts_form_body() {
        let fetcher = MockFetcher::new(200, "{}");
        let m = manager(
            "g:\n  type: gotify\n  endpoint: \"https://gotify.example/message?token=app\"\n  priority: 8\n",
            fetcher.clone(),
        );
        assert!(only_provider(&m)
            .push("标题", "正文", AlertLevel::Warn)
            .await
            .unwrap());
        let req = fetcher.only_request();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://gotify.example/message?token=app");
        assert_eq!(
            req.form,
            Some(vec![
                ("title".to_string(), "标题".to_string()),
                ("message".to_string(), "正文".to_string()),
                ("priority".to_string(), "8".to_string()),
            ])
        );
        assert!(req.body.is_none());
    }

    #[tokio::test]
    async fn gotify_priority_defaults_to_five() {
        let fetcher = MockFetcher::new(200, "{}");
        let m = manager("g:\n  type: gotify\n", fetcher.clone());
        assert!(only_provider(&m)
            .push("t", "c", AlertLevel::Info)
            .await
            .unwrap());
        assert_eq!(
            fetcher.only_request().form,
            Some(vec![
                ("title".to_string(), "t".to_string()),
                ("message".to_string(), "c".to_string()),
                ("priority".to_string(), "5".to_string()),
            ])
        );
    }

    // ---------------- ntfy ----------------

    #[tokio::test]
    async fn ntfy_builds_exact_request_with_headers() {
        let fetcher = MockFetcher::new(200, r#"{"id":"x"}"#);
        let m = manager(
            "n:\n  type: ntfy\n  server_url: \"https://ntfy.sh\"\n  topic: \"mytopic\"\n  token: \"tk_test\"\n  priority: 4\n  tags: \"warning,pbh\"\n",
            fetcher.clone(),
        );
        assert!(only_provider(&m)
            .push("标题", "正文", AlertLevel::Warn)
            .await
            .unwrap());
        let req = fetcher.only_request();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://ntfy.sh/mytopic");
        assert_eq!(req.body.as_deref(), Some("正文"));
        assert_eq!(req.header("Content-Type"), Some("text/plain; charset=utf-8"));
        // Title 为 RFC 2047 编码字（base64(UTF-8)）
        let expected_title = format!(
            "=?UTF-8?B?{}?=",
            base64::engine::general_purpose::STANDARD.encode("标题")
        );
        assert_eq!(req.header("Title"), Some(expected_title.as_str()));
        assert_eq!(req.header("Priority"), Some("4"));
        assert_eq!(req.header("Icon"), Some(ICON_URL));
        assert_eq!(req.header("Authorization"), Some("Bearer tk_test"));
        assert_eq!(req.header("Tags"), Some("warning,pbh"));
    }

    #[tokio::test]
    async fn ntfy_skips_blank_token_and_tags() {
        let fetcher = MockFetcher::new(200, "ok");
        let m = manager("n:\n  type: ntfy\n  topic: \"t\"\n", fetcher.clone());
        assert!(only_provider(&m)
            .push("t", "c", AlertLevel::Info)
            .await
            .unwrap());
        let req = fetcher.only_request();
        assert_eq!(req.url, "https://ntfy.sh/t");
        assert_eq!(req.header("Authorization"), None);
        assert_eq!(req.header("Tags"), None);
        assert_eq!(req.header("Priority"), Some("3"));
    }

    // ---------------- webhook ----------------

    /// 固定时间戳：用于 `{date}`/`{time}`/`{datetime}` 的确定性断言。
    const FIXED_NOW_MS: i64 = 1_789_862_400_000;

    #[tokio::test]
    async fn webhook_post_renders_default_json_template() {
        let fetcher = MockFetcher::new(200, "ok");
        let m = manager(
            "w:\n  type: webhook\n  url: \"https://hook.example/push?ch={channelName}\"\n",
            fetcher.clone(),
        );
        assert!(only_provider(&m)
            .push_at("标题", "正文", AlertLevel::Error, FIXED_NOW_MS)
            .await
            .unwrap());
        let req = fetcher.only_request();
        assert_eq!(req.method, "POST");
        assert_eq!(req.url, "https://hook.example/push?ch=w");
        assert_eq!(req.header("Content-Type"), Some("application/json"));
        let expected = format!(
            "{{\n    \"title\":\"标题\",\n    \"content\":\"正文\",\n    \"level\":\"ERROR\",\n    \"date\":\"{}\",\n    \"time\":\"{}\",\n    \"datetime\":\"{}\",\n    \"channelName\":\"w\"\n}}\n",
            format_date_only(FIXED_NOW_MS),
            format_time_only(FIXED_NOW_MS),
            format_date_time(FIXED_NOW_MS),
        );
        assert_eq!(req.body.as_deref(), Some(expected.as_str()));
    }

    #[tokio::test]
    async fn webhook_get_has_no_body_and_custom_headers() {
        let fetcher = MockFetcher::new(200, "ok");
        let m = manager(
            "w:\n  type: webhook\n  url: \"https://hook.example/notify?title={title}\"\n  method: \"get\"\n  body-template: \"ignored\"\n  headers:\n    X-Token: \"abc\"\n",
            fetcher.clone(),
        );
        assert!(only_provider(&m)
            .push_at("中文 标题", "正文", AlertLevel::Info, FIXED_NOW_MS)
            .await
            .unwrap());
        let req = fetcher.only_request();
        assert_eq!(req.method, "GET", "方法名大写归一（对齐 toUpperCase）");
        // URL 场景按 URLEncoder 规则编码（空格 ⇒ %20）
        assert_eq!(
            req.url,
            "https://hook.example/notify?title=%E4%B8%AD%E6%96%87%20%E6%A0%87%E9%A2%98"
        );
        assert!(req.body.is_none());
        assert_eq!(req.header("Content-Type"), None);
        assert_eq!(req.header("X-Token"), Some("abc"));
    }

    #[tokio::test]
    async fn webhook_encodes_non_ascii_header_values() {
        let fetcher = MockFetcher::new(200, "ok");
        let m = manager(
            "w:\n  type: webhook\n  url: \"https://hook.example/\"\n  headers:\n    X-Note: \"中文\"\n",
            fetcher.clone(),
        );
        assert!(only_provider(&m)
            .push_at("t", "c", AlertLevel::Info, FIXED_NOW_MS)
            .await
            .unwrap());
        assert_eq!(
            fetcher.only_request().header("X-Note"),
            Some("UTF-8''%E4%B8%AD%E6%96%87")
        );
    }

    #[tokio::test]
    async fn webhook_skips_null_values_and_blank_header_names() {
        let fetcher = MockFetcher::new(200, "ok");
        let m = manager(
            "w:\n  type: webhook\n  url: \"https://hook.example/\"\n  headers:\n    X-Null: null\n    X-Empty: \"\"\n    \"\": \"blank-key\"\n",
            fetcher.clone(),
        );
        assert!(only_provider(&m)
            .push_at("t", "c", AlertLevel::Info, FIXED_NOW_MS)
            .await
            .unwrap());
        let req = fetcher.only_request();
        // null 值被忽略（对齐 `headerValue != null`）；空串仍会发送
        assert_eq!(req.header("X-Null"), None);
        assert_eq!(req.header("X-Empty"), Some(""));
        // 空白的头名被忽略（对齐 `!headerKey.isBlank()`）
        assert!(
            req.headers.iter().all(|(name, _)| !name.trim().is_empty()),
            "空白的头名不应出现在请求里: {:?}",
            req.headers
        );
    }

    #[tokio::test]
    async fn webhook_rejects_unsupported_method_and_blank_url() {
        let fetcher = MockFetcher::new(200, "ok");
        let m = manager(
            "w:\n  type: webhook\n  url: \"https://hook.example/\"\n  method: \"PUT\"\n",
            fetcher.clone(),
        );
        let err = only_provider(&m)
            .push_at("t", "c", AlertLevel::Info, FIXED_NOW_MS)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Unsupported webhook method"),
            "{err}"
        );

        let fetcher = MockFetcher::new(200, "ok");
        let m = manager("w:\n  type: webhook\n", fetcher);
        let err = only_provider(&m)
            .push_at("t", "c", AlertLevel::Info, FIXED_NOW_MS)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Webhook URL cannot be empty"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn webhook_failure_reports_status_body() {
        let fetcher = MockFetcher::new(502, "gateway down");
        let m = manager("w:\n  type: webhook\n  url: \"https://hook.example/\"\n", fetcher);
        let err = only_provider(&m)
            .push_at("t", "c", AlertLevel::Info, FIXED_NOW_MS)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("gateway down"), "{err}");
    }

    // ---------------- smtp ----------------

    #[test]
    fn smtp_encryption_names_match_upstream_enum() {
        assert_eq!(parse_smtp_encryption("SSLTLS"), Some(SmtpEncryption::SslTls));
        assert_eq!(
            parse_smtp_encryption("STARTTLS"),
            Some(SmtpEncryption::StartTls)
        );
        assert_eq!(
            parse_smtp_encryption("ENFORCE_STARTTLS"),
            Some(SmtpEncryption::EnforceStartTls)
        );
        assert_eq!(parse_smtp_encryption("NONE"), Some(SmtpEncryption::None));
        // 上游 Enum.valueOf 大小写敏感：小写写法会被记为「无法识别的加密类型」
        assert_eq!(parse_smtp_encryption("starttls"), None);
        assert_eq!(parse_smtp_encryption(""), None);
    }

    #[test]
    fn smtp_message_parts_match_upstream() {
        let settings = SmtpSettings {
            host: "smtp.example.com".into(),
            port: 465,
            auth: true,
            username: "u".into(),
            password: "p".into(),
            sender: "pbh@example.com".into(),
            sender_name: "PeerBanHelper".into(),
            receivers: vec!["admin@example.com".into(), "not-an-address".into()],
            encryption: "SSLTLS".into(),
            send_partial: true,
        };

        // 非法收件人在加载期即被识别（上游 warn 后 filter 掉）
        assert!(Mailbox::from_str("not-an-address").is_err());

        // 正文为 markdown 渲染出的 HTML（对齐 commonmark HtmlRenderer 的等价输出）
        assert_eq!(
            markdown_to_html("[PeerBanHelper/WARN] 标题: 正文"),
            "<p>[PeerBanHelper/WARN] 标题: 正文</p>\n"
        );

        // 组装后的 MIME：发件人显示名取配置的 name、非法收件人被跳过、正文为 HTML
        let raw = String::from_utf8_lossy(
            &smtp_build_message(&settings, "主题", "**bold**")
                .unwrap()
                .formatted(),
        )
        .to_string();
        assert!(
            raw.contains("From: PeerBanHelper <pbh@example.com>"),
            "{raw}"
        );
        assert!(raw.contains("To: admin@example.com"), "{raw}");
        assert!(!raw.contains("not-an-address"), "非法收件人不应出现在邮件里");
        assert!(
            raw.contains("Content-Type: text/html; charset=utf-8"),
            "{raw}"
        );
        assert!(raw.contains("<strong>bold</strong>"), "{raw}");
        // 非 ASCII 主题走 RFC 2047 编码字（对齐 JavaMail setSubject(subject, "UTF-8")）
        assert!(raw.contains("Subject: =?utf-8?b?"), "{raw}");

        // 无有效收件人 ⇒ 直接报错（对齐 JavaMail 的 No recipient addresses）
        let mut no_receiver = settings.clone();
        no_receiver.receivers = vec!["bad".into()];
        assert!(smtp_build_message(&no_receiver, "s", "t").is_err());
    }

    #[tokio::test]
    async fn smtp_provider_loads_from_config_and_failure_is_non_fatal() {
        let fetcher = MockFetcher::new(200, "{}");
        let m = manager(
            "mail:\n  type: smtp\n  host: \"smtp.example.com\"\n  port: 465\n  auth: true\n  username: \"u\"\n  password: \"p\"\n  sender: \"pbh@example.com\"\n  receiver:\n    - \"not-an-address\"\n",
            fetcher,
        );
        let provider = only_provider(&m);
        assert_eq!(provider.config_type(), "smtp");
        assert_eq!(provider.name(), "mail");
        match provider {
            PushProvider::Smtp { settings, .. } => {
                assert_eq!(settings.host, "smtp.example.com");
                assert_eq!(settings.port, 465);
                assert!(settings.auth);
                // 上游 `loadFromYaml` 的默认值
                assert_eq!(settings.sender_name, "PeerBanHelper");
                assert_eq!(settings.encryption, "SSLTLS");
                assert!(settings.send_partial);
            }
            other => panic!("应为 smtp 渠道，实际为 {}", other.config_type()),
        }
        // 收件人全部非法 ⇒ 在建立连接前就失败；`push` 返回 Ok(false)（不 panic、不中断 wave）
        assert!(!provider.push("t", "c", AlertLevel::Info).await.unwrap());
    }

    // ---------------- 管理器语义 ----------------

    #[tokio::test]
    async fn unknown_provider_is_skipped_and_type_is_case_insensitive() {
        let fetcher = MockFetcher::new(200, r#"{"code":200}"#);
        let m = manager(
            "bad:\n  type: no-such-provider\n  token: \"x\"\nplus:\n  type: PuShPlUs\n  token: \"tk\"\n",
            fetcher.clone(),
        );
        // 未识别的 type 只记录日志并跳过（其余渠道照常注册）
        assert_eq!(m.provider_list().len(), 1);
        assert!(m.push_message("t", "c", AlertLevel::Warn).await);
        assert_eq!(fetcher.requests().len(), 1);
    }

    #[tokio::test]
    async fn push_message_is_or_over_providers_in_config_order() {
        let fetcher = MockFetcher::new(200, "{}");
        let m = manager(
            "a:\n  type: bark\n  device_key: \"k\"\nb:\n  type: pushdeer\n  pushkey: \"k\"\n",
            fetcher.clone(),
        );
        assert_eq!(m.provider_list().len(), 2, "保持配置书写顺序");
        assert_eq!(m.provider_list()[0].config_type(), "bark");
        assert_eq!(m.provider_list()[1].config_type(), "pushdeer");
        assert!(m.push_message("t", "c", AlertLevel::Warn).await);
        assert_eq!(fetcher.requests().len(), 2);
    }

    #[tokio::test]
    async fn push_message_reports_false_when_all_providers_fail() {
        let fetcher = MockFetcher::new(500, "boom");
        let m = manager("a:\n  type: bark\n  device_key: \"k\"\n", fetcher.clone());
        assert!(!m.push_message("t", "c", AlertLevel::Warn).await);
        assert_eq!(fetcher.requests().len(), 1, "失败渠道仍应尝试过");
    }

    #[tokio::test]
    async fn failed_provider_does_not_block_other_providers() {
        let fetcher = MockFetcher::new(500, "boom");
        let m = manager(
            "a:\n  type: bark\n  device_key: \"k\"\nb:\n  type: pushdeer\n  pushkey: \"k\"\n",
            fetcher.clone(),
        );
        assert!(!m.push_message("t", "c", AlertLevel::Warn).await);
        assert_eq!(fetcher.requests().len(), 2, "前一个渠道失败后仍应尝试后一个");
    }

    #[tokio::test]
    async fn provider_name_dots_are_replaced() {
        let fetcher = MockFetcher::new(200, "{}");
        let m = manager("a.b:\n  type: bark\n  device_key: \"k\"\n", fetcher);
        assert_eq!(only_provider(&m).name(), "a-b");
    }

    // ---------------- 告警管理器 ----------------

    fn translator() -> Arc<Translator> {
        Arc::new(Translator::embedded())
    }

    fn nat_alert_components() -> (TranslationComponent, TranslationComponent) {
        (
            TranslationComponent::new("DOWNLOADER_DOCKER_INCORRECT_NETWORK_DETECTED_TITLE"),
            TranslationComponent::with_params(
                "DOWNLOADER_DOCKER_INCORRECT_NETWORK_DETECTED_DESCRIPTION",
                vec![
                    Param::Text("qBittorrent".into()),
                    Param::Text("192.168.0.1".into()),
                ],
            ),
        )
    }

    #[tokio::test]
    async fn alert_title_prefix_and_identifier_dedup() {
        let fetcher = MockFetcher::new(200, "{}");
        let m = Arc::new(manager("a:\n  type: bark\n  device_key: \"k\"\n", fetcher.clone()));
        let alerts = AlertManager::new(m, translator(), "zh_cn");
        let identifier = "downloader-nat-setup-error@qBittorrent";
        let (title, content) = nat_alert_components();

        alerts
            .publish_alert(true, AlertLevel::Error, identifier, &title, &content)
            .await;
        let body = fetcher.only_request().body.unwrap();
        // 标题前缀 "[PeerBanHelper/<LEVEL>] " + tlUI(title)，正文为 tlUI(content)
        assert!(body.contains("[PeerBanHelper/ERROR]"), "{body}");
        assert!(body.contains("严重错误"), "标题应渲染文案：{body}");
        assert!(body.contains("qBittorrent"), "正文应渲染参数：{body}");
        assert!(alerts.identifier_alert_exists(identifier));
        assert_eq!(
            alerts.get_highest_unread_alert_level(),
            Some(AlertLevel::Error)
        );

        // 相同 identifier 的未读告警不重复发布
        alerts
            .publish_alert(true, AlertLevel::Error, identifier, &title, &content)
            .await;
        assert_eq!(fetcher.requests().len(), 1);

        // 已读后 identifierAlertExists=false，但 IncludeRead 仍为 true
        alerts.mark_alert_as_read(identifier);
        assert!(!alerts.identifier_alert_exists(identifier));
        assert!(alerts.identifier_alert_exists_include_read(identifier));
        assert_eq!(alerts.get_highest_unread_alert_level(), None);

        // push=false ⇒ 只记录告警、不推送
        alerts
            .publish_alert(false, AlertLevel::Info, "another", &title, &content)
            .await;
        assert_eq!(fetcher.requests().len(), 1);
        assert_eq!(
            alerts.get_highest_unread_alert_level(),
            Some(AlertLevel::Info)
        );
    }

    #[tokio::test]
    async fn alert_manager_without_providers_is_silent() {
        let fetcher = MockFetcher::new(200, "{}");
        let m = Arc::new(manager("{}", fetcher.clone()));
        let alerts = AlertManager::new(m, translator(), "zh_cn");
        let (title, content) = nat_alert_components();
        alerts
            .publish_alert(true, AlertLevel::Error, "id", &title, &content)
            .await;
        assert!(fetcher.requests().is_empty(), "无渠道时不应产生流量");
        assert!(alerts.identifier_alert_exists("id"), "告警仍需被记录");
    }

    #[test]
    fn alert_level_names_and_ntfy_priority_match_upstream() {
        assert_eq!(AlertLevel::Tip.name(), "TIP");
        assert_eq!(AlertLevel::Info.name(), "INFO");
        assert_eq!(AlertLevel::Warn.name(), "WARN");
        assert_eq!(AlertLevel::Error.name(), "ERROR");
        assert_eq!(AlertLevel::Fatal.name(), "FATAL");
        assert_eq!(AlertLevel::Tip.ntfy_priority(), 1);
        assert_eq!(AlertLevel::Info.ntfy_priority(), 2);
        assert_eq!(AlertLevel::Warn.ntfy_priority(), 3);
        assert_eq!(AlertLevel::Error.ntfy_priority(), 4);
        assert_eq!(AlertLevel::Fatal.ntfy_priority(), 5);
        // 顺序即 ordinal（getHighestUnreadAlertLevel 依赖）
        assert!(AlertLevel::Tip < AlertLevel::Info);
        assert!(AlertLevel::Warn < AlertLevel::Error);
        assert!(AlertLevel::Error < AlertLevel::Fatal);
    }
}
