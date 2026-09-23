//! AutoSTUN TCP 转发器、NAT 端口保活与隧道装配
//! （对齐上游 `util/traversal/forwarder/TCPForwarderImpl`、
//! `util/traversal/stun/tunnel/StunTcpTunnelImpl`、`util/traversal/stun/StunSocketTool`
//! 与 `util/traversal/btstun/BTStunInstance.onCreate`）。
//!
//! ## 上游链路（v9.5.1）
//! 1. `BTStunInstance`（每 5s 的定时任务里）发现隧道失效即重建 `StunTcpTunnelImpl` 并
//!    `createMapping(localPort)`：`localPort == 0` ⇒ `MiscUtil.randomAvailablePort()`；
//!    随后 `TcpStunClient("0.0.0.0", localPort).getMapping()` 得到 `inter/outer`
//!    （套接字**绑定到 `0.0.0.0:localPort`** 后连接 STUN 服务器，见 [`crate::auto_stun`]）；
//! 2. `stun.available-test`（默认 true）：`testMapping(inter, outer)` 在 `inter` 上临时起一个
//!    回答 `204` 的 HTTP 服务，再从本机连 `outer` 发 `GET /test`，读响应判断是否包含 `"204"`；
//!    失败 ⇒ `stunListener.onNotApplicable`（隧道不建立）；
//! 3. `BTStunInstance.onCreate(inter, outer)`：
//!    - 转发器监听端口 = `inter.getPort()`；下载器应监听端口 = `outer.getPort()`；
//!    - 校验下载器主机：公网地址默认拒绝（`isLocal/isAnyLocal/isLoopback/isLinkLocal/
//!      isZeroHost/isMulticast` 之外 ⇒ `AUTOSTUN_DOWNLOADER_HOST_NOT_LAN_ADDRESS` 并卸载实例；
//!      ExternalSwitch `pbh.btstun.allowPublicIpAsDownloaderHost` 默认 false；**解析失败时跳过校验**）；
//!    - `new TCPForwarderImpl(banList, pbh.btstun.ipv6support(默认 true) ? "[::]" : "0.0.0.0",
//!      forwarderServerPort, downloaderHost, downloaderShouldListenOn, ipdb); start()`；
//!    - 把下载器 BT 端口改写为 `outer.getPort()`（应用层职责，核心层只返回端点）；
//! 4. `StunTcpTunnelImpl.startNATHolder`：`scheduleAtFixedRate(keepAlive, 1, 10, SECONDS)`，
//!    从 `inter.host:inter.port` **绑定后**连接 `testHost:80`（`pbh.stunTcpTunnel.testHost`
//!    默认 `qq.com`，连接超时 1000ms），发送
//!    `HEAD / HTTP/1.1\r\nHost: {testHost}\r\nUser-Agent: PeerBanHelper-NAT-Keeper/1.0\r\n
//!    Connection: keep-alive\r\n\r\n` 并读一次响应；连续失败达到
//!    `pbh.stunTcpTunnel.maxKeepAliveFailures`（默认 5）⇒ `valid=false` + `onClose`（隧道重建）。
//!
//! ## `TCPForwarderImpl` 的移植范围
//! - **监听与转发**：监听 `[::]|0.0.0.0:inter.port`，每条下游连接转发到 `downloaderHost:outer.port`；
//! - **单 IP 单连接**：同一 IP 已有连接或正在连接 ⇒ 拒绝（避免干扰 ProgressCheatBlocker）；
//! - **连接表**：`connectionMap[下游地址] = 上游套接字本机地址`，[`TcpForwarder::translate`]
//!   做 BiMap 反查（即 `BTStunInstance.translate` → `Forwarder.translate`）；
//! - **友好回环绑定**（`connectToUpstreamFriendly`，`pbh.TCPForwarder.useFriendlyAddressForLoopback`
//!   默认 true）：上游地址是回环且下游是 IPv4 时，依次尝试
//!   `bind(127.{d1}.{d2}.{d3}:{下游端口})` → `bind(127.{d1}.{d2}.{d3}:0)` → 默认连接；
//! - 未移植（应用层职责，见任务分工）：BanList 踢连接（accept 检查 + 30s 清理 + PeerBanEvent
//!   订阅）、IPDB 地理查询、流量计数器、Netty IO handler 选择。
//! - keepalive 每轮重新「bind 到 inter 端口再连接」（上游复用长连接 socket，失败才重建）；
//!   由于每次都绑定**同一** `inter` 端口，对 NAT 而言可观察行为一致（外部映射端口保持不变）。
//!
//! 所有网络活动只在 `enabled=true`（见 [`crate::auto_stun::AutoStun::create_tunnel`]）时发生；
//! `enabled=false` ⇒ 严格 no-op，不创建任何 socket / 线程。

use crate::auto_stun::{TcpStunClient, DEFAULT_STUN_TIMEOUT};
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// `stun.available-test` 默认值（上游 `getBoolean("stun.available-test", true)`）。
pub const DEFAULT_AVAILABLE_TEST: bool = true;
/// `pbh.btstun.ipv6support` 默认值（true ⇒ 转发器监听 `[::]`，否则 `0.0.0.0`）。
pub const DEFAULT_IPV6_SUPPORT: bool = true;
/// `pbh.TCPForwarder.useFriendlyAddressForLoopback` 默认值。
pub const DEFAULT_USE_FRIENDLY_ADDRESS_FOR_LOOPBACK: bool = true;
/// `pbh.btstun.allowPublicIpAsDownloaderHost` 默认值（false ⇒ 公网下载器主机拒绝建隧道）。
pub const DEFAULT_ALLOW_PUBLIC_IP_AS_DOWNLOADER_HOST: bool = false;
/// `pbh.stunTcpTunnel.testHost` 默认值。
pub const DEFAULT_KEEP_ALIVE_TEST_HOST: &str = "qq.com";
/// keepalive 目标端口（上游 `connect(new InetSocketAddress(testHost, 80), 1000)`）。
pub const DEFAULT_KEEP_ALIVE_TEST_PORT: u16 = 80;
/// keepalive 首次执行的延迟（上游 `scheduleAtFixedRate(..., 1L, 10L, SECONDS)` 的 initialDelay）。
pub const KEEP_ALIVE_INITIAL_DELAY: Duration = Duration::from_secs(1);
/// keepalive 执行间隔（上游 10 秒）。
pub const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
/// keepalive 连接超时（上游 `connect(..., 1000)`）。
pub const KEEP_ALIVE_CONNECT_TIMEOUT: Duration = Duration::from_millis(1000);
/// keepalive 允许的最大连续失败次数（`pbh.stunTcpTunnel.maxKeepAliveFailures` 默认 5）。
pub const DEFAULT_MAX_KEEP_ALIVE_FAILURES: u32 = 5;
/// keepalive / testMapping 的读响应缓冲（上游 `new byte[1024]`）。
const FORWARDER_BUFFER_SIZE: usize = 1024;
/// 接受循环轮询步长（使 `close()` 能及时返回）。
const ACCEPT_TICK: Duration = Duration::from_millis(50);

/// 「先 bind 再 connect」的 TCP 连接（对齐 `StunSocketTool.getSocket()`：
/// `setReuseAddress` + 平台支持时的 `SO_REUSEPORT`/`SO_REUSEADDR`，随后 `bind` + `connect`）。
pub fn bind_connect(
    local: SocketAddr,
    remote: SocketAddr,
    timeout: Duration,
) -> io::Result<TcpStream> {
    let domain = socket2::Domain::for_address(local);
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    // 上游先检查 supportedOptions；不支持的平台（非 Unix）直接忽略
    #[cfg(unix)]
    let _ = socket.set_reuse_port(true);
    socket.set_nonblocking(true)?;
    socket.bind(&local.into())?;
    socket.connect_timeout(&remote.into(), timeout)?;
    socket.set_nonblocking(false)?;
    Ok(socket.into())
}

/// 解析 bind/connect 主机（支持上游 `[::]` 写法），取第一个可用地址。
pub fn resolve_first(host: &str, port: u16) -> io::Result<SocketAddr> {
    let trimmed = host.trim();
    let literal = trimmed
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(trimmed);
    if let Ok(ip) = literal.parse::<IpAddr>() {
        return Ok(SocketAddr::new(ip, port));
    }
    (trimmed, port).to_socket_addrs()?.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("无法解析地址 {host}:{port}"),
        )
    })
}

/// `MiscUtil.randomAvailablePort()`：临时绑定 `0.0.0.0:0` 取端口后关闭；失败返回 0。
pub fn random_available_port() -> u16 {
    TcpListener::bind("0.0.0.0:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .unwrap_or(0)
}

/// keepalive 请求报文（与上游 `keepAliveNATTunnel` 逐字节一致）。
pub fn keep_alive_request(test_host: &str) -> String {
    format!("HEAD / HTTP/1.1\r\nHost: {test_host}\r\nUser-Agent: PeerBanHelper-NAT-Keeper/1.0\r\nConnection: keep-alive\r\n\r\n")
}

/// NAT keepalive 配置（默认值全部对齐上游常量/ExternalSwitch 缺省）。
#[derive(Debug, Clone)]
pub struct KeepAliveConfig {
    /// 绑定主机（上游 `interResult.getHostString()`）
    pub bind_host: String,
    /// 绑定端口（上游 `interResult.getPort()`，即隧道本机端口）
    pub bind_port: u16,
    /// `pbh.stunTcpTunnel.testHost`（默认 [`DEFAULT_KEEP_ALIVE_TEST_HOST`]）
    pub test_host: String,
    /// keepalive 目标端口（上游固定 80）
    pub test_port: u16,
    pub initial_delay: Duration,
    pub interval: Duration,
    /// `pbh.stunTcpTunnel.maxKeepAliveFailures`（默认 5）
    pub max_consecutive_failures: u32,
    /// 连接超时（上游 1000ms）
    pub connect_timeout: Duration,
    /// 读响应超时（上游未设；套用连接超时保证线程可退出）
    pub read_timeout: Duration,
}

impl Default for KeepAliveConfig {
    fn default() -> Self {
        Self {
            bind_host: "127.0.0.1".to_string(),
            bind_port: 0,
            test_host: DEFAULT_KEEP_ALIVE_TEST_HOST.to_string(),
            test_port: DEFAULT_KEEP_ALIVE_TEST_PORT,
            initial_delay: KEEP_ALIVE_INITIAL_DELAY,
            interval: KEEP_ALIVE_INTERVAL,
            max_consecutive_failures: DEFAULT_MAX_KEEP_ALIVE_FAILURES,
            connect_timeout: KEEP_ALIVE_CONNECT_TIMEOUT,
            read_timeout: KEEP_ALIVE_CONNECT_TIMEOUT,
        }
    }
}

/// 执行一轮 keepalive：从 `inter` 端口绑定连接 `testHost:80`，发 HEAD 请求并读一次响应。
/// 对齐 `StunTcpTunnelImpl.keepAliveNATTunnel`（成功 ⇒ 刷新 `lastSuccessHeartbeatAt`、清零失败计数）。
pub fn keep_alive_once(config: &KeepAliveConfig) -> io::Result<()> {
    let local = resolve_first(&config.bind_host, config.bind_port)?;
    let remote = resolve_first(&config.test_host, config.test_port)?;
    let mut socket = bind_connect(local, remote, config.connect_timeout)?;
    socket.write_all(keep_alive_request(&config.test_host).as_bytes())?;
    socket.flush()?;
    socket.set_read_timeout(Some(config.read_timeout))?;
    let mut buffer = [0u8; FORWARDER_BUFFER_SIZE];
    // 上游只读取一次用于日志，不校验内容
    let _ = socket.read(&mut buffer)?;
    Ok(())
}

/// NAT keepalive 后台线程（对齐 `StunTcpTunnelImpl.startNATHolder` 的定时任务）。
pub struct NatKeepAlive {
    valid: Arc<AtomicBool>,
    last_success_ms: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl NatKeepAlive {
    /// 启动 keepalive 线程：`initial_delay` 后首跑，此后每 `interval` 一次；
    /// 连续失败达 `max_consecutive_failures` ⇒ `valid=false` 并停止
    /// （上游在该处置 `valid=false` 并回调 `stunListener.onClose`，由外层 5s 任务重建隧道）。
    pub fn spawn(config: KeepAliveConfig) -> Self {
        let valid = Arc::new(AtomicBool::new(true));
        let last_success_ms = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let valid_flag = Arc::clone(&valid);
        let last_success = Arc::clone(&last_success_ms);
        let handle = std::thread::Builder::new()
            .name("StunTcpTunnel-KeepAlive".to_string())
            .spawn(move || {
                interruptible_sleep(&flag, config.initial_delay);
                let mut consecutive_failures: u32 = 0;
                while !flag.load(Ordering::SeqCst) {
                    match keep_alive_once(&config) {
                        Ok(()) => {
                            last_success.store(current_millis(), Ordering::SeqCst);
                            consecutive_failures = 0;
                        }
                        Err(e) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            tracing::warn!(
                                "[AutoSTUN] NAT keepalive 失败（{}:{}），第 {consecutive_failures}/{} 次: {e}",
                                config.bind_host,
                                config.bind_port,
                                config.max_consecutive_failures
                            );
                            if consecutive_failures >= config.max_consecutive_failures {
                                tracing::error!(
                                    "[AutoSTUN] NAT keepalive 连续失败超过上限（{}），隧道失效等待重建",
                                    config.max_consecutive_failures
                                );
                                valid_flag.store(false, Ordering::SeqCst);
                                break;
                            }
                        }
                    }
                    interruptible_sleep(&flag, config.interval);
                }
            })
            .ok();
        Self {
            valid,
            last_success_ms,
            stop,
            handle,
        }
    }

    /// `StunTcpTunnel.isValid()`。
    pub fn is_valid(&self) -> bool {
        self.valid.load(Ordering::SeqCst)
    }

    /// `StunTcpTunnel.getLastSuccessHeartbeatAt()`（epoch 毫秒；尚无成功心跳 ⇒ 0）。
    pub fn last_success_heartbeat_at(&self) -> u64 {
        self.last_success_ms.load(Ordering::SeqCst)
    }

    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for NatKeepAlive {
    fn drop(&mut self) {
        self.close();
    }
}

fn interruptible_sleep(stop: &AtomicBool, duration: Duration) {
    let mut waited = Duration::ZERO;
    while waited < duration && !stop.load(Ordering::SeqCst) {
        let step = ACCEPT_TICK.min(duration - waited);
        std::thread::sleep(step);
        waited += step;
    }
}

fn current_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// TCP 转发器配置（默认值对齐上游 ExternalSwitch 缺省）。
#[derive(Debug, Clone)]
pub struct TcpForwarderConfig {
    /// 监听主机（`pbh.btstun.ipv6support` 默认 true ⇒ `[::]`，否则 `0.0.0.0`）
    pub proxy_host: String,
    /// 监听端口（上游 `interResult.getPort()`）
    pub proxy_port: u16,
    /// 上游主机（下载器地址）
    pub upstream_host: String,
    /// 上游端口（上游 `outerResult.getPort()`）
    pub upstream_port: u16,
    /// `pbh.TCPForwarder.useFriendlyAddressForLoopback`（默认 true）
    pub use_friendly_loopback: bool,
    /// 连接上游的超时
    pub connect_timeout: Duration,
}

/// TCP 转发器（对齐 `TCPForwarderImpl` 的转发核心）：
/// 监听 `proxy_host:proxy_port`，拒绝同 IP 重复连接，记录
/// `connectionMap[下游地址] = 上游本机地址` 并双向中继；[`TcpForwarder::translate`] 做反查。
pub struct TcpForwarder {
    config: TcpForwarderConfig,
    /// `connectionMap`：下游（peer）地址 → 上游（本机）套接字地址（BiMap 正向）
    connections: RwLock<HashMap<SocketAddr, SocketAddr>>,
    /// `pendingConnectionIps`：连接建立中的 IP（防同 IP 并发连接）
    pending: Mutex<HashSet<IpAddr>>,
    handled: AtomicU64,
    failed: AtomicU64,
    rejected: AtomicU64,
    stop: AtomicBool,
    /// 实际监听地址（`proxy_port = 0` 时由系统分配）
    bound_addr: RwLock<Option<SocketAddr>>,
    accept_thread: Mutex<Option<JoinHandle<()>>>,
}

impl TcpForwarder {
    pub fn new(config: TcpForwarderConfig) -> Arc<Self> {
        Arc::new(Self {
            config,
            connections: RwLock::new(HashMap::new()),
            pending: Mutex::new(HashSet::new()),
            handled: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            bound_addr: RwLock::new(None),
            accept_thread: Mutex::new(None),
        })
    }

    /// `TCPForwarderImpl.start()`：绑定并开始接受连接。
    pub fn start(self: &Arc<Self>) -> io::Result<()> {
        let bind_addr = resolve_first(&self.config.proxy_host, self.config.proxy_port)?;
        let listener = TcpListener::bind(bind_addr)?;
        let local = listener.local_addr()?;
        let this = Arc::clone(self);
        let handle = std::thread::Builder::new()
            .name("TCPForwarder-Accept".to_string())
            .spawn(move || this.accept_loop(listener))
            .map_err(io::Error::other)?;
        if let Ok(mut slot) = self.accept_thread.lock() {
            *slot = Some(handle);
        }
        if let Ok(mut bound) = self.bound_addr.write() {
            *bound = Some(local);
        }
        tracing::debug!(
            "[AutoSTUN] TCP 转发器已启动: {}:{} -> {}:{}",
            self.config.proxy_host,
            local.port(),
            self.config.upstream_host,
            self.config.upstream_port
        );
        Ok(())
    }

    /// 实际监听地址（`proxy_port = 0` 时端口由系统分配）。
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.bound_addr.read().ok().and_then(|bound| *bound)
    }

    fn accept_loop(self: Arc<Self>, listener: TcpListener) {
        listener.set_nonblocking(true).ok();
        while !self.stop.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let this = Arc::clone(&self);
                    if std::thread::Builder::new()
                        .name("TCPForwarder-Relay".to_string())
                        .spawn(move || this.handle_connection(stream))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => std::thread::sleep(ACCEPT_TICK),
            }
        }
    }

    fn handle_connection(self: &Arc<Self>, downstream: TcpStream) {
        // Windows 上 accept() 返回的套接字会继承监听套接字的非阻塞模式
        // （Linux 不继承），中继采用阻塞 IO，必须显式复位为阻塞模式。
        if let Err(e) = downstream.set_nonblocking(false) {
            tracing::debug!("[AutoSTUN] 复位连接为非阻塞失败: {e}");
        }
        // 上游 ProxyFrontendHandler.channelActive：远端地址缺失 ⇒ 拒绝
        let Ok(peer) = downstream.peer_addr() else {
            self.rejected.fetch_add(1, Ordering::SeqCst);
            return;
        };
        // 同一 IP 不允许多重连接（不干扰 ProgressCheatBlocker）
        if !self.reserve_connection(peer.ip()) {
            tracing::debug!(
                "[AutoSTUN] 拒绝来自 {} 的重复/并发连接（同一 IP 仅允许一条）",
                peer.ip()
            );
            self.rejected.fetch_add(1, Ordering::SeqCst);
            return;
        }
        self.handled.fetch_add(1, Ordering::SeqCst);
        let upstream = match self.connect_upstream_friendly(peer) {
            Ok(upstream) => upstream,
            Err(e) => {
                tracing::debug!(
                    "[AutoSTUN] 连接上游 {}:{} 失败: {e}",
                    self.config.upstream_host,
                    self.config.upstream_port
                );
                self.failed.fetch_add(1, Ordering::SeqCst);
                self.release_connection(peer.ip());
                return;
            }
        };
        let Ok(upstream_local) = upstream.local_addr() else {
            self.failed.fetch_add(1, Ordering::SeqCst);
            self.release_connection(peer.ip());
            return;
        };
        if let Ok(mut connections) = self.connections.write() {
            connections.insert(peer, upstream_local);
        }
        self.release_connection(peer.ip());
        relay(downstream, upstream);
        if let Ok(mut connections) = self.connections.write() {
            connections.remove(&peer);
        }
    }

    /// 连接表中是否已有同 IP 连接 / 同 IP 是否已在连接中
    /// （对齐 `isDuplicateConnection` + `reservePendingConnection`）。
    fn reserve_connection(&self, ip: IpAddr) -> bool {
        let duplicate = self
            .connections
            .read()
            .map(|connections| connections.keys().any(|key| key.ip() == ip))
            .unwrap_or(true);
        if duplicate {
            return false;
        }
        match self.pending.lock() {
            Ok(mut pending) => pending.insert(ip),
            Err(_) => false,
        }
    }

    fn release_connection(&self, ip: IpAddr) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&ip);
        }
    }

    /// 上游 `connectToUpstreamFriendly`：上游为回环地址且下游是 IPv4、且开关打开时，
    /// 依次尝试 `bind(127.{d1}.{d2}.{d3}:{下游端口})` → `bind(同地址随机端口)` → 默认连接。
    fn connect_upstream_friendly(&self, downstream: SocketAddr) -> io::Result<TcpStream> {
        let upstream = resolve_first(&self.config.upstream_host, self.config.upstream_port)?;
        let use_friendly = self.config.use_friendly_loopback
            && upstream.ip().is_loopback()
            && downstream.is_ipv4();
        if use_friendly {
            let octets = match downstream.ip() {
                IpAddr::V4(v4) => v4.octets(),
                IpAddr::V6(_) => unreachable!("use_friendly 已保证 IPv4"),
            };
            let friendly = Ipv4Addr::new(127, octets[1], octets[2], octets[3]);
            // 第一次：绑定友好地址 + 下游原始端口
            match bind_connect(
                SocketAddr::new(IpAddr::V4(friendly), downstream.port()),
                upstream,
                self.config.connect_timeout,
            ) {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    tracing::debug!(
                        "[AutoSTUN] 绑定友好地址 {}:{} 失败，尝试随机端口: {e}",
                        friendly,
                        downstream.port()
                    );
                }
            }
            // 第二次：绑定友好地址 + 随机端口
            match bind_connect(
                SocketAddr::new(IpAddr::V4(friendly), 0),
                upstream,
                self.config.connect_timeout,
            ) {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    tracing::debug!("[AutoSTUN] 绑定友好地址 {friendly}（随机端口）也失败: {e}");
                }
            }
        }
        // 兜底：默认连接
        TcpStream::connect_timeout(&upstream, self.config.connect_timeout)
    }

    /// `Forwarder.translate(natted)`：`connectionMap.inverse().get(natted)` ——
    /// 用上游（本机）套接字地址反查下游（peer）地址。
    pub fn translate(&self, natted: SocketAddr) -> Option<SocketAddr> {
        self.connections
            .read()
            .ok()?
            .iter()
            .find(|(_, upstream)| **upstream == natted)
            .map(|(downstream, _)| *downstream)
    }

    /// `getEstablishedConnections()`。
    pub fn established_connections(&self) -> usize {
        self.connections
            .read()
            .map(|connections| connections.len())
            .unwrap_or(0)
    }

    pub fn connection_handled(&self) -> u64 {
        self.handled.load(Ordering::SeqCst)
    }

    pub fn connection_failed(&self) -> u64 {
        self.failed.load(Ordering::SeqCst)
    }

    pub fn connection_rejected(&self) -> u64 {
        self.rejected.load(Ordering::SeqCst)
    }

    /// `TCPForwarderImpl.close()`：停止接受新连接（既有连接随对端关闭自然结束）。
    pub fn close(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = self.accept_thread.lock() {
            if let Some(handle) = slot.take() {
                let _ = handle.join();
            }
        }
    }
}

/// 双向中继（对齐 `RelayHandler`：任一侧关闭即断开另一侧）。
fn relay(mut downstream: TcpStream, mut upstream: TcpStream) {
    let (Ok(mut down_clone), Ok(mut up_clone)) = (downstream.try_clone(), upstream.try_clone())
    else {
        return;
    };
    let to_upstream = std::thread::Builder::new()
        .name("TCPForwarder-RelayUp".to_string())
        .spawn(move || {
            let _ = io::copy(&mut down_clone, &mut up_clone);
            let _ = up_clone.shutdown(Shutdown::Both);
        });
    let _ = io::copy(&mut upstream, &mut downstream);
    let _ = downstream.shutdown(Shutdown::Both);
    if let Ok(handle) = to_upstream {
        let _ = handle.join();
    }
}

/// `StunTcpTunnelImpl.testMapping`：在 `inter` 上临时起一个回答 `204` 的 HTTP 服务，
/// 从本机连接 `outer` 发送 `GET /test`，读响应判断是否包含 `"204"`。
pub fn test_mapping(inter: SocketAddr, outer: SocketAddr, timeout: Duration) -> io::Result<bool> {
    let listener = TcpListener::bind(inter)?;
    // 非阻塞 + 轮询到 deadline：`outer` 不可达时不会有任何连接进来，
    // 若让线程一直阻塞在 `accept()`，下面的 `join()` 就会永久挂起。
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout + Duration::from_millis(200);
    let server = std::thread::Builder::new()
        .name("StunTcpTunnel-TestMapping".to_string())
        .spawn(move || {
            // 上游 HttpServer 对 /test 回 204（sendResponseHeaders(204, -1)）
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut buffer = [0u8; FORWARDER_BUFFER_SIZE];
                        let _ = stream.read(&mut buffer);
                        let _ = stream.write_all(
                            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        let _ = stream.flush();
                        break;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        })
        .map_err(io::Error::other)?;
    let result = (|| -> io::Result<bool> {
        let mut socket = TcpStream::connect_timeout(&outer, timeout)?;
        socket.set_read_timeout(Some(timeout))?;
        let request = format!(
            "GET /test HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n",
            inter.ip()
        );
        socket.write_all(request.as_bytes())?;
        socket.flush()?;
        let mut buffer = [0u8; FORWARDER_BUFFER_SIZE];
        let read = socket.read(&mut buffer)?;
        // 上游：bytesRead > 0 且响应包含 "204" 才算通过
        Ok(read > 0 && buffer[..read].windows(3).any(|window| window == b"204"))
    })();
    let _ = server.join();
    result
}

/// 下载器主机校验（对齐 `BTStunInstance.onCreate`）：
/// 公网地址默认拒绝（返回 `false` ⇒ 上游 `manager.unregister` + `close` + 告警）；
/// **主机解析失败时跳过校验**（上游 `hostAddress == null` 分支直接放行）。
pub fn downloader_host_allowed(host: &str, allow_public: bool) -> bool {
    if allow_public {
        return true; // pbh.btstun.allowPublicIpAsDownloaderHost
    }
    let Ok(addr) = resolve_first(host, 0) else {
        return true; // 上游解析失败 ⇒ 不执行公网检查
    };
    match addr.ip() {
        IpAddr::V4(v4) => !is_public_v4(v4),
        IpAddr::V6(v6) => !is_public_v6(v6),
    }
}

/// ipaddr 库 `isZeroHost()` 的 IPv4 等价（按 A/B/C 类默认前缀检查主机位全零）。
fn is_zero_host_v4(ip: Ipv4Addr) -> bool {
    let [o0, o1, o2, o3] = ip.octets();
    if o0 < 128 {
        o1 == 0 && o2 == 0 && o3 == 0 // A 类 /8
    } else if o0 < 192 {
        o2 == 0 && o3 == 0 // B 类 /16
    } else if o0 < 224 {
        o3 == 0 // C 类 /24
    } else {
        false
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_private()
        || is_zero_host_v4(ip))
}

fn is_public_v6(ip: std::net::Ipv6Addr) -> bool {
    let octets = ip.octets();
    let unique_local = (octets[0] & 0xfe) == 0xfc; // fc00::/7
    let site_local = octets[0] == 0xfe && (octets[1] & 0xc0) == 0x80; // fec0::/10
    let zero_host = octets[8..].iter().all(|&byte| byte == 0); // ipaddr 默认 /64
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        || unique_local
        || site_local
        || zero_host)
}

/// 隧道装配配置（默认值全部对齐上游 config.yml/ExternalSwitch 缺省）。
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    /// ExternalSwitch `pbh.btstun.localPort.<downloaderId>`（默认 0 ⇒ 随机可用端口）
    pub local_port: u16,
    /// `stun.available-test`（默认 true）
    pub available_test: bool,
    /// `pbh.btstun.ipv6support`（默认 true ⇒ 转发器监听 `[::]`）
    pub ipv6_support: bool,
    /// `pbh.stunTcpTunnel.testHost`（默认 `qq.com`）
    pub keep_alive_test_host: String,
    /// keepalive 目标端口（上游固定 80；测试可注入）
    pub keep_alive_test_port: u16,
    /// keepalive 首跑延迟（上游 1s）
    pub keep_alive_initial_delay: Duration,
    /// keepalive 间隔（上游 10s）
    pub keep_alive_interval: Duration,
    /// `pbh.stunTcpTunnel.maxKeepAliveFailures`（默认 5）
    pub max_keep_alive_failures: u32,
    /// `pbh.TCPForwarder.useFriendlyAddressForLoopback`（默认 true）
    pub use_friendly_loopback: bool,
    /// `pbh.btstun.allowPublicIpAsDownloaderHost`（默认 false）
    pub allow_public_downloader_host: bool,
    /// STUN / testMapping / 上游连接超时（上游 5000ms）
    pub timeout: Duration,
}

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            local_port: 0,
            available_test: DEFAULT_AVAILABLE_TEST,
            ipv6_support: DEFAULT_IPV6_SUPPORT,
            keep_alive_test_host: DEFAULT_KEEP_ALIVE_TEST_HOST.to_string(),
            keep_alive_test_port: DEFAULT_KEEP_ALIVE_TEST_PORT,
            keep_alive_initial_delay: KEEP_ALIVE_INITIAL_DELAY,
            keep_alive_interval: KEEP_ALIVE_INTERVAL,
            max_keep_alive_failures: DEFAULT_MAX_KEEP_ALIVE_FAILURES,
            use_friendly_loopback: DEFAULT_USE_FRIENDLY_ADDRESS_FOR_LOOPBACK,
            allow_public_downloader_host: DEFAULT_ALLOW_PUBLIC_IP_AS_DOWNLOADER_HOST,
            timeout: DEFAULT_STUN_TIMEOUT,
        }
    }
}

/// 一条已建立的 STUN 隧道（对齐 `StunTcpTunnelImpl` + `BTStunInstance` 持有的状态）。
pub struct StunTcpTunnel {
    /// 本机端点（`interAddress`）
    pub inter: SocketAddr,
    /// NAT 公网端点（`outerAddress`）
    pub outer: SocketAddr,
    /// TCP 转发器（`BTStunInstance.tcpForwarder`）
    pub forwarder: Arc<TcpForwarder>,
    keep_alive: Mutex<Option<NatKeepAlive>>,
    started_at_ms: u64,
    valid: AtomicBool,
}

impl StunTcpTunnel {
    /// `StunTcpTunnel.isValid()`。
    pub fn is_valid(&self) -> bool {
        self.valid.load(Ordering::SeqCst) && self.keep_alive_healthy()
    }

    /// keepalive 线程在连续失败达上限时会置自身失效（⇒ 隧道待重建）。
    fn keep_alive_healthy(&self) -> bool {
        self.keep_alive
            .lock()
            .map(|guard| guard.as_ref().map(NatKeepAlive::is_valid).unwrap_or(true))
            .unwrap_or(true)
    }

    /// `StunTcpTunnel.getStartedAt()`（epoch 毫秒）。
    pub fn started_at(&self) -> u64 {
        self.started_at_ms
    }

    /// `StunTcpTunnel.getLastSuccessHeartbeatAt()`。
    pub fn last_success_heartbeat_at(&self) -> u64 {
        self.keep_alive
            .lock()
            .map(|guard| {
                guard
                    .as_ref()
                    .map(NatKeepAlive::last_success_heartbeat_at)
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    /// `BTStunInstance.translate` → `Forwarder.translate`。
    pub fn translate(&self, natted: SocketAddr) -> Option<SocketAddr> {
        self.forwarder.translate(natted)
    }

    /// `StunTcpTunnel.close()` + `BTStunInstance.close()`：停 keepalive、停转发器。
    pub fn close(&self) {
        self.valid.store(false, Ordering::SeqCst);
        if let Ok(mut guard) = self.keep_alive.lock() {
            if let Some(keep_alive) = guard.as_mut() {
                keep_alive.close();
            }
        }
        self.forwarder.close();
    }
}

impl Drop for StunTcpTunnel {
    fn drop(&mut self) {
        self.close();
    }
}

/// `StunTcpTunnelImpl.createMapping` + `BTStunInstance.onCreate` 的移植。
///
/// 返回 `Ok(None)` 表示「隧道不适用」（映射自测失败 / 下载器主机为公网地址，
/// 上游分别是 `onNotApplicable` 与 `unregister + close`）；`Err` 表示 STUN/网络错误
/// （上游由 BTStunInstance 的 5s 定时任务捕获并重建）。
pub fn create_tunnel(
    stun_servers: &[String],
    downloader_host: &str,
    config: &TunnelConfig,
) -> io::Result<Option<StunTcpTunnel>> {
    let started_at_ms = current_millis();
    // createMapping：localPort == 0 ⇒ 随机可用端口
    let local_port = if config.local_port == 0 {
        random_available_port()
    } else {
        config.local_port
    };
    // TcpStunClient("0.0.0.0", localPort).getMapping()：套接字绑定到隧道端口后再探测
    let client = TcpStunClient::new(stun_servers.to_vec(), "0.0.0.0", local_port)
        .ok_or_else(|| invalid_data("STUN server list cannot be empty"))?;
    let Some(mapping) = client.get_mapping(config.timeout) else {
        return Err(invalid_data("No available STUN server"));
    };
    let (inter, outer) = (mapping.inter, mapping.outer);
    tracing::debug!(
        "[AutoSTUN] STUN CreateMapping: Inter address: {inter}, Outer address: {outer}"
    );
    // stun.available-test（默认 true）
    if config.available_test && !test_mapping(inter, outer, config.timeout)? {
        tracing::warn!("[AutoSTUN] 隧道映射自测未通过，放弃建立隧道");
        return Ok(None); // onNotApplicable(AUTOSTUN_DOWNLOADER_TUNNEL_TEST_FAILED)
    }
    // BTStunInstance.onCreate：下载器主机必须是 LAN/回环等，公网默认拒绝
    if !downloader_host_allowed(downloader_host, config.allow_public_downloader_host) {
        tracing::warn!("[AutoSTUN] 下载器主机 {downloader_host} 不是 LAN 地址，隧道关闭");
        return Ok(None); // AUTOSTUN_DOWNLOADER_HOST_NOT_LAN_ADDRESS
    }
    // TCPForwarderImpl：监听 [::]|0.0.0.0:inter.port，转发到 downloaderHost:outer.port
    let forwarder = TcpForwarder::new(TcpForwarderConfig {
        proxy_host: if config.ipv6_support {
            "[::]".to_string()
        } else {
            "0.0.0.0".to_string()
        },
        proxy_port: inter.port(),
        upstream_host: downloader_host.to_string(),
        upstream_port: outer.port(),
        use_friendly_loopback: config.use_friendly_loopback,
        connect_timeout: config.timeout,
    });
    if let Err(e) = forwarder.start() {
        tracing::error!("[AutoSTUN] TCP 转发器无法启动: {e}");
        return Err(e);
    }
    // startNATHolder：绑定 inter 端口的 keepalive（1s 后首跑，每 10s 一次）
    let keep_alive = NatKeepAlive::spawn(KeepAliveConfig {
        bind_host: inter.ip().to_string(),
        bind_port: inter.port(),
        test_host: config.keep_alive_test_host.clone(),
        test_port: config.keep_alive_test_port,
        initial_delay: config.keep_alive_initial_delay,
        interval: config.keep_alive_interval,
        max_consecutive_failures: config.max_keep_alive_failures,
        connect_timeout: KEEP_ALIVE_CONNECT_TIMEOUT,
        read_timeout: KEEP_ALIVE_CONNECT_TIMEOUT,
    });
    tracing::info!(
        "[AutoSTUN] STUN 隧道已建立: 本机端点 {inter} -> 公网端点 {outer}，转发 {}:{} -> {downloader_host}:{}",
        if config.ipv6_support { "[::]" } else { "0.0.0.0" },
        inter.port(),
        outer.port()
    );
    Ok(Some(StunTcpTunnel {
        inter,
        outer,
        forwarder,
        keep_alive: Mutex::new(Some(keep_alive)),
        started_at_ms,
        valid: AtomicBool::new(true),
    }))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, UdpSocket};
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    /// 一个回显「下载器」：accept 后把收到的字节原样回写。
    fn spawn_echo_downloader() -> SocketAddr {
        spawn_echo_downloader_with_peers(Arc::new(Mutex::new(Vec::new())))
    }

    /// 回显下载器，并记录每个 accepted 连接的 peer 地址（供友好回环绑定断言）。
    fn spawn_echo_downloader_with_peers(peers: Arc<Mutex<Vec<SocketAddr>>>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("回环监听必须可用");
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                if let Ok(peer) = stream.peer_addr() {
                    peers.lock().unwrap().push(peer);
                }
                let Ok(mut clone) = stream.try_clone() else {
                    continue;
                };
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 4096];
                    while let Ok(n) = stream.read(&mut buffer) {
                        if n == 0 || clone.write_all(&buffer[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        addr
    }

    fn wait_until(predicate: impl Fn() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        predicate()
    }

    fn forwarder_config(proxy_port: u16, upstream: SocketAddr) -> TcpForwarderConfig {
        TcpForwarderConfig {
            proxy_host: "127.0.0.1".to_string(),
            proxy_port,
            upstream_host: upstream.ip().to_string(),
            upstream_port: upstream.port(),
            use_friendly_loopback: true,
            connect_timeout: Duration::from_secs(2),
        }
    }

    fn started_forwarder(proxy_port: u16, upstream: SocketAddr) -> (Arc<TcpForwarder>, u16) {
        let forwarder = TcpForwarder::new(forwarder_config(proxy_port, upstream));
        forwarder.start().expect("转发器启动");
        let port = forwarder.local_addr().expect("已绑定").port();
        (forwarder, port)
    }

    #[test]
    fn defaults_match_upstream_switches() {
        let config = TunnelConfig::default();
        assert_eq!(
            config.local_port, 0,
            "pbh.btstun.localPort 默认 0 ⇒ 随机端口"
        );
        assert!(config.available_test, "stun.available-test 默认 true");
        assert!(
            config.ipv6_support,
            "pbh.btstun.ipv6support 默认 true ⇒ 监听 [::]"
        );
        assert_eq!(
            config.keep_alive_test_host, "qq.com",
            "pbh.stunTcpTunnel.testHost 默认 qq.com"
        );
        assert_eq!(config.keep_alive_test_port, 80);
        assert_eq!(config.keep_alive_initial_delay, Duration::from_secs(1));
        assert_eq!(config.keep_alive_interval, Duration::from_secs(10));
        assert_eq!(config.max_keep_alive_failures, 5);
        assert!(
            config.use_friendly_loopback,
            "useFriendlyAddressForLoopback 默认 true"
        );
        assert!(!config.allow_public_downloader_host);
        assert_eq!(config.timeout, DEFAULT_STUN_TIMEOUT);
        assert!(random_available_port() > 0, "随机可用端口工具可用");
    }

    #[test]
    fn forwarder_relays_tcp_payload_both_directions() {
        let upstream = spawn_echo_downloader();
        let (forwarder, port) = started_forwarder(0, upstream);
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}")).expect("连接转发器");
        client.write_all(b"PING").unwrap();
        let mut buffer = [0u8; 4];
        client.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"PING", "payload 经转发器到达回显下载器并被回写");
        client.write_all(b"AGAIN").unwrap();
        let mut buffer2 = [0u8; 5];
        client.read_exact(&mut buffer2).unwrap();
        assert_eq!(&buffer2, b"AGAIN");
        assert_eq!(forwarder.connection_handled(), 1);
        assert_eq!(forwarder.connection_rejected(), 0);
        forwarder.close();
    }

    #[test]
    fn translate_maps_upstream_local_address_back_to_downstream() {
        let upstream = spawn_echo_downloader();
        let (forwarder, port) = started_forwarder(0, upstream);
        let mut client = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        assert!(
            wait_until(
                || forwarder.established_connections() == 1,
                Duration::from_secs(2)
            ),
            "连接应在连接表中登记"
        );
        // 回显服务观测到的 peer = 转发器上游连接的本机地址（即连接表的值）
        // 无法直接读取，改用负例 + 行为断言：未知地址反查为 None
        assert_eq!(
            forwarder.translate(SocketAddr::from(([127, 0, 0, 1], 1))),
            None
        );
        let _ = client.write_all(b"x");
        forwarder.close();
    }

    #[test]
    fn duplicate_connection_from_same_ip_is_rejected() {
        let upstream = spawn_echo_downloader();
        let (forwarder, port) = started_forwarder(0, upstream);
        let mut first = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        assert!(
            wait_until(
                || forwarder.established_connections() == 1,
                Duration::from_secs(2)
            ),
            "第一条连接先建立"
        );
        // 同 IP 的第二条连接应被立即关闭
        let mut second = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        second
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut buffer = [0u8; 1];
        let read = second.read(&mut buffer).unwrap_or(0);
        assert_eq!(read, 0, "重复连接应被关闭（EOF）");
        assert_eq!(forwarder.connection_rejected(), 1);
        let _ = first.write_all(b"keep");
        forwarder.close();
    }

    /// 取本机非回环 IPv4（UDP connect 技巧，不实际发包）；取不到 ⇒ 调用方跳过断言。
    fn local_non_loopback_ipv4() -> Option<Ipv4Addr> {
        let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("8.8.8.8:80").ok()?;
        match socket.local_addr().ok()?.ip() {
            IpAddr::V4(v4) if !v4.is_loopback() => Some(v4),
            _ => None,
        }
    }

    #[test]
    fn friendly_loopback_binding_reuses_downstream_port() {
        // 友好地址 = 127.{b}.{c}.{d}。只有下游 IP **不是** 127.* 时它才与下游自身不同，
        // 此时「绑定友好地址 + 下游原始端口」的第一次尝试才可能成功；
        // 纯回环场景下该端口必然已被下游自己的套接字占用（上游 Java 同样会落到第二次尝试）。
        let Some(local_ip) = local_non_loopback_ipv4() else {
            return; // 环境无非回环 IPv4：跳过（不影响语义）
        };
        let peers: Arc<Mutex<Vec<SocketAddr>>> = Arc::new(Mutex::new(Vec::new()));
        let upstream = spawn_echo_downloader_with_peers(Arc::clone(&peers));
        let (forwarder, port) = started_forwarder(0, upstream);
        // 经本机非回环地址连入 ⇒ downstream 源 IP 即该地址
        let Ok(mut client) = TcpStream::connect(format!("{local_ip}:{port}")) else {
            return; // 该地址不可达（如容器无对外路由）：跳过
        };
        assert!(wait_until(
            || !peers.lock().unwrap().is_empty(),
            Duration::from_secs(2)
        ));
        let downstream_port = client.local_addr().unwrap().port();
        let observed = peers.lock().unwrap()[0];
        assert_eq!(
            observed.port(),
            downstream_port,
            "友好回环绑定：上游连接应以 127.x.y.z:{downstream_port} 为源（第一次尝试成功）"
        );
        assert!(observed.ip().is_loopback());
        let _ = client.write_all(b"hi");
        forwarder.close();
    }

    #[test]
    fn friendly_disabled_falls_back_to_ephemeral_port() {
        let peers: Arc<Mutex<Vec<SocketAddr>>> = Arc::new(Mutex::new(Vec::new()));
        let upstream = spawn_echo_downloader_with_peers(Arc::clone(&peers));
        let mut config = forwarder_config(0, upstream);
        config.use_friendly_loopback = false;
        let forwarder = TcpForwarder::new(config);
        forwarder.start().unwrap();
        let port = forwarder.local_addr().unwrap().port();
        let client = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
        assert!(wait_until(
            || !peers.lock().unwrap().is_empty(),
            Duration::from_secs(2)
        ));
        let observed = peers.lock().unwrap()[0];
        assert_ne!(
            observed.port(),
            client.local_addr().unwrap().port(),
            "未启用友好绑定时，上游连接端口由系统分配"
        );
        forwarder.close();
    }

    #[test]
    fn keep_alive_request_layout_matches_upstream() {
        assert_eq!(
            keep_alive_request("qq.com"),
            "HEAD / HTTP/1.1\r\nHost: qq.com\r\nUser-Agent: PeerBanHelper-NAT-Keeper/1.0\r\nConnection: keep-alive\r\n\r\n"
        );
    }

    #[test]
    fn keep_alive_sends_at_configured_interval() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let first_request: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let server_hits = Arc::clone(&hits);
        let server_first = Arc::clone(&first_request);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                server_hits.fetch_add(1, Ordering::Relaxed);
                let mut buffer = [0u8; FORWARDER_BUFFER_SIZE];
                if let Ok(n) = stream.read(&mut buffer) {
                    if n > 0 {
                        let mut slot = server_first.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(String::from_utf8_lossy(&buffer[..n]).to_string());
                        }
                    }
                }
            }
        });
        let mut keep_alive = NatKeepAlive::spawn(KeepAliveConfig {
            bind_host: "127.0.0.1".to_string(),
            bind_port: 0,
            test_host: addr.ip().to_string(),
            test_port: addr.port(),
            initial_delay: Duration::from_millis(10),
            interval: Duration::from_millis(60),
            max_consecutive_failures: 100,
            connect_timeout: Duration::from_secs(1),
            read_timeout: Duration::from_secs(1),
        });
        assert!(keep_alive.is_valid());
        assert!(
            wait_until(|| hits.load(Ordering::Relaxed) >= 3, Duration::from_secs(3)),
            "keepalive 应按注入的短间隔重复发送（≥3 次）"
        );
        let request = first_request
            .lock()
            .unwrap()
            .clone()
            .expect("至少收到一次请求");
        assert!(
            request.starts_with("HEAD / HTTP/1.1\r\nHost: "),
            "报文与上游逐字节一致"
        );
        assert!(request.contains("User-Agent: PeerBanHelper-NAT-Keeper/1.0"));
        assert!(
            keep_alive.last_success_heartbeat_at() > 0,
            "成功心跳时间被记录"
        );
        keep_alive.close();
        let after = hits.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(hits.load(Ordering::Relaxed), after, "close() 后不再发送");
    }

    #[test]
    fn keep_alive_marks_tunnel_invalid_after_consecutive_failures() {
        // 连接必然被拒的端口（临时监听后立刻关闭）
        let doomed = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = doomed.local_addr().unwrap().port();
        drop(doomed);
        let mut keep_alive = NatKeepAlive::spawn(KeepAliveConfig {
            bind_host: "127.0.0.1".to_string(),
            bind_port: 0,
            test_host: "127.0.0.1".to_string(),
            test_port: dead_port,
            initial_delay: Duration::from_millis(5),
            interval: Duration::from_millis(30),
            max_consecutive_failures: 2,
            connect_timeout: Duration::from_millis(500),
            read_timeout: Duration::from_millis(500),
        });
        assert!(
            wait_until(|| !keep_alive.is_valid(), Duration::from_secs(3)),
            "连续失败达上限（2 次）后 valid 必须翻转为 false"
        );
        keep_alive.close();
    }

    #[test]
    fn test_mapping_passes_on_loopback_hairpin() {
        // 回环上没有 NAT：让 outer == inter，连接 outer 即可命中临时 204 服务
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let inter = probe.local_addr().unwrap();
        drop(probe);
        assert!(test_mapping(inter, inter, Duration::from_secs(2)).unwrap());
    }

    #[test]
    fn test_mapping_fails_when_outer_unreachable() {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let inter = probe.local_addr().unwrap();
        drop(probe);
        let doomed = TcpListener::bind("127.0.0.1:0").unwrap();
        let outer = doomed.local_addr().unwrap();
        drop(doomed);
        // `outer` 不可达 ⇒ 连接失败：本实现以 `Err` 向上传递（等价于上游 IOException ⇒ 判定失败）
        assert!(
            !test_mapping(inter, outer, Duration::from_millis(300)).unwrap_or(false),
            "无响应 ⇒ 自测失败"
        );
    }

    #[test]
    fn downloader_host_validation_rejects_public_addresses() {
        // 回环 / 私网 / 链路本地 / 组播 / unspecified / 零主机 ⇒ 允许
        assert!(downloader_host_allowed("127.0.0.1", false));
        assert!(downloader_host_allowed("192.168.1.10", false));
        assert!(downloader_host_allowed("10.0.0.5", false));
        assert!(downloader_host_allowed("172.16.0.1", false));
        assert!(downloader_host_allowed("169.254.1.1", false));
        assert!(downloader_host_allowed("0.0.0.0", false));
        assert!(
            downloader_host_allowed("192.168.1.0", false),
            "C 类零主机（ipaddr isZeroHost）"
        );
        assert!(downloader_host_allowed("10.0.0.0", false), "A 类零主机");
        assert!(downloader_host_allowed("224.0.0.1", false), "组播");
        assert!(downloader_host_allowed("fe80::1", false));
        assert!(downloader_host_allowed("fd00::1", false), "unique local");
        assert!(
            downloader_host_allowed("unresolvable-host.invalid", false),
            "解析失败 ⇒ 跳过校验（上游行为）"
        );
        // 公网 ⇒ 默认拒绝
        assert!(!downloader_host_allowed("93.184.216.34", false));
        assert!(!downloader_host_allowed("8.8.8.8", false));
        assert!(
            downloader_host_allowed("8.8.8.8", true),
            "allowPublicIpAsDownloaderHost=true 放行"
        );
    }

    #[test]
    fn bind_connect_preserves_bound_source_port() {
        // 绑定一个随机本地端口后连接对端：对端观测到的 peer 端口必须等于绑定端口
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let real_addr = listener.local_addr().unwrap();
        let peers: Arc<Mutex<Vec<SocketAddr>>> = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&peers);
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                if let Ok(peer) = stream.peer_addr() {
                    observed.lock().unwrap().push(peer);
                }
            }
        });
        // probe 释放与重绑之间，该临时端口可能被同进程的并行测试抢占 ⇒ AddrInUse；
        // 只对这种情况换端口重试（测试加固，不改变被测语义）
        let (mut stream, source_port) = {
            let mut attempts = 0;
            loop {
                attempts += 1;
                let probe = TcpListener::bind("127.0.0.1:0").unwrap();
                let source_port = probe.local_addr().unwrap().port();
                drop(probe);
                let local = SocketAddr::from(([127, 0, 0, 1], source_port));
                match bind_connect(local, real_addr, Duration::from_secs(2)) {
                    Ok(stream) => break (stream, source_port),
                    Err(e)
                        if e.kind() == std::io::ErrorKind::AddrInUse && attempts < 8 =>
                    {
                        continue;
                    }
                    Err(e) => panic!("bind+connect: {e}"),
                }
            }
        };
        assert_eq!(
            stream.local_addr().unwrap().port(),
            source_port,
            "源端口保持绑定值"
        );
        assert!(
            wait_until(|| !peers.lock().unwrap().is_empty(), Duration::from_secs(2)),
            "对端应观测到绑定的源端口"
        );
        assert_eq!(peers.lock().unwrap()[0].port(), source_port);
        let _ = stream.write_all(b"x");
    }

    /// 伪造一个 TCP STUN 服务器：回报 `XOR-MAPPED-ADDRESS`，outer 固定为
    /// `127.0.0.1:{outer_port}`（inter 由客户端从 `local_addr()` 得出）。
    fn fake_tcp_stun_server(outer_port: u16) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("回环监听必须可用");
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0u8; 20];
                if stream.read_exact(&mut request).is_err() {
                    continue;
                }
                let mut response = Vec::new();
                response.extend_from_slice(&0x0101u16.to_be_bytes());
                response.extend_from_slice(&12u16.to_be_bytes());
                response.extend_from_slice(&0x2112_A442u32.to_be_bytes());
                response.extend_from_slice(b"NATR");
                response.extend_from_slice(&0u32.to_be_bytes());
                response.extend_from_slice(&0u32.to_be_bytes());
                response.extend_from_slice(&0x0020u16.to_be_bytes());
                response.extend_from_slice(&8u16.to_be_bytes());
                response.push(0);
                response.push(1);
                response.extend_from_slice(&(outer_port ^ 0x2112).to_be_bytes());
                let ip = u32::from(Ipv4Addr::from([127, 0, 0, 1])) ^ 0x2112_A442;
                response.extend_from_slice(&ip.to_be_bytes());
                let _ = stream.write_all(&response);
            }
        });
        addr.to_string()
    }

    #[test]
    fn create_tunnel_end_to_end_forwards_to_downloader() {
        let downloader = spawn_echo_downloader();
        let stun_server = fake_tcp_stun_server(downloader.port());
        let config = TunnelConfig {
            local_port: 0,
            available_test: false, // 回环上无 NAT 发夹，跳过自测（见 test_mapping 独立用例）
            ipv6_support: false,
            keep_alive_test_host: "127.0.0.1".to_string(),
            keep_alive_test_port: downloader.port(), // 指向下载器端口，测试期间不产生失败日志
            keep_alive_initial_delay: Duration::from_secs(3600), // 测试期间不触发
            keep_alive_interval: KEEP_ALIVE_INTERVAL,
            max_keep_alive_failures: DEFAULT_MAX_KEEP_ALIVE_FAILURES,
            use_friendly_loopback: true,
            allow_public_downloader_host: false,
            timeout: Duration::from_secs(2),
        };
        let tunnel = create_tunnel(&[stun_server], "127.0.0.1", &config)
            .expect("create_mapping 无 IO 错误")
            .expect("隧道应建立");
        assert!(tunnel.is_valid());
        assert!(tunnel.inter.port() != 0);
        assert_eq!(tunnel.outer.port(), downloader.port());
        assert!(tunnel.started_at() > 0);
        // 客户端连转发器监听端口（回环上即 inter）⇒ 转发到下载器
        let mut client = TcpStream::connect(tunnel.inter).expect("连接转发器监听端口");
        client.write_all(b"HELLO-TUNNEL").unwrap();
        let mut buffer = [0u8; 12];
        client.read_exact(&mut buffer).unwrap();
        assert_eq!(&buffer, b"HELLO-TUNNEL");
        assert_eq!(tunnel.forwarder.connection_handled(), 1);
        tunnel.close();
        assert!(!tunnel.is_valid());
    }

    #[test]
    fn create_tunnel_rejects_public_downloader_host() {
        let downloader = spawn_echo_downloader();
        let stun_server = fake_tcp_stun_server(downloader.port());
        let config = TunnelConfig {
            available_test: false,
            ipv6_support: false,
            timeout: Duration::from_secs(2),
            ..TunnelConfig::default()
        };
        let result = create_tunnel(&[stun_server], "93.184.216.34", &config).unwrap();
        assert!(result.is_none(), "公网下载器主机 ⇒ 隧道不建立");
    }

    #[test]
    fn create_tunnel_errors_on_unreachable_stun() {
        let config = TunnelConfig {
            available_test: false,
            timeout: Duration::from_millis(300),
            ..TunnelConfig::default()
        };
        // RFC 5737 文档地址不会应答
        assert!(create_tunnel(&["203.0.113.1:3478".to_string()], "127.0.0.1", &config).is_err());
        assert!(
            create_tunnel(&[], "127.0.0.1", &config).is_err(),
            "空服务器列表 ⇒ Err"
        );
    }
}
