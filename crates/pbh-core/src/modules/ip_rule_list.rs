//! IP 黑名单远程订阅（`ip-address-blocker-rules`），忠实复刻上游 `IPBlackRuleList` + `IPMatcher`。
//!
//! 本模块只负责**解析规则文本**与**判定**；下载与定时刷新由应用层负责
//! （对齐上游 `getResource` + `registerScheduledTask(reloadConfig, 0, check-interval)`），
//! 解析后的数据通过 [`IpRuleListModule::set_subscription`] 注入。
//!
//! 规则文本语法（对齐 `stringToIPList` / `parseRuleLine`）：
//! - 空行跳过；
//! - 以 `#` 或 `//` 开头的整行是**注释行**，会累积给下一条 IP 规则（多行用 `\n` 连接）；
//! - 含 `,` 的行按 DAT/eMule 格式 `start , end , level [, comment]` 解析；
//!   `level >= 128` 丢弃；区间取「覆盖整个区间的最小前缀块」；
//! - 其余行取行内第一个 `#` 或 `//` 之前的 IP，之后为尾注释（优先于累积注释）；
//! - 同一地址块重复出现时合并注释（`\n` 连接）。

use crate::i18n::TranslationComponent;
use crate::iputil::parse_addr;
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, RuleModule};
use ipnet::IpNet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::sync::RwLock;

/// 一条规则：网段 + 备注（对齐上游 trie 中的 `(IPAddress, comment)`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleListEntry {
    pub net: IpNet,
    pub comment: String,
}

/// 解析规则文本，返回 `(去重后的规则条目, 上游口径的“加载行数”)`。
///
/// 说明：上游对不可解析的行会用占位地址 `127.123.123.123`（`IPAddressUtil` 的行为）并计入行数；
/// 本实现直接跳过非法行（占位地址恒被 bypass，行为等价且不会污染规则集），故此处计数不含非法行。
pub fn parse_rule_list(data: &str) -> (Vec<RuleListEntry>, usize) {
    let mut entries: Vec<RuleListEntry> = Vec::new();
    let mut accumulated_comment: Vec<String> = Vec::new();
    let mut count = 0usize;

    for raw_line in data.split('\n') {
        let line = raw_line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            accumulated_comment.push(rest.to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("//") {
            accumulated_comment.push(rest.to_string());
            continue;
        }
        let pre_read_comment = accumulated_comment.join("\n");
        let parsed = parse_rule_line(line, &pre_read_comment);
        // 无论解析成功与否都消费掉累积注释（对齐上游 finally 块）
        accumulated_comment.clear();
        if let Some(entry) = parsed {
            count += 1;
            match entries.iter_mut().find(|e| e.net == entry.net) {
                Some(existing) => {
                    if !existing.comment.is_empty() && !entry.comment.is_empty() {
                        existing.comment.push('\n');
                    }
                    existing.comment.push_str(&entry.comment);
                }
                None => entries.push(entry),
            }
        }
    }
    (entries, count)
}

/// 解析单行规则（对齐上游 `parseRuleLine`）。
fn parse_rule_line(line: &str, pre_read_comment: &str) -> Option<RuleListEntry> {
    if line.contains(',') {
        // DAT/eMule 格式：start , end , level [, comment]
        // 注意：上游用 `Integer.parseInt`（不 trim），带空格的字段会抛异常导致该行被丢弃
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() < 3 {
            return None;
        }
        let start = parse_rule_host(fields[0])?;
        let end = parse_rule_host(fields[1])?;
        let level: i64 = fields[2].parse().ok()?;
        if level >= 128 {
            return None;
        }
        let comment = if fields.len() > 3 {
            fields[3].to_string()
        } else {
            pre_read_comment.to_string()
        };
        let net = cover_with_prefix_block(start, end)?;
        return Some(RuleListEntry { net, comment });
    }

    // ip #end-line-comment / ip //end-line-comment
    let double_slash = line.find("//");
    let hash = line.find('#');
    let comment_index = match (double_slash, hash) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    };
    match comment_index {
        Some(idx) => {
            let net = parse_rule_net(&line[..idx])?;
            // `#` 占 1 字符，`//` 占 2 字符
            let delimiter_len = if line.as_bytes()[idx] == b'#' { 1 } else { 2 };
            let comment = line[idx + delimiter_len..].to_string();
            Some(RuleListEntry { net, comment })
        }
        None => {
            let net = parse_rule_net(line)?;
            Some(RuleListEntry {
                net,
                comment: pre_read_comment.to_string(),
            })
        }
    }
}

fn host_net(addr: IpAddr) -> IpNet {
    match addr {
        IpAddr::V4(v4) => IpNet::new(IpAddr::V4(v4), 32).expect("v4/32"),
        IpAddr::V6(v6) => IpNet::new(IpAddr::V6(v6), 128).expect("v6/128"),
    }
}

/// 解析规则行中的地址/网段（支持 `1.2.3.0/24`、单 IP、以及 IPv4 前导零写法）。
///
/// 对齐上游 `IPAddressUtil.getIPAddress`：其内部对输入做 trim（见 `parseRuleLine`
/// 中对 `ele.substring(0, commentIndex)` 的处理——`4.4.4.4 # 注释` 的尾部空格被容忍）。
fn parse_rule_net(raw: &str) -> Option<IpNet> {
    let s = raw.trim();
    if let Some(addr) = parse_addr(s) {
        return Some(host_net(addr));
    }
    if let Ok(net) = IpNet::from_str(s) {
        return Some(net);
    }
    parse_lenient_ipv4(s).map(|v4| host_net(IpAddr::V4(v4)))
}

/// 解析 DAT 行中的区间端点（仅主机地址）。
///
/// 对齐上游 `IPAddressUtil.getIPAddress`（内部 trim）。注意：DAT 行的**等级字段**
/// 走 `Integer.parseInt` 且**不 trim**，带空格会抛异常导致整行被丢弃——该行为由
/// `parse_rule_line` 中 `fields[2].parse()`（同样不 trim）忠实复刻。
fn parse_rule_host(raw: &str) -> Option<IpAddr> {
    let s = raw.trim();
    if let Some(addr) = parse_addr(s) {
        return Some(addr);
    }
    parse_lenient_ipv4(s).map(IpAddr::V4)
}

fn parse_lenient_ipv4(s: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut octets = [0u8; 4];
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        octets[i] = part.parse().ok()?;
    }
    Some(Ipv4Addr::from(octets))
}

/// `spanWithRange(...).coverWithPrefixBlock()`：覆盖 `[start, end]` 的最小前缀块。
///
/// 必须先把 `start` 向下对齐到前缀边界——上游的 `coverWithPrefixBlock` 返回的是
/// 对齐后的前缀块，而非原起点。非对齐区间（如 `[1.2.3.5, 1.2.3.6]`）上游生成
/// `1.2.3.4/30`，若不对其，`IpNet::new` 会因主机位非零返回 `None` 而整行被丢弃。
fn cover_with_prefix_block(start: IpAddr, end: IpAddr) -> Option<IpNet> {
    match (start, end) {
        (IpAddr::V4(a), IpAddr::V4(b)) => {
            let (a_bits, b_bits) = (u32::from(a), u32::from(b));
            if a_bits > b_bits {
                return None;
            }
            let prefix = (a_bits ^ b_bits).leading_zeros() as u8;
            let mask = if prefix == 0 {
                0u32
            } else {
                !0u32 << (32 - prefix)
            };
            let aligned = Ipv4Addr::from(a_bits & mask);
            IpNet::new(IpAddr::V4(aligned), prefix).ok()
        }
        (IpAddr::V6(a), IpAddr::V6(b)) => {
            let (a_bits, b_bits) = (u128::from(a), u128::from(b));
            if a_bits > b_bits {
                return None;
            }
            let prefix = (a_bits ^ b_bits).leading_zeros() as u8;
            let mask = if prefix == 0 {
                0u128
            } else {
                !0u128 << (128 - prefix)
            };
            let aligned = Ipv6Addr::from(a_bits & mask);
            IpNet::new(IpAddr::V6(aligned), prefix).ok()
        }
        _ => None,
    }
}

/// 网段索引：按前缀长度分桶，查询时从最长前缀（最精确）向最短匹配。
#[derive(Debug, Default)]
struct RuleIndex {
    v4: Vec<Vec<(u32, u32, String)>>,
    v6: Vec<Vec<(u128, u128, String)>>,
}

impl RuleIndex {
    fn build(entries: &[RuleListEntry]) -> Self {
        let mut index = RuleIndex {
            v4: vec![Vec::new(); 33],
            v6: vec![Vec::new(); 129],
        };
        for entry in entries {
            let prefix = entry.net.prefix_len();
            match entry.net.addr() {
                IpAddr::V4(v4) => {
                    let start = u32::from(v4);
                    // `/0` 掩码为主机位全 1；`/32` 无主机位。
                    // 不能写 `!0u32 >> prefix`：prefix == 32 时移位量等于位宽，debug 下 panic。
                    let end = start | (!0u32).checked_shr(prefix as u32).unwrap_or(0);
                    index.v4[prefix as usize].push((start, end, entry.comment.clone()));
                }
                IpAddr::V6(v6) => {
                    let start = u128::from(v6);
                    // 同上：prefix == 128 时 `!0u128 >> prefix` 会 panic
                    let end = start | (!0u128).checked_shr(prefix as u32).unwrap_or(0);
                    index.v6[prefix as usize].push((start, end, entry.comment.clone()));
                }
            }
        }
        for bucket in index.v4.iter_mut() {
            bucket.sort_unstable_by_key(|e| e.0);
        }
        for bucket in index.v6.iter_mut() {
            bucket.sort_unstable_by_key(|e| e.0);
        }
        index
    }

    /// 最长前缀匹配：返回命中网段的备注（可能为空字符串）。
    ///
    /// 同一前缀长度的网段互不相交，因此每个桶内按起始地址二分即可；
    /// 桶按前缀长度从长到短遍历，保证命中「最精确」的网段（对齐 trie 的 `elementsContaining`）。
    fn lookup(&self, ip: IpAddr) -> Option<&str> {
        match ip {
            IpAddr::V4(v4) => {
                let value = u32::from(v4);
                for bucket in self.v4.iter().rev() {
                    let idx = bucket.partition_point(|e| e.0 <= value);
                    if idx > 0 {
                        let candidate = &bucket[idx - 1];
                        if value <= candidate.1 {
                            return Some(candidate.2.as_str());
                        }
                    }
                }
                None
            }
            IpAddr::V6(v6) => {
                let value = u128::from(v6);
                for bucket in self.v6.iter().rev() {
                    let idx = bucket.partition_point(|e| e.0 <= value);
                    if idx > 0 {
                        let candidate = &bucket[idx - 1];
                        if value <= candidate.1 {
                            return Some(candidate.2.as_str());
                        }
                    }
                }
                None
            }
        }
    }
}

/// 规则订阅的更新计划（对齐上游 `updateRule` 的 sha256 比对流程）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuleUpdatePlan {
    /// 解析远程内容；`write_cache` 表示内容与本地缓存不同，需要写回缓存文件
    ParseRemote { write_cache: bool },
    /// 远程不可用时回退解析本地缓存文件
    ParseCache,
    /// 无需动作（内容未变化且已加载，或远程不可用且无缓存）
    NoAction,
}

/// 决策规则订阅是否需要解析/落盘。
///
/// 上游流程：
/// - 远程成功：与本地缓存文件 sha256 不同 → 解析远程内容并写回缓存；
///   相同但内存中尚未加载该规则 → 解析远程内容（不重复写盘）；否则跳过；
/// - 远程失败：若存在缓存文件且尚未加载 → 解析缓存文件；否则跳过。
pub fn plan_rule_update(
    remote_available: bool,
    remote_differs_from_cache: bool,
    cache_exists: bool,
    already_loaded: bool,
) -> RuleUpdatePlan {
    if remote_available {
        if remote_differs_from_cache {
            RuleUpdatePlan::ParseRemote { write_cache: true }
        } else if !already_loaded {
            RuleUpdatePlan::ParseRemote { write_cache: false }
        } else {
            RuleUpdatePlan::NoAction
        }
    } else if cache_exists && !already_loaded {
        RuleUpdatePlan::ParseCache
    } else {
        RuleUpdatePlan::NoAction
    }
}

/// 一条订阅（对齐上游一个 `IPMatcher`）。
#[derive(Debug)]
pub struct RuleSubscription {
    pub rule_id: String,
    pub name: String,
    pub entries: Vec<RuleListEntry>,
}

struct LoadedSubscription {
    rule_id: String,
    name: String,
    index: RuleIndex,
    entry_count: usize,
}

pub struct IpRuleListModule {
    /// 0 表示使用全局封禁时长
    pub ban_duration_ms: i64,
    subscriptions: RwLock<Vec<LoadedSubscription>>,
}

impl IpRuleListModule {
    pub fn new(ban_duration_ms: i64) -> Self {
        Self {
            ban_duration_ms,
            subscriptions: RwLock::new(Vec::new()),
        }
    }

    /// 新增或更新一条订阅；返回 `true` 表示新增，`false` 表示更新了已有订阅
    /// （对齐上游 `ipBanMatchers.stream().filter(...).findFirst().ifPresentOrElse(...)`）。
    pub fn set_subscription(&self, subscription: RuleSubscription) -> bool {
        let loaded = LoadedSubscription {
            rule_id: subscription.rule_id.clone(),
            name: subscription.name,
            entry_count: subscription.entries.len(),
            index: RuleIndex::build(&subscription.entries),
        };
        let Ok(mut list) = self.subscriptions.write() else {
            return false;
        };
        match list.iter_mut().find(|s| s.rule_id == loaded.rule_id) {
            Some(existing) => {
                *existing = loaded;
                false
            }
            None => {
                list.push(loaded);
                true
            }
        }
    }

    /// 移除订阅（对齐上游规则被禁用时的 `removeIf`）。
    pub fn remove_subscription(&self, rule_id: &str) {
        if let Ok(mut list) = self.subscriptions.write() {
            list.retain(|s| s.rule_id != rule_id);
        }
    }

    pub fn subscription_count(&self) -> usize {
        self.subscriptions.read().map(|l| l.len()).unwrap_or(0)
    }

    /// 该订阅是否已加载（对齐上游 `ipBanMatchers.stream().noneMatch(...)` 判断）。
    pub fn has_subscription(&self, rule_id: &str) -> bool {
        self.subscriptions
            .read()
            .map(|l| l.iter().any(|s| s.rule_id == rule_id))
            .unwrap_or(false)
    }

    /// 各订阅的条目数（供日志/状态展示）。
    pub fn entry_counts(&self) -> Vec<(String, usize)> {
        self.subscriptions
            .read()
            .map(|l| {
                l.iter()
                    .map(|s| (s.rule_id.clone(), s.entry_count))
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl RuleModule for IpRuleListModule {
    fn name(&self) -> &str {
        "IP Blacklist Rule List"
    }
    fn config_name(&self) -> &str {
        "ip-address-blocker-rules"
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
        let Some(addr) = parse_addr(&peer.ip) else {
            return CheckResult::pass(&module);
        };
        let Ok(list) = self.subscriptions.read() else {
            return CheckResult::pass(&module);
        };
        for subscription in list.iter() {
            if let Some(comment) = subscription.index.lookup(addr) {
                return CheckResult::ban(
                    &module,
                    self.ban_duration_ms,
                    &subscription.name,
                    &format!("matched IP rule subscription `{}`", subscription.name),
                    serde_json::json!({ "ruleName": subscription.name }),
                )
                .with_keys(
                    // 上游把规则名当作字面量 key（文案表通常无此键 -> 原样输出）
                    TranslationComponent::new(subscription.name.clone()),
                    TranslationComponent::with_params(
                        "MODULE_IBL_MATCH_IP_RULE",
                        vec![
                            subscription.name.clone().into(),
                            peer.ip.clone().into(),
                            // 上游注释可为空字符串（此时渲染为空），
                            // MODULE_IBL_COMMENT_UNKNOWN 仅在 comment 为 null 时才可能用到（实际不可达）
                            TranslationComponent::new(comment.to_string()).into(),
                        ],
                    ),
                );
            }
        }
        CheckResult::pass(&module)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cover_with_prefix_block_matches_upstream_examples() {
        let net = cover_with_prefix_block(
            IpAddr::V4(Ipv4Addr::new(16, 0, 0, 0)),
            IpAddr::V4(Ipv4Addr::new(16, 255, 255, 255)),
        )
        .unwrap();
        assert_eq!(net.to_string(), "16.0.0.0/8");

        let single = cover_with_prefix_block(
            IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)),
            IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)),
        )
        .unwrap();
        assert_eq!(single.to_string(), "1.2.3.4/32");

        // 非对齐起点：上游 coverWithPrefixBlock 向下对齐到前缀边界
        let aligned = cover_with_prefix_block(
            IpAddr::V4(Ipv4Addr::new(1, 2, 3, 5)),
            IpAddr::V4(Ipv4Addr::new(1, 2, 3, 6)),
        )
        .unwrap();
        assert_eq!(aligned.to_string(), "1.2.3.4/30");

        // 跨 4 地址、非对齐
        let aligned2 = cover_with_prefix_block(
            IpAddr::V4(Ipv4Addr::new(1, 2, 3, 5)),
            IpAddr::V4(Ipv4Addr::new(1, 2, 3, 8)),
        )
        .unwrap();
        assert_eq!(aligned2.to_string(), "1.2.3.0/28");

        // IPv6 同样对齐
        let v6 = cover_with_prefix_block(
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x5)),
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0x6)),
        )
        .unwrap();
        assert_eq!(v6.to_string(), "2001:db8::4/126");
    }

    #[test]
    fn lenient_ipv4_accepts_leading_zeros() {
        assert_eq!(
            parse_lenient_ipv4("016.000.000.000"),
            Some(Ipv4Addr::new(16, 0, 0, 0))
        );
        assert_eq!(parse_lenient_ipv4("1.2.3"), None);
        assert_eq!(parse_lenient_ipv4("1.2.3.256"), None);
    }
}
