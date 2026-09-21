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
}
