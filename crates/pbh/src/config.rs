//! 应用配置模型与加载/生成。
//!
//! 字段对齐上游：`config.yml`（server/database/persist/downloaders）+
//! `profile.yml`（`check-interval` / `ban-duration` / `ignore-peers-from-addresses` / `module.*`）。
//! 本阶段把两者合并到同一个 `config.yml` 的 `profile:` 段，便于单文件部署。

use pbh_core::btn_transport::BtnNetworkConfig;
use pbh_core::config::{
    IpDatabaseConfig, LookupConfig, PerformanceConfig, ProfileConfig, ProxyConfig,
    ResolversConfig,
};
use pbh_core::remap::{BanlistRemapping, IpRemapConfig, RemapConfig};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_server")]
    pub server: ServerConfig,
    #[serde(default = "default_database")]
    pub database: DatabaseConfig,
    #[serde(default = "default_persist")]
    pub persist: PersistConfig,
    #[serde(default = "default_profile")]
    pub profile: ProfileConfig,
    /// `banlist-remapping:` 段（封禁列表 CIDR 重映射）
    #[serde(rename = "banlist-remapping", default)]
    pub banlist_remapping: BanlistRemapping,
    /// `ip-remapping:` 段（Teredo / NAT64 地址翻译）
    #[serde(rename = "ip-remapping", default)]
    pub ip_remapping: IpRemapConfig,
    /// `ip-database:` 段（GeoIP/GeoCN 数据库，对齐上游 `config.yml` 的 `ip-database`
    /// 与 `IPDBManager#setupIPDB`；数据库文件位于 `<data>/ipdb/geoip/`）
    #[serde(rename = "ip-database", default)]
    pub ip_database: IpDatabaseConfig,
    /// 服务端日志/入库文案语言（对齐上游 `Main.DEF_LOCALE` 的作用）
    #[serde(default = "default_language")]
    pub language: LanguageConfig,
    /// 匿名统计开关（对齐上游 `config.yml` 的 `analytics`；
    /// 本移植不采集任何统计，该键用于与上游配置文件/WebUI 状态完全互通）
    #[serde(default = "p_true")]
    pub analytics: bool,
    /// `push:` 段（告警推送渠道）
    #[serde(default)]
    pub push: PushSection,
    /// `btn:` 段（BTN 网络传输层，对齐上游 `config.yml` 的 `btn:`）。
    ///
    /// 默认形态（[`BtnNetworkConfig::default`]）**全部关闭** ⇒ 不构造 BTN 客户端、
    /// 不起后台线程、零网络请求（对齐上游 `isEnableBTN() == false`）；
    /// 此时 `profile.module.btn` 保持「未注入规则 ⇒ 恒 pass、绝不封禁」。
    #[serde(default)]
    pub btn: BtnNetworkConfig,
    #[serde(default)]
    pub downloaders: Vec<DownloaderConfig>,
    /// 上游 `client:` 段（下载器 UUID → 配置）；加载时并入 [`AppConfig::downloaders`]。
    #[serde(rename = "client", default)]
    pub clients: Option<serde_yaml::Mapping>,
    /// 上游 `push-notification:` 段（渠道名 → 渠道配置）；加载时并入 [`AppConfig::push`]。
    #[serde(rename = "push-notification", default)]
    pub push_notification: Option<PushSection>,
    /// `logger:` 段（对齐上游 `config.yml` 的 `logger`）
    #[serde(default)]
    pub logger: LoggerConfig,
    /// `lookup:` 段（`lookup.dns-reverse-lookup`）
    #[serde(default)]
    pub lookup: LookupConfig,
    /// `resolvers:` 段（PTR 查询使用的 DNS 服务器）
    #[serde(default)]
    pub resolvers: ResolversConfig,
    /// `proxy:` 段（出站 HTTP 代理）
    #[serde(default)]
    pub proxy: ProxyConfig,
    /// `performance:` 段（Windows EcoQoS）
    #[serde(default)]
    pub performance: PerformanceConfig,
}

/// `logger:` 段：目前仅覆盖 ban wave 完成日志的隐藏开关
/// （上游 `DownloaderServerImpl.hideFinishLogs`）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LoggerConfig {
    /// 上游 `logger.hide-finish-log`：为 true 时不打印 ban wave 完成日志（默认 false）
    #[serde(rename = "hide-finish-log", default)]
    pub hide_finish_log: bool,
}

/// `push:` 段：渠道名 → 渠道配置（段内 `type` 决定渠道类型）。
///
/// 用 `serde_yaml::Mapping`（内部是 `IndexMap`）而非 `BTreeMap`：保持 YAML 中的书写顺序，
/// 对齐上游 `PushManagerImpl.reloadConfig` 用 `config.getKeys(false)` 的遍历顺序
/// （`PushManagerImpl.pushMessage` 按该顺序逐个推送）。
pub type PushSection = serde_yaml::Mapping;

/// 单个推送渠道配置，对齐上游 `PushManagerImpl.createPushProvider(name, type, section)`：
/// 渠道名来自 `push:` 段的键，类型来自段内的 `type`，其余字段即各 provider 的
/// `loadFromYaml` 读取的键（大小写不敏感的 `type` 由 [`PushProviderConfig::parse`] 归一化）。
///
/// 默认值逐条对齐上游各 `loadFromYaml` 的 `getString(key, default)` / `getInt(key, default)`：
/// `bark.backend_url` = `https://api.day.app/push`、`pushdeer.endpoint` =
/// `https://api2.pushdeer.com/message/push`、`gotify.endpoint` =
/// `https://push.example.de/message?token=<apptoken>` + `priority` = 5、`ntfy.server_url` =
/// `https://ntfy.sh` + `priority` = 3、`smtp.name` = `PeerBanHelper` + `encryption` = `SSLTLS`
/// + `sendPartial` = true、`webhook.method` = `POST` + `content-type` = `application/json`
/// + `body-template` = [`DEFAULT_WEBHOOK_BODY_TEMPLATE`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PushProviderConfig {
    /// `PushPlusPushProvider.loadFromYaml`：空白的 `topic`/`channel` 视为未配置
    PushPlus {
        #[serde(default)]
        token: String,
        #[serde(default)]
        topic: String,
        #[serde(default)]
        channel: String,
    },
    /// `ServerChanPushProvider.loadFromYaml`：空白的 `channel`/`openid` 视为未配置
    ServerChan {
        #[serde(default)]
        sendkey: String,
        #[serde(default)]
        channel: String,
        #[serde(default)]
        openid: String,
    },
    /// `SmtpPushProvider.loadFromYaml`（`port` 缺省为 0，与上游 `getInt("port")` 一致）
    Smtp {
        #[serde(default)]
        host: String,
        /// 上游示例配置把端口写成**字符串**（`port: '587'`），这里两种写法都接受。
        #[serde(default, deserialize_with = "deserialize_port_string_or_number")]
        port: u16,
        #[serde(default)]
        auth: bool,
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: String,
        #[serde(default)]
        sender: String,
        #[serde(default = "default_smtp_sender_name")]
        name: String,
        #[serde(default)]
        receiver: Vec<String>,
        #[serde(default = "default_smtp_encryption")]
        encryption: String,
        #[serde(default = "p_true", rename = "sendPartial")]
        send_partial: bool,
    },
    /// `TelegramPushProvider.loadFromYaml`
    Telegram {
        #[serde(default)]
        token: String,
        #[serde(default)]
        chatid: String,
    },
    /// `BarkPushProvider.loadFromYaml`（`message_group` 为空时仍按空串发送，不做置空）
    Bark {
        #[serde(default = "default_bark_backend_url")]
        backend_url: String,
        #[serde(default)]
        device_key: String,
        #[serde(default)]
        message_group: String,
    },
    /// `PushDeerPushProvider.loadFromYaml`
    PushDeer {
        #[serde(default = "default_pushdeer_endpoint")]
        endpoint: String,
        #[serde(default)]
        pushkey: String,
    },
    /// `GotifyPushProvider.loadFromYaml`
    Gotify {
        #[serde(default = "default_gotify_endpoint")]
        endpoint: String,
        #[serde(default = "default_gotify_priority")]
        priority: i64,
    },
    /// `NtfyPushProvider.loadFromYaml`
    Ntfy {
        #[serde(default = "default_ntfy_server_url")]
        server_url: String,
        #[serde(default)]
        topic: String,
        #[serde(default)]
        token: String,
        #[serde(default = "default_ntfy_priority")]
        priority: i64,
        #[serde(default)]
        tags: String,
    },
    /// `WebhookPushProvider.loadFromYaml`；同时接受 `loadFromJson` 的键名
    /// `content_type` / `body_template`（上游两套加载器对同一份配置使用不同命名）
    Webhook {
        #[serde(default)]
        url: String,
        #[serde(default = "default_webhook_method")]
        method: String,
        #[serde(
            default = "default_webhook_content_type",
            rename = "content-type",
            alias = "content_type"
        )]
        content_type: String,
        #[serde(
            default = "default_webhook_body_template",
            rename = "body-template",
            alias = "body_template"
        )]
        body_template: String,
        /// 自定义请求头；值为 `null` 的表项按上游 `applyCustomHeaders` 的 `headerValue != null`
        /// 判断被忽略（空串仍会发送）
        #[serde(default)]
        headers: std::collections::BTreeMap<String, Option<String>>,
    },
}

impl PushProviderConfig {
    /// 按上游 `PushManagerImpl.createPushProvider` 解析单个渠道配置。
    ///
    /// `type` 大小写不敏感（对齐上游 `type.toLowerCase(Locale.ROOT)`）：先归一化 `type`
    /// 再走 serde 反序列化；未知 `type` 或字段非法都返回错误，由调用方按渠道粒度隔离。
    pub fn parse(raw: &serde_yaml::Value) -> anyhow::Result<Self> {
        let serde_yaml::Value::Mapping(map) = raw else {
            anyhow::bail!("push provider 配置必须是映射（缺少 type 键）");
        };
        let Some(kind) = map.get("type").and_then(|v| v.as_str()) else {
            anyhow::bail!("push provider 配置缺少 type 键");
        };
        let mut normalized = map.clone();
        normalized.insert(
            serde_yaml::Value::String("type".into()),
            serde_yaml::Value::String(kind.trim().to_lowercase()),
        );
        Ok(serde_yaml::from_value(serde_yaml::Value::Mapping(
            normalized,
        ))?)
    }
}

/// 对齐上游 `WebhookPushProvider.DEFAULT_BODY_TEMPLATE`（Java 文本块，含结尾换行）。
pub const DEFAULT_WEBHOOK_BODY_TEMPLATE: &str = "{\n    \"title\":\"{title}\",\n    \"content\":\"{content}\",\n    \"level\":\"{level}\",\n    \"date\":\"{date}\",\n    \"time\":\"{time}\",\n    \"datetime\":\"{datetime}\",\n    \"channelName\":\"{channelName}\"\n}\n";

fn default_bark_backend_url() -> String {
    "https://api.day.app/push".into()
}
fn default_pushdeer_endpoint() -> String {
    "https://api2.pushdeer.com/message/push".into()
}
fn default_gotify_endpoint() -> String {
    "https://push.example.de/message?token=<apptoken>".into()
}
fn default_gotify_priority() -> i64 {
    5
}
fn default_ntfy_server_url() -> String {
    "https://ntfy.sh".into()
}
fn default_ntfy_priority() -> i64 {
    3
}
fn default_smtp_sender_name() -> String {
    "PeerBanHelper".into()
}
fn default_smtp_encryption() -> String {
    "SSLTLS".into()
}
fn default_webhook_method() -> String {
    "POST".into()
}
fn default_webhook_content_type() -> String {
    "application/json".into()
}
fn default_webhook_body_template() -> String {
    DEFAULT_WEBHOOK_BODY_TEMPLATE.to_string()
}

/// 端口类配置：上游示例把端口写成字符串（`port: '587'`），数字与字符串都接受。
fn deserialize_port_string_or_number<'de, D>(deserializer: D) -> Result<u16, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Number(u16),
        Text(String),
    }
    match Raw::deserialize(deserializer)? {
        Raw::Number(port) => Ok(port),
        Raw::Text(text) => {
            let text = text.trim();
            if text.is_empty() {
                Ok(0)
            } else {
                text.parse::<u16>().map_err(serde::de::Error::custom)
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanguageConfig {
    /// 文案语言，如 `zh_cn` / `zh_tw` / `en_us`
    pub locale: String,
}

/// 上游 `language:` 是**字符串**（`default` 跟随系统 / `zh_cn` / `en_us` / `zh_tw`），
/// 本移植的既有配置是 `{ locale: ... }` 结构体；两者都接受，写回时统一输出字符串。
impl<'de> Deserialize<'de> for LanguageConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Name(String),
            Locale { locale: String },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Name(name) => LanguageConfig {
                locale: resolve_locale_name(&name),
            },
            Raw::Locale { locale } => LanguageConfig { locale },
        })
    }
}

impl Serialize for LanguageConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.locale)
    }
}

/// `default`（跟随系统）→ 本移植内嵌文案的可用 locale；未知值原样保留（查不到时回退 zh_cn）。
fn resolve_locale_name(name: &str) -> String {
    match name.trim() {
        "" | "default" => default_locale(),
        other => other.to_string(),
    }
}

fn default_locale() -> String {
    "zh_cn".into()
}

fn default_language() -> LanguageConfig {
    LanguageConfig {
        locale: default_locale(),
    }
}

impl AppConfig {
    /// 组装下载器侧需要的重映射配置。
    pub fn remap_config(&self) -> RemapConfig {
        RemapConfig {
            banlist_remapping: self.banlist_remapping.clone(),
            ip_remapping: self.ip_remapping.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "p9898")]
    pub http: u16,
    #[serde(default = "p_addr")]
    pub address: String,
    #[serde(default)]
    pub token: String,
    /// 对外地址（用于生成下载器可访问的 blocklist URL；留空则按监听地址推断）
    #[serde(default, rename = "external-address")]
    pub external_address: Option<String>,
    /// 上游 `server.prefix`：完整 URL（如 `http://127.0.0.1:9898`），
    /// 仅用于推导对外 host（本移植自行拼接 `/blocklist/...` 路径）。
    #[serde(default)]
    pub prefix: Option<String>,
    /// 演练模式：照常登录/拉取/判定/落库，但不向下载器下发封禁/解封与限速。
    /// 与 CLI `--dry-run` 任一开启即生效；WebUI 配置页修改后 reload 热应用。
    #[serde(default, rename = "dry-run")]
    pub dry_run: bool,
    /// 允许任意来源的 CORS（对齐上游 `server.allow-cors`；默认 false）
    #[serde(default, rename = "allow-cors")]
    pub allow_cors: bool,
}

impl ServerConfig {
    /// 下载器侧可访问的地址（`0.0.0.0`/`::` 视为本机回环）。
    pub fn public_host(&self) -> String {
        if let Some(addr) = &self.external_address {
            if !addr.trim().is_empty() {
                return addr.trim().to_string();
            }
        }
        // 上游 `prefix` 是完整 URL，取其中的 host
        if let Some(prefix) = &self.prefix {
            let prefix = prefix.trim();
            if !prefix.is_empty() {
                if let Some(rest) = prefix.split("://").nth(1) {
                    let host = rest.split('/').next().unwrap_or(rest);
                    let host = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
                    if !host.is_empty() && host != "0.0.0.0" && host != "::" {
                        return host.to_string();
                    }
                }
            }
        }
        match self.address.as_str() {
            "0.0.0.0" | "::" | "[::]" | "" => "127.0.0.1".to_string(),
            other => other.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    #[serde(default = "p_sqlite")]
    #[serde(rename = "type")]
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PersistConfig {
    #[serde(default = "p_keep_days", alias = "ban_logs_keep_days")]
    pub ban_logs_keep_days: i64,
    #[serde(default = "p_true")]
    pub banlist: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloaderConfig {
    /// 下载器 ID：上游用配置键（UUID）作为 ID（`history.downloader` / `pcb_*` 等表都用它），
    /// 本移植的单文件布局缺省回退为 `name`。
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default = "p_qb_name")]
    pub name: String,
    #[serde(default = "p_qb_type")]
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub endpoint: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    #[serde(rename = "api-key", alias = "apiKey")]
    pub api_key: Option<String>,
    /// 访问令牌：BiglyBT 适配器插件用（`Authorization: Bearer <token>`），
    /// Aria2Next 用（`Authorization: token:<secret>` 且作为 JSON-RPC params 首元素，
    /// 对齐上游 aria2 的 `--rpc-secret`）
    #[serde(default)]
    pub token: String,
    /// HTTP Basic 认证（反向代理在 qBittorrent 前面时用）；上游 `basic-auth: {user, pass}`。
    /// 序列化成 YAML 时是嵌套映射，前端读的是 `basicAuth` 对象。
    #[serde(default)]
    #[serde(rename = "basic-auth", alias = "basicAuth")]
    pub basic_auth: Option<DownloaderBasicAuth>,
    #[serde(default = "p_true")]
    #[serde(rename = "ignore-private", alias = "ignorePrivate")]
    pub ignore_private: bool,
    #[serde(default = "p_true")]
    #[serde(rename = "increment-ban", alias = "incrementBan")]
    pub increment_ban: bool,
    /// 是否校验 TLS 证书（上游键名 `verify-ssl`；本移植早期用 `verify-tls`，两者都读）
    #[serde(default)]
    #[serde(rename = "verify-ssl", alias = "verify-tls", alias = "verifySsl")]
    pub verify_tls: bool,
    /// RPC 路径；留空时由各适配器按类型回退默认值
    /// （Transmission 为 `/transmission/rpc`、Deluge 为 `/json`）
    #[serde(default)]
    #[serde(rename = "rpc-url", alias = "rpcUrl")]
    pub rpc_url: String,
    /// 前端表单的 HTTP 版本下拉框字段（上游 Java 无此字段、Gson 反序列化时忽略；
    /// 本项目同样只回显不生效，但**必须存在**，否则编辑弹窗的下拉框为空）
    #[serde(default = "p_http_version")]
    #[serde(rename = "http-version", alias = "httpVersion")]
    pub http_version: String,
    /// 启动时暂停该下载器（对齐上游配置的 `paused`，Deluge 使用）
    #[serde(default)]
    pub paused: bool,
}

/// 上游 `QBittorrentBasicAuth(user, pass)`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DownloaderBasicAuth {
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub pass: Option<String>,
}

impl DownloaderConfig {
    /// 下载器 ID（配置里的 `id`，缺省回退 `name`）。
    ///
    /// 对齐上游：`client:` 段的键（UUID）就是下载器 ID，`history.downloader` /
    /// `pcb_address.downloader` / `peer_records.downloader` 等表都存这个值。
    pub fn resolved_id(&self) -> String {
        match &self.id {
            Some(id) if !id.trim().is_empty() => id.trim().to_string(),
            _ => self.name.clone(),
        }
    }

    /// `rpc-url` 留空时按下载器类型取默认值。
    pub fn resolved_rpc_url(&self, default: &str) -> String {
        let value = self.rpc_url.trim();
        if value.is_empty() {
            default.to_string()
        } else {
            value.to_string()
        }
    }

    /// 上游 `basic-auth` 的生效判定：`user` 非空才启用（qB 适配器在收到 401 时重试）。
    pub fn basic_auth_pair(&self) -> Option<(String, String)> {
        let auth = self.basic_auth.as_ref()?;
        let user = auth.user.clone().unwrap_or_default();
        if user.trim().is_empty() {
            return None;
        }
        Some((user, auth.pass.clone().unwrap_or_default()))
    }

    /// **Web API 专用**的配置形状：对齐上游 `saveDownloaderJson()`（Gson 序列化 Java
    /// `Config` 对象 ⇒ camelCase），键集与前端 `webui/src/api/model/downloader.ts`
    /// 各类型的表单一致。
    ///
    /// 不能直接序列化本结构：YAML 侧是 kebab-case 且 `basic-auth` 可缺省，
    /// 而 qBittorrent 表单绑定 `config.basicAuth.user` —— 该对象缺失会让子表单
    /// 渲染时抛错，编辑弹窗只剩「类型 / 暂停检查」两项（2026-10-29 生产反馈）。
    pub fn web_json(&self) -> serde_json::Value {
        let mut full = serde_json::Map::new();
        full.insert("type".into(), serde_json::json!(self.kind));
        full.insert("name".into(), serde_json::json!(self.name));
        full.insert("endpoint".into(), serde_json::json!(self.endpoint));
        full.insert("username".into(), serde_json::json!(self.username));
        full.insert("password".into(), serde_json::json!(self.password));
        full.insert(
            "apiKey".into(),
            serde_json::json!(self.api_key.clone().unwrap_or_default()),
        );
        full.insert("token".into(), serde_json::json!(self.token.clone()));
        full.insert(
            "basicAuth".into(),
            serde_json::json!({
                "user": self
                    .basic_auth
                    .as_ref()
                    .and_then(|a| a.user.clone())
                    .unwrap_or_default(),
                "pass": self
                    .basic_auth
                    .as_ref()
                    .and_then(|a| a.pass.clone())
                    .unwrap_or_default(),
            }),
        );
        full.insert("httpVersion".into(), serde_json::json!(self.http_version));
        full.insert("incrementBan".into(), serde_json::json!(self.increment_ban));
        full.insert("verifySsl".into(), serde_json::json!(self.verify_tls));
        full.insert(
            "ignorePrivate".into(),
            serde_json::json!(self.ignore_private),
        );
        full.insert("paused".into(), serde_json::json!(self.paused));
        full.insert("rpcUrl".into(), serde_json::json!(self.rpc_url));

        // 上游各 Config 的字段集不同（前端表单也只绑这些），按类型裁剪
        let kept: &[&str] = match self.kind.as_str() {
            "qbittorrent" => &[
                "type",
                "name",
                "endpoint",
                "username",
                "password",
                "apiKey",
                "basicAuth",
                "httpVersion",
                "incrementBan",
                "verifySsl",
                "ignorePrivate",
                "paused",
            ],
            "qbittorrentee" => &[
                "type",
                "name",
                "endpoint",
                "username",
                "password",
                "apiKey",
                "basicAuth",
                "httpVersion",
                "incrementBan",
                "useShadowBan",
                "verifySsl",
                "ignorePrivate",
                "paused",
            ],
            "transmission" => &[
                "type",
                "name",
                "endpoint",
                "username",
                "password",
                "httpVersion",
                "verifySsl",
                "ignorePrivate",
                "paused",
                "rpcUrl",
            ],
            "deluge" => &[
                "type",
                "name",
                "endpoint",
                "password",
                "httpVersion",
                "incrementBan",
                "verifySsl",
                "ignorePrivate",
                "paused",
                "rpcUrl",
            ],
            "biglybt" => &[
                "type",
                "name",
                "endpoint",
                "token",
                "httpVersion",
                "verifySsl",
                "ignorePrivate",
                "paused",
            ],
            "bitcomet" => &[
                "type",
                "name",
                "endpoint",
                "username",
                "password",
                "httpVersion",
                "incrementBan",
                "verifySsl",
                "ignorePrivate",
                "paused",
            ],
            // aria2next 表单没有 httpVersion（对齐上游前端模型）
            "aria2next" | "aria2" => &[
                "type",
                "name",
                "endpoint",
                "token",
                "verifySsl",
                "ignorePrivate",
                "paused",
            ],
            _ => &[],
        };
        if kept.is_empty() {
            return serde_json::Value::Object(full);
        }
        let mut out = serde_json::Map::new();
        for key in kept {
            if let Some(value) = full.get(*key) {
                out.insert((*key).to_string(), value.clone());
            }
        }
        // qBittorrentEE 的 useShadowBan 本移植未实现，按 false 回显（表单开关不缺失）
        if self.kind == "qbittorrentee" && !out.contains_key("useShadowBan") {
            out.insert("useShadowBan".into(), serde_json::json!(false));
        }
        serde_json::Value::Object(out)
    }
}

fn p9898() -> u16 {
    9898
}
fn p_addr() -> String {
    "0.0.0.0".into()
}
fn p_sqlite() -> String {
    "sqlite".into()
}
fn p_keep_days() -> i64 {
    180
}
/// 前端 HTTP 版本下拉框的回显默认值（上游 Java 无该字段，仅界面用）。
fn p_http_version() -> String {
    "HTTP_1_1".to_string()
}

fn p_true() -> bool {
    true
}
fn p_qb_name() -> String {
    "qBittorrent".into()
}
fn p_qb_type() -> String {
    "qbittorrent".into()
}
fn default_server() -> ServerConfig {
    ServerConfig {
        http: p9898(),
        address: p_addr(),
        token: String::new(),
        external_address: None,
        prefix: None,
        dry_run: false,
        allow_cors: false,
    }
}
fn default_database() -> DatabaseConfig {
    DatabaseConfig { kind: p_sqlite() }
}
fn default_persist() -> PersistConfig {
    PersistConfig {
        ban_logs_keep_days: p_keep_days(),
        banlist: true,
    }
}
fn default_profile() -> ProfileConfig {
    ProfileConfig::default()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: default_server(),
            database: default_database(),
            persist: default_persist(),
            profile: default_profile(),
            banlist_remapping: BanlistRemapping::default(),
            ip_remapping: IpRemapConfig::default(),
            ip_database: IpDatabaseConfig::default(),
            language: default_language(),
            push: PushSection::default(),
            btn: BtnNetworkConfig::default(),
            downloaders: vec![],
            clients: None,
            push_notification: None,
            logger: LoggerConfig::default(),
            lookup: LookupConfig::default(),
            resolvers: ResolversConfig::default(),
            proxy: ProxyConfig::default(),
            performance: PerformanceConfig::default(),
            analytics: true,
        }
    }
}

const DEFAULT_CONFIG_YAML: &str = include_str!("default-config.yml");

/// 生成随机访问令牌（32 位十六进制，形状对齐上游 UUID 去连字符的写法）。
fn generate_server_token() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| char::from_digit(rng.gen_range(0..16), 16).unwrap_or('0'))
        .collect()
}

impl AppConfig {
    /// 从 data 目录加载配置；不存在则写入默认配置。
    ///
    /// 支持两种布局（上游优先）：
    /// 1. **上游部署布局**：`<data>/config/config.yml` + `<data>/config/profile.yml`
    ///    （`profile.yml` 存在时覆盖 `config.yml` 里的 `profile:` 段，对齐上游分文件存储）；
    /// 2. **本移植单文件布局**：`<data>/config.yml`（`profile:` 内嵌）。
    pub fn load_or_create(data_dir: &Path) -> anyhow::Result<(Self, PathBuf)> {
        std::fs::create_dir_all(data_dir)?;
        let upstream_config = data_dir.join("config").join("config.yml");
        if upstream_config.exists() {
            let text = std::fs::read_to_string(&upstream_config)?;
            let mut cfg: AppConfig = serde_yaml::from_str(&text)?;
            let upstream_profile = data_dir.join("config").join("profile.yml");
            if upstream_profile.exists() {
                let profile_text = std::fs::read_to_string(&upstream_profile)?;
                cfg.profile = serde_yaml::from_str(&profile_text)?;
            }
            cfg.normalize_upstream();
            return Ok((cfg, upstream_config));
        }
        let path = data_dir.join("config.yml");
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let mut cfg: AppConfig = serde_yaml::from_str(&text)?;
            cfg.normalize_upstream();
            Ok((cfg, path))
        } else {
            // 首次启动：随机生成访问令牌并写回。
            // 上游在初始化向导里设置 token，空 token 会让所有鉴权 API 返回 303 /init；
            // 本移植暂无向导，因此在这里生成，避免出厂配置直接把写接口暴露给匿名访问。
            let token = generate_server_token();
            let text =
                DEFAULT_CONFIG_YAML.replacen("token: \"\"", &format!("token: \"{token}\""), 1);
            std::fs::write(&path, &text)?;
            let mut cfg: AppConfig = serde_yaml::from_str(&text)?;
            if cfg.server.token.is_empty() {
                cfg.server.token = token;
            }
            tracing::info!("已生成访问令牌并写入 {}（WebUI 鉴权使用）", path.display());
            Ok((cfg, path))
        }
    }

    /// 把**上游布局**的配置段并入本移植结构：
    /// - `client:` 映射（下载器 UUID → 配置）→ `downloaders` 列表（UUID 作为下载器 ID）；
    /// - `push-notification:` 段 → `push` 段。
    ///
    /// 两者都只在目标段为空时生效，避免覆盖本移植自身的写法。
    pub fn normalize_upstream(&mut self) {
        if self.downloaders.is_empty() {
            if let Some(mapping) = self.clients.take() {
                for (key, value) in mapping {
                    let Some(uuid) = key.as_str() else {
                        warn!("client 段存在非字符串键，已跳过: {key:?}");
                        continue;
                    };
                    match serde_yaml::from_value::<DownloaderConfig>(value) {
                        Ok(mut config) => {
                            if config.id.is_none() {
                                config.id = Some(uuid.to_string());
                            }
                            if config.name.is_empty() {
                                config.name = uuid.to_string();
                            }
                            self.downloaders.push(config);
                        }
                        Err(e) => warn!("client.{uuid} 解析失败，已跳过: {e}"),
                    }
                }
            }
        }
        if self.push.is_empty() {
            if let Some(mapping) = self.push_notification.take() {
                self.push = mapping;
            }
        }
    }

    /// 序列化并写回配置文件（Web 配置保存与下载器/推送热管理的共用写入口）。
    ///
    /// 若加载自上游布局（`<data>/config/config.yml`），同步维护同目录的 `profile.yml`，
    /// 保证 Java 端与本移植可以共用同一数据目录（`profile` 段独立成文件）。
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        // 与上游 `mergeYaml` 一致：以现有文件为基底增量覆盖，保留 WebUI/本移植
        // 未建模的键（`gui` / `pbh-plus-key` / `proxy` 等），避免保存一次配置就丢配置。
        let mut document = read_yaml_document(path);
        let mut overlay = serde_yaml::to_value(self)?;
        if let Some(mapping) = overlay.as_mapping_mut() {
            // 内部别名不落盘：下载器统一写 `downloaders`、推送统一写 `push`
            // （字段的 serde 名就是 `client` / `push-notification`）
            for key in ["client", "clients", "push-notification", "push_notification"] {
                mapping.remove(serde_yaml::Value::String(key.into()));
            }
        }
        if let Some(mapping) = document.as_mapping_mut() {
            // 上游布局的别名：内容已并入 `downloaders` / `push`，移除避免下次加载重复
            mapping.remove(serde_yaml::Value::String("client".into()));
            mapping.remove(serde_yaml::Value::String("push-notification".into()));
        }
        merge_yaml(&mut document, &overlay);
        std::fs::write(path, serde_yaml::to_string(&document)?)?;
        let from_config_dir = path
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name == "config")
            .unwrap_or(false);
        if from_config_dir {
            let profile_path = path.with_file_name("profile.yml");
            let mut profile_document = read_yaml_document(&profile_path);
            merge_yaml(&mut profile_document, &serde_yaml::to_value(&self.profile)?);
            std::fs::write(profile_path, serde_yaml::to_string(&profile_document)?)?;
        }
        Ok(())
    }
}

/// 读取 YAML 文档；文件不存在/无法解析时返回空映射（保存时以空基底合并）。
fn read_yaml_document(path: &Path) -> serde_yaml::Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_yaml::from_str::<serde_yaml::Value>(&text).ok())
        .filter(serde_yaml::Value::is_mapping)
        .unwrap_or_else(|| serde_yaml::Value::Mapping(Default::default()))
}

/// 递归合并：`overlay` 存在的键覆盖 `base`，列表整体替换；未提交的键保留。
fn merge_yaml(base: &mut serde_yaml::Value, overlay: &serde_yaml::Value) {
    if let (Some(base_map), Some(overlay_map)) = (base.as_mapping_mut(), overlay.as_mapping()) {
        for (key, value) in overlay_map {
            match base_map.get_mut(key) {
                Some(existing) => merge_yaml(existing, value),
                None => {
                    base_map.insert(key.clone(), value.clone());
                }
            }
        }
    } else {
        *base = overlay.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbh_core::modules::InMemoryMonitorSink;
    use std::sync::Arc;

    /// 出厂配置必须能被完整解析，且本轮接线新增的配置节都能落到运行期结构上。
    ///
    /// 键名写错时 serde 会**静默忽略**，因此这里逐段断言（而不是只断言「能解析」）。
    #[test]
    fn shipped_default_config_parses_with_all_integrated_sections() {
        let cfg: AppConfig = serde_yaml::from_str(DEFAULT_CONFIG_YAML).expect("出厂配置必须可解析");

        // ip-database（GeoIP/GeoCN）
        assert!(cfg.ip_database.auto_update);
        assert_eq!(cfg.ip_database.database_city, "GeoLite2-City");
        assert_eq!(cfg.ip_database.database_asn, "GeoLite2-ASN");
        assert_eq!(cfg.ip_database.database_geocn, "GeoCN");

        // module.ip-address-blocker 的 GeoIP 维度
        let ipb = cfg
            .profile
            .module
            .ip_address_blocker
            .as_ref()
            .expect("ip-address-blocker");
        assert_eq!(ipb.asns, vec![0]);
        assert_eq!(ipb.regions, vec!["0".to_string()]);
        assert_eq!(ipb.cities, vec!["示例海南".to_string()]);
        assert!(
            ipb.net_type.to_tokens().is_empty(),
            "随包 net-type 开关全为 false"
        );

        // btn（传输层）：出厂默认必须关闭 ⇒ 不构造客户端、不起线程、零网络请求
        assert!(!cfg.btn.is_active(), "出厂配置必须关闭 BTN 联网");

        // module.btn：已进入流水线（AutoRangeBan 之后、IPBlackRuleList 之前）
        let btn = cfg.profile.module.btn.as_ref().expect("module.btn");
        assert_eq!(btn.ban_duration_ms, 259_200_000);
        let pipeline = cfg.profile.build_pipeline_with_geo(None);
        let names = pbh_core::config::module_config_names(&pipeline);
        let btn_index = names
            .iter()
            .position(|n| *n == "btn")
            .expect("btn 必须进流水线");
        assert_eq!(names[btn_index - 1], "auto-range-ban");
        assert_eq!(names[btn_index + 1], "ip-address-blocker-rules");

        // 监控模块：配置存在、可构造、但不进流水线
        assert!(cfg.profile.module.active_monitoring.is_some());
        assert!(cfg.profile.module.peer_analyse_service.is_some());
        let monitor_modules = cfg
            .profile
            .build_monitor_modules(Arc::new(InMemoryMonitorSink::new()));
        let monitor_names: Vec<&str> = monitor_modules.iter().map(|m| m.config_name()).collect();
        assert_eq!(
            monitor_names,
            vec![
                "active-monitoring",
                "peer-analyse-service.swarm-tracking",
                "peer-analyse-service.session-analyse",
                "peer-analyse-service.peer-recording"
            ]
        );
        assert!(
            !names.contains(&"active-monitoring"),
            "监控模块不参与 peer 判定"
        );

        // ip-remapping.auto-stun：默认关闭 ⇒ 不挂载映射表（严格 no-op）
        let remap = cfg.remap_config();
        assert!(!remap.ip_remapping.auto_stun.enabled);
        assert!(remap.ip_remapping.auto_stun_registry.is_none());
        assert_eq!(remap.ip_remapping.auto_stun.tcp_servers.len(), 3);
        assert_eq!(remap.ip_remapping.auto_stun.udp_servers.len(), 5);
    }

    /// 上游部署布局：`<data>/config/config.yml` + `<data>/config/profile.yml`，
    /// 覆盖 `language: default`、`client:` 映射、`push-notification:`、字符串端口、
    /// `ban-duration: default` 等上游写法（对跑验证时逐条修过的兼容点）。
    #[test]
    fn upstream_layout_loads_client_mapping_and_profile_file() {
        let dir = std::env::temp_dir().join(format!("pbh-cfg-upstream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(
            dir.join("config").join("config.yml"),
            r#"
config-version: 47
language: default
server:
  http: 9898
  address: 0.0.0.0
  prefix: http://127.0.0.1:9898
  token: test-token
persist:
  ban-logs-keep-days: 30
  banlist: true
client:
  bbe015c9-31bc-423b-b6b2-2d17c37a5113:
    type: qbittorrent
    name: qBittorrent_a
    endpoint: http://127.0.0.1:9091
    username: admin
    password: 'secret'
    increment-ban: true
    ignore-private: false
push-notification:
  example:
    type: ntfy
    server_url: https://ntfy.example.com
    token: t
  email-example:
    type: smtp
    host: smtp.example.com
    port: '587'
"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("config").join("profile.yml"),
            "check-interval: 120000\nban-duration: 1209600000\nmodule:\n  expression-engine:\n    enabled: true\n    ban-duration: default\n",
        )
        .unwrap();

        let (cfg, path) = AppConfig::load_or_create(&dir).unwrap();
        assert_eq!(path, dir.join("config").join("config.yml"));
        assert_eq!(
            cfg.language.locale, "zh_cn",
            "language: default → 服务端默认 locale"
        );
        assert_eq!(
            cfg.server.public_host(),
            "127.0.0.1",
            "server.prefix 推导对外 host"
        );
        assert_eq!(cfg.persist.ban_logs_keep_days, 30, "kebab-case 键名");
        assert_eq!(cfg.downloaders.len(), 1);
        assert_eq!(
            cfg.downloaders[0].resolved_id(),
            "bbe015c9-31bc-423b-b6b2-2d17c37a5113",
            "client 段的键（UUID）即下载器 ID，供 history/pcb 等表关联"
        );
        assert_eq!(cfg.downloaders[0].name, "qBittorrent_a");
        assert!(!cfg.downloaders[0].ignore_private);
        assert!(
            cfg.push
                .contains_key(serde_yaml::Value::String("example".into())),
            "push-notification 段并入 push"
        );
        assert!(
            cfg.push
                .contains_key(serde_yaml::Value::String("email-example".into())),
            "SMTP 字符串端口（'587'）也要能解析"
        );
        assert_eq!(
            cfg.profile.check_interval, 120_000,
            "profile.yml 覆盖 config.yml"
        );
        let engine = cfg
            .profile
            .module
            .expression_engine
            .as_ref()
            .expect("expression-engine");
        assert_eq!(
            engine.ban_duration_ms, 0,
            "ban-duration: default → 0（回退全局）"
        );

        // 保存时同步维护同目录的 profile.yml（保证 Java 端仍可读取）
        cfg.save_to(&path).unwrap();
        let profile_text = std::fs::read_to_string(dir.join("config").join("profile.yml")).unwrap();
        assert!(profile_text.contains("check-interval"), "{profile_text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 保存主配置时保留未建模键（上游 `mergeYaml` 语义），并把上游 `client:` 布局
    /// 转换为 `downloaders:`（避免下次加载重复）；profile.yml 的未知段同样保留。
    #[test]
    fn save_preserves_unknown_keys_and_converts_upstream_client_layout() {
        let dir = std::env::temp_dir().join(format!("pbh-config-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let config_dir = dir.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let path = config_dir.join("config.yml");
        std::fs::write(
            &path,
            r#"
config-version: 48
gui: auto
pbh-plus-key: []
server:
  http: 9898
  address: "0.0.0.0"
  token: "abc"
  prefix: "http://127.0.0.1:9898"
  allow-cors: false
client:
  uuid-1:
    name: "qB"
    type: "qbittorrent"
    endpoint: "http://127.0.0.1:8080"
"#,
        )
        .unwrap();
        std::fs::write(
            config_dir.join("profile.yml"),
            "check-interval: 5000\ncustom-section:\n  keep: true\n",
        )
        .unwrap();

        let mut cfg: AppConfig =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        cfg.normalize_upstream();
        cfg.save_to(&path).unwrap();

        let saved: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved["gui"], "auto");
        assert!(saved.get("pbh-plus-key").is_some());
        assert!(saved.get("client").is_none(), "client 应转换为 downloaders");
        assert_eq!(saved["downloaders"][0]["name"], "qB");

        let profile: serde_yaml::Value = serde_yaml::from_str(
            &std::fs::read_to_string(config_dir.join("profile.yml")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            profile["custom-section"]["keep"], true,
            "profile 未建模键应保留"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 从 JSON 构造下载器配置（测试用，走与运行期相同的 serde 规则）。
    fn dl_config(json: serde_json::Value) -> DownloaderConfig {
        serde_json::from_value(json).expect("下载器配置必须可解析")
    }

    /// JSON 对象的键名升序列表（比对前端契约字段集用）。
    fn sorted_keys(value: &serde_json::Value) -> Vec<&str> {
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("config 必须是对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        keys
    }

    /// Web API 的 `config` 形状必须等于上游 `saveDownloaderJson()`（Gson camelCase）
    /// 与前端 `api/model/downloader.ts` 的字段集——qBittorrent 表单绑定
    /// `config.basicAuth.user`，缺该对象会让编辑弹窗整个子表单渲染失败。
    #[test]
    fn web_json_matches_upstream_frontend_contract() {
        let qb = dl_config(serde_json::json!({
            "type": "qbittorrent", "name": "qB", "endpoint": "http://127.0.0.1:9091",
            "username": "admin", "password": "pw", "api-key": "ak",
            "basic-auth": { "user": "proxy-user", "pass": "proxy-pass" },
            "ignore-private": true, "increment-ban": false, "verify-ssl": true, "paused": false,
        }));
        let value = qb.web_json();
        let keys = sorted_keys(&value);
        assert_eq!(
            keys,
            [
                "apiKey",
                "basicAuth",
                "endpoint",
                "httpVersion",
                "ignorePrivate",
                "incrementBan",
                "name",
                "password",
                "paused",
                "type",
                "username",
                "verifySsl"
            ],
            "qbittorrent 的 config 键集必须与前端模型一致"
        );
        assert_eq!(value["basicAuth"]["user"], "proxy-user");
        assert_eq!(value["basicAuth"]["pass"], "proxy-pass");
        assert_eq!(value["apiKey"], "ak");
        assert_eq!(value["verifySsl"], true);
        assert_eq!(value["ignorePrivate"], true);
        assert_eq!(value["httpVersion"], "HTTP_1_1");

        // 其它类型按各自表单绑定的键集裁剪：transmission 无 apiKey/basicAuth，
        // aria2next 连 httpVersion 都没有（对齐上游前端模型）
        let tr = dl_config(serde_json::json!({ "type": "transmission", "name": "tr" }));
        let tr_value = tr.web_json();
        let tr_keys = sorted_keys(&tr_value);
        assert_eq!(
            tr_keys,
            [
                "endpoint",
                "httpVersion",
                "ignorePrivate",
                "name",
                "password",
                "paused",
                "rpcUrl",
                "type",
                "username",
                "verifySsl"
            ]
        );
        let aria = dl_config(serde_json::json!({ "type": "aria2next", "name": "a2" }));
        let aria = aria.web_json();
        assert!(aria.get("httpVersion").is_none(), "aria2next 表单没有 httpVersion");
        assert_eq!(aria["token"], "");
        assert!(aria.get("apiKey").is_none());
    }

    /// `basicAuth` 必须**始终**是对象（哪怕没配）：前端 `config.basicAuth.user`
    /// 直接取嵌套属性，缺对象就抛错。
    #[test]
    fn web_json_always_emits_basic_auth_object() {
        let qb = dl_config(serde_json::json!({ "type": "qbittorrent", "name": "qB" }));
        let value = qb.web_json();
        assert!(value["basicAuth"].is_object(), "basicAuth 必须是对象");
        assert_eq!(value["basicAuth"]["user"], "");
        assert_eq!(value["basicAuth"]["pass"], "");
        // 未配置 ⇒ 不启用 HTTP Basic（上游用空 user 时也不会带认证头）
        assert!(qb.basic_auth_pair().is_none());
    }

    /// 前端保存时提交的是 camelCase（`apiKey` / `verifySsl` / `basicAuth` / `rpcUrl`），
    /// 写入路径必须解析得动，否则编辑一次就把配置清空成默认值。
    #[test]
    fn frontend_camel_case_payload_round_trips() {
        let payload = serde_json::json!({
            "type": "qbittorrent",
            "name": "qB",
            "endpoint": "http://127.0.0.1:9091/",
            "username": "admin",
            "password": "pw",
            "apiKey": "ak-1",
            "basicAuth": { "user": "bu", "pass": "bp" },
            "httpVersion": "HTTP_2",
            "incrementBan": true,
            "verifySsl": false,
            "ignorePrivate": true,
            "paused": false
        });
        let parsed = dl_config(payload.clone());
        assert_eq!(parsed.api_key.as_deref(), Some("ak-1"));
        assert!(!parsed.verify_tls);
        assert!(parsed.ignore_private);
        assert_eq!(parsed.http_version, "HTTP_2");
        assert_eq!(
            parsed.basic_auth_pair(),
            Some(("bu".to_string(), "bp".to_string()))
        );

        // 往返：web_json 再解析回来，值不丢
        let again = dl_config(parsed.web_json());
        assert_eq!(again.api_key, parsed.api_key);
        assert_eq!(again.verify_tls, parsed.verify_tls);
        assert_eq!(again.ignore_private, parsed.ignore_private);
        assert_eq!(again.increment_ban, parsed.increment_ban);
        assert_eq!(again.basic_auth_pair(), parsed.basic_auth_pair());
        assert_eq!(again.rpc_url, parsed.rpc_url);

        // 前端 PUT 的完整 body 是 `{id, config}`，端点取 config 后走同一条解析路径
        let from_envelope = dl_config(payload);
        assert_eq!(from_envelope.name, "qB");
    }

    /// YAML 侧仍是上游的 kebab-case 键名（`verify-ssl` / `basic-auth`），
    /// 且必须继续读得懂本移植早期写下的 `verify-tls`。
    #[test]
    fn yaml_kebab_keys_are_canonical_and_legacy_alias_kept() {
        let legacy = dl_config(serde_json::json!({
            "type": "qbittorrent", "name": "qB", "verify-tls": true,
            "basic-auth": { "user": "u", "pass": "p" }
        }));
        assert!(legacy.verify_tls, "verify-tls 别名必须仍可解析");

        let as_yaml: serde_yaml::Value =
            serde_yaml::from_str(&serde_yaml::to_string(&legacy).unwrap()).unwrap();
        assert_eq!(as_yaml["verify-ssl"], true, "写出的键名对齐上游 verify-ssl");
        assert!(as_yaml.get("verify-tls").is_none());
        assert_eq!(as_yaml["basic-auth"]["user"], "u");
        assert!(
            matches!(as_yaml["api-key"], serde_yaml::Value::Null | serde_yaml::Value::String(_)),
            "api-key 写出为字符串或 null（未配置时）");
    }
}
