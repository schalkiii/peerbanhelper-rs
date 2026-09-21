//! IP 黑名单规则订阅的拉取与刷新（对齐上游 `IPBlackRuleList.updateRule` / `getResource`）。
//!
//! - 远程内容缓存在 `<data>/sub/<ruleId>.txt`；
//! - 通过 sha256 比对判断是否需要刷新（相同且已加载时不重复解析）；
//! - 远程不可用时回退到本地缓存文件；
//! - 规则被禁用时从模块中移除对应订阅。

use pbh_core::config::{IpRuleListConfig, IpRuleSubscriptionConfig};
use pbh_core::i18n::TranslationComponent;
use pbh_core::modules::ip_rule_list::{
    parse_rule_list, plan_rule_update, IpRuleListModule, RuleSubscription, RuleUpdatePlan,
};
use pbh_core::pipeline::Pipeline;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tracing::{info, warn};

/// 规则订阅需要运行的模块名。
pub const MODULE_NAME: &str = "ip-address-blocker-rules";

fn cache_path(data_dir: &Path, rule_id: &str) -> PathBuf {
    data_dir.join("sub").join(format!("{rule_id}.txt"))
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// 拉取并刷新全部规则订阅；返回每个订阅的处理结果描述。
pub async fn refresh_all(
    pipeline: &Pipeline,
    config: &IpRuleListConfig,
    data_dir: &Path,
    http: &reqwest::Client,
) -> Vec<String> {
    let Some(module) = pipeline.module_as::<IpRuleListModule>(MODULE_NAME) else {
        return Vec::new();
    };
    let mut reports = Vec::new();
    let sub_dir = data_dir.join("sub");

    for (rule_id, rule) in &config.rules {
        let name = if rule.name.trim().is_empty() { rule_id.clone() } else { rule.name.clone() };
        if !is_enabled(rule) {
            module.remove_subscription(rule_id);
            reports.push(format!("{rule_id}: 已禁用"));
            continue;
        }
        let path = cache_path(data_dir, rule_id);
        let cached = std::fs::read(&path).ok();
        let remote = fetch_remote(http, rule, &name).await;
        let differs = match (&remote, &cached) {
            (Some(remote), Some(cached)) => sha256(remote) != sha256(cached),
            (Some(_), None) => true,
            _ => false,
        };
        let already_loaded = module.has_subscription(rule_id);

        match plan_rule_update(remote.is_some(), differs, cached.is_some(), already_loaded) {
            RuleUpdatePlan::ParseRemote { write_cache } => {
                let bytes = remote.expect("remote bytes");
                let text = String::from_utf8_lossy(&bytes).to_string();
                let (entries, count) = parse_rule_list(&text);
                if count == 0 {
                    reports.push(format!("{name}: 无有效规则"));
                    continue;
                }
                let added = module.set_subscription(RuleSubscription {
                    rule_id: rule_id.clone(),
                    name: name.clone(),
                    entries,
                });
                if write_cache {
                    if let Err(e) = std::fs::create_dir_all(&sub_dir) {
                        warn!("创建规则订阅缓存目录失败: {e}");
                    } else if let Err(e) = std::fs::write(&path, &bytes) {
                        warn!("写入规则订阅缓存失败: {e}");
                    }
                }
                let key = if added { "IP_BAN_RULE_LOAD_SUCCESS" } else { "IP_BAN_RULE_UPDATE_SUCCESS" };
                reports.push(log_line(key, &name, count));
            }
            RuleUpdatePlan::ParseCache => {
                let bytes = cached.expect("cached bytes");
                let text = String::from_utf8_lossy(&bytes).to_string();
                let (entries, count) = parse_rule_list(&text);
                if count == 0 {
                    reports.push(log_line("IP_BAN_RULE_LOAD_FAILED", &name, 0));
                    continue;
                }
                module.set_subscription(RuleSubscription {
                    rule_id: rule_id.clone(),
                    name: name.clone(),
                    entries,
                });
                reports.push(log_line("IP_BAN_RULE_USE_CACHE", &name, count));
            }
            RuleUpdatePlan::NoAction => {
                if remote.is_some() {
                    reports.push(log_line("IP_BAN_RULE_NO_UPDATE", &name, 0));
                }
            }
        }
    }
    reports
}

fn is_enabled(rule: &IpRuleSubscriptionConfig) -> bool {
    rule.enabled && !rule.url.trim().is_empty()
}

async fn fetch_remote(
    http: &reqwest::Client,
    rule: &IpRuleSubscriptionConfig,
    name: &str,
) -> Option<Vec<u8>> {
    let url = rule.url.trim();
    if url.is_empty() {
        warn!("{}", log_line("IP_BAN_RULE_UPDATE_FAILED", name, 0));
        return None;
    }
    match http.get(url).send().await {
        Ok(resp) => {
            let status = resp.status();
            // 上游未检查状态码；这里对非 2xx 视为订阅失败（避免把错误页当作规则解析）
            if !status.is_success() {
                warn!("{} (HTTP {status})", log_line("IP_BAN_RULE_UPDATE_FAILED", name, 0));
                return None;
            }
            match resp.bytes().await {
                Ok(bytes) => Some(bytes.to_vec()),
                Err(e) => {
                    warn!("{} ({e})", log_line("IP_BAN_RULE_UPDATE_FAILED", name, 0));
                    None
                }
            }
        }
        Err(e) => {
            warn!("{} ({e})", log_line("IP_BAN_RULE_UPDATE_FAILED", name, 0));
            None
        }
    }
}

/// 用内嵌文案表渲染一行日志（与上游 `tlUI(Lang...)` 一致）。
fn log_line(key: &str, name: &str, count: usize) -> String {
    let translator = pbh_core::i18n::Translator::embedded();
    let mut line =
        translator.render(&TranslationComponent::with_params(key, vec![name.into()]), "zh_cn");
    if count > 0 && key == "IP_BAN_RULE_UPDATE_SUCCESS" {
        line.push_str(&format!("（{count} 条）"));
    }
    line
}

/// 供调度器记录：模块内当前订阅与条目数。
pub fn subscription_summary(pipeline: &Pipeline) -> Vec<(String, usize)> {
    pipeline
        .module_as::<IpRuleListModule>(MODULE_NAME)
        .map(|m| m.entry_counts())
        .unwrap_or_default()
}

/// 触发一次日志（启动时输出一次订阅概览）。
pub fn log_summary(pipeline: &Pipeline) {
    let summary = subscription_summary(pipeline);
    if summary.is_empty() {
        info!("IP 黑名单规则订阅：未加载任何规则");
    } else {
        let text: Vec<String> = summary.iter().map(|(id, n)| format!("{id}={n} 条")).collect();
        info!("IP 黑名单规则订阅：{}", text.join(", "));
    }
}
