//! 领域模型：PeerAddress / PeerFlag / Peer / Torrent。
//! 字段命名与 qBittorrent Web API（`sync/torrentPeers`、`torrents/info`）一一对应，
//! 行为契约见 SPEC.md 第 2 节。

use rhai::CustomType;
use serde::{Deserialize, Serialize};

/// Peer 地址包装器。`raw_ip` 为下载器返回的 peers Map 键（`ip:port` / `[v6]:port`）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PeerAddress {
    /// 用于判定的 IP（可能经 NAT/Teredo 转换）
    pub ip: String,
    pub port: u16,
    /// 下载器报告的原始 `ip:port` 键
    pub raw_ip: String,
}

impl PeerAddress {
    pub fn new(ip: impl Into<String>, port: u16, raw_ip: impl Into<String>) -> Self {
        Self {
            ip: ip.into(),
            port,
            raw_ip: raw_ip.into(),
        }
    }
    /// 缓存键，对齐上游 Peer.getCacheKey：`ip:port`
    pub fn cache_key(&self) -> String {
        format!("{}:{}", self.ip, self.port)
    }
}

/// libtorrent 连接 flags，解析逻辑对齐上游 `PeerFlag.parseLibTorrent`（SPEC 2.2 [GOLDEN]）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerFlag {
    pub raw: String,
    pub interesting: bool,
    pub choked: bool,
    pub remote_interested: bool,
    pub remote_choked: bool,
    pub local_connection: bool,
    pub optimistic_unchoke: bool,
    pub snubbed: bool,
    pub from_dht: bool,
    pub from_pex: bool,
    pub from_lsd: bool,
    pub rc4_encrypted: bool,
    pub plaintext_encrypted: bool,
    pub utp_socket: bool,
}

impl PeerFlag {
    pub fn parse(flags: &str) -> Self {
        // 与 Java 初始值一致
        let mut interesting = false;
        let mut remote_choked = true;
        let mut remote_interested = false;
        let mut choked = true;
        let mut optimistic_unchoke = false;
        let mut snubbed = false;
        let mut local_connection = true;
        let mut from_dht = false;
        let mut from_pex = false;
        let mut from_lsd = false;
        let mut rc4_encrypted = false;
        let mut plaintext_encrypted = false;
        let mut utp_socket = false;
        for c in flags.chars() {
            match c {
                'd' => {
                    interesting = true;
                    remote_choked = true;
                }
                'D' => {
                    interesting = true;
                    remote_choked = false;
                }
                'u' => {
                    remote_interested = true;
                    choked = true;
                }
                'U' => {
                    remote_interested = true;
                    choked = false;
                }
                'K' => {
                    remote_choked = false;
                    interesting = false;
                }
                '?' => {
                    choked = false;
                    remote_interested = false;
                }
                'O' => optimistic_unchoke = true,
                'S' => snubbed = true,
                'I' => local_connection = false,
                'H' => from_dht = true,
                'X' => from_pex = true,
                'L' => from_lsd = true,
                'E' => rc4_encrypted = true,
                'e' => plaintext_encrypted = true,
                'P' => utp_socket = true,
                _ => {}
            }
        }
        PeerFlag {
            raw: flags.to_string(),
            interesting,
            choked,
            remote_interested,
            remote_choked,
            local_connection,
            optimistic_unchoke,
            snubbed,
            from_dht,
            from_pex,
            from_lsd,
            rc4_encrypted,
            plaintext_encrypted,
            utp_socket,
        }
    }

    pub fn is_from_incoming(&self) -> bool {
        // 上游 RunCheckModuleOrgan 中用于 NAT 误配提醒的简化判定
        !self.outgoing_connection()
    }

    pub fn outgoing_connection(&self) -> bool {
        // libtorrent flags 中没有直接字符；本地连接且非入站语义保守处理
        self.local_connection
    }
}

/// 一个对等体的观测数据（与下载器无关的统一表示）。
#[derive(Debug, Clone, CustomType, Serialize, Deserialize)]
pub struct PeerData {
    pub client_name: Option<String>,
    pub peer_id: Option<String>,
    pub dl_speed: i64,
    pub downloaded: i64,
    pub up_speed: i64,
    pub uploaded: i64,
    pub progress: f64,
    pub flags: Option<String>,
    pub ip: String,
    pub port: u16,
    pub raw_ip: String,
    pub connection: Option<String>,
}

impl PeerData {
    pub fn address(&self) -> PeerAddress {
        PeerAddress::new(self.ip.clone(), self.port, self.raw_ip.clone())
    }
    pub fn peer_flag(&self) -> Option<PeerFlag> {
        self.flags
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(PeerFlag::parse)
    }
    /// 握手中：up_speed<=0 && dl_speed<=0（SPEC 2.3 [GOLDEN]）
    pub fn is_handshaking(&self) -> bool {
        self.up_speed <= 0 && self.dl_speed <= 0
    }
    /// 我们是否正在向该 peer 上传（PCB 判定前置条件）
    pub fn is_uploading_to_peer(&self) -> bool {
        self.up_speed > 0 || self.uploaded > 0
    }
}

/// 一个 torrent 的观测数据。
#[derive(Debug, Clone, CustomType, Serialize, Deserialize)]
pub struct TorrentData {
    pub hash: String,
    pub name: String,
    pub progress: f64,
    pub total_size: i64,
    pub piece_size: i64,
    pub pieces_have: i64,
    /// 部分下载器（如 Transmission）直接用别的口径给出完成量，此时覆盖默认计算
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_override: Option<i64>,
    pub dlspeed: i64,
    pub upspeed: i64,
    pub is_private: Option<bool>,
}

impl TorrentData {
    pub fn id(&self) -> &str {
        &self.hash
    }
    /// 对齐 QBittorrentTorrent.getCompletedSize；下载器给出覆盖值时优先使用
    pub fn completed_size(&self) -> i64 {
        if let Some(completed) = self.completed_override {
            return completed;
        }
        if self.piece_size > 0 && self.pieces_have > 0 {
            self.piece_size * self.pieces_have
        } else {
            -1
        }
    }
    pub fn is_private(&self) -> bool {
        self.is_private.unwrap_or(false)
    }
    pub fn is_seeding(&self) -> bool {
        self.progress >= 1.0
    }
}
