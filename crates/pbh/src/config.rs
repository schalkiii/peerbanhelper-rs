//! 应用配置模型与加载/生成。
//!
//! 字段对齐上游：`config.yml`（server/database/persist/downloaders）+
//! `profile.yml`（`check-interval` / `ban-duration` / `ignore-peers-from-addresses` / `module.*`）。
//! 本阶段把两者合并到同一个 `config.yml` 的 `profile:` 段，便于单文件部署。

use pbh_core::btn_transport::BtnNetworkConfig;
use pbh_core::config::{IpDatabaseConfig, ProfileConfig};
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
    #[serde(rename = "api-key")]
    pub api_key: Option<String>,
    /// 访问令牌：BiglyBT 适配器插件用（`Authorization: Bearer <token>`），
    /// Aria2Next 用（`Authorization: token:<secret>` 且作为 JSON-RPC params 首元素，
    /// 对齐上游 aria2 的 `--rpc-secret`）
    #[serde(default)]
    pub token: String,
    #[serde(default = "p_true")]
    #[serde(rename = "ignore-private")]
    pub ignore_private: bool,
    #[serde(default = "p_true")]
    #[serde(rename = "increment-ban")]
    pub increment_ban: bool,
    /// 是否校验 TLS 证书（兼容上游键名 `verify-ssl`）
    #[serde(default)]
    #[serde(rename = "verify-tls", alias = "verify-ssl")]
    pub verify_tls: bool,
    /// RPC 路径；留空时由各适配器按类型回退默认值
    /// （Transmission 为 `/transmission/rpc`、Deluge 为 `/json`）
    #[serde(default)]
    #[serde(rename = "rpc-url")]
    pub rpc_url: String,
    /// 启动时暂停该下载器（对齐上游配置的 `paused`，Deluge 使用）
    #[serde(default)]
    pub paused: bool,
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
        let text = serde_yaml::to_string(self)?;
        std::fs::write(path, text)?;
        let from_config_dir = path
            .parent()
            .and_then(|parent| parent.file_name())
            .map(|name| name == "config")
            .unwrap_or(false);
        if from_config_dir {
            let profile_path = path.with_file_name("profile.yml");
            let profile_text = serde_yaml::to_string(&self.profile)?;
            std::fs::write(profile_path, profile_text)?;
        }
        Ok(())
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
}
