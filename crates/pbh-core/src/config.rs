//! `profile.yml` 配置模型与「配置 → 判定流水线」的构建。
//!
//! 契约见 SPEC 第 6 节；键名、默认值与「配置节缺失即禁用模块」的语义均对齐上游 v9.5.1。
//!
//! 上游行为要点（`AbstractFeatureModule` / 各模块 `reloadConfig()`）：
//! - 模块配置节缺失，或节内缺少 `enabled` 键 → `shouldModuleEnabled()` 为假 → **模块被禁用**；
//! - 模块 `ban-duration` 缺省 / 为 0 → 使用全局 `ban-duration`；
//! - 规则列表是「JSON 文本字符串」数组，逐条解析。

use crate::defaults::{self, pcb};
use crate::geoip::{
    GeoIpConfig, GeoIpProvider, NET_TYPE_BACKBONE_NETWORK, NET_TYPE_BASE_STATION,
    NET_TYPE_BUSINESS_PLATFORM, NET_TYPE_DATACENTER, NET_TYPE_GOVERNMENT_AND_ENTERPRISE_LINE,
    NET_TYPE_INTERNET_CAFE, NET_TYPE_IOT, NET_TYPE_IP_PRIVATE_NETWORK, NET_TYPE_WIDEBAND,
};
use crate::iputil::IpSet;
use crate::module::RuleModule;
use crate::modules::{
    ActiveMonitoringModule, AntiVampire, AntiVampireSettings, AutoRangeBan, BtnNetworkOnline,
    ExpressionEngine, IdleConnectionDosProtection, IdleProtectionSettings, IpBlacklist,
    IpRuleListModule, MonitorSink, MultiDialingBlocker, PcbConfig, PeerRecordingServiceModule,
    ProgressCheatBlocker, ProtectionMode, SessionAnalyseServiceModule, StringBlacklist,
    SwarmTrackingModule,
};
use crate::pipeline::Pipeline;
use crate::rule::RuleSet;
use serde::{Deserialize, Serialize};

fn default_check_interval() -> u64 {
    defaults::DEFAULT_CHECK_INTERVAL_MS
}

fn default_global_ban_duration() -> i64 {
    defaults::DEFAULT_BAN_DURATION_MS
}

/// 模块 `ban-duration` 的反序列化：接受上游的字符串 `default`（等价 0 = 回退全局设置），
/// 也接受数字（毫秒）与数字字符串。
fn deserialize_ban_duration<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Keyword(String),
        Millis(i64),
    }
    match Raw::deserialize(deserializer)? {
        Raw::Millis(ms) => Ok(ms),
        Raw::Keyword(text) => {
            let text = text.trim();
            if text.is_empty() || text.eq_ignore_ascii_case("default") {
                // 0 ⇒ 使用全局 `ban-duration`（对齐上游 `use default to fallback to global settings`）
                Ok(0)
            } else {
                text.parse::<i64>().map_err(serde::de::Error::custom)
            }
        }
    }
}

fn default_ignore_addresses() -> Vec<String> {
    defaults::DEFAULT_IGNORE_ADDRESSES
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_true() -> bool {
    true
}

/// 上游 `profile.yml` 的模型。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileConfig {
    #[serde(rename = "check-interval", default = "default_check_interval")]
    pub check_interval: u64,
    /// 全局封禁时长（毫秒），模块 ban-duration 为 0/缺省时使用
    #[serde(
        rename = "ban-duration",
        default = "default_global_ban_duration",
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration: i64,
    #[serde(
        rename = "ignore-peers-from-addresses",
        default = "default_ignore_addresses"
    )]
    pub ignore_peers_from_addresses: Vec<String>,
    #[serde(default)]
    pub module: ModulesSection,
}

impl Default for ProfileConfig {
    fn default() -> Self {
        Self {
            check_interval: default_check_interval(),
            ban_duration: default_global_ban_duration(),
            ignore_peers_from_addresses: default_ignore_addresses(),
            module: ModulesSection::default(),
        }
    }
}

/// `module:` 段。**字段为 `Option`**：配置节缺失时对应模块被禁用（对齐上游语义）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModulesSection {
    #[serde(rename = "peer-id-blacklist", default)]
    pub peer_id_blacklist: Option<PeerIdBlacklistConfig>,
    #[serde(rename = "client-name-blacklist", default)]
    pub client_name_blacklist: Option<ClientNameBlacklistConfig>,
    #[serde(rename = "ip-address-blocker", default)]
    pub ip_address_blocker: Option<IpAddressBlockerConfig>,
    #[serde(rename = "progress-cheat-blocker", default)]
    pub progress_cheat_blocker: Option<ProgressCheatBlockerConfig>,
    #[serde(rename = "multi-dialing-blocker", default)]
    pub multi_dialing_blocker: Option<MultiDialingBlockerConfig>,
    #[serde(rename = "auto-range-ban", default)]
    pub auto_range_ban: Option<AutoRangeBanConfig>,
    #[serde(rename = "anti-vampire", default)]
    pub anti_vampire: Option<AntiVampireConfig>,
    #[serde(rename = "ip-address-blocker-rules", default)]
    pub ip_rule_list: Option<IpRuleListConfig>,
    #[serde(rename = "expression-engine", default)]
    pub expression_engine: Option<ExpressionEngineConfig>,
    #[serde(rename = "ptr-blacklist", default)]
    pub ptr_blacklist: Option<PtrBlacklistConfig>,
    #[serde(rename = "idle-connection-dos-protection", default)]
    pub idle_connection_dos_protection: Option<IdleProtectionConfig>,
    /// BTN 网络在线规则（上游 `BtnNetworkOnline`，配置键 `btn`）；
    /// 是否真正联网还取决于主配置 `btn.enabled`，未配置 BTN 服务器时该模块恒 pass。
    #[serde(rename = "btn", default)]
    pub btn: Option<BtnConfig>,
    /// 主动监控（上游 `ActiveMonitoringModule`，配置键 `active-monitoring`）：
    /// 流量日志、每日上行阈值告警、滑动窗口上传限速。**不参与 peer 判定**。
    #[serde(rename = "active-monitoring", default)]
    pub active_monitoring: Option<ActiveMonitoringConfig>,
    /// Peer 分析服务（上游 `SessionAnalyseServiceModule` / `SwarmTrackingModule` /
    /// `PeerRecordingServiceModule` 共用的配置节 `peer-analyse-service`）。**不参与 peer 判定**。
    #[serde(rename = "peer-analyse-service", default)]
    pub peer_analyse_service: Option<PeerAnalyseServiceConfig>,
}

/// `module.ptr-blacklist`（上游默认关闭）。
///
/// 注意：上游 `registerModules()` 中该模块的注册被注释掉（`//moduleClasses.add(PTRBlacklist.class);`），
/// 因此即使 profile.yml 打开也不会生效；本实现按「显式启用即注册」处理，
/// 并在 SPEC §5.9 中标注该差异。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtrBlacklistConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default = "default_ptr_ban_duration",
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
    #[serde(rename = "ptr-rules", default = "default_ptr_rules")]
    pub ptr_rules: Vec<String>,
    /// PTR 预热使用的 DNS 服务器（本实现新增；为空则不预热，模块恒不命中）
    #[serde(rename = "dns-servers", default)]
    pub dns_servers: Vec<String>,
}

fn default_ptr_ban_duration() -> i64 {
    259_200_000
}

fn default_ptr_rules() -> Vec<String> {
    vec![r#"{"method":"EQUALS","content":"example.com"}"#.to_string()]
}

/// `module.idle-connection-dos-protection`（上游默认关闭）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdleProtectionConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default = "default_idle_ban_duration",
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
    #[serde(rename = "max-allowed-idle-time", default = "default_idle_max")]
    pub max_allowed_idle_time_ms: i64,
    #[serde(rename = "idle-speed-threshold", default = "default_idle_speed")]
    pub idle_speed_threshold: i64,
    #[serde(
        rename = "min-status-change-percentage",
        default = "default_idle_status_change"
    )]
    pub min_status_change_percentage: f64,
    #[serde(rename = "reset-on-status-change", default = "default_true")]
    pub reset_on_status_change: bool,
    #[serde(rename = "protect-mode", default)]
    pub protect_mode: i64,
}

fn default_idle_ban_duration() -> i64 {
    900_000
}
fn default_idle_max() -> i64 {
    300_000
}
fn default_idle_speed() -> i64 {
    64
}
fn default_idle_status_change() -> f64 {
    0.001
}

/// `module.expression-engine`（默认值对齐上游 `profile.yml`）。
///
/// 上游默认 `enabled: true`、`ban-duration: default`（即 0，回退全局）；
/// 脚本取自 `<data>/scripts/*.av`（AviatorScript，本实现用 rhai 等价执行）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExpressionEngineConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default,
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
    /// 显式指定脚本目录（默认 `<data>/scripts`）；留空则按 `PBH_DATA_DIR` 解析
    #[serde(rename = "scripts-dir", default)]
    pub scripts_dir: Option<String>,
}

/// `module.ip-address-blocker-rules`（默认值对齐上游 `profile.yml`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IpRuleListConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default = "default_ip_rule_ban_duration",
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
    /// 检查（刷新）间隔，毫秒；上游 profile.yml 默认 14400000（4 小时），代码内默认 86400000
    #[serde(rename = "check-interval", default = "default_ip_rule_check_interval")]
    pub check_interval_ms: i64,
    #[serde(rename = "preload-banlist", default)]
    pub preload_banlist: bool,
    #[serde(default)]
    pub rules: std::collections::BTreeMap<String, IpRuleSubscriptionConfig>,
}

impl IpRuleListConfig {
    /// 是否启用规则订阅（缺省 `false`，对齐上游 `getBoolean` 缺省语义）。
    pub fn enabled(&self) -> bool {
        enabled_or_disabled(&self.enabled)
    }
}

/// 单条订阅配置（键为 ruleId）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpRuleSubscriptionConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: String,
}

fn default_ip_rule_ban_duration() -> i64 {
    259_200_000
}

fn default_ip_rule_check_interval() -> i64 {
    14_400_000
}

/// `module.anti-vampire`（默认值对齐上游 `profile.yml`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntiVampireConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default = "default_anti_vampire_duration",
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
    #[serde(default)]
    pub presets: AntiVampirePresets,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AntiVampirePresets {
    #[serde(default)]
    pub xunlei: AntiVampireXunleiPreset,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AntiVampireXunleiPreset {
    /// 上游 `getBoolean` 缺省为 false（`profile.yml` 显式写了 true）
    #[serde(default)]
    pub enabled: bool,
}

fn default_anti_vampire_duration() -> i64 {
    14_400_000
}

/// `module.multi-dialing-blocker`（默认值对齐上游 `profile.yml`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiDialingBlockerConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default,
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
    #[serde(rename = "subnet-mask-length", default = "default_mdb_subnet_v4")]
    pub subnet_mask_length: u8,
    #[serde(rename = "subnet-mask-v6-length", default = "default_mdb_subnet_v6")]
    pub subnet_mask_v6_length: u8,
    #[serde(rename = "tolerate-num-ipv4", default = "default_mdb_tolerate_v4")]
    pub tolerate_num_ipv4: i64,
    #[serde(rename = "tolerate-num-ipv6", default = "default_mdb_tolerate_v6")]
    pub tolerate_num_ipv6: i64,
    /// 秒
    #[serde(rename = "cache-lifespan", default = "default_mdb_cache_lifespan")]
    pub cache_lifespan_secs: i64,
    #[serde(rename = "keep-hunting", default)]
    pub keep_hunting: bool,
    /// 秒（上游默认 2592000 = 30 天）
    #[serde(
        rename = "keep-hunting-time",
        default = "default_mdb_keep_hunting_time"
    )]
    pub keep_hunting_time_secs: i64,
}

impl MultiDialingBlockerConfig {
    fn to_settings(&self) -> crate::modules::MultiDialingSettings {
        crate::modules::MultiDialingSettings {
            ban_duration_ms: self.ban_duration_ms,
            subnet_mask_length: self.subnet_mask_length,
            subnet_mask_v6_length: self.subnet_mask_v6_length,
            tolerate_num_ipv4: self.tolerate_num_ipv4,
            tolerate_num_ipv6: self.tolerate_num_ipv6,
            cache_lifespan_ms: self.cache_lifespan_secs * 1000,
            keep_hunting: self.keep_hunting,
            keep_hunting_time_ms: self.keep_hunting_time_secs * 1000,
        }
    }
}

fn default_mdb_subnet_v4() -> u8 {
    24
}
fn default_mdb_subnet_v6() -> u8 {
    56
}
fn default_mdb_tolerate_v4() -> i64 {
    2
}
fn default_mdb_tolerate_v6() -> i64 {
    5
}
fn default_mdb_cache_lifespan() -> i64 {
    86_400
}
fn default_mdb_keep_hunting_time() -> i64 {
    2_592_000
}

/// `module.auto-range-ban`（默认值对齐上游 `profile.yml`）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoRangeBanConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default,
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
    #[serde(default = "default_arb_ipv4")]
    pub ipv4: u8,
    #[serde(default = "default_arb_ipv6")]
    pub ipv6: u8,
}

fn default_arb_ipv4() -> u8 {
    30
}
fn default_arb_ipv6() -> u8 {
    48
}

fn enabled_or_disabled(v: &Option<bool>) -> bool {
    // 上游：节存在但缺 `enabled` 键时 getBoolean 抛异常 -> 记为禁用
    v.unwrap_or(false)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerIdBlacklistConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default,
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
    #[serde(rename = "banned-peer-id", default = "default_banned_peer_id")]
    pub banned_peer_id: Vec<String>,
}

fn default_banned_peer_id() -> Vec<String> {
    defaults::DEFAULT_BANNED_PEER_ID
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientNameBlacklistConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default,
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
    #[serde(rename = "banned-client-name", default = "default_banned_client_name")]
    pub banned_client_name: Vec<String>,
}

fn default_banned_client_name() -> Vec<String> {
    defaults::DEFAULT_BANNED_CLIENT_NAME
        .iter()
        .map(|s| s.to_string())
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpAddressBlockerConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default,
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
    #[serde(default)]
    pub ips: Vec<String>,
    #[serde(default)]
    pub ports: Vec<u16>,
    /// 按 ASN 封禁（对齐上游 `IPBlackList#reloadConfig` 的 `asns`，需要 GeoIP-ASN 数据库）
    #[serde(default, deserialize_with = "de_lenient_i64_list")]
    pub asns: Vec<i64>,
    /// 按国家/地区 ISO 代码封禁（大小写敏感，需要 GeoIP-City 数据库）
    #[serde(default)]
    pub regions: Vec<String>,
    /// 按城市/区/县封禁（`contains` 匹配）
    #[serde(default)]
    pub cities: Vec<String>,
    /// 按网络类型封禁（仅中国大陆地区 IP 有效，需要 GeoCN 数据库）
    #[serde(rename = "net-type", default)]
    pub net_type: NetTypeConfig,
}

impl IpAddressBlockerConfig {
    /// 转成运行期 GeoIP 维度配置（`net-type` 的两种形态统一为上游 `reloadConfig` 的 camelCase 集合）。
    pub fn to_geo_config(&self) -> GeoIpConfig {
        GeoIpConfig::new(
            self.asns.iter().copied(),
            self.regions.clone(),
            self.cities.clone(),
            self.net_type.to_tokens(),
        )
    }
}

/// 上游 `profile.yml` 把 ASN 写成字符串（`asns: - "0"`），而 Bukkit `getLongList` 取的是数字。
/// 这里两种形态都接受，无法解析的项忽略（对齐 `getLongList` 跳过非法元素的语义）。
fn de_lenient_i64_list<'de, D>(deserializer: D) -> Result<Vec<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<serde_yaml::Value>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .filter_map(|value| match value {
            serde_yaml::Value::Number(number) => number.as_i64(),
            serde_yaml::Value::String(text) => text.trim().parse::<i64>().ok(),
            _ => None,
        })
        .collect())
}

/// `module.ip-address-blocker.net-type`。
///
/// 上游有两种形态，而 `IPBlackList#reloadConfig` 读取的始终是 `Set<String>`（camelCase 标志）：
/// - `profile.yml` 现形态：kebab 布尔开关（`wideband: false` …）；
/// - `ProfileUpdateScript#ipAddressBlockerNetType`(v29) 迁移后的形态：camelCase 字符串列表。
///
/// 本实现两种都接受，并在 [`NetTypeConfig::to_tokens`] 中完成等价于 v29 迁移脚本的换算。
#[derive(Debug, Clone)]
pub enum NetTypeConfig {
    /// camelCase 字符串列表（上游迁移后的运行形态）
    List(Vec<String>),
    /// kebab 布尔开关映射（`profile.yml` 现形态）
    Flags(NetTypeFlags),
}

impl Default for NetTypeConfig {
    fn default() -> Self {
        NetTypeConfig::List(Vec::new())
    }
}

impl NetTypeConfig {
    /// 对齐 `ProfileUpdateScript#ipAddressBlockerNetType`(v29)：kebab 开关 → camelCase 列表。
    pub fn to_tokens(&self) -> Vec<String> {
        match self {
            NetTypeConfig::List(list) => list
                .iter()
                .map(|item| item.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            NetTypeConfig::Flags(flags) => {
                let mut tokens = Vec::new();
                if flags.wideband {
                    tokens.push(NET_TYPE_WIDEBAND.to_string());
                }
                if flags.base_station {
                    tokens.push(NET_TYPE_BASE_STATION.to_string());
                }
                if flags.government_and_enterprise_line {
                    tokens.push(NET_TYPE_GOVERNMENT_AND_ENTERPRISE_LINE.to_string());
                }
                if flags.business_platform {
                    tokens.push(NET_TYPE_BUSINESS_PLATFORM.to_string());
                }
                if flags.backbone_network {
                    tokens.push(NET_TYPE_BACKBONE_NETWORK.to_string());
                }
                if flags.ip_private_network {
                    tokens.push(NET_TYPE_IP_PRIVATE_NETWORK.to_string());
                }
                if flags.internet_cafe {
                    tokens.push(NET_TYPE_INTERNET_CAFE.to_string());
                }
                if flags.iot {
                    tokens.push(NET_TYPE_IOT.to_string());
                }
                if flags.datacenter {
                    tokens.push(NET_TYPE_DATACENTER.to_string());
                }
                tokens
            }
        }
    }
}

impl Serialize for NetTypeConfig {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            NetTypeConfig::List(list) => list.serialize(serializer),
            NetTypeConfig::Flags(flags) => flags.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for NetTypeConfig {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            List(Vec<String>),
            Flags(NetTypeFlags),
        }
        // `net-type:` 为空值（null）时按「未启用任何网络类型」处理
        Ok(match Option::<Raw>::deserialize(deserializer)? {
            None => NetTypeConfig::List(Vec::new()),
            Some(Raw::List(list)) => NetTypeConfig::List(list),
            Some(Raw::Flags(flags)) => NetTypeConfig::Flags(flags),
        })
    }
}

/// `profile.yml` 现形态的 9 个 kebab 开关（键名逐字对齐上游 `profile.yml` 的 `net-type:` 段）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetTypeFlags {
    #[serde(default)]
    pub wideband: bool,
    #[serde(rename = "base-station", default)]
    pub base_station: bool,
    #[serde(rename = "government-and-enterprise-line", default)]
    pub government_and_enterprise_line: bool,
    #[serde(rename = "business-platform", default)]
    pub business_platform: bool,
    #[serde(rename = "backbone-network", default)]
    pub backbone_network: bool,
    #[serde(rename = "ip-private-network", default)]
    pub ip_private_network: bool,
    #[serde(rename = "internet-cafe", default)]
    pub internet_cafe: bool,
    #[serde(default)]
    pub iot: bool,
    #[serde(default)]
    pub datacenter: bool,
}

/// `config.yml` 的 `ip-database` 段（对齐上游 `IPDBManager#setupIPDB` 读取的键与默认值）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IpDatabaseConfig {
    #[serde(rename = "account-id", default)]
    pub account_id: String,
    #[serde(rename = "license-key", default)]
    pub license_key: String,
    #[serde(rename = "database-city", default = "default_database_city")]
    pub database_city: String,
    #[serde(rename = "database-asn", default = "default_database_asn")]
    pub database_asn: String,
    #[serde(rename = "database-geocn", default = "default_database_geocn")]
    pub database_geocn: String,
    /// 上游 `getBoolean("ip-database.auto-update")` 缺省为 false（`config.yml` 内写的是 true）
    #[serde(rename = "auto-update", default)]
    pub auto_update: bool,
}

impl Default for IpDatabaseConfig {
    fn default() -> Self {
        Self {
            account_id: String::new(),
            license_key: String::new(),
            database_city: default_database_city(),
            database_asn: default_database_asn(),
            database_geocn: default_database_geocn(),
            auto_update: false,
        }
    }
}

fn default_database_city() -> String {
    "GeoLite2-City".to_string()
}
fn default_database_asn() -> String {
    "GeoLite2-ASN".to_string()
}
fn default_database_geocn() -> String {
    "GeoCN".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressCheatBlockerConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default = "default_pcb_ban_duration",
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
    #[serde(rename = "minimum-size", default = "default_pcb_minimum_size")]
    pub minimum_size: i64,
    #[serde(rename = "block-excessive-clients", default = "default_true")]
    pub block_excessive_clients: bool,
    #[serde(
        rename = "excessive-threshold",
        default = "default_pcb_excessive_threshold"
    )]
    pub excessive_threshold: f64,
    #[serde(
        rename = "maximum-difference",
        default = "default_pcb_maximum_difference"
    )]
    pub maximum_difference: f64,
    #[serde(
        rename = "rewind-maximum-difference",
        default = "default_pcb_rewind_maximum_difference"
    )]
    pub rewind_maximum_difference: f64,
    #[serde(rename = "ipv4-prefix-length", default = "default_pcb_ipv4_prefix")]
    pub ipv4_prefix_length: u8,
    #[serde(rename = "ipv6-prefix-length", default = "default_pcb_ipv6_prefix")]
    pub ipv6_prefix_length: u8,
    #[serde(rename = "persist-duration", default = "default_pcb_persist_duration")]
    pub persist_duration_ms: i64,
    #[serde(rename = "max-wait-duration", default = "default_pcb_max_wait")]
    pub max_wait_duration_ms: i64,
    #[serde(
        rename = "fast-pcb-test-percentage",
        default = "default_pcb_fast_percentage"
    )]
    pub fast_pcb_test_percentage: f64,
    #[serde(
        rename = "fast-pcb-test-block-duration",
        default = "default_pcb_fast_block_duration"
    )]
    pub fast_pcb_test_block_duration_ms: i64,
    /// `enable-persist`：是否把 PCB 状态落库（上游默认 true）
    #[serde(rename = "enable-persist", default = "default_true")]
    pub enable_persist: bool,
}

fn default_pcb_ban_duration() -> i64 {
    defaults::PCB_BAN_DURATION_MS
}
fn default_pcb_minimum_size() -> i64 {
    pcb::MINIMUM_SIZE
}
fn default_pcb_excessive_threshold() -> f64 {
    pcb::EXCESSIVE_THRESHOLD
}
fn default_pcb_maximum_difference() -> f64 {
    pcb::MAXIMUM_DIFFERENCE
}
fn default_pcb_rewind_maximum_difference() -> f64 {
    pcb::REWIND_MAXIMUM_DIFFERENCE
}
fn default_pcb_ipv4_prefix() -> u8 {
    pcb::IPV4_PREFIX_LENGTH
}
fn default_pcb_ipv6_prefix() -> u8 {
    pcb::IPV6_PREFIX_LENGTH
}
fn default_pcb_persist_duration() -> i64 {
    pcb::PERSIST_DURATION_MS
}
fn default_pcb_max_wait() -> i64 {
    pcb::MAX_WAIT_DURATION_MS
}
fn default_pcb_fast_percentage() -> f64 {
    pcb::FAST_PCB_TEST_PERCENTAGE
}
fn default_pcb_fast_block_duration() -> i64 {
    pcb::FAST_PCB_TEST_BLOCK_DURATION_MS
}

impl ProgressCheatBlockerConfig {
    fn to_pcb_config(&self) -> PcbConfig {
        PcbConfig {
            torrent_minimum_size: self.minimum_size,
            block_excessive_clients: self.block_excessive_clients,
            excessive_threshold: self.excessive_threshold,
            maximum_difference: self.maximum_difference,
            rewind_maximum_difference: self.rewind_maximum_difference,
            ipv4_prefix_length: self.ipv4_prefix_length,
            ipv6_prefix_length: self.ipv6_prefix_length,
            ban_duration_ms: self.ban_duration_ms,
            max_wait_duration_ms: self.max_wait_duration_ms,
            fast_pcb_test_percentage: self.fast_pcb_test_percentage,
            fast_pcb_test_block_duration_ms: self.fast_pcb_test_block_duration_ms,
            persist_enabled: self.enable_persist,
            persist_duration_ms: self.persist_duration_ms,
        }
    }
}

/// `module.btn`（上游 `BtnNetworkOnline`，配置键 `btn`）。
///
/// 上游 `profile.yml` 默认为 `enabled: true` / `ban-duration: 259200000`（3 天）；
/// 与其他模块一致，节内**缺少** `enabled` 键时按 `AbstractFeatureModule.shouldModuleEnabled()`
/// 语义视为禁用（[`enabled_or_disabled`]）。注意本模块的规则全部来自 BTN 服务器，
/// 未配置 BTN 服务器时即使启用也恒 pass、绝不封禁。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtnConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "ban-duration",
        default = "default_btn_ban_duration",
        deserialize_with = "deserialize_ban_duration"
    )]
    pub ban_duration_ms: i64,
}

impl Default for BtnConfig {
    fn default() -> Self {
        Self {
            enabled: Some(true),
            ban_duration_ms: default_btn_ban_duration(),
        }
    }
}

fn default_btn_ban_duration() -> i64 {
    crate::modules::btn::BTN_BAN_DURATION_MS
}

/// `module.active-monitoring`（对齐上游 `profile.yml` 第 381-405 行与
/// `ActiveMonitoringModule#reloadConfig` 读取的键）。
///
/// 注意：该模块**不参与 peer 判定**（上游非 `RuleFeatureModule`），
/// 由应用层按定时任务驱动，接入方式见 `modules/monitor.rs` 头部的 INTEGRATION SNIPPET。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ActiveMonitoringConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(rename = "traffic-monitoring", default)]
    pub traffic_monitoring: TrafficMonitoringConfig,
    #[serde(rename = "traffic-sliding-capping", default)]
    pub traffic_sliding_capping: TrafficSlidingCappingConfig,
}

impl ActiveMonitoringConfig {
    pub fn to_settings(&self) -> crate::modules::ActiveMonitoringSettings {
        crate::modules::ActiveMonitoringSettings {
            daily_traffic_capping: self.traffic_monitoring.daily,
            use_traffic_sliding_capping: self.traffic_sliding_capping.enabled,
            max_traffic_allowed_in_window_period: self
                .traffic_sliding_capping
                .daily_max_allowed_upload_traffic,
            traffic_sliding_capping_max_speed: self.traffic_sliding_capping.max_speed,
            traffic_sliding_capping_min_speed: self.traffic_sliding_capping.min_speed,
        }
    }
}

/// `module.active-monitoring.traffic-monitoring`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrafficMonitoringConfig {
    /// 每日上行流量阈值（bytes）；`getLong("traffic-monitoring.daily", -1)`，-1 禁用
    #[serde(default = "default_daily_traffic_capping")]
    pub daily: i64,
}

impl Default for TrafficMonitoringConfig {
    fn default() -> Self {
        Self {
            daily: default_daily_traffic_capping(),
        }
    }
}

fn default_daily_traffic_capping() -> i64 {
    -1
}

/// `module.active-monitoring.traffic-sliding-capping`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrafficSlidingCappingConfig {
    /// `getBoolean("traffic-sliding-capping.enabled")`（缺键 -> false）
    #[serde(default)]
    pub enabled: bool,
    /// **Java 实际读取**的键：`traffic-sliding-capping.daily-max-allowed-upload-traffic`
    /// （`getLong` 缺键 -> 0）。
    #[serde(rename = "daily-max-allowed-upload-traffic", default)]
    pub daily_max_allowed_upload_traffic: i64,
    /// 上游 `profile.yml` 第 399 行写的是 `max-allowed-upload-traffic`，但
    /// `ActiveMonitoringModule#reloadConfig` **从不读取**该键（缺键 -> 0）。
    /// 这里仅保留解析以便锁定该上游差异，不参与 `to_settings()`。
    #[serde(rename = "max-allowed-upload-traffic", default)]
    pub profile_yml_max_allowed_upload_traffic: Option<i64>,
    #[serde(rename = "max-speed", default)]
    pub max_speed: i64,
    #[serde(rename = "min-speed", default)]
    pub min_speed: i64,
}

/// `module.peer-analyse-service`（对齐上游 `profile.yml` 第 458-483 行）。
///
/// 三个子模块都不参与 peer 判定（上游非 `RuleFeatureModule`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PeerAnalyseServiceConfig {
    #[serde(rename = "session-analyse", default)]
    pub session_analyse: Option<SessionAnalyseConfig>,
    #[serde(rename = "swarm-tracking", default)]
    pub swarm_tracking: Option<SwarmTrackingConfig>,
    #[serde(rename = "peer-recording", default)]
    pub peer_recording: Option<PeerRecordingConfig>,
}

/// `module.peer-analyse-service.session-analyse`。
///
/// 上游 `onEnable` 用 `getLong(key)`（无默认值 -> 缺键为 0）读取 `cleanup-interval`
/// 与 `data-flush-interval`，`reloadConfig()` 同样以无默认值方式读取 `data-retention-time`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionAnalyseConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `data-flush-interval`（`profile.yml` 默认 3600000）
    #[serde(rename = "data-flush-interval", default)]
    pub data_flush_interval_ms: i64,
    /// `cleanup-interval`（`profile.yml` 默认 3600000）
    #[serde(rename = "cleanup-interval", default)]
    pub cleanup_interval_ms: i64,
    /// `data-retention-time`（`profile.yml` 默认 15552000000）
    #[serde(rename = "data-retention-time", default)]
    pub data_retention_time_ms: i64,
}

impl SessionAnalyseConfig {
    pub fn to_settings(&self) -> crate::modules::SessionAnalyseSettings {
        crate::modules::SessionAnalyseSettings {
            cleanup_interval_ms: self.cleanup_interval_ms,
            data_flush_interval_ms: self.data_flush_interval_ms,
            data_retention_time_ms: self.data_retention_time_ms,
        }
    }
}

/// `module.peer-analyse-service.swarm-tracking`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SwarmTrackingConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// `data-flush-interval`（`getLong` 无默认值 -> 缺键 0；`profile.yml` 写 3600000）
    #[serde(rename = "data-flush-interval", default)]
    pub data_flush_interval_ms: i64,
}

impl SwarmTrackingConfig {
    pub fn to_settings(&self) -> crate::modules::SwarmTrackingSettings {
        crate::modules::SwarmTrackingSettings {
            data_flush_interval_ms: self.data_flush_interval_ms,
        }
    }
}

/// `module.peer-analyse-service.peer-recording`。
///
/// 上游三个键都带代码级默认值：`data-retention-time: -1`、`data-cleanup-interval: -1`、
/// `data-flush-interval: 20000`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerRecordingConfig {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(
        rename = "data-retention-time",
        default = "default_peer_recording_retention"
    )]
    pub data_retention_time_ms: i64,
    #[serde(
        rename = "data-cleanup-interval",
        default = "default_peer_recording_cleanup"
    )]
    pub data_cleanup_interval_ms: i64,
    #[serde(
        rename = "data-flush-interval",
        default = "default_peer_recording_flush"
    )]
    pub data_flush_interval_ms: i64,
}

impl Default for PeerRecordingConfig {
    fn default() -> Self {
        Self {
            enabled: None,
            data_retention_time_ms: default_peer_recording_retention(),
            data_cleanup_interval_ms: default_peer_recording_cleanup(),
            data_flush_interval_ms: default_peer_recording_flush(),
        }
    }
}

impl PeerRecordingConfig {
    pub fn to_settings(&self) -> crate::modules::PeerRecordingSettings {
        crate::modules::PeerRecordingSettings {
            data_retention_time_ms: self.data_retention_time_ms,
            data_cleanup_interval_ms: self.data_cleanup_interval_ms,
            data_flush_interval_ms: self.data_flush_interval_ms,
        }
    }
}

fn default_peer_recording_retention() -> i64 {
    -1
}
fn default_peer_recording_cleanup() -> i64 {
    -1
}
fn default_peer_recording_flush() -> i64 {
    20_000
}

impl ProfileConfig {
    /// 按上游模块注册顺序构建流水线：
    /// `IPBlackList → PeerIdBlacklist → ClientNameBlacklist → ProgressCheatBlocker`
    /// （见 `PeerBanHelper.registerModules()`）。
    ///
    /// 配置节缺失 / `enabled` 非 true 的模块会被跳过；
    /// `ban-duration` 为 0 的模块在判定时回退到全局 `ban-duration`。
    pub fn build_pipeline(&self) -> Pipeline {
        self.build_pipeline_with_geo(None)
    }

    /// 同 [`ProfileConfig::build_pipeline`]，但注入 GeoIP 数据源
    /// （对齐上游 `@Autowired IPDBManager`）。
    ///
    /// `None`（数据库缺失/损坏/被 `pbh.forceDisableIPDB` 关闭）时
    /// `ip-address-blocker` 的 ASN / 地区 / 城市 / 网络类型四个维度全部不命中。
    pub fn build_pipeline_with_geo(
        &self,
        geo_provider: Option<std::sync::Arc<dyn GeoIpProvider>>,
    ) -> Pipeline {
        let mut pipeline = Pipeline {
            modules: Vec::new(),
            ignore: IpSet::from_cidrs(self.ignore_peers_from_addresses.iter().map(|s| s.as_str())),
            global_ban_duration_ms: self.ban_duration,
            // 封禁表由 wave 维护（启动恢复、每轮解封），模块共享同一实例
            ban_list: std::sync::Arc::new(std::sync::Mutex::new(crate::banlist::BanList::new())),
        };
        if let Some(cfg) = &self.module.ip_address_blocker {
            if enabled_or_disabled(&cfg.enabled) {
                pipeline.add_module(Box::new(
                    IpBlacklist::with_geo(
                        &cfg.ips,
                        &cfg.ports,
                        cfg.ban_duration_ms,
                        cfg.to_geo_config(),
                    )
                    .with_provider(geo_provider),
                ));
            }
        }
        if let Some(cfg) = &self.module.peer_id_blacklist {
            if enabled_or_disabled(&cfg.enabled) {
                if let Ok(rules) = RuleSet::from_json_text(&cfg.banned_peer_id) {
                    pipeline.add_module(Box::new(StringBlacklist::peer_id_with(
                        rules,
                        cfg.ban_duration_ms,
                    )));
                }
            }
        }
        if let Some(cfg) = &self.module.client_name_blacklist {
            if enabled_or_disabled(&cfg.enabled) {
                if let Ok(rules) = RuleSet::from_json_text(&cfg.banned_client_name) {
                    pipeline.add_module(Box::new(StringBlacklist::client_name_with(
                        rules,
                        cfg.ban_duration_ms,
                    )));
                }
            }
        }
        // 对齐上游 registerModules 顺序：expression-engine 在 client-name 之后、progress-cheat 之前
        if let Some(cfg) = &self.module.expression_engine {
            if enabled_or_disabled(&cfg.enabled) {
                pipeline.add_module(Box::new(ExpressionEngine::new(
                    cfg.ban_duration_ms,
                    cfg.scripts_dir.as_deref(),
                )));
            }
        }
        if let Some(cfg) = &self.module.progress_cheat_blocker {
            if enabled_or_disabled(&cfg.enabled) {
                pipeline.add_module(Box::new(ProgressCheatBlocker::new(cfg.to_pcb_config())));
            }
        }
        if let Some(cfg) = &self.module.multi_dialing_blocker {
            if enabled_or_disabled(&cfg.enabled) {
                pipeline.add_module(Box::new(MultiDialingBlocker::new(cfg.to_settings())));
            }
        }
        if let Some(cfg) = &self.module.auto_range_ban {
            if enabled_or_disabled(&cfg.enabled) {
                let ban_list = pipeline.ban_list.clone();
                pipeline.add_module(Box::new(AutoRangeBan::new(
                    cfg.ipv4,
                    cfg.ipv6,
                    cfg.ban_duration_ms,
                    ban_list,
                )));
            }
        }
        // 对齐上游 registerModules 顺序：AutoRangeBan -> BtnNetworkOnline -> IPBlackRuleList
        // 注意：规则全部由 BTN 服务器下发，未配置 BTN 客户端的部署里本模块恒 `pass()`、绝不封禁。
        if let Some(cfg) = &self.module.btn {
            if enabled_or_disabled(&cfg.enabled) {
                pipeline.add_module(Box::new(BtnNetworkOnline::new(cfg.ban_duration_ms)));
            }
        }
        if let Some(cfg) = &self.module.ip_rule_list {
            if enabled_or_disabled(&cfg.enabled) {
                pipeline.add_module(Box::new(IpRuleListModule::new(cfg.ban_duration_ms)));
            }
        }
        // 上游 `registerModules()` 中 `//moduleClasses.add(PTRBlacklist.class);` 被注释掉，
        // 因此**即使 profile.yml 打开该模块也不会生效**；本移植同样不注册（配置仍可解析，
        // 保证与上游配置文件互通），仅提示一次以免使用者误以为已生效。
        if let Some(cfg) = &self.module.ptr_blacklist {
            if enabled_or_disabled(&cfg.enabled) {
                tracing::warn!(
                    "ptr-blacklist 已在 profile.yml 启用，但上游 v9.5.1 的 registerModules() 未注册该模块，\
                     本移植按上游行为跳过（不参与判定）"
                );
            }
        }

        if let Some(cfg) = &self.module.idle_connection_dos_protection {
            if enabled_or_disabled(&cfg.enabled) {
                pipeline.add_module(Box::new(IdleConnectionDosProtection::new(
                    IdleProtectionSettings {
                        ban_duration_ms: cfg.ban_duration_ms,
                        max_allowed_idle_time_ms: cfg.max_allowed_idle_time_ms,
                        idle_speed_threshold: cfg.idle_speed_threshold,
                        min_status_change_percentage: cfg.min_status_change_percentage,
                        reset_on_status_change: cfg.reset_on_status_change,
                        protect_mode: ProtectionMode::from_code(cfg.protect_mode),
                    },
                )));
            }
        }
        if let Some(cfg) = &self.module.anti_vampire {
            if enabled_or_disabled(&cfg.enabled) {
                pipeline.add_module(Box::new(AntiVampire::new(AntiVampireSettings {
                    ban_duration_ms: cfg.ban_duration_ms,
                    xunlei_preset: cfg.presets.xunlei.enabled,
                })));
            }
        }
        pipeline
    }

    /// 按上游 `PeerBanHelper.registerModules()` 的顺序构造监控模块：
    /// `ActiveMonitoring → SwarmTracking → SessionAnalyse → PeerRecording`。
    ///
    /// 上游 `RunCheckModuleOrgan` 只对 `RuleFeatureModule` 调 `shouldBanPeer`，这四个模块
    /// 都不是 `RuleFeatureModule`，因此**从不参与 peer 判定**；忠实做法是把它们交给应用层
    /// 调度器（不加进 [`Pipeline`]）。它们的 `check` 恒 `pass()`，加进去也不影响任何裁决。
    ///
    /// `sink` 由应用层注入：pbh-core 不持有数据库，默认可用
    /// [`crate::modules::InMemoryMonitorSink`]（DB 版实现见 `modules/monitor.rs` 头部清单）。
    pub fn build_monitor_modules(
        &self,
        sink: std::sync::Arc<dyn MonitorSink>,
    ) -> Vec<Box<dyn RuleModule>> {
        let mut modules: Vec<Box<dyn RuleModule>> = Vec::new();
        if let Some(cfg) = &self.module.active_monitoring {
            if enabled_or_disabled(&cfg.enabled) {
                modules.push(Box::new(ActiveMonitoringModule::new(
                    sink.clone(),
                    cfg.to_settings(),
                )));
            }
        }
        if let Some(cfg) = &self.module.peer_analyse_service {
            if let Some(sub) = &cfg.swarm_tracking {
                if enabled_or_disabled(&sub.enabled) {
                    modules.push(Box::new(SwarmTrackingModule::new(
                        sink.clone(),
                        sub.to_settings(),
                    )));
                }
            }
            if let Some(sub) = &cfg.session_analyse {
                if enabled_or_disabled(&sub.enabled) {
                    modules.push(Box::new(SessionAnalyseServiceModule::new(
                        sink.clone(),
                        sub.to_settings(),
                    )));
                }
            }
            if let Some(sub) = &cfg.peer_recording {
                if enabled_or_disabled(&sub.enabled) {
                    modules.push(Box::new(PeerRecordingServiceModule::new(
                        sink.clone(),
                        sub.to_settings(),
                    )));
                }
            }
        }
        modules
    }
}

/// 便于静态断言模块顺序的小工具（供调用方/测试使用）。
pub fn module_config_names(pipeline: &Pipeline) -> Vec<&str> {
    pipeline.modules.iter().map(|m| m.config_name()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_disables_all_modules() {
        // 上游：profile.yml 中缺少 module 节时全部规则模块禁用
        let p = ProfileConfig::default().build_pipeline();
        assert!(p.modules.is_empty());
    }

    #[test]
    fn module_order_follows_upstream_registration() {
        let mut cfg = ProfileConfig::default();
        cfg.module.peer_id_blacklist = Some(PeerIdBlacklistConfig {
            enabled: Some(true),
            ban_duration_ms: 0,
            banned_peer_id: default_banned_peer_id(),
        });
        cfg.module.progress_cheat_blocker = Some(ProgressCheatBlockerConfig {
            enabled: Some(true),
            ban_duration_ms: 0,
            minimum_size: pcb::MINIMUM_SIZE,
            block_excessive_clients: true,
            excessive_threshold: pcb::EXCESSIVE_THRESHOLD,
            maximum_difference: pcb::MAXIMUM_DIFFERENCE,
            rewind_maximum_difference: pcb::REWIND_MAXIMUM_DIFFERENCE,
            ipv4_prefix_length: pcb::IPV4_PREFIX_LENGTH,
            ipv6_prefix_length: pcb::IPV6_PREFIX_LENGTH,
            persist_duration_ms: pcb::PERSIST_DURATION_MS,
            max_wait_duration_ms: pcb::MAX_WAIT_DURATION_MS,
            fast_pcb_test_percentage: pcb::FAST_PCB_TEST_PERCENTAGE,
            fast_pcb_test_block_duration_ms: pcb::FAST_PCB_TEST_BLOCK_DURATION_MS,
            enable_persist: true,
        });
        let p = cfg.build_pipeline();
        assert_eq!(
            module_config_names(&p),
            vec!["peer-id-blacklist", "progress-cheat-blocker"]
        );
    }

    /// 上游 `profile.yml` 的 `net-type:` 是 kebab 布尔开关映射，
    /// `ProfileUpdateScript#ipAddressBlockerNetType`(v29) 把它迁移成 camelCase 列表。
    #[test]
    fn net_type_flag_mapping_migrates_to_camel_case_tokens() {
        let cfg: ProfileConfig = serde_yaml::from_str(
            r#"
module:
  ip-address-blocker:
    enabled: true
    net-type:
      wideband: true
      base-station: false
      government-and-enterprise-line: true
      business-platform: false
      backbone-network: false
      ip-private-network: false
      internet-cafe: false
      iot: true
      datacenter: true
"#,
        )
        .expect("profile yaml");
        let tokens = cfg.module.ip_address_blocker.unwrap().net_type.to_tokens();
        assert_eq!(
            tokens,
            vec![
                "wideband",
                "governmentAndEnterpriseLine",
                "iot",
                "dataCenter"
            ]
        );
    }

    /// 迁移后的形态（camelCase 字符串列表）与空值都必须可解析
    #[test]
    fn net_type_list_and_null_forms_are_accepted() {
        let cfg: ProfileConfig = serde_yaml::from_str(
            r#"
module:
  ip-address-blocker:
    enabled: true
    net-type:
      - "wideband"
      - "datacenter"
"#,
        )
        .expect("profile yaml");
        let config = cfg.module.ip_address_blocker.unwrap();
        assert_eq!(config.net_type.to_tokens(), vec!["wideband", "datacenter"]);
        assert!(config.to_geo_config().network_type_hit("宽带"));
        // 上游 `case "数据中心", "IDC" -> contains("dataCenter") || contains("datacenter")`
        assert!(config.to_geo_config().network_type_hit("IDC"));

        let cfg: ProfileConfig = serde_yaml::from_str(
            r#"
module:
  ip-address-blocker:
    enabled: true
    net-type:
"#,
        )
        .expect("profile yaml");
        let config = cfg.module.ip_address_blocker.unwrap();
        assert!(config.net_type.to_tokens().is_empty());
        assert!(!config.to_geo_config().network_type_hit("宽带"));
    }

    /// 上游 `profile.yml` 把 ASN 写成字符串（`asns: - "0"`），数字形态同样要支持
    #[test]
    fn asn_list_accepts_strings_and_numbers() {
        let cfg: ProfileConfig = serde_yaml::from_str(
            r#"
module:
  ip-address-blocker:
    enabled: true
    asns:
      - "0"
      - 64512
    regions:
      - "CN"
    cities:
      - "示例海南"
"#,
        )
        .expect("profile yaml");
        let config = cfg.module.ip_address_blocker.unwrap();
        assert_eq!(config.asns, vec![0, 64512]);
        let geo = config.to_geo_config();
        assert!(geo.asns.contains(&0) && geo.asns.contains(&64512));
        assert!(geo.regions.contains("CN"));
        assert_eq!(geo.cities, vec!["示例海南".to_string()]);
        // 未启用任何网络类型
        assert!(geo.network_type.is_empty());
    }

    /// 默认配置节（未写任何 GeoIP 维度）⇒ 四个维度全部不命中
    #[test]
    fn missing_geo_dimensions_default_to_empty() {
        let cfg: ProfileConfig = serde_yaml::from_str(
            r#"
module:
  ip-address-blocker:
    enabled: true
    ips:
      - "1.2.3.0/24"
"#,
        )
        .expect("profile yaml");
        let geo = cfg.module.ip_address_blocker.unwrap().to_geo_config();
        assert!(geo.is_empty());
        assert!(!geo.network_type_hit("宽带"));
        assert!(!geo.network_type_hit("IDC"));
        assert!(!geo.network_type_hit("数据中心"));
    }

    /// 主配置 `ip-database` 段（`config.yml`）：键名与默认值对齐上游 `IPDBManager#setupIPDB`
    #[test]
    fn ip_database_section_matches_upstream_keys() {
        let cfg: IpDatabaseConfig = serde_yaml::from_str(
            r#"
auto-update: true
database-city: "GeoLite2-City"
database-asn: "GeoLite2-ASN"
database-geocn: "GeoCN"
"#,
        )
        .expect("ip-database yaml");
        assert_eq!(cfg.account_id, "");
        assert_eq!(cfg.license_key, "");
        assert_eq!(cfg.database_city, "GeoLite2-City");
        assert_eq!(cfg.database_asn, "GeoLite2-ASN");
        assert_eq!(cfg.database_geocn, "GeoCN");
        assert!(cfg.auto_update);

        // 上游 `getBoolean("ip-database.auto-update")` 缺省为 false（随包 config.yml 里写的是 true）
        let defaulted = IpDatabaseConfig::default();
        assert!(!defaulted.auto_update);
        assert_eq!(defaulted.database_city, "GeoLite2-City");
        assert_eq!(defaulted.database_asn, "GeoLite2-ASN");
        assert_eq!(defaulted.database_geocn, "GeoCN");

        // 缺失的键同样回落到上游默认值
        let partial: IpDatabaseConfig = serde_yaml::from_str("auto-update: true\n").unwrap();
        assert_eq!(partial.database_geocn, "GeoCN");
    }

    /// `module.btn` 段（上游 `profile.yml` 默认 `enabled: true` / `ban-duration: 259200000`）。
    ///
    /// 本模块已在 `build_pipeline` 中按上游 `registerModules()` 的位置实例化
    /// （`AutoRangeBan` 之后、`IPBlackRuleList` 之前）；规则全部来自 BTN 服务器，
    /// 未注入规则时恒 `pass()`。
    #[test]
    fn btn_section_matches_upstream_profile_yml() {
        let cfg: ProfileConfig = serde_yaml::from_str(
            r#"
module:
  btn:
    enabled: true
    ban-duration: 259200000
"#,
        )
        .expect("profile yaml");
        let btn = cfg.module.btn.expect("btn 段必须可解析");
        assert!(enabled_or_disabled(&btn.enabled));
        assert_eq!(btn.ban_duration_ms, 259_200_000);
        assert_eq!(
            btn.ban_duration_ms,
            crate::modules::btn::BTN_BAN_DURATION_MS
        );

        // 上游 profile.yml 的默认值；缺 `enabled` 键 -> 视为禁用（对齐 shouldModuleEnabled）
        let defaulted = BtnConfig::default();
        assert_eq!(defaulted.enabled, Some(true));
        assert_eq!(defaulted.ban_duration_ms, 259_200_000);
        let partial: BtnConfig = serde_yaml::from_str("ban-duration: 1000\n").unwrap();
        assert_eq!(partial.enabled, None);
        assert!(!enabled_or_disabled(&partial.enabled));

        // 未写 btn 段的配置不产生该模块
        let none = ProfileConfig::default();
        assert!(none.module.btn.is_none());
        assert!(none.build_pipeline().modules.is_empty());

        // 已启用 + 未注入任何 BTN 规则 -> 模块在位但恒 pass（绝不封禁）
        let mut enabled = ProfileConfig::default();
        enabled.module.auto_range_ban = Some(AutoRangeBanConfig {
            enabled: Some(true),
            ban_duration_ms: 0,
            ipv4: 30,
            ipv6: 48,
        });
        enabled.module.btn = Some(BtnConfig::default());
        enabled.module.ip_rule_list = Some(IpRuleListConfig {
            enabled: Some(true),
            ban_duration_ms: 0,
            check_interval_ms: 14_400_000,
            preload_banlist: false,
            rules: Default::default(),
        });
        let pipeline = enabled.build_pipeline();
        assert_eq!(
            module_config_names(&pipeline),
            vec!["auto-range-ban", "btn", "ip-address-blocker-rules"],
            "对齐上游 registerModules：AutoRangeBan -> BtnNetworkOnline -> IPBlackRuleList"
        );
        let btn = pipeline
            .module_as::<crate::modules::BtnNetworkOnline>("btn")
            .unwrap();
        assert!(!btn.is_manager_initialized(), "未握手 -> 未初始化");
        let torrent = crate::model::TorrentData {
            hash: "h".into(),
            name: "t".into(),
            progress: 0.5,
            total_size: 1_000_000,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 0,
            upspeed: 0,
            is_private: Some(false),
        };
        let peer = crate::model::PeerData {
            client_name: Some("qBittorrent/4.5.0".into()),
            peer_id: Some("-qB4500-xxxxxxxxxxxx".into()),
            dl_speed: 1000,
            downloaded: 1000,
            up_speed: 1000,
            uploaded: 1000,
            progress: 0.5,
            flags: Some("d u".into()),
            ip: "1.2.3.4".into(),
            port: 6881,
            raw_ip: "1.2.3.4:6881".into(),
            connection: Some("uTP".into()),
        };
        let ctx = crate::module::CheckContext {
            now_ms: 0,
            features: vec!["UNBAN_IP".into()],
        };
        assert_eq!(
            btn.check("d", &torrent, &peer, &ctx).action,
            crate::module::PeerAction::NoAction
        );
    }

    /// 监控模块由 `build_monitor_modules` 按上游 `registerModules()` 顺序构造，
    /// 且**不进入**判定流水线（上游 `RunCheckModuleOrgan` 只跑 `RuleFeatureModule`）。
    #[test]
    fn monitor_modules_are_built_outside_the_pipeline() {
        use crate::modules::InMemoryMonitorSink;

        let cfg: ProfileConfig = serde_yaml::from_str(
            r#"
module:
  active-monitoring:
    enabled: true
  peer-analyse-service:
    session-analyse:
      enabled: true
      data-flush-interval: 3600000
      cleanup-interval: 3600000
      data-retention-time: 15552000000
    swarm-tracking:
      enabled: true
      data-flush-interval: 3600000
    peer-recording:
      enabled: true
      data-flush-interval: 900000
      data-retention-time: 5184000000
      data-cleanup-interval: 604800000
"#,
        )
        .expect("profile yaml");

        let sink: std::sync::Arc<dyn MonitorSink> = std::sync::Arc::new(InMemoryMonitorSink::new());
        let modules = cfg.build_monitor_modules(sink);
        let names: Vec<&str> = modules.iter().map(|m| m.config_name()).collect();
        assert_eq!(
            names,
            vec![
                "active-monitoring",
                "peer-analyse-service.swarm-tracking",
                "peer-analyse-service.session-analyse",
                "peer-analyse-service.peer-recording"
            ],
            "对齐上游 registerModules：ActiveMonitoring -> SwarmTracking -> SessionAnalyse -> PeerRecording"
        );
        // 监控模块从不参与 peer 判定
        assert!(cfg.build_pipeline().modules.is_empty());

        // `enabled: false` / 配置节缺失 -> 不构造
        let disabled: ProfileConfig =
            serde_yaml::from_str("module:\n  active-monitoring:\n    enabled: false\n").unwrap();
        let sink: std::sync::Arc<dyn MonitorSink> = std::sync::Arc::new(InMemoryMonitorSink::new());
        assert!(disabled.build_monitor_modules(sink).is_empty());
        let sink: std::sync::Arc<dyn MonitorSink> = std::sync::Arc::new(InMemoryMonitorSink::new());
        assert!(ProfileConfig::default()
            .build_monitor_modules(sink)
            .is_empty());
    }

    /// `module.active-monitoring` 段（值逐字取自上游 `profile.yml` 第 381-405 行）。
    ///
    /// 本模块不在 `build_pipeline` 中实例化（监控模块不参与 peer 判定），
    /// 接入方式见 `modules/monitor.rs` 头部的 INTEGRATION SNIPPET。
    #[test]
    fn active_monitoring_section_matches_upstream_profile_yml() {
        let cfg: ProfileConfig = serde_yaml::from_str(
            r#"
module:
  active-monitoring:
    enabled: true
    traffic-monitoring:
      daily: -1
    traffic-sliding-capping:
      enabled: false
      max-allowed-upload-traffic: 25000000000
      max-speed: 0
      min-speed: 0
"#,
        )
        .expect("profile yaml");
        let active = cfg
            .module
            .active_monitoring
            .expect("active-monitoring 段必须可解析");
        assert!(enabled_or_disabled(&active.enabled));
        assert_eq!(active.traffic_monitoring.daily, -1);
        assert!(!active.traffic_sliding_capping.enabled);
        // 上游 profile.yml 的 `max-allowed-upload-traffic` 会被解析，但 Java 从不读取它
        assert_eq!(
            active
                .traffic_sliding_capping
                .profile_yml_max_allowed_upload_traffic,
            Some(25_000_000_000)
        );
        let settings = active.to_settings();
        assert_eq!(settings.daily_traffic_capping, -1);
        assert!(!settings.use_traffic_sliding_capping);
        assert_eq!(
            settings.max_traffic_allowed_in_window_period, 0,
            "归属 Java 实际读取的 daily-max-allowed-upload-traffic（缺键 -> 0）"
        );
        assert_eq!(settings.traffic_sliding_capping_max_speed, 0);
        assert_eq!(settings.traffic_sliding_capping_min_speed, 0);

        // 迁移脚本（ProfileUpdateScript v27）写入的形态
        let migrated: ActiveMonitoringConfig = serde_yaml::from_str(
            r#"
enabled: true
traffic-sliding-capping:
  enabled: true
  daily-max-allowed-upload-traffic: 53687091200
  max-speed: 10485760
  min-speed: 0
"#,
        )
        .expect("migrated active-monitoring yaml");
        let settings = migrated.to_settings();
        assert_eq!(
            settings.max_traffic_allowed_in_window_period,
            53_687_091_200
        );
        assert_eq!(settings.traffic_sliding_capping_max_speed, 10_485_760);
        assert_eq!(settings.traffic_sliding_capping_min_speed, 0);
        assert!(settings.use_traffic_sliding_capping);
        assert_eq!(
            settings.daily_traffic_capping, -1,
            "traffic-monitoring 段缺失 -> -1"
        );

        // 代码级默认值
        let defaulted = ActiveMonitoringConfig::default();
        assert_eq!(defaulted.enabled, None);
        assert_eq!(defaulted.to_settings().daily_traffic_capping, -1);
        assert_eq!(
            defaulted.to_settings().max_traffic_allowed_in_window_period,
            0
        );
        assert!(!defaulted.to_settings().use_traffic_sliding_capping);
    }

    /// `module.peer-analyse-service` 段（值逐字取自上游 `profile.yml` 第 458-483 行）。
    #[test]
    fn peer_analyse_service_section_matches_upstream_profile_yml() {
        let cfg: ProfileConfig = serde_yaml::from_str(
            r#"
module:
  peer-analyse-service:
    session-analyse:
      enabled: true
      data-flush-interval: 3600000
      cleanup-interval: 3600000
      data-retention-time: 15552000000
    swarm-tracking:
      enabled: true
      data-flush-interval: 3600000
    peer-recording:
      enabled: true
      data-flush-interval: 900000
      data-retention-time: 5184000000
      data-cleanup-interval: 604800000
"#,
        )
        .expect("profile yaml");
        let analyse = cfg
            .module
            .peer_analyse_service
            .expect("peer-analyse-service 段必须可解析");

        let session = analyse.session_analyse.expect("session-analyse 段");
        assert!(enabled_or_disabled(&session.enabled));
        let settings = session.to_settings();
        assert_eq!(settings.cleanup_interval_ms, 3_600_000);
        assert_eq!(settings.data_flush_interval_ms, 3_600_000);
        assert_eq!(settings.data_retention_time_ms, 15_552_000_000);

        let swarm = analyse.swarm_tracking.expect("swarm-tracking 段");
        assert!(enabled_or_disabled(&swarm.enabled));
        assert_eq!(swarm.to_settings().data_flush_interval_ms, 3_600_000);

        let recording = analyse.peer_recording.expect("peer-recording 段");
        assert!(enabled_or_disabled(&recording.enabled));
        let settings = recording.to_settings();
        assert_eq!(settings.data_flush_interval_ms, 900_000);
        assert_eq!(settings.data_retention_time_ms, 5_184_000_000);
        assert_eq!(settings.data_cleanup_interval_ms, 604_800_000);

        // 子段缺失 -> None（对齐 getConfigurationSection 返回 null -> 模块禁用）
        let partial: PeerAnalyseServiceConfig =
            serde_yaml::from_str("session-analyse:\n  enabled: true\n").unwrap();
        assert!(partial.swarm_tracking.is_none());
        assert!(partial.peer_recording.is_none());

        // 代码级默认值：session-analyse / swarm-tracking 的 getLong 无默认值（缺键 -> 0）；
        // peer-recording 三个键都有代码级默认值（-1 / -1 / 20000）
        let defaulted_session = SessionAnalyseConfig::default();
        assert_eq!(defaulted_session.enabled, None);
        assert_eq!(defaulted_session.cleanup_interval_ms, 0);
        assert_eq!(defaulted_session.data_flush_interval_ms, 0);
        assert_eq!(defaulted_session.data_retention_time_ms, 0);
        assert_eq!(SwarmTrackingConfig::default().data_flush_interval_ms, 0);
        let defaulted_recording = PeerRecordingConfig::default();
        assert_eq!(defaulted_recording.data_retention_time_ms, -1);
        assert_eq!(defaulted_recording.data_cleanup_interval_ms, -1);
        assert_eq!(defaulted_recording.data_flush_interval_ms, 20_000);

        // 未写 peer-analyse-service 段的配置不产生这些模块
        assert!(ProfileConfig::default()
            .module
            .peer_analyse_service
            .is_none());
        assert!(ProfileConfig::default().module.active_monitoring.is_none());
    }

    /// 无 GeoIP 数据库时 `build_pipeline_with_geo(None)` 与 `build_pipeline()` 等价
    #[test]
    fn pipeline_without_geoip_provider_matches_plain_pipeline() {
        let mut cfg = ProfileConfig::default();
        cfg.module.ip_address_blocker = Some(IpAddressBlockerConfig {
            enabled: Some(true),
            ban_duration_ms: 0,
            ips: vec!["1.2.3.0/24".to_string()],
            ports: vec![],
            asns: vec![64512],
            regions: vec!["CN".to_string()],
            cities: vec!["示例海南".to_string()],
            net_type: NetTypeConfig::default(),
        });
        let plain = cfg.build_pipeline();
        let without_geo = cfg.build_pipeline_with_geo(None);
        assert_eq!(
            module_config_names(&plain),
            module_config_names(&without_geo)
        );
        assert_eq!(module_config_names(&plain), vec!["ip-address-blocker"]);
    }
}
