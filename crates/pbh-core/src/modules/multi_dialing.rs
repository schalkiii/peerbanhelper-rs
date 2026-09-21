//! 多拨封禁（`multi-dialing-blocker`），忠实复刻上游 `MultiDialingBlocker`。
//!
//! 判定：同一个子网（IPv4 默认 /24、IPv6 默认 /56）在同一 torrent 上出现的**不同 IP 数**
//! 超过容忍值（IPv4 默认 2、IPv6 默认 5）即视为多拨，触发封禁；触发后进入「追猎名单」，
//! 在 `keep-hunting-time` 窗口内该子网新出现的 peer 也会被直接封禁。
//!
//! 状态语义对齐上游 guava Cache：
//! - peer 记录 `torrentId@ip`：`expireAfterWrite = cache-lifespan`；
//! - 子网分组 `torrentId@subnet` → `{ip: ts}`：分组 `expireAfterAccess = cache-lifespan`，
//!   组内条目同样按 `cache-lifespan` 过期；
//! - 追猎名单 `torrentId@subnet` → 时间戳：窗口为 `keep-hunting-time`，命中后刷新。
//!
//! 所有时间来自 `CheckContext.now_ms`，因此测试完全确定，不需要真实等待。

use crate::i18n::TranslationComponent;
use crate::iputil::{parse_addr, prefix_block};
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, RuleModule};
use std::collections::HashMap;
use std::sync::Mutex as StdMutex;

/// 模块参数（默认值对齐上游 `profile.yml`）。
#[derive(Clone, Debug)]
pub struct MultiDialingSettings {
    pub ban_duration_ms: i64,
    pub subnet_mask_length: u8,
    pub subnet_mask_v6_length: u8,
    pub tolerate_num_ipv4: i64,
    pub tolerate_num_ipv6: i64,
    pub cache_lifespan_ms: i64,
    pub keep_hunting: bool,
    pub keep_hunting_time_ms: i64,
}

impl Default for MultiDialingSettings {
    fn default() -> Self {
        Self {
            ban_duration_ms: 1_296_000_000,
            subnet_mask_length: 24,
            subnet_mask_v6_length: 56,
            tolerate_num_ipv4: 2,
            tolerate_num_ipv6: 5,
            cache_lifespan_ms: 86_400_000,
            keep_hunting: false,
            keep_hunting_time_ms: 0,
        }
    }
}

#[derive(Default, Debug)]
struct DialState {
    /// `torrentId@ip` -> 最近上报时间
    peers: HashMap<String, i64>,
    /// `torrentId@subnet` -> `{ ip -> 时间戳 }`
    subnets: HashMap<String, HashMap<String, i64>>,
    /// `torrentId@subnet` -> 最近一次追猎时间
    hunting: HashMap<String, i64>,
}

pub struct MultiDialingBlocker {
    pub settings: MultiDialingSettings,
    state: StdMutex<DialState>,
}

impl MultiDialingBlocker {
    pub fn new(settings: MultiDialingSettings) -> Self {
        Self { settings, state: StdMutex::new(DialState::default()) }
    }

    /// 清理过期记录（对齐 guava Cache 的 expireAfterWrite / expireAfterAccess）。
    fn prune(state: &mut DialState, now_ms: i64, lifespan_ms: i64, keep_hunting_time_ms: i64) {
        state.peers.retain(|_, ts| now_ms - *ts < lifespan_ms);
        for group in state.subnets.values_mut() {
            group.retain(|_, ts| now_ms - *ts < lifespan_ms);
        }
        state.subnets.retain(|_, group| !group.is_empty());
        state
            .hunting
            .retain(|_, ts| keep_hunting_time_ms > 0 && now_ms - *ts < keep_hunting_time_ms);
    }
}

impl RuleModule for MultiDialingBlocker {
    fn name(&self) -> &str {
        "Multi Dialing Blocker"
    }
    fn config_name(&self) -> &str {
        "multi-dialing-blocker"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn check(
        &self,
        _downloader_id: &str,
        torrent: &TorrentData,
        peer: &PeerData,
        ctx: &CheckContext,
    ) -> CheckResult {
        let module = self.config_name().to_string();
        if peer.is_handshaking() {
            return CheckResult::handshaking(&module);
        }
        let Some(addr) = parse_addr(&peer.ip) else {
            return CheckResult::pass(&module);
        };
        let s = &self.settings;
        let (prefix_len, tolerate) = if addr.is_ipv4() {
            (s.subnet_mask_length, s.tolerate_num_ipv4)
        } else {
            (s.subnet_mask_v6_length, s.tolerate_num_ipv6)
        };
        let Some(subnet) = prefix_block(&addr.to_string(), prefix_len, prefix_len) else {
            return CheckResult::pass(&module);
        };
        let now = ctx.now_ms;
        let torrent_id = torrent.id();
        let peer_key = format!("{torrent_id}@{}", addr);
        let subnet_key = format!("{torrent_id}@{subnet}");

        let Ok(mut state) = self.state.lock() else {
            return CheckResult::pass(&module);
        };
        Self::prune(&mut state, now, s.cache_lifespan_ms, s.keep_hunting_time_ms);
        state.peers.insert(peer_key, now);
        let group_size = {
            let group = state.subnets.entry(subnet_key.clone()).or_default();
            group.insert(addr.to_string(), now);
            group.len() as i64
        };

        // 子网内不同 IP 数超过容忍值 -> 多拨
        if group_size > tolerate {
            state.hunting.insert(subnet_key.clone(), now);
            return CheckResult::ban(
                &module,
                s.ban_duration_ms,
                "multiDialingDetected",
                &format!("multi-dialing detected in {subnet}"),
                serde_json::json!({ "subnetPeersSize": group_size, "subnet": subnet_key }),
            )
            .with_keys(
                TranslationComponent::new("MDB_MULTI_DIALING_DETECTED"),
                TranslationComponent::with_params(
                    "MODULE_MDB_MULTI_DIALING_DETECTED",
                    vec![subnet.clone().into(), addr.to_string().into()],
                ),
            );
        }

        // 追猎窗口内：同子网新 peer 一并封禁
        if s.keep_hunting && s.keep_hunting_time_ms > 0 {
            if let Some(ts) = state.hunting.get(&subnet_key).copied() {
                if now - ts < s.keep_hunting_time_ms {
                    state.hunting.insert(subnet_key.clone(), now);
                    return CheckResult::ban(
                        &module,
                        s.ban_duration_ms,
                        "multiHunting",
                        &format!("multi-dialing hunting triggered for {subnet}"),
                        serde_json::json!({ "subnetPeersSize": group_size, "subnet": subnet_key }),
                    )
                    .with_keys(
                        TranslationComponent::new("MDB_MULTI_HUNTING"),
                        TranslationComponent::with_params(
                            "MODULE_MDB_MULTI_DIALING_HUNTING_TRIGGERED",
                            vec![subnet.clone().into(), addr.to_string().into()],
                        ),
                    );
                }
                state.hunting.remove(&subnet_key);
            }
        }

        CheckResult::pass(&module)
    }
}
