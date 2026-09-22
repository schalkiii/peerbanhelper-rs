//! 文案国际化：对齐上游 `TranslationComponent` + `TextManager.tl` + `MsgUtil.fillArgs`。
//!
//! - 文案资源逐字复用上游 `src/main/resources/lang/*`（GPL-3.0），通过 `include_str!` 内嵌；
//!   运行时可再叠加 `data/lang/<locale>/messages.yml` 覆盖（对齐上游 override 机制）。
//! - 渲染流程与上游一致：`locale` 归一化（小写、`-`→`_`）→ 查表（locale → `en_us` →
//!   `messages_fallback`）→ 查不到就用 key 本身 → 再把模板中的 `{}` 按位置填充参数。
//! - 抽查使用的格式化工具是 `DecimalFormat("0.00%")`（见 [`format_percent`]）。

use serde::{Deserialize, Serialize};
use serde_yaml::Value;
use std::collections::HashMap;
use std::path::Path;

/// 渲染参数：字面文本，或另一个待渲染的可翻译文本（对齐上游参数可为 `TranslationComponent`）。
///
/// 上游 `TranslationComponent.params` 是 `Object[]`，元素可以是字符串 / 数字 / 布尔 / 嵌套组件；
/// Gson 把标量写成裸值、把组件写成 `{"key":…,"params":[…]}`。本类型的 serde 实现
/// 与之逐项对齐（标量统一收成 [`Param::Text`]），同时兼容本移植早期写下的
/// externally-tagged 形式（`{"Text":…}` / `{"Component":…}`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Param {
    Text(String),
    Component(TranslationComponent),
}

impl Serialize for Param {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Param::Text(text) => serializer.serialize_str(text),
            Param::Component(component) => component.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Param {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            /// 裸字符串（上游标量参数）
            Text(String),
            /// 裸数字（上游数字参数，收成文本）
            Number(f64),
            /// 裸布尔
            Bool(bool),
            /// 嵌套组件（上游 `Object[]` 里也可以是 `TranslationComponent`）
            Component(TranslationComponent),
            /// 本移植早期格式：`{"Text": "…"}`
            TaggedText { #[serde(rename = "Text")] text: String },
            /// 本移植早期格式：`{"Component": {…}}`
            TaggedComponent { #[serde(rename = "Component")] component: TranslationComponent },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Text(text) => Param::Text(text),
            Raw::Number(number) => Param::Text(format_scalar_number(number)),
            Raw::Bool(flag) => Param::Text(flag.to_string()),
            Raw::Component(component) => Param::Component(component),
            Raw::TaggedText { text } => Param::Text(text),
            Raw::TaggedComponent { component } => Param::Component(component),
        })
    }
}

/// 数字参数的文本化：整数值不带小数点（Gson 会把 `1.0` 写成 `1.0`，这里取更自然的写法）。
fn format_scalar_number(number: f64) -> String {
    if number.fract() == 0.0 && number.abs() < 1e15 {
        format!("{}", number as i64)
    } else {
        format!("{number}")
    }
}

impl From<String> for Param {
    fn from(value: String) -> Self {
        Param::Text(value)
    }
}

impl From<&str> for Param {
    fn from(value: &str) -> Self {
        Param::Text(value.to_string())
    }
}

impl From<&String> for Param {
    fn from(value: &String) -> Self {
        Param::Text(value.clone())
    }
}

impl From<TranslationComponent> for Param {
    fn from(value: TranslationComponent) -> Self {
        Param::Component(value)
    }
}

/// 可翻译文本：`key` + 位置参数（对齐上游 `TranslationComponent`）。
///
/// JSON 形状与上游一致：`{"key":"…","params":[…]}`（`params` 缺省/为 null 时按空数组处理）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranslationComponent {
    pub key: String,
    #[serde(default)]
    pub params: Vec<Param>,
}

impl TranslationComponent {
    pub fn new(key: impl Into<String>) -> Self {
        Self { key: key.into(), params: Vec::new() }
    }

    pub fn with_params(key: impl Into<String>, params: Vec<Param>) -> Self {
        Self { key: key.into(), params }
    }

    /// 便捷渲染（等价 `Translator::render`）。
    pub fn render(&self, translator: &Translator, locale: &str) -> String {
        translator.render(self, locale)
    }
}

/// 文案表：`locale` → `key` → 模板。
#[derive(Debug, Default)]
pub struct Translator {
    tables: HashMap<String, HashMap<String, String>>,
}

const EMBEDDED_FALLBACK: &str = include_str!("../resources/lang/messages_fallback.yml");
const EMBEDDED_EN_US: &str = include_str!("../resources/lang/en_us/messages.yml");
const EMBEDDED_ZH_CN: &str = include_str!("../resources/lang/zh_cn/messages.yml");
const EMBEDDED_ZH_TW: &str = include_str!("../resources/lang/zh_tw/messages.yml");

/// 上游 `locale.toLowerCase(Locale.ROOT).replace("-", "_")`
pub fn normalize_locale(locale: &str) -> String {
    locale.trim().to_lowercase().replace('-', "_")
}

impl Translator {
    /// 仅使用内嵌的 4 份上游文案表。
    pub fn embedded() -> Self {
        let mut tables = HashMap::new();
        for (locale, raw) in [
            ("en_us", EMBEDDED_EN_US),
            ("zh_cn", EMBEDDED_ZH_CN),
            ("zh_tw", EMBEDDED_ZH_TW),
            ("messages_fallback", EMBEDDED_FALLBACK),
        ] {
            tables.insert(locale.to_string(), parse_messages(raw));
        }
        Self { tables }
    }

    /// 内嵌文案表 + 可选的 `data/lang` 覆盖目录（对齐上游 override）。
    ///
    /// 目录布局：`<dir>/<locale>/messages.yml` 与 `<dir>/messages_fallback.yml`。
    pub fn with_overrides(dir: Option<&Path>) -> Self {
        let mut translator = Self::embedded();
        if let Some(dir) = dir {
            translator.apply_override_dir(dir);
        }
        translator
    }

    fn apply_override_dir(&mut self, dir: &Path) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let locale = entry.file_name().to_string_lossy().to_lowercase();
                    let file = path.join("messages.yml");
                    if let Ok(text) = std::fs::read_to_string(&file) {
                        self.tables.entry(locale).or_default().extend(parse_messages(&text));
                    }
                }
            }
        }
        let fallback = dir.join("messages_fallback.yml");
        if let Ok(text) = std::fs::read_to_string(&fallback) {
            self.tables
                .entry("messages_fallback".to_string())
                .or_default()
                .extend(parse_messages(&text));
        }
    }

    /// 按上游回退链取模板：`locale` → `en_us` → `messages_fallback`。
    pub fn template(&self, key: &str, locale: &str) -> Option<&str> {
        let locale = normalize_locale(locale);
        for candidate in [locale.as_str(), "en_us", "messages_fallback"] {
            if let Some(table) = self.tables.get(candidate) {
                if let Some(text) = table.get(key) {
                    return Some(text);
                }
            }
        }
        None
    }

    /// 渲染：查表 → 查不到用 key 本身 → 解析参数（嵌套成分递归渲染）→ `fill_args` 填参。
    pub fn render(&self, component: &TranslationComponent, locale: &str) -> String {
        if component.key.is_empty() {
            return String::new();
        }
        let template = self.template(&component.key, locale).unwrap_or(&component.key);
        let args: Vec<String> = component
            .params
            .iter()
            .map(|p| match p {
                Param::Text(s) => s.clone(),
                Param::Component(c) => self.render(c, locale),
            })
            .collect();
        fill_args(template, &args)
    }
}

/// 解析上游 `messages.yml`（扁平 `KEY: "value"` 映射）。
fn parse_messages(raw: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(raw) else {
        return out;
    };
    let Some(map) = value.as_mapping() else {
        return out;
    };
    for (k, v) in map {
        let (Some(key), Some(val)) = (k.as_str(), value_to_string(v)) else {
            continue;
        };
        out.insert(key.to_string(), val);
    }
    out
}

fn value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// 位置参数填充，对齐 `MsgUtil.fillArgs`：
/// 逐个 `{}` 替换；参数不足时保留 `{}`；多余参数忽略；参数为 null 时填 `""`。
pub fn fill_args(raw: &str, args: &[String]) -> String {
    if raw.is_empty() {
        return String::new();
    }
    let mut result = String::with_capacity(raw.len());
    let mut start = 0usize;
    let mut arg_index = 0usize;
    while start < raw.len() {
        match raw[start..].find("{}") {
            None => {
                result.push_str(&raw[start..]);
                break;
            }
            Some(offset) => {
                let placeholder = start + offset;
                result.push_str(&raw[start..placeholder]);
                if arg_index < args.len() {
                    result.push_str(&args[arg_index]);
                    arg_index += 1;
                } else {
                    result.push_str("{}");
                }
                start = placeholder + 2;
            }
        }
    }
    result
}

/// 对齐上游 `MsgUtil.getPercentageFormatter()`：`DecimalFormat("0.00%")`，
/// 即数值 ×100、保留两位小数、附带 `%`（四舍五入）。
pub fn format_percent(value: f64) -> String {
    format!("{:.2}%", value * 100.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_locale_matches_upstream() {
        assert_eq!(normalize_locale("zh-CN"), "zh_cn");
        assert_eq!(normalize_locale(" EN_us "), "en_us");
    }

    #[test]
    fn embedded_tables_are_loaded() {
        let t = Translator::embedded();
        assert_eq!(t.tables.len(), 4);
        assert!(t.template("PCB_RULE_PROGRESS_REWIND", "zh_cn").is_some());
    }
}
