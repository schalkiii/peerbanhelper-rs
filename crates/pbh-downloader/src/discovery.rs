//! 本机下载器自动发现（对齐上游 `DownloaderDiscovery`）。
//!
//! 流程：枚举本机 TCP LISTEN 端点（Windows `GetExtendedTcpTable` / Linux
//! `/proc/net/tcp*`），对每个端点发一次 `GET /`，按响应头/正文识别
//! qBittorrent / Transmission / BiglyBT / BitComet。另附加常见默认端口作为兜底
//! （枚举不可用的平台，如 macOS）。
//!
//! 探测失败、超时、非下载器 WebUI 一律忽略（对齐上游 `checkDownloader` 返回 null）。

use serde::Serialize;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

/// 探测超时（上游 `callTimeout(8s)`）。
const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
const PROBE_CONCURRENCY: usize = 64;
/// 兜底端口：qBittorrent / Transmission / Deluge / BitComet / aria2。
const FALLBACK_PORTS: [u16; 5] = [8080, 9091, 8112, 12345, 6800];

/// 一个被发现的下载器（字段对齐上游 `DiscoveredDownloader`）。
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DiscoveredDownloader {
    pub host: String,
    pub port: u16,
    #[serde(rename = "type")]
    pub kind: String,
    pub pid: u32,
    /// `NEW` / `ADDED`（上游 `DiscoverStatus`）
    pub added: String,
}

/// 扫描本机监听端口并识别下载器。
pub async fn scan(added_ports: &[u16]) -> Vec<DiscoveredDownloader> {
    let mut candidates = listening_tcp_ports();
    for port in FALLBACK_PORTS {
        candidates.push((IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), port, 0));
    }
    scan_endpoints(candidates, added_ports).await
}

/// 对给定端点执行探测（与 [`scan`] 分离：测试可注入空候选，避免真实扫端口）。
pub async fn scan_endpoints(
    candidates: Vec<(IpAddr, u16, u32)>,
    added_ports: &[u16],
) -> Vec<DiscoveredDownloader> {
    // 同一端点只探测一次
    let mut candidates = candidates;
    candidates.sort_by_key(|(addr, port, _)| (addr.to_string(), *port));
    candidates.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

    let semaphore = Arc::new(Semaphore::new(PROBE_CONCURRENCY));
    let added_ports = added_ports.to_vec();
    let mut tasks = JoinSet::new();
    for (addr, port, pid) in candidates {
        if port == 0 {
            continue;
        }
        let semaphore = semaphore.clone();
        let added_ports = added_ports.clone();
        tasks.spawn(async move {
            let _permit = semaphore.acquire_owned().await.ok()?;
            let host = if addr.is_unspecified() {
                "127.0.0.1".to_string()
            } else {
                addr.to_string()
            };
            probe(&host, port, pid, &added_ports).await
        });
    }

    let mut found = Vec::new();
    while let Some(result) = tasks.join_next().await {
        if let Ok(Some(item)) = result {
            found.push(item);
        }
    }
    found.sort_by(|a, b| a.host.cmp(&b.host).then(a.port.cmp(&b.port)));
    found.dedup_by(|a, b| a.host == b.host && a.port == b.port);
    found
}

/// 探测一个端点并识别下载器类型；非下载器 / 不可达返回 `None`。
pub async fn probe(
    host: &str,
    port: u16,
    pid: u32,
    added_ports: &[u16],
) -> Option<DiscoveredDownloader> {
    // 本机端点探测不应经过 HTTP 代理（即使全局配置了代理）
    let client = reqwest::Client::builder()
        .timeout(PROBE_TIMEOUT)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()
        .ok()?;
    let url = if host.contains(':') {
        format!("http://[{host}]:{port}/")
    } else {
        format!("http://{host}:{port}/")
    };
    let response = client.get(url).send().await.ok()?;
    let server = response
        .headers()
        .get("server")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let authenticate = response
        .headers()
        .get("www-authenticate")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = response.text().await.unwrap_or_default();

    let kind = if server.contains("Transmission") {
        "transmission"
    } else if server.contains("PeerBanHelper-BiglyBT-Adapter") {
        "biglybt"
    } else if authenticate.contains("BitComet Remote Login")
        || body.contains("BitComet Remote Access")
        || body.contains("BitComet WebUI")
    {
        "bitcomet"
    } else if body.contains("qBittorrent WebUI") {
        "qbittorrent"
    } else {
        return None;
    };
    Some(DiscoveredDownloader {
        host: host.to_string(),
        port,
        kind: kind.to_string(),
        pid,
        added: if added_ports.contains(&port) {
            "ADDED"
        } else {
            "NEW"
        }
        .to_string(),
    })
}

// ---------------------------------------------------------------- 监听端口

#[cfg(windows)]
fn listening_tcp_ports() -> Vec<(IpAddr, u16, u32)> {
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCPROW_OWNER_PID,
        TCP_TABLE_OWNER_PID_LISTENER,
    };
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

    let mut result = Vec::new();
    // IPv4
    let mut size = 0u32;
    unsafe {
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            AF_INET as u32,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
    }
    if size > 0 {
        let mut buffer = vec![0u8; size as usize];
        let status = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr() as *mut _,
                &mut size,
                0,
                AF_INET as u32,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if status == 0 {
            let count = unsafe { *(buffer.as_ptr() as *const u32) } as usize;
            let rows = unsafe { buffer.as_ptr().add(4) as *const MIB_TCPROW_OWNER_PID };
            for index in 0..count {
                let row = unsafe { &*rows.add(index) };
                let address = std::net::Ipv4Addr::from(row.dwLocalAddr.swap_bytes());
                let port = u16::from_be((row.dwLocalPort & 0xFFFF) as u16);
                result.push((IpAddr::V4(address), port, row.dwOwningPid));
            }
        }
    }
    // IPv6
    let mut size = 0u32;
    unsafe {
        GetExtendedTcpTable(
            std::ptr::null_mut(),
            &mut size,
            0,
            AF_INET6 as u32,
            TCP_TABLE_OWNER_PID_LISTENER,
            0,
        );
    }
    if size > 0 {
        let mut buffer = vec![0u8; size as usize];
        let status = unsafe {
            GetExtendedTcpTable(
                buffer.as_mut_ptr() as *mut _,
                &mut size,
                0,
                AF_INET6 as u32,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            )
        };
        if status == 0 {
            let count = unsafe { *(buffer.as_ptr() as *const u32) } as usize;
            let rows = unsafe { buffer.as_ptr().add(4) as *const MIB_TCP6ROW_OWNER_PID };
            for index in 0..count {
                let row = unsafe { &*rows.add(index) };
                let address = std::net::Ipv6Addr::from(row.ucLocalAddr);
                let port = u16::from_be((row.dwLocalPort & 0xFFFF) as u16);
                result.push((IpAddr::V6(address), port, row.dwOwningPid));
            }
        }
    }
    result
}

#[cfg(target_os = "linux")]
fn listening_tcp_ports() -> Vec<(IpAddr, u16, u32)> {
    let mut result = Vec::new();
    for (path, v6) in [("/proc/net/tcp", false), ("/proc/net/tcp6", true)] {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < 4 || fields[3] != "0A" {
                continue; // 0A = TCP_LISTEN
            }
            let Some((address, port)) = parse_proc_endpoint(fields[1], v6) else {
                continue;
            };
            result.push((address, port, 0));
        }
    }
    result
}

#[cfg(not(any(windows, target_os = "linux")))]
fn listening_tcp_ports() -> Vec<(IpAddr, u16, u32)> {
    Vec::new()
}

/// 解析 `/proc/net/tcp{,6}` 的 `local_address`（`0100007F:1F90` 形式）。
#[cfg(target_os = "linux")]
fn parse_proc_endpoint(value: &str, v6: bool) -> Option<(IpAddr, u16)> {
    let (address, port) = value.split_once(':')?;
    let port = u16::from_str_radix(port, 16).ok()?;
    if v6 {
        // 4 组 32-bit 小端十六进制
        let mut bytes = [0u8; 16];
        for group in 0..4 {
            let chunk = address.get(group * 8..(group + 1) * 8)?;
            let word = u32::from_str_radix(chunk, 16).ok()?.swap_bytes();
            bytes[group * 4..group * 4 + 4].copy_from_slice(&word.to_ne_bytes());
        }
        Some((IpAddr::V6(std::net::Ipv6Addr::from(bytes)), port))
    } else {
        let word = u32::from_str_radix(address, 16).ok()?.swap_bytes();
        Some((IpAddr::V4(std::net::Ipv4Addr::from(word)), port))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// 起一个只回一次 HTTP 响应的本地服务，验证识别逻辑。
    fn serve_once(response: &'static str) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        port
    }

    /// 验证平台枚举确实能拿到当前进程刚绑定的 LISTEN 端口
    /// （Windows `GetExtendedTcpTable` / Linux `/proc/net/tcp`）。
    #[test]
    fn enumerates_a_bound_listening_socket() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let ports = listening_tcp_ports();
        assert!(
            ports.iter().any(|(_, candidate, _)| *candidate == port),
            "本地监听端口 {port} 未出现在枚举结果中：{ports:?}"
        );
    }

    #[tokio::test]
    async fn identifies_transmission_by_server_header() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\nServer: Transmission\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let found = probe("127.0.0.1", port, 42, &[port]).await.expect("应识别");
        assert_eq!(found.kind, "transmission");
        assert_eq!(found.pid, 42);
        assert_eq!(found.added, "ADDED");
    }

    #[tokio::test]
    async fn identifies_qbittorrent_by_body_and_ignores_other_webui() {
        let qb = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 20\r\nConnection: close\r\n\r\nqBittorrent WebUI v5",
        );
        assert_eq!(
            probe("127.0.0.1", qb, 0, &[]).await.unwrap().kind,
            "qbittorrent"
        );
        let other = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nhello",
        );
        assert!(probe("127.0.0.1", other, 0, &[]).await.is_none());
    }

    /// 空候选必须立即返回空数组（Web 契约测试用它代替真实扫端口）。
    #[tokio::test]
    async fn scan_endpoints_with_no_candidates_is_empty() {
        assert!(scan_endpoints(Vec::new(), &[]).await.is_empty());
    }
}
