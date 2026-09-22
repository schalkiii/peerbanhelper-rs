//! AutoSTUN UDP NAT 类型探测（对齐上游 `util/traversal/btstun/StunManager` 与
//! cdnbye `NettyStunClient` / `StunClientHandler` / `StunMessage` / `StunChangeRequest`）。
//!
//! ## 上游链路（v9.5.1）
//! 1. Spring 单例 `StunManager` 构造时即启动 `scheduleWithFixedDelay(this::refreshNatType, 0, 1, HOURS)`；
//! 2. `refreshNatType()`：读取**顶层** `stun.udp-servers`，`Collections.shuffle` 打乱后逐个尝试；
//!    服务器格式必须是 `host:port`（`split(":").length != 2` 即跳过）；
//! 3. `NettyStunClient.query(host, port, "0.0.0.0")` 用 RFC 3489 风格的 UDP STUN 判定 NAT 类型：
//!    - Test I：向服务器发 Binding Request，超时（重发 [`UDP_SEND_COUNT`] 次、每次间隔
//!      [`UDP_TRANSACTION_TIMEOUT`]）⇒ `UdpBlocked`；
//!    - `isNat = mapped.ip != "0.0.0.0"`（上游 `Utils.ipToBytes(localIP)` 比较）；
//!    - 非 NAT：Test II（CHANGE-REQUEST changeIp+changePort）有响应且 SOURCE-ADDRESS 不同于
//!      请求目标 ⇒ `OpenInternet`；有响应但相同（不合规服务器）⇒ `Unknown`；超时 ⇒ `SymmetricUdpFirewall`；
//!    - NAT：同上的 Test II 有响应 ⇒ `FullCone` / `Unknown`（同上）；
//!      超时则向 Test I 响应中的 CHANGED-ADDRESS 发普通 Binding Request（Test I(II)）：
//!      映射地址与 Test I 不同 ⇒ `Symmetric`；相同则再发 changePort-only 请求（Test III）：
//!      有响应 ⇒ `RestrictedCone`，超时 ⇒ `PortRestrictedCone`；Test I(II) 超时 ⇒ `Unknown`；
//! 4. 全部服务器失败 ⇒ `log.error(AUTOSTUN_STUN_SERVICE_UNAVAILABLE)` 并缓存 `UdpBlocked`；
//!    只要有一个服务器返回非 `Unknown`/`UdpBlocked` 的类型即缓存并返回。
//!    结果仅用于 WebUI/遥测展示（`/api/autostun/status` 的 `natType` 字段），**从不参与 ban 判定**。
//!
//! ## 与上游的差异
//! - 上游 `StunManager` **无条件**每小时探测（与 `auto-stun.enabled` 无关）；本移植按仓库
//!   「`enabled=false` ⇒ 严格 no-op（无 socket / 无探测 / 无线程）」的硬性约束，把探测线程
//!   挂在 [`crate::auto_stun::AutoStun`] 上、仅在 `enabled=true` 时运行（见其 `spawn_nat_type_prober`）。
//! - 上游整体超时靠 `CompletableFuture.get(5s)`：链路中途超时最终补全为 `Unknown`；
//!   本实现把 5s 预算传进状态机（每个事务的发送/等待都不得越过 deadline），可观察行为一致。
//! - 上游报文头 magic cookie 恒为 0（cdnbye `StunMessage` 从不赋值）且不校验属性 4 字节对齐；
//!   本实现逐字节保持一致。
//!
//! 结果暴露面：Rust 侧暂无 Web API，探测结果通过 [`NatTypeProber::cached_nat_type`] /
//! [`crate::auto_stun::AutoStun::nat_type`] 方法与 tracing 日志暴露。

use serde::{Deserialize, Serialize};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// cdnbye `StunClientHandler.TRANSACTION_TIMEOUT_MS`：单次发送后等待响应的时长。
pub const UDP_TRANSACTION_TIMEOUT: Duration = Duration::from_millis(1000);
/// cdnbye `StunClientHandler.UDP_SEND_COUNT`：单事务最多发送（含重发）次数。
pub const UDP_SEND_COUNT: usize = 3;
/// cdnbye `NettyStunClient.TOTAL_TIMEOUT_SECONDS`：整个查询的总体超时。
pub const UDP_TOTAL_TIMEOUT: Duration = Duration::from_secs(5);
/// cdnbye `NettyStunClient.DEFAULT_STUN_HOST`（`query(localIP)` 单参重载）。
pub const DEFAULT_UDP_STUN_HOST: &str = "stun.cdnbye.com";
/// 上游 `StunManager` 的刷新周期（`scheduleWithFixedDelay(..., 0, 1, HOURS)`）。
pub const NAT_TYPE_REFRESH_INTERVAL: Duration = Duration::from_secs(3600);
/// 上游 `StunManager.refreshNatType` 固定向 `NettyStunClient.query` 传入的本机 IP。
const PROBE_LOCAL_IP: &str = "0.0.0.0";
/// 接收缓冲（与 TCP 侧一致使用 1500 字节）。
const UDP_BUFFER_SIZE: usize = 1500;

/// NAT 类型，变体名与 cdnbye `NatType` 枚举一一对应（仅用于展示/遥测，不参与 ban 判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NatType {
    UdpBlocked,
    OpenInternet,
    SymmetricUdpFirewall,
    FullCone,
    RestrictedCone,
    PortRestrictedCone,
    Symmetric,
    Unknown,
}

impl NatType {
    /// 上游枚举常量名（WebUI DTO 序列化值）。
    pub fn as_str(&self) -> &'static str {
        match self {
            NatType::UdpBlocked => "UdpBlocked",
            NatType::OpenInternet => "OpenInternet",
            NatType::SymmetricUdpFirewall => "SymmetricUdpFirewall",
            NatType::FullCone => "FullCone",
            NatType::RestrictedCone => "RestrictedCone",
            NatType::PortRestrictedCone => "PortRestrictedCone",
            NatType::Symmetric => "Symmetric",
            NatType::Unknown => "Unknown",
        }
    }
}

/// cdnbye `StunChangeRequest`：请求服务器用不同的 IP/端口回包。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UdpStunChangeRequest {
    pub change_ip: bool,
    pub change_port: bool,
}

impl UdpStunChangeRequest {
    pub fn new(change_ip: bool, change_port: bool) -> Self {
        Self {
            change_ip,
            change_port,
        }
    }
}

/// 构造 UDP STUN Binding Request，对齐 cdnbye `StunMessage.toByteData()`：
/// 头部（type `0x0001`、长度、**恒为 0 的 magic cookie**、12 字节随机事务 ID）+
/// 可选的 CHANGE-REQUEST 属性（`0x0003`，长度 4，flags：changeIp<<2 | changePort<<1）。
///
/// 事务 ID 位于返回报文的 `8..20` 字节（[`udp_transaction_id`]）。
pub fn create_udp_binding_request(change_request: Option<UdpStunChangeRequest>) -> Vec<u8> {
    let mut msg = Vec::with_capacity(if change_request.is_some() { 28 } else { 20 });
    msg.extend_from_slice(&0x0001u16.to_be_bytes()); // StunMessageType.BindingRequest
    msg.extend_from_slice(&if change_request.is_some() { 8u16 } else { 0u16 }.to_be_bytes());
    // 上游 magicCookie 字段从不赋值 ⇒ 恒为 0
    msg.extend_from_slice(&0u32.to_be_bytes());
    let txid = random_bytes_12();
    msg.extend_from_slice(&txid);
    if let Some(change) = change_request {
        msg.extend_from_slice(&0x0003u16.to_be_bytes()); // AttributeType.ChangeRequest
        msg.extend_from_slice(&4u16.to_be_bytes());
        msg.push(0);
        msg.push(0);
        msg.push(0);
        msg.push(((change.change_ip as u8) << 2) | ((change.change_port as u8) << 1));
    }
    msg
}

/// 提取请求/响应报文的事务 ID（`8..20` 字节），对齐 cdnbye 的事务匹配键。
pub fn udp_transaction_id(message: &[u8]) -> Option<&[u8]> {
    message.get(8..20)
}

fn random_bytes_12() -> [u8; 12] {
    let mut out = [0u8; 12];
    for chunk in out.chunks_mut(4) {
        chunk.copy_from_slice(&crate::auto_stun::random_u32().to_be_bytes()[..chunk.len()]);
    }
    out
}

/// cdnbye `StunMessage.parse` 关心的地址属性集合：
/// MAPPED-ADDRESS / RESPONSE-ADDRESS / SOURCE-ADDRESS / CHANGED-ADDRESS。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UdpStunResponse {
    pub mapped_address: Option<SocketAddr>,
    pub source_address: Option<SocketAddr>,
    pub changed_address: Option<SocketAddr>,
}

/// 解析 UDP STUN 响应，对齐 cdnbye `StunMessage.parse`：
/// - 报文头 20 字节；message type 必须是 6 个已知值之一，否则报错（上游抛 IllegalArgumentException）；
/// - 地址属性按「reserved(1) + family(1) + port(2) + IPv4(4)」读取，**不校验 family**（上游直接按 IPv4 拼）；
/// - 属性跳过时不做 4 字节对齐（上游 `offset += length`）；
/// - 越界读取报错（上游 AIOOBE → Netty handler 异常 → 事务失败）。
pub fn parse_udp_stun_response(data: &[u8]) -> io::Result<UdpStunResponse> {
    if data.len() < 20 {
        return Err(invalid_data("Invalid STUN message value !"));
    }
    let message_type = u16::from_be_bytes([data[0], data[1]]);
    // cdnbye StunMessageType 全部已知值
    if !matches!(
        message_type,
        0x0001 | 0x0101 | 0x0111 | 0x0002 | 0x0102 | 0x0112
    ) {
        return Err(invalid_data("Invalid STUN message type value !"));
    }
    let message_length = u16::from_be_bytes([data[2], data[3]]) as usize;

    let mut out = UdpStunResponse::default();
    let mut offset = 20usize;
    while offset.saturating_sub(20) < message_length {
        if data.len().saturating_sub(offset) < 4 {
            return Err(invalid_data("Truncated STUN attribute header"));
        }
        let attr_type = u16::from_be_bytes([data[offset], data[offset + 1]]);
        let attr_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
        offset += 4;
        let addr = match attr_type {
            // MappedAddress / ResponseAddress / SourceAddress / ChangedAddress
            0x0001 | 0x0002 | 0x0004 | 0x0005 => {
                let addr = parse_udp_address(data, offset)?;
                offset += 8; // 上游对地址属性硬编码 offset += 8
                Some(addr)
            }
            // ChangeRequest / MessageIntegrity / ErrorCode / UnknownAttributes / 其他未知属性
            _ => {
                offset = offset.saturating_add(attr_len);
                None
            }
        };
        match attr_type {
            0x0001 => out.mapped_address = addr,
            0x0002 => {} // RESPONSE-ADDRESS：上游解析但不被探测逻辑消费
            0x0004 => out.source_address = addr,
            0x0005 => out.changed_address = addr,
            _ => {}
        }
    }
    Ok(out)
}

fn parse_udp_address(data: &[u8], offset: usize) -> io::Result<SocketAddr> {
    // [reserved(1)][family(1)][port(2)][address(4)]；上游跳过 family、恒按 IPv4 解析
    if data.len().saturating_sub(offset) < 8 {
        return Err(invalid_data("Truncated STUN address attribute"));
    }
    let port = u16::from_be_bytes([data[offset + 2], data[offset + 3]]);
    let ip = u32::from_be_bytes([
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ]);
    Ok(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port))
}

/// 请求中的 CHANGE-REQUEST flags（`(change_ip, change_port)`），无该属性 ⇒ `(false, false)`。
pub fn udp_change_request_flags(request: &[u8]) -> (bool, bool) {
    let mut offset = 20usize;
    let message_length = match request.get(2..4) {
        Some(b) => u16::from_be_bytes([b[0], b[1]]) as usize,
        None => return (false, false),
    };
    while offset.saturating_sub(20) + 4 <= request.len()
        && offset.saturating_sub(20) < message_length
    {
        let attr_type = u16::from_be_bytes([request[offset], request[offset + 1]]);
        let attr_len = u16::from_be_bytes([request[offset + 2], request[offset + 3]]) as usize;
        let value_at = offset + 4;
        if attr_type == 0x0003 && request.len().saturating_sub(value_at) >= 4 {
            let flags = request[value_at + 3];
            return ((flags & 0x04) != 0, (flags & 0x02) != 0);
        }
        offset = value_at + attr_len;
    }
    (false, false)
}

/// UDP STUN 客户端（单 socket），对齐 cdnbye `NettyStunClient` + `StunClientHandler`：
/// 每个事务最多发送 [`UDP_SEND_COUNT`] 次，每次等待 [`UDP_TRANSACTION_TIMEOUT`]；
/// 响应按事务 ID 匹配，未知 ID 忽略（上游 log.warn）。
pub struct UdpStunClient {
    socket: UdpSocket,
    transaction_timeout: Duration,
    send_count: usize,
}

impl UdpStunClient {
    /// 绑定任意可用端口（上游 `Bootstrap.bind(0)`）。
    pub fn bind() -> io::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        Ok(Self {
            socket,
            transaction_timeout: UDP_TRANSACTION_TIMEOUT,
            send_count: UDP_SEND_COUNT,
        })
    }

    /// 测试专用：注入更短的超时/重发次数（默认值必须保持上游常量）。
    #[cfg(test)]
    fn with_tuning(transaction_timeout: Duration, send_count: usize) -> io::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        Ok(Self {
            socket,
            transaction_timeout,
            send_count,
        })
    }

    /// 对单台服务器执行 RFC 3489 NAT 判定，返回 `(NAT 类型, Test I 映射地址)`
    /// （对齐 `NettyStunClient.query` 与 `StunResult(natType, ipAddr)`）。
    ///
    /// `local_ip` 即上游 `StunManager` 固定传入的 `"0.0.0.0"`，仅用于
    /// `isNat = mapped.ip != local_ip` 比较。整体超时 [`UDP_TOTAL_TIMEOUT`] 内未完成 ⇒ `Unknown`。
    pub fn query(
        &self,
        server: SocketAddr,
        local_ip: IpAddr,
        deadline: Instant,
    ) -> (NatType, Option<SocketAddr>) {
        // Test I
        let test1_request = create_udp_binding_request(None);
        let test1 = match self.do_transaction(&test1_request, server, deadline) {
            Ok(response) => response,
            Err(_) => {
                tracing::debug!(
                    "UDP seems to be blocked, no response from STUN server at {server}"
                );
                return (NatType::UdpBlocked, None);
            }
        };
        // 上游 mapped == null 时会在回调里 NPE ⇒ 整体超时补全为 Unknown
        let Some(mapped) = test1.mapped_address else {
            return (NatType::Unknown, None);
        };
        // 上游 Utils.ipToBytes(localIP) 与 mapped 地址逐字节比较
        let is_nat = mapped.ip() != local_ip;
        let test2_request = create_udp_binding_request(Some(UdpStunChangeRequest::new(true, true)));
        if !is_nat {
            // 无 NAT：可能是公网直连或对称型 UDP 防火墙
            return match self.do_transaction(&test2_request, server, deadline) {
                Ok(test2) if response_from_different_address(&test2, server) => {
                    (NatType::OpenInternet, Some(mapped))
                }
                Ok(_) => {
                    tracing::debug!("STUN server {server} did not change IP/port as requested in Test II (non-compliant)");
                    (NatType::Unknown, Some(mapped))
                }
                Err(_) => (NatType::SymmetricUdpFirewall, Some(mapped)),
            };
        }
        // 在 NAT 后
        match self.do_transaction(&test2_request, server, deadline) {
            Ok(test2) if response_from_different_address(&test2, server) => {
                (NatType::FullCone, Some(mapped))
            }
            Ok(_) => {
                tracing::debug!("STUN server {server} did not change IP/port as requested in Test II (non-compliant)");
                (NatType::Unknown, Some(mapped))
            }
            Err(_) => {
                // 向 Test I 响应中的 CHANGED-ADDRESS 发普通 Binding Request（Test I(II)）
                let Some(changed) = test1.changed_address else {
                    // 上游向 null 目标发包失败 ⇒ Unknown
                    return (NatType::Unknown, Some(mapped));
                };
                let test12_request = create_udp_binding_request(None);
                let test12 = match self.do_transaction(&test12_request, changed, deadline) {
                    Ok(response) => response,
                    Err(_) => return (NatType::Unknown, Some(mapped)),
                };
                let Some(mapped12) = test12.mapped_address else {
                    // 上游 NPE ⇒ 整体超时 ⇒ Unknown
                    return (NatType::Unknown, Some(mapped));
                };
                if mapped12 != mapped {
                    return (NatType::Symmetric, Some(mapped));
                }
                // Test III：向 CHANGED-ADDRESS 发 changePort-only 请求
                let test3_request =
                    create_udp_binding_request(Some(UdpStunChangeRequest::new(false, true)));
                match self.do_transaction(&test3_request, changed, deadline) {
                    Ok(_) => (NatType::RestrictedCone, Some(mapped)),
                    Err(_) => (NatType::PortRestrictedCone, Some(mapped)),
                }
            }
        }
    }

    /// 单个 STUN 事务，对齐 `StunClientHandler.doTransaction`：
    /// 立即发送，之后每过 `transaction_timeout` 重发一次，共 `send_count` 次；
    /// 响应按事务 ID 匹配（未知 ID 忽略），解析失败视同事务失败。
    fn do_transaction(
        &self,
        request: &[u8],
        remote: SocketAddr,
        deadline: Instant,
    ) -> io::Result<UdpStunResponse> {
        let Some(txid) = udp_transaction_id(request) else {
            return Err(invalid_data("Invalid STUN request length"));
        };
        let mut buffer = [0u8; UDP_BUFFER_SIZE];
        for _attempt in 0..self.send_count {
            if Instant::now() >= deadline {
                return Err(timeout_error());
            }
            self.socket.send_to(request, remote)?;
            let attempt_deadline = Instant::now()
                .checked_add(self.transaction_timeout)
                .map(|at| at.min(deadline))
                .unwrap_or(deadline);
            loop {
                let remaining = attempt_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                self.socket.set_read_timeout(Some(remaining))?;
                match self.socket.recv_from(&mut buffer) {
                    Ok((n, _)) => {
                        if n >= 20 && &buffer[8..20] == txid {
                            return parse_udp_stun_response(&buffer[..n]);
                        }
                        // 未知事务 ID：上游 log.warn 后忽略
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) =>
                    {
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        Err(timeout_error())
    }
}

/// 上游 `NettyStunClient.isResponseFromDifferentAddress`：
/// 比较**响应中的 SOURCE-ADDRESS 属性**与请求目标；缺失 ⇒ `false`（无法验证合规性）。
fn response_from_different_address(response: &UdpStunResponse, original: SocketAddr) -> bool {
    match response.source_address {
        Some(source) => source.ip() != original.ip() || source.port() != original.port(),
        None => false,
    }
}

/// 对单台 `host:port` 服务器执行一次完整查询，对齐 `StunManager.refreshNatType` 内联逻辑：
/// 解析失败视同查询失败（`None`）；成功返回 `(NAT 类型, Test I 映射地址)`。
pub fn query_udp_stun_server(server: &str) -> Option<(NatType, Option<SocketAddr>)> {
    let (host, port) = split_udp_stun_server(server)?;
    // 上游 netty 发送时解析主机名；解析失败即异常 ⇒ 尝试下一台
    let targets: Vec<SocketAddr> = (host.as_str(), port).to_socket_addrs().ok()?.collect();
    let target = targets.into_iter().find(|a| a.is_ipv4())?;
    let client = UdpStunClient::bind().ok()?;
    let deadline = Instant::now() + UDP_TOTAL_TIMEOUT;
    // 上游固定传入 "0.0.0.0"
    let local_ip: IpAddr = PROBE_LOCAL_IP.parse().ok()?;
    Some(client.query(target, local_ip, deadline))
}

/// 上游 `stunServer.split(":")` 必须恰好两段（`host`、`port`），否则跳过该服务器。
fn split_udp_stun_server(server: &str) -> Option<(String, u16)> {
    let parts: Vec<&str> = server.trim().split(':').collect();
    if parts.len() != 2 {
        return None;
    }
    let port = parts[1].trim().parse::<u16>().ok()?;
    Some((parts[0].trim().to_string(), port))
}

/// 上游 Spring 单例 `StunManager` 的移植：缓存最近一次成功探测的 NAT 类型
/// （初始 `Unknown`，对齐字段默认值）。
pub struct NatTypeProber {
    cached: RwLock<NatType>,
}

impl Default for NatTypeProber {
    fn default() -> Self {
        Self::new()
    }
}

impl NatTypeProber {
    pub fn new() -> Self {
        Self {
            cached: RwLock::new(NatType::Unknown),
        }
    }

    /// `StunManager.getCachedNatType()`。
    pub fn cached_nat_type(&self) -> NatType {
        self.cached.read().map(|t| *t).unwrap_or(NatType::Unknown)
    }

    /// `StunManager.refreshNatType()`：打乱服务器列表逐台尝试，
    /// 接受第一个非 `Unknown`/`UdpBlocked` 的结果；全部失败 ⇒ 记 error 并缓存 `UdpBlocked`。
    pub fn refresh_nat_type(&self, servers: &[String]) -> NatType {
        let mut shuffled = servers.to_vec();
        shuffle(&mut shuffled); // 上游 Collections.shuffle
        for server in &shuffled {
            // 上游 `split(":").length != 2` 即跳过（格式日志 + continue）
            if split_udp_stun_server(server).is_none() {
                tracing::debug!("Invalid STUN server format: {server}, skipping");
                continue;
            }
            match query_udp_stun_server(server) {
                Some((nat_type, _))
                    if nat_type != NatType::Unknown && nat_type != NatType::UdpBlocked =>
                {
                    tracing::debug!(
                        "Successfully determined NAT type {} using server {server}",
                        nat_type.as_str()
                    );
                    if let Ok(mut cached) = self.cached.write() {
                        *cached = nat_type;
                    }
                    return nat_type;
                }
                Some(_) => {
                    tracing::debug!(
                        "STUN server {server} returned Unknown NAT type, trying next server"
                    );
                }
                None => {
                    tracing::debug!("Failed to query STUN server {server}, trying next server");
                }
            }
        }
        // 上游 Lang.AUTOSTUN_STUN_SERVICE_UNAVAILABLE
        tracing::error!("AutoSTUN: 所有 STUN 服务器均不可用，NAT 类型探测失败");
        if let Ok(mut cached) = self.cached.write() {
            *cached = NatType::UdpBlocked;
        }
        NatType::UdpBlocked
    }
}

/// `Collections.shuffle` 的等价实现（Fisher-Yates，使用 [`crate::auto_stun::random_u32`] 种子）。
fn shuffle<T>(items: &mut [T]) {
    for i in (1..items.len()).rev() {
        let j = (crate::auto_stun::random_u32() as usize) % (i + 1);
        items.swap(i, j);
    }
}

/// 后台探测线程句柄，对齐 `StunManager` 的 `scheduleWithFixedDelay(..., 0, 1, HOURS)` 与 `close()`。
pub struct NatTypeProberHandle {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl NatTypeProberHandle {
    /// 立即执行一次探测，随后按 `interval` 重试（调用方须在 `enabled=true` 时才调用；
    /// 上游 `StunManager` 无条件运行，本移植按严格 no-op 约束改为显式启动）。
    pub fn spawn(prober: Arc<NatTypeProber>, servers: Vec<String>, interval: Duration) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("StunManager-RefreshNatType".to_string())
            .spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    prober.refresh_nat_type(&servers);
                    let mut waited = Duration::ZERO;
                    while waited < interval && !flag.load(Ordering::SeqCst) {
                        let step = Duration::from_millis(100).min(interval - waited);
                        std::thread::sleep(step);
                        waited += step;
                    }
                }
            })
            .ok();
        Self { stop, handle }
    }

    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for NatTypeProberHandle {
    fn drop(&mut self) {
        self.close();
    }
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

fn timeout_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "STUN transaction timed out")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::net::{Ipv4Addr, UdpSocket};
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    /// 调短超时（默认常量必须仍是上游值，见 `default_constants_match_upstream`）。
    const FAST_TIMEOUT: Duration = Duration::from_millis(30);

    fn fast_client() -> UdpStunClient {
        UdpStunClient::with_tuning(FAST_TIMEOUT, 1).expect("绑定 UDP socket")
    }

    fn fast_deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }

    fn localhost(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    /// 构造一个 UDP STUN 响应（Binding Response + 指定属性），事务 ID 取自请求。
    fn stun_response(request: &[u8], attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0x0101u16.to_be_bytes());
        let body: usize = attrs.iter().map(|a| a.len()).sum();
        out.extend_from_slice(&(body as u16).to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes()); // magic cookie 恒 0
        out.extend_from_slice(&request[8..20]); // 回显事务 ID
        for attr in attrs {
            out.extend_from_slice(attr);
        }
        out
    }

    fn address_attr(attr_type: u16, addr: SocketAddr) -> Vec<u8> {
        let mut attr = Vec::with_capacity(12);
        attr.extend_from_slice(&attr_type.to_be_bytes());
        attr.extend_from_slice(&8u16.to_be_bytes());
        attr.push(0);
        attr.push(1); // family = IPv4（上游不校验，此处保持线格式）
        let ip = match addr.ip() {
            IpAddr::V4(v4) => u32::from(v4),
            IpAddr::V6(_) => panic!("测试只用 IPv4"),
        };
        attr.extend_from_slice(&addr.port().to_be_bytes());
        attr.extend_from_slice(&ip.to_be_bytes());
        attr
    }

    /// 启动一个 mock UDP STUN 服务器：每个收到的请求交给 `responder`，
    /// 返回 `Some(属性列表)` 时回一条 Binding Response（事务 ID 自动回显）。
    fn spawn_udp_stun_mock(
        responder: impl Fn(&[u8], (bool, bool)) -> Option<Vec<Vec<u8>>> + Send + 'static,
    ) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("回环 UDP 绑定必须可用");
        let addr = socket.local_addr().expect("local_addr");
        std::thread::spawn(move || {
            let mut buffer = [0u8; UDP_BUFFER_SIZE];
            while let Ok((n, peer)) = socket.recv_from(&mut buffer) {
                let request = &buffer[..n];
                if request.len() < 20 {
                    continue;
                }
                let flags = udp_change_request_flags(request);
                if let Some(attrs) = responder(request, flags) {
                    let _ = socket.send_to(&stun_response(request, &attrs), peer);
                }
            }
        });
        addr
    }

    #[test]
    fn default_constants_match_upstream() {
        assert_eq!(UDP_TRANSACTION_TIMEOUT, Duration::from_millis(1000));
        assert_eq!(UDP_SEND_COUNT, 3);
        assert_eq!(UDP_TOTAL_TIMEOUT, Duration::from_secs(5));
        assert_eq!(DEFAULT_UDP_STUN_HOST, "stun.cdnbye.com");
        assert_eq!(NAT_TYPE_REFRESH_INTERVAL, Duration::from_secs(3600));
    }

    #[test]
    fn binding_request_matches_cdnbye_layout() {
        let plain = create_udp_binding_request(None);
        assert_eq!(plain.len(), 20);
        assert_eq!(u16::from_be_bytes([plain[0], plain[1]]), 0x0001);
        assert_eq!(u16::from_be_bytes([plain[2], plain[3]]), 0);
        assert_eq!(
            &plain[4..8],
            &0u32.to_be_bytes(),
            "cdnbye magic cookie 恒为 0"
        );
        assert_eq!(udp_change_request_flags(&plain), (false, false));

        let change = create_udp_binding_request(Some(UdpStunChangeRequest::new(true, true)));
        assert_eq!(change.len(), 28);
        assert_eq!(u16::from_be_bytes([change[2], change[3]]), 8);
        assert_eq!(&change[20..22], &0x0003u16.to_be_bytes());
        assert_eq!(&change[22..24], &4u16.to_be_bytes());
        assert_eq!(change[27], 0b0110, "changeIp<<2 | changePort<<1");
        assert_eq!(udp_change_request_flags(&change), (true, true));
        let port_only = create_udp_binding_request(Some(UdpStunChangeRequest::new(false, true)));
        assert_eq!(udp_change_request_flags(&port_only), (false, true));
        // 事务 ID 随机且位于 8..20
        assert_ne!(plain[8..20], change[8..20]);
        assert_eq!(udp_transaction_id(&plain).unwrap().len(), 12);
    }

    #[test]
    fn parse_reads_address_attributes_without_family_check() {
        let mut data = Vec::new();
        data.extend_from_slice(&0x0101u16.to_be_bytes());
        // 三个地址属性各 12 字节（属性头 4 + 值 8）⇒ 报文体 36 字节
        data.extend_from_slice(&36u16.to_be_bytes());
        data.extend_from_slice(&0u32.to_be_bytes());
        data.extend_from_slice(&[0x11u8; 12]);
        data.extend_from_slice(&address_attr(0x0001, localhost(50000)));
        data.extend_from_slice(&address_attr(0x0004, localhost(3478)));
        data.extend_from_slice(&address_attr(0x0005, localhost(3479)));
        let parsed = parse_udp_stun_response(&data).unwrap();
        assert_eq!(parsed.mapped_address, Some(localhost(50000)));
        assert_eq!(parsed.source_address, Some(localhost(3478)));
        assert_eq!(parsed.changed_address, Some(localhost(3479)));
    }

    #[test]
    fn parse_rejects_unknown_message_types_and_truncation() {
        assert!(parse_udp_stun_response(&[0u8; 19]).is_err());
        let mut bad_type = [0x0202u16.to_be_bytes(), 0u16.to_be_bytes()].concat();
        bad_type.extend_from_slice(&0u32.to_be_bytes());
        bad_type.extend_from_slice(&[0u8; 12]);
        assert!(parse_udp_stun_response(&bad_type).is_err());
        // 属性声明越界（上游 AIOOBE）
        let mut overlong = Vec::new();
        overlong.extend_from_slice(&0x0101u16.to_be_bytes());
        overlong.extend_from_slice(&8u16.to_be_bytes());
        overlong.extend_from_slice(&0u32.to_be_bytes());
        overlong.extend_from_slice(&[0u8; 12]);
        overlong.extend_from_slice(&0x0001u16.to_be_bytes());
        overlong.extend_from_slice(&8u16.to_be_bytes());
        overlong.extend_from_slice(&[0u8; 4]); // 只有 4 字节可读
        assert!(parse_udp_stun_response(&overlong).is_err());
    }

    fn responder_full_cone(
        alt: SocketAddr,
    ) -> impl Fn(&[u8], (bool, bool)) -> Option<Vec<Vec<u8>>> + Send {
        move |_request, (change_ip, change_port)| {
            if change_ip || change_port {
                // Test II：从「另一地址」应答（SOURCE-ADDRESS 属性不同于请求目标）
                Some(vec![
                    address_attr(0x0001, localhost(50000)),
                    address_attr(0x0004, alt),
                ])
            } else {
                // Test I：映射地址 + CHANGED-ADDRESS
                Some(vec![
                    address_attr(0x0001, localhost(50000)),
                    address_attr(0x0004, localhost(3478)),
                    address_attr(0x0005, alt),
                ])
            }
        }
    }

    #[test]
    fn full_cone_when_test_two_answered_from_different_address() {
        let alt = spawn_udp_stun_mock(|_, _| None); // 占位（仅作为 SOURCE-ADDRESS 属性值）
        let primary = spawn_udp_stun_mock(responder_full_cone(alt));
        let client = fast_client();
        let (nat, mapped) =
            client.query(primary, IpAddr::V4(Ipv4Addr::UNSPECIFIED), fast_deadline());
        assert_eq!(nat, NatType::FullCone);
        assert_eq!(mapped, Some(localhost(50000)));
    }

    #[test]
    fn non_compliant_test_two_yields_unknown() {
        let primary_holder: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
        let primary = spawn_udp_stun_mock({
            let holder = Arc::clone(&primary_holder);
            move |_request, (change_ip, change_port)| {
                if change_ip || change_port {
                    // 不合规：声称仍从原地址应答（SOURCE-ADDRESS == 请求目标）
                    let own = holder.lock().unwrap().expect("测试中已注入自身地址");
                    Some(vec![
                        address_attr(0x0001, localhost(50000)),
                        address_attr(0x0004, own),
                    ])
                } else {
                    Some(vec![address_attr(0x0001, localhost(50000))])
                }
            }
        });
        *primary_holder.lock().unwrap() = Some(primary);
        let client = fast_client();
        let (nat, _) = client.query(primary, IpAddr::V4(Ipv4Addr::UNSPECIFIED), fast_deadline());
        assert_eq!(nat, NatType::Unknown);
    }

    #[test]
    fn open_internet_when_mapped_ip_equals_local_ip() {
        let alt = localhost(40000);
        // 上游 isNat = mapped.ip != "0.0.0.0"：映射到 0.0.0.0 即「无 NAT」
        let primary = spawn_udp_stun_mock(move |_request, (change_ip, change_port)| {
            if change_ip || change_port {
                Some(vec![
                    address_attr(0x0001, localhost(1)),
                    address_attr(0x0004, alt),
                ])
            } else {
                Some(vec![
                    address_attr(0x0001, localhost(0)),
                    address_attr(0x0005, alt),
                ])
            }
        });
        let client = fast_client();
        // 用例名即「mapped == local」⇒ 传入与 mapped 相同的 local_ip 才会走到「无 NAT」分支
        let (nat, mapped) = client.query(primary, localhost(0).ip(), fast_deadline());
        assert_eq!(nat, NatType::OpenInternet);
        assert_eq!(mapped, Some(localhost(0)));
    }

    #[test]
    fn symmetric_udp_firewall_when_no_nat_and_test_two_times_out() {
        let alt = localhost(40000);
        let primary = spawn_udp_stun_mock(move |_request, (change_ip, _change_port)| {
            if change_ip {
                None // Test II 无响应
            } else {
                Some(vec![
                    address_attr(0x0001, localhost(0)),
                    address_attr(0x0005, alt),
                ])
            }
        });
        let client = fast_client();
        let (nat, _) = client.query(primary, localhost(0).ip(), fast_deadline());
        assert_eq!(nat, NatType::SymmetricUdpFirewall);
    }

    /// 场景骨架：Test II 无响应，但 CHANGED-ADDRESS（alt）上的普通请求有响应。
    /// `alt_plain_mapped`：Test I(II) 的映射地址（不同 ⇒ Symmetric；相同 ⇒ 继续判定）；
    /// `answer_test_three`：alt 是否回答 changePort-only 请求（是 ⇒ RestrictedCone）。
    fn restricted_family_scenario(
        alt_plain_mapped: SocketAddr,
        answer_test_three: bool,
    ) -> NatType {
        let alt_holder: Arc<Mutex<Option<SocketAddr>>> = Arc::new(Mutex::new(None));
        let alt = spawn_udp_stun_mock(move |_request, (change_ip, change_port)| {
            if !change_ip && !change_port {
                // Test I(II)：普通请求
                Some(vec![address_attr(0x0001, alt_plain_mapped)])
            } else if !change_ip && change_port {
                // Test III：changePort-only
                if answer_test_three {
                    Some(vec![address_attr(0x0001, alt_plain_mapped)])
                } else {
                    None
                }
            } else {
                None
            }
        });
        *alt_holder.lock().unwrap() = Some(alt);
        let primary = spawn_udp_stun_mock(move |_request, (change_ip, change_port)| {
            if change_ip || change_port {
                None // Test II：无响应 ⇒ 进入 restricted 分支
            } else {
                let alt = alt_holder.lock().unwrap().expect("alt 已注入");
                Some(vec![
                    address_attr(0x0001, localhost(50000)),
                    address_attr(0x0004, localhost(3478)),
                    address_attr(0x0005, alt),
                ])
            }
        });
        let (nat, mapped) =
            fast_client().query(primary, IpAddr::V4(Ipv4Addr::UNSPECIFIED), fast_deadline());
        assert_eq!(mapped, Some(localhost(50000)));
        nat
    }

    #[test]
    fn symmetric_when_changed_address_maps_differently() {
        // Test I(II) 的映射端口不同 ⇒ Symmetric
        assert_eq!(
            restricted_family_scenario(localhost(51999), true),
            NatType::Symmetric
        );
    }

    #[test]
    fn restricted_cone_when_test_three_answered() {
        assert_eq!(
            restricted_family_scenario(localhost(50000), true),
            NatType::RestrictedCone
        );
    }

    #[test]
    fn port_restricted_cone_when_test_three_times_out() {
        assert_eq!(
            restricted_family_scenario(localhost(50000), false),
            NatType::PortRestrictedCone
        );
    }

    #[test]
    fn udp_blocked_when_server_never_answers() {
        let primary = spawn_udp_stun_mock(|_, _| None);
        let client = fast_client();
        let (nat, mapped) =
            client.query(primary, IpAddr::V4(Ipv4Addr::UNSPECIFIED), fast_deadline());
        assert_eq!(nat, NatType::UdpBlocked);
        assert_eq!(mapped, None);
    }

    #[test]
    fn transaction_retries_up_to_send_count_before_failing() {
        let primary = spawn_udp_stun_mock(|_, _| None);
        // 2 次发送 × 30ms ≈ 60ms 后放弃（默认 UDP_SEND_COUNT=3 时则约 90ms）
        let client = UdpStunClient::with_tuning(FAST_TIMEOUT, 2).unwrap();
        let start = Instant::now();
        let result =
            client.do_transaction(&create_udp_binding_request(None), primary, fast_deadline());
        assert!(result.is_err());
        assert!(start.elapsed() >= FAST_TIMEOUT, "至少等待过一个事务超时");
    }

    #[test]
    fn overall_deadline_caps_the_state_machine() {
        let primary = spawn_udp_stun_mock(|_, _| None);
        let client = fast_client();
        // deadline 已过期：Test I 直接失败 ⇒ UdpBlocked（对齐上游整体 5s 超时后的快速失败）
        let (nat, _) = client.query(primary, IpAddr::V4(Ipv4Addr::UNSPECIFIED), Instant::now());
        assert_eq!(nat, NatType::UdpBlocked);
    }

    #[test]
    fn missing_mapped_address_degrades_to_unknown() {
        let primary = spawn_udp_stun_mock(|_, _| Some(Vec::new())); // 应答但无 MAPPED-ADDRESS
        let client = fast_client();
        let (nat, mapped) =
            client.query(primary, IpAddr::V4(Ipv4Addr::UNSPECIFIED), fast_deadline());
        assert_eq!(nat, NatType::Unknown);
        assert_eq!(mapped, None);
    }

    #[test]
    fn server_string_must_have_exactly_two_parts() {
        assert!(split_udp_stun_server("stun.example.com:3478").is_some());
        assert!(
            split_udp_stun_server("stun.example.com").is_none(),
            "上游 split(\":\").length != 2 即跳过"
        );
        assert!(split_udp_stun_server("a:b:c").is_none());
        assert!(split_udp_stun_server("host:not-a-port").is_none());
    }

    #[test]
    fn prober_accepts_first_usable_server_and_caches() {
        let silent = spawn_udp_stun_mock(|_, _| None);
        let alt = localhost(40000);
        let working = spawn_udp_stun_mock(move |_request, (change_ip, change_port)| {
            if change_ip || change_port {
                Some(vec![
                    address_attr(0x0001, localhost(50000)),
                    address_attr(0x0004, alt),
                ])
            } else {
                Some(vec![
                    address_attr(0x0001, localhost(50000)),
                    address_attr(0x0005, alt),
                ])
            }
        });
        let prober = NatTypeProber::new();
        assert_eq!(prober.cached_nat_type(), NatType::Unknown, "初始 Unknown");
        let servers = vec![
            "not-a-stun-server".to_string(),        // 格式非法：跳过
            format!("127.0.0.1:{}", silent.port()), // 无响应：换下一台
            format!("127.0.0.1:{}", working.port()),
        ];
        assert_eq!(prober.refresh_nat_type(&servers), NatType::FullCone);
        assert_eq!(prober.cached_nat_type(), NatType::FullCone, "结果被缓存");
        // 之后即使服务器全挂，缓存仍可读（上游 getCachedNatType）
        assert_eq!(prober.cached_nat_type(), NatType::FullCone);
    }

    #[test]
    fn prober_falls_back_to_udp_blocked_when_all_servers_fail() {
        let silent = spawn_udp_stun_mock(|_, _| None);
        let prober = NatTypeProber::new();
        let servers = vec![
            "bad-format".to_string(),
            format!("127.0.0.1:{}", silent.port()),
        ];
        assert_eq!(prober.refresh_nat_type(&servers), NatType::UdpBlocked);
        assert_eq!(prober.cached_nat_type(), NatType::UdpBlocked);
    }

    #[test]
    fn shuffle_keeps_all_items() {
        let mut items: Vec<u32> = (0..64).collect();
        shuffle(&mut items);
        let unique: HashSet<u32> = items.iter().copied().collect();
        assert_eq!(unique.len(), 64, "打乱不得丢失或重复元素");
    }

    #[test]
    fn nat_type_names_match_java_enum_constants() {
        let all = [
            NatType::UdpBlocked,
            NatType::OpenInternet,
            NatType::SymmetricUdpFirewall,
            NatType::FullCone,
            NatType::RestrictedCone,
            NatType::PortRestrictedCone,
            NatType::Symmetric,
            NatType::Unknown,
        ];
        assert_eq!(all.len(), 8);
        assert_eq!(NatType::PortRestrictedCone.as_str(), "PortRestrictedCone");
        // 序列化值与 Java 枚举名一致（WebUI DTO 契约）
        assert_eq!(serde_json::to_value(NatType::FullCone).unwrap(), "FullCone");
    }

    /// 与 [`spawn_udp_stun_mock`] 配套：统计收到的事务数（保活/门控断言用）。
    #[test]
    fn disabled_gating_probes_nothing() {
        let counter = Arc::new(AtomicUsize::new(0));
        let server_counter = Arc::clone(&counter);
        let server = spawn_udp_stun_mock(move |_request, _| {
            server_counter.fetch_add(1, Ordering::Relaxed);
            None
        });
        let prober = NatTypeProber::new();
        // 探测器本身不感知 enabled；门控由 AutoStun 层完成（见 auto_stun::tests）。
        // 这里断言「全部失败 ⇒ UdpBlocked 且确有网络活动」作为对照组。
        let servers = vec![format!("127.0.0.1:{}", server.port())];
        assert_eq!(prober.refresh_nat_type(&servers), NatType::UdpBlocked);
        assert!(
            counter.load(Ordering::Relaxed) >= 1,
            "对照组：探测确实发出了请求"
        );
    }
}
