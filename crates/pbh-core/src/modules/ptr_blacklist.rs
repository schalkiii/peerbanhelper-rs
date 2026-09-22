//! PTR 黑名单（`ptr-blacklist`），忠实复刻上游 `PTRBlacklist`。
//!
//! 判定：把 peer IP 转成反向 DNS 名（`4.3.2.1.in-addr.arpa`）→ 取 PTR 记录 →
//! 用规则集匹配 PTR 主机名，命中即封禁。
//!
//! 与上游的差异（见 SPEC §5.9）：上游在模块内**同步**做 DNS 查询（3 秒超时），
//! 本实现把查询放到应用层的后台预热任务里，模块只读取 `lookup` 回调给出的结果；
//! 查不到（等价上游超时/无记录）即 `pass()`，判定语义一致。

use crate::i18n::TranslationComponent;
use crate::iputil::reverse_dns_name;
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, RuleModule};
use crate::rule::RuleSet;
use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};

/// PTR 查询结果缓存：应用层后台预热，模块只读（对齐上游「查不到就放行」的语义）。
#[derive(Debug, Default)]
pub struct PtrCache {
    entries: StdMutex<HashMap<String, Option<String>>>,
}

impl PtrCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次查询结果（`None` 表示无 PTR 记录）。
    pub fn insert(&self, reverse_name: impl Into<String>, value: Option<String>) {
        if let Ok(mut map) = self.entries.lock() {
            map.insert(reverse_name.into(), value);
        }
    }

    /// 查询 PTR 名；未预热或无记录均返回 `None`。
    pub fn get(&self, reverse_name: &str) -> Option<String> {
        self.entries
            .lock()
            .ok()
            .and_then(|map| map.get(reverse_name).cloned())
            .flatten()
    }

    pub fn is_resolved(&self, reverse_name: &str) -> bool {
        self.entries
            .lock()
            .map(|map| map.contains_key(reverse_name))
            .unwrap_or(false)
    }

    pub fn len(&self) -> usize {
        self.entries.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct PtrBlacklist {
    pub rules: RuleSet,
    /// 0 表示使用全局封禁时长
    pub ban_duration_ms: i64,
    /// PTR 结果缓存（由应用层预热）
    pub cache: Arc<PtrCache>,
}

impl PtrBlacklist {
    pub fn new(rules: RuleSet, ban_duration_ms: i64, cache: Arc<PtrCache>) -> Self {
        Self {
            rules,
            ban_duration_ms,
            cache,
        }
    }
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
