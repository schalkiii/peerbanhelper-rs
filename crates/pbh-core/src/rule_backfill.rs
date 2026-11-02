//! 默认规则补齐：让「随程序版本新增的默认规则」也能落到既有配置里。
//!
//! 上游 Java 用 `config-version` + `@UpdateScript(version = N)`（`ProfileUpdateScript`）
//! 逐条迁移脚本实现这件事。本移植没有那份脚本历史，改用等价的「已见过集合」机制：
//! 每条默认规则第一次见到时，如果用户列表里没有就追加；此后即使用户把它删掉，
//! 也不会被再次加回来——避免复活用户明确删除的规则。

use crate::config::ProfileConfig;
use crate::defaults::{DEFAULT_BANNED_CLIENT_NAME, DEFAULT_BANNED_PEER_ID};

/// 规则的可比较身份：`模块|method|content`。
///
/// 配置里存的是 JSON 字符串，键序/空白可能与人手写的不一致，所以按语义比较而不是按字面。
pub fn fingerprint(target: &str, rule: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(rule) {
        Ok(v) => {
            let method = v.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let content = v.get("content").and_then(|c| c.as_str()).unwrap_or("");
            format!("{target}|{method}|{content}")
        }
        // 解析不了的规则（例如手写漏了引号）退化为按原始串比较
        Err(_) => format!("{target}|{rule}"),
    }
}

fn backfill_one(
    target: &str,
    defaults: &[&str],
    list: Option<&mut Vec<String>>,
    seen: &mut Vec<String>,
    added: &mut Vec<String>,
) {
    // 模块配置节不存在 ⇒ 用户没有这个模块，不代为创建
    let Some(list) = list else { return };
    for rule in defaults {
        let fp = fingerprint(target, rule);
        if seen.contains(&fp) {
            continue;
        }
        seen.push(fp.clone());
        if !list.iter().any(|r| fingerprint(target, r) == fp) {
            list.push((*rule).to_string());
            added.push(fp);
        }
    }
}

/// 把新增的默认规则补进 profile，返回本次**实际追加**的规则指纹。
///
/// 调用方负责在返回非空时把配置写回磁盘（`crates/pbh/src/main.rs` 启动期调用）。
pub fn apply_missing_defaults(profile: &mut ProfileConfig) -> Vec<String> {
    let mut added = Vec::new();
    let mut seen = std::mem::take(&mut profile.default_rules_seen);
    backfill_one(
        "client-name",
        DEFAULT_BANNED_CLIENT_NAME,
        profile
            .module
            .client_name_blacklist
            .as_mut()
            .map(|c| &mut c.banned_client_name),
        &mut seen,
        &mut added,
    );
    backfill_one(
        "peer-id",
        DEFAULT_BANNED_PEER_ID,
        profile
            .module
            .peer_id_blacklist
            .as_mut()
            .map(|c| &mut c.banned_peer_id),
        &mut seen,
        &mut added,
    );
    profile.default_rules_seen = seen;
    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClientNameBlacklistConfig, PeerIdBlacklistConfig};

    /// 构造带指定规则列表的 profile；传 `None` 表示整个模块节不存在（= 用户没启用该模块）。
    fn profile_with(
        client_rules: Option<Vec<String>>,
        peer_rules: Option<Vec<String>>,
    ) -> ProfileConfig {
        let mut p = ProfileConfig::default();
        if let Some(rules) = client_rules {
            p.module.client_name_blacklist = Some(ClientNameBlacklistConfig {
                enabled: Some(true),
                ban_duration_ms: 0,
                banned_client_name: rules,
            });
        }
        if let Some(rules) = peer_rules {
            p.module.peer_id_blacklist = Some(PeerIdBlacklistConfig {
                enabled: Some(true),
                ban_duration_ms: 0,
                banned_peer_id: rules,
            });
        }
        p
    }

    fn all_defaults(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    fn client_rules(p: &ProfileConfig) -> Vec<String> {
        p.module
            .client_name_blacklist
            .as_ref()
            .unwrap()
            .banned_client_name
            .clone()
    }

    #[test]
    fn new_default_rule_is_appended_and_recorded() {
        // 去掉默认列表最后一条 ⇒ 模拟「随版本新增的默认规则」在老配置里缺失
        let mut older = all_defaults(DEFAULT_BANNED_CLIENT_NAME);
        let newest = older.pop().expect("默认列表应至少两条");
        let mut p = profile_with(Some(older.clone()), None);
        let added = apply_missing_defaults(&mut p);
        assert_eq!(
            added,
            vec![fingerprint("client-name", &newest)],
            "只补新增的那一条"
        );
        let kept = client_rules(&p);
        assert_eq!(kept.len(), older.len() + 1);
        assert!(kept.contains(&newest));
    }

    /// 关键语义：补齐只发生一次，用户之后删掉不会被「复活」。
    #[test]
    fn user_deleted_default_is_not_resurrected_on_next_run() {
        let mut p = profile_with(
            Some(all_defaults(DEFAULT_BANNED_CLIENT_NAME)),
            Some(all_defaults(DEFAULT_BANNED_PEER_ID)),
        );
        assert!(apply_missing_defaults(&mut p).is_empty(), "默认列表齐全时不该追加");
        // 用户删掉一条默认规则
        let mut trimmed = client_rules(&p);
        let removed = trimmed.remove(0);
        p.module.client_name_blacklist.as_mut().unwrap().banned_client_name = trimmed;
        let added = apply_missing_defaults(&mut p);
        assert!(added.is_empty(), "第二次运行不该再补任何东西");
        assert!(
            !client_rules(&p).contains(&removed),
            "被用户删除的规则不应回来"
        );
    }

    #[test]
    fn rule_already_present_by_another_spelling_is_not_duplicated() {
        // 键序不同（content 在前）也应识别为同一条规则
        let owned = r#"{"content":"Gopeed bt-","method":"STARTS_WITH"}"#.to_string();
        let mut p = profile_with(Some(vec![owned.clone()]), None);
        let _ = apply_missing_defaults(&mut p);
        let same = client_rules(&p)
            .iter()
            .filter(|r| fingerprint("client-name", r) == fingerprint("client-name", &owned))
            .count();
        assert_eq!(same, 1, "同义规则不应重复追加");
    }

    #[test]
    fn missing_module_section_is_not_created() {
        let mut p = profile_with(Some(vec![]), None);
        let _ = apply_missing_defaults(&mut p);
        assert!(
            p.module.peer_id_blacklist.is_none(),
            "用户删掉整个模块节时不该代为创建"
        );
    }

    #[test]
    fn second_run_is_a_noop() {
        let mut p = profile_with(Some(vec![]), Some(vec![]));
        let first = apply_missing_defaults(&mut p);
        assert!(
            !first.is_empty(),
            "空列表首次运行应把整套默认规则补进来"
        );
        let snapshot = serde_json::to_value(&p).unwrap();
        let second = apply_missing_defaults(&mut p);
        assert!(second.is_empty());
        assert_eq!(snapshot, serde_json::to_value(&p).unwrap(), "状态不应变化");
    }

    #[test]
    fn unparseable_rule_falls_back_to_raw_fingerprint() {
        let weird = "not-json{";
        assert_eq!(
            fingerprint("client-name", weird),
            format!("client-name|{weird}")
        );
    }
}
