//! PTR 反查（对齐上游 `DNSLookupImpl` + `lookup.dns-reverse-lookup`）。
//!
//! 解析链：
//! 1. `resolvers.servers` 按书写顺序尝试：
//!    - 普通 DNS：`1.1.1.1`、`8.8.8.8:5353`、`[2001:4860:4860::8888]:53`；
//!    - DoH：`https://dns.google/dns-query`（POST `application/dns-message`）；
//! 2. `resolvers.use-system: true`（默认）时用系统解析器（`getnameinfo`）。
//!
//! 单次查询上限 3 秒（对齐上游 `ExtendedResolver.setTimeout(3s)`）。
//! [`PtrLookup::lookup`] 是**阻塞**调用：Web 层须放在 `spawn_blocking` 中执行。
//!
//! 上游用虚拟线程做异步反查（几乎无成本）；Rust 的 OS 线程不便宜，因此
//! `wave` 侧统一经 [`PtrLookup::submit`] 投递到固定大小 worker 池，
//! 系统解析器的阻塞线程也由独立小池承载，避免「每个封禁两条线程」的无界增长。

use crate::config::ResolversConfig;
use crate::iputil::reverse_dns_name;
use crate::modules::ptr_blacklist::{build_ptr_query, parse_ptr_answer, PTR_LOOKUP_TIMEOUT};
use std::net::{IpAddr, UdpSocket};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// PTR 反查任务池的固定线程数（wave 异步回填）。
const PTR_WORKER_THREADS: usize = 4;
/// 系统解析器（`getnameinfo` 无超时接口）的固定线程数。
const SYSTEM_WORKER_THREADS: usize = 4;

/// 一个 DNS 解析器（普通 UDP 或 DoH）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum DnsServer {
    Udp(IpAddr, u16),
    Doh(String),
}

/// 固定大小的任务池：队列无界、线程数恒定（首个任务投递时才创建线程）。
#[derive(Debug)]
struct WorkerPool {
    sender: mpsc::Sender<Box<dyn FnOnce() + Send + 'static>>,
    workers: usize,
}

impl WorkerPool {
    fn new(workers: usize, name: &str) -> Self {
        let (sender, receiver) = mpsc::channel::<Box<dyn FnOnce() + Send + 'static>>();
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..workers {
            let receiver = receiver.clone();
            let _ = std::thread::Builder::new()
                .name(format!("{name}-{index}"))
                .spawn(move || loop {
                    let job = {
                        let guard = receiver.lock().unwrap_or_else(|e| e.into_inner());
                        guard.recv()
                    };
                    match job {
                        Ok(job) => job(),
                        Err(_) => break,
                    }
                });
        }
        Self { sender, workers }
    }

    fn submit(&self, job: Box<dyn FnOnce() + Send + 'static>) {
        if self.sender.send(job).is_err() {
            tracing::warn!("PTR worker 池（{} 线程）已退出，反查任务被丢弃", self.workers);
        }
    }
}

/// PTR 查询器（可在多线程间共享）。
#[derive(Debug, Clone)]
pub struct PtrLookup {
    /// `lookup.dns-reverse-lookup`：仅控制「封禁元数据 / peers 列表」是否反查；
    /// `/api/peer/{ip}` 与上游一致，始终尝试解析。
    enabled: bool,
    use_system: bool,
    servers: Vec<DnsServer>,
    timeout: Duration,
    /// 懒初始化的反查任务池（禁用 PTR 的实例不会创建任何线程）
    pool: Arc<OnceLock<WorkerPool>>,
}

impl PtrLookup {
    pub fn from_config(config: &ResolversConfig, enabled: bool) -> Self {
        let mut servers = Vec::new();
        for server in &config.servers {
            match parse_server(server) {
                Some(parsed) => servers.push(parsed),
                None => tracing::warn!("无法解析 resolvers.servers 条目（已跳过）：{server}"),
            }
        }
        Self {
            enabled,
            use_system: config.use_system,
            servers,
            timeout: PTR_LOOKUP_TIMEOUT,
            pool: Arc::new(OnceLock::new()),
        }
    }

    /// 无解析器的空实现（测试 / 未配置 DNS 时使用）。
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            use_system: false,
            servers: Vec::new(),
            timeout: PTR_LOOKUP_TIMEOUT,
            pool: Arc::new(OnceLock::new()),
        }
    }

    /// 把反查任务投递到固定大小 worker 池（不阻塞调用方）。
    ///
    /// 任务在池内串行排队执行，线程数不超过 [`PTR_WORKER_THREADS`]；
    /// 首个任务投递时才创建线程，禁用 PTR 的实例没有额外开销。
    pub fn submit<F>(&self, job: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.pool
            .get_or_init(|| WorkerPool::new(PTR_WORKER_THREADS, "pbh-ptr"))
            .submit(Box::new(job));
    }

    /// 池已创建的 worker 线程数（测试用：断言线程数有界）。
    #[cfg(test)]
    fn worker_threads(&self) -> usize {
        self.pool.get().map(|pool| pool.workers).unwrap_or(0)
    }

    /// `lookup.dns-reverse-lookup` 是否开启（封禁元数据、peers 列表使用）。
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// 反查 IP 的 PTR 名；无记录 / 超时 / 未配置解析器时返回 `None`。
    ///
    /// **阻塞调用**（普通 DNS 最多 3s、DoH 最多 3s、系统解析器最多 3s）：
    /// tokio 上下文中必须经 `spawn_blocking` 调用。
    pub fn lookup(&self, ip: &str) -> Option<String> {
        let reverse = reverse_dns_name(ip)?;
        let query = build_ptr_query(&reverse)?;
        for server in &self.servers {
            let answer = match server {
                DnsServer::Udp(address, port) => {
                    udp_query(*address, *port, &query, self.timeout)
                }
                DnsServer::Doh(url) => doh_query(url, &query, self.timeout),
            };
            if let Some(name) = answer {
                return Some(ensure_fqdn(name));
            }
        }
        if self.use_system {
            let address: IpAddr = crate::iputil::parse_addr(ip)?;
            return system_lookup(address, self.timeout).map(ensure_fqdn);
        }
        None
    }
}

impl Default for PtrLookup {
    fn default() -> Self {
        Self::disabled()
    }
}

/// 向指定 DNS 服务器发一次 UDP PTR 查询（支持自定义端口）。
fn udp_query(address: IpAddr, port: u16, query: &[u8], timeout: Duration) -> Option<String> {
    let bind = if address.is_ipv4() {
        "0.0.0.0:0"
    } else {
        "[::]:0"
    };
    let socket = UdpSocket::bind(bind).ok()?;
    socket.connect((address, port)).ok()?;
    socket.set_read_timeout(Some(timeout)).ok()?;
    socket.set_write_timeout(Some(timeout)).ok()?;
    socket.send(query).ok()?;
    let mut buffer = [0u8; 4096];
    let received = socket.recv(&mut buffer).ok()?;
    parse_ptr_answer(&buffer[..received])
}

/// DoH（RFC 8484 POST）：请求与响应均为 DNS 线格式。
fn doh_query(url: &str, query: &[u8], timeout: Duration) -> Option<String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .ok()?;
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/dns-message")
        .header(reqwest::header::ACCEPT, "application/dns-message")
        .body(query.to_vec())
        .send()
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let bytes = response.bytes().ok()?;
    parse_ptr_answer(&bytes)
}

/// 系统解析器查询（`getnameinfo`）带等待上限：超时后返回 `None`，
/// 后台任务会在系统解析器自身超时后结束。
///
/// 阻塞线程由固定大小池承载（`getnameinfo` 没有超时参数，取消不了，
/// 线程数必须有上界）；池满时任务排队，调用方最多等待 `timeout`。
fn system_lookup(address: IpAddr, timeout: Duration) -> Option<String> {
    static POOL: OnceLock<WorkerPool> = OnceLock::new();
    let pool = POOL.get_or_init(|| WorkerPool::new(SYSTEM_WORKER_THREADS, "pbh-ptr-system"));
    let (sender, receiver) = mpsc::channel();
    pool.submit(Box::new(move || {
        let result = dns_lookup::lookup_addr(&address);
        let _ = sender.send(result);
    }));
    match receiver.recv_timeout(timeout) {
        Ok(Ok(name)) if !name.is_empty() => Some(name),
        _ => None,
    }
}

/// 上游 dnsjava `PTRRecord.getTarget().toString()` 输出带尾点的绝对域名
/// （如 `dns.google.`）；系统 `getnameinfo` 通常省略尾点，这里统一补齐。
fn ensure_fqdn(name: String) -> String {
    if name.ends_with('.') {
        name
    } else {
        format!("{name}.")
    }
}

/// 解析 `resolvers.servers` 条目：
/// `https://...` → DoH；`IP` / `IP:port` / `[IPv6]:port` → UDP（缺省 53）。
fn parse_server(value: &str) -> Option<DnsServer> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if value.starts_with("https://") || value.starts_with("http://") {
        return Some(DnsServer::Doh(value.to_string()));
    }
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Some(DnsServer::Udp(ip, 53));
    }
    if let Some(rest) = value.strip_prefix('[') {
        let (host, tail) = rest.split_once(']')?;
        let ip = host.parse().ok()?;
        let port = match tail.strip_prefix(':') {
            Some(port) => port.parse().ok()?,
            None => 53,
        };
        return Some(DnsServer::Udp(ip, port));
    }
    let (host, port) = value.rsplit_once(':')?;
    Some(DnsServer::Udp(host.parse().ok()?, port.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn parses_plain_port_ipv6_and_doh_servers() {
        assert_eq!(
            parse_server("1.1.1.1"),
            Some(DnsServer::Udp("1.1.1.1".parse().unwrap(), 53))
        );
        assert_eq!(
            parse_server("8.8.8.8:5353"),
            Some(DnsServer::Udp("8.8.8.8".parse().unwrap(), 5353))
        );
        assert_eq!(
            parse_server("[2001:4860:4860::8888]:53"),
            Some(DnsServer::Udp(
                "2001:4860:4860::8888".parse().unwrap(),
                53
            ))
        );
        assert_eq!(
            parse_server("https://dns.google/dns-query"),
            Some(DnsServer::Doh("https://dns.google/dns-query".into()))
        );
        assert_eq!(parse_server("not-an-ip"), None);
    }

    /// 构造一个带 A/PTR 答案的 DNS 响应：question 原样复制，answer 指向 0xC00C。
    fn ptr_response(query: &[u8], target: &str) -> Vec<u8> {
        let mut response = query.to_vec();
        response[2] = 0x81; // QR=1, RD=1
        response[3] = 0x80; // RA=1, RCODE=0
        response[6] = 0x00;
        response[7] = 0x01; // ANCOUNT=1
        let mut rdata = Vec::new();
        for label in target.split('.') {
            rdata.push(label.len() as u8);
            rdata.extend_from_slice(label.as_bytes());
        }
        rdata.push(0);
        response.extend_from_slice(&[0xC0, 0x0C]); // name pointer → question
        response.extend_from_slice(&[0x00, 0x0C, 0x00, 0x01]); // PTR / IN
        response.extend_from_slice(&[0x00, 0x00, 0x00, 0x3C]); // TTL=60
        response.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        response.extend_from_slice(&rdata);
        response
    }

    #[test]
    fn doh_post_query_parses_ptr_answer() {
        let query = build_ptr_query("4.3.2.1.in-addr.arpa").unwrap();
        let response_body = ptr_response(&query, "host.example.com");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/dns-message\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response_body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&response_body);
            }
        });
        let url = format!("http://127.0.0.1:{port}/dns-query");
        assert_eq!(
            doh_query(&url, &query, Duration::from_secs(3)),
            Some("host.example.com".to_string())
        );
    }

    #[test]
    fn fqdn_gets_a_trailing_dot_once() {
        assert_eq!(ensure_fqdn("dns.google".into()), "dns.google.");
        assert_eq!(ensure_fqdn("dns.google.".into()), "dns.google.");
    }

    #[test]
    fn disabled_lookup_returns_none() {
        let ptr = PtrLookup::disabled();
        assert!(!ptr.enabled());
        assert!(ptr.lookup("1.2.3.4").is_none());
    }

    /// 大量封禁任务的异步回填不得演变成「一任务一线程」。
    #[test]
    fn submit_pool_bounds_worker_threads() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let ptr = PtrLookup::disabled();
        let done = Arc::new(AtomicUsize::new(0));
        for _ in 0..64 {
            let done = done.clone();
            ptr.submit(move || {
                std::thread::sleep(Duration::from_millis(1));
                done.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert!(
            ptr.worker_threads() <= PTR_WORKER_THREADS,
            "worker 线程数 {} 超过上限 {PTR_WORKER_THREADS}",
            ptr.worker_threads()
        );
        // 任务仍应全部执行（队列无界，不丢任务）
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while done.load(Ordering::SeqCst) < 64 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(done.load(Ordering::SeqCst), 64);
    }
}
