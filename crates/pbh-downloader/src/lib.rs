//! 下载器统一抽象（对齐 Java `Downloader` 接口的本阶段所需子集）。

pub mod aria2;
pub mod biglybt;
pub mod bitcomet;
pub mod deluge;
pub mod http;
pub mod qbittorrent;
pub mod transmission;

pub use aria2::{Aria2Config, Aria2Downloader};
pub use biglybt::{BiglyBtConfig, BiglyBtDownloader};
pub use bitcomet::{BitCometConfig, BitCometDownloader};
pub use deluge::{DelugeConfig, DelugeDownloader};
pub use http::{BoxFuture, HttpFetcher, HttpRequest, HttpResponse, ReqwestFetcher};
pub use qbittorrent::{QBConfig, QBittorrentDownloader};
pub use transmission::{TRConfig, TransmissionDownloader};

use pbh_core::model::{PeerData, TorrentData};

/// 登录/健康检查结果
#[derive(Clone, Debug)]
pub struct LoginResult {
    pub success: bool,
    pub message: String,
    pub version: String,
}

/// 下载器概要统计（对齐 alltime_ul / alltime_dl）
#[derive(Clone, Debug, Default)]
pub struct DownloaderStatistics {
    pub all_time_upload: i64,
    pub all_time_download: i64,
}

/// 待封禁 peer（raw_ip 用于增量封禁载荷）
#[derive(Clone, Debug)]
pub struct BanEntry {
    pub ip: String,
    pub port: u16,
    pub raw_ip: String,
}

/// 下载器特性标志（与 Java `DownloaderFeatureFlag` 枚举同名）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DownloaderFeature {
    ReadPeerProtocols,
    UnbanIp,
    TrafficStats,
    LiveUpdateBtProtocolPort,
    RangeBanIp,
}

impl DownloaderFeature {
    pub fn name(&self) -> &'static str {
        match self {
            DownloaderFeature::ReadPeerProtocols => "READ_PEER_PROTOCOLS",
            DownloaderFeature::UnbanIp => "UNBAN_IP",
            DownloaderFeature::TrafficStats => "TRAFFIC_STATS",
            DownloaderFeature::LiveUpdateBtProtocolPort => "LIVE_UPDATE_BT_PROTOCOL_PORT",
            DownloaderFeature::RangeBanIp => "RANGE_BAN_IP",
        }
    }
}

/// 下载器统一接口。使用显式 boxed future 以支持 `dyn Downloader` 且 future 为 `Send`。
pub trait Downloader: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;
    fn downloader_type(&self) -> &'static str;
    fn feature_flags(&self) -> Vec<String>;

    fn login<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<LoginResult>>;
    fn fetch_torrents<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<Vec<TorrentData>>>;
    fn fetch_peers<'a>(
        &'a self,
        torrent: &'a TorrentData,
    ) -> BoxFuture<'a, anyhow::Result<Vec<PeerData>>>;
    fn ban_peers<'a>(&'a self, peers: &'a [BanEntry]) -> BoxFuture<'a, anyhow::Result<()>>;
    fn replace_banned_ips<'a>(&'a self, ips: &'a [String]) -> BoxFuture<'a, anyhow::Result<()>>;
    fn statistics<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<DownloaderStatistics>>;

    /// 读取当前限速，返回 `(upload, download)`。
    ///
    /// 对齐 Java `Downloader#getSpeedLimiter()`（返回 `DownloaderSpeedLimiter(upload, download)`）：
    /// - 单位统一为 **bytes/s**。各适配器负责与自己的原生单位换算，并在实现处注明
    ///   （qB / BiglyBT / BitComet / Aria2 原生即 bytes/s；Transmission 用 KB/s ×1024，
    ///   Deluge 用 KiB/s ×1024）。
    /// - `<= 0` 表示「不限制」（对齐 `DownloaderSpeedLimiter.isUploadUnlimited()` /
    ///   `isDownloadUnlimited()`：`<= 0` 即无限制）。Transmission / Aria2 在读取时会按
    ///   各自上游逻辑把「未启用限速」映射为 `0`。
    /// - Java 用 `null` 表达「不支持限速或请求失败」（调用方 `ActiveMonitoringModule`
    ///   据此 `continue` 跳过该下载器）；本移植用 `Err` 表达同一语义：调用方
    ///   （`pbh::monitor::collect_traffic_stats`）记日志后按 `None` 处理，行为等价。
    fn get_speed_limiter<'a>(&'a self) -> BoxFuture<'a, anyhow::Result<(i64, i64)>>;

    /// 设置当前限速（`upload` / `download`，单位 **bytes/s**，`<= 0` 表示不限制）。
    ///
    /// 对齐 Java `Downloader#setSpeedLimiter(DownloaderSpeedLimiter)`：各适配器在自己的
    /// 原生单位与 bytes/s 之间换算，并把「不限制」翻译成对应后端的表达
    /// （qB / Deluge / BitComet / Aria2 用 0，Transmission 用 `*-enabled = false`，
    /// BiglyBT 原样透传）。失败语义逐适配器对齐上游（有的抛错、有的只记日志）。
    fn set_speed_limiter<'a>(
        &'a self,
        upload: i64,
        download: i64,
    ) -> BoxFuture<'a, anyhow::Result<()>>;
}
