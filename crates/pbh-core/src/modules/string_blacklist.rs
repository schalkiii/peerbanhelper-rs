//! 基于规则集的字符串黑名单通用逻辑（PeerId / ClientName 共用）。

use crate::defaults::BLACKLIST_BAN_DURATION_MS;
use crate::i18n::TranslationComponent;
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, RuleModule};
use crate::rule::RuleSet;

/// 对 peer 某个字符串字段应用规则集；命中即 BAN。
pub struct StringBlacklist {
    pub display_name: String,
    pub config_name: String,
    pub rules: RuleSet,
    pub ban_duration_ms: i64,
    /// 取待匹配字段（如 peer_id / client_name）
    pub field: fn(&PeerData) -> Option<String>,
    /// 命中后 data.type
    pub data_type: &'static str,
}

impl StringBlacklist {
    /// 默认 PeerId 黑名单（内置上游 `profile.yml` 默认规则集）。
    pub fn peer_id() -> Self {
        let rules = RuleSet::from_json_text(
            &crate::defaults::DEFAULT_BANNED_PEER_ID
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
        .expect("default peer id rules parse");
        Self::peer_id_with(rules, BLACKLIST_BAN_DURATION_MS)
    }

    /// 使用外部传入的规则集与封禁时长构建 PeerId 黑名单（`profile.yml` 驱动）。
    pub fn peer_id_with(rules: RuleSet, ban_duration_ms: i64) -> Self {
        Self {
            display_name: "PeerId Blacklist".to_string(),
            config_name: "peer-id-blacklist".to_string(),
            rules,
            ban_duration_ms,
            field: |p| p.peer_id.clone(),
            data_type: "peerId",
        }
    }

    /// 默认 ClientName 黑名单（内置上游 `profile.yml` 默认规则集）。
    pub fn client_name() -> Self {
        let rules = RuleSet::from_json_text(
            &crate::defaults::DEFAULT_BANNED_CLIENT_NAME
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
        .expect("default client name rules parse");
        Self::client_name_with(rules, BLACKLIST_BAN_DURATION_MS)
    }

    /// 使用外部传入的规则集与封禁时长构建 ClientName 黑名单（`profile.yml` 驱动）。
    pub fn client_name_with(rules: RuleSet, ban_duration_ms: i64) -> Self {
        Self {
            display_name: "ClientName Blacklist".to_string(),
            config_name: "client-name-blacklist".to_string(),
            rules,
            ban_duration_ms,
            field: |p| p.client_name.clone(),
            data_type: "clientName",
        }
    }
}

impl RuleModule for StringBlacklist {
    fn name(&self) -> &str {
        &self.display_name
    }
    fn config_name(&self) -> &str {
        &self.config_name
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
        let module = self.config_name.clone();
        let value = (self.field)(peer);
        // 对齐 Java PeerIdBlacklist/ClientNameBlacklist：
        //   if (isHandShaking(peer) && (id == null || id.isBlank())) return handshaking();
        // 注意「与」条件：已携带标识的握手中 peer 仍要参与规则判定。
        let blank = value.as_deref().is_none_or(|s| s.trim().is_empty());
        if peer.is_handshaking() && blank {
            return CheckResult::handshaking(&module);
        }
        let result = self.rules.r#match(value.as_deref());
        if result.hit {
            // 对齐上游：rule = 命中规则的 matcherName()，reason = MODULE_CNB_MATCH_CLIENT_NAME(com comment)
            let matched = self
                .rules
                .rules
                .get(result.index.max(0) as usize)
                .map(|m| m.name_component())
                .unwrap_or_else(|| TranslationComponent::new(""));
            let reason_key = TranslationComponent::with_params(
                "MODULE_CNB_MATCH_CLIENT_NAME",
                vec![matched.clone().into()],
            );
            CheckResult::ban(
                &module,
                self.ban_duration_ms,
                "rule",
                &format!(
                    "matched {self_data_type}: {value}",
                    self_data_type = self.data_type,
                    value = value.as_deref().unwrap_or("")
                ),
                serde_json::json!({ "type": self.data_type, "rule": value, "index": result.index }),
            )
            .with_keys(matched, reason_key)
        } else {
            // Java 未命中即返回 pass()（OK_CHECK_RESULT）
            CheckResult::pass(&module)
        }
    }
}
