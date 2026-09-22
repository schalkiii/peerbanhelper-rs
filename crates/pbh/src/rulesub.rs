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
use pbh_db::Database;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::{info, warn};

/// 规则订阅需要运行的模块名。
pub const MODULE_NAME: &str = "ip-address-blocker-rules";

/// 规则订阅刷新类型（对齐上游 `IPBanRuleUpdateType`）：`AUTO` = 定时自动、`MANUAL` = 用户手动触发。
pub const UPDATE_TYPE_AUTO: &str = "AUTO";
pub const UPDATE_TYPE_MANUAL: &str = "MANUAL";

fn cache_path(data_dir: &Path, rule_id: &str) -> PathBuf {
    data_dir.join("sub").join(format!("{rule_id}.txt"))
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().into()
}

/// 拉取并刷新全部规则订阅；返回每个订阅的处理结果描述。
///
/// - `db`：提供时，每个成功加载/更新的订阅会写入 `rule_sub_log`（更新历史）与
///   `rule_sub_info`（当前状态），对齐上游 `RuleSub*Service` 的落库行为；
/// - `update_type`：`AUTO`（定时刷新）/ `MANUAL`（手动刷新）。
pub async fn refresh_all(
    pipeline: &Pipeline,
    config: &IpRuleListConfig,
    data_dir: &Path,
    http: &reqwest::Client,
    db: Option<&Arc<Database>>,
    update_type: &str,
) -> Vec<String> {
    let Some(module) = pipeline.module_as::<IpRuleListModule>(MODULE_NAME) else {
        return Vec::new();
    };
    let mut reports = Vec::new();
    let sub_dir = data_dir.join("sub");

    for (rule_id, rule) in &config.rules {
        let name = if rule.name.trim().is_empty() {
            rule_id.clone()
        } else {
            rule.name.clone()
        };
        let enabled = is_enabled(rule);
        if !enabled {
            module.remove_subscription(rule_id);
            reports.push(format!("{rule_id}: 已禁用"));
            if let Some(db) = db {
                let _ = db.upsert_rule_sub_info(rule_id, false, &name, &rule.url, None, None);
            }
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
                let key = if added {
                    "IP_BAN_RULE_LOAD_SUCCESS"
                } else {
                    "IP_BAN_RULE_UPDATE_SUCCESS"
                };
                reports.push(log_line(key, &name, count));
                record_sub_update(db, rule_id, &name, &rule.url, count, update_type);
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
                record_sub_update(db, rule_id, &name, &rule.url, count, update_type);
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

/// 写入一条规则订阅更新日志与当前状态（对齐上游 `RuleSubLogService` / `RuleSubInfoService`）。
fn record_sub_update(
    db: Option<&Arc<Database>>,
    rule_id: &str,
    name: &str,
    url: &str,
    count: usize,
    update_type: &str,
) {
    let Some(db) = db else { return };
    let now = chrono::Utc::now().timestamp_millis();
    if let Err(e) = db.upsert_rule_sub_info(rule_id, true, name, url, Some(now), Some(count as i64))
    {
        warn!("规则订阅状态写入失败（{rule_id}）: {e}");
    }
    if let Err(e) = db.insert_rule_sub_log(rule_id, count, update_type, now) {
        warn!("规则订阅日志写入失败（{rule_id}）: {e}");
    }
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
                warn!(
                    "{} (HTTP {status})",
                    log_line("IP_BAN_RULE_UPDATE_FAILED", name, 0)
                );
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
    let mut line = translator.render(
        &TranslationComponent::with_params(key, vec![name.into()]),
        "zh_cn",
    );
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
        let text: Vec<String> = summary
            .iter()
            .map(|(id, n)| format!("{id}={n} 条"))
            .collect();
        info!("IP 黑名单规则订阅：{}", text.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbh_core::modules::ip_rule_list::IpRuleListModule;
    use pbh_db::Database;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pbh-rulesub-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 把内容写入 `<data>/sub/<id>.txt` 缓存（模拟上一个成功拉取的结果）。
    fn write_cache(dir: &Path, id: &str, content: &str) {
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join(format!("{id}.txt")), content).unwrap();
    }

    #[test]
    fn refresh_all_writes_rule_sub_log_and_info_on_cache_parse() {
        let dir = temp_dir();
        // 远程不可用（url 为空）但本地缓存存在且未加载 ⇒ ParseCache，应落库
        write_cache(&dir, "rules-1", "1.2.3.4\n5.6.7.8\n");
        let config = IpRuleListConfig {
            enabled: Some(true),
            ban_duration_ms: 0,
            check_interval_ms: 14_400_000,
            preload_banlist: false,
            rules: {
                let mut m = BTreeMap::new();
                m.insert(
                    "rules-1".to_string(),
                    IpRuleSubscriptionConfig {
                        enabled: true,
                        name: "Rules One".to_string(),
                        // 非空 URL 才能让 is_enabled 成立；使用不可达地址，fetch 立即失败回退缓存
                        url: "http://127.0.0.1:1/rules.txt".to_string(),
                    },
                );
                m
            },
        };
        let db = Arc::new(Database::open_in_memory().unwrap());
        let http = reqwest::Client::new();
        let mut pipeline = Pipeline::default();
        pipeline.add_module(Box::new(IpRuleListModule::new(0)));
        let pipeline = Arc::new(pipeline);

        let reports = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(refresh_all(
                &pipeline,
                &config,
                &dir,
                &http,
                Some(&db),
                UPDATE_TYPE_MANUAL,
            ));
        assert!(
            reports.iter().any(|r| r.contains("Rules One")),
            "{reports:?}"
        );

        // rule_sub_log 记录一条更新历史
        let logs = db.list_rule_sub_log(None, 10, 0).unwrap();
        assert_eq!(logs.len(), 1, "应写入一条订阅更新日志");
        assert_eq!(logs[0].rule_id, "rules-1");
        assert_eq!(logs[0].count, 2, "缓存含 2 条规则");
        assert_eq!(logs[0].update_type, UPDATE_TYPE_MANUAL);

        // rule_sub_info 反映当前状态
        let info = db
            .get_rule_sub_info("rules-1")
            .unwrap()
            .expect("应写入 rule_sub_info");
        assert!(info.enabled);
        assert_eq!(info.rule_name, "Rules One");
        assert_eq!(info.ent_count, Some(2));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_all_skips_disabled_rule_in_db() {
        let dir = temp_dir();
        let config = IpRuleListConfig {
            enabled: Some(true),
            ban_duration_ms: 0,
            check_interval_ms: 14_400_000,
            preload_banlist: false,
            rules: {
                let mut m = BTreeMap::new();
                m.insert(
                    "disabled-1".to_string(),
                    IpRuleSubscriptionConfig {
                        enabled: false,
                        name: "Off".to_string(),
                        url: String::new(),
                    },
                );
                m
            },
        };
        let db = Arc::new(Database::open_in_memory().unwrap());
        let http = reqwest::Client::new();
        let mut pipeline = Pipeline::default();
        pipeline.add_module(Box::new(IpRuleListModule::new(0)));
        let pipeline = Arc::new(pipeline);

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(refresh_all(
                &pipeline,
                &config,
                &dir,
                &http,
                Some(&db),
                UPDATE_TYPE_AUTO,
            ));

        // 禁用的订阅只更新 rule_sub_info（enabled=0），不写更新日志
        assert_eq!(db.count_rule_sub_log(None).unwrap(), 0);
        let info = db
            .get_rule_sub_info("disabled-1")
            .unwrap()
            .expect("应写入 rule_sub_info");
        assert!(!info.enabled);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
