//! 单条规则匹配器，忠实复刻上游 `util/rule` 包（SPEC 第 4 节 [GOLDEN]）。

use regex::Regex;
use serde_json::Value;
use std::convert::TryFrom;

/// 三态裁决，对齐 `MatchResultEnum`：DEFAULT / TRUE / FALSE
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Verdict {
    Default,
    True,
    False,
}

impl Verdict {
    pub fn parse(s: &str) -> Verdict {
        match s.trim().to_ascii_uppercase().as_str() {
            "TRUE" => Verdict::True,
            "FALSE" => Verdict::False,
            _ => Verdict::Default,
        }
    }
}

/// 虚拟规则（非 JSON 对象），对齐 RuleParser 中的匿名 AbstractMatcher
#[derive(Clone, Debug)]
pub enum Virtual {
    /// JsonNull -> 恒 TRUE
    Null,
    /// JSON 布尔
    Bool(bool),
    /// JSON 数字：非 0 -> TRUE，0 -> FALSE
    Number(i64),
    /// JSON 字符串：Boolean.parseBoolean，仅 "true"（忽略大小写）为 TRUE
    BoolString(String),
}

#[derive(Clone, Debug)]
pub enum Kind {
    /// 小写前缀匹配
    StartsWith(String),
    /// 小写后缀匹配
    EndsWith(String),
    /// 小写包含匹配
    Contains(String),
    /// 大小写不敏感全等（Java `String.equalsIgnoreCase`）
    Equals(String),
    /// 正则整段匹配（Java `matcher.matches()`）
    Regex(String, Regex),
    /// 长度区间（含边界，按 Java `String.length()` 即 UTF-16 码元数）
    Length(i64, i64),
    /// 虚拟规则
    Virtual(Virtual),
}

/// Java `String.toLowerCase(Locale.ROOT)`：Unicode 感知的小写映射。
/// 注意不能用 `to_ascii_lowercase`，否则非 ASCII 规则（如 `Ä`）会漏判。
fn java_lowercase(s: &str) -> String {
    s.to_lowercase()
}

/// Java `Character.toUpperCase(char)`：返回单字符的**简单**大小写映射
/// （Unicode 的 C 映射，而 Rust `char::to_uppercase` 是可能展开的 F 映射）。
fn simple_upper(c: char) -> char {
    let mut it = c.to_uppercase();
    let first = it.next().unwrap_or(c);
    if it.next().is_some() {
        c
    } else {
        first
    }
}

/// Java `Character.toLowerCase(char)`：同上，简单映射。
fn simple_lower(c: char) -> char {
    let mut it = c.to_lowercase();
    let first = it.next().unwrap_or(c);
    if it.next().is_some() {
        c
    } else {
        first
    }
}

/// 对齐 Java `String.equalsIgnoreCase` → `String.regionMatches(true, ...)`：
/// 逐字符比较 `c1 == c2 || toUpperCase(c1) == toUpperCase(c2) || toLowerCase(c1) == toLowerCase(c2)`。
fn equals_ignore_case(a: &str, b: &str) -> bool {
    let mut ai = a.chars();
    let mut bi = b.chars();
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) => {
                if x != y
                    && simple_upper(x) != simple_upper(y)
                    && simple_lower(x) != simple_lower(y)
                {
                    return false;
                }
            }
            _ => return false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Matcher {
    /// 可选前置条件 `if`：条件裁决为 FALSE 时直接判 FALSE
    pub condition: Option<Box<Matcher>>,
    pub kind: Kind,
    pub hit: Verdict,
    pub miss: Verdict,
}

fn verdict_or(v: &Value, key: &str, default: Verdict) -> Verdict {
    match v.get(key).and_then(|x| x.as_str()) {
        Some(s) => Verdict::parse(s),
        None => default,
    }
}

impl Matcher {
    /// 对齐上游各匹配器的 `matcherName()`，用于生成可翻译的规则名。
    ///
    /// - 字符串匹配器：`MATCH_STRING_*` + 规则内容（LENGTH 为 `"Min-x, Max-y"`）
    /// - 正则：`MATCH_STRING_REGEX` + 原始模式
    /// - 虚拟规则：无对应文案，返回空 key（渲染为空串）
    pub fn name_component(&self) -> crate::i18n::TranslationComponent {
        use crate::i18n::TranslationComponent;
        match &self.kind {
            Kind::StartsWith(rule) => {
                TranslationComponent::with_params("MATCH_STRING_STARTS_WITH", vec![rule.into()])
            }
            Kind::EndsWith(rule) => {
                TranslationComponent::with_params("MATCH_STRING_ENDS_WITH", vec![rule.into()])
            }
            Kind::Contains(rule) => {
                TranslationComponent::with_params("MATCH_STRING_CONTAINS", vec![rule.into()])
            }
            Kind::Equals(rule) => {
                TranslationComponent::with_params("MATCH_STRING_EQUALS", vec![rule.into()])
            }
            Kind::Regex(pattern, _) => {
                TranslationComponent::with_params("MATCH_STRING_REGEX", vec![pattern.into()])
            }
            Kind::Length(min, max) => TranslationComponent::with_params(
                "MATCH_STRING_LENGTH",
                vec![format!("Min-{min}, Max-{max}").into()],
            ),
            Kind::Virtual(_) => TranslationComponent::new(""),
        }
    }

    /// 对齐上游各匹配器的 `metadata()`（用于结构化数据与日志）。
    pub fn metadata(&self) -> String {
        match &self.kind {
            Kind::StartsWith(rule)
            | Kind::EndsWith(rule)
            | Kind::Contains(rule)
            | Kind::Equals(rule) => rule.clone(),
            Kind::Regex(pattern, _) => pattern.clone(),
            Kind::Length(min, max) => format!("min: {min}, max: {max}"),
            Kind::Virtual(_) => String::new(),
        }
    }

    /// 从规则 JSON 解析；与上游 RuleParser.parse 对齐。
    pub fn parse(v: &Value) -> anyhow::Result<Matcher> {
        match v {
            Value::Null => Ok(Matcher::virtual_(Virtual::Null)),
            Value::Bool(b) => Ok(Matcher::virtual_(Virtual::Bool(*b))),
            Value::Number(n) => {
                let i = n.as_i64().unwrap_or(0);
                Ok(Matcher::virtual_(Virtual::Number(i)))
            }
            Value::String(s) => Ok(Matcher::virtual_(Virtual::BoolString(s.clone()))),
            Value::Object(_) => {
                let method = v
                    .get("method")
                    .and_then(|x| x.as_str())
                    .ok_or_else(|| anyhow::anyhow!("rule missing method: {v}"))?;
                let condition = match v.get("if") {
                    Some(c) => Some(Box::new(Matcher::parse(c)?)),
                    None => None,
                };
                let hit = verdict_or(v, "hit", Verdict::True);
                let miss = verdict_or(v, "miss", Verdict::Default);
                let content = |key: &str| {
                    v.get(key)
                        .and_then(|x| x.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_default()
                };
                let kind = match method {
                    "STARTS_WITH" => Kind::StartsWith(java_lowercase(&content("content"))),
                    "ENDS_WITH" => Kind::EndsWith(java_lowercase(&content("content"))),
                    "CONTAINS" => Kind::Contains(java_lowercase(&content("content"))),
                    "EQUALS" => Kind::Equals(content("content")),
                    "REGEX" => {
                        let pattern = content("content");
                        // Java matches() 要求整段匹配；用 \A(?:p)\z 对齐
                        let wrapped = format!(r"\A(?:{})\z", pattern);
                        let re = Regex::new(&wrapped)
                            .map_err(|e| anyhow::anyhow!("invalid regex `{pattern}`: {e}"))?;
                        Kind::Regex(pattern, re)
                    }
                    "LENGTH" => {
                        let min = v.get("min").and_then(|x| x.as_i64()).unwrap_or(0);
                        let max = v.get("max").and_then(|x| x.as_i64()).unwrap_or(i64::MAX);
                        Kind::Length(min, max)
                    }
                    other => anyhow::bail!("unknown matcher method: {other}"),
                };
                Ok(Matcher {
                    condition,
                    kind,
                    hit,
                    miss,
                })
            }
            Value::Array(_) => anyhow::bail!("rule must be object/primitive, got array"),
        }
    }

    fn virtual_(virt: Virtual) -> Matcher {
        Matcher {
            condition: None,
            kind: Kind::Virtual(virt),
            hit: Verdict::True,
            miss: Verdict::False,
        }
    }

    /// match0：不含 `if` 前置条件的纯匹配
    pub fn match0(&self, content: &str) -> Verdict {
        let matched = match &self.kind {
            Kind::StartsWith(rule) => java_lowercase(content).starts_with(rule),
            Kind::EndsWith(rule) => java_lowercase(content).ends_with(rule),
            Kind::Contains(rule) => java_lowercase(content).contains(rule),
            Kind::Equals(rule) => equals_ignore_case(content, rule),
            Kind::Regex(_, re) => re.is_match(content),
            Kind::Length(min, max) => {
                // Java 的 String.length() 是 UTF-16 码元数
                let len = content.encode_utf16().count() as i64;
                len >= *min && len <= *max
            }
            Kind::Virtual(v) => match v {
                Virtual::Null => true,
                Virtual::Bool(b) => *b,
                Virtual::Number(n) => *n != 0,
                // Boolean.parseBoolean：仅 equalsIgnoreCase("true")
                Virtual::BoolString(s) => s.eq_ignore_ascii_case("true"),
            },
        };
        if matched {
            self.hit
        } else {
            self.miss
        }
    }

    /// 完整匹配：null 归一为空串，先评估 `if` 条件（FALSE 短路）。
    pub fn matches(&self, content: Option<&str>) -> Verdict {
        let content = content.unwrap_or("");
        if let Some(cond) = &self.condition {
            if cond.matches(Some(content)) == Verdict::False {
                return Verdict::False;
            }
        }
        self.match0(content)
    }
}

impl TryFrom<&Value> for Matcher {
    type Error = anyhow::Error;
    fn try_from(v: &Value) -> Result<Self, Self::Error> {
        Matcher::parse(v)
    }
}
