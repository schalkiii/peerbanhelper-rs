// === INTEGRATION SNIPPET (applied by main agent) ===
//
// 【1】pbh-core/src/geoip_update.rs —— 本文件已落地，主 agent 无需再改：
//      对齐上游 `util/ipdb/IPDB#updateMMDB` / `util/ipdb/IPDBDownloadSource` /
//      `util/ipdb/IPDBManager#setupIPDB`。要点：
//        - 三处镜像（顺序与上游一致）：GitHub Releases → pbh-static.paulzzh.com → pbh-static.ghostchu.com
//        - URL 形如 `<baseUrl><databaseName>.mmdb.xz`（三处镜像均 `supportXzip = true`）
//        - XZ 解压（上游 `org.tukaani.xz.XZInputStream`）→ 纯 Rust `lzma-rs`
//        - 解压后先用 MaxMind 读取器打开一次（上游 `IPDB#validateMMDB`），再原子替换目标文件
//        - 更新间隔：45 天（上游 `updateInterval = 3888000000L`，按文件 mtime 判定）
//        - 磁盘布局：`<data>/ipdb/geoip/{GeoIP-City.mmdb, GeoIP-ASN.mmdb, GeoCN.mmdb}`
//          （即 `geoip.rs` 的 `GeoIpDb::load(<data>/ipdb)` 读取的位置）
//
// 【2】crates/pbh/src/main.rs —— 在现有 `GeoIpDb::load(&ipdb_dir)` 之前插入
//      （上游 `IPDB` 构造函数是「先 `updateMMDB` 再 `loadMMDB`」）：
//
//      let ipdb_dir = data_dir.join("ipdb");
//      // GeoIP 数据库自动更新（对齐上游 `IPDBManager#setupIPDB` 的 `CompletableFuture.runAsync`）：
//      // `ip-database.auto-update` 为 false / 缺省时严格 no-op（连本地缺失也不下载）。
//      // `spawn_update` 在独立线程里跑，因此 `reqwest::blocking` 不会踩到 tokio runtime。
//      // （这层 if 只为省掉客户端构造；更新器内部对 auto-update: false 同样是严格 no-op）
//      if cfg.ip_database.auto_update {
//          match pbh_core::geoip_update::ReqwestBlockingHttpClient::new() {
//              Ok(http) => {
//                  if let Ok(handle) = pbh_core::geoip_update::spawn_update(
//                      ipdb_dir.clone(),
//                      cfg.ip_database.clone(),
//                      Arc::new(http),
//                  ) {
//                      // 后台更新，不阻塞启动；上游同样在后台线程里更新
//                      let _ = tokio::task::spawn_blocking(move || handle.join());
//                  }
//              }
//              Err(e) => warn!("GeoIP 下载客户端初始化失败，跳过自动更新: {e}"),
//          }
//      }
//      // ↓ 现有代码不动：更新是后台的，本次启动先用已有数据库（就绪后由下次启动/重启生效）
//      match GeoIpDb::load(&ipdb_dir) { ... }
//
//      若希望「下载完再加载」（上游 `setupIPDB` 在同一个异步任务里先更新后加载），改成：
//
//      let report = tokio::task::spawn_blocking(move || {
//          let http = pbh_core::geoip_update::ReqwestBlockingHttpClient::new()?;
//          pbh_core::geoip_update::GeoIpUpdater::new(ipdb_dir.clone(), cfg.ip_database.clone(), &http)
//              .update_if_needed()
//      })
//      .await?;
//      // report 为 `UpdateReport::Skipped(..)` 或逐个数据库的 `DatabaseUpdate`，可据此记日志；
//      // 三个库都不可用时 `GeoIpDb::load` 会返回 Err，按上游 `IPDBManager#setupIPDB` 的 catch
//      // 分支记录 `geoip_update::LANG_IPDB_INVALID`（`pbh.forceDisableIPDB` 已打开则跳过整个流程）。
//
// 【3】crates/pbh/src/default-config.yml —— `ip-database:` 段已含 4 个键，无需改动：
//        auto-update: true
//        database-city: "GeoLite2-City"
//        database-asn: "GeoLite2-ASN"
//        database-geocn: "GeoCN"
//      （`account-id` / `license-key` 为上游 `config.yml` 未写出的遗留键，仅在收到 401 时用作
//       Basic 凭据重试，对齐 `IPDB` 的 OkHttp authenticator。）
//
// 【4】若要复刻上游「`auto-update: false` 但本地文件缺失时仍下载一次」的分支，
//      用 `GeoIpUpdater::with_download_missing(true)`（见 `download_missing` 字段文档）。
//
// 【5】`ok_data_level3.csv`（上游 jar 内资源，GeoCN rev2 的行政区划表）**不属于**本更新器
//      的下载内容（上游同样不下载它）。移植版按 `geoip.rs` 的约定在
//      `<data>/ipdb/geoip/ok_data_level3.csv` 或 `<data>/ipdb/ok_data_level3.csv` 查找；
//      缺失时 GeoCN rev2 记录按「division 表查不到」丢弃（MaxMind 结果保留，见 geoip.rs 头部）。
// ===

//! GeoIP 数据库自动更新层：对齐上游 `util/ipdb/IPDB#updateMMDB`、`util/ipdb/IPDBDownloadSource`
//! 与 `util/ipdb/IPDBManager#setupIPDB`，把 mmdb 数据库下载到 [`GeoIpDb`](crate::geoip::GeoIpDb)
//! 读取的位置（`<ipdb_dir>/geoip/{GeoIP-City.mmdb, GeoIP-ASN.mmdb, GeoCN.mmdb}`）。
//!
//! 逐项对齐关系：
//! - [`IpdbDownloadSource`] ≈ `IPDBDownloadSource`：`url() = baseUrl + databaseName + (".mmdb.xz" | ".mmdb")`；
//! - [`default_mirrors`] ≈ `IPDB#updateMMDB` 里的 `mirror1` / `mirror3` / `mirror4`（顺序、URL 逐字一致）；
//! - [`GeoIpUpdater::update_if_needed`] ≈ `IPDB` 构造函数的三次 `needUpdateMMDB` + `updateMMDB`；
//! - [`need_update_mmdb`] ≈ `IPDB#needUpdateMMDB`（缺失 ⇒ 更新；`auto-update: false` ⇒ 不更新；
//!   否则 mtime 超过 [`MMDB_UPDATE_INTERVAL_MS`]（45 天）⇒ 更新）；
//! - [`decompress_xz`] ≈ `new XZInputStream(body.byteStream())`（纯 Rust `lzma-rs`，不依赖系统 liblzma）；
//! - [`GeoIpUpdater`] 内部的 `validate_mmdb` ≈ `IPDB#validateMMDB`（用 MaxMind 读取器打开一次）；
//! - 失败策略 ≈ `downloadFile`：镜像逐个轮换（`IPDB_RETRY_WITH_BACKUP_SOURCE`），全部失败后
//!   目标文件**保持原样**并记录 `IPDB_UPDATE_FAILED` / `IPDB_EXISTS_UPDATE_FAILED`。
//!
//! 与上游的四处**有意差异**（都不改变「有可用文件就用、没有就降级」的对外语义）：
//! 1. `auto-update: false` 时本移植版整体旁路（**不产生任何网络请求**，连本地缺失也不下载）；
//!    上游 `needUpdateMMDB` 对缺失文件仍返回 true，于是 `auto-update: false` 也会下载一次。
//!    需要上游行为时用 [`GeoIpUpdater::with_download_missing`] 显式打开该分支。
//! 2. 临时文件建在**目标文件同目录**（`.<文件名>.<pid>.<序号>.tmp`），随后 `rename` 原子替换；
//!    上游在系统临时目录建文件再 `Files.move`。任何一步失败都会删除临时文件，
//!    绝不出现半成品数据库（上游在「下载失败但目标已存在」时会继续 `Files.move` 一个空临时文件，
//!    会截断既有数据库；此处不复刻该缺陷，失败时一律保留原文件）。
//! 3. 上游用 `BackgroundTaskManager` 汇报下载进度（`DOWNLOAD_PROGRESS*` 文案），本移植版改为
//!    `debug!` 日志（同样的文案键），因为本 crate 没有后台任务 UI 层。
//! 4. 「本地从来没有过该库且下载失败」时上游抛 `IllegalStateException`，整个 `IPDB` 构造失败
//!    （后续数据库不再尝试、`IPDBManager.ipdb` 保持 null ⇒ 四个维度全部不命中）；
//!    本移植版记录为 [`DatabaseUpdate::Failed`] `{ kept_local_copy: false }` 并继续处理其余数据库，
//!    由调用方决定降级（`GeoIpDb::load` 仍会因文件缺失而返回 `Err`，即同样的「全不命中」）。
//!
//! 未移植（都不影响判定结果）：`IPDBManager` 的 Guava 查询缓存、`IPDB#loadMMDB` 对损坏文件的
//! `deleteOnExit`（`geoip.rs` 的 [`GeoIpDb::load`](crate::geoip::GeoIpDb::load) 改为返回 `Err`
//! 由调用方降级）、`ExternalSwitch` 之外的 OkHttp 连接池调优。

use crate::config::IpDatabaseConfig;
use crate::geoip::{
    default_locale, geoip_force_disabled, ASN_MMDB, CITY_MMDB, GEOCN_MMDB, GEOIP_DIR,
};
use crate::i18n::{Param, TranslationComponent, Translator};
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, error, info, warn};

// ------------------------------------------------------------------ 常量

/// `IPDB` 构造函数：`new File(Main.getDataDirectory(), "ipdb")`
pub const IPDB_DIR: &str = "ipdb";

/// `IPDB#updateMMDB` 的三处镜像 baseUrl（顺序与上游一致：mirror1 / mirror3 / mirror4）。
///
/// 三处都提供 `.mmdb.xz`（上游 `new IPDBDownloadSource(baseUrl, databaseName, true)`）。
pub const DEFAULT_MIRROR_BASES: [&str; 3] = [
    "https://github.com/PBH-BTN/GeoLite.mmdb/releases/latest/download/",
    "https://pbh-static.paulzzh.com/ipdb/",
    "https://pbh-static.ghostchu.com/ipdb/",
];

/// `IPDB#needUpdateMMDB` 的更新间隔：`3888000000L` 毫秒（45 天）
pub const MMDB_UPDATE_INTERVAL_MS: i64 = 3_888_000_000;

/// `IPDBDownloadSource#getIPDBUrl` 的两种后缀
pub const XZ_SUFFIX: &str = ".mmdb.xz";
pub const MMDB_SUFFIX: &str = ".mmdb";

/// 上游 `Main.getUserAgent()` 生成 UA；本移植版无版本信息提供者，用与其它 crate 一致的 UA
pub const DEFAULT_USER_AGENT: &str = "PeerBanHelper-RS";

/// 上游 `IPDB` 下载客户端的超时（`connectTimeout(15s)` / `readTimeout`+`callTimeout(3min)`）
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
pub const READ_TIMEOUT: Duration = Duration::from_secs(180);

/// 上游 `Lang.*` 文案键（`IPDB#updateMMDB` / `IPDBManager#setupIPDB` 使用）
pub const LANG_IPDB_UPDATING: &str = "IPDB_UPDATING";
pub const LANG_IPDB_UPDATE_SUCCESS: &str = "IPDB_UPDATE_SUCCESS";
pub const LANG_IPDB_UPDATE_FAILED: &str = "IPDB_UPDATE_FAILED";
pub const LANG_IPDB_EXISTS_UPDATE_FAILED: &str = "IPDB_EXISTS_UPDATE_FAILED";
pub const LANG_IPDB_RETRY_WITH_BACKUP_SOURCE: &str = "IPDB_RETRY_WITH_BACKUP_SOURCE";
pub const LANG_IPDB_UNGZIP_FAILED: &str = "IPDB_UNGZIP_FAILED";
/// 上游用它做后台任务**标题**（`new FunctionalBackgroundTask(new TranslationComponent(Lang.IPDB_DOWNLOAD_MMDB), ...)`）；
/// 本移植版没有后台任务 UI 层，因此只保留该键、不参与日志输出
pub const LANG_IPDB_DOWNLOAD_MMDB: &str = "IPDB_DOWNLOAD_MMDB";
/// 上游用它做下载**状态文本**（参数为镜像 URL），此处以 `debug!` 输出
pub const LANG_IPDB_DOWNLOAD_MMDB_DESCRIPTION: &str = "IPDB_DOWNLOAD_MMDB_DESCRIPTION";
pub const LANG_IPDB_INVALID: &str = "IPDB_INVALID";
pub const LANG_DOWNLOAD_PROGRESS: &str = "DOWNLOAD_PROGRESS";

/// 上游 `log.error(tlUI(Lang.IPDB_UPDATE_FAILED, ...))` 里的失败详情：本移植版截断到 256 字符
const BODY_PREVIEW_LIMIT: usize = 256;

/// 渲染上游文案（对齐 `tlUI(...)`：内嵌 4 份文案表 + `Main.DEF_LOCALE` 的 locale 回退链）。
fn t(key: &str, params: Vec<Param>) -> String {
    static TRANSLATOR: OnceLock<Translator> = OnceLock::new();
    let translator = TRANSLATOR.get_or_init(Translator::embedded);
    translator.render(
        &TranslationComponent::with_params(key, params),
        &default_locale(),
    )
}

// ------------------------------------------------------------- 下载源

/// `IPDBDownloadSource`：baseUrl + databaseName + 是否 XZ 压缩
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IpdbDownloadSource {
    pub base_url: String,
    pub database_name: String,
    /// 上游 `supportXzip`：三处镜像均为 true
    pub support_xzip: bool,
}

impl IpdbDownloadSource {
    /// 对齐 2 参构造：`supportXzip = false`
    pub fn new(base_url: impl Into<String>, database_name: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            database_name: database_name.into(),
            support_xzip: false,
        }
    }

    /// 对齐 3 参构造：`supportXzip = true`
    pub fn with_xzip(base_url: impl Into<String>, database_name: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            database_name: database_name.into(),
            support_xzip: true,
        }
    }

    /// 对齐 `IPDBDownloadSource#getIPDBUrl`
    pub fn url(&self) -> String {
        if self.support_xzip {
            format!("{}{}{}", self.base_url, self.database_name, XZ_SUFFIX)
        } else {
            format!("{}{}{}", self.base_url, self.database_name, MMDB_SUFFIX)
        }
    }
}

/// 对齐 `IPDB#updateMMDB` 中构造的三处镜像（顺序即重试顺序）
pub fn default_mirrors(database_name: &str) -> Vec<IpdbDownloadSource> {
    DEFAULT_MIRROR_BASES
        .iter()
        .map(|base| IpdbDownloadSource::with_xzip(*base, database_name))
        .collect()
}

// ------------------------------------------------------------- HTTP 抽象

/// 一次下载请求（本 crate 自有的极小抽象：不依赖 `pbh-downloader`，也便于测试注入计数 mock）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeoIpHttpRequest {
    pub url: String,
    /// 对齐 `IPDB` 的 OkHttp `authenticator`：收到 401 后带 `account-id`/`license-key` 重试
    pub basic: Option<(String, String)>,
}

impl GeoIpHttpRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            basic: None,
        }
    }

    /// 对齐 `okhttp3.Credentials.basic(accountId, licenseKey)`
    pub fn with_basic(
        url: impl Into<String>,
        account_id: impl Into<String>,
        license_key: impl Into<String>,
    ) -> Self {
        Self {
            url: url.into(),
            basic: Some((account_id.into(), license_key.into())),
        }
    }
}

/// 下载响应：保留状态码（上游判定的是 `response.code() == 200`，而不是 2xx）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeoIpHttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl GeoIpHttpResponse {
    pub fn new(status: u16, body: Vec<u8>) -> Self {
        Self { status, body }
    }

    /// 对齐 `IPDB#downloadFile` 的 `response.code() == 200`
    pub fn is_ok(&self) -> bool {
        self.status == 200
    }
}

/// 阻塞式 HTTP 下载接口（上游是同步 OkHttp 调用）。
///
/// 实现必须是可重入的：更新器会按镜像顺序依次调用，单次失败只影响本次下载。
pub trait GeoIpHttpClient: Send + Sync + std::fmt::Debug {
    fn execute(&self, request: GeoIpHttpRequest) -> anyhow::Result<GeoIpHttpResponse>;
}

/// 闭包/函数适配器：便于测试脚本化响应，也便于调用方复用自己的 HTTP 栈。
pub struct ClosureHttpClient<F> {
    f: F,
}

impl<F> std::fmt::Debug for ClosureHttpClient<F> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 闭包不可 Debug，只打印类型名
        formatter
            .debug_struct("ClosureHttpClient")
            .finish_non_exhaustive()
    }
}

impl<F> ClosureHttpClient<F> {
    pub fn new(f: F) -> Self {
        Self { f }
    }
}

impl<F> GeoIpHttpClient for ClosureHttpClient<F>
where
    F: Fn(&GeoIpHttpRequest) -> anyhow::Result<GeoIpHttpResponse> + Send + Sync,
{
    fn execute(&self, request: GeoIpHttpRequest) -> anyhow::Result<GeoIpHttpResponse> {
        (self.f)(&request)
    }
}

/// 生产实现：阻塞式 reqwest（对齐 `IPDB` 的 OkHttp 客户端配置）。
///
/// 上游配置逐项对应：`connectTimeout(15s)` → [`CONNECT_TIMEOUT`]；`readTimeout` 与
/// `callTimeout` 都是 3 分钟 → [`READ_TIMEOUT`]；`followRedirects(true)` → reqwest 默认跟随
/// 重定向（上限 10 次，GitHub Releases 的 `latest/download` 依赖它）；`userAgent` →
/// [`DEFAULT_USER_AGENT`]（上游 `Main.getUserAgent()`）。
///
/// **不要在 async 上下文里直接调用**：`reqwest::blocking` 内部自建运行时，
/// 在 tokio 任务线程上调用会 panic。请用 [`spawn_update`] 或 `spawn_blocking`。
#[derive(Debug)]
pub struct ReqwestBlockingHttpClient {
    client: reqwest::blocking::Client,
}

impl ReqwestBlockingHttpClient {
    pub fn new() -> anyhow::Result<Self> {
        Self::with_user_agent(DEFAULT_USER_AGENT)
    }

    pub fn with_user_agent(user_agent: impl Into<String>) -> anyhow::Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(READ_TIMEOUT)
            .user_agent(user_agent.into())
            .build()?;
        Ok(Self { client })
    }
}

impl GeoIpHttpClient for ReqwestBlockingHttpClient {
    fn execute(&self, request: GeoIpHttpRequest) -> anyhow::Result<GeoIpHttpResponse> {
        let mut builder = self.client.get(&request.url);
        if let Some((user, password)) = &request.basic {
            builder = builder.basic_auth(user, Some(password));
        }
        let response = builder.send()?;
        let status = response.status().as_u16();
        let body = response.bytes()?.to_vec();
        Ok(GeoIpHttpResponse::new(status, body))
    }
}

// ------------------------------------------------------------ 更新判定

/// 对齐 `IPDB#needUpdateMMDB`（原样保留四个分支）。
///
/// 注意：本地文件缺失时恒为 `true`，**即使 `auto_update` 为 false** —— 这正是上游
/// `auto-update: false` 仍会下载一次缺失文件的原因；[`GeoIpUpdater::update_if_needed`]
/// 默认在 `auto_update == false` 时整体旁路，需要该分支时用
/// [`GeoIpUpdater::with_download_missing`]。
pub fn need_update_mmdb(target: &Path, auto_update: bool) -> bool {
    need_update_mmdb_at(target, auto_update, now_millis())
}

/// [`need_update_mmdb`] 的可注入时钟版本（对齐 `System.currentTimeMillis()`）
pub fn need_update_mmdb_at(target: &Path, auto_update: bool, now_ms: i64) -> bool {
    let modified_ms = match std::fs::metadata(target).and_then(|meta| meta.modified()) {
        // `!target.exists()`：不存在（或 metadata 不可读）⇒ 需要更新
        Err(_) => return true,
        Ok(modified) => system_time_millis(modified),
    };
    if !auto_update {
        return false;
    }
    now_ms - modified_ms > MMDB_UPDATE_INTERVAL_MS
}

fn now_millis() -> i64 {
    system_time_millis(SystemTime::now())
}

fn system_time_millis(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_millis() as i64,
        Err(_) => 0,
    }
}

// -------------------------------------------------------------- 报告

/// 更新器整体旁路的原因（都没有产生任何网络请求）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// `pbh.forceDisableIPDB`（对齐 `IPDBManager` 构造函数：为真时连 `setupIPDB` 都不跑）
    ForceDisabled,
    /// `ip-database.auto-update: false` / 配置段缺失
    AutoUpdateDisabled,
}

/// 单个数据库的处理结果
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DatabaseUpdate {
    /// `needUpdateMMDB == false`：未产生网络请求
    UpToDate,
    /// 下载（必要时解压 + mmdb 校验）成功并已原子替换目标文件
    Updated,
    /// 全部镜像失败：`kept_local_copy` 为 true 表示保留了原有的本地数据库文件
    /// （上游对应 `IPDB_EXISTS_UPDATE_FAILED`；false 表示本地从来没有过该库，
    /// 上游对应抛 `IllegalStateException("Download mmdb database failed!")`）
    Failed {
        message: String,
        kept_local_copy: bool,
    },
}

/// 单个数据库的处理报告（顺序与上游构造函数的三次调用一致：City → ASN → GeoCN）
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatabaseReport {
    /// 配置里的数据库名（`database-city` / `database-asn` / `database-geocn`）
    pub database: String,
    /// 落盘目标：`<ipdb_dir>/geoip/<file>`
    pub target: PathBuf,
    /// 最终成功的下载 URL（`UpToDate` / `Failed` 时为 `None`）
    pub source: Option<String>,
    pub update: DatabaseUpdate,
}

/// [`GeoIpUpdater::update_if_needed`] 的结果
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateReport {
    /// 开关旁路（无任何网络请求）
    Skipped(SkipReason),
    /// 逐个数据库的结果
    Performed(Vec<DatabaseReport>),
}

impl UpdateReport {
    pub fn is_skipped(&self) -> bool {
        matches!(self, UpdateReport::Skipped(_))
    }

    /// 本次实际重新下载的数据库
    pub fn updated(&self) -> Vec<&DatabaseReport> {
        self.entries()
            .into_iter()
            .filter(|report| matches!(report.update, DatabaseUpdate::Updated))
            .collect()
    }

    /// 尝试过下载但失败的数据库（含本地保留旧文件的情况）
    pub fn failures(&self) -> Vec<&DatabaseReport> {
        self.entries()
            .into_iter()
            .filter(|report| matches!(report.update, DatabaseUpdate::Failed { .. }))
            .collect()
    }

    fn entries(&self) -> Vec<&DatabaseReport> {
        match self {
            UpdateReport::Skipped(_) => Vec::new(),
            UpdateReport::Performed(entries) => entries.iter().collect(),
        }
    }
}

// ------------------------------------------------------------ 更新器

/// mmdb 更新器（≈ 上游 `IPDB` 构造函数里的更新部分 + `IPDB#updateMMDB`）。
#[derive(Debug)]
pub struct GeoIpUpdater<'a> {
    /// `<data>/ipdb`
    ipdb_dir: PathBuf,
    /// 对齐 `IPDBManager#setupIPDB` 读取的 `ip-database` 段
    config: IpDatabaseConfig,
    http: &'a dyn GeoIpHttpClient,
    /// 镜像模板（默认 [`default_mirrors`] 的三处；`database_name` 由更新器按配置填入）
    mirrors: Vec<IpdbDownloadSource>,
    /// 复刻上游「`auto-update: false` 也下载缺失文件」的分支（默认关闭，见模块文档差异 1）
    download_missing: bool,
}

impl<'a> GeoIpUpdater<'a> {
    pub fn new(
        ipdb_dir: impl Into<PathBuf>,
        config: IpDatabaseConfig,
        http: &'a dyn GeoIpHttpClient,
    ) -> Self {
        // 默认镜像的第一个库名只用于占位，实际下载时会按配置里的数据库名重建 URL
        let placeholder = config.database_city.clone();
        Self {
            ipdb_dir: ipdb_dir.into(),
            config,
            http,
            mirrors: default_mirrors(&placeholder),
            download_missing: false,
        }
    }

    /// 覆盖镜像 baseUrl（全部按上游 `supportXzip = true` 处理）；生产路径保持默认的三处镜像与顺序
    pub fn with_mirror_bases(mut self, bases: impl IntoIterator<Item = String>) -> Self {
        self.mirrors = bases
            .into_iter()
            .map(|base| IpdbDownloadSource::with_xzip(base, String::new()))
            .collect();
        self
    }

    /// 覆盖镜像列表（`database_name` 由更新器按配置填入，便于测试与自建镜像）
    pub fn with_mirrors(mut self, mirrors: impl IntoIterator<Item = IpdbDownloadSource>) -> Self {
        self.mirrors = mirrors.into_iter().collect();
        self
    }

    /// 对齐上游对缺失数据库的强制下载：忽略 `auto-update`，只补「本地不存在」的库
    /// （已存在的库仍按 `auto-update` 决定是否刷新，因此 `auto-update: false` 时不会覆盖本地文件）。
    pub fn with_download_missing(mut self, download_missing: bool) -> Self {
        self.download_missing = download_missing;
        self
    }

    /// `<ipdb_dir>/geoip`（对齐 `new File(dataFolder, "geoip")`）
    pub fn geoip_dir(&self) -> PathBuf {
        self.ipdb_dir.join(GEOIP_DIR)
    }

    /// 对齐 `IPDB` 构造函数：对 City / ASN / GeoCN 依次 `needUpdateMMDB` → `updateMMDB`
    pub fn update_if_needed(&self) -> UpdateReport {
        self.update_if_needed_at(now_millis())
    }

    /// [`GeoIpUpdater::update_if_needed`] 的可注入时钟版本
    pub fn update_if_needed_at(&self, now_ms: i64) -> UpdateReport {
        // `IPDBManager` 构造函数：`pbh.forceDisableIPDB` 为真时完全不加载/更新数据库
        if geoip_force_disabled() {
            return UpdateReport::Skipped(SkipReason::ForceDisabled);
        }
        // 本移植版的既定约定：auto-update 关闭时严格 no-op（一个请求都不发）
        if !self.config.auto_update && !self.download_missing {
            return UpdateReport::Skipped(SkipReason::AutoUpdateDisabled);
        }
        let directory = self.geoip_dir();
        // 上游 `directory.mkdirs()`：失败时后续写盘会失败并降级为「保留本地文件」
        if let Err(e) = std::fs::create_dir_all(&directory) {
            warn!("无法创建 GeoIP 目录 {}: {e}", directory.display());
        }
        let databases = [
            (self.config.database_city.clone(), CITY_MMDB),
            (self.config.database_asn.clone(), ASN_MMDB),
            (self.config.database_geocn.clone(), GEOCN_MMDB),
        ];
        let mut reports = Vec::with_capacity(databases.len());
        for (database_name, file_name) in databases {
            let target = directory.join(file_name);
            let need_update = if self.download_missing {
                // 上游 `needUpdateMMDB` 对缺失文件恒为 true（与 auto-update 无关）
                !target.exists()
            } else {
                need_update_mmdb_at(&target, self.config.auto_update, now_ms)
            };
            if !need_update {
                reports.push(DatabaseReport {
                    database: database_name,
                    target,
                    source: None,
                    update: DatabaseUpdate::UpToDate,
                });
                continue;
            }
            reports.push(self.update_mmdb(&database_name, &target));
        }
        UpdateReport::Performed(reports)
    }

    /// 对齐 `IPDB#updateMMDB`：逐个镜像下载 → 解压 → 校验 → 原子替换，全部失败则保留本地文件
    fn update_mmdb(&self, database_name: &str, target: &Path) -> DatabaseReport {
        // 上游 `log.info(tlUI(Lang.IPDB_UPDATING, databaseName))`
        info!("{}", t(LANG_IPDB_UPDATING, vec![database_name.into()]));
        let tmp = temp_path(target);
        let mirrors = self.mirrors_for(database_name);
        let mut last_error: Option<String> = None;
        for (index, mirror) in mirrors.iter().enumerate() {
            // 上游 `bgTask.setStatusText(new TranslationComponent(Lang.IPDB_DOWNLOAD_MMDB_DESCRIPTION, mirror.getIPDBUrl()))`
            debug!(
                "{}",
                t(
                    LANG_IPDB_DOWNLOAD_MMDB_DESCRIPTION,
                    vec![mirror.url().into()]
                )
            );
            match self.download_and_decompress(mirror, &tmp) {
                Ok(()) => {
                    return match std::fs::rename(&tmp, target) {
                        // 对齐 `Files.move(tmp, target, REPLACE_EXISTING)`：同目录 rename 为原子替换
                        Ok(()) => {
                            info!(
                                "{}",
                                t(LANG_IPDB_UPDATE_SUCCESS, vec![database_name.into()])
                            );
                            DatabaseReport {
                                database: database_name.to_string(),
                                target: target.to_path_buf(),
                                source: Some(mirror.url()),
                                update: DatabaseUpdate::Updated,
                            }
                        }
                        Err(e) => {
                            let _ = std::fs::remove_file(&tmp);
                            let message = format!(
                                "move {} to {} failed: {e}",
                                tmp.display(),
                                target.display()
                            );
                            error!(
                                "{}",
                                t(
                                    LANG_IPDB_UPDATE_FAILED,
                                    vec![database_name.into(), message.clone().into()]
                                )
                            );
                            DatabaseReport {
                                database: database_name.to_string(),
                                target: target.to_path_buf(),
                                source: None,
                                update: DatabaseUpdate::Failed {
                                    message,
                                    kept_local_copy: target.exists(),
                                },
                            }
                        }
                    };
                }
                Err(e) => {
                    // 绝不留下半成品；上游会把临时文件留在系统临时目录
                    let _ = std::fs::remove_file(&tmp);
                    last_error = Some(e.to_string());
                    if index + 1 < mirrors.len() {
                        warn!("{}", t(LANG_IPDB_RETRY_WITH_BACKUP_SOURCE, Vec::new()));
                    }
                }
            }
        }
        let message = last_error.unwrap_or_else(|| "no download mirror configured".to_string());
        error!(
            "{}",
            t(
                LANG_IPDB_UPDATE_FAILED,
                vec![database_name.into(), message.clone().into()]
            )
        );
        let kept_local_copy = target.exists();
        if !kept_local_copy {
            // 上游：`throw new IllegalStateException("Download mmdb database failed!")`
            error!("Download mmdb database failed ({database_name}): {message}");
        } else {
            warn!(
                "{}",
                t(LANG_IPDB_EXISTS_UPDATE_FAILED, vec![database_name.into()])
            );
        }
        DatabaseReport {
            database: database_name.to_string(),
            target: target.to_path_buf(),
            source: None,
            update: DatabaseUpdate::Failed {
                message,
                kept_local_copy,
            },
        }
    }

    /// 对齐 `IPDB#downloadFile` 的单次尝试（含 401 重试与 XZ 解压），成功时 `tmp` 已就绪
    fn download_and_decompress(
        &self,
        mirror: &IpdbDownloadSource,
        tmp: &Path,
    ) -> anyhow::Result<()> {
        let url = mirror.url();
        let mut request = GeoIpHttpRequest::get(&url);
        let mut response = self.http.execute(request.clone())?;
        // 对齐 OkHttp `authenticator`：401 且尚未带凭据时用 `account-id`/`license-key` 重试一次
        // （上游「已经尝试过认证，不再重试」由 `request.header("Authorization") != null` 判定）
        if response.status == 401
            && request.basic.is_none()
            && !self.config.account_id.is_empty()
            && !self.config.license_key.is_empty()
        {
            request = GeoIpHttpRequest::with_basic(
                &url,
                self.config.account_id.clone(),
                self.config.license_key.clone(),
            );
            response = self.http.execute(request)?;
        }
        if !response.is_ok() {
            // 上游：`response.code() + " - " + response.body().string()`
            anyhow::bail!(
                "HTTP {} - {}",
                response.status,
                body_preview(&response.body)
            );
        }
        debug!(
            "{}",
            t(
                LANG_DOWNLOAD_PROGRESS,
                vec![response.body.len().to_string().into()]
            )
        );
        if mirror.support_xzip {
            // 上游把 XZ 解压与 `validateMMDB` 放在同一个 try/catch 里：任一失败都记
            // `IPDB_UNGZIP_FAILED` 并轮换备用源
            let outcome = decompress_xz(&response.body).and_then(|mmdb| {
                std::fs::write(tmp, mmdb)
                    .map_err(|e| anyhow::anyhow!("write {} failed: {e}", tmp.display()))
                    .and_then(|()| validate_mmdb(tmp))
            });
            if let Err(e) = outcome {
                warn!(
                    "{}",
                    t(
                        LANG_IPDB_UNGZIP_FAILED,
                        vec![mirror.database_name.clone().into()]
                    )
                );
                return Err(e);
            }
            Ok(())
        } else {
            // 上游非 XZ 分支：直接落盘且**不校验**（镜像都支持 XZ，该分支仅保留语义）
            std::fs::write(tmp, &response.body)
                .map_err(|e| anyhow::anyhow!("write {} failed: {e}", tmp.display()))
        }
    }

    /// 对齐 `IPDB#updateMMDB` 的三处 `new IPDBDownloadSource(baseUrl, databaseName, true)`：
    /// 把配置里的数据库名填进镜像模板
    fn mirrors_for(&self, database_name: &str) -> Vec<IpdbDownloadSource> {
        self.mirrors
            .iter()
            .map(|mirror| IpdbDownloadSource {
                base_url: mirror.base_url.clone(),
                database_name: database_name.to_string(),
                support_xzip: mirror.support_xzip,
            })
            .collect()
    }
}

/// 对齐 `IPDB#validateMMDB`：用 MaxMind 读取器打开一次（元数据校验不通过即失败），
/// 上游在成功时打印 `databaseType`，这里同样输出到 `debug`。
fn validate_mmdb(path: &Path) -> anyhow::Result<()> {
    let reader = maxminddb::Reader::open_readfile(path)
        .map_err(|e| anyhow::anyhow!("validate {} failed: {e}", path.display()))?;
    debug!(
        "Validate mmdb {} success: {}",
        path.display(),
        reader.metadata().database_type
    );
    Ok(())
}

/// 临时文件路径：与目标同目录同名（`.<文件名>.<pid>.<序号>.tmp`）以保证 `rename` 原子替换。
fn temp_path(target: &Path) -> PathBuf {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "geoip".to_string());
    target.with_file_name(format!(
        ".{file_name}.{}.{sequence}.tmp",
        std::process::id()
    ))
}

/// 上游把整个响应体拼进失败日志；这里截断到 [`BODY_PREVIEW_LIMIT`] 个字符（UTF-8 无损截断）
fn body_preview(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    if text.chars().count() <= BODY_PREVIEW_LIMIT {
        text.to_string()
    } else {
        let mut truncated: String = text.chars().take(BODY_PREVIEW_LIMIT).collect();
        truncated.push('…');
        truncated
    }
}

// --------------------------------------------------------------- XZ 解压

/// XZ 解压（对齐上游 `org.tukaani.xz.XZInputStream`）。
///
/// 用纯 Rust 的 `lzma-rs`，不依赖系统 liblzma（避免额外原生库）。
/// 这里是解压的唯一入口，便于替换实现或注入 mock。
pub fn decompress_xz(input: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut output = Vec::new();
    lzma_rs::xz_decompress(&mut BufReader::new(input), &mut output)
        .map_err(|e| anyhow::anyhow!("xz decompress failed: {e}"))?;
    Ok(output)
}

// ------------------------------------------------------ 后台更新（启动接线）

/// 对齐 `IPDBManager` 构造函数：`CompletableFuture.runAsync(this::setupIPDB)` ——
/// 在独立线程里做更新，不阻塞启动流程，也让阻塞式 HTTP 客户端远离 async 运行时的线程。
pub fn spawn_update(
    ipdb_dir: impl Into<PathBuf>,
    config: IpDatabaseConfig,
    http: Arc<dyn GeoIpHttpClient>,
) -> std::io::Result<std::thread::JoinHandle<UpdateReport>> {
    spawn_update_with_mirrors(ipdb_dir, config, http, None)
}

/// 同 [`spawn_update`]，但可覆盖镜像列表（`database_name` 由更新器填入；`None` = 上游默认三处镜像）
pub fn spawn_update_with_mirrors(
    ipdb_dir: impl Into<PathBuf>,
    config: IpDatabaseConfig,
    http: Arc<dyn GeoIpHttpClient>,
    mirrors: Option<Vec<IpdbDownloadSource>>,
) -> std::io::Result<std::thread::JoinHandle<UpdateReport>> {
    let ipdb_dir = ipdb_dir.into();
    std::thread::Builder::new()
        .name("pbh-geoip-update".to_string())
        .spawn(move || {
            let updater = GeoIpUpdater::new(ipdb_dir, config, http.as_ref());
            let updater = match mirrors {
                Some(mirrors) => updater.with_mirrors(mirrors),
                None => updater,
            };
            updater.update_if_needed()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geoip::GeoIpDb;
    use std::collections::HashSet;
    use std::fs;
    use std::sync::Mutex;

    // ---------------------------------------------------------- 测试脚手架

    /// 独占临时目录（Drop 时递归删除）
    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        fn new(name: &str) -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "pbh-core-geoip-update-{name}-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create test dir");
            Self { path }
        }

        fn ipdb_dir(&self) -> PathBuf {
            self.path.join(IPDB_DIR)
        }

        /// `<dir>/ipdb/geoip`
        fn geoip_dir(&self) -> PathBuf {
            self.ipdb_dir().join(GEOIP_DIR)
        }

        /// 目录内的全部文件名（用于断言「没有半成品临时文件残留」）
        fn file_names(&self) -> HashSet<String> {
            let mut names = HashSet::new();
            if let Ok(entries) = fs::read_dir(self.geoip_dir()) {
                for entry in entries.flatten() {
                    names.insert(entry.file_name().to_string_lossy().to_string());
                }
            }
            names
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    /// 脚本化响应
    #[derive(Clone, Debug)]
    enum Script {
        /// 200 + body
        Body(Vec<u8>),
        /// 指定状态码 + 空 body
        Status(u16),
        /// 传输层错误（连接失败/超时）
        Transport(String),
    }

    /// 计数 + 脚本化 mock（URL 精确匹配，未命中即 404）
    #[derive(Debug, Default)]
    struct MockHttpClient {
        /// `(url, 是否限定带 Basic 凭据, 脚本)`；`None` = 不限定
        scripts: Mutex<Vec<(String, Option<bool>, Script)>>,
        requests: Mutex<Vec<GeoIpHttpRequest>>,
    }

    impl MockHttpClient {
        fn serve(self, url: &str, script: Script) -> Self {
            self.scripts
                .lock()
                .unwrap()
                .push((url.to_string(), None, script));
            self
        }

        /// 只在请求带/不带 Basic 凭据时命中（用于 401 重试）
        fn serve_with_basic(self, url: &str, has_basic: bool, script: Script) -> Self {
            self.scripts
                .lock()
                .unwrap()
                .push((url.to_string(), Some(has_basic), script));
            self
        }

        fn calls(&self) -> Vec<GeoIpHttpRequest> {
            self.calls_impl()
        }

        fn call_count(&self) -> usize {
            self.calls_impl().len()
        }

        fn calls_impl(&self) -> Vec<GeoIpHttpRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl GeoIpHttpClient for MockHttpClient {
        fn execute(&self, request: GeoIpHttpRequest) -> anyhow::Result<GeoIpHttpResponse> {
            self.requests.lock().unwrap().push(request.clone());
            let scripts = self.scripts.lock().unwrap();
            let found = scripts.iter().find(|(url, has_basic, _)| {
                url == &request.url
                    && has_basic
                        .map(|expected| expected == request.basic.is_some())
                        .unwrap_or(true)
            });
            match found.map(|(_, _, script)| script) {
                Some(Script::Body(body)) => Ok(GeoIpHttpResponse::new(200, body.clone())),
                Some(Script::Status(status)) => Ok(GeoIpHttpResponse::new(*status, Vec::new())),
                Some(Script::Transport(message)) => Err(anyhow::anyhow!(message.clone())),
                None => Ok(GeoIpHttpResponse::new(404, Vec::new())),
            }
        }
    }

    const TEST_MIRROR: &str = "https://mirror.test/ipdb/";

    fn mirror_url(database_name: &str) -> String {
        format!("{TEST_MIRROR}{database_name}{XZ_SUFFIX}")
    }

    fn updater<'a>(
        dir: &TestDir,
        config: IpDatabaseConfig,
        http: &'a dyn GeoIpHttpClient,
    ) -> GeoIpUpdater<'a> {
        GeoIpUpdater::new(dir.ipdb_dir(), config, http)
            .with_mirror_bases(vec![TEST_MIRROR.to_string()])
    }

    fn auto_update_config() -> IpDatabaseConfig {
        IpDatabaseConfig {
            auto_update: true,
            ..IpDatabaseConfig::default()
        }
    }

    /// 文件 mtime（毫秒，对齐 `File.lastModified()`）
    fn mtime_millis(path: &Path) -> i64 {
        system_time_millis(
            fs::metadata(path)
                .and_then(|meta| meta.modified())
                .expect("mtime"),
        )
    }

    /// 把 mtime 改成 `age_ms` 毫秒之前（用于触发 45 天间隔逻辑）
    fn set_age(path: &Path, age_ms: i64) {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open for set_modified");
        let when = SystemTime::now() - Duration::from_millis(age_ms as u64);
        file.set_modified(when).expect("set mtime");
    }

    /// 三个数据库各自的合法 mmdb 夹具，第三个元素是 XZ 压缩后的下载体
    fn fixtures() -> [(&'static str, Vec<u8>, Vec<u8>); 3] {
        ["GeoLite2-City", "GeoLite2-ASN", "GeoCN"].map(|name| {
            let mmdb = minimal_mmdb(name);
            let mut xz = Vec::new();
            lzma_rs::xz_compress(&mut BufReader::new(&mmdb[..]), &mut xz).expect("xz compress");
            (name, mmdb, xz)
        })
    }

    fn serve_all(fixtures: &[(&str, Vec<u8>, Vec<u8>); 3]) -> MockHttpClient {
        let mut mock = MockHttpClient::default();
        for (name, _, xz) in fixtures {
            mock = mock.serve(&mirror_url(name), Script::Body(xz.clone()));
        }
        mock
    }

    // ------------------------------------------------- 最小合法 mmdb 夹具

    /// 生成一个最小的合法 MaxMind DB（v2 二进制格式）：
    /// 搜索树只有 1 个节点，两个记录都指向数据段偏移 0 处的空 map，
    /// 因此任何查询都会命中一条「字段全空」的记录，且 `Reader::from_source` 的全部校验通过。
    fn minimal_mmdb(database_type: &str) -> Vec<u8> {
        const RECORD_SIZE: u64 = 24;
        const NODE_COUNT: u64 = 1;
        const DATA_SECTION_SEPARATOR_SIZE: usize = 16;
        const METADATA_MARKER: &[u8] = b"\xab\xcd\xefMaxMind.com";

        let mut file = Vec::new();
        // 数据段偏移 = record - node_count - 16，因此指向偏移 0 的 record = 1 + 16 = 17
        let data_pointer = NODE_COUNT + DATA_SECTION_SEPARATOR_SIZE as u64;
        for _ in 0..2 {
            file.extend_from_slice(&data_pointer.to_be_bytes()[5..]); // 24 位记录
        }
        file.extend(std::iter::repeat_n(0u8, DATA_SECTION_SEPARATOR_SIZE));
        file.push(0xE0); // 数据段偏移 0：空 map
        file.extend_from_slice(METADATA_MARKER);
        file.extend_from_slice(&metadata_map(database_type, NODE_COUNT, RECORD_SIZE));
        file
    }

    fn metadata_map(database_type: &str, node_count: u64, record_size: u64) -> Vec<u8> {
        let mut out = mmdb_ctrl(7, 9); // map，9 个键
        out.extend(mmdb_str("binary_format_major_version"));
        out.extend(mmdb_uint(5, 2)); // uint16
        out.extend(mmdb_str("binary_format_minor_version"));
        out.extend(mmdb_uint(5, 0));
        out.extend(mmdb_str("build_epoch"));
        out.extend(mmdb_uint(9, 1_700_000_000)); // uint64
        out.extend(mmdb_str("database_type"));
        out.extend(mmdb_str(database_type));
        out.extend(mmdb_str("description"));
        out.extend(mmdb_ctrl(7, 1));
        out.extend(mmdb_str("en"));
        out.extend(mmdb_str("PBH-RS GeoIP update fixture"));
        out.extend(mmdb_str("ip_version"));
        out.extend(mmdb_uint(5, 4)); // uint16
        out.extend(mmdb_str("languages"));
        out.extend(mmdb_ctrl(11, 1)); // array
        out.extend(mmdb_str("en"));
        out.extend(mmdb_str("node_count"));
        out.extend(mmdb_uint(6, node_count)); // uint32
        out.extend(mmdb_str("record_size"));
        out.extend(mmdb_uint(5, record_size));
        out
    }

    /// MaxMind 数据格式控制字节：基础类型（0-7）用高 3 位；扩展类型（8-15）先写 0x00 打开扩展，
    /// 紧跟 `type - 7` 字节，size 仍在首字节低 5 位（与真实 GeoLite2/GeoCN 文件一致）。
    fn mmdb_ctrl(type_num: u8, size: usize) -> Vec<u8> {
        assert!(size < 29, "夹具只使用短 size");
        if type_num < 8 {
            vec![(type_num << 5) | size as u8]
        } else {
            vec![size as u8, type_num - 7]
        }
    }

    fn mmdb_str(value: &str) -> Vec<u8> {
        let mut out = mmdb_ctrl(2, value.len());
        out.extend_from_slice(value.as_bytes());
        out
    }

    /// 整数按最短大端字节数编码（0 用 size 0），与真实文件一致
    fn mmdb_uint(type_num: u8, value: u64) -> Vec<u8> {
        let bytes = value.to_be_bytes();
        let start = bytes
            .iter()
            .position(|byte| *byte != 0)
            .unwrap_or(bytes.len());
        let payload = &bytes[start..];
        let mut out = mmdb_ctrl(type_num, payload.len());
        out.extend_from_slice(payload);
        out
    }

    // ---------------------------------------------------------------- 测试

    #[test]
    fn upstream_mirrors_and_url_shape() {
        // 上游 `IPDB#updateMMDB`：mirror1 / mirror3 / mirror4，顺序逐字一致
        let mirrors = default_mirrors("GeoLite2-City");
        assert_eq!(mirrors.len(), 3);
        assert_eq!(
            mirrors[0].url(),
            "https://github.com/PBH-BTN/GeoLite.mmdb/releases/latest/download/GeoLite2-City.mmdb.xz"
        );
        assert_eq!(
            mirrors[1].url(),
            "https://pbh-static.paulzzh.com/ipdb/GeoLite2-City.mmdb.xz"
        );
        assert_eq!(
            mirrors[2].url(),
            "https://pbh-static.ghostchu.com/ipdb/GeoLite2-City.mmdb.xz"
        );
        assert!(mirrors.iter().all(|mirror| mirror.support_xzip));
        // 2 参构造（`IPDBDownloadSource(baseUrl, databaseName)`）不带 .xz
        assert_eq!(
            IpdbDownloadSource::new("https://example.test/", "GeoCN").url(),
            "https://example.test/GeoCN.mmdb"
        );
    }

    #[test]
    fn need_update_mmdb_matches_upstream_branches() {
        let dir = TestDir::new("need-update");
        fs::create_dir_all(dir.geoip_dir()).unwrap();
        let target = dir.geoip_dir().join(CITY_MMDB);

        // 分支 1：文件不存在 ⇒ true（与 auto_update 无关）
        assert!(need_update_mmdb_at(&target, true, now_millis()));
        assert!(need_update_mmdb_at(&target, false, now_millis()));

        fs::write(&target, b"mmdb").unwrap();
        // 以文件真实 mtime 为基准（避免文件系统时间戳精度影响边界断言）
        let modified = mtime_millis(&target);
        // 分支 2：存在且 auto_update == false ⇒ false
        assert!(!need_update_mmdb_at(&target, false, modified));
        assert!(!need_update_mmdb_at(
            &target,
            false,
            modified + MMDB_UPDATE_INTERVAL_MS * 10
        ));
        // 分支 3：存在且未超过 45 天 ⇒ false
        assert!(!need_update_mmdb_at(&target, true, modified));
        assert!(!need_update_mmdb_at(
            &target,
            true,
            modified + MMDB_UPDATE_INTERVAL_MS
        ));
        // 分支 4：严格超过 45 天（`3888000000L` 毫秒）⇒ true
        assert!(need_update_mmdb_at(
            &target,
            true,
            modified + MMDB_UPDATE_INTERVAL_MS + 1
        ));
    }

    #[test]
    fn auto_update_disabled_is_strict_no_op() {
        let dir = TestDir::new("disabled");
        fs::create_dir_all(dir.geoip_dir()).unwrap();
        let city = dir.geoip_dir().join(CITY_MMDB);
        fs::write(&city, b"existing-city").unwrap();

        let fixtures = fixtures();
        let mock = serve_all(&fixtures);
        // `IpDatabaseConfig::default()` 的 auto_update 就是 false（缺省即关闭）
        let report = updater(&dir, IpDatabaseConfig::default(), &mock).update_if_needed();

        assert_eq!(
            report,
            UpdateReport::Skipped(SkipReason::AutoUpdateDisabled),
            "auto-update 关闭时整体旁路"
        );
        assert_eq!(mock.call_count(), 0, "关闭时必须一个请求都不发");
        assert!(report.updated().is_empty());
        assert!(report.failures().is_empty());
        // 既有文件保持原样，且没有半成品残留
        assert_eq!(fs::read(&city).unwrap(), b"existing-city");
        assert_eq!(
            dir.file_names(),
            HashSet::from([CITY_MMDB.to_string()]),
            "不应凭空创建目录/文件"
        );
    }

    #[test]
    fn missing_databases_are_downloaded_and_loadable() {
        let dir = TestDir::new("success");
        let fixtures = fixtures();
        let mock = serve_all(&fixtures);

        let report = updater(&dir, auto_update_config(), &mock).update_if_needed();

        let entries = match &report {
            UpdateReport::Performed(entries) => entries.clone(),
            other => panic!("期望逐个数据库的结果，实际 {other:?}"),
        };
        assert_eq!(entries.len(), 3, "City → ASN → GeoCN 三个库");
        assert_eq!(entries[0].database, "GeoLite2-City");
        assert_eq!(entries[1].database, "GeoLite2-ASN");
        assert_eq!(entries[2].database, "GeoCN");
        for entry in &entries {
            assert_eq!(entry.update, DatabaseUpdate::Updated);
            let expected_source = mirror_url(&entry.database);
            assert_eq!(entry.source.as_deref(), Some(expected_source.as_str()));
        }
        // 请求顺序 = 上游构造函数顺序，且 URL 是 `.mmdb.xz`
        assert_eq!(
            mock.calls()
                .iter()
                .map(|req| req.url.clone())
                .collect::<Vec<_>>(),
            vec![
                mirror_url("GeoLite2-City"),
                mirror_url("GeoLite2-ASN"),
                mirror_url("GeoCN"),
            ]
        );

        // 落盘的是解压后的 mmdb（不是 .xz），且三个库都能被 `GeoIpDb::load` 打开
        for (name, mmdb, _) in &fixtures {
            let file_name = match *name {
                "GeoLite2-City" => CITY_MMDB,
                "GeoLite2-ASN" => ASN_MMDB,
                _ => GEOCN_MMDB,
            };
            assert_eq!(&fs::read(dir.geoip_dir().join(file_name)).unwrap(), mmdb);
        }
        assert_eq!(
            dir.file_names(),
            HashSet::from([
                CITY_MMDB.to_string(),
                ASN_MMDB.to_string(),
                GEOCN_MMDB.to_string()
            ]),
            "不应残留临时文件"
        );
        let db = GeoIpDb::load(dir.ipdb_dir()).expect("下载后的三个 mmdb 均可加载");
        let data = db.query_geo("1.2.3.4".parse().unwrap());
        assert_eq!(
            data.country.and_then(|country| country.iso),
            None,
            "夹具记录字段全空"
        );
        assert!(data.as_data.is_some());
    }

    #[test]
    fn download_missing_reproduces_upstream_forced_download() {
        let dir = TestDir::new("download-missing");
        fs::create_dir_all(dir.geoip_dir()).unwrap();
        let city = dir.geoip_dir().join(CITY_MMDB);
        fs::write(&city, b"stale-city").unwrap();
        set_age(&city, MMDB_UPDATE_INTERVAL_MS * 2); // 早已过期

        let fixtures = fixtures();
        let mock = serve_all(&fixtures);
        let updater = updater(&dir, IpDatabaseConfig::default(), &mock).with_download_missing(true);
        let report = updater.update_if_needed();

        let entries = report.entries().into_iter().cloned().collect::<Vec<_>>();
        assert_eq!(entries.len(), 3);
        // 已存在的 City 不刷新（`auto-update: false`），缺失的 ASN/GeoCN 被补下来
        assert_eq!(entries[0].update, DatabaseUpdate::UpToDate);
        assert_eq!(entries[1].update, DatabaseUpdate::Updated);
        assert_eq!(entries[2].update, DatabaseUpdate::Updated);
        assert_eq!(
            mock.calls()
                .iter()
                .map(|req| req.url.clone())
                .collect::<Vec<_>>(),
            vec![mirror_url("GeoLite2-ASN"), mirror_url("GeoCN")]
        );
        assert_eq!(fs::read(&city).unwrap(), b"stale-city");
    }

    #[test]
    fn http_failure_keeps_existing_files_and_leaves_no_temp_files() {
        let dir = TestDir::new("http-failure");
        fs::create_dir_all(dir.geoip_dir()).unwrap();
        let city = dir.geoip_dir().join(CITY_MMDB);
        fs::write(&city, b"existing-city").unwrap();
        set_age(&city, MMDB_UPDATE_INTERVAL_MS * 2);

        let mock = MockHttpClient::default()
            .serve(&mirror_url("GeoLite2-City"), Script::Status(500))
            .serve(
                &mirror_url("GeoLite2-ASN"),
                Script::Transport("network unreachable".into()),
            )
            .serve(&mirror_url("GeoCN"), Script::Status(404));

        let report = updater(&dir, auto_update_config(), &mock).update_if_needed();

        let failures = report.failures();
        assert_eq!(failures.len(), 3, "三个库全部失败");
        // City 本地有旧文件 ⇒ 保留；ASN / GeoCN 本地从来没有过（上游抛 IllegalStateException 的情形）
        assert_eq!(
            failures
                .iter()
                .map(|failure| failure.update.clone())
                .collect::<Vec<_>>(),
            vec![
                DatabaseUpdate::Failed {
                    message: "HTTP 500 - ".to_string(),
                    kept_local_copy: true,
                },
                DatabaseUpdate::Failed {
                    message: "network unreachable".to_string(),
                    kept_local_copy: false,
                },
                DatabaseUpdate::Failed {
                    message: "HTTP 404 - ".to_string(),
                    kept_local_copy: false,
                },
            ]
        );
        // 既有文件逐字节未动，且没有半成品
        assert_eq!(fs::read(&city).unwrap(), b"existing-city");
        assert_eq!(dir.file_names(), HashSet::from([CITY_MMDB.to_string()]));
    }

    #[test]
    fn transport_error_falls_back_to_backup_mirror() {
        let dir = TestDir::new("fallback");
        let fixtures = fixtures();
        let backup = "https://backup.test/ipdb/";
        let mock = MockHttpClient::default()
            .serve(
                &format!("{TEST_MIRROR}GeoLite2-City{XZ_SUFFIX}"),
                Script::Status(503),
            )
            .serve(
                &format!("{backup}GeoLite2-City{XZ_SUFFIX}"),
                Script::Body(fixtures[0].2.clone()),
            )
            .serve(
                &format!("{TEST_MIRROR}GeoLite2-ASN{XZ_SUFFIX}"),
                Script::Transport("timeout".into()),
            )
            .serve(
                &format!("{backup}GeoLite2-ASN{XZ_SUFFIX}"),
                Script::Body(fixtures[1].2.clone()),
            )
            .serve(
                &format!("{TEST_MIRROR}GeoCN{XZ_SUFFIX}"),
                Script::Status(500),
            )
            .serve(
                &format!("{backup}GeoCN{XZ_SUFFIX}"),
                Script::Body(fixtures[2].2.clone()),
            );

        let updater = GeoIpUpdater::new(dir.ipdb_dir(), auto_update_config(), &mock)
            .with_mirror_bases(vec![TEST_MIRROR.to_string(), backup.to_string()]);
        let report = updater.update_if_needed();

        assert!(report.failures().is_empty(), "备用源应全部补齐");
        assert_eq!(report.updated().len(), 3);
        let expected_source = format!("{backup}GeoLite2-City{XZ_SUFFIX}");
        assert_eq!(
            report.updated()[0].source.as_deref(),
            Some(expected_source.as_str())
        );
        // 每个库先试主源、失败后换备用源
        assert_eq!(mock.call_count(), 6);
        assert_eq!(
            &fs::read(dir.geoip_dir().join(CITY_MMDB)).unwrap(),
            &fixtures[0].1
        );
        assert_eq!(dir.file_names().len(), 3);
    }

    #[test]
    fn corrupt_xz_body_never_replaces_database() {
        let dir = TestDir::new("corrupt-xz");
        fs::create_dir_all(dir.geoip_dir()).unwrap();
        let city = dir.geoip_dir().join(CITY_MMDB);
        fs::write(&city, b"existing-city").unwrap();
        set_age(&city, MMDB_UPDATE_INTERVAL_MS * 2);

        let mock = MockHttpClient::default()
            .serve(
                &mirror_url("GeoLite2-City"),
                Script::Body(b"not an xz stream".to_vec()),
            )
            .serve(
                &mirror_url("GeoLite2-ASN"),
                Script::Body(vec![0xFD, 0x37, 0x7A, 0x58, 0x5A, 0x00]),
            )
            .serve(&mirror_url("GeoCN"), Script::Body(Vec::new()));

        let report = updater(&dir, auto_update_config(), &mock).update_if_needed();

        assert_eq!(report.failures().len(), 3);
        assert_eq!(
            fs::read(&city).unwrap(),
            b"existing-city",
            "损坏体不得替换数据库"
        );
        assert_eq!(
            dir.file_names(),
            HashSet::from([CITY_MMDB.to_string()]),
            "半成品临时文件必须被清理"
        );
    }

    #[test]
    fn xz_of_non_mmdb_is_rejected_by_validation() {
        let dir = TestDir::new("invalid-mmdb");
        let mut xz_of_garbage = Vec::new();
        lzma_rs::xz_compress(
            &mut BufReader::new(&b"definitely not a mmdb"[..]),
            &mut xz_of_garbage,
        )
        .unwrap();

        let mock = MockHttpClient::default()
            .serve(
                &mirror_url("GeoLite2-City"),
                Script::Body(xz_of_garbage.clone()),
            )
            .serve(
                &mirror_url("GeoLite2-ASN"),
                Script::Body(xz_of_garbage.clone()),
            )
            .serve(&mirror_url("GeoCN"), Script::Body(xz_of_garbage));

        let report = updater(&dir, auto_update_config(), &mock).update_if_needed();

        // 解压成功但 `validateMMDB` 失败 ⇒ 视同失败，不落盘（本地从未有过 ⇒ kept_local_copy = false）
        assert_eq!(report.failures().len(), 3);
        for failure in report.failures() {
            match &failure.update {
                DatabaseUpdate::Failed {
                    message,
                    kept_local_copy,
                } => {
                    assert!(
                        message.starts_with("validate ") && message.contains("invalid database"),
                        "错误信息应为 MaxMind 读取器的失败原因，实际: {message}"
                    );
                    assert!(!kept_local_copy, "本地从未有过该库");
                }
                other => panic!("期望校验失败，实际 {other:?}"),
            }
        }
        assert!(dir.file_names().is_empty(), "不校验通过就绝不落盘");
    }

    #[test]
    fn plain_mmdb_mirror_skips_decompression() {
        let dir = TestDir::new("plain");
        let fixtures = fixtures();
        // 非 XZ 源（`IPDBDownloadSource(baseUrl, databaseName)`）：直接落盘且不校验
        let mock = MockHttpClient::default()
            .serve(
                &format!("{TEST_MIRROR}GeoLite2-City{MMDB_SUFFIX}"),
                Script::Body(fixtures[0].1.clone()),
            )
            .serve(
                &format!("{TEST_MIRROR}GeoLite2-ASN{MMDB_SUFFIX}"),
                Script::Body(fixtures[1].1.clone()),
            )
            .serve(
                &format!("{TEST_MIRROR}GeoCN{MMDB_SUFFIX}"),
                Script::Body(fixtures[2].1.clone()),
            );

        let updater = GeoIpUpdater::new(dir.ipdb_dir(), auto_update_config(), &mock)
            .with_mirrors(vec![IpdbDownloadSource::new(TEST_MIRROR, String::new())]);
        let report = updater.update_if_needed();

        assert_eq!(report.updated().len(), 3);
        assert_eq!(
            &fs::read(dir.geoip_dir().join(CITY_MMDB)).unwrap(),
            &fixtures[0].1
        );
        assert!(GeoIpDb::load(dir.ipdb_dir()).is_ok());
    }

    #[test]
    fn stale_database_is_refreshed_but_fresh_one_is_skipped() {
        let dir = TestDir::new("stale");
        fs::create_dir_all(dir.geoip_dir()).unwrap();
        let city = dir.geoip_dir().join(CITY_MMDB);
        let asn = dir.geoip_dir().join(ASN_MMDB);
        let fixtures = fixtures();
        fs::write(&city, b"stale-city").unwrap();
        fs::write(&asn, b"fresh-asn").unwrap();
        set_age(&city, MMDB_UPDATE_INTERVAL_MS * 2);

        let mock = MockHttpClient::default()
            .serve(
                &mirror_url("GeoLite2-City"),
                Script::Body(fixtures[0].2.clone()),
            )
            .serve(&mirror_url("GeoCN"), Script::Body(fixtures[2].2.clone()));

        let report = updater(&dir, auto_update_config(), &mock).update_if_needed();

        let entries = report.entries().into_iter().cloned().collect::<Vec<_>>();
        assert_eq!(
            entries[0].update,
            DatabaseUpdate::Updated,
            "45 天前的 City 应刷新"
        );
        assert_eq!(entries[1].update, DatabaseUpdate::UpToDate, "新 ASN 不刷新");
        assert_eq!(
            entries[2].update,
            DatabaseUpdate::Updated,
            "缺失的 GeoCN 应补齐"
        );
        assert_eq!(&fs::read(&city).unwrap(), &fixtures[0].1);
        assert_eq!(fs::read(&asn).unwrap(), b"fresh-asn");
        assert_eq!(
            mock.calls()
                .iter()
                .map(|req| req.url.clone())
                .collect::<Vec<_>>(),
            vec![mirror_url("GeoLite2-City"), mirror_url("GeoCN")]
        );
    }

    #[test]
    fn auth_retry_uses_account_credentials_on_401() {
        let dir = TestDir::new("auth");
        let fixtures = fixtures();
        let mock = MockHttpClient::default()
            // 未带凭据 ⇒ 401（对齐 OkHttp authenticator 的触发条件）
            .serve_with_basic(&mirror_url("GeoLite2-City"), false, Script::Status(401))
            .serve_with_basic(
                &mirror_url("GeoLite2-City"),
                true,
                Script::Body(fixtures[0].2.clone()),
            )
            .serve_with_basic(&mirror_url("GeoLite2-ASN"), false, Script::Status(401))
            .serve_with_basic(
                &mirror_url("GeoLite2-ASN"),
                true,
                Script::Body(fixtures[1].2.clone()),
            )
            .serve_with_basic(&mirror_url("GeoCN"), false, Script::Status(401))
            .serve_with_basic(
                &mirror_url("GeoCN"),
                true,
                Script::Body(fixtures[2].2.clone()),
            );

        let config = IpDatabaseConfig {
            auto_update: true,
            account_id: "account".to_string(),
            license_key: "license".to_string(),
            ..IpDatabaseConfig::default()
        };
        let report = updater(&dir, config, &mock).update_if_needed();

        assert_eq!(report.updated().len(), 3);
        let calls = mock.calls();
        assert_eq!(calls.len(), 6, "每个库 401 后带凭据重试一次");
        assert_eq!(calls[0].basic, None);
        assert_eq!(
            calls[1].basic,
            Some(("account".to_string(), "license".to_string()))
        );
    }

    #[test]
    fn without_credentials_401_is_a_failure() {
        let dir = TestDir::new("no-credentials");
        let mock = MockHttpClient::default()
            .serve(&mirror_url("GeoLite2-City"), Script::Status(401))
            .serve(&mirror_url("GeoLite2-ASN"), Script::Status(401))
            .serve(&mirror_url("GeoCN"), Script::Status(401));

        let report = updater(&dir, auto_update_config(), &mock).update_if_needed();

        assert_eq!(report.failures().len(), 3);
        assert_eq!(mock.call_count(), 3, "没有凭据就不重试");
    }

    #[test]
    fn spawn_update_runs_off_thread_and_respects_the_switch() {
        let dir = TestDir::new("spawn");
        let fixtures = fixtures();

        // auto-update 关闭 ⇒ 线程里同样严格 no-op
        let mock = Arc::new(serve_all(&fixtures));
        let handle = spawn_update(dir.ipdb_dir(), IpDatabaseConfig::default(), mock.clone())
            .expect("spawn update thread");
        assert_eq!(
            handle.join().expect("join update thread"),
            UpdateReport::Skipped(SkipReason::AutoUpdateDisabled)
        );
        assert_eq!(mock.call_count(), 0);

        // 打开 auto-update ⇒ 后台线程完成三个库的下载（镜像可注入，测试不出网）
        let mock = Arc::new(serve_all(&fixtures));
        let handle = spawn_update_with_mirrors(
            dir.ipdb_dir(),
            auto_update_config(),
            mock.clone(),
            Some(vec![IpdbDownloadSource::with_xzip(
                TEST_MIRROR,
                String::new(),
            )]),
        )
        .expect("spawn update thread");
        let report = handle.join().expect("join update thread");
        assert_eq!(report.updated().len(), 3);
        assert_eq!(mock.call_count(), 3);
        assert!(GeoIpDb::load(dir.ipdb_dir()).is_ok());
    }

    #[test]
    fn decompress_xz_rejects_non_xz_body() {
        assert!(decompress_xz(b"not an xz stream").is_err());
        assert!(decompress_xz(&[]).is_err());
        let fixtures = fixtures();
        assert_eq!(decompress_xz(&fixtures[0].2).unwrap(), fixtures[0].1);
    }

    #[test]
    fn body_preview_is_truncated() {
        assert_eq!(body_preview(b"ok"), "ok");
        assert_eq!(body_preview(&[0xFF, 0xFE]), "\u{fffd}\u{fffd}");
        let long = "a".repeat(BODY_PREVIEW_LIMIT + 10);
        let preview = body_preview(long.as_bytes());
        assert_eq!(preview.chars().count(), BODY_PREVIEW_LIMIT + 1);
        assert!(preview.ends_with('…'));
    }
}
