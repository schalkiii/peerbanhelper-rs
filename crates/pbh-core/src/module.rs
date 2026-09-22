//! 规则模块 trait 与判定结果（SPEC 第 4.3、5 节）。

use crate::i18n::TranslationComponent;
use crate::model::{PeerData, TorrentData};
use serde::{Deserialize, Serialize};

/// 对齐上游 PeerAction
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerAction {
    Ban,
    /// 短暂封禁以断开连接（快速 PCB 测试）
    BanForDisconnect,
    /// 跳过该 peer（如命中忽略地址）
    Skip,
    NoAction,
}

impl PeerAction {
    /// 对齐 Java `PeerAction` 的枚举序（聚合时按此等级择优）：
    /// `NO_ACTION(0) < BAN_FOR_DISCONNECT(1) < BAN(2) < SKIP(3)`。
    pub fn ordinal(&self) -> u8 {
        match self {
            PeerAction::NoAction => 0,
            PeerAction::BanForDisconnect => 1,
            PeerAction::Ban => 2,
            PeerAction::Skip => 3,
        }
    }

    /// 是否需要下发给下载器封禁（Java 中 `BAN` 与 `BAN_FOR_DISCONNECT` 都会封禁）。
    pub fn is_ban(&self) -> bool {
        matches!(self, PeerAction::Ban | PeerAction::BanForDisconnect)
    }

    /// 对齐 `PeerAction.excludeFromReport`（仅 `BAN_FOR_DISCONNECT` 为 true）：
    /// 该类封禁不写入 `history`（`PersistMetrics.recordPeerBan` 直接 return）。
    pub fn exclude_from_report(&self) -> bool {
        matches!(self, PeerAction::BanForDisconnect)
    }

    /// 对齐 `PeerAction.excludeFromDisplay`（仅 `BAN_FOR_DISCONNECT` 为 true）：
    /// WebUI 的 `/api/bans` 默认过滤这类条目。
    pub fn exclude_from_display(&self) -> bool {
        matches!(self, PeerAction::BanForDisconnect)
    }
}

/// 未收录模块的占位类名（调用方可据此回退为 configName）。
pub const UNKNOWN_MODULE_CLASS: &str = "com.ghostchu.peerbanhelper.module.UnknownModule";

/// 模块 `configName` → 上游 Java 类全名。
///
/// 上游 `CheckResult.moduleContext` 是 `Class<?>`，落库到 `history.module_name`、
/// 写进 `banlist.metadata.context`，并原样上报给 BTN（`BtnBan.module`）；
/// WebUI 的 `/api/metadata/manifest` 也用同一类名作为模块开关的唯一键。
/// 本移植的模块实现没有 Java 类，这里按上游包路径给出等价类名。
///
/// 注意：`peer-name-black-rule-list` 在 v9.5.1 整文件被注释（模块已停用），故不在此表。
pub fn java_module_class(config_name: &str) -> &'static str {
    match config_name {
        "ip-address-blocker" => concat!(
            "com.ghostchu.peerbanhelper.module.impl.rule.",
            "IPBlackList"
        ),
        "peer-id-blacklist" => concat!(
            "com.ghostchu.peerbanhelper.module.impl.rule.",
            "PeerIdBlacklist"
        ),
        "client-name-blacklist" => {
            concat!(
                "com.ghostchu.peerbanhelper.module.impl.rule.",
                "ClientNameBlacklist"
            )
        }
        "expression-engine" | "expression-rule" => {
            concat!(
                "com.ghostchu.peerbanhelper.module.impl.rule.",
                "ExpressionRule"
            )
        }
        "progress-cheat-blocker" => {
            concat!(
                "com.ghostchu.peerbanhelper.module.impl.rule.",
                "ProgressCheatBlocker"
            )
        }
        "multi-dialing-blocker" => {
            concat!(
                "com.ghostchu.peerbanhelper.module.impl.rule.",
                "MultiDialingBlocker"
            )
        }
        "auto-range-ban" => concat!(
            "com.ghostchu.peerbanhelper.module.impl.rule.",
            "AutoRangeBan"
        ),
        "btn" => concat!(
            "com.ghostchu.peerbanhelper.module.impl.rule.",
            "BtnNetworkOnline"
        ),
        "ip-address-blocker-rules" => {
            concat!(
                "com.ghostchu.peerbanhelper.module.impl.rule.",
                "IPBlackRuleList"
            )
        }
        "anti-vampire" => concat!(
            "com.ghostchu.peerbanhelper.module.impl.rule.",
            "AntiVampire"
        ),
        "ptr-blacklist" => concat!(
            "com.ghostchu.peerbanhelper.module.impl.rule.",
            "PTRBlacklist"
        ),
        "idle-connection-dos-protection" => {
            concat!(
                "com.ghostchu.peerbanhelper.module.impl.rule.",
                "IdleConnectionDosProtection"
            )
        }
        // 上游已删除的旧模块名（WebUI manifest 兼容保留）
        "peer-blacklist" => concat!(
            "com.ghostchu.peerbanhelper.module.impl.rule.",
            "PeerBlacklist"
        ),
        _ => UNKNOWN_MODULE_CLASS,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckResult {
    /// 模块 configName
    pub module: String,
    pub action: PeerAction,
    /// 封禁时长 ms；0 表示使用全局默认
    pub ban_duration_ms: i64,
    pub rule: String,
    pub reason: String,
    /// 结构化数据（对齐 StructuredData），至少包含 status/type
    pub data: serde_json::Value,
    /// 上游 `CheckResult.rule` 的可翻译形式（`Lang` 键 + 参数）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_key: Option<TranslationComponent>,
    /// 上游 `CheckResult.reason` 的可翻译形式（`Lang` 键 + 参数）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_key: Option<TranslationComponent>,
}

impl CheckResult {
    pub fn no_action(module: &str, status: &str) -> Self {
        CheckResult {
            module: module.to_string(),
            action: PeerAction::NoAction,
            ban_duration_ms: 0,
            rule: "N/A".to_string(),
            reason: status.to_string(),
            data: serde_json::json!({ "status": status }),
            // 上游 OK/HANDSHAKING 常量的 rule/reason 就是字面量 key（查表失败即原样输出）
            rule_key: Some(TranslationComponent::new("N/A")),
            reason_key: Some(TranslationComponent::new(status)),
        }
    }

    pub fn handshaking(module: &str) -> Self {
        // 上游 HANDSHAKING_CHECK_RESULT 的 reason 为字面量 "Peer handshaking"
        let mut result = Self::no_action(module, "handshaking");
        result.reason_key = Some(TranslationComponent::new("Peer handshaking"));
        result
    }

    pub fn pass(module: &str) -> Self {
        // 上游 OK_CHECK_RESULT 的 reason 为字面量 "Check passed"
        let mut result = Self::no_action(module, "pass");
        result.reason_key = Some(TranslationComponent::new("Check passed"));
        result
    }

    pub fn ban(
        module: &str,
        duration_ms: i64,
        rule: &str,
        reason: &str,
        data: serde_json::Value,
    ) -> Self {
        CheckResult {
            module: module.to_string(),
            action: PeerAction::Ban,
            ban_duration_ms: duration_ms,
            rule: rule.to_string(),
            reason: reason.to_string(),
            data,
            rule_key: None,
            reason_key: None,
        }
    }

    /// 设置上游对应的可翻译规则名与原因。
    pub fn with_keys(
        mut self,
        rule_key: TranslationComponent,
        reason_key: TranslationComponent,
    ) -> Self {
        self.rule_key = Some(rule_key);
        self.reason_key = Some(reason_key);
        self
    }
}

/// 每轮判定的上下文（确定性时间与下载器特性）。
#[derive(Clone, Debug, Default)]
pub struct CheckContext {
    /// 当前时间（毫秒），测试中可固定
    pub now_ms: i64,
    /// 下载器特性标志（如 "UNBAN_IP"）
    pub features: Vec<String>,
}

impl CheckContext {
    pub fn has_feature(&self, f: &str) -> bool {
        self.features.iter().any(|x| x == f)
    }
}

/// 规则模块统一接口。
pub trait RuleModule: Send + Sync {
    /// 展示名（如 "Peer ID Blacklist"）
    fn name(&self) -> &str;
    /// 配置键名（如 "peer-id-blacklist"）
    fn config_name(&self) -> &str;
    /// 对单个 peer 做判定。
    fn check(
        &self,
        downloader_id: &str,
        torrent: &TorrentData,
        peer: &PeerData,
        ctx: &CheckContext,
    ) -> CheckResult;
    /// 向上转型，供流水线按类型取回具体模块（如 PCB 的持久化接口）。
    fn as_any(&self) -> &dyn std::any::Any;
}
