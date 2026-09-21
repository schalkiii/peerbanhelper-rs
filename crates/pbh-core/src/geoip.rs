// === INTEGRATION SNIPPET (applied by main agent) ===
//
// [1] pbh-core/src/config.rs —— 本分支已落地，主 agent 无需再改：
//    `IpAddressBlockerConfig` 新增 4 个字段（对齐上游 `IPBlackList#reloadConfig`）：
//        asns: Vec<i64>            <- YAML `asns`      （按 ASN 封禁）
//        regions: Vec<String>      <- YAML `regions`   （按国家/地区 ISO 代码封禁）
//        cities: Vec<String>       <- YAML `cities`    （按城市/区/县名 contains 匹配）
//        net_type: NetTypeConfig   <- YAML `net-type`  （按网络类型封禁）
//    另新增主配置节结构体 `IpDatabaseConfig`（对齐上游 `config.yml` 的 `ip-database`）。
//    `ProfileConfig::build_pipeline()` 已把上述字段传给 `IpBlacklist`；
//    新增 `ProfileConfig::build_pipeline_with_geo(Option<Arc<dyn GeoIpProvider>>)` 用于注入数据库。
//
// [2] crates/pbh/src/default-config.yml —— 把 `module.ip-address-blocker:` 段扩充为：
//
//    module:
//      ip-address-blocker:
//        enabled: true
//        ban-duration: 259200000
//        ips:
//          - "0.0.0.0"
//        ports:
//          - 0
//        # 按 ASN 封禁（需要 GeoIP-ASN 数据库）
//        asns:
//          - "0"
//        # 按国家/地区 ISO 代码封禁（需要 GeoIP-City 数据库）
//        regions:
//          - "0"
//        # 按城市/区/县封禁（GeoCN 可用时使用 GeoCN 写法）
//        cities:
//          - "示例海南"
//        # 按网络类型封禁（仅中国大陆地区 IP 有效，需要 GeoCN 数据库）
//        net-type:
//          wideband: false
//          base-station: false
//          government-and-enterprise-line: false
//          business-platform: false
//          backbone-network: false
//          ip-private-network: false
//          internet-cafe: false
//          iot: false
//          datacenter: false
//
//    并在同一文件顶层追加主配置节（对齐上游 `config.yml`）：
//
//    ip-database:
//      auto-update: true
//      database-city: "GeoLite2-City"
//      database-asn: "GeoLite2-ASN"
//      database-geocn: "GeoCN"
//
// [3] crates/pbh/src/config.rs / main.rs —— 构建 GeoIP 提供器并注入流水线：
//
//    use std::sync::Arc;
//    use pbh_core::geoip::{geoip_force_disabled, GeoIpDb, GeoIpProvider};
//
//    let geo: Option<Arc<dyn GeoIpProvider>> = if geoip_force_disabled() {
//        None
//    } else {
//        // 上游 `new IPDB(new File(dataDirectory, "ipdb"), ...)` -> <data>/ipdb/geoip/*.mmdb
//        match GeoIpDb::load(data_dir.join("ipdb")) {
//            Ok(db) => Some(Arc::new(db)),
//            Err(e) => { tracing::info!("GeoIP 数据库不可用，ASN/地区/城市/网络类型维度全部不命中: {e}"); None }
//        }
//    };
//    let pipeline = profile.build_pipeline_with_geo(geo);
// ===

//! GeoIP 查询层：对齐上游 `util/ipdb/IPDBManager`、`util/ipdb/IPDB` 与
//! `util/ipdb/geocn/GeoCN1|2`，为 `IpBlacklist` 的 ASN / 地区 / 城市 / 网络类型四个维度提供数据。
//!
//! 与上游的对应关系：
//! - [`GeoIpDb`] ≈ `IPDB`：从 `<data>/ipdb/geoip/{GeoIP-City.mmdb, GeoIP-ASN.mmdb, GeoCN.mmdb}` 加载；
//! - [`GeoIpProvider::query`] ≈ `IPDBManager#queryIPDB`：**数据库不可用时返回 `None`**
//!   （上游 `new IPDBResponse(new LazyLoad<>(() -> null))`），调用方据此退化为「四维度全不命中」；
//! - [`IpGeoData`] ≈ `IPGeoData`：含逐字段覆盖式合并（`mergeFrom(other, overwrite)`）。
//!
//! 未移植的次要行为（都不改变任何判定结果）：
//! - `IPDBManager` 的 Guava 查询结果缓存（外部开关 `pbh.geoIpCache.timeout` / `pbh.geoIpCache.size`）；
//! - `IPDB#updateMMDB` 的数据库下载 / XZ 解压（属于下载层职责，本 crate 只读取已存在的文件）。
//!
//! 与上游的一处环境差异：`GeoCN2` 依赖 jar 内资源 `/ok_data_level3.csv`，
//! 本移植版改为在 ipdb 目录内查找同名文件（见 [`GEOIP_DIVISION_CSV`]）；
//! 缺失时 rev2 记录按上游「division 表查不到」处理（整条记录丢弃，MaxMind 结果保留）。

use crate::i18n::normalize_locale;
use maxminddb::geoip2;
use maxminddb::Reader;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------- IPGeoData

/// `IPGeoData`：一次 GeoIP 查询的全部结果（各字段可为空，对齐上游 `@Nullable`）。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IpGeoData {
    pub city: Option<CityData>,
    pub country: Option<CountryData>,
    pub as_data: Option<AsData>,
    pub network: Option<NetworkData>,
}

/// `IPGeoData.CityData`
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CityData {
    pub name: Option<String>,
    pub iso: Option<i64>,
    pub cn_province: Option<String>,
    pub cn_city: Option<String>,
    pub cn_districts: Option<String>,
}

/// `IPGeoData.CountryData`
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CountryData {
    pub name: Option<String>,
    pub iso: Option<String>,
}

/// `IPGeoData.ASData`
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AsData {
    pub number: Option<i64>,
    pub organization: Option<String>,
    pub ip_address: Option<String>,
    pub network: Option<AsNetwork>,
}

/// `IPGeoData.ASData.ASNetwork`
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AsNetwork {
    pub ip_address: Option<String>,
    pub prefix_length: Option<i32>,
}

/// `IPGeoData.NetworkData`
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NetworkData {
    pub isp: Option<String>,
    pub net_type: Option<String>,
}

/// 对齐 Java 的逐字段合并语义：`other` 为 null 时不动；否则 `this` 为空则取 `other`，
/// 非空时仅在 `overwrite` 下覆盖。
fn merge_opt<T: Clone>(this: &mut Option<T>, other: &Option<T>, overwrite: bool) {
    if let Some(value) = other {
        if this.is_none() || overwrite {
            *this = Some(value.clone());
        }
    }
}

impl CityData {
    /// 对齐 `CityData#merge(other, overwrite)`
    pub fn merge(&mut self, other: &CityData, overwrite: bool) {
        merge_opt(&mut self.name, &other.name, overwrite);
        merge_opt(&mut self.iso, &other.iso, overwrite);
        merge_opt(&mut self.cn_province, &other.cn_province, overwrite);
        merge_opt(&mut self.cn_city, &other.cn_city, overwrite);
        merge_opt(&mut self.cn_districts, &other.cn_districts, overwrite);
    }
}

impl CountryData {
    /// 对齐 `CountryData#merge(other, overwrite)`
    pub fn merge(&mut self, other: &CountryData, overwrite: bool) {
        merge_opt(&mut self.name, &other.name, overwrite);
        merge_opt(&mut self.iso, &other.iso, overwrite);
    }
}

impl AsNetwork {
    /// 对齐 `ASNetwork#mergeFrom(other, overwrite)`
    pub fn merge_from(&mut self, other: &AsNetwork, overwrite: bool) {
        merge_opt(&mut self.ip_address, &other.ip_address, overwrite);
        merge_opt(&mut self.prefix_length, &other.prefix_length, overwrite);
    }
}

impl AsData {
    /// 对齐 `ASData#mergeFrom(other, overwrite)`
    pub fn merge_from(&mut self, other: &AsData, overwrite: bool) {
        merge_opt(&mut self.number, &other.number, overwrite);
        merge_opt(&mut self.organization, &other.organization, overwrite);
        merge_opt(&mut self.ip_address, &other.ip_address, overwrite);
        if let Some(other_network) = &other.network {
            match &mut self.network {
                None => self.network = Some(other_network.clone()),
                Some(network) => network.merge_from(other_network, overwrite),
            }
        }
    }
}

impl NetworkData {
    /// 对齐 `NetworkData#mergeFrom(other, overwrite)`
    pub fn merge_from(&mut self, other: &NetworkData, overwrite: bool) {
        merge_opt(&mut self.isp, &other.isp, overwrite);
        merge_opt(&mut self.net_type, &other.net_type, overwrite);
    }
}

impl IpGeoData {
    /// 对齐 `IPGeoData#mergeFrom(other, overwrite)`（GeoCN 回填时使用 `overwrite = true`）
    pub fn merge_from(&mut self, other: &IpGeoData, overwrite: bool) {
        if let Some(other_city) = &other.city {
            match &mut self.city {
                None => self.city = Some(other_city.clone()),
                Some(city) => city.merge(other_city, overwrite),
            }
        }
        if let Some(other_country) = &other.country {
            match &mut self.country {
                None => self.country = Some(other_country.clone()),
                Some(country) => country.merge(other_country, overwrite),
            }
        }
        if let Some(other_as) = &other.as_data {
            match &mut self.as_data {
                None => self.as_data = Some(other_as.clone()),
                Some(as_data) => as_data.merge_from(other_as, overwrite),
            }
        }
        if let Some(other_network) = &other.network {
            match &mut self.network {
                None => self.network = Some(other_network.clone()),
                Some(network) => network.merge_from(other_network, overwrite),
            }
        }
    }
}

// ------------------------------------------------------------ GeoIP 维度配置

/// 网络类型维度：对齐 `IPBlackList#checkIPDB` 中 `switch (netType)` 比对的那组 camelCase 标志。
pub const NET_TYPE_WIDEBAND: &str = "wideband";
pub const NET_TYPE_BASE_STATION: &str = "baseStation";
pub const NET_TYPE_GOVERNMENT_AND_ENTERPRISE_LINE: &str = "governmentAndEnterpriseLine";
pub const NET_TYPE_BUSINESS_PLATFORM: &str = "businessPlatform";
pub const NET_TYPE_BACKBONE_NETWORK: &str = "backboneNetwork";
pub const NET_TYPE_IP_PRIVATE_NETWORK: &str = "ipPrivateNetwork";
pub const NET_TYPE_INTERNET_CAFE: &str = "internetCafe";
pub const NET_TYPE_IOT: &str = "iot";
/// 上游 `case "数据中心", "IDC" -> networkType.contains("dataCenter") || contains("datacenter")`
pub const NET_TYPE_DATACENTER: &str = "dataCenter";
pub const NET_TYPE_DATACENTER_ALT: &str = "datacenter";

/// GeoIP 四维度的运行期配置（对齐 `IPBlackList` 的 `asns` / `regions` / `cities` / `networkType`）。
#[derive(Clone, Debug, Default)]
pub struct GeoIpConfig {
    pub asns: HashSet<i64>,
    pub regions: HashSet<String>,
    pub cities: Vec<String>,
    /// camelCase 网络类型标志集合
    pub network_type: HashSet<String>,
}

impl GeoIpConfig {
    pub fn new(
        asns: impl IntoIterator<Item = i64>,
        regions: impl IntoIterator<Item = String>,
        cities: impl IntoIterator<Item = String>,
        network_type: impl IntoIterator<Item = String>,
    ) -> Self {
        Self {
            asns: asns.into_iter().collect(),
            regions: regions.into_iter().collect(),
            cities: cities.into_iter().collect(),
            network_type: network_type.into_iter().collect(),
        }
    }

    /// 四个维度是否都没有配置（空配置下一律不命中）
    pub fn is_empty(&self) -> bool {
        self.asns.is_empty()
            && self.regions.is_empty()
            && self.cities.is_empty()
            && self.network_type.is_empty()
    }

    /// 对齐上游 `switch (netType)`：数据库侧是 GeoCN 的中文原文，配置侧是 camelCase 标志。
    pub fn network_type_hit(&self, net_type: &str) -> bool {
        match net_type {
            "宽带" => self.network_type.contains(NET_TYPE_WIDEBAND),
            "基站" => self.network_type.contains(NET_TYPE_BASE_STATION),
            "政企专线" | "专线" => self
                .network_type
                .contains(NET_TYPE_GOVERNMENT_AND_ENTERPRISE_LINE),
            "业务平台" => self.network_type.contains(NET_TYPE_BUSINESS_PLATFORM),
            "骨干网" => self.network_type.contains(NET_TYPE_BACKBONE_NETWORK),
            "IP 专网" | "IP专网" => self.network_type.contains(NET_TYPE_IP_PRIVATE_NETWORK),
            "网吧" => self.network_type.contains(NET_TYPE_INTERNET_CAFE),
            "物联网" => self.network_type.contains(NET_TYPE_IOT),
            "数据中心" | "IDC" => {
                self.network_type.contains(NET_TYPE_DATACENTER)
                    || self.network_type.contains(NET_TYPE_DATACENTER_ALT)
            }
            _ => false,
        }
    }
}

// ------------------------------------------------------------- 外部开关

/// `IPDBManager` 构造函数：`ExternalSwitch.parseBoolean("pbh.forceDisableIPDB")`
pub const SWITCH_FORCE_DISABLE_IPDB: &str = "pbh.forceDisableIPDB";
/// `IPDBManager` 查询缓存过期时间（未移植，保留常量以便对齐文档）
pub const SWITCH_GEOIP_CACHE_TIMEOUT: &str = "pbh.geoIpCache.timeout";
/// `IPDBManager` 查询缓存容量（未移植）
pub const SWITCH_GEOIP_CACHE_SIZE: &str = "pbh.geoIpCache.size";
/// `Main` 中 `language: default` 分支读取的语言开关
pub const SWITCH_USER_LOCALE: &str = "pbh.userLocale";
pub const DEFAULT_GEOIP_CACHE_TIMEOUT_MS: i64 = 300_000;
pub const DEFAULT_GEOIP_CACHE_SIZE: u32 = 300;

/// 对齐 `ExternalSwitch.parse`：环境变量（键名 `小写点分键` → `大写下划线`）。
///
/// Java 侧还会先查系统属性、并支持 `--key=value` 启动参数；本移植版只保留环境变量一级，
/// 键名转换规则与上游 `args.replace(".", "_").replace("-", "_").toUpperCase(Locale.ROOT)` 一致。
pub fn external_switch(key: &str) -> Option<String> {
    let env_key = key.replace('.', "_").replace('-', "_").to_uppercase();
    match std::env::var(env_key) {
        Ok(value) if !value.is_empty() => Some(value),
        _ => None,
    }
}

/// 对齐 `ExternalSwitch.parseBoolean`
pub fn external_switch_bool(key: &str, def: bool) -> bool {
    match external_switch(key) {
        // Java `Boolean.parseBoolean`：仅 "true"（忽略大小写）为真
        Some(value) => value.eq_ignore_ascii_case("true"),
        None => def,
    }
}

/// 对齐 `ExternalSwitch.parseInt`
pub fn external_switch_i64(key: &str, def: i64) -> i64 {
    external_switch(key)
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(def)
}

/// `IPDBManager` 构造函数的开关：为真时完全不加载 GeoIP 数据库（四维度全部不命中）
pub fn geoip_force_disabled() -> bool {
    external_switch_bool(SWITCH_FORCE_DISABLE_IPDB, false)
}

/// 对齐 `Main.DEF_LOCALE` 的取值链：
/// `pbh.userLocale` 外部开关 → 进程 locale（`LANG` / `LC_ALL` 等）→ `en_us`。
pub fn default_locale() -> String {
    if let Some(value) = external_switch(SWITCH_USER_LOCALE) {
        let normalized = normalize_locale(&value);
        if !normalized.is_empty() {
            return normalized;
        }
    }
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(key) {
            // "zh_CN.UTF-8" -> "zh_CN" -> "zh_cn"
            let tag = value.split(['.', '@']).next().unwrap_or("").trim();
            if tag.is_empty() || tag == "C" || tag == "POSIX" {
                continue;
            }
            let normalized = normalize_locale(tag);
            if !normalized.is_empty() {
                return normalized;
            }
        }
    }
    "en_us".to_string()
}

// ------------------------------------------------------------ GeoIpProvider

/// GeoIP 数据源（≈ 上游 `IPDBManager`）。
///
/// 实现者**不得** panic：任何查询失败（数据库缺失、记录缺失、解码失败）都返回 `None`，
/// 调用方据此退化为「该 IP 不命中任何 GeoIP 维度」。
pub trait GeoIpProvider: Send + Sync + std::fmt::Debug {
    fn query(&self, ip: IpAddr) -> Option<IpGeoData>;
}

/// 注入用的静态数据源（按 IP 精确命中）。
///
/// 仅用于测试与手工注入：上游没有对应实现，生产路径应使用 [`GeoIpDb`]。
#[derive(Clone, Debug, Default)]
pub struct StaticGeoIp {
    data: std::collections::HashMap<IpAddr, IpGeoData>,
}

impl StaticGeoIp {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, ip: IpAddr, data: IpGeoData) -> &mut Self {
        self.data.insert(ip, data);
        self
    }
}

impl GeoIpProvider for StaticGeoIp {
    fn query(&self, ip: IpAddr) -> Option<IpGeoData> {
        self.data.get(&ip).cloned()
    }
}

// ------------------------------------------------------------------ GeoIpDb

/// GeoIP 数据库文件名（对齐 `IPDB` 构造函数中的常量）
pub const GEOIP_DIR: &str = "geoip";
pub const CITY_MMDB: &str = "GeoIP-City.mmdb";
pub const ASN_MMDB: &str = "GeoIP-ASN.mmdb";
pub const GEOCN_MMDB: &str = "GeoCN.mmdb";
/// 上游从 jar 内资源 `/ok_data_level3.csv` 读取行政区划表；移植版在 ipdb 目录内查找同名文件
pub const GEOIP_DIVISION_CSV: &str = "ok_data_level3.csv";

/// GeoIP 数据库打开失败。拿到 `Err` 的调用方应当**不注入任何 provider**，
/// 此时四个维度与上游「GeoIP 库不可用」的行为一致：全部不命中。
#[derive(Debug, thiserror::Error)]
pub enum GeoIpError {
    #[error("GeoIP 数据库 {path} 打开失败: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: maxminddb::MaxMindDbError,
    },
}

/// MaxMind-DB reader（≈ 上游 `IPDB` 的三份 mmdb 句柄）。
///
/// 字段保持 `Option` 以保留上游的 `if (mmdbXxx == null) return null;` 分支；
/// [`GeoIpDb::load`] 会一次性打开三份数据库，任一份缺失/损坏都会返回 `Err`——
/// 这与上游一致（`IPDB` 构造函数中任何 IOException 都会让 `IPDBManager.ipdb` 保持 `null`，
/// 进而使四个维度全部不命中）。
#[derive(Debug)]
pub struct GeoIpDb {
    city: Option<Reader<Vec<u8>>>,
    asn: Option<Reader<Vec<u8>>>,
    geo_cn: Option<Reader<Vec<u8>>>,
    division: Option<DivisionTable>,
    /// 对齐 `IPDB.languageTag = List.of(Main.DEF_LOCALE, "en")`
    locales: Vec<String>,
}

impl GeoIpDb {
    /// 按上游目录布局加载：`<ipdb_dir>/geoip/{GeoIP-City.mmdb, GeoIP-ASN.mmdb, GeoCN.mmdb}`。
    ///
    /// 语言使用 [`default_locale`]（对齐 `Main.DEF_LOCALE`），回退语言固定 `en`。
    pub fn load(ipdb_dir: impl AsRef<Path>) -> Result<Self, GeoIpError> {
        Self::load_with_locales(ipdb_dir, &[default_locale(), "en".to_string()])
    }

    /// 同 [`GeoIpDb::load`]，但显式指定语言优先级（对齐 `DatabaseReader.Builder#locales`）。
    pub fn load_with_locales(
        ipdb_dir: impl AsRef<Path>,
        locales: &[String],
    ) -> Result<Self, GeoIpError> {
        let directory = ipdb_dir.as_ref().join(GEOIP_DIR);
        let city = directory.join(CITY_MMDB);
        let asn = directory.join(ASN_MMDB);
        let geo_cn = directory.join(GEOCN_MMDB);
        let mut db = Self::open_files(Some(&city), Some(&asn), Some(&geo_cn), locales)?;
        // 行政区划表：优先 geoip 子目录，其次 ipdb 目录
        db.division = DivisionTable::load(&directory.join(GEOIP_DIVISION_CSV))
            .or_else(|| DivisionTable::load(&ipdb_dir.as_ref().join(GEOIP_DIVISION_CSV)));
        Ok(db)
    }

    /// 直接按文件打开（`None` = 该数据库不加载，对齐上游可空的 `mmdbCity` / `mmdbASN`）。
    pub fn open_files(
        city: Option<&Path>,
        asn: Option<&Path>,
        geo_cn: Option<&Path>,
        locales: &[String],
    ) -> Result<Self, GeoIpError> {
        let locales = if locales.is_empty() {
            vec!["en".to_string()]
        } else {
            locales.to_vec()
        };
        Ok(Self {
            city: open_reader(city)?,
            asn: open_reader(asn)?,
            geo_cn: open_reader(geo_cn)?,
            division: None,
            locales,
        })
    }

    /// 对齐 `IPDB#query`：逐项查询，任一项失败只丢该项，最后对 CN/TW/HK/MO 回填 GeoCN。
    pub fn query_geo(&self, address: IpAddr) -> IpGeoData {
        let mut geo = IpGeoData {
            city: self.query_city(address),
            country: self.query_country(address),
            as_data: self.query_as(address),
            network: self.query_network(address),
        };
        if let Some(iso) = geo.country.as_ref().and_then(|c| c.iso.as_deref()) {
            if ["CN", "TW", "HK", "MO"]
                .iter()
                .any(|code| code.eq_ignore_ascii_case(iso))
            {
                self.query_geo_cn(address, &mut geo);
            }
        }
        geo
    }

    /// 对齐 `IPDB#queryAS`
    fn query_as(&self, address: IpAddr) -> Option<AsData> {
        let reader = self.asn.as_ref()?;
        let result = reader.lookup(address).ok()?;
        let asn: geoip2::Asn = result.decode().ok()??;
        let mut data = AsData {
            number: asn.autonomous_system_number.map(i64::from),
            organization: asn.autonomous_system_organization.map(str::to_string),
            ip_address: Some(address.to_string()),
            network: None,
        };
        if let Ok(network) = result.network() {
            data.network = Some(AsNetwork {
                ip_address: Some(network.ip().to_string()),
                prefix_length: Some(i32::from(network.prefix())),
            });
        }
        Some(data)
    }

    /// 对齐 `IPDB#queryCountry`
    fn query_country(&self, address: IpAddr) -> Option<CountryData> {
        let reader = self.city.as_ref()?;
        let result = reader.lookup(address).ok()?;
        let record: geoip2::City = result.decode().ok()??;
        let country = record.country;
        let iso = country.iso_code.map(str::to_string);
        let mut name = name_for_locales(&country.names, &self.locales);
        // 上游：台湾、香港、澳门地区有独立 ISO 代码，中文语境下补「中国」前缀
        if let (Some(name), Some(iso)) = (name.as_mut(), iso.as_deref()) {
            let code = self
                .locales
                .first()
                .map(|c| normalize_locale(c))
                .unwrap_or_default();
            if matches!(code.as_str(), "zh_cn" | "zh_hk" | "zh_mo")
                && ["TW", "HK", "MO"]
                    .iter()
                    .any(|c| c.eq_ignore_ascii_case(iso))
            {
                *name = format!("中国{name}");
            }
        }
        Some(CountryData { name, iso })
    }

    /// 对齐 `IPDB#queryCity`
    fn query_city(&self, address: IpAddr) -> Option<CityData> {
        let reader = self.city.as_ref()?;
        let result = reader.lookup(address).ok()?;
        let record: geoip2::City = result.decode().ok()??;
        Some(CityData {
            name: name_for_locales(&record.city.names, &self.locales),
            iso: record.city.geoname_id.map(i64::from),
            cn_province: None,
            cn_city: None,
            cn_districts: None,
        })
    }

    /// 对齐 `IPDB#queryNetwork`（注意上游显式把 `netType` 置为 null，
    /// 网络类型只能由 GeoCN 回填）
    fn query_network(&self, address: IpAddr) -> Option<NetworkData> {
        let reader = self.asn.as_ref()?;
        let result = reader.lookup(address).ok()?;
        let asn: geoip2::Asn = result.decode().ok()??;
        Some(NetworkData {
            isp: asn.autonomous_system_organization.map(str::to_string),
            net_type: None,
        })
    }

    /// 对齐 `IPDB#queryGeoCN`：先按 GeoCN rev2 解析，命中 rev1 布局时回退 rev1。
    fn query_geo_cn(&self, address: IpAddr, geo: &mut IpGeoData) {
        let Some(reader) = self.geo_cn.as_ref() else {
            return;
        };
        let Ok(result) = reader.lookup(address) else {
            return;
        };
        let Ok(Some(record)) = result.decode::<GeoCnRecord>() else {
            return;
        };
        // rev2：以 `division_code` 标识；rev1：以 `province` 标识（对齐 `GeoCN2#query` 的
        // `!containsKey("division_code") && containsKey("province")` 判定）
        let as_network = result.network().ok().map(|network| AsNetwork {
            ip_address: Some(network.ip().to_string()),
            prefix_length: Some(i32::from(network.prefix())),
        });
        let data = if record.division_code.is_some() {
            self.query_geo_cn_rev2(&record, as_network)
        } else if record.province.is_some() {
            geo_cn_rev1(&record)
        } else {
            None
        };
        if let Some(data) = data {
            geo.merge_from(&data, true);
        }
    }

    /// 对齐 `GeoCN2#query`
    fn query_geo_cn_rev2(
        &self,
        record: &GeoCnRecord,
        as_network: Option<AsNetwork>,
    ) -> Option<IpGeoData> {
        let division_code = record
            .division_code
            .as_ref()
            .map(CodeValue::as_string)
            .unwrap_or_default();
        let division_code_num = division_code.parse::<i64>().unwrap_or(0);
        let division_data = self
            .division
            .as_ref()
            .map(|table| table.lookup(&division_code))
            .unwrap_or_default();
        if division_data.is_empty() {
            return None;
        }
        let province = division_data.first().cloned();
        let city = division_data.get(1).cloned();
        let county = division_data.get(2).cloned();
        let town = division_data.get(3).cloned();
        let full_name = division_data.join(" ");
        let cn_districts = county.map(|county| match town {
            None => county,
            Some(town) => format!("{county} {town}"),
        });
        Some(IpGeoData {
            city: Some(CityData {
                name: Some(full_name),
                iso: Some(division_code_num),
                cn_province: province,
                cn_city: city,
                cn_districts,
            }),
            country: None,
            as_data: Some(AsData {
                network: as_network,
                ..AsData::default()
            }),
            network: Some(NetworkData {
                isp: record.isp.clone(),
                net_type: record.net_type.clone(),
            }),
        })
    }
}

impl GeoIpProvider for GeoIpDb {
    fn query(&self, ip: IpAddr) -> Option<IpGeoData> {
        Some(self.query_geo(ip))
    }
}

/// 打开单个 mmdb；`None` 路径表示不加载该库（对齐上游可空字段）。
fn open_reader(path: Option<&Path>) -> Result<Option<Reader<Vec<u8>>>, GeoIpError> {
    match path {
        None => Ok(None),
        Some(path) => Reader::open_readfile(path)
            .map(Some)
            .map_err(|source| GeoIpError::Open {
                path: path.to_path_buf(),
                source,
            }),
    }
}

/// 从 `geoip2::Names` 按 locale 优先级取名（对齐 `DatabaseReader.Builder#locales`：
/// 依次尝试每个 locale，取第一个非空名称）。
///
/// Rust 侧 `geoip2::Names` 是固定字段结构，这里把 locale 归一化后映射到对应字段
/// （`zh_cn` → `zh-CN` 等）；数据库中其它语言（如 `zh-TW`）在 Rust 侧无对应字段。
fn name_for_locales(names: &geoip2::Names<'_>, locales: &[String]) -> Option<String> {
    for locale in locales {
        let code = normalize_locale(locale);
        let value = match code.as_str() {
            "en" | "en_us" | "en_gb" => names.english,
            "zh" | "zh_cn" | "zh_hans" | "zh_hans_cn" | "zh_sg" => names.simplified_chinese,
            "de" | "de_de" => names.german,
            "es" | "es_es" => names.spanish,
            "fr" | "fr_fr" => names.french,
            "ja" | "ja_jp" => names.japanese,
            "pt_br" => names.brazilian_portuguese,
            "ru" | "ru_ru" => names.russian,
            _ => None,
        };
        if let Some(value) = value {
            return Some(value.to_string());
        }
    }
    None
}

// -------------------------------------------------------------------- GeoCN

/// GeoCN 记录的编码值：可能是数字，也可能是字符串（对齐上游 `Object` + `toString()` 解析）
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum CodeValue {
    Int(i64),
    Text(String),
}

impl CodeValue {
    fn as_string(&self) -> String {
        match self {
            CodeValue::Int(value) => value.to_string(),
            CodeValue::Text(value) => value.clone(),
        }
    }
}

/// GeoCN 记录（同时兼容 rev1 / rev2 布局，字段名与上游读取的键一致）
#[derive(Clone, Debug, Default, Deserialize)]
struct GeoCnRecord {
    #[serde(default)]
    isp: Option<String>,
    /// rev1 网络类型（GeoCN1 的 `net`）
    #[serde(default)]
    net: Option<String>,
    /// rev2 网络类型（GeoCN2 的 `type`）
    #[serde(default, rename = "type")]
    net_type: Option<String>,
    #[serde(default)]
    province: Option<String>,
    #[serde(default)]
    city: Option<String>,
    #[serde(default)]
    districts: Option<String>,
    #[serde(default)]
    province_code: Option<CodeValue>,
    #[serde(default)]
    city_code: Option<CodeValue>,
    #[serde(default)]
    districts_code: Option<CodeValue>,
    #[serde(default)]
    division_code: Option<CodeValue>,
}

/// 对齐 Java 字符串拼接时 `null` 会变成字面量 `"null"` 的行为（`GeoCN1#query` 的城市名拼接）。
fn java_text(value: Option<&String>) -> &str {
    value.map(String::as_str).unwrap_or("null")
}

/// 对齐 `GeoCN1#query`：任一字段解码失败（含区划代码全空导致的 `NumberFormatException`）
/// 都会让整条记录作废 → 返回 `None`。
fn geo_cn_rev1(record: &GeoCnRecord) -> Option<IpGeoData> {
    let city_name = format!(
        "{} {} {}",
        java_text(record.province.as_ref()),
        java_text(record.city.as_ref()),
        java_text(record.districts.as_ref())
    );
    let city_name = city_name.trim().to_string();
    let code = record
        .province_code
        .as_ref()
        .map(CodeValue::as_string)
        .or_else(|| record.city_code.as_ref().map(CodeValue::as_string))
        .or_else(|| record.districts_code.as_ref().map(CodeValue::as_string))?;
    let iso = format!("86{code}").parse::<i64>().ok()?;
    let mut city = CityData {
        iso: Some(iso),
        cn_province: record.province.clone(),
        cn_city: record.city.clone(),
        cn_districts: record.districts.clone(),
        name: None,
    };
    if !city_name.is_empty() {
        city.name = Some(city_name);
    }
    let network = NetworkData {
        isp: record
            .isp
            .as_ref()
            .filter(|isp| !isp.trim().is_empty())
            .cloned(),
        // 上游把 GeoCN 的原始类型名交给 `tlUI()` 查表；随包分发的 4 份文案表中
        // 「宽带 / 基站 / …」都不是文案键，因此查表结果即原文。
        net_type: record
            .net
            .as_ref()
            .filter(|net| !net.trim().is_empty())
            .cloned(),
    };
    Some(IpGeoData {
        city: Some(city),
        country: None,
        as_data: None,
        network: Some(network),
    })
}

/// `GeoCN2.DivisionParser`：`ok_data_level3.csv` 的 `id -> ext_name` 前缀表。
#[derive(Debug, Default)]
struct DivisionTable {
    by_code: BTreeMap<String, String>,
}

impl DivisionTable {
    /// 读取失败（文件不存在 / 无法读取 / 表头缺列）时返回 `None`：
    /// 此时 rev2 记录按上游「division 表查不到」处理。
    fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut lines = text.lines();
        let header = parse_csv_line(lines.next()?);
        let id_index = header.iter().position(|column| column == "id")?;
        let name_index = header.iter().position(|column| column == "ext_name")?;
        let mut by_code = BTreeMap::new();
        for line in lines {
            let fields = parse_csv_line(line);
            let (Some(id), Some(name)) = (fields.get(id_index), fields.get(name_index)) else {
                continue;
            };
            if id.is_empty() || name.is_empty() {
                continue;
            }
            by_code.insert(id.clone(), name.clone());
        }
        Some(Self { by_code })
    }

    /// 对齐 `GeoCN2.DivisionParser#lookup`：逐字符取前缀，命中几个就返回几级
    fn lookup(&self, code: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut boundaries: Vec<usize> = code.char_indices().map(|(index, _)| index).collect();
        boundaries.push(code.len());
        for &end in boundaries.iter().skip(1) {
            if let Some(name) = self.by_code.get(&code[..end]) {
                out.push(name.clone());
            }
        }
        out
    }
}

/// 最小 CSV 解析：支持双引号包裹与 `""` 转义（对齐 fastcsv 的默认行为）
fn parse_csv_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if in_quotes => {
                if chars.peek() == Some(&'"') {
                    current.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            }
            '"' => in_quotes = true,
            ',' if !in_quotes => fields.push(std::mem::take(&mut current)),
            _ => current.push(c),
        }
    }
    fields.push(current);
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_directory_is_err_and_never_panics() {
        let dir = std::env::temp_dir().join("pbh-core-geoip-does-not-exist");
        let result = GeoIpDb::load(&dir);
        assert!(
            result.is_err(),
            "缺少 mmdb 时必须返回 Err，由调用方退化为「不注入 provider」"
        );
    }

    #[test]
    fn empty_provider_reports_no_data() {
        // 上游 `IPDBManager#queryIPDB`：ipdb == null -> LazyLoad 返回 null
        let provider = StaticGeoIp::new();
        assert_eq!(provider.query("1.2.3.4".parse().unwrap()), None);
    }

    #[test]
    fn static_provider_returns_exact_membership() {
        let mut provider = StaticGeoIp::new();
        let ip: IpAddr = "1.2.3.4".parse().unwrap();
        let data = IpGeoData {
            as_data: Some(AsData {
                number: Some(64512),
                ..AsData::default()
            }),
            ..IpGeoData::default()
        };
        provider.insert(ip, data.clone());
        assert_eq!(provider.query(ip), Some(data));
        assert_eq!(provider.query("1.2.3.5".parse().unwrap()), None);
    }

    #[test]
    fn network_type_switch_matches_upstream_cases() {
        let mut config = GeoIpConfig::default();
        assert!(!config.network_type_hit("宽带"), "空集合恒不命中");
        config.network_type.insert(NET_TYPE_WIDEBAND.into());
        config.network_type.insert(NET_TYPE_DATACENTER_ALT.into());
        assert!(config.network_type_hit("宽带"));
        assert!(!config.network_type_hit("基站"));
        assert!(config.network_type_hit("数据中心"));
        assert!(config.network_type_hit("IDC"));
        assert!(!config.network_type_hit("未知类型"));
    }

    #[test]
    fn net_type_config_is_empty_for_default() {
        assert!(GeoIpConfig::default().is_empty());
    }

    #[test]
    fn merge_overwrites_only_when_requested() {
        let mut base = IpGeoData {
            network: Some(NetworkData {
                isp: Some("A".into()),
                net_type: None,
            }),
            ..IpGeoData::default()
        };
        let other = IpGeoData {
            network: Some(NetworkData {
                isp: Some("B".into()),
                net_type: Some("宽带".into()),
            }),
            ..IpGeoData::default()
        };
        base.merge_from(&other, false);
        assert_eq!(base.network.as_ref().unwrap().isp.as_deref(), Some("A"));
        assert_eq!(
            base.network.as_ref().unwrap().net_type.as_deref(),
            Some("宽带")
        );
        base.merge_from(&other, true);
        assert_eq!(base.network.as_ref().unwrap().isp.as_deref(), Some("B"));
    }

    #[test]
    fn division_lookup_returns_prefix_path() {
        let mut table = DivisionTable::default();
        table.by_code.insert("11".into(), "北京市".into());
        table.by_code.insert("1101".into(), "北京市".into());
        table.by_code.insert("110101".into(), "东城区".into());
        assert_eq!(table.lookup("110101"), vec!["北京市", "北京市", "东城区"]);
        assert!(table.lookup("999999").is_empty());
    }

    #[test]
    fn csv_line_parsing_handles_quotes() {
        assert_eq!(
            parse_csv_line("11,0,0,\"北京\",\"b\",\"bei jing\",\"110000000000\",\"北京市\""),
            vec![
                "11",
                "0",
                "0",
                "北京",
                "b",
                "bei jing",
                "110000000000",
                "北京市"
            ]
        );
    }

    #[test]
    fn geo_cn_rev1_drops_record_without_division_code() {
        let record = GeoCnRecord {
            province: Some("海南省".into()),
            ..GeoCnRecord::default()
        };
        assert!(
            geo_cn_rev1(&record).is_none(),
            "区划代码全空 -> 整条记录作废"
        );
    }

    #[test]
    fn geo_cn_rev1_builds_city_and_net_type() {
        let record = GeoCnRecord {
            isp: Some("中国电信".into()),
            net: Some("宽带".into()),
            province: Some("海南省".into()),
            city: Some("海口市".into()),
            districts: Some("美兰区".into()),
            province_code: Some(CodeValue::Int(460000)),
            ..GeoCnRecord::default()
        };
        let data = geo_cn_rev1(&record).unwrap();
        let city = data.city.unwrap();
        assert_eq!(city.name.as_deref(), Some("海南省 海口市 美兰区"));
        assert_eq!(city.iso, Some(86460000));
        assert_eq!(city.cn_province.as_deref(), Some("海南省"));
        let network = data.network.unwrap();
        assert_eq!(network.isp.as_deref(), Some("中国电信"));
        assert_eq!(network.net_type.as_deref(), Some("宽带"));
    }

    #[test]
    fn external_switch_uses_uppercase_env_key() {
        assert_eq!(external_switch("pbh.not.exists"), None);
        assert_eq!(
            external_switch("pbh.forceDisableIPDB"),
            std::env::var("PBH_FORCEDISABLEIPDB")
                .ok()
                .filter(|v| !v.is_empty())
        );
        assert_eq!(
            geoip_force_disabled(),
            external_switch_bool(SWITCH_FORCE_DISABLE_IPDB, false)
        );
    }

    #[test]
    fn locale_falls_back_to_en_us() {
        assert!(!default_locale().is_empty());
    }
}
