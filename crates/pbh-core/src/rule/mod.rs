//! 规则集与多规则裁决，对齐 `RuleParser.matchRule`（SPEC 4.2 [GOLDEN]）。

pub mod matcher;

pub use matcher::{Kind, Matcher, Verdict, Virtual};

use serde_json::Value;

#[derive(Clone, Debug, Default)]
pub struct RuleSet {
    pub rules: Vec<Matcher>,
}

/// 多规则裁决结果
#[derive(Clone, Copy, Debug)]
pub struct RuleMatchResult {
    pub hit: bool,
    /// 命中（首个 TRUE）规则下标，未命中为 -1
    pub index: isize,
    pub verdict: Verdict,
}

impl RuleSet {
    pub fn new(rules: Vec<Matcher>) -> Self {
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// profile.yml 中规则以「JSON 文本字符串」列表存储，逐条反序列化。
    pub fn from_json_text(items: &[String]) -> anyhow::Result<Self> {
        let mut rules = Vec::with_capacity(items.len());
        for raw in items {
            let value: Value = serde_json::from_str(raw.trim())
                .map_err(|e| anyhow::anyhow!("bad rule json `{raw}`: {e}"))?;
            rules.push(Matcher::parse(&value)?);
        }
        Ok(Self { rules })
    }

    pub fn from_values(values: &[Value]) -> anyhow::Result<Self> {
        let mut rules = Vec::with_capacity(values.len());
        for v in values {
            rules.push(Matcher::parse(v)?);
        }
        Ok(Self { rules })
    }

    /// 裁决：FALSE 立即短路（最高优先级）；TRUE 记录但可被后续 FALSE 覆盖；DEFAULT 忽略。
    ///
    /// 对齐 Java `RuleParser.matchRule`：每次命中 TRUE 都**覆盖**已记录结果，
    /// 因此最终上报的规则是「最后一条命中 TRUE 的规则」，而非第一条。
    pub fn r#match(&self, content: Option<&str>) -> RuleMatchResult {
        let mut result = RuleMatchResult { hit: false, index: -1, verdict: Verdict::Default };
        for (i, rule) in self.rules.iter().enumerate() {
            match rule.matches(content) {
                Verdict::Default => {}
                Verdict::True => {
                    result = RuleMatchResult { hit: true, index: i as isize, verdict: Verdict::True };
                }
                Verdict::False => {
                    return RuleMatchResult { hit: false, index: i as isize, verdict: Verdict::False };
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(json: &str) -> Matcher {
        let v: Value = serde_json::from_str(json).unwrap();
        Matcher::parse(&v).unwrap()
    }

    #[test]
    fn false_short_circuits_even_after_true() {
        // 第一条 CONTAINS a -> TRUE，第二条 EQUALS 显式 FALSE：最终不命中（FALSE 短路）
        let rules = vec![
            m(r#"{"method":"CONTAINS","content":"a"}"#),
            m(r#"{"method":"EQUALS","content":"aX","hit":"FALSE","miss":"DEFAULT"}"#),
        ];
        let rs = RuleSet::new(rules);
        assert!(!rs.r#match(Some("ax")).hit);
    }

    #[test]
    fn first_true_wins_and_defaults_noop() {
        let rules = vec![
            m(r#"{"method":"CONTAINS","content":"-hp"}"#),
            m(r#"{"method":"CONTAINS","content":"zzz"}"#),
        ];
        let rs = RuleSet::new(rules);
        let r = rs.r#match(Some("-HP001-xx"));
        assert!(r.hit);
        assert_eq!(r.index, 0);
    }

    #[test]
    fn regex_requires_full_match() {
        // Java matches() 整段匹配；部分匹配不算命中
        let rule = m(r#"{"method":"REGEX","content":"-hp.*"}"#);
        assert_eq!(rule.matches(Some("-hp001")), Verdict::True);
        assert_eq!(rule.matches(Some("xx-hp001")), Verdict::Default);
    }

    #[test]
    fn equals_is_case_insensitive() {
        let rule = m(r#"{"method":"EQUALS","content":"unknown"}"#);
        assert_eq!(rule.matches(Some("Unknown")), Verdict::True);
    }

    #[test]
    fn length_inclusive_bounds() {
        let rule = m(r#"{"method":"LENGTH","min":2,"max":4,"hit":"TRUE","miss":"FALSE"}"#);
        assert_eq!(rule.matches(Some("ab")), Verdict::True);
        assert_eq!(rule.matches(Some("abcd")), Verdict::True);
        assert_eq!(rule.matches(Some("abcde")), Verdict::False);
    }

    #[test]
    fn virtual_rules() {
        assert_eq!(Matcher::parse(&Value::Null).unwrap().matches(Some("x")), Verdict::True);
        assert_eq!(Matcher::parse(&Value::Bool(false)).unwrap().matches(Some("x")), Verdict::False);
        assert_eq!(Matcher::parse(&serde_json::json!(1)).unwrap().matches(Some("x")), Verdict::True);
        assert_eq!(Matcher::parse(&serde_json::json!(0)).unwrap().matches(Some("x")), Verdict::False);
        assert_eq!(Matcher::parse(&Value::String("true".into())).unwrap().matches(Some("x")), Verdict::True);
        assert_eq!(Matcher::parse(&Value::String("false".into())).unwrap().matches(Some("x")), Verdict::False);
    }

    #[test]
    fn if_condition_false_short_circuits() {
        // if 条件 miss 显式 FALSE：不含 qbit 时整体 FALSE；含 qbit 时正常走主匹配器
        let rule = m(
            r#"{"if":{"method":"CONTAINS","content":"qbit","hit":"TRUE","miss":"FALSE"},"method":"CONTAINS","content":"-hp"}"#,
        );
        assert_eq!(rule.matches(Some("-hp001")), Verdict::False);
        assert_eq!(rule.matches(Some("qbit-hp001")), Verdict::True);
        // 普通条件（miss=DEFAULT）未命中不会短路
        let rule_default = m(
            r#"{"if":{"method":"CONTAINS","content":"qbit"},"method":"CONTAINS","content":"-hp"}"#,
        );
        assert_eq!(rule_default.matches(Some("-hp001")), Verdict::True);
    }
}
