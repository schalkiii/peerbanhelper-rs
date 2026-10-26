use super::*;
use base64::Engine as _;
use crate::config::PushSection;
use lettre::message::Mailbox;
use pbh_core::i18n::{Param, TranslationComponent, Translator};
use serde_json::Value as JsonValue;
use pbh_downloader::http::{BoxFuture, HttpFetcher, HttpRequest, HttpResponse};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

/// 记录请求、返回固定响应的 mock fetcher。
struct MockFetcher {
    status: u16,
    body: String,
    requests: Mutex<Vec<HttpRequest>>,
}

impl MockFetcher {
    fn new(status: u16, body: &str) -> Arc<Self> {
        Arc::new(Self {
            status,
            body: body.to_string(),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn only_request(&self) -> HttpRequest {
        let requests = self.requests();
        assert_eq!(requests.len(), 1, "应当恰好发出一个请求");
        requests[0].clone()
    }
}

impl HttpFetcher for MockFetcher {
    fn execute<'a>(&'a self, req: HttpRequest) -> BoxFuture<'a, anyhow::Result<HttpResponse>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(req);
            Ok(HttpResponse::new(self.status, self.body.clone()))
        })
    }
}

/// 由 YAML 片段构造管理器（键 = 渠道名）。
fn manager(yaml: &str, fetcher: Arc<MockFetcher>) -> PushManager {
    let section: PushSection = serde_yaml::from_str(yaml).expect("测试用 push 配置应可解析");
    PushManager::from_config(&section, fetcher)
}

fn only_provider(manager: &PushManager) -> &PushProvider {
    assert_eq!(manager.provider_list().len(), 1, "应当恰好注册一个渠道");
    &manager.provider_list()[0]
}

/// 断言 JSON 请求体：比较解析后的 `Value`（JSON 对象键顺序无语义；
/// 上游用 HashMap + Gson，键顺序本就不确定）。
fn assert_json_body(req: &HttpRequest, expected: &str) {
    let actual: JsonValue = serde_json::from_str(req.body.as_deref().expect("请求体不应为空"))
        .expect("请求体必须是合法 JSON");
    let expected: JsonValue = serde_json::from_str(expected).expect("期望值必须是合法 JSON");
    assert_eq!(actual, expected);
}

// ---------------- 空配置 ----------------

#[tokio::test]
async fn empty_provider_list_is_noop() {
    let fetcher = MockFetcher::new(200, "{}");
    let section = PushSection::default();
    let manager = PushManager::from_config(&section, fetcher.clone());
    assert!(manager.provider_list().is_empty());
    assert!(!manager.push_message("t", "c", AlertLevel::Warn).await);
    assert!(fetcher.requests().is_empty(), "空配置不应产生任何请求");
}

// ---------------- pushplus ----------------

#[tokio::test]
async fn pushplus_builds_exact_request_and_checks_code() {
    let fetcher = MockFetcher::new(200, r#"{"code":200,"msg":"请求成功"}"#);
    let m = manager(
        "my-plus:\n  type: pushplus\n  token: \"tk\"\n  topic: \"tp\"\n  channel: \"wechat\"\n",
        fetcher.clone(),
    );
    let provider = only_provider(&m);
    assert_eq!(provider.name(), "my-plus");
    assert_eq!(provider.config_type(), "pushplus");
    assert!(provider
        .push("标题", "正文", AlertLevel::Warn)
        .await
        .unwrap());

    let req = fetcher.only_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.url, "https://www.pushplus.plus/send");
    assert_eq!(req.header("Content-Type"), Some("application/json"));
    assert_json_body(
        &req,
        r#"{"token":"tk","topic":"tp","channel":"wechat","title":"标题","content":"正文","template":"markdown"}"#,
    );
}

#[tokio::test]
async fn pushplus_omits_blank_optional_fields() {
    let fetcher = MockFetcher::new(200, r#"{"code":200}"#);
    let m = manager("p:\n  type: pushplus\n  token: \"tk\"\n", fetcher.clone());
    assert!(only_provider(&m)
        .push("t", "c", AlertLevel::Info)
        .await
        .unwrap());
    // 空白的 topic/channel 整键不出现（对齐上游 `isBlank() → null` + 条件 put）
    assert_json_body(
        &fetcher.only_request(),
        r#"{"token":"tk","title":"t","content":"c","template":"markdown"}"#,
    );
}

#[tokio::test]
async fn pushplus_body_code_is_success_criterion() {
    // HTTP 200 但业务 code != 200 ⇒ 失败（对齐上游 IllegalStateException）
    let fetcher = MockFetcher::new(200, r#"{"code":500,"msg":"token 错误"}"#);
    let m = manager("p:\n  type: pushplus\n  token: \"tk\"\n", fetcher);
    let err = only_provider(&m)
        .push("t", "c", AlertLevel::Info)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("token 错误"), "{err}");

    // HTTP 非 2xx ⇒ 失败
    let fetcher = MockFetcher::new(500, "boom");
    let m = manager("p:\n  type: pushplus\n  token: \"tk\"\n", fetcher);
    assert!(only_provider(&m)
        .push("t", "c", AlertLevel::Info)
        .await
        .is_err());
}

// ---------------- serverchan ----------------

#[tokio::test]
async fn serverchan_builds_exact_request() {
    let fetcher = MockFetcher::new(200, r#"{"code":0}"#);
    let m = manager(
        "sc:\n  type: serverchan\n  sendkey: \"SCT123\"\n  channel: \"9\"\n  openid: \"oid\"\n",
        fetcher.clone(),
    );
    assert!(only_provider(&m)
        .push("标题", "正文", AlertLevel::Warn)
        .await
        .unwrap());
    let req = fetcher.only_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.url, "https://sctapi.ftqq.com/SCT123.send");
    assert_eq!(req.header("Content-Type"), Some("application/json"));
    assert_json_body(
        &req,
        r#"{"title":"标题","desp":"正文","text":"标题","channel":"9","openid":"oid"}"#,
    );
}

#[tokio::test]
async fn serverchan_omits_blank_channel_and_openid_and_reports_message() {
    let fetcher = MockFetcher::new(200, r#"{"code":0}"#);
    let m = manager(
        "sc:\n  type: serverchan\n  sendkey: \"SCT\"\n",
        fetcher.clone(),
    );
    assert!(only_provider(&m)
        .push("t", "c", AlertLevel::Info)
        .await
        .unwrap());
    // 空白的 channel/openid 整键不出现
    assert_json_body(
        &fetcher.only_request(),
        r#"{"title":"t","desp":"c","text":"t"}"#,
    );

    // 非 2xx：错误信息取响应体 message
    let fetcher = MockFetcher::new(400, r#"{"message":"bad sendkey"}"#);
    let m = manager("sc:\n  type: serverchan\n  sendkey: \"SCT\"\n", fetcher);
    let err = only_provider(&m)
        .push("t", "c", AlertLevel::Info)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("bad sendkey"), "{err}");
}

// ---------------- telegram ----------------

#[tokio::test]
async fn telegram_builds_exact_request() {
    let fetcher = MockFetcher::new(200, r#"{"ok":true}"#);
    let m = manager(
        "tg:\n  type: telegram\n  token: \"123:abc\"\n  chatid: \"-100\"\n",
        fetcher.clone(),
    );
    assert!(only_provider(&m)
        .push("标题", "正文", AlertLevel::Error)
        .await
        .unwrap());
    let req = fetcher.only_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.url, "https://api.telegram.org/bot123:abc/sendPhoto");
    assert_eq!(req.header("Content-Type"), Some("application/json"));
    let expected = format!(
        "{{\"chat_id\":\"-100\",\"caption\":\"*标题*\\n正文\",\"text\":\"*标题*\\n正文\",\"photo\":\"{ICON_URL}\",\"parse_mode\":\"Markdown\"}}"
    );
    assert_json_body(&req, &expected);
}

#[tokio::test]
async fn telegram_reports_description_on_failure() {
    let fetcher = MockFetcher::new(
        400,
        r#"{"ok":false,"error_code":400,"description":"chat not found"}"#,
    );
    let m = manager(
        "tg:\n  type: telegram\n  token: \"t\"\n  chatid: \"1\"\n",
        fetcher,
    );
    let err = only_provider(&m)
        .push("t", "c", AlertLevel::Info)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("chat not found"), "{err}");
}

// ---------------- bark ----------------

#[tokio::test]
async fn bark_builds_exact_request() {
    let fetcher = MockFetcher::new(200, r#"{"code":200}"#);
    let m = manager(
        "b:\n  type: bark\n  backend_url: \"https://bark.example/push\"\n  device_key: \"dk\"\n  message_group: \"PBH\"\n",
        fetcher.clone(),
    );
    assert!(only_provider(&m)
        .push("标题", "正文", AlertLevel::Warn)
        .await
        .unwrap());
    let req = fetcher.only_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.url, "https://bark.example/push");
    assert_eq!(req.header("Content-Type"), Some("application/json"));
    let expected = format!(
        "{{\"title\":\"标题\",\"body\":\"正文\",\"device_key\":\"dk\",\"group\":\"PBH\",\"icon\":\"{ICON_URL}\"}}"
    );
    assert_json_body(&req, &expected);
}

#[tokio::test]
async fn bark_defaults_backend_url_to_official_api() {
    let fetcher = MockFetcher::new(200, "{}");
    let m = manager("b:\n  type: bark\n  device_key: \"dk\"\n", fetcher.clone());
    assert!(only_provider(&m)
        .push("t", "c", AlertLevel::Info)
        .await
        .unwrap());
    let req = fetcher.only_request();
    assert_eq!(req.url, "https://api.day.app/push");
    // 空 group 照发（上游不对 message_group 做空值归一化）
    assert_json_body(
        &req,
        &format!(
            "{{\"title\":\"t\",\"body\":\"c\",\"device_key\":\"dk\",\"group\":\"\",\"icon\":\"{ICON_URL}\"}}"
        ),
    );
}

// ---------------- pushdeer ----------------

#[tokio::test]
async fn pushdeer_builds_exact_request() {
    let fetcher = MockFetcher::new(200, r#"{"code":0}"#);
    let m = manager(
        "pd:\n  type: pushdeer\n  endpoint: \"https://api2.pushdeer.com/message/push\"\n  pushkey: \"pk\"\n",
        fetcher.clone(),
    );
    assert!(only_provider(&m)
        .push("标题", "正文", AlertLevel::Info)
        .await
        .unwrap());
    let req = fetcher.only_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.url, "https://api2.pushdeer.com/message/push");
    assert_eq!(req.header("Content-Type"), Some("application/json"));
    assert_json_body(
        &req,
        r#"{"pushkey":"pk","text":"标题","desp":"正文","type":"markdown"}"#,
    );
}

// ---------------- gotify ----------------

#[tokio::test]
async fn gotify_posts_form_body() {
    let fetcher = MockFetcher::new(200, "{}");
    let m = manager(
        "g:\n  type: gotify\n  endpoint: \"https://gotify.example/message?token=app\"\n  priority: 8\n",
        fetcher.clone(),
    );
    assert!(only_provider(&m)
        .push("标题", "正文", AlertLevel::Warn)
        .await
        .unwrap());
    let req = fetcher.only_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.url, "https://gotify.example/message?token=app");
    assert_eq!(
        req.form,
        Some(vec![
            ("title".to_string(), "标题".to_string()),
            ("message".to_string(), "正文".to_string()),
            ("priority".to_string(), "8".to_string()),
        ])
    );
    assert!(req.body.is_none());
}

#[tokio::test]
async fn gotify_priority_defaults_to_five() {
    let fetcher = MockFetcher::new(200, "{}");
    let m = manager("g:\n  type: gotify\n", fetcher.clone());
    assert!(only_provider(&m)
        .push("t", "c", AlertLevel::Info)
        .await
        .unwrap());
    assert_eq!(
        fetcher.only_request().form,
        Some(vec![
            ("title".to_string(), "t".to_string()),
            ("message".to_string(), "c".to_string()),
            ("priority".to_string(), "5".to_string()),
        ])
    );
}

// ---------------- ntfy ----------------

#[tokio::test]
async fn ntfy_builds_exact_request_with_headers() {
    let fetcher = MockFetcher::new(200, r#"{"id":"x"}"#);
    let m = manager(
        "n:\n  type: ntfy\n  server_url: \"https://ntfy.sh\"\n  topic: \"mytopic\"\n  token: \"tk_test\"\n  priority: 4\n  tags: \"warning,pbh\"\n",
        fetcher.clone(),
    );
    assert!(only_provider(&m)
        .push("标题", "正文", AlertLevel::Warn)
        .await
        .unwrap());
    let req = fetcher.only_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.url, "https://ntfy.sh/mytopic");
    assert_eq!(req.body.as_deref(), Some("正文"));
    assert_eq!(
        req.header("Content-Type"),
        Some("text/plain; charset=utf-8")
    );
    // Title 为 RFC 2047 编码字（base64(UTF-8)）
    let expected_title = format!(
        "=?UTF-8?B?{}?=",
        base64::engine::general_purpose::STANDARD.encode("标题")
    );
    assert_eq!(req.header("Title"), Some(expected_title.as_str()));
    assert_eq!(req.header("Priority"), Some("4"));
    assert_eq!(req.header("Icon"), Some(ICON_URL));
    assert_eq!(req.header("Authorization"), Some("Bearer tk_test"));
    assert_eq!(req.header("Tags"), Some("warning,pbh"));
}

#[tokio::test]
async fn ntfy_skips_blank_token_and_tags() {
    let fetcher = MockFetcher::new(200, "ok");
    let m = manager("n:\n  type: ntfy\n  topic: \"t\"\n", fetcher.clone());
    assert!(only_provider(&m)
        .push("t", "c", AlertLevel::Info)
        .await
        .unwrap());
    let req = fetcher.only_request();
    assert_eq!(req.url, "https://ntfy.sh/t");
    assert_eq!(req.header("Authorization"), None);
    assert_eq!(req.header("Tags"), None);
    assert_eq!(req.header("Priority"), Some("3"));
}

// ---------------- webhook ----------------

/// 固定时间戳：用于 `{date}`/`{time}`/`{datetime}` 的确定性断言。
const FIXED_NOW_MS: i64 = 1_789_862_400_000;

#[tokio::test]
async fn webhook_post_renders_default_json_template() {
    let fetcher = MockFetcher::new(200, "ok");
    let m = manager(
        "w:\n  type: webhook\n  url: \"https://hook.example/push?ch={channelName}\"\n",
        fetcher.clone(),
    );
    assert!(only_provider(&m)
        .push_at("标题", "正文", AlertLevel::Error, FIXED_NOW_MS)
        .await
        .unwrap());
    let req = fetcher.only_request();
    assert_eq!(req.method, "POST");
    assert_eq!(req.url, "https://hook.example/push?ch=w");
    assert_eq!(req.header("Content-Type"), Some("application/json"));
    let expected = format!(
        "{{\n    \"title\":\"标题\",\n    \"content\":\"正文\",\n    \"level\":\"ERROR\",\n    \"date\":\"{}\",\n    \"time\":\"{}\",\n    \"datetime\":\"{}\",\n    \"channelName\":\"w\"\n}}\n",
        format_date_only(FIXED_NOW_MS),
        format_time_only(FIXED_NOW_MS),
        format_date_time(FIXED_NOW_MS),
    );
    assert_eq!(req.body.as_deref(), Some(expected.as_str()));
}

#[tokio::test]
async fn webhook_get_has_no_body_and_custom_headers() {
    let fetcher = MockFetcher::new(200, "ok");
    let m = manager(
        "w:\n  type: webhook\n  url: \"https://hook.example/notify?title={title}\"\n  method: \"get\"\n  body-template: \"ignored\"\n  headers:\n    X-Token: \"abc\"\n",
        fetcher.clone(),
    );
    assert!(only_provider(&m)
        .push_at("中文 标题", "正文", AlertLevel::Info, FIXED_NOW_MS)
        .await
        .unwrap());
    let req = fetcher.only_request();
    assert_eq!(req.method, "GET", "方法名大写归一（对齐 toUpperCase）");
    // URL 场景按 URLEncoder 规则编码（空格 ⇒ %20）
    assert_eq!(
        req.url,
        "https://hook.example/notify?title=%E4%B8%AD%E6%96%87%20%E6%A0%87%E9%A2%98"
    );
    assert!(req.body.is_none());
    assert_eq!(req.header("Content-Type"), None);
    assert_eq!(req.header("X-Token"), Some("abc"));
}

#[tokio::test]
async fn webhook_encodes_non_ascii_header_values() {
    let fetcher = MockFetcher::new(200, "ok");
    let m = manager(
        "w:\n  type: webhook\n  url: \"https://hook.example/\"\n  headers:\n    X-Note: \"中文\"\n",
        fetcher.clone(),
    );
    assert!(only_provider(&m)
        .push_at("t", "c", AlertLevel::Info, FIXED_NOW_MS)
        .await
        .unwrap());
    assert_eq!(
        fetcher.only_request().header("X-Note"),
        Some("UTF-8''%E4%B8%AD%E6%96%87")
    );
}

#[tokio::test]
async fn webhook_skips_null_values_and_blank_header_names() {
    let fetcher = MockFetcher::new(200, "ok");
    let m = manager(
        "w:\n  type: webhook\n  url: \"https://hook.example/\"\n  headers:\n    X-Null: null\n    X-Empty: \"\"\n    \"\": \"blank-key\"\n",
        fetcher.clone(),
    );
    assert!(only_provider(&m)
        .push_at("t", "c", AlertLevel::Info, FIXED_NOW_MS)
        .await
        .unwrap());
    let req = fetcher.only_request();
    // null 值被忽略（对齐 `headerValue != null`）；空串仍会发送
    assert_eq!(req.header("X-Null"), None);
    assert_eq!(req.header("X-Empty"), Some(""));
    // 空白的头名被忽略（对齐 `!headerKey.isBlank()`）
    assert!(
        req.headers.iter().all(|(name, _)| !name.trim().is_empty()),
        "空白的头名不应出现在请求里: {:?}",
        req.headers
    );
}

#[tokio::test]
async fn webhook_rejects_unsupported_method_and_blank_url() {
    let fetcher = MockFetcher::new(200, "ok");
    let m = manager(
        "w:\n  type: webhook\n  url: \"https://hook.example/\"\n  method: \"PUT\"\n",
        fetcher.clone(),
    );
    let err = only_provider(&m)
        .push_at("t", "c", AlertLevel::Info, FIXED_NOW_MS)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("Unsupported webhook method"),
        "{err}"
    );

    let fetcher = MockFetcher::new(200, "ok");
    let m = manager("w:\n  type: webhook\n", fetcher);
    let err = only_provider(&m)
        .push_at("t", "c", AlertLevel::Info, FIXED_NOW_MS)
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("Webhook URL cannot be empty"),
        "{err}"
    );
}

#[tokio::test]
async fn webhook_failure_reports_status_body() {
    let fetcher = MockFetcher::new(502, "gateway down");
    let m = manager(
        "w:\n  type: webhook\n  url: \"https://hook.example/\"\n",
        fetcher,
    );
    let err = only_provider(&m)
        .push_at("t", "c", AlertLevel::Info, FIXED_NOW_MS)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("gateway down"), "{err}");
}

// ---------------- smtp ----------------

#[test]
fn smtp_encryption_names_match_upstream_enum() {
    assert_eq!(
        parse_smtp_encryption("SSLTLS"),
        Some(SmtpEncryption::SslTls)
    );
    assert_eq!(
        parse_smtp_encryption("STARTTLS"),
        Some(SmtpEncryption::StartTls)
    );
    assert_eq!(
        parse_smtp_encryption("ENFORCE_STARTTLS"),
        Some(SmtpEncryption::EnforceStartTls)
    );
    assert_eq!(parse_smtp_encryption("NONE"), Some(SmtpEncryption::None));
    // 上游 Enum.valueOf 大小写敏感：小写写法会被记为「无法识别的加密类型」
    assert_eq!(parse_smtp_encryption("starttls"), None);
    assert_eq!(parse_smtp_encryption(""), None);
}

#[test]
fn smtp_message_parts_match_upstream() {
    let settings = SmtpSettings {
        host: "smtp.example.com".into(),
        port: 465,
        auth: true,
        username: "u".into(),
        password: "p".into(),
        sender: "pbh@example.com".into(),
        sender_name: "PeerBanHelper".into(),
        receivers: vec!["admin@example.com".into(), "not-an-address".into()],
        encryption: "SSLTLS".into(),
        send_partial: true,
    };

    // 非法收件人在加载期即被识别（上游 warn 后 filter 掉）
    assert!(Mailbox::from_str("not-an-address").is_err());

    // 正文为 markdown 渲染出的 HTML（对齐 commonmark HtmlRenderer 的等价输出）
    assert_eq!(
        markdown_to_html("[PeerBanHelper/WARN] 标题: 正文"),
        "<p>[PeerBanHelper/WARN] 标题: 正文</p>\n"
    );

    // 组装后的 MIME：发件人显示名取配置的 name、非法收件人被跳过、正文为 HTML
    let raw = String::from_utf8_lossy(
        &smtp_build_message(&settings, "主题", "**bold**")
            .unwrap()
            .formatted(),
    )
    .to_string();
    assert!(
        raw.contains("From: PeerBanHelper <pbh@example.com>"),
        "{raw}"
    );
    assert!(raw.contains("To: admin@example.com"), "{raw}");
    assert!(
        !raw.contains("not-an-address"),
        "非法收件人不应出现在邮件里"
    );
    assert!(
        raw.contains("Content-Type: text/html; charset=utf-8"),
        "{raw}"
    );
    assert!(raw.contains("<strong>bold</strong>"), "{raw}");
    // 非 ASCII 主题走 RFC 2047 编码字（对齐 JavaMail setSubject(subject, "UTF-8")）
    assert!(raw.contains("Subject: =?utf-8?b?"), "{raw}");

    // 无有效收件人 ⇒ 直接报错（对齐 JavaMail 的 No recipient addresses）
    let mut no_receiver = settings.clone();
    no_receiver.receivers = vec!["bad".into()];
    assert!(smtp_build_message(&no_receiver, "s", "t").is_err());
}

#[tokio::test]
async fn smtp_provider_loads_from_config_and_failure_is_non_fatal() {
    let fetcher = MockFetcher::new(200, "{}");
    let m = manager(
        "mail:\n  type: smtp\n  host: \"smtp.example.com\"\n  port: 465\n  auth: true\n  username: \"u\"\n  password: \"p\"\n  sender: \"pbh@example.com\"\n  receiver:\n    - \"not-an-address\"\n",
        fetcher,
    );
    let provider = only_provider(&m);
    assert_eq!(provider.config_type(), "smtp");
    assert_eq!(provider.name(), "mail");
    match provider {
        PushProvider::Smtp { settings, .. } => {
            assert_eq!(settings.host, "smtp.example.com");
            assert_eq!(settings.port, 465);
            assert!(settings.auth);
            // 上游 `loadFromYaml` 的默认值
            assert_eq!(settings.sender_name, "PeerBanHelper");
            assert_eq!(settings.encryption, "SSLTLS");
            assert!(settings.send_partial);
        }
        other => panic!("应为 smtp 渠道，实际为 {}", other.config_type()),
    }
    // 收件人全部非法 ⇒ 在建立连接前就失败；`push` 返回 Ok(false)（不 panic、不中断 wave）
    assert!(!provider.push("t", "c", AlertLevel::Info).await.unwrap());
}

// ---------------- 管理器语义 ----------------

#[tokio::test]
async fn unknown_provider_is_skipped_and_type_is_case_insensitive() {
    let fetcher = MockFetcher::new(200, r#"{"code":200}"#);
    let m = manager(
        "bad:\n  type: no-such-provider\n  token: \"x\"\nplus:\n  type: PuShPlUs\n  token: \"tk\"\n",
        fetcher.clone(),
    );
    // 未识别的 type 只记录日志并跳过（其余渠道照常注册）
    assert_eq!(m.provider_list().len(), 1);
    assert!(m.push_message("t", "c", AlertLevel::Warn).await);
    assert_eq!(fetcher.requests().len(), 1);
}

#[tokio::test]
async fn push_message_is_or_over_providers_in_config_order() {
    let fetcher = MockFetcher::new(200, "{}");
    let m = manager(
        "a:\n  type: bark\n  device_key: \"k\"\nb:\n  type: pushdeer\n  pushkey: \"k\"\n",
        fetcher.clone(),
    );
    assert_eq!(m.provider_list().len(), 2, "保持配置书写顺序");
    assert_eq!(m.provider_list()[0].config_type(), "bark");
    assert_eq!(m.provider_list()[1].config_type(), "pushdeer");
    assert!(m.push_message("t", "c", AlertLevel::Warn).await);
    assert_eq!(fetcher.requests().len(), 2);
}

#[tokio::test]
async fn push_message_reports_false_when_all_providers_fail() {
    let fetcher = MockFetcher::new(500, "boom");
    let m = manager("a:\n  type: bark\n  device_key: \"k\"\n", fetcher.clone());
    assert!(!m.push_message("t", "c", AlertLevel::Warn).await);
    assert_eq!(fetcher.requests().len(), 1, "失败渠道仍应尝试过");
}

#[tokio::test]
async fn failed_provider_does_not_block_other_providers() {
    let fetcher = MockFetcher::new(500, "boom");
    let m = manager(
        "a:\n  type: bark\n  device_key: \"k\"\nb:\n  type: pushdeer\n  pushkey: \"k\"\n",
        fetcher.clone(),
    );
    assert!(!m.push_message("t", "c", AlertLevel::Warn).await);
    assert_eq!(
        fetcher.requests().len(),
        2,
        "前一个渠道失败后仍应尝试后一个"
    );
}

#[tokio::test]
async fn provider_name_dots_are_replaced() {
    let fetcher = MockFetcher::new(200, "{}");
    let m = manager("a.b:\n  type: bark\n  device_key: \"k\"\n", fetcher);
    assert_eq!(only_provider(&m).name(), "a-b");
}

// ---------------- 告警管理器 ----------------

fn translator() -> Arc<Translator> {
    Arc::new(Translator::embedded())
}

fn nat_alert_components() -> (TranslationComponent, TranslationComponent) {
    (
        TranslationComponent::new("DOWNLOADER_DOCKER_INCORRECT_NETWORK_DETECTED_TITLE"),
        TranslationComponent::with_params(
            "DOWNLOADER_DOCKER_INCORRECT_NETWORK_DETECTED_DESCRIPTION",
            vec![
                Param::Text("qBittorrent".into()),
                Param::Text("192.168.0.1".into()),
            ],
        ),
    )
}

#[tokio::test]
async fn alert_title_prefix_and_identifier_dedup() {
    let fetcher = MockFetcher::new(200, "{}");
    let m = Arc::new(manager(
        "a:\n  type: bark\n  device_key: \"k\"\n",
        fetcher.clone(),
    ));
    let alerts = AlertManager::new(m, translator(), "zh_cn");
    let identifier = "downloader-nat-setup-error@qBittorrent";
    let (title, content) = nat_alert_components();

    alerts
        .publish_alert(true, AlertLevel::Error, identifier, &title, &content)
        .await;
    let body = fetcher.only_request().body.unwrap();
    // 标题前缀 "[PeerBanHelper/<LEVEL>] " + tlUI(title)，正文为 tlUI(content)
    assert!(body.contains("[PeerBanHelper/ERROR]"), "{body}");
    assert!(body.contains("严重错误"), "标题应渲染文案：{body}");
    assert!(body.contains("qBittorrent"), "正文应渲染参数：{body}");
    assert!(alerts.identifier_alert_exists(identifier));
    assert_eq!(
        alerts.get_highest_unread_alert_level(),
        Some(AlertLevel::Error)
    );

    // 相同 identifier 的未读告警不重复发布
    alerts
        .publish_alert(true, AlertLevel::Error, identifier, &title, &content)
        .await;
    assert_eq!(fetcher.requests().len(), 1);

    // 已读后 identifierAlertExists=false，但 IncludeRead 仍为 true
    alerts.mark_alert_as_read(identifier);
    assert!(!alerts.identifier_alert_exists(identifier));
    assert!(alerts.identifier_alert_exists_include_read(identifier));
    assert_eq!(alerts.get_highest_unread_alert_level(), None);

    // push=false ⇒ 只记录告警、不推送
    alerts
        .publish_alert(false, AlertLevel::Info, "another", &title, &content)
        .await;
    assert_eq!(fetcher.requests().len(), 1);
    assert_eq!(
        alerts.get_highest_unread_alert_level(),
        Some(AlertLevel::Info)
    );
}

#[tokio::test]
async fn alert_manager_without_providers_is_silent() {
    let fetcher = MockFetcher::new(200, "{}");
    let m = Arc::new(manager("{}", fetcher.clone()));
    let alerts = AlertManager::new(m, translator(), "zh_cn");
    let (title, content) = nat_alert_components();
    alerts
        .publish_alert(true, AlertLevel::Error, "id", &title, &content)
        .await;
    assert!(fetcher.requests().is_empty(), "无渠道时不应产生流量");
    assert!(alerts.identifier_alert_exists("id"), "告警仍需被记录");
}

#[test]
fn alert_level_names_and_ntfy_priority_match_upstream() {
    assert_eq!(AlertLevel::Tip.name(), "TIP");
    assert_eq!(AlertLevel::Info.name(), "INFO");
    assert_eq!(AlertLevel::Warn.name(), "WARN");
    assert_eq!(AlertLevel::Error.name(), "ERROR");
    assert_eq!(AlertLevel::Fatal.name(), "FATAL");
    assert_eq!(AlertLevel::Tip.ntfy_priority(), 1);
    assert_eq!(AlertLevel::Info.ntfy_priority(), 2);
    assert_eq!(AlertLevel::Warn.ntfy_priority(), 3);
    assert_eq!(AlertLevel::Error.ntfy_priority(), 4);
    assert_eq!(AlertLevel::Fatal.ntfy_priority(), 5);
    // 顺序即 ordinal（getHighestUnreadAlertLevel 依赖）
    assert!(AlertLevel::Tip < AlertLevel::Info);
    assert!(AlertLevel::Warn < AlertLevel::Error);
    assert!(AlertLevel::Error < AlertLevel::Fatal);
}
