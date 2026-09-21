//! i18n 黄金测试：对齐上游 `TranslationComponent` + `TextManager.tl` + `MsgUtil.fillArgs`。
//!
//! 文案资源逐字复用上游 `src/main/resources/lang/*`（GPL-3.0），因此断言里的字符串
//! 就是上游 UI 会显示的内容。

use pbh_core::i18n::{format_percent, TranslationComponent, Translator};

#[test]
fn renders_upstream_templates_with_positional_params() {
    let t = Translator::embedded();
    let c = TranslationComponent::with_params(
        "MODULE_PCB_PEER_BAN_INCORRECT_PROGRESS",
        vec![
            format_percent(0.20).into(),
            format_percent(0.40).into(),
            format_percent(0.20).into(),
        ],
    );
    assert_eq!(
        t.render(&c, "en_us"),
        "Client progress: 20.00%, calculated minimal: 40.00%, difference: 20.00%"
    );
    assert_eq!(
        t.render(&c, "zh_cn"),
        "客户端进度：20.00%，实际进度：40.00%，差值：20.00%"
    );
}

#[test]
fn renders_single_arg_templates() {
    let t = Translator::embedded();
    let c = TranslationComponent::with_params("MODULE_IBL_MATCH_IP", vec!["1.2.3.0/24".into()]);
    assert_eq!(t.render(&c, "zh_cn"), "匹配 IP 规则: 1.2.3.0/24");
    assert_eq!(t.render(&c, "en_us"), "Match IP rule: 1.2.3.0/24");
    // 无参模板
    let no_args = TranslationComponent::new("PCB_RULE_REACHED_MAX_DIFFERENCE");
    assert_eq!(t.render(&no_args, "zh_cn"), "已超过允许的进度差异最大值");
    // 繁体
    assert_eq!(t.render(&no_args, "zh_tw"), "已超過允許的進度差異最大值");
}

#[test]
fn unknown_key_renders_as_the_key_itself() {
    // 对齐 `TextManager.tl`：查表失败时返回 key 本身
    let t = Translator::embedded();
    let c = TranslationComponent::new("NOT_A_REAL_KEY");
    assert_eq!(t.render(&c, "zh_cn"), "NOT_A_REAL_KEY");
}

#[test]
fn locale_matching_normalizes_case_and_dashes() {
    let t = Translator::embedded();
    let c = TranslationComponent::new("PCB_RULE_REACHED_MAX_DIFFERENCE");
    assert_eq!(t.render(&c, "zh-CN"), "已超过允许的进度差异最大值");
    // 未收录的语言回退到 en_us
    assert_eq!(t.render(&c, "xx_yy"), "Exceeded maximum allowed progress difference");
}

#[test]
fn fill_args_follows_msg_util_semantics() {
    // 参数不足时保留 `{}` 占位符
    let t = Translator::embedded();
    let c = TranslationComponent::with_params("MODULE_IBL_MATCH_IP", Vec::new());
    assert_eq!(t.render(&c, "en_us"), "Match IP rule: {}");
    // 多余参数被忽略
    let c2 = TranslationComponent::with_params("MODULE_IBL_MATCH_IP", vec!["a".into(), "b".into()]);
    assert_eq!(t.render(&c2, "en_us"), "Match IP rule: a");
}

/// 嵌套的可翻译成分会被递归渲染后再填参（对齐上游 `TextManager.convert`）。
#[test]
fn nested_components_are_rendered_recursively() {
    let t = Translator::embedded();
    let inner = TranslationComponent::with_params("MATCH_STRING_STARTS_WITH", vec!["-hp".into()]);
    let outer = TranslationComponent::with_params("MODULE_CNB_MATCH_CLIENT_NAME", vec![inner.into()]);
    assert_eq!(t.render(&outer, "en_us"), "Match ClientName (UserAgent): String StartsWith: -hp");
    assert_eq!(t.render(&outer, "zh_cn"), "匹配 ClientName (UserAgent): 字符串开头: -hp");
}

#[test]
fn percent_formatting_matches_java_decimal_format() {
    // DecimalFormat("0.00%")
    assert_eq!(format_percent(0.0), "0.00%");
    assert_eq!(format_percent(0.07), "7.00%");
    assert_eq!(format_percent(0.2), "20.00%");
    assert_eq!(format_percent(1.0), "100.00%");
    assert_eq!(format_percent(1.5), "150.00%");
    assert_eq!(format_percent(0.1234), "12.34%");
}
