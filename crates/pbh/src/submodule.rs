//! 规则订阅的 Web 后端（`SubModule` trait 实现，对齐上游 `RuleSubController`）。
//!
//! 上游 `RuleSubController` 直接读写 `config.yml` 的 `module.ip-address-blocker-rules` 段，
//! 并通过 `RuleSub*Service` 把更新历史落库到 `rule_sub_log` / `rule_sub_info`。
//! 本实现持有共享的规则配置（运行时可被 Web 增删改并回写），刷新时复用 [`crate::rulesub`]
//! 的拉取逻辑（落库由 `refresh_all` 内部完成）。

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use pbh_core::config::{IpRuleListConfig, IpRuleSubscriptionConfig};
use pbh_core::pipeline::Pipeline;
use pbh_db::Database;
use pbh_web::SubModule;
use serde_json::{json, Value};

use crate::config::AppConfig;
use crate::rulesub::{refresh_all, UPDATE_TYPE_MANUAL};

/// 规则订阅 Web 后端。
pub struct RuleSubBackend {
    pipeline: Arc<Pipeline>,
    db: Arc<Database>,
    data_dir: PathBuf,
    http: Arc<reqwest::Client>,
    /// 配置文件路径（`<data>/config/config.yml` 或 `<data>/config.yml`），用于回写规则段。
    config_path: PathBuf,
    /// 运行时共享的规则订阅配置（Web 增删改 → 回写文件）。
    rules: Arc<RwLock<IpRuleListConfig>>,
}

impl RuleSubBackend {
    pub fn new(
        pipeline: Arc<Pipeline>,
        db: Arc<Database>,
        data_dir: PathBuf,
        http: Arc<reqwest::Client>,
        config_path: PathBuf,
        rules: IpRuleListConfig,
    ) -> Arc<Self> {
        Arc::new(Self {
            pipeline,
            db,
            data_dir,
            http,
            config_path,
            rules: Arc::new(RwLock::new(rules)),
        })
    }

    /// 把当前内存规则配置写回 `config.yml` 的 `module.ip-address-blocker-rules` 段。
    fn save(&self, rules: &IpRuleListConfig) -> Result<(), String> {
        let parent = self
            .config_path
            .parent()
            .ok_or_else(|| "config 路径缺少父目录".to_string())?;
        let (mut cfg, path) =
            AppConfig::load_or_create(parent).map_err(|e| e.to_string())?;
        cfg.profile.module.ip_rule_list = Some(rules.clone());
        cfg.save_to(&path).map_err(|e| e.to_string())?;
        Ok(())
    }

    /// 同步执行一次刷新（手动触发），返回处理摘要。
    fn run_refresh(&self, rule_id: Option<&str>) -> String {
        let cfg = {
            let guard = self.rules.read().unwrap();
            let mut cfg = guard.clone();
            if let Some(id) = rule_id {
                cfg.rules.retain(|k, _| k == id);
            }
            cfg
        };
        let pipeline = self.pipeline.clone();
        let data_dir = self.data_dir.clone();
        let http = self.http.clone();
        let db = self.db.clone();
        // Web 在 tokio 运行时内运行；用当前运行时句柄驱动异步拉取（含网络 IO）。
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async move {
                refresh_all(
                    &pipeline,
                    &cfg,
                    &data_dir,
                    &http,
                    Some(&db),
                    UPDATE_TYPE_MANUAL,
                )
                .await
            })
        });
        result.join("\n")
    }
}

impl SubModule for RuleSubBackend {
    fn list_rules(&self) -> Vec<Value> {
        let rules = self.rules.read().unwrap();
        let counts: std::collections::HashMap<String, usize> = self
            .pipeline
            .module_as::<pbh_core::modules::ip_rule_list::IpRuleListModule>(
                crate::rulesub::MODULE_NAME,
            )
            .map(|m| m.entry_counts().into_iter().collect())
            .unwrap_or_default();
        rules
            .rules
            .iter()
            .map(|(id, rule)| {
                json!({
                    "id": id,
                    "name": rule.name,
                    "url": rule.url,
                    "enabled": rule.enabled,
                    "entries": counts.get(id).copied().unwrap_or(0),
                })
            })
            .collect()
    }

    fn add_rule(&self, rule: &Value) -> Result<(), String> {
        let id = rule
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| "缺少规则 ID".to_string())?
            .to_string();
        let sub = parse_subscription(rule)?;
        {
            let mut rules = self.rules.write().unwrap();
            if rules.rules.contains_key(&id) {
                return Err(format!("规则 {id} 已存在"));
            }
            rules.rules.insert(id.clone(), sub);
            self.save(&rules)?;
        }
        // 立即刷新一次以加载内容（落库由 refresh_all 内部完成）
        let _ = self.run_refresh(Some(&id));
        Ok(())
    }

    fn update_rule(&self, id: &str, rule: &Value) -> Result<(), String> {
        {
            let mut rules = self.rules.write().unwrap();
            let existing = rules
                .rules
                .get_mut(id)
                .ok_or_else(|| format!("规则 {id} 不存在"))?;
            if let Some(name) = rule.get("name").and_then(|v| v.as_str()) {
                existing.name = name.to_string();
            }
            if let Some(url) = rule.get("url").and_then(|v| v.as_str()) {
                existing.url = url.to_string();
            }
            if let Some(enabled) = rule.get("enabled").and_then(|v| v.as_bool()) {
                existing.enabled = enabled;
            }
            self.save(&rules)?;
        }
        let _ = self.run_refresh(Some(id));
        Ok(())
    }

    fn remove_rule(&self, id: &str) -> Result<(), String> {
        {
            let mut rules = self.rules.write().unwrap();
            if rules.rules.remove(id).is_none() {
                return Err(format!("规则 {id} 不存在"));
            }
            self.save(&rules)?;
        }
        if let Some(module) =
            self.pipeline
                .module_as::<pbh_core::modules::ip_rule_list::IpRuleListModule>(
                    crate::rulesub::MODULE_NAME,
                )
        {
            module.remove_subscription(id);
        }
        Ok(())
    }

    fn refresh_rule(&self, id: Option<&str>) -> String {
        self.run_refresh(id)
    }

    fn logs(&self, page: i64, size: i64) -> (i64, Vec<Value>) {
        let limit = size.clamp(1, 100);
        let offset = (page.max(0)) * limit;
        let total = self.db.count_rule_sub_log(None).unwrap_or(0);
        let rows = self
            .db
            .list_rule_sub_log(None, limit, offset)
            .unwrap_or_default();
        let items = rows
            .into_iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "ruleId": r.rule_id,
                    "updateTime": r.update_time,
                    "count": r.count,
                    "updateType": r.update_type,
                })
            })
            .collect();
        (total, items)
    }

    fn interval(&self) -> i64 {
        self.rules.read().unwrap().check_interval_ms
    }

    fn set_interval(&self, interval_ms: i64) -> Result<(), String> {
        {
            let mut rules = self.rules.write().unwrap();
            rules.check_interval_ms = interval_ms.max(60_000);
            self.save(&rules)?;
        }
        Ok(())
    }
}

/// 从 Web 请求体解析单条订阅配置（与上游 `RuleSubscribe` 实体同形）。
fn parse_subscription(rule: &Value) -> Result<IpRuleSubscriptionConfig, String> {
    Ok(IpRuleSubscriptionConfig {
        enabled: rule.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
        name: rule
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        url: rule
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "缺少订阅 URL".to_string())?
            .to_string(),
    })
}
