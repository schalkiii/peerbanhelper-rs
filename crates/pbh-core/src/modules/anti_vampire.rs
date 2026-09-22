//! 反吸血（`anti-vampire`），忠实复刻上游 `AntiVampire` 的迅雷预设。
//!
//! 策略（上游注释）：
//! - 做种状态下禁止所有版本迅雷客户端（任何版本迅雷都不做种）；
//! - 下载状态下仅放行迅雷 0019 客户端（它会正常参与 swarm 分享）；
//! - 依据 `peer_id` 前缀 `-xl` / `-xl0019` 与 `client` 前缀 `xunlei` / 包含 `0019`|`0.0.1.9` 识别。
//!
//! 注意：本模块**不做**握手判定（上游直接进入预设检查）。

use crate::i18n::TranslationComponent;
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, RuleModule};

/// 模块参数（默认值对齐上游 `profile.yml`）。
#[derive(Clone, Debug)]
pub struct AntiVampireSettings {
    pub ban_duration_ms: i64,
    /// `presets.xunlei.enabled`
    pub xunlei_preset: bool,
}

impl Default for AntiVampireSettings {
    fn default() -> Self {
        Self {
            ban_duration_ms: 14_400_000,
            xunlei_preset: true,
        }
    }
}

pub struct AntiVampire {
    pub settings: AntiVampireSettings,
}

impl AntiVampire {
    pub fn new(settings: AntiVampireSettings) -> Self {
        Self { settings }
    }
}

impl RuleModule for AntiVampire {
    fn name(&self) -> &str {
        "Anti Vampire"
    }
    fn config_name(&self) -> &str {
        "anti-vampire"
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn check(
        &self,
        _downloader_id: &str,
        torrent: &TorrentData,
        peer: &PeerData,
        _ctx: &CheckContext,
    ) -> CheckResult {
        let module = self.config_name().to_string();
        if !self.settings.xunlei_preset {
            return CheckResult::pass(&module);
        }
        self.check_xunlei_preset(&module, torrent, peer)
    }
}

impl AntiVampire {
    fn check_xunlei_preset(
        &self,
        module: &str,
        torrent: &TorrentData,
        peer: &PeerData,
    ) -> CheckResult {
        let mut is_xunlei = false;
        let mut is_xunlei_0019 = false;
        if let Some(peer_id) = &peer.peer_id {
            let pid = peer_id.to_lowercase();
            if pid.starts_with("-xl") {
                is_xunlei = true;
                if pid.starts_with("-xl0019") {
                    is_xunlei_0019 = true;
                }
            }
        }
        if let Some(client_name) = &peer.client_name {
            let client = client_name.to_lowercase();
            if client.starts_with("xunlei") {
                is_xunlei = true;
                if client.contains("0019") || client.contains("0.0.1.9") {
                    is_xunlei_0019 = true;
                }
            }
        }
        if !is_xunlei {
            return CheckResult::pass(module);
        }

        let seeding = torrent.is_seeding();
        if !is_xunlei_0019 {
            return CheckResult::ban(
                module,
                self.settings.ban_duration_ms,
                "antiVampire",
                &format!("xunlei (non-0019) blocked, seeding={seeding}"),
                serde_json::json!({ "xunleiType": "non-0019", "seeding": seeding }),
            )
            .with_keys(
                TranslationComponent::new("MODULE_ANTI_VAMPIRE_TITLE"),
                TranslationComponent::new("MODULE_ANTI_VAMPIRE_DESCRIPTION_XUNLEI_NON_0019"),
            );
        }
        // 是迅雷且为 0019：不允许连接做种任务
        if seeding {
            return CheckResult::ban(
                module,
                self.settings.ban_duration_ms,
                "antiVampire",
                "xunlei 0019 is not allowed to connect seeding torrents",
                serde_json::json!({ "xunleiType": "0019", "seeding": seeding }),
            )
            .with_keys(
                TranslationComponent::new("MODULE_ANTI_VAMPIRE_TITLE"),
                TranslationComponent::new("MODULE_ANTI_VAMPIRE_DESCRIPTION_XUNLEI_0019_SEEDING"),
            );
        }
        CheckResult::pass(module)
    }
}
