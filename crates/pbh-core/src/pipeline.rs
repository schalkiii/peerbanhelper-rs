//! ban wave 判定流水线：bypass 过滤 + 顺序执行模块，首个决定性结果生效。

use crate::defaults::DEFAULT_BAN_DURATION_MS;
use crate::i18n::TranslationComponent;
use crate::iputil::IpSet;
use crate::model::{PeerData, TorrentData};
use crate::module::{CheckContext, CheckResult, PeerAction, RuleModule};
use std::sync::{Arc, Mutex as StdMutex};

pub struct Pipeline {
    pub modules: Vec<Box<dyn RuleModule>>,
    /// 命中即跳过所有检查的地址（bypass）
    pub ignore: IpSet,
    /// 模块 ban-duration 为 0 时使用的全局封禁时长
    pub global_ban_duration_ms: i64,
    /// 内存封禁表：wave 维护，`auto-range-ban` 读取（对齐上游注入的 `BanList`）
    pub ban_list: Arc<StdMutex<crate::banlist::BanList>>,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self {
            modules: Vec::new(),
            ignore: IpSet::from_cidrs(crate::defaults::DEFAULT_IGNORE_ADDRESSES.iter().copied()),
            global_ban_duration_ms: DEFAULT_BAN_DURATION_MS,
            ban_list: Arc::new(StdMutex::new(crate::banlist::BanList::new())),
        }
    }
}

/// 对单个 peer 的最终决策
#[derive(Clone, Debug)]
pub enum Decision {
    /// 不处理
    None,
    /// 跳过（bypass 等）
    Skip(CheckResult),
    /// 封禁
    Ban(CheckResult),
}

impl Pipeline {
    pub fn add_module(&mut self, m: Box<dyn RuleModule>) {
        self.modules.push(m);
    }

    /// 按配置名取回具体类型的模块（例如 PCB 的持久化接口）。
    pub fn module_as<T: 'static>(&self, config_name: &str) -> Option<&T> {
        self.modules
            .iter()
            .find(|m| m.config_name() == config_name)
            .and_then(|m| m.as_any().downcast_ref::<T>())
    }

    pub fn evaluate(
        &self,
        downloader_id: &str,
        torrent: &TorrentData,
        peer: &PeerData,
        ctx: &CheckContext,
    ) -> Decision {
        // bypass：对齐 RunCheckModuleOrgan.checkIfPossibleBadConfig
        // （Java 在此直接返回 SKIP 结果，不再执行任何规则模块）
        if !self.ignore.is_empty() && self.ignore.contains(&peer.ip) {
            return Decision::Skip(CheckResult {
                module: "general".to_string(),
                action: PeerAction::Skip,
                ban_duration_ms: 0,
                rule: "general-rule-ignored-address".to_string(),
                reason: "general-reason-skip-ignored-peers".to_string(),
                data: serde_json::json!({ "type": "ignoredAddresses" }),
                // 上游此处直接用字面量作为 key（文案表中不存在，渲染即原样输出）
                rule_key: Some(TranslationComponent::new("general-rule-ignored-address")),
                reason_key: Some(TranslationComponent::new(
                    "general-reason-skip-ignored-peers",
                )),
            });
        }

        // 所有模块都要执行；结果按 Java `DigestionSession.extractFromLastOrgan` 聚合：
        //   - PeerAction 等级更高者胜（SKIP > BAN > BAN_FOR_DISCONNECT > NO_ACTION）
        //   - 等级相同时取更长的 ban 时长
        //   - 完全并列时保留模块注册顺序中更早者（Java 因并发执行而不确定，此处取确定性实现）
        let mut best: Option<CheckResult> = None;
        for module in &self.modules {
            let result = module.check(downloader_id, torrent, peer, ctx);
            let replace = match &best {
                None => true,
                Some(current) => {
                    let (new_rank, cur_rank) = (result.action.ordinal(), current.action.ordinal());
                    new_rank > cur_rank
                        || (new_rank == cur_rank
                            && result.ban_duration_ms > current.ban_duration_ms)
                }
            };
            if replace {
                best = Some(result);
            }
        }

        match best {
            None => Decision::None,
            Some(mut result) => match result.action {
                PeerAction::Skip => Decision::Skip(result),
                PeerAction::NoAction => Decision::None,
                PeerAction::Ban | PeerAction::BanForDisconnect => {
                    if result.ban_duration_ms == 0 {
                        result.ban_duration_ms = self.global_ban_duration_ms;
                    }
                    Decision::Ban(result)
                }
            },
        }
    }
}
