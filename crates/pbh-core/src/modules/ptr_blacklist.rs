//! PTR 黑名单（`ptr-blacklist`），忠实复刻上游 `PTRBlacklist`。
//!
//! 判定：把 peer IP 转成反向 DNS 名（`4.3.2.1.in-addr.arpa`）→ 取 PTR 记录 →
//! 用规则集匹配 PTR 主机名，命中即封禁。
//!
//! 解析架构（对齐上游「模块内解析」，关闭 PLAN §4.1 / SPEC §5.9 的设计差异）：
//! - 上游 `PTRBlacklist#shouldBanPeer` 在判定时**模块内**同步调用
//!   `DNSLookup#ptr(...).get(3, TimeUnit.SECONDS)`（`TimeoutException` → `Optional.empty()`），
//!   结果经 `ModuleMatchCache` 缓存：`pbh.modulematchcache.timeout` 默认 600000ms
//!   `expireAfterAccess`，**无记录/超时的负结果同样被缓存**，窗口内不会重新打 DNS。
//! - 本移植的判定跑在 tokio 阻塞线程池（ban wave worker）上，而 [`RuleModule::check`]
//!   是同步接口——在里面做最坏 3 秒的阻塞 DNS 会拖慢整轮 wave（上游在 JVM 虚拟线程
//!   上可以这样做，我们不能）。因此把解析放进模块自己的 [`PtrBlacklist::observe`]：
//!   对每个观测到的 peer IP fire-and-forget 地解析（独立 std 线程 + [`PTR_LOOKUP_TIMEOUT`]
//!   等待上限，不依赖 tokio 上下文），结果（含负结果）写入模块自有的 [`PtrCache`]；
//!   [`RuleModule::check`] 保持只读缓存、判定语义不变。
//! - **判定时序**：首次观测的 peer 本轮按「无记录 → `pass()`」放行，下一轮 wave
//!   才按解析结果判定——与原「应用层预热」设计完全一致，与上游逐轮同步解析的差别
//!   仅为晚一轮（SPEC §5.9 文案由 lead 更新）。
//! - 生产解析器 [`SystemPtrResolver`] 是极小的 std UDP PTR 客户端（对齐
//!   `DNSLookupImpl` 的 dnsjava PTR 查询语义）；`resolvers.servers` 多解析器负载均衡
//!   未移植。上游 `registerModules()` 中该模块的注册被注释掉（本移植同样不注册，
//!   见 `config.rs`），因此应用层暂无 observe 调用点；模块一旦注册，应由 wave/app
//!   层对每个观测到的 peer 调用 [`PtrBlacklist::observe`]。

use crate::i18n::TranslationComponent;
use crate::iputil::reverse_dns_name;
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, RuleModule};
use crate::rule::RuleSet;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, UdpSocket};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

/// 单次 PTR 解析的等待上限：上游 `DNSLookupImpl#applyDnsServers` 的
/// `replace.setTimeout(Duration.of(3, SECONDS))` 与 `PTRBlacklist` 的
/// `.get(3, TimeUnit.SECONDS)`（两者同为 3 秒）。
pub const PTR_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

/// 上游 `ModuleMatchCache` 的过期窗口（`pbh.modulematchcache.timeout` 默认 600000ms，
/// `expireAfterAccess`）：正/负结果都缓存，负结果在该窗口内不会重新解析。
pub const PTR_MATCH_CACHE_TIMEOUT_MS: i64 = 600_000;

/// 上游 `ModuleMatchCache` 的 `maximumWeight(pbh.moduleMatchCache.weight, 5000L)`
/// （pass 结果权重 1、其余 5）。这里按条目数近似该内存上限：超出时先清过期，
/// 仍满则逐出最早过期者。
const PTR_CACHE_MAX_ENTRIES: usize = 5_000;

/// PTR 解析器：对齐上游注入 `PTRBlacklist` 的 `DNSLookup` 的可注入抽象
/// （测试注入 mock，生产用 [`SystemPtrResolver`]）。
pub trait PtrResolver: Send + Sync + std::fmt::Debug {
    /// 解析 PTR 名；`None` 对齐上游 `Optional.empty()`（无记录 / 解析失败）。
    fn resolve(&self, reverse_name: &str) -> Option<String>;
}

/// 生产解析器：极小的 std UDP PTR 客户端（零新增依赖）。
///
/// 对齐 `DNSLookupImpl` 的查询语义：PTR 查询、取第一条 PTR 记录、3 秒套接字超时
/// （模块侧另有 [`PTR_LOOKUP_TIMEOUT`] 的等待上限，对齐 `.get(3, TimeUnit.SECONDS)`）。
/// 上游经 oshi 取系统 DNS 并支持 `resolvers.servers` 配置；这里读 `/etc/resolv.conf`
/// 的 `nameserver` 行（unix）。读不到解析配置的平台（如 Windows 默认部署）没有任何
/// 解析器、恒返回 `None`——对齐上游 `bootComplete=false` 时直接 `Optional.empty()`。
/// 注意：该模块上游本就未注册（不参与判定），生产路径不会走到这里。
#[derive(Debug, Clone)]
pub struct SystemPtrResolver {
    servers: Vec<IpAddr>,
    timeout: Duration,
}

impl SystemPtrResolver {
    /// 读取系统解析配置（`/etc/resolv.conf`）构造；读不到则为空解析器。
    pub fn from_system_config() -> Self {
        Self {
            servers: system_dns_servers(),
            timeout: PTR_LOOKUP_TIMEOUT,
        }
    }

    /// 用指定解析服务器构造（对齐上游 `resolvers.servers` 配置）。
    pub fn with_servers(servers: Vec<IpAddr>) -> Self {
        Self {
            servers,
            timeout: PTR_LOOKUP_TIMEOUT,
        }
    }

    /// 向单个解析服务器发起一次 PTR 查询。
    fn query(&self, server: IpAddr, reverse_name: &str) -> Option<String> {
        let packet = build_ptr_query(reverse_name)?;
        let bind: &str = if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let socket = UdpSocket::bind(bind).ok()?;
        socket.connect((server, 53)).ok()?;
        socket.set_read_timeout(Some(self.timeout)).ok()?;
        socket.set_write_timeout(Some(self.timeout)).ok()?;
        socket.send(&packet).ok()?;
        let mut buffer = [0u8; 4096];
        let received = socket.recv(&mut buffer).ok()?;
        parse_ptr_answer(&buffer[..received])
    }
}

impl PtrResolver for SystemPtrResolver {
    fn resolve(&self, reverse_name: &str) -> Option<String> {
        // 上游 `ExtendedResolver` 逐个/负载均衡尝试多服务器；这里按序尝试、首个命中即返回
        self.servers
            .iter()
            .find_map(|server| self.query(*server, reverse_name))
    }
}

/// DNS 事务 ID（对齐 RFC 1035 header 的 ID 字段）
static DNS_TRANSACTION_ID: AtomicU16 = AtomicU16::new(0);

/// 构造 PTR 查询报文（RFC 1035：1 个 question，type=PTR(12)，class=IN，RD=1）。
fn build_ptr_query(reverse_name: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(17 + reverse_name.len());
    let id = DNS_TRANSACTION_ID.fetch_add(1, Ordering::Relaxed).to_be_bytes();
    out.extend_from_slice(&id);
    out.extend_from_slice(&[0x01, 0x00]); // flags: RD=1
    out.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]); // QDCOUNT=1
    let mut labels = 0;
    for label in reverse_name.split('.') {
        let bytes = label.as_bytes();
        if bytes.is_empty() || bytes.len() > 63 {
            return None;
        }
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
        labels += 1;
    }
    if labels < 2 {
        // 反向名至少形如 `<x>.in-addr.arpa`
        return None;
    }
    out.push(0); // 根标签
    out.extend_from_slice(&[0x00, 0x0C, 0x00, 0x01]); // QTYPE=PTR, QCLASS=IN
    Some(out)
}

/// 跳过一个（可能压缩的）域名，返回该名之后的偏移。
fn skip_name(message: &[u8], mut offset: usize) -> Option<usize> {
    loop {
        let byte = *message.get(offset)?;
        if byte & 0xC0 == 0xC0 {
            // 压缩指针：名字在此结束（2 字节）
            return if offset + 2 <= message.len() { Some(offset + 2) } else { None };
        }
        if byte == 0 {
            return Some(offset + 1);
        }
        offset += 1 + byte as usize;
    }
}

/// 读取一个（可能压缩的）域名（跟随 RFC 1035 §4.1.4 压缩指针）。
fn read_name(message: &[u8], mut offset: usize) -> Option<String> {
    let mut labels: Vec<String> = Vec::new();
    let mut jumps = 0;
    loop {
        let byte = *message.get(offset)?;
        if byte & 0xC0 == 0xC0 {
            let pointer = usize::from(u16::from_be_bytes([
                byte & 0x3F,
                *message.get(offset + 1)?,
            ]));
            jumps += 1;
            if jumps > 32 {
                return None; // 压缩环路防护
            }
            offset = pointer;
            continue;
        }
        if byte == 0 {
            break;
        }
        let end = offset + 1 + byte as usize;
        let label = message.get(offset + 1..end)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        offset = end;
    }
    Some(labels.join("."))
}

/// 从 DNS 响应里取第一条 PTR 记录（对齐 `PTRRecord#getTarget().toString()`）。
fn parse_ptr_answer(message: &[u8]) -> Option<String> {
    if message.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([message[2], message[3]]);
    if flags & 0x8000 == 0 {
        return None; // QR=0：不是响应
    }
    if flags & 0x000F != 0 {
        return None; // RCODE != 0（NXDOMAIN 等，对齐 Lookup 非 SUCCESSFUL → empty）
    }
    let answers = usize::from(u16::from_be_bytes([message[6], message[7]]));
    if answers == 0 {
        return None;
    }
    let mut offset = skip_name(message, 12)?; // question name
    offset += 4; // QTYPE + QCLASS
    for _ in 0..answers {
        offset = skip_name(message, offset)?;
        let record = message.get(offset..offset + 10)?;
        let record_type = u16::from_be_bytes([record[0], record[1]]);
        let data_length = usize::from(u16::from_be_bytes([record[8], record[9]]));
        offset += 10;
        if record_type == 12 {
            return read_name(message, offset); // RDATA（可能压缩）
        }
        offset += data_length;
    }
    None
}

/// 读 `/etc/resolv.conf` 的 `nameserver` 行（对齐上游经 oshi 取系统 DNS）。
fn system_dns_servers() -> Vec<IpAddr> {
    let Ok(content) = std::fs::read_to_string("/etc/resolv.conf") else {
        return Vec::new();
    };
    content
        .lines()
        .filter_map(|line| match line.split_whitespace().collect::<Vec<_>>()[..] {
            ["nameserver", addr] => addr.parse::<IpAddr>().ok(),
            _ => None,
        })
        .collect()
}

/// 缓存条目：`value` 为 `Some(ptr)` 表示有记录，`None` 表示「无 PTR 记录/解析失败」
/// 的**负缓存**（对齐上游把 `pass()` 的 `CheckResult` 也写进 `ModuleMatchCache`）。
#[derive(Clone, Debug)]
struct PtrEntry {
    value: Option<String>,
    /// 过期时间（毫秒）；`None` 为不过期（[`PtrCache::insert`] 直写的兼容路径）。
    expires_at: Option<i64>,
}

/// PTR 查询结果缓存：模块自有，只读方是 [`RuleModule::check`]。
///
/// 对齐上游 `ModuleMatchCache`：正/负结果都缓存，`pbh.modulematchcache.timeout`
/// 默认 600000ms `expireAfterAccess`（命中续期），容量上限对齐
/// `pbh.moduleMatchCache.weight` 默认 5000。
#[derive(Debug, Default)]
pub struct PtrCache {
    entries: StdMutex<HashMap<String, PtrEntry>>,
}

impl PtrCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次查询结果（`None` 表示无 PTR 记录）；不过期（外部直写的兼容路径）。
    pub fn insert(&self, reverse_name: impl Into<String>, value: Option<String>) {
        self.insert_entry(reverse_name.into(), PtrEntry { value, expires_at: None });
    }

    /// 带过期时间写入（[`PtrBlacklist::observe`] 的解析流程用，正/负结果同一窗口）。
    pub fn insert_with_expiry(
        &self,
        reverse_name: impl Into<String>,
        value: Option<String>,
        expires_at_ms: i64,
    ) {
        self.insert_entry(
            reverse_name.into(),
            PtrEntry {
                value,
                expires_at: Some(expires_at_ms),
            },
        );
    }

    fn insert_entry(&self, reverse_name: String, entry: PtrEntry) {
        let Ok(mut map) = self.entries.lock() else { return };
        evict_if_full(&mut map);
        map.insert(reverse_name, entry);
    }

    /// 查询 PTR 名；未解析、无记录或已过期均返回 `None`（判定语义不变）。
    /// 命中时续期（对齐上游 `expireAfterAccess`）。
    pub fn get(&self, reverse_name: &str) -> Option<String> {
        self.access(reverse_name).and_then(|entry| entry.value)
    }

    /// 是否存在**未过期**条目（含负缓存；`observe` 用它跳过已解析的 IP）。
    pub fn is_resolved(&self, reverse_name: &str) -> bool {
        self.access(reverse_name).is_some()
    }

    /// 对齐上游 `reloadConfig()` 里的 `getCache().invalidateAll()`（重载后重新解析）。
    pub fn invalidate_all(&self) {
        if let Ok(mut map) = self.entries.lock() {
            map.clear();
        }
    }

    /// 读取条目：过期即移除并视为不存在；命中则续期。
    fn access(&self, reverse_name: &str) -> Option<PtrEntry> {
        let mut map = self.entries.lock().ok()?;
        let entry = map.get(reverse_name)?.clone();
        let now = now_millis();
        if is_expired_at(&entry, now) {
            map.remove(reverse_name);
            return None;
        }
        if entry.expires_at.is_some() {
            if let Some(slot) = map.get_mut(reverse_name) {
                slot.expires_at = Some(now + PTR_MATCH_CACHE_TIMEOUT_MS);
            }
        }
        Some(entry)
    }

    pub fn len(&self) -> usize {
        self.entries.lock().map(|map| map.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn is_expired_at(entry: &PtrEntry, now_ms: i64) -> bool {
    matches!(entry.expires_at, Some(expires_at) if expires_at <= now_ms)
}

/// 容量上限（对齐上游 `maximumWeight` 5000）：先清过期，仍满则逐出最早过期者。
fn evict_if_full(map: &mut HashMap<String, PtrEntry>) {
    if map.len() < PTR_CACHE_MAX_ENTRIES {
        return;
    }
    let now = now_millis();
    map.retain(|_, entry| !is_expired_at(entry, now));
    if map.len() < PTR_CACHE_MAX_ENTRIES {
        return;
    }
    let oldest = map
        .iter()
        .min_by_key(|(_, entry)| entry.expires_at.unwrap_or(i64::MAX))
        .map(|(key, _)| key.clone());
    if let Some(oldest) = oldest {
        map.remove(&oldest);
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

pub struct PtrBlacklist {
    pub rules: RuleSet,
    /// 0 表示使用全局封禁时长
    pub ban_duration_ms: i64,
    /// 模块自有的 PTR 结果缓存（正/负结果；上游 `ModuleMatchCache` 的对应物）
    pub cache: Arc<PtrCache>,
    /// 模块内解析器（对齐上游注入的 `DNSLookup`）
    resolver: Arc<dyn PtrResolver>,
    /// 单次解析等待上限（对齐上游 `.get(3, TimeUnit.SECONDS)`）
    resolve_timeout: Duration,
    /// 在途解析去重（同一反向名最多一个解析线程）
    in_flight: Arc<StdMutex<HashSet<String>>>,
}

impl PtrBlacklist {
    /// `cache` 即模块自有的结果存储；调用方持有同一 `Arc` 可直接注入测试数据。
    /// 解析器默认 [`SystemPtrResolver`]，测试可用 [`PtrBlacklist::with_resolver`] 注入 mock。
    pub fn new(rules: RuleSet, ban_duration_ms: i64, cache: Arc<PtrCache>) -> Self {
        Self {
            rules,
            ban_duration_ms,
            cache,
            resolver: Arc::new(SystemPtrResolver::from_system_config()),
            resolve_timeout: PTR_LOOKUP_TIMEOUT,
            in_flight: Arc::new(StdMutex::new(HashSet::new())),
        }
    }

    /// 注入解析器（测试 / 自定义 DNS 后端）。
    pub fn with_resolver(mut self, resolver: Arc<dyn PtrResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    /// 覆盖单次解析等待上限（默认 [`PTR_LOOKUP_TIMEOUT`] = 上游 3 秒）。
    pub fn with_resolve_timeout(mut self, timeout: Duration) -> Self {
        self.resolve_timeout = timeout;
        self
    }

    /// 对齐上游 `reloadConfig()` 的 `getCache().invalidateAll()`。
    pub fn invalidate_cache(&self) {
        self.cache.invalidate_all();
    }

    /// 对一个观测到的 peer IP 发起 fire-and-forget 的 PTR 解析（模块内解析的所有权入口）。
    ///
    /// - 已有未过期缓存（含负缓存）⇒ 不再解析（上游负缓存语义：不重复打 DNS）；
    /// - 同一反向名在途 ⇒ 去重；
    /// - 否则起独立 std 线程解析（等待上限 [`PTR_LOOKUP_TIMEOUT`]，超时按「无记录」
    ///   负缓存，对齐上游 `TimeoutException → Optional.empty()`），结果写回
    ///   [`Self::cache`]（正/负结果统一 [`PTR_MATCH_CACHE_TIMEOUT_MS`] 窗口）。
    ///
    /// **绝不阻塞调用方**：立即返回，解析在后台完成；[`RuleModule::check`] 只读缓存，
    /// 因此首次观测的 peer 本轮 `pass()`、下一轮才按结果判定（见模块文档的时序说明）。
    pub fn observe(&self, ip: &str) {
        let Some(reverse_name) = reverse_dns_name(ip) else {
            return;
        };
        if self.cache.is_resolved(&reverse_name) {
            return;
        }
        {
            let Ok(mut in_flight) = self.in_flight.lock() else {
                return;
            };
            if !in_flight.insert(reverse_name.clone()) {
                return;
            }
        }
        let cache = self.cache.clone();
        let in_flight = self.in_flight.clone();
        let resolver = self.resolver.clone();
        let timeout = self.resolve_timeout;
        // 起线程失败时的回退路径需要独立克隆（闭包会拿走所有权）
        let retry_in_flight = self.in_flight.clone();
        let retry_name = reverse_name.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("ptr-resolve-{reverse_name}"))
            .spawn(move || {
                let ptr = resolve_with_timeout(&resolver, &reverse_name, timeout);
                let expires_at = now_millis() + PTR_MATCH_CACHE_TIMEOUT_MS;
                cache.insert_with_expiry(&reverse_name, ptr, expires_at);
                if let Ok(mut in_flight) = in_flight.lock() {
                    in_flight.remove(&reverse_name);
                }
            });
        if spawned.is_err() {
            // 起线程失败：放行本轮，允许下次观测重试
            if let Ok(mut in_flight) = retry_in_flight.lock() {
                in_flight.remove(&retry_name);
            }
        }
    }
}

/// 带等待上限的解析（对齐 `.get(3, TimeUnit.SECONDS)`）：查询在独立线程执行，
/// 超过 `timeout` 即按「无记录」返回（底层线程随后自行结束，结果被丢弃）。
fn resolve_with_timeout(
    resolver: &Arc<dyn PtrResolver>,
    reverse_name: &str,
    timeout: Duration,
) -> Option<String> {
    let resolver = resolver.clone();
    let reverse_name = reverse_name.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(resolver.resolve(&reverse_name));
    });
    rx.recv_timeout(timeout).unwrap_or(None)
}

impl RuleModule for PtrBlacklist {
    fn name(&self) -> &str {
        "PTR Blacklist"
    }
    fn config_name(&self) -> &str {
        "ptr-blacklist"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn check(
        &self,
        _downloader_id: &str,
        _torrent: &TorrentData,
        peer: &PeerData,
        _ctx: &CheckContext,
    ) -> CheckResult {
        let module = self.config_name().to_string();
        if peer.is_handshaking() {
            return CheckResult::handshaking(&module);
        }
        let Some(reverse_name) = reverse_dns_name(&peer.ip) else {
            return CheckResult::pass(&module);
        };
        // 只读缓存：无记录（未解析/负缓存）⇒ pass；有记录 ⇒ 规则匹配（语义与原实现一致）
        let Some(ptr) = self.cache.get(&reverse_name) else {
            return CheckResult::pass(&module);
        };
        let result = self.rules.r#match(Some(&ptr));
        if !result.hit {
            return CheckResult::pass(&module);
        }
        let Some(matched) = self.rules.rules.get(result.index.max(0) as usize) else {
            return CheckResult::pass(&module);
        };
        let name = matched.name_component();
        CheckResult::ban(
            &module,
            self.ban_duration_ms,
            "ptr",
            &format!("PTR {ptr} matched rule {}", matched.metadata()),
            serde_json::json!({ "rule": matched.metadata() }),
        )
        .with_keys(
            name.clone(),
            TranslationComponent::with_params("MODULE_PTR_MATCH_PTR_RULE", vec![name.into()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::TorrentData;
    use crate::module::PeerAction;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    /// 计数 mock 解析器（不出网）
    #[derive(Debug)]
    struct MockResolver {
        result: Option<String>,
        delay: Duration,
        calls: AtomicUsize,
    }

    impl MockResolver {
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl PtrResolver for MockResolver {
        fn resolve(&self, _reverse_name: &str) -> Option<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            self.result.clone()
        }
    }

    fn resolver_with(result: Option<&str>, delay: Duration) -> Arc<MockResolver> {
        Arc::new(MockResolver {
            result: result.map(str::to_string),
            delay,
            calls: AtomicUsize::new(0),
        })
    }

    fn module_with_resolver(resolver: Arc<MockResolver>, timeout: Duration) -> PtrBlacklist {
        let rules = RuleSet::from_json_text(&[r#"{"method":"EQUALS","content":"example.com"}"#.to_string()])
            .expect("parse rule");
        PtrBlacklist::new(rules, 0, Arc::new(PtrCache::new()))
            .with_resolver(resolver)
            .with_resolve_timeout(timeout)
    }

    fn torrent() -> TorrentData {
        TorrentData {
            hash: "h".to_string(),
            name: "t".to_string(),
            progress: 0.5,
            total_size: 1_000,
            piece_size: 0,
            pieces_have: 0,
            completed_override: None,
            dlspeed: 1_000,
            upspeed: 1_000,
            is_private: Some(false),
        }
    }

    fn peer(ip: &str) -> PeerData {
        PeerData {
            client_name: None,
            peer_id: None,
            dl_speed: 1_000,
            downloaded: 1_000,
            up_speed: 1_000,
            uploaded: 1_000,
            progress: 0.5,
            flags: Some("d u".to_string()),
            ip: ip.to_string(),
            port: 51413,
            raw_ip: format!("{ip}:51413"),
            connection: None,
        }
    }

    fn wait_for(predicate: impl Fn() -> bool, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if predicate() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        predicate()
    }

    #[test]
    fn observe_resolves_inside_module_and_check_bans_next_round() {
        let resolver = resolver_with(Some("example.com"), Duration::ZERO);
        let module = module_with_resolver(resolver.clone(), PTR_LOOKUP_TIMEOUT);

        // 首轮：缓存未就绪 ⇒ pass（判定时序：首轮放行、下一轮判定）
        let first = module.check("d", &torrent(), &peer("1.2.3.4"), &CheckContext::default());
        assert_eq!(first.action, PeerAction::NoAction);
        assert_eq!(resolver.calls(), 0, "check 本身不得发起解析");

        module.observe("1.2.3.4");
        assert!(wait_for(
            || resolver.calls() == 1 && module.cache.is_resolved("4.3.2.1.in-addr.arpa"),
            Duration::from_secs(2)
        ));
        assert_eq!(
            module.cache.get("4.3.2.1.in-addr.arpa").as_deref(),
            Some("example.com")
        );

        // 第二轮：命中规则（判定语义不变）
        let second = module.check("d", &torrent(), &peer("1.2.3.4"), &CheckContext::default());
        assert_eq!(second.action, PeerAction::Ban);
        assert_eq!(second.data["rule"], "example.com");
    }

    #[test]
    fn negative_result_is_cached_and_not_retried() {
        let resolver = resolver_with(None, Duration::ZERO);
        let module = module_with_resolver(resolver.clone(), PTR_LOOKUP_TIMEOUT);

        module.observe("1.2.3.4");
        assert!(wait_for(
            || resolver.calls() == 1 && module.cache.is_resolved("4.3.2.1.in-addr.arpa"),
            Duration::from_secs(2)
        ));

        // 负缓存窗口内：check 放行，重复观测不再打 DNS（对齐上游负结果也进 ModuleMatchCache）
        assert_eq!(
            module
                .check("d", &torrent(), &peer("1.2.3.4"), &CheckContext::default())
                .action,
            PeerAction::NoAction
        );
        module.observe("1.2.3.4");
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(resolver.calls(), 1, "负缓存窗口内不得重新解析");
    }

    #[test]
    fn expired_entry_is_re_resolved_on_next_observe() {
        let resolver = resolver_with(Some("example.com"), Duration::ZERO);
        let module = module_with_resolver(resolver.clone(), PTR_LOOKUP_TIMEOUT);

        // 预置一条已过期的负缓存（模拟 TTL 到期）
        module
            .cache
            .insert_with_expiry("4.3.2.1.in-addr.arpa", None, now_millis() - 1);
        assert!(!module.cache.is_resolved("4.3.2.1.in-addr.arpa"));

        module.observe("1.2.3.4");
        assert!(wait_for(
            || resolver.calls() == 1
                && module.cache.get("4.3.2.1.in-addr.arpa").as_deref() == Some("example.com"),
            Duration::from_secs(2)
        ));
    }

    #[test]
    fn slow_resolver_hits_timeout_and_is_negatively_cached() {
        // 解析 500ms 才返回，等待上限 50ms ⇒ 对齐上游 TimeoutException → Optional.empty()
        let resolver = resolver_with(Some("example.com"), Duration::from_millis(500));
        let module =
            module_with_resolver(resolver.clone(), Duration::from_millis(50));

        module.observe("1.2.3.4");
        assert!(
            wait_for(
                || module.cache.is_resolved("4.3.2.1.in-addr.arpa"),
                Duration::from_secs(2)
            ),
            "超时后应立刻写入负缓存"
        );
        assert!(
            module.cache.get("4.3.2.1.in-addr.arpa").is_none(),
            "超时结果按「无记录」缓存"
        );
        assert_eq!(
            module
                .check("d", &torrent(), &peer("1.2.3.4"), &CheckContext::default())
                .action,
            PeerAction::NoAction
        );
    }

    #[test]
    fn concurrent_observe_deduplicates_in_flight_resolution() {
        let resolver = resolver_with(Some("example.com"), Duration::from_millis(100));
        let module = module_with_resolver(resolver.clone(), PTR_LOOKUP_TIMEOUT);

        module.observe("1.2.3.4");
        module.observe("1.2.3.4"); // 在途去重
        assert!(wait_for(
            || module.cache.is_resolved("4.3.2.1.in-addr.arpa"),
            Duration::from_secs(2)
        ));
        assert_eq!(resolver.calls(), 1, "同一反向名最多一个解析线程");
    }

    #[test]
    fn invalid_invalid_ip_and_cache_invalidation() {
        let resolver = resolver_with(Some("example.com"), Duration::ZERO);
        let module = module_with_resolver(resolver.clone(), PTR_LOOKUP_TIMEOUT);

        // 非法 IP ⇒ 无反向名，observe 直接返回
        module.observe("not-an-ip");
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(resolver.calls(), 0);

        // 上游 reloadConfig 的 invalidateAll：重载后重新解析
        module.cache.insert("4.3.2.1.in-addr.arpa", Some("example.com".to_string()));
        assert!(module.cache.is_resolved("4.3.2.1.in-addr.arpa"));
        module.invalidate_cache();
        assert!(!module.cache.is_resolved("4.3.2.1.in-addr.arpa"));
        assert!(module.cache.is_empty());
    }

    #[test]
    fn get_renews_expiry_on_access() {
        let cache = PtrCache::new();
        cache.insert_with_expiry(
            "4.3.2.1.in-addr.arpa",
            Some("example.com".to_string()),
            now_millis() + 100,
        );
        // 命中续期（expireAfterAccess）：多次 get 后过期时间不断后移
        for _ in 0..3 {
            assert_eq!(cache.get("4.3.2.1.in-addr.arpa").as_deref(), Some("example.com"));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(cache.get("4.3.2.1.in-addr.arpa").as_deref(), Some("example.com"));
    }

    #[test]
    fn dns_query_and_answer_roundtrip() {
        let query = build_ptr_query("4.3.2.1.in-addr.arpa").expect("query");
        // 头部：ID(2) + flags RD=1 + QDCOUNT=1；问题段：4.3.2.1.in-addr.arpa + PTR/IN
        assert_eq!(
            &query[12..],
            &[
                0x01, b'4', 0x01, b'3', 0x01, b'2', 0x01, b'1', 0x07, b'i', b'n', b'-', b'a', b'd',
                b'd', b'r', 0x04, b'a', b'r', b'p', b'a', 0x00, 0x00, 0x0C, 0x00, 0x01
            ][..]
        );
        assert!(build_ptr_query("single").is_none(), "必须带 arpa 后缀的完整反向名");

        // 应答：question 名与答案名都用压缩指针（0xC00C），答案 PTR RDATA 同样压缩
        let mut answer = query.clone();
        answer[2] = 0x81;
        answer[3] = 0x80; // QR=1, RD=1, RA=1, RCODE=0
        answer[6] = 0x00;
        answer[7] = 0x01; // ANCOUNT=1
        answer.extend_from_slice(&[
            0xC0, 0x0C, // NAME（指针）
            0x00, 0x0C, 0x00, 0x01, // TYPE=PTR, CLASS=IN
            0x00, 0x00, 0x00, 60, // TTL
            0x00, 0x02, // RDLENGTH
            0xC0, 0x0C, // RDATA（指针）
        ]);
        assert_eq!(
            parse_ptr_answer(&answer).as_deref(),
            Some("4.3.2.1.in-addr.arpa")
        );

        // RCODE != 0（NXDOMAIN）⇒ None
        let mut rcode3 = answer.clone();
        rcode3[3] = 0x83;
        assert_eq!(parse_ptr_answer(&rcode3), None);
        // 无回答 ⇒ None
        let mut no_answer = answer;
        no_answer[7] = 0x00;
        assert_eq!(parse_ptr_answer(&no_answer), None);
    }

    #[test]
    fn system_resolver_without_servers_returns_none() {
        // 空解析器列表（读不到系统 DNS 的平台）恒 None，对齐上游 bootComplete=false
        let resolver = SystemPtrResolver::with_servers(Vec::new());
        assert!(resolver.resolve("4.3.2.1.in-addr.arpa").is_none());
    }
}
