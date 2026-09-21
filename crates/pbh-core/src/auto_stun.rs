//! AutoSTUN 内置 NAT 地址翻译（对齐上游 `util/traversal/btstun/*`、
//! `util/traversal/stun/TcpStunClient` 与 `AbstractDownloader.convertIfManagedByBuiltInNat`）。
//!
//! ## 上游链路（v9.5.1）
//! 1. `BTStunManager.load()`：`auto-stun.enabled=false` 时**不注册任何实例**，
//!    Spring 单例 `NatAddressProviderRegistry` 为空 ⇒ 翻译恒为直通；
//! 2. `BTStunInstance.restart()` 每 5 秒检查隧道，`StunTcpTunnelImpl.createMapping()` 用
//!    `TcpStunClient.getMapping()`（TCP Binding Request）拿到 `MappingResult(interAddress, outerAddress)`，
//!    即「本机端点 → NAT 公网端点」；
//! 3. `TCPForwarderImpl` 为每条下游连接记录 `connectionMap[下游(peer) 地址] = 上游(本机)套接字地址`，
//!    `Forwarder.translate(natted)` = `connectionMap.inverse().get(natted)`（BiMap 反查）；
//! 4. `AbstractDownloader.addressTranslate`：内置 NAT → Teredo → NAT64 → IPv4-mapped 归一，
//!    每一步都读取**已改写**的地址（就地 `peerAddress.applyNat/setIp`）。
//!
//! ## 本移植的映射模型
//! 上游 `connectionMap` 是「每连接」双向表，脱离隧道/转发器无法静态表达；其可观察语义为
//! 「下载器看到的本地/内网地址 → 隧道另一侧的 NAT 地址」。因此本模块用等价的静态表
//! [`NatMapping`]（`local: CIDR → public: IP`）表达同一关系：
//! - 表项登记（`insert_mapping`）对应上游把隧道端点写入连接表；
//! - 查表语义对齐 `NatAddressProviderRegistry.translate`：按登记顺序取**第一个**命中项，
//!   未命中返回 `None` ⇒ 调用方直通；
//! - 端口保持不变（上游 `convertIfManagedByBuiltInNat` 改写的是 `applyNat(host, port)` 中的 `host`；
//!   端口来自连接表，友好回环映射场景下即下游 peer 的原始端口）。
//!
//! 上游「友好回环映射」（`auto-stun.use-friendly-loopback-mapping`：把下游 IP 的
//! `127.x.y.z` 写法作为回源地址）依赖连接表精确匹配（地址 **与** 端口），无法用静态前缀表达，
//! 因此本模块只记录该配置项，不据此猜测性改写地址（避免误改真实 `127.0.0.0/8` peer）。
//!
//! ## 次要能力（本文件仅做门控接线，实现见兄弟模块）
//! - UDP NAT 类型探测（`StunManager` + cdnbye `NettyStunClient`，仅用于 WebUI/遥测展示
//!   `nat_type`，不参与 ban 判定）：[`crate::auto_stun_probe`]，入口 [`AutoStun::refresh_nat_type`]
//!   / [`AutoStun::spawn_nat_type_prober`]，结果经 [`AutoStun::nat_type`] 暴露（Rust 侧暂无
//!   Web API，另有 tracing 日志）；
//! - TCP 转发器 + 端口保活 + 友好回环绑定（`TCPForwarderImpl` / `StunTcpTunnelImpl` /
//!   `StunSocketTool` / `BTStunInstance.onCreate`）：[`crate::auto_stun_forwarder`]，
//!   入口 [`AutoStun::create_tunnel`]。
//!
//! ## 与上游的差异（严格 no-op 约束）
//! 上游 `StunManager` 的 UDP 探测**无条件**每小时运行（与 `auto-stun.enabled` 无关）；
//! 本移植按仓库「`enabled=false` ⇒ 无 socket / 无探测 / 无线程」的硬性约束，把探测与
//! 隧道全部挂在 [`AutoStun`] 上、仅在 `enabled=true` 时激活（所有入口都会先检查 enabled）。
//!
//! 与上游的一处早期实现差异已消除：现在 `getMappingTcp` 同样先绑定源地址再连接 STUN 服务器
//! （`StunSocketTool.getSocket()` + `bind`，经 `socket2` 实现），本机端口即隧道端口
//! （`inter` 取 `local_addr()`，与上游 `getLocalSocketAddress()` 完全一致）。

use crate::auto_stun_forwarder::{StunTcpTunnel, TunnelConfig};
use crate::auto_stun_probe::{NatType, NatTypeProber, NAT_TYPE_REFRESH_INTERVAL};
use crate::iputil::{parse_addr, parse_net};
use ipnet::{IpNet, Ipv4Net};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// STUN 服务端缺省端口（`TcpStunClient.getMappingFromServer`：`parts.length > 1 ? ... : 3478`）。
pub const DEFAULT_STUN_PORT: u16 = 3478;
/// 上游 `StunTcpTunnelImpl.testMapping` 的连接超时（5000ms）；上游 STUN 读写未设超时，
/// 本实现统一套用该上限，避免网络异常时永久阻塞后台线程。
pub const DEFAULT_STUN_TIMEOUT: Duration = Duration::from_secs(5);
/// 上游 `BTStunInstance` 的隧道定时任务周期（`scheduleWithFixedDelay(..., 0, 5, SECONDS)`）。
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(5);
/// 上游 `TcpStunClient` 单次读取的缓冲大小（`new byte[1500]`）。
const STUN_BUFFER_SIZE: usize = 1500;
/// 后台线程可中断休眠的步长（使 `close()` 能及时返回）。
const REFRESH_TICK: Duration = Duration::from_millis(100);

/// `config.yml` 的 `auto-stun:` 段（上层配置节；默认 `enabled: false`）。
///
/// STUN 服务器列表在上游位于**顶层** `stun:` 段（`stun.tcp-servers` /
/// `stun.udp-servers` / `stun.available-test`），本移植把前两者并进本结构体
/// （应用层配置树中的路径为 `ip-remapping.auto-stun`，见 `remap.rs`）：
/// - `tcp-servers` 是唯一被移植路径消费的列表（`StunTcpTunnelImpl` → [`AutoStun::refresh_from_servers`]）；
/// - `udp-servers` 只被上游 `StunManager` 的 UDP NAT 类型探测使用（仅 WebUI/遥测展示 `nat_type`，
///   不参与地址翻译，未移植），此处保留同名键以便配置与上游一一对应；
/// - `stun.available-test`（默认 true，`StunTcpTunnelImpl.testMapping` 的可用性预检）未移植：
///   本实现的 [`TcpStunClient::get_mapping`] 直接按顺序轮换服务器，失败即换下一个。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoStunConfig {
    /// `auto-stun.enabled`：默认关闭 ⇒ 内置 NAT 翻译严格直通
    #[serde(default)]
    pub enabled: bool,
    /// `auto-stun.use-friendly-loopback-mapping`：默认开启（仅记录，见模块文档）
    #[serde(rename = "use-friendly-loopback-mapping", default = "default_true")]
    pub use_friendly_loopback_mapping: bool,
    /// `auto-stun.downloaders`：启用隧道的下载器列表（默认空）
    #[serde(default)]
    pub downloaders: Vec<String>,
    /// `stun.tcp-servers`：TCP STUN 服务器列表（缺省值即上游 `config.yml` 随包值）
    #[serde(rename = "tcp-servers", default = "default_tcp_stun_servers")]
    pub tcp_servers: Vec<String>,
    /// `stun.udp-servers`：UDP STUN 服务器列表（上游仅用于未移植的 NAT 类型探测）
    #[serde(rename = "udp-servers", default = "default_udp_stun_servers")]
    pub udp_servers: Vec<String>,
}

impl Default for AutoStunConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            use_friendly_loopback_mapping: true,
            downloaders: Vec::new(),
            tcp_servers: default_tcp_stun_servers(),
            udp_servers: default_udp_stun_servers(),
        }
    }
}

/// 上游 `config.yml` 的 `stun.tcp-servers`
fn default_tcp_stun_servers() -> Vec<String> {
    vec![
        "turn.cloudflare.com:3478".to_string(),
        "stun.nextcloud.com:3478".to_string(),
        "stun.sipnet.com:3478".to_string(),
    ]
}

/// 上游 `config.yml` 的 `stun.udp-servers`
fn default_udp_stun_servers() -> Vec<String> {
    vec![
        "stun.cdnbye.com:3478".to_string(),
        "stun.nextcloud.com:3478".to_string(),
        "stun.miwifi.com:3478".to_string(),
        "stun.syncthing.net:3478".to_string(),
        "stun.l.google.com:3478".to_string(),
    ]
}

fn default_true() -> bool {
    true
}

impl AutoStunConfig {
    /// 构建运行时映射表（对齐 `BTStunManager.register()` 注册到 `NatAddressProviderRegistry`）。
    ///
    /// `enabled=false` 时注册表为空，`translate` 恒返回 `None`。
    pub fn build(&self) -> Arc<AutoStun> {
        Arc::new(AutoStun::new(self))
    }
}

/// 一条内置 NAT 映射：本机（LAN 侧）网段 → NAT 公网地址。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NatMapping {
    /// 下载器看到的本地地址网段（上游连接表的键）
    pub local: IpNet,
    /// 隧道另一侧的公网地址（上游连接表的值）
    pub public: IpAddr,
}

/// STUN 隧道发现的内/外端点，对齐上游 `MappingResult(interAddress, outerAddress)`
/// 与 `StunListener.onCreate(inter, outer)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StunMapping {
    /// 本机端点（上游 `socket.getLocalSocketAddress()`）
    pub inter: SocketAddr,
    /// NAT 公网端点（响应中的 `XOR-MAPPED-ADDRESS`）
    pub outer: SocketAddr,
}

/// AutoSTUN 运行时映射表（对齐上游 Spring 单例 `NatAddressProviderRegistry` + 各实例的连接表）。
pub struct AutoStun {
    enabled: bool,
    use_friendly_loopback_mapping: bool,
    downloaders: Vec<String>,
    udp_servers: Vec<String>,
    nat_type_prober: NatTypeProber,
    mappings: RwLock<Vec<NatMapping>>,
    public_endpoint: RwLock<Option<SocketAddr>>,
}

impl std::fmt::Debug for AutoStun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AutoStun")
            .field("enabled", &self.enabled)
            .field("use_friendly_loopback_mapping", &self.use_friendly_loopback_mapping)
            .field("downloaders", &self.downloaders)
            .field("udp_servers", &self.udp_servers)
            .field("nat_type", &self.nat_type())
            .field("mappings", &self.mappings())
            .field("public_endpoint", &self.public_endpoint())
            .finish()
    }
}

impl AutoStun {
    pub fn new(config: &AutoStunConfig) -> Self {
        Self {
            enabled: config.enabled,
            use_friendly_loopback_mapping: config.use_friendly_loopback_mapping,
            downloaders: config.downloaders.clone(),
            udp_servers: config.udp_servers.clone(),
            nat_type_prober: NatTypeProber::new(),
            mappings: RwLock::new(Vec::new()),
            public_endpoint: RwLock::new(None),
        }
    }

    /// `auto-stun.enabled`：关闭时翻译严格直通（对齐上游「未注册任何 provider」）。
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// `auto-stun.use-friendly-loopback-mapping`（见模块文档：不参与静态前缀改写）。
    pub fn use_friendly_loopback_mapping(&self) -> bool {
        self.use_friendly_loopback_mapping
    }

    /// `auto-stun.downloaders`（隧道启用的下载器列表，由应用层使用）。
    pub fn downloaders(&self) -> Vec<String> {
        self.downloaders.clone()
    }

    /// 当前映射表快照。
    pub fn mappings(&self) -> Vec<NatMapping> {
        self.mappings.read().map(|m| m.clone()).unwrap_or_default()
    }

    /// 清空映射表（对齐 `BTStunManager.close()` 清空实例并移除 provider）。
    pub fn clear_mappings(&self) {
        if let Ok(mut mappings) = self.mappings.write() {
            mappings.clear();
        }
    }

    /// 最近一次 STUN 发现到的 NAT 公网端点（`outerAddress`）。
    pub fn public_endpoint(&self) -> Option<SocketAddr> {
        self.public_endpoint.read().ok().and_then(|endpoint| *endpoint)
    }

    /// 注入一条映射：`local` 为网段或单个 IP（按 /32、/128 处理），`public` 为 NAT 地址。
    ///
    /// 语义对齐上游 `BiMap.put`：同一 `local` 键重复登记覆盖旧值；指向同一 `public` 的旧键被移除
    /// （连接表是双向表，一个本机套接字只对应一个对端地址）。任一侧无法解析时返回 `false` 且不改动表。
    pub fn insert_mapping(&self, local: &str, public: &str) -> bool {
        let Some(net) = parse_net(local) else {
            return false;
        };
        let Some(public) = parse_addr(public) else {
            return false;
        };
        self.insert_mapping_addr(net, public);
        true
    }

    /// [`AutoStun::insert_mapping`] 的类型化版本。
    pub fn insert_mapping_addr(&self, local: IpNet, public: IpAddr) {
        let Ok(mut mappings) = self.mappings.write() else {
            return;
        };
        mappings.retain(|mapping| mapping.local != local && mapping.public != public);
        mappings.push(NatMapping { local, public });
    }

    /// 翻译下载器报告的 peer 地址，对齐 `NatAddressProviderRegistry.translate`。
    ///
    /// - 未启用 ⇒ `None`（直通）：上游 `enabled=false` 时注册表为空；
    /// - 命中映射 ⇒ `Some((公网地址, 原端口))`；
    /// - 未命中（含尚未解析出映射 / STUN 不可达）⇒ `None`（直通），不阻塞、不报错；
    /// - IPv6：`TcpStunClient.parseStunResponse` 只解析 `family == 1`（IPv4）的映射地址，
    ///   `TCPForwarderImpl.connectToUpstreamFriendly` 的友好回环绑定也只对 `Inet4Address` 生效，
    ///   故 STUN 发现路径登记的全部是 IPv4 网段，IPv6 peer 地址不会命中映射，原样直通。
    pub fn translate(&self, addr: IpAddr, port: u16) -> Option<(IpAddr, u16)> {
        if !self.enabled {
            return None;
        }
        let mappings = self.mappings.read().ok()?;
        mappings
            .iter()
            .find(|mapping| mapping.local.contains(&addr))
            .map(|mapping| (mapping.public, port))
    }

    /// 执行一次 STUN 隧道刷新，对齐 `BTStunInstance.restart()` + `StunListener.onCreate(inter, outer)`：
    /// 发现 `inter → outer` 后记录公网端点，并把本机端点地址登记为 `local → outer.ip()` 映射。
    ///
    /// 上游在此处还把 `outer.getPort()` 写回下载器 BT 端口、`inter.getPort()` 作为转发器监听端口；
    /// 端口改写属于下载器/转发器层，核心层只登记地址映射。
    /// 未启用或 STUN 不可达 ⇒ `None`（保留既有映射，翻译直通）。
    pub fn refresh_from_servers(
        &self,
        servers: &[String],
        source_host: &str,
        source_port: u16,
        timeout: Duration,
    ) -> Option<StunMapping> {
        if !self.enabled {
            return None;
        }
        let client = TcpStunClient::new(servers.to_vec(), source_host, source_port)?;
        let mapping = client.get_mapping(timeout)?;
        if let Ok(mut endpoint) = self.public_endpoint.write() {
            *endpoint = Some(mapping.outer);
        }
        // 上游 parseStunResponse 仅支持 IPv4（family == 1），故此处只登记 IPv4 本机地址；
        // 需要覆盖整个 LAN 网段时用 insert_mapping 显式登记（例如 "192.168.0.0/16"）。
        if let IpAddr::V4(inter_v4) = mapping.inter.ip() {
            if let Ok(net) = Ipv4Net::new(inter_v4, 32) {
                self.insert_mapping_addr(IpNet::V4(net), mapping.outer.ip());
            }
        }
        // 上游 Lang.BTSTUN_ON_TUNNEL_CREATED
        tracing::debug!(
            "[AutoSTUN] STUN 隧道映射已更新: 本机端点 {} -> 公网端点 {}",
            mapping.inter,
            mapping.outer
        );
        Some(mapping)
    }

    /// 启动后台刷新线程：**立即**执行一次，随后按 `interval` 重试
    /// （对齐上游 `sched.scheduleWithFixedDelay(this::restart, 0, 5, TimeUnit.SECONDS)`）。
    ///
    /// 线程创建失败时返回的句柄不持有线程（调用方无需处理错误，翻译路径不受影响）。
    /// 所有网络等待都发生在本线程内，`translate` 调用方永不被阻塞。
    pub fn spawn_refresher(
        self: Arc<Self>,
        servers: Vec<String>,
        source_host: String,
        source_port: u16,
        interval: Duration,
        timeout: Duration,
    ) -> AutoStunRefresher {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let table = Arc::clone(&self);
        let handle = std::thread::Builder::new()
            .name("AutoStun-Refresh".to_string())
            .spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    if table
                        .refresh_from_servers(&servers, &source_host, source_port, timeout)
                        .is_none()
                    {
                        tracing::debug!("STUN 隧道刷新失败，保留既有映射（无映射时翻译直通）");
                    }
                    let mut waited = Duration::ZERO;
                    while waited < interval && !flag.load(Ordering::SeqCst) {
                        let step = REFRESH_TICK.min(interval - waited);
                        std::thread::sleep(step);
                        waited += step;
                    }
                }
            })
            .ok();
        AutoStunRefresher { stop, handle }
    }

    /// 最近一次缓存的 NAT 类型（`StunManager.getCachedNatType()`；初始 `Unknown`）。
    ///
    /// 上游该结果仅用于 WebUI/遥测展示（`/api/autostun/status` 的 `natType`），从不参与
    /// ban 判定；Rust 侧暂无 Web API，此处作为方法 + 日志暴露。
    pub fn nat_type(&self) -> NatType {
        self.nat_type_prober.cached_nat_type()
    }

    /// `StunManager.refreshNatType()` 的门控包装：用 `stun.udp-servers` 做 UDP NAT 类型探测。
    ///
    /// - `enabled=false` ⇒ 严格 no-op（不发任何 UDP 包），返回 `NatType::Unknown`；
    /// - `enabled=true` ⇒ 与上游一致：打乱服务器逐台尝试，全部失败 ⇒ 缓存 `UdpBlocked`。
    ///
    /// 与上游的差异：上游 `StunManager` 无条件每小时探测（与 enabled 无关）；本移植按
    /// 「disabled ⇒ 严格 no-op」约束改为仅在启用时运行（见模块文档）。
    pub fn refresh_nat_type(&self) -> NatType {
        if !self.enabled {
            tracing::debug!("[AutoSTUN] 未启用，跳过 NAT 类型探测（严格 no-op）");
            return NatType::Unknown;
        }
        self.nat_type_prober.refresh_nat_type(&self.udp_servers)
    }

    /// 上游 `StunManager` 构造时的 `scheduleWithFixedDelay(this::refreshNatType, 0, 1, HOURS)`。
    /// 未启用 ⇒ 返回不持有线程的句柄（严格 no-op）。
    pub fn spawn_nat_type_prober(self: &Arc<Self>) -> AutoStunRefresher {
        self.spawn_nat_type_prober_with(NAT_TYPE_REFRESH_INTERVAL)
    }

    /// [`AutoStun::spawn_nat_type_prober`] 的可注入周期版本（测试用短周期）。
    pub fn spawn_nat_type_prober_with(self: &Arc<Self>, interval: Duration) -> AutoStunRefresher {
        if !self.enabled {
            tracing::debug!("[AutoSTUN] 未启用，不启动 NAT 类型探测线程（严格 no-op）");
            return AutoStunRefresher::completed();
        }
        let this = Arc::clone(self);
        AutoStunRefresher::spawn_named("StunManager-RefreshNatType", move |stop| {
            while !stop.load(Ordering::SeqCst) {
                this.refresh_nat_type();
                let mut waited = Duration::ZERO;
                while waited < interval && !stop.load(Ordering::SeqCst) {
                    let step = REFRESH_TICK.min(interval - waited);
                    std::thread::sleep(step);
                    waited += step;
                }
            }
        })
    }

    /// `StunTcpTunnelImpl.createMapping` + `BTStunInstance.onCreate` 的门控包装
    /// （TCP 转发器 + 端口保活 + 友好回环绑定，实现见 [`crate::auto_stun_forwarder`]）。
    ///
    /// - `enabled=false` ⇒ 严格 no-op（无 socket / 无线程），返回 `None`；
    /// - `enabled=true` 且隧道不适用（映射自测失败 / 下载器主机为公网）⇒ `None`；
    /// - STUN/网络错误 ⇒ 记录告警并返回 `None`（上游由 5s 定时任务捕获并在下一周期重建）。
    pub fn create_tunnel(
        &self,
        stun_servers: &[String],
        downloader_host: &str,
        config: &TunnelConfig,
    ) -> Option<StunTcpTunnel> {
        if !self.enabled {
            tracing::debug!("[AutoSTUN] 未启用，跳过隧道创建（严格 no-op）");
            return None;
        }
        match crate::auto_stun_forwarder::create_tunnel(stun_servers, downloader_host, config) {
            Ok(tunnel) => tunnel,
            Err(e) => {
                // 上游 Lang.BTSTUN_RESTART_FAILED：由 5s 定时任务捕获，下一周期重建
                tracing::warn!("[AutoSTUN] 隧道创建失败（下一刷新周期将重试）: {e}");
                None
            }
        }
    }
}

/// 后台刷新句柄，对齐 `BTStunInstance` 的定时任务与 `close()`。
pub struct AutoStunRefresher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl AutoStunRefresher {
    /// 停止后台刷新并等待线程退出（对齐 `BTStunInstance.close()`：置 `shutdown` 后 `awaitTermination`）。
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    /// 以 `name` 启动一个可通过 [`AutoStunRefresher::close`] 停止的后台线程
    /// （线程体周期性检查 `stop` 标志）。
    pub(crate) fn spawn_named(name: &str, body: impl Fn(&AtomicBool) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name(name.to_string())
            .spawn(move || body(&flag))
            .ok();
        Self { stop, handle }
    }

    /// 不持有线程的已完成句柄（`enabled=false` ⇒ 严格 no-op，不创建任何线程）。
    pub(crate) fn completed() -> Self {
        Self { stop: Arc::new(AtomicBool::new(true)), handle: None }
    }
}

impl Drop for AutoStunRefresher {
    fn drop(&mut self) {
        self.close();
    }
}

/// TCP STUN 客户端，对齐上游 `TcpStunClient`：向 STUN 服务器发送 RFC 5389 Binding Request，
/// 从响应中取出 `MAPPED-ADDRESS` / `XOR-MAPPED-ADDRESS`（仅 IPv4），
/// `inter` 取本机套接字地址（上游为绑定端口后的 `getLocalSocketAddress()`，见模块文档的差异说明）。
pub struct TcpStunClient {
    servers: Vec<String>,
    source_host: String,
    source_port: u16,
}

impl TcpStunClient {
    /// 空服务器列表 ⇒ `None`（上游抛 `IllegalArgumentException("STUN server list cannot be empty")`）。
    pub fn new(servers: Vec<String>, source_host: impl Into<String>, source_port: u16) -> Option<Self> {
        if servers.is_empty() {
            return None;
        }
        Some(Self { servers, source_host: source_host.into(), source_port })
    }

    /// 按顺序轮换服务器直到成功；全部失败 ⇒ `None`。
    ///
    /// 上游 `getMapping()` 在全部服务器失败后休眠 10 秒并**无限重试**（隧道专用线程）；
    /// 本实现返回 `None`，由调用方（[`AutoStunRefresher`]）在下一个周期重试，
    /// 从而不长期占用调用方线程。
    pub fn get_mapping(&self, timeout: Duration) -> Option<StunMapping> {
        for server in &self.servers {
            match self.get_mapping_from_server(server, timeout) {
                Ok(mapping) => return Some(mapping),
                Err(e) => tracing::debug!("STUN 服务器 {server} 不可用，尝试下一个: {e}"),
            }
        }
        // 上游 Lang.STUN_CLIENT_NO_AVAILABLE_SERVER
        tracing::warn!("无法连接到任何 STUN 服务器：{:?}", self.servers);
        None
    }

    fn get_mapping_from_server(&self, server: &str, timeout: Duration) -> io::Result<StunMapping> {
        let Some((host, port)) = parse_stun_server(server) else {
            return Err(invalid_data(format!("无效的 STUN 服务器地址: {server}")));
        };
        self.get_mapping_tcp(&host, port, timeout)
    }

    fn get_mapping_tcp(&self, host: &str, port: u16, timeout: Duration) -> io::Result<StunMapping> {
        let source = resolve_socket_addr(&self.source_host, self.source_port)?;
        let targets: Vec<SocketAddr> = (host, port).to_socket_addrs()?.collect();
        if targets.is_empty() {
            return Err(invalid_data(format!("无法解析 STUN 服务器 {host}:{port}")));
        }
        let mut last_error = None;
        for target in targets {
            if target.is_ipv4() != source.is_ipv4() {
                // 绑定地址族与目标地址族不一致（上游由 Socket.connect 内部选择，失败即轮换）
                continue;
            }
            match exchange_stun(source, target, timeout) {
                Ok(mapping) => return Ok(mapping),
                Err(e) => last_error = Some(e),
            }
        }
        Err(last_error.unwrap_or_else(|| invalid_data(format!("STUN 服务器不可用: {host}:{port}"))))
    }
}

/// 服务器字符串 `host[:port]` → `(host, port)`，缺省端口 [`DEFAULT_STUN_PORT`]
/// （对齐 `TcpStunClient.getMappingFromServer`；与上游一致不支持 IP 字面量外的 IPv6 写法）。
pub fn parse_stun_server(server: &str) -> Option<(String, u16)> {
    let mut parts = server.trim().split(':');
    let host = parts.next()?.trim();
    if host.is_empty() {
        return None;
    }
    match parts.next() {
        Some(raw) => raw.trim().parse::<u16>().ok().map(|port| (host.to_string(), port)),
        None => Some((host.to_string(), DEFAULT_STUN_PORT)),
    }
}

/// 构造 STUN Binding Request（RFC 5389），对齐 `TcpStunClient.createStunBindingRequest`：
/// `0x0001` + 长度 0 + magic cookie `0x2112A442` + 事务 ID（固定前缀 `"NATR"` + 两个随机数）。
pub fn create_stun_binding_request() -> [u8; 20] {
    let mut request = [0u8; 20];
    request[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
    request[2..4].copy_from_slice(&0u16.to_be_bytes());
    request[4..8].copy_from_slice(&0x2112_A442u32.to_be_bytes());
    request[8..12].copy_from_slice(&0x4e41_5452u32.to_be_bytes());
    request[12..16].copy_from_slice(&random_u32().to_be_bytes());
    request[16..20].copy_from_slice(&random_u32().to_be_bytes());
    request
}

/// 解析 STUN Binding Response，对齐 `TcpStunClient.parseStunResponse`：
/// 校验类型 `0x0101` 与 magic cookie（不校验事务 ID），扫描属性取
/// `MAPPED-ADDRESS`(0x0001) 或 `XOR-MAPPED-ADDRESS`(0x0020)，仅接受 `family == 1`（IPv4）。
///
/// 与上游的差异仅在失败形态：上游对越界属性长度抛 `IllegalArgumentException`，
/// 本实现统一返回 `io::Error`（两者都会让 `get_mapping()` 轮换到下一个服务器）。
pub fn parse_stun_response(buffer: &[u8]) -> io::Result<SocketAddr> {
    if buffer.len() < 20 {
        return Err(invalid_data("Invalid STUN response length"));
    }
    let message_type = u16::from_be_bytes([buffer[0], buffer[1]]);
    if message_type != 0x0101 {
        return Err(invalid_data(format!("Invalid STUN response type: {message_type:x}")));
    }
    let magic_cookie = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
    if magic_cookie != 0x2112_A442 {
        return Err(invalid_data("Invalid magic cookie"));
    }

    let mut pos = 20usize; // 跳过事务 ID（上游 `buf.position(20)`）
    let mut found: Option<SocketAddr> = None;
    while buffer.len().saturating_sub(pos) >= 4 {
        let attr_type = u16::from_be_bytes([buffer[pos], buffer[pos + 1]]);
        let attr_len = u16::from_be_bytes([buffer[pos + 2], buffer[pos + 3]]) as usize;
        pos += 4;
        if attr_type == 0x0001 || attr_type == 0x0020 {
            if attr_len >= 8 {
                // [reserved(1)][family(1)][port(2)][address(4|16)]
                if buffer.len().saturating_sub(pos) < 2 {
                    break;
                }
                let family = buffer[pos + 1];
                pos += 2;
                if family == 1 {
                    if buffer.len().saturating_sub(pos) < 6 {
                        break;
                    }
                    let raw_port = u16::from_be_bytes([buffer[pos], buffer[pos + 1]]);
                    let raw_ip = u32::from_be_bytes([
                        buffer[pos + 2],
                        buffer[pos + 3],
                        buffer[pos + 4],
                        buffer[pos + 5],
                    ]);
                    let (port, ip) = if attr_type == 0x0020 {
                        (raw_port ^ 0x2112, raw_ip ^ 0x2112_A442)
                    } else {
                        (raw_port, raw_ip)
                    };
                    found = Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port));
                    break;
                }
                // family != 1（IPv6 映射）：与上游一致地继续扫描后续属性
            } else {
                pos = pos.saturating_add(attr_len);
            }
        } else {
            pos = pos.saturating_add(attr_len);
            let padding = (4 - (attr_len % 4)) % 4;
            pos = pos.saturating_add(padding);
        }
    }

    match found {
        // 上游：`ip == null || port == 0` → "No mapped address found in STUN response"
        Some(addr) if addr.port() != 0 => Ok(addr),
        _ => Err(invalid_data("No mapped address found in STUN response")),
    }
}

/// 连接 STUN 服务器并发起一次交换（对齐 `getMappingTcp`：
/// `StunSocketTool.getSocket()` → `bind(source)` → `connect(server)` → `getLocalSocketAddress()`
/// → `write/read`）。先绑定 `sourceHost:sourcePort` 以保持隧道端口的 NAT 映射
/// （经 `socket2` 实现「先绑定再连接」，与上游语义完全一致）。
fn exchange_stun(source: SocketAddr, target: SocketAddr, timeout: Duration) -> io::Result<StunMapping> {
    let mut stream = crate::auto_stun_forwarder::bind_connect(source, target, timeout)?;
    // 上游未设读写超时（可能永久阻塞）；本实现统一套用调用方超时，保证后台线程可退出
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let inter = stream.local_addr()?;
    stream.write_all(&create_stun_binding_request())?;
    stream.flush()?;
    let outer = read_stun_response(&mut stream)?;
    tracing::debug!("STUN TCP: outer={outer}, inter={inter}, 期望本机端点={source}, server={target}");
    Ok(StunMapping { inter, outer })
}

/// 读取一个 STUN 响应：先取满 20 字节头，再按报文声明长度补齐属性区（上限为上游的 1500 字节缓冲）。
fn read_stun_response(stream: &mut TcpStream) -> io::Result<SocketAddr> {
    let mut buffer = [0u8; STUN_BUFFER_SIZE];
    let mut filled = 0usize;
    while filled < 20 {
        let read = stream.read(&mut buffer[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    if filled < 20 {
        return Err(invalid_data("Invalid STUN response length"));
    }
    let declared = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;
    let total = (20 + declared).min(STUN_BUFFER_SIZE);
    while filled < total {
        let read = stream.read(&mut buffer[filled..total])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    parse_stun_response(&buffer[..filled])
}

fn resolve_socket_addr(host: &str, port: u16) -> io::Result<SocketAddr> {
    (host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| invalid_data(format!("无法解析本机绑定地址 {host}:{port}")))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

/// 非密码学随机数（事务 ID 只需唯一性，对齐上游 `ThreadLocalRandom.nextInt()`）。
/// 使用标准库 `RandomState` 的随机哈希种子 + 单调计数器 + 时钟，避免引入额外依赖。
pub(crate) fn random_u32() -> u32 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u64(nanos);
    hasher.write_u64(counter);
    hasher.finish() as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn config(enabled: bool) -> AutoStunConfig {
        AutoStunConfig { enabled, ..AutoStunConfig::default() }
    }

    fn public_ip() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(203, 0, 113, 9))
    }

    #[test]
    fn default_config_matches_upstream_yaml() {
        let cfg = AutoStunConfig::default();
        assert!(!cfg.enabled, "auto-stun.enabled 默认 false");
        assert!(cfg.use_friendly_loopback_mapping, "use-friendly-loopback-mapping 默认 true");
        assert!(cfg.downloaders.is_empty());
        assert!(!cfg.build().is_enabled());
        // `stun.tcp-servers` / `stun.udp-servers` 的缺省值即上游随包 config.yml 的值
        assert_eq!(
            cfg.tcp_servers,
            vec![
                "turn.cloudflare.com:3478",
                "stun.nextcloud.com:3478",
                "stun.sipnet.com:3478"
            ]
        );
        assert_eq!(cfg.udp_servers.len(), 5);
        assert!(cfg.udp_servers.iter().any(|s| s == "stun.l.google.com:3478"));
        // 服务器列表可被 YAML 覆盖（键名与上游 `stun:` 段一致）
        let custom: AutoStunConfig =
            serde_yaml::from_str("enabled: true\ntcp-servers:\n  - \"1.2.3.4:3478\"\n").unwrap();
        assert_eq!(custom.tcp_servers, vec!["1.2.3.4:3478"]);
        assert_eq!(custom.udp_servers.len(), 5, "未写的键回落上游默认值");
    }

    #[test]
    fn disabled_config_is_passthrough_even_with_mappings() {
        let table = config(false).build();
        assert!(table.insert_mapping("192.168.0.0/16", "203.0.113.9"));
        let private = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
        assert_eq!(table.translate(private, 6881), None);
        assert!(!table.mappings().is_empty(), "表项仍被记录，只是不参与翻译");
    }

    #[test]
    fn enabled_and_mapped_private_ip_is_rewritten_with_same_port() {
        let table = config(true).build();
        assert!(table.insert_mapping("192.168.0.0/16", "203.0.113.9"));
        assert_eq!(
            table.translate(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 6881),
            Some((public_ip(), 6881))
        );
        assert_eq!(
            table.translate(IpAddr::V4(Ipv4Addr::new(192, 168, 255, 254)), 65535),
            Some((public_ip(), 65535))
        );
    }

    #[test]
    fn enabled_but_unmapped_private_ip_is_passthrough() {
        let table = config(true).build();
        assert!(table.insert_mapping("192.168.1.0/24", "203.0.113.9"));
        assert_eq!(table.translate(IpAddr::V4(Ipv4Addr::new(192, 168, 2, 5)), 6881), None);
        // 尚未解析出映射（STUN 不可达 / 未启动）时同样直通
        let empty = config(true).build();
        assert_eq!(empty.translate(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 6881), None);
        assert!(empty.mappings().is_empty());
        assert_eq!(empty.public_endpoint(), None);
    }

    #[test]
    fn public_ip_is_passthrough() {
        let table = config(true).build();
        assert!(table.insert_mapping("192.168.0.0/16", "203.0.113.9"));
        assert!(table.insert_mapping("10.0.0.0/8", "203.0.113.10"));
        assert_eq!(table.translate(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)), 6881), None);
        assert_eq!(table.translate(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), 1), None);
    }

    #[test]
    fn ipv6_peer_address_is_never_managed_by_built_in_nat() {
        let table = config(true).build();
        // STUN 发现路径只登记 IPv4：TcpStunClient 仅解析 family == 1
        assert!(table.insert_mapping("192.168.0.0/16", "203.0.113.9"));
        let v6 = IpAddr::V6("2001:db8::1".parse().unwrap());
        assert_eq!(table.translate(v6, 6881), None);
        // IPv4-mapped IPv6 归一后按 IPv4 处理（上游 InetSocketAddress 同样归一）
        let mapped = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
        assert_eq!(table.translate(mapped, 6881), Some((public_ip(), 6881)));
    }

    #[test]
    fn multiple_public_addresses_resolve_per_registered_prefix() {
        let table = config(true).build();
        assert!(table.insert_mapping("192.168.1.0/24", "198.51.100.7"));
        assert!(table.insert_mapping("192.168.2.0/24", "203.0.113.9"));
        assert_eq!(
            table.translate(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 6881),
            Some((IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)), 6881))
        );
        assert_eq!(
            table.translate(IpAddr::V4(Ipv4Addr::new(192, 168, 2, 5)), 6881),
            Some((public_ip(), 6881))
        );
        assert_eq!(table.mappings().len(), 2);
    }

    #[test]
    fn reinserting_a_mapping_replaces_it_like_a_bimap() {
        let table = config(true).build();
        assert!(table.insert_mapping("192.168.1.0/24", "198.51.100.7"));
        assert!(table.insert_mapping("192.168.1.0/24", "203.0.113.9"));
        assert_eq!(
            table.translate(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 6881),
            Some((public_ip(), 6881))
        );
        // 同一公网地址换绑到另一个本地网段时，旧键被移除
        assert!(table.insert_mapping("10.0.0.0/8", "203.0.113.9"));
        assert_eq!(table.translate(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 6881), None);
        assert_eq!(
            table.translate(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 6881),
            Some((public_ip(), 6881))
        );
        assert_eq!(table.mappings().len(), 1);
    }

    #[test]
    fn single_ip_mapping_is_treated_as_a_host_prefix() {
        let table = config(true).build();
        assert!(table.insert_mapping("127.0.0.1", "203.0.113.9"));
        assert_eq!(
            table.translate(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 6881),
            Some((public_ip(), 6881))
        );
        assert_eq!(table.translate(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 2)), 6881), None);
    }

    #[test]
    fn unparsable_mapping_is_rejected() {
        let table = config(true).build();
        assert!(!table.insert_mapping("not-a-network", "203.0.113.9"));
        assert!(!table.insert_mapping("192.168.1.0/24", "not-an-ip"));
        assert!(table.mappings().is_empty());
    }

    #[test]
    fn clear_mappings_removes_all_entries() {
        let table = config(true).build();
        assert!(table.insert_mapping("192.168.1.0/24", "203.0.113.9"));
        table.clear_mappings();
        assert!(table.mappings().is_empty());
        assert_eq!(table.translate(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5)), 6881), None);
    }

    #[test]
    fn binding_request_matches_upstream_layout() {
        let request = create_stun_binding_request();
        assert_eq!(request.len(), 20);
        assert_eq!(u16::from_be_bytes([request[0], request[1]]), 0x0001);
        assert_eq!(u16::from_be_bytes([request[2], request[3]]), 0);
        assert_eq!(u32::from_be_bytes([request[4], request[5], request[6], request[7]]), 0x2112_A442);
        assert_eq!(&request[8..12], b"NATR");
        assert_ne!(create_stun_binding_request(), request, "事务 ID 随机");
    }

    #[test]
    fn stun_server_string_parsing_defaults_to_3478() {
        assert_eq!(parse_stun_server("stun.nextcloud.com"), Some(("stun.nextcloud.com".into(), 3478)));
        assert_eq!(
            parse_stun_server("stun.nextcloud.com:3479"),
            Some(("stun.nextcloud.com".into(), 3479))
        );
        assert_eq!(parse_stun_server(""), None);
        assert_eq!(parse_stun_server("host:not-a-port"), None);
    }

    fn binding_response(attr_type: u16, ip: Ipv4Addr, port: u16, magic: u32, msg_type: u16) -> Vec<u8> {
        let mut out = Vec::with_capacity(32);
        out.extend_from_slice(&msg_type.to_be_bytes());
        out.extend_from_slice(&12u16.to_be_bytes());
        out.extend_from_slice(&magic.to_be_bytes());
        out.extend_from_slice(b"NATR");
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&0u32.to_be_bytes());
        out.extend_from_slice(&attr_type.to_be_bytes());
        out.extend_from_slice(&8u16.to_be_bytes());
        out.push(0);
        out.push(1); // family = IPv4
        let (raw_port, raw_ip) = if attr_type == 0x0020 {
            (port ^ 0x2112, u32::from(ip) ^ 0x2112_A442)
        } else {
            (port, u32::from(ip))
        };
        out.extend_from_slice(&raw_port.to_be_bytes());
        out.extend_from_slice(&raw_ip.to_be_bytes());
        out
    }

    #[test]
    fn parse_xor_mapped_address_response() {
        let raw = binding_response(0x0020, Ipv4Addr::new(203, 0, 113, 9), 50000, 0x2112_A442, 0x0101);
        let addr = parse_stun_response(&raw).unwrap();
        assert_eq!(addr, SocketAddr::from(([203, 0, 113, 9], 50000)));
    }

    #[test]
    fn parse_plain_mapped_address_response() {
        let raw = binding_response(0x0001, Ipv4Addr::new(198, 51, 100, 7), 3478, 0x2112_A442, 0x0101);
        assert_eq!(parse_stun_response(&raw).unwrap(), SocketAddr::from(([198, 51, 100, 7], 3478)));
    }

    #[test]
    fn parse_skips_unknown_attributes_with_padding() {
        let mut raw = binding_response(0x0020, Ipv4Addr::new(203, 0, 113, 9), 50000, 0x2112_A442, 0x0101);
        let header_len = 20;
        let attr = raw.split_off(header_len);
        raw.extend_from_slice(&0x8022u16.to_be_bytes()); // SOFTWARE（长度 3，需补齐 1 字节）
        raw.extend_from_slice(&3u16.to_be_bytes());
        raw.extend_from_slice(b"abc");
        raw.push(0);
        raw.extend_from_slice(&attr);
        let body_len = (raw.len() as u16) - 20;
        raw[2..4].copy_from_slice(&body_len.to_be_bytes());
        assert_eq!(
            parse_stun_response(&raw).unwrap(),
            SocketAddr::from(([203, 0, 113, 9], 50000))
        );
    }

    #[test]
    fn parse_rejects_invalid_responses() {
        assert!(parse_stun_response(&[]).is_err());
        assert!(parse_stun_response(&[0u8; 19]).is_err());
        // 非 Binding Response
        let wrong_type = binding_response(0x0020, Ipv4Addr::new(1, 2, 3, 4), 1, 0x2112_A442, 0x0111);
        assert!(parse_stun_response(&wrong_type).is_err());
        // magic cookie 错误
        let wrong_cookie = binding_response(0x0020, Ipv4Addr::new(1, 2, 3, 4), 1, 0x2112_A443, 0x0101);
        assert!(parse_stun_response(&wrong_cookie).is_err());
        // 无映射属性
        let mut no_addr = Vec::new();
        no_addr.extend_from_slice(&0x0101u16.to_be_bytes());
        no_addr.extend_from_slice(&0u16.to_be_bytes());
        no_addr.extend_from_slice(&0x2112_A442u32.to_be_bytes());
        no_addr.extend_from_slice(&[0u8; 12]);
        assert!(parse_stun_response(&no_addr).is_err());
    }

    /// 伪造一个只应答一次 Binding Request 的 STUN 服务器（仅回环）。
    fn fake_stun_server(outer: Ipv4Addr, outer_port: u16) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("回环监听必须可用");
        let addr = listener.local_addr().expect("local_addr");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut request = [0u8; 20];
                if stream.read_exact(&mut request).is_err() {
                    continue;
                }
                let mut response =
                    binding_response(0x0020, outer, outer_port, 0x2112_A442, 0x0101);
                response[8..12].copy_from_slice(&request[8..12]); // 回显事务 ID 前缀
                let _ = stream.write_all(&response);
            }
        });
        addr.to_string()
    }

    #[test]
    fn tcp_stun_client_discovers_outer_endpoint_over_loopback() {
        let server = fake_stun_server(Ipv4Addr::new(203, 0, 113, 9), 50000);
        let client = TcpStunClient::new(vec![server], "127.0.0.1", 0).expect("非空服务器列表");
        let mapping = client.get_mapping(DEFAULT_STUN_TIMEOUT).expect("回环 STUN 应答");
        assert_eq!(mapping.outer, SocketAddr::from(([203, 0, 113, 9], 50000)));
        assert_eq!(mapping.inter.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(mapping.inter.port(), 0);
        // 空服务器列表 ⇒ 不上报映射（上游抛 IllegalArgumentException）
        assert!(TcpStunClient::new(Vec::new(), "0.0.0.0", 0).is_none());
    }

    #[test]
    fn unreachable_stun_server_degrades_to_no_mapping() {
        let table = config(true).build();
        // 保留端口（RFC 5737 文档地址）不会应答：必须返回 None 且不 panic
        let mapping = table.refresh_from_servers(
            &["203.0.113.1:3478".to_string()],
            "127.0.0.1",
            0,
            Duration::from_millis(300),
        );
        assert_eq!(mapping, None);
        assert_eq!(table.public_endpoint(), None);
        assert!(table.mappings().is_empty());
        // 未启用时不做任何网络操作
        let off = config(false).build();
        assert_eq!(
            off.refresh_from_servers(&["127.0.0.1:1".to_string()], "127.0.0.1", 0, Duration::ZERO),
            None
        );
    }

    #[test]
    fn refresh_registers_discovered_mapping_and_refresher_updates_it() {
        let server = fake_stun_server(Ipv4Addr::new(198, 51, 100, 7), 51413);
        let table = config(true).build();
        let mapping = table
            .refresh_from_servers(std::slice::from_ref(&server), "127.0.0.1", 0, DEFAULT_STUN_TIMEOUT)
            .expect("回环 STUN 应答");
        assert_eq!(mapping.outer, SocketAddr::from(([198, 51, 100, 7], 51413)));
        assert_eq!(table.public_endpoint(), Some(mapping.outer));
        // 本机端点（连接表键）登记为 /32 映射，端口保持
        assert_eq!(
            table.translate(mapping.inter.ip(), 6881),
            Some((IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)), 6881))
        );

        // 后台刷新线程（立即执行一次）也会写入公网端点
        let shared = config(true).build();
        let mut refresher = shared.clone().spawn_refresher(
            vec![server],
            "127.0.0.1".to_string(),
            0,
            Duration::from_millis(100),
            DEFAULT_STUN_TIMEOUT,
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while shared.public_endpoint().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(shared.public_endpoint(), Some(SocketAddr::from(([198, 51, 100, 7], 51413))));
        // `close()`（以及随后的 `Drop`）必须能及时停止后台线程
        refresher.close();
    }
}
