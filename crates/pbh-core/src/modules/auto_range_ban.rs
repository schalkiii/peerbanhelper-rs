//! 范围自动封禁（`auto-range-ban`），忠实复刻上游 `AutoRangeBan`。
//!
//! 逻辑：若某个 peer 的地址落在**已封禁地址**的指定前缀网段内（同地址族），
//! 则对它执行连锁封禁。PCB 快速测试产生的 `BAN_FOR_DISCONNECT` 记录不参与连锁。

use crate::banlist::BanList;
use crate::i18n::TranslationComponent;
use crate::iputil::{parse_addr, prefix_block};
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, RuleModule};
use std::net::IpAddr;
use std::sync::{Arc, Mutex as StdMutex};

pub struct AutoRangeBan {
    pub ipv4_prefix: u8,
    pub ipv6_prefix: u8,
    /// 0 表示使用全局封禁时长
    pub ban_duration_ms: i64,
    /// 与 wave 共享的内存封禁表（对齐上游注入的 `BanList`）
    pub ban_list: Arc<StdMutex<BanList>>,
}

impl AutoRangeBan {
    pub fn new(
        ipv4_prefix: u8,
        ipv6_prefix: u8,
        ban_duration_ms: i64,
        ban_list: Arc<StdMutex<BanList>>,
    ) -> Self {
        Self { ipv4_prefix, ipv6_prefix, ban_duration_ms, ban_list }
    }
}

impl RuleModule for AutoRangeBan {
    fn name(&self) -> &str {
        "Auto Range Ban"
    }
    fn config_name(&self) -> &str {
        "auto-range-ban"
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
        // 上游此处返回 pass()（OK_CHECK_RESULT），而非 handshaking()
        if peer.is_handshaking() {
            return CheckResult::pass(&module);
        }
        let Some(peer_addr) = parse_addr(&peer.ip) else {
            return CheckResult::pass(&module);
        };
        let Ok(list) = self.ban_list.lock() else {
            return CheckResult::pass(&module);
        };
        // 自身已在封禁表中：交给其它模块处理
        if list.contains(&peer_addr.to_string()) {
            return CheckResult::pass(&module);
        }

        for (banned_ip, meta) in list.iter() {
            if meta.ban_for_disconnect {
                continue;
            }
            let Some(banned_addr) = parse_addr(banned_ip) else {
                continue;
            };
            if banned_addr.is_ipv4() != peer_addr.is_ipv4() {
                continue;
            }
            let (prefix_len, address_type) = match banned_addr {
                IpAddr::V4(_) => (self.ipv4_prefix, format!("IPv4/{}", self.ipv4_prefix)),
                IpAddr::V6(_) => (self.ipv6_prefix, format!("IPv6/{}", self.ipv6_prefix)),
            };
            let Some(cidr) = prefix_block(&banned_addr.to_string(), prefix_len, prefix_len) else {
                continue;
            };
            let Some(net) = crate::iputil::parse_net(&cidr) else {
                continue;
            };
            if net.contains(&peer_addr) {
                return CheckResult::ban(
                    &module,
                    self.ban_duration_ms,
                    &address_type,
                    &format!(
                        "peer {} is in the same ban range as banned address {banned_ip}",
                        peer_addr
                    ),
                    serde_json::json!({ "relatedBannedAddress": banned_ip }),
                )
                .with_keys(
                    // 上游把地址类型当作字面量 key（文案表无此键 -> 原样输出）
                    TranslationComponent::new(address_type.clone()),
                    TranslationComponent::with_params(
                        "ARB_BANNED",
                        vec![
                            peer_addr.to_string().into(),
                            banned_ip.clone().into(),
                            cidr.into(),
                            address_type.into(),
                        ],
                    ),
                );
            }
        }
        CheckResult::pass(&module)
    }
}
